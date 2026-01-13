use anyhow::Ok;
use base64::Engine;
use clap::{Args, Parser, Subcommand};
use comfy_table::{Attribute, Cell, Table, presets::NOTHING};
use indicatif::{ProgressBar, ProgressStyle};
use boot_api_client;

mod server_config;
mod systemd_service;

#[derive(Parser)]
#[command(name = "boot")]
struct CLI {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(name = "start-server")]
    StartServer(StartServerArgs),
    #[command(name = "stop-server")]
    StopServer,
    #[command(name = "run-server")]
    RunServer(RunServerArgs),
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
    Run(RunArgs),
}

#[derive(Args)]
struct StartServerArgs {
    /// Host to listen on (defaults to 127.0.0.1)
    #[arg(long)]
    host: Option<String>,
    /// Port to run the server on (defaults to 46673)
    #[arg(short, long)]
    port: Option<u16>,
}

#[derive(Args)]
struct RunServerArgs {
    /// Host to listen on (defaults to 127.0.0.1)
    #[arg(long)]
    host: Option<String>,
    /// Port to run the server on (defaults to 46673)
    #[arg(short, long)]
    port: Option<u16>,
}

#[derive(Args)]
struct AddServerArgs {
    /// Name of the server
    name: String,
    /// Connection URL (e.g., http://localhost:46673)
    url: String,
}

#[derive(Args)]
struct RemoveServerArgs {
    /// Name of the server to remove
    name: String,
}

#[derive(Args)]
struct RunArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    server_and_selector: String,
    /// Path to the ELF file
    elf_path: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CLI::parse();
    match cli.command {
        Command::StartServer(args) => start_server(args).await,
        Command::StopServer => stop_server().await,
        Command::RunServer(args) => run_server(args).await,
        Command::APIDesc => api_desc().await,
        Command::ListServers => list_servers().await,
        Command::AddServer(args) => add_server(args).await,
        Command::RemoveServer(args) => remove_server(args).await,
        Command::ListProgrammers => list_programmers().await,
        Command::Run(args) => run(args).await,
    }
}

fn print_servers_table(servers: &[server_config::Server], highlight_name: Option<&str>, highlight_style: Option<Attribute>) {
    if servers.is_empty() {
        println!("No servers configured.");
        return;
    }
    
    let mut table = Table::new();
    table.load_preset(NOTHING);
    
    for server in servers {
        if let Some(name) = highlight_name {
            if server.name == name {
                if let Some(style) = highlight_style {
                    table.add_row(vec![
                        Cell::new(&server.name).add_attribute(style),
                        Cell::new(&server.url).add_attribute(style),
                    ]);
                    continue;
                }
            }
        }
        table.add_row(vec![&server.name, &server.url]);
    }
    
    println!("{table}");
}

async fn start_server(args: StartServerArgs) -> anyhow::Result<()> {
    let host = args.host.clone().unwrap_or("localhost".to_string());
    let port = args.port.clone().unwrap_or(46673);
    systemd_service::start_service(args.host, args.port)?;
    
    // Add local server to the servers list
    let mut config = server_config::ServerConfig::load()?;
    
    // Remove existing "local" entry if it exists
    config.servers.retain(|s| s.name != "local");
    
    // Determine the URL based on the host
    let url = if host == "0.0.0.0".to_string() {
        // If listening on all interfaces, use localhost for client connection
        format!("http://localhost:{}", port)
    } else {
        format!("http://{}:{}", host, port)
    };
    
    // Add the local server
    config.add_server("local".to_string(), url)?;
    config.save()?;
    
    // Display the updated servers list with the new local server in bold
    print_servers_table(config.list_servers(), Some("local"), Some(Attribute::Bold));
    Ok(())
}

async fn stop_server() -> anyhow::Result<()> {
    systemd_service::stop_service()?;
    
    // Remove local server from the servers list
    let mut config = server_config::ServerConfig::load()?;
    
    // Check if local server exists
    let local_exists = config.list_servers().iter().any(|s| s.name == "local");
    
    if local_exists {
        // Display the servers list with local crossed out before removal
        print_servers_table(config.list_servers(), Some("local"), Some(Attribute::CrossedOut));
        
        // Now remove the local server
        config.remove_server("local")?;
        config.save()?;
    }
    
    Ok(())
}
async fn run_server(args: RunServerArgs) -> anyhow::Result<()> {
    boot_api::run(args.host, args.port).await
}

