use anyhow::Ok;
use clap::{Parser, Subcommand};
use goose_api_client;

mod systemd_service;

#[derive(Parser)]
#[command(name = "goose")]
struct CLI {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(name = "start-server")]
    StartServer,
    #[command(name = "stop-server")]
    StopServer,
    #[command(name = "run-server")]
    RunServer,
    #[command(name = "api-desc")]
    APIDesc,
    #[command(name = "list-programmers")]
    ListProgrammers,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CLI::parse();
    match cli.command {
        Command::StartServer => start_server().await,
        Command::StopServer => stop_server().await,
        Command::RunServer => run_server().await,
        Command::APIDesc => api_desc().await,
        api_dependent_cmd => {
            systemd_service::start_service_if_needed()?;
            let client = goose_api_client::Client::new("http://localhost:46673");
            match api_dependent_cmd {
                Command::ListProgrammers => list_programmers(client).await,
                _ => unreachable!(),
            }
        },
    }
}

async fn start_server() -> anyhow::Result<()> {
    systemd_service::start_service()
}
async fn stop_server() -> anyhow::Result<()> {
    systemd_service::stop_service()
}
async fn run_server() -> anyhow::Result<()> {
    goose_api::run().await
}

async fn api_desc() -> anyhow::Result<()> {
    let openapi = goose_api::openapi();
    println!("{}", openapi);
    Ok(())
}

async fn list_programmers(client: goose_api_client::Client) -> anyhow::Result<()> {
    let response = client.programmers().await?;
    println!("response: {:?}", response);
    Ok(())
}
