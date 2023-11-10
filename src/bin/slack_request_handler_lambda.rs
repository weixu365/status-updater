use aws_lambda_events::event::apigw::{ApiGatewayProxyRequest, ApiGatewayProxyResponse};

use on_call_support::{errors::AppError, slack_handler::{handle_slack_command, response, handle_slack_oauth}};
use tokio;
use lambda_runtime::{service_fn, LambdaEvent, Error};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let func = service_fn(func);
    let result = lambda_runtime::run(func).await;
    
    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            println!("Error occurred: {:?}", err);
            Err(err)
        }
    }    
}

async fn func(event: LambdaEvent<ApiGatewayProxyRequest>) -> Result<ApiGatewayProxyResponse, AppError> {
    let (event, _context) = event.into_parts();
    let env = "dev";

    match &event.path {
        Some(p) if p == "/slack/oauth" => {
            // println!("Received Slack oauth request. event: {:?}", event);
            match handle_slack_oauth(env, event.query_string_parameters).await  {
                Ok(res) => Ok(res),
                Err(err) => {
                    println!("Failed to process Slack OAuth request. err: {:?}", err);
                    Err(err)
                }
            }
        },
        Some(p) if p == "/slack/command" => {
            // println!("Received Slack command. event: {:?}", event);

            match handle_slack_command(env, event.headers, event.body).await {
                Ok(res) => Ok(res),
                Err(err) => {
                    println!("Failed to process Slack command. err: {:?}", err);
                    Err(err)
                }
            }
        },
        _ => {
            println!("Ignored invalid request. event: {:?}", event);
            Ok(response(400, format!("Invalid request")))
        },
    }
}
