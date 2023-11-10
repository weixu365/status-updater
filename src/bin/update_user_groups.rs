use std::env;

use on_call_support::errors::AppError;
use on_call_support::user_group_updater::update_user_groups;
use tokio;

#[tokio::main]
async fn main() -> Result<(), AppError> {
    env::set_var("UPDATE_USER_GROUP_LAMBDA", "");
    env::set_var("UPDATE_USER_GROUP_LAMBDA_ROLE", "");
    
    update_user_groups("dev").await?;
    
    Ok(())
}
