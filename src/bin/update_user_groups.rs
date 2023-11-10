use on_call_support::errors::AppError;
use on_call_support::user_group_updater::update_user_groups;
use tokio;

#[tokio::main]
async fn main() -> Result<(), AppError> { 
    update_user_groups("dev").await?;
    
    Ok(())
}
