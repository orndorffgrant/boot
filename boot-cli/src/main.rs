use anyhow::Ok;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod admin;
mod auth;
mod cli_db;
mod probe;
mod serial_port;
mod server;
mod session;
mod systemd_service;
mod update;

#[derive(Parser)]
#[command(name = "boot", version)]
struct CLI {
    /// Path to the CLI database (defaults to BOOT_CLI_DB or the boot data directory)
    #[arg(long, global = true, value_name = "PATH")]
    cli_db: Option<PathBuf>,

    /// Path to the API database (defaults to BOOT_API_DB or the boot data directory)
    #[arg(long, global = true, value_name = "PATH")]
    api_db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    // Server Management
    /// Start the boot server as a systemd service
    #[command(name = "start-server")]
    StartServer(server::ServerArgs),

    /// Stop the boot server systemd service
    #[command(name = "stop-server")]
    StopServer,

    /// Restart the boot server systemd service
    #[command(name = "restart-server")]
    RestartServer,

    /// Run the boot server in the foreground
    #[command(name = "run-server")]
    RunServer(server::ServerArgs),

    /// List configured servers
    #[command(name = "list-servers")]
    ListServers,

    /// Add a server and optionally store its token
    #[command(name = "add-server")]
    AddServer(server::AddServerArgs),

    /// Store a token for an existing server
    #[command(name = "set-token")]
    SetToken(auth::SetTokenArgs),

    /// Check reachability and token scopes for a server
    #[command(name = "check-auth")]
    CheckAuth(server::CheckAuthArgs),

    /// Remove a server and its stored token
    #[command(name = "remove-server")]
    RemoveServer(server::RemoveServerArgs),

    // Server-side token administration
    /// Create, list, and revoke API tokens
    #[command(name = "admin")]
    Admin(admin::AdminArgs),

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

    /// Start a DAP (Debug Adapter Protocol) server for a probe
    #[command(name = "start-probe-dap")]
    StartProbeDap(probe::StartProbeDapArgs),

    // Serial Ports
    /// List available serial ports
    #[command(name = "list-serial-ports")]
    ListSerialPorts,

    /// Open an interactive serial shell
    #[command(name = "serial-shell")]
    SerialShell(serial_port::SerialShellArgs),

    // Sessions
    /// List live and retained sessions
    #[command(name = "list-sessions")]
    ListSessions,

    /// Replay a retained session
    #[command(name = "session-replay")]
    SessionReplay(session::SessionReplayArgs),

    /// Export a retained session as asciicast v3
    #[command(name = "session-export")]
    SessionExport(session::SessionExportArgs),

    // Other
    /// Print the OpenAPI specification
    #[command(name = "api-desc")]
    APIDesc,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CLI::parse();
    let cli_db = cli.cli_db.as_deref();
    let api_db = cli.api_db.as_deref();
    match cli.command {
        Command::StartServer(args) => server::start_server(args, cli_db, api_db).await,
        Command::StopServer => server::stop_server(cli_db).await,
        Command::RestartServer => server::restart_server().await,
        Command::RunServer(args) => server::run_server(args, cli_db, api_db).await,
        Command::ListServers => server::list_servers(cli_db).await,
        Command::AddServer(args) => server::add_server(args, cli_db).await,
        Command::SetToken(args) => server::set_token(args, cli_db).await,
        Command::CheckAuth(args) => server::check_auth(args, cli_db).await,
        Command::RemoveServer(args) => server::remove_server(args, cli_db).await,
        Command::Admin(args) => admin::admin(args, cli_db, api_db).await,
        Command::ListProbes => probe::list_probes(cli_db).await,
        Command::ProbeRun(args) => probe::probe_run(args, cli_db).await,
        Command::ProbeAttach(args) => probe::probe_attach(args, cli_db).await,
        Command::StartProbeDap(args) => probe::start_probe_dap(args, cli_db).await,
        Command::ListSerialPorts => serial_port::list_serial_ports(cli_db).await,
        Command::SerialShell(args) => serial_port::serial_shell(args, cli_db).await,
        Command::ListSessions => session::list_sessions(cli_db).await,
        Command::SessionReplay(args) => session::replay_session(args, cli_db).await,
        Command::SessionExport(args) => session::export_session(args, cli_db).await,
        Command::APIDesc => api_desc().await,
    }
}

async fn api_desc() -> anyhow::Result<()> {
    let openapi = boot_api::openapi();
    println!("{}", openapi);
    Ok(())
}
