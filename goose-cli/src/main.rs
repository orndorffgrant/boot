use anyhow::Ok;
use clap::{Args, Parser, Subcommand};
use comfy_table::{Table, presets::NOTHING};
use goose_api_client;

mod server_config;
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
    #[command(name = "list-servers")]
    ListServers,
    #[command(name = "add-server")]
    AddServer(AddServerArgs),
    #[command(name = "remove-server")]
    RemoveServer(RemoveServerArgs),
}

#[derive(Args)]
struct AddServerArgs {
    /// Name of the server
    name: String,
    /// Host address
    host: String,
    /// Port number (defaults to 46673)
    #[arg(short, long, default_value = "46673")]
    port: u16,
}

#[derive(Args)]
struct RemoveServerArgs {
    /// Name of the server to remove
    name: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CLI::parse();
    match cli.command {
        Command::StartServer => start_server().await,
        Command::StopServer => stop_server().await,
        Command::RunServer => run_server().await,
        Command::APIDesc => api_desc().await,
        Command::ListServers => list_servers().await,
        Command::AddServer(args) => add_server(args).await,
        Command::RemoveServer(args) => remove_server(args).await,
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

async fn list_servers() -> anyhow::Result<()> {
    let config = server_config::ServerConfig::load()?;
    let servers = config.list_servers();
    
    if servers.is_empty() {
        println!("No servers configured.");
        return Ok(());
    }
    
    let mut table = Table::new();
    table.load_preset(NOTHING);
    
    for server in servers {
        table.add_row(vec![&server.name, &server.host, &server.port.to_string()]);
    }
    
    println!("{table}");
    
    Ok(())
}

async fn add_server(args: AddServerArgs) -> anyhow::Result<()> {
    let mut config = server_config::ServerConfig::load()?;
    config.add_server(args.name.clone(), args.host.clone(), args.port)?;
    config.save()?;
    
    println!("Added server '{}' at {}:{}", args.name, args.host, args.port);
    Ok(())
}

async fn remove_server(args: RemoveServerArgs) -> anyhow::Result<()> {
    let mut config = server_config::ServerConfig::load()?;
    config.remove_server(&args.name)?;
    config.save()?;
    
    println!("Removed server '{}'", args.name);
    Ok(())
}
