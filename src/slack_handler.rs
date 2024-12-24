use std::{collections::HashMap, env};
use aws_lambda_events::{event::apigw::ApiGatewayProxyResponse, encodings::Body, http::{HeaderMap, HeaderValue}, query_map::QueryMap};

use chrono::{Local, Utc};
use chrono_tz::Tz;
use std::str::FromStr;
use crate::{scheduled_tasks::{ScheduledTask, ScheduledTasksDynamodb, EventBridgeScheduler}, cron::get_next_schedule_from, secrets::SecretsClient, encryption::Encryption, errors::AppError, build_http_client, service_provider::slack::swap_slack_access_token, db::{SlackInstallation, SlackInstallationsDynamoDb}, config::Config};
use form_urlencoded;
use ring::hmac;
use clap::{Args, Subcommand};
use clap::Parser;
use lazy_static::lazy_static;
use regex::Regex;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct App {
    #[clap(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Args)]
struct ScheduleArgs {
    #[arg(long)]
    user_group: String,

    #[arg(long)]
    pagerduty_schedule: String,

    #[arg(long)]
    pagerduty_api_key: Option<String>,

    #[arg(long)]
    cron: String,

    #[arg(long)]
    timezone: Option<String>,
}

#[derive(Debug, Args)]
struct SetupPagerdutyArgs {
    #[arg(long)]
    pagerduty_api_key: String,
}

#[derive(Debug, Args)]
struct ListSchedulesArgs {
    #[arg(long)]
    all: Option<bool>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Schedule(ScheduleArgs),
    ListSchedules(ListSchedulesArgs),
    SetupPagerduty(SetupPagerdutyArgs),
    New,
}

fn cleanse(text: &str) -> String {
    lazy_static! {
        static ref DOUBLE_QUOTES: Regex = Regex::new("[“”]").unwrap();
        static ref SINGLE_QUOTES: Regex = Regex::new("[‘’]").unwrap();
    }
    
    let cleansed_double_quote = DOUBLE_QUOTES.replace_all(text, "\"");
    let cleansed = SINGLE_QUOTES.replace_all(&cleansed_double_quote, "'");
    
    cleansed.to_string()
}

fn get_param(params: &HashMap<String, String>, name: &str) -> String {
    params.get(&name.to_string()).unwrap_or(&"".to_string()).to_string()
}

pub async fn handle_slack_oauth(env: &str, query_map: QueryMap) -> Result<ApiGatewayProxyResponse, AppError> {
    let code_parameter = query_map.first("code");

    match code_parameter {
        Some(temporary_code) => {
            let http_client = build_http_client()?;
            let config = ::aws_config::load_from_env().await;
            let secrets_client = SecretsClient::new(&config);
            let secrets = secrets_client.get_secret("on-call-support/secrets").await?;

            let encryption = Encryption::new(&secrets.encryption_key);

            let oauth_response = swap_slack_access_token(&http_client, temporary_code, &secrets.slack_client_id, &secrets.slack_client_secret).await?;
            
            // Save to dynamodb
            let db = SlackInstallationsDynamoDb::new(&config, format!("on-call-support-installations-{}", env), encryption);
            let installation = SlackInstallation {
                team_id: oauth_response.team.id,
                team_name: oauth_response.team.name,
                enterprise_id: oauth_response.enterprise.id,
                enterprise_name: oauth_response.enterprise.name,
                is_enterprise_install: oauth_response.is_enterprise_install,
            
                access_token: oauth_response.access_token,
                token_type: oauth_response.token_type,
                scope: oauth_response.scope,
            
                authed_user_id: oauth_response.authed_user.id,
                app_id: oauth_response.app_id,
                bot_user_id: oauth_response.bot_user_id,

                pager_duty_token: None,
            };

            db.save_slack_installation(&installation).await?;
            Ok(response(200, format!("Received slack oauth callback.")))
        },
        None => Ok(response(400, format!("Invalid request"))),
    }
}

pub async fn handle_slack_command(env: &str, request_header: HeaderMap<HeaderValue>, request_body: Option<String>) -> Result<ApiGatewayProxyResponse, AppError> {
    let request_body = request_body.unwrap_or_default();
    
    let params: HashMap<String, String> = form_urlencoded::parse(request_body.as_bytes()).into_owned().collect();
    // println!("params in body: {:?}", params);

    let team_id = get_param(&params, "team_id");
    let team_domain = get_param(&params, "team_domain");
    let channel_id = get_param(&params, "channel_id");
    let channel_name = get_param(&params, "channel_name");
    let enterprise_id = get_param(&params, "enterprise_id");
    let enterprise_name = get_param(&params, "enterprise_name");
    let is_enterprise_install = get_param(&params, "is_enterprise_install").eq_ignore_ascii_case("true");

    let user_id = get_param(&params, "user_id");
    let user_name = get_param(&params, "user_name");
    let command = get_param(&params, "command");
    let text = get_param(&params, "text");
    let _response_url = get_param(&params, "response_url");
    let slack_request_timestamp_str = request_header.get("X-Slack-Request-Timestamp").map(|v| v.to_str())
        .expect("Missing X-Slack-Request-Timestamp")?;
    let slack_request_signature = request_header.get("X-Slack-Signature").map(|v| v.to_str())
        .expect("Missing X-Slack-Signature")?;
    let now = Local::now().timestamp();

    // println!("parsed parameter: {}", json!({
    //     "team_id": team_id,
    //     "team_domain": team_domain,
    //     "channel_id": channel_id,
    //     "channel_name": channel_name,
    //     "user_id": user_id,
    //     "user_name": user_name,
    //     "command": command,
    //     "text": text,
    //     "response_url": response_url,
    //     "X-Slack-Request-Timestamp": slack_request_timestamp_str,
    //     "X-Slack-Signature": slack_request_signature,
    //     "current timestamp": now,
    // }));
    
    let slack_request_timestamp: i64 = slack_request_timestamp_str.parse::<i64>().expect("failed to parse timestamp");
        
    if (now - slack_request_timestamp).abs() > 60 * 5 {
        return Ok(ApiGatewayProxyResponse {
            status_code: 400,
            body: Some(Body::from(format!("Invalid slack command due to invalid timestamp: {} {}", command, text))),
            ..Default::default()
        })
    }
    
    let sig_basestring = format!("v0:{}:{}", slack_request_timestamp, request_body);
    // println!("string to sign: {:?}", sig_basestring);

    let signing_key = "aa2ad1a24622382aa823959083867312";
    let verification_key = hmac::Key::new(hmac::HMAC_SHA256, signing_key.as_bytes());
    let signature = hex::encode(hmac::sign(&verification_key, sig_basestring.as_bytes()).as_ref());

    if format!("v0={}", signature) != slack_request_signature {
        println!("Signature doesn't match {} vs {}", signature, slack_request_signature);
        return Ok(ApiGatewayProxyResponse {
            status_code: 400,
            body: Some(Body::from(format!("Invalid slack command signature: {} {}", command, text))),
            ..Default::default()
        })
    } else {
        // println!("Signature matched");
    }
    
    let arg = match shlex::split(cleanse(format!("{} {}", command, text).as_str()).as_str()) {
        Some(args) => Some(App::parse_from(args.iter())),
        None => None
    };

    // println!("Parsed arg: {:?}", arg);
    
    let aws_config = ::aws_config::load_from_env().await;
    let secrets = SecretsClient::new(&aws_config).get_secret("on-call-support/secrets").await?;
    let encryption = Encryption::new(&secrets.encryption_key);

    let response_body = match arg.unwrap().command {
        Some(Command::Schedule(arg)) => {
            // 
            let user_group_id: String;
            let user_group_handle: String;

            let re = Regex::new(r"<!subteam\^(\w+)\|@([^>]+)>").unwrap();
            if let Some(captures) = re.captures(arg.user_group.as_str()) {
                user_group_id = captures.get(1).unwrap().as_str().to_string();
                user_group_handle = captures.get(2).unwrap().as_str().to_string();
            } else {
                println!("Invalid user group: {}", arg.user_group);

                return Ok(ApiGatewayProxyResponse {
                    status_code: 400,
                    body: Some(Body::from(format!("Invalid user group: {}", arg.user_group))),
                    ..Default::default()
                })
            }
            
            let lambda_arn = env::var("UPDATE_USER_GROUP_LAMBDA")?;
            let lambda_role = env::var("UPDATE_USER_GROUP_LAMBDA_ROLE")?;

            let db = ScheduledTasksDynamodb::new(&aws_config, format!("on-call-support-schedules-{}", env), encryption);
            let scheduler = EventBridgeScheduler::new(&aws_config, format!("on-call-support-dev_UpdateUserGroupSchedule_"), lambda_arn, lambda_role);

            let timezone = Tz::from_str(&arg.timezone.unwrap_or("UTC".to_string())).unwrap();
            let from = Utc::now().with_timezone(&timezone);

            let next_schedule = get_next_schedule_from(&arg.cron, &from).expect("The cron has no future scheduled time from now");

            let task_id = format!("{}:{}:{}:{}:{}", channel_name, channel_id, user_group_handle, user_group_id, arg.pagerduty_schedule);

            let task = ScheduledTask {
                team: format!("{}:{}", &team_id, &enterprise_id),
                task_id,
                next_update_timestamp_utc: next_schedule.next_timestamp_utc,
                next_update_time: next_schedule.next_datetime.to_rfc3339(),

                team_id,
                team_domain,
                channel_id,
                channel_name,
                enterprise_id,
                enterprise_name,
                is_enterprise_install,

                user_group_id,
                user_group_handle,
                pager_duty_schedule_id: arg.pagerduty_schedule,
                pager_duty_token: arg.pagerduty_api_key,
                cron: arg.cron,
                timezone: timezone.to_string(),

                created_by_user_id: user_id,
                created_by_user_name: user_name,
                created_at: Utc::now().to_rfc3339(),
                last_updated_at: Utc::now().to_rfc3339(),
            };
            
            if let Err(err) = db.save_scheduled_task(&task).await {
                println!("Failed to save to dynamodb, {:?}", err);
                return Ok(response(500, format!("Can't process slack command due to save to dynamodb failed\nCommand: {} {}", command, text)))
            }

            if let Err(err) = scheduler.update_next_schedule(&next_schedule).await {
                println!("Failed to update scheduler, {:?}", err);
                return Ok(response(500, format!("Can't process slack command due to save to update scheduler\nCommand: {} {}", command, text)))
            }
            
            vec!(format!("Update user group: {}|{} based on pagerduty schedule: {}, at: {}", task.user_group_id, task.user_group_handle, &task.pager_duty_schedule_id, &task.cron))
        },
        Some(Command::SetupPagerduty(args)) => {
            let config = Config::new(env);
            let slack_installations_db = SlackInstallationsDynamoDb::new(&aws_config, config.installations_table_name, encryption.clone());

            //TODO: validate if the installation exists
            //TODO: validate if the pagerduty token valid

            slack_installations_db.update_pagerduty_token(team_id, enterprise_id, &args.pagerduty_api_key).await?;

            vec!(format!("Setup pagerduty with api key"))
        },
        Some(Command::ListSchedules(args)) => {
            let db = ScheduledTasksDynamodb::new(&aws_config, format!("on-call-support-schedules-{}", env), encryption);
            let tasks = db.list_scheduled_tasks().await?;

            tasks.into_iter()
                .map(|t| format!("## {}\nUpdate {} on {}\nNext schedule: {}", t.channel_name, t.user_group_handle, t.cron, t.next_update_time))
                .collect()
        },
        Some(Command::New) => vec!(format!("Show wizard to add new schedule")),
        None => vec!(format!("default command"))
    };
    
    let sections = response_body.into_iter()
        .map(|p| format!(r#"{{"type": "section", "text": {{ "type": "mrkdwn", "text": "{}" }} }}"#, p))
        .collect::<Vec<String>>()
        .join(",\n")
    ;

    Ok(response(200, format!(r#"{{ "blocks": [{}] }}"#, sections)))
}

pub fn response(status_code: i64, body: String) -> ApiGatewayProxyResponse {
    let mut response_headers = HeaderMap::new();
    response_headers.insert("response_type", "in_channel".parse().unwrap());
    response_headers.insert("Content-type", "application/json".parse().unwrap());

    ApiGatewayProxyResponse {
        status_code,
        headers: response_headers,
        body: Some(Body::from(body)),
        ..Default::default()
    }
}
