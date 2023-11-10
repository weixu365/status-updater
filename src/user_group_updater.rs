use std::{sync::Arc, collections::HashMap, env};

use futures::StreamExt;
use crate::{scheduled_tasks::{ScheduledTasksDynamodb, EventBridgeScheduler}, secrets::SecretsClient, encryption::Encryption, db::SlackInstallationsDynamoDb, config::Config};

use chrono::{Utc, Duration, DateTime};
use reqwest::Client;
use crate::{build_http_client, errors::AppError, service_provider::{pager_duty::PagerDuty, slack::Slack}};

pub async fn update_user_group(
    http_client: Arc<Box<Client>>, 
    pager_duty_api_key: &str,
    pager_duty_schedule_id: &str,
    pager_duty_schedule_from: DateTime<Utc>,
    slack_api_key: &str,
    slack_channel_id: &str,
    slack_user_group_name: &str,
) -> Result<(), AppError>{
    println!("Getting the current on-call users");

    let from = pager_duty_schedule_from;
    let until = from + Duration::minutes(10);
    // let now = Utc.with_ymd_and_hms(2023, 5, 18, 23, 0, 0).unwrap();

    let pager_duty = PagerDuty::new(http_client.clone(), pager_duty_api_key.to_string(), pager_duty_schedule_id.to_string());
    let oncall_users = pager_duty.get_on_call_users(from).await?;
    println!("Found {} users on call from {} to {}",  oncall_users.len(), from, until);
    
    for user in &oncall_users {
        println!("  - User: {}, {}", user.name, user.email);
    }

    let slack = Slack::new(http_client.clone(), slack_api_key.to_string());

    let user_group = slack.get_user_group(&slack_user_group_name).await?;
    println!("Found user group: {:?}", user_group);

    let slack_user_ids: Vec<String> = futures::stream::iter(&oncall_users).then(|user| async {
        let slack_user = slack.get_user_by_email(&user.email).await
            .expect(format!("Couldn't find user in Slack by email: {:?}", user.email).as_str());
        slack_user.expect(format!("Couldn't find user in Slack by email: {:?}", user.email).as_str()).id
    }).collect().await;
    
    let current_users = slack.get_user_group_users(&user_group.id).await?;
    let current_user_names: Vec<String> = futures::stream::iter(&current_users).then(|user_id| async {
        let id = user_id.clone();
        let slack_user = slack.get_user_by_id(&id).await
            .expect(format!("Couldn't find user in Slack by id: {:?}", id).as_str());
        slack_user.expect(format!("Couldn't find user in Slack by id: {:?}", id).as_str()).name
    }).collect().await;
    
    if current_users.len() > slack_user_ids.len() + 2 {
        // send message to channel with message: failed to update user group due to too many users 
        // return Err(AppError::SlackUpdateUserGroupError("Too many users in the current group, is the group correct?".to_string()));
    }

    println!("Current user ids in group: {:?}", current_users);
    println!("Current user names in group: {:?}", current_user_names);

    println!("Update users to group: {:?}", slack_user_ids);
    
    println!("Does users changed: {:?}", slack_user_ids != current_users);
    slack.update_user_group_users(&user_group.id, &slack_user_ids).await?;
    
    if slack_user_ids != current_users {
        println!("Send message to channel");
        slack.send_message(&slack_channel_id, &format!("Updated support user group <!subteam^{}> to: <@{}>", &user_group.id, slack_user_ids.join(", "))).await?;
    }

    Ok(())
}

pub async fn update_user_groups(env: &str) -> Result<(), AppError> {
    let lambda_arn = env::var("UPDATE_USER_GROUP_LAMBDA")?;
    let lambda_role = env::var("UPDATE_USER_GROUP_LAMBDA_ROLE")?;
    let config = Config::new(env);
    
    let http_client = Arc::new(Box::new(build_http_client()?));

    let aws_config = ::aws_config::load_from_env().await;
    let scheduler = EventBridgeScheduler::new(&aws_config, config.schedule_name_prefix, lambda_arn, lambda_role);
    
    let secrets_client = SecretsClient::new(&aws_config);
    let encryption_key = secrets_client.get_secret(&config.secret_name).await?;
    let encryption = Encryption::new(&encryption_key.encryption_key);

    let slack_installations_db = SlackInstallationsDynamoDb::new(&aws_config, config.installations_table_name, encryption.clone());
    let slack_tokens: HashMap<String, String> = slack_installations_db.list_installations().await?
        .into_iter()
        .map(|i| (i.team_id, i.access_token))
        .collect();
    let scheduled_tasks_db = ScheduledTasksDynamodb::new(&aws_config, config.schedules_table_name, encryption.clone());

    let tasks = scheduled_tasks_db.list_scheduled_tasks().await?;
    let mut timestamp_of_next_trigger = i64::MAX;
    let mut next_task = None;
    let start_of_the_update = Utc::now();

    println!("Found {} tasks", tasks.len());
    for mut task in tasks {
        if task.next_update_timestamp_utc > 0 && task.next_update_timestamp_utc <= Utc::now().timestamp() {
            println!("Updating user group for task {}, scheduled at: {}", task.task_id, task.cron);

            let slack_api_key = slack_tokens.get(&task.team_id)
                .expect(format!("Could not find slack installation for team: {}, task: {}", task.team, task.task_id).as_str());
    
            update_user_group(
                http_client.clone(),
                &task.pager_duty_token,
                &task.pager_duty_schedule_id,
                Utc::now(),
                slack_api_key,
                &task.channel_id,
                &task.user_group_handle,
            ).await?;
    
            task.last_updated_at = Utc::now().to_rfc3339();

            if let Some(task_next_schedule) = task.calculate_next_schedule(&Utc::now()) {
                task.next_update_timestamp_utc = task_next_schedule.next_timestamp_utc;
                task.next_update_time = task_next_schedule.next_datetime.to_rfc3339();
            } else {
                task.next_update_timestamp_utc = -1;
                task.next_update_time = "".to_string();
            }
            scheduled_tasks_db.update_next_schedule(&task).await?;
        } else {
            println!("Skipped {}, next trigger is: {} which is: {} greater than {}", task.task_id, task.next_update_time, task.next_update_timestamp_utc, Utc::now().timestamp());
        }

        if task.next_update_timestamp_utc > 0 && task.next_update_timestamp_utc < timestamp_of_next_trigger {
            timestamp_of_next_trigger = task.next_update_timestamp_utc;
            next_task = Some(task.clone());
        }
    }

    // at least re-run daily
    // (Utc::now() + Duration::days(1)).timestamp()
    if let Some(next) = next_task {
        if let Some(next_schedule) = next.calculate_next_schedule(&start_of_the_update) {
            //TODO: if next schedule is earlier than now, re-run the above loop
            scheduler.update_next_schedule(&next_schedule).await?;
        }
    }

    println!("Finished updating user groups");

    Ok(())
}