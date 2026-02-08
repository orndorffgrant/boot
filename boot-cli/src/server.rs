use anyhow::Result;
use comfy_table::{Attribute, Cell, Table, presets::NOTHING};
use clap::Args;

use crate::server_config;
use crate::systemd_service;

#[derive(Args)]
pub struct StartServerArgs {
    /// Host to listen on (defaults to 127.0.0.1)
    #[arg(long)]
    pub host: Option<String>,
    /// Port to run the server on (defaults to 46673)
    #[arg(short, long)]
    pub port: Option<u16>,
}

#[derive(Args)]
pub struct RunServerArgs {
    /// Host to listen on (defaults to 127.0.0.1)
    #[arg(long)]
    pub host: Option<String>,
    /// Port to run the server on (defaults to 46673)
    #[arg(short, long)]
    pub port: Option<u16>,
}

#[derive(Args)]
pub struct AddServerArgs {
    /// Name of the server
    pub name: String,
    /// Connection URL (e.g., http://localhost:46673)
    pub url: String,
}

#[derive(Args)]
pub struct RemoveServerArgs {
    /// Name of the server to remove
    pub name: String,
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

pub async fn start_server(args: StartServerArgs) -> Result<()> {
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

pub async fn stop_server() -> Result<()> {
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

pub async fn restart_server() -> Result<()> {
    systemd_service::restart_service()?;
    println!("Server restarted");
    Ok(())
}

pub async fn run_server(args: RunServerArgs) -> Result<()> {
    boot_api::run(args.host, args.port).await
}

pub async fn list_servers() -> Result<()> {
    let config = server_config::ServerConfig::load()?;
    print_servers_table(config.list_servers(), None, None);
    Ok(())
}

pub async fn add_server(args: AddServerArgs) -> Result<()> {
    let mut config = server_config::ServerConfig::load()?;
    config.add_server(args.name.clone(), args.url.clone())?;
    config.save()?;
    
    print_servers_table(config.list_servers(), Some(&args.name), Some(Attribute::Bold));
    Ok(())
}

pub async fn remove_server(args: RemoveServerArgs) -> Result<()> {
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
