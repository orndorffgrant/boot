use anyhow::Ok;
use clap::{Parser, Subcommand};

mod server_config;
mod systemd_service;
mod server;
mod probe;
mod serial_port;

#[derive(Parser)]
#[command(name = "boot")]
struct CLI {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    // Server Management
    /// Start the boot server as a systemd service
    #[command(name = "start-server")]
    StartServer(server::StartServerArgs),
    
    /// Stop the boot server systemd service
    #[command(name = "stop-server")]
    StopServer,
    
    /// Restart the boot server systemd service
    #[command(name = "restart-server")]
    RestartServer,
    
    /// Run the boot server in the foreground
    #[command(name = "run-server")]
    RunServer(server::RunServerArgs),
    
    /// List configured servers
    #[command(name = "list-servers")]
    ListServers,
    
    /// Add a server to the configuration
    #[command(name = "add-server")]
    AddServer(server::AddServerArgs),
    
    /// Remove a server from the configuration
    #[command(name = "remove-server")]
    RemoveServer(server::RemoveServerArgs),
    
    // Debug Probes
    /// List available debug probes
    #[command(name = "list-probes")]
    ListProbes,
    
    /// Flash firmware and run on target
    #[command(name = "probe-run")]
    ProbeRun(probe::ProbeRunArgs),
    
    /// Attach to probe and stream RTT output
    #[command(name = "probe-attach")]
    ProbeAttach(probe::ProbeAttachArgs),
    
    // Serial Ports
    /// List available serial ports
    #[command(name = "list-serial-ports")]
    ListSerialPorts,
    
    /// Open an interactive serial shell
    #[command(name = "serial-shell")]
    SerialShell(serial_port::SerialShellArgs),
    
    // Other
    /// Print the OpenAPI specification
    #[command(name = "api-desc")]
    APIDesc,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CLI::parse();
    match cli.command {
        Command::StartServer(args) => server::start_server(args).await,
        Command::StopServer => server::stop_server().await,
        Command::RestartServer => server::restart_server().await,
        Command::RunServer(args) => server::run_server(args).await,
        Command::ListServers => server::list_servers().await,
        Command::AddServer(args) => server::add_server(args).await,
        Command::RemoveServer(args) => server::remove_server(args).await,
        Command::ListProbes => probe::list_probes().await,
        Command::ProbeRun(args) => probe::probe_run(args).await,
        Command::ProbeAttach(args) => probe::probe_attach(args).await,
        Command::ListSerialPorts => serial_port::list_serial_ports().await,
        Command::SerialShell(args) => serial_port::serial_shell(args).await,
        Command::APIDesc => api_desc().await,
    }
}

async fn api_desc() -> anyhow::Result<()> {
    let openapi = boot_api::openapi();
    println!("{}", openapi);
    Ok(())
}