async fn api_desc() -> anyhow::Result<()> {
    let openapi = boot_api::openapi();
    println!("{}", openapi);
    Ok(())
}

async fn list_programmers() -> anyhow::Result<()> {
    let config = server_config::ServerConfig::load()?;
    let servers = config.list_servers();
    
    if servers.is_empty() {
        println!("No servers configured.");
        return Ok(());
    }
    
    let mut table = Table::new();
    table.load_preset(NOTHING);
    
    for server in servers {
        let client = boot_api_client::Client::new(&server.url);
        match client.programmers().await {
            std::result::Result::Ok(response) => {
                let programmers = response.into_inner();
                // Skip if no programmers found
                if !programmers.is_empty() {
                    for programmer in programmers {
                        let selector = format!(
                            "{:04x}:{:04x}:{}",
                            programmer.vendor_id,
                            programmer.product_id,
                            programmer.serial_number
                                .as_ref()
                                .map(|s| format!("{}", s))
                                .unwrap_or_default()
                        );
                        table.add_row(vec![&server.name, &selector, &programmer.name]);
                    }
                }
            }
            std::result::Result::Err(e) => {
                // Print errors to stderr
                eprintln!("Error connecting to server '{}' ({}): {}", server.name, server.url, e);
            }
        }
    }
    
    println!("{table}");
    Ok(())
}

async fn list_servers() -> anyhow::Result<()> {
    let config = server_config::ServerConfig::load()?;
    print_servers_table(config.list_servers(), None, None);
    Ok(())
}

async fn add_server(args: AddServerArgs) -> anyhow::Result<()> {
    let mut config = server_config::ServerConfig::load()?;
    config.add_server(args.name.clone(), args.url.clone())?;
    config.save()?;
    
    print_servers_table(config.list_servers(), Some(&args.name), Some(Attribute::Bold));
    Ok(())
}

async fn remove_server(args: RemoveServerArgs) -> anyhow::Result<()> {
    let config = server_config::ServerConfig::load()?;
    
    // Check if server exists before removal
    if !config.list_servers().iter().any(|s| s.name == args.name) {
        anyhow::bail!("Server '{}' not found", args.name);
    }
    
    // Display the servers list with the server being removed crossed out
    print_servers_table(config.list_servers(), Some(&args.name), Some(Attribute::CrossedOut));
    
    // Now actually remove and save
    let mut config = config;
    config.remove_server(&args.name)?;
    config.save()?;
    
    Ok(())
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    // Parse server name and selector
    let parts: Vec<&str> = args.server_and_selector.split('/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid format. Expected {{server-name}}/{{debugprobeselector}}");
    }
    let server_name = parts[0];
    let selector = parts[1];
    
    // Load server configuration
    let config = server_config::ServerConfig::load()?;
    let server = config.list_servers()
        .iter()
        .find(|s| s.name == server_name)
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
    
    // Create spinner for reading file
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap()
    );
    spinner.set_message(format!("Reading {}...", args.elf_path));
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));
    
    // Read ELF file
    let elf_bytes = std::fs::read(&args.elf_path)
        .map_err(|e| anyhow::anyhow!("Failed to read ELF file '{}': {}", args.elf_path, e))?;
    
    spinner.finish_with_message(format!("Read {} ({} bytes)", args.elf_path, elf_bytes.len()));
    
    // Create spinner for encoding
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap()
    );
    spinner.set_message("Encoding to base64...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));
    
    // Encode to base64
    let elf_data = base64::prelude::BASE64_STANDARD.encode(&elf_bytes);
    
    spinner.finish_and_clear();
    
    // Create client with longer timeout for large file transfers
    let reqwest_client = reqwest::ClientBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let client = boot_api_client::Client::new_with_client(&server.url, reqwest_client);
    
    // Create spinner for flashing
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap()
    );
    spinner.set_message(format!("Flashing to {} via {}...", selector, server_name));
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));
    
    client.flash_and_run(selector, &boot_api_client::types::FlashRequest { elf_data })
        .await
        .map_err(|e| {
            spinner.finish_and_clear();
            anyhow::anyhow!("Failed to flash firmware: {:#}", e)
        })?;
    
    spinner.finish_with_message(format!("✓ Successfully flashed and started firmware on {}", selector));
    
    Ok(())
}
