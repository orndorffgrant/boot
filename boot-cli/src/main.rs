use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod admin;
mod auth;
mod cli_db;
mod help;
mod probe;
mod serial_port;
mod server;
mod session;
mod systemd_service;
mod update;

#[derive(Parser)]
#[command(name = "boot", version)]
pub(crate) struct CLI {
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
    // Server management
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

    /// Show a server's configuration, auth status, and local service state
    #[command(name = "show-server")]
    ShowServer(server::ShowServerArgs),

    /// Add a server and optionally store its token
    #[command(name = "add-server")]
    AddServer(server::AddServerArgs),

    /// Edit a server's URL, CA certificate, or token
    #[command(name = "edit-server")]
    EditServer(server::EditServerArgs),

    /// Remove a server and its stored token
    #[command(name = "remove-server")]
    RemoveServer(server::RemoveServerArgs),

    // Token administration
    /// Create a server-side API token
    #[command(name = "create-token")]
    CreateToken(admin::CreateTokenArgs),

    /// List server-side API token metadata
    #[command(name = "list-tokens")]
    ListTokens(admin::ListTokensArgs),

    /// Revoke a server-side API token, or all tokens
    #[command(name = "revoke-token")]
    RevokeToken(admin::RevokeTokenArgs),

    // Debug probes
    /// List available debug probes
    #[command(name = "list-probes")]
    ListProbes,

    /// Flash firmware and optionally attach
    #[command(name = "run-firmware")]
    RunFirmware(probe::RunFirmwareArgs),

    /// Attach to probe and stream RTT output
    #[command(name = "attach-probe")]
    AttachProbe(probe::AttachProbeArgs),

    /// Start a DAP (Debug Adapter Protocol) server for a probe
    #[command(name = "start-dap")]
    StartDap(probe::StartDapArgs),

    // Serial ports
    /// List available serial ports
    #[command(name = "list-serial-ports")]
    ListSerialPorts,

    /// Attach to a serial session and open an interactive shell
    #[command(name = "attach-serial")]
    AttachSerial(serial_port::AttachSerialArgs),

    // Sessions
    /// List live and retained sessions
    #[command(name = "list-sessions")]
    ListSessions,

    /// Replay a retained session
    #[command(name = "replay-session")]
    ReplaySession(session::ReplaySessionArgs),

    /// Export a retained session as asciicast v3
    #[command(name = "export-session")]
    ExportSession(session::ExportSessionArgs),

    // System
    /// Update the locally installed boot binary
    #[command(name = "update")]
    Update(update::UpdateArgs),

    /// Print the OpenAPI specification
    #[command(name = "show-openapi")]
    ShowOpenapi,

    // Grouped help
    /// Show grouped help for server commands
    #[command(name = "help-server")]
    HelpServer,

    /// Show grouped help for token commands
    #[command(name = "help-token")]
    HelpToken,

    /// Show grouped help for probe commands
    #[command(name = "help-probe")]
    HelpProbe,

    /// Show grouped help for serial commands
    #[command(name = "help-serial")]
    HelpSerial,

    /// Show grouped help for session commands
    #[command(name = "help-session")]
    HelpSession,

    /// Show grouped help for firmware commands
    #[command(name = "help-firmware")]
    HelpFirmware,

    /// Show grouped help for DAP commands
    #[command(name = "help-dap")]
    HelpDap,

    /// Show grouped help for OpenAPI commands
    #[command(name = "help-openapi")]
    HelpOpenapi,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args_os().collect::<Vec<_>>();
    if let Some(code) = help::handle(&args)? {
        std::process::exit(code);
    }

    let cli = CLI::parse();
    let cli_db = cli.cli_db.as_deref();
    let api_db = cli.api_db.as_deref();
    match cli.command {
        Command::StartServer(args) => server::start_server(args, cli_db, api_db).await,
        Command::StopServer => server::stop_server(cli_db).await,
        Command::RestartServer => server::restart_server().await,
        Command::RunServer(args) => server::run_server(args, cli_db, api_db).await,
        Command::ListServers => server::list_servers(cli_db).await,
        Command::ShowServer(args) => server::show_server(args, cli_db).await,
        Command::AddServer(args) => server::add_server(args, cli_db).await,
        Command::EditServer(args) => server::edit_server(args, cli_db).await,
        Command::RemoveServer(args) => server::remove_server(args, cli_db).await,
        Command::CreateToken(args) => admin::create_token(args, cli_db, api_db).await,
        Command::ListTokens(args) => admin::list_tokens(args, cli_db, api_db).await,
        Command::RevokeToken(args) => admin::revoke_token(args, cli_db, api_db).await,
        Command::ListProbes => probe::list_probes(cli_db).await,
        Command::RunFirmware(args) => probe::run_firmware(args, cli_db).await,
        Command::AttachProbe(args) => probe::attach_probe(args, cli_db).await,
        Command::StartDap(args) => probe::start_dap(args, cli_db).await,
        Command::ListSerialPorts => serial_port::list_serial_ports(cli_db).await,
        Command::AttachSerial(args) => serial_port::attach_serial(args, cli_db).await,
        Command::ListSessions => session::list_sessions(cli_db).await,
        Command::ReplaySession(args) => session::replay_session(args, cli_db).await,
        Command::ExportSession(args) => session::export_session(args, cli_db).await,
        Command::Update(args) => update::update(args).await,
        Command::ShowOpenapi => show_openapi().await,
        Command::HelpServer => help::print_focused("help-server"),
        Command::HelpToken => help::print_focused("help-token"),
        Command::HelpProbe => help::print_focused("help-probe"),
        Command::HelpSerial => help::print_focused("help-serial"),
        Command::HelpSession => help::print_focused("help-session"),
        Command::HelpFirmware => help::print_focused("help-firmware"),
        Command::HelpDap => help::print_focused("help-dap"),
        Command::HelpOpenapi => help::print_focused("help-openapi"),
    }
}

async fn show_openapi() -> anyhow::Result<()> {
    let openapi = boot_api::openapi();
    println!("{}", openapi);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_verb_noun_commands() {
        for args in [
            vec!["boot", "run-firmware", "local/probe", "firmware.elf"],
            vec!["boot", "attach-probe", "local/probe"],
            vec!["boot", "start-dap", "local/probe"],
            vec!["boot", "attach-serial", "local/tty"],
            vec!["boot", "replay-session", "local/ses"],
            vec![
                "boot",
                "export-session",
                "local/ses",
                "--out",
                "session.cast",
            ],
            vec!["boot", "show-server"],
            vec!["boot", "show-server", "pi"],
            vec!["boot", "edit-server", "pi", "--token-stdin"],
            vec!["boot", "create-token", "pi", "--name", "ci"],
            vec!["boot", "create-token", "--direct", "--name", "ci"],
            vec!["boot", "list-tokens"],
            vec!["boot", "list-tokens", "--server", "pi"],
            vec!["boot", "list-tokens", "--direct"],
            vec!["boot", "revoke-token", "pi/ci", "--yes"],
            vec!["boot", "revoke-token", "pi", "--all", "--yes"],
            vec!["boot", "revoke-token", "--direct", "ci", "--yes"],
            vec!["boot", "update", "--check"],
            vec!["boot", "show-openapi"],
            vec!["boot", "help-server"],
            vec!["boot", "help-token"],
            vec!["boot", "help-probe"],
            vec!["boot", "help-serial"],
            vec!["boot", "help-session"],
            vec!["boot", "help-firmware"],
            vec!["boot", "help-dap"],
            vec!["boot", "help-openapi"],
        ] {
            CLI::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn rejects_removed_command_names() {
        for args in [
            vec!["boot", "probe-run", "local/probe", "firmware.elf"],
            vec!["boot", "probe-attach", "local/probe"],
            vec!["boot", "session-replay", "local/ses"],
            vec!["boot", "serial-shell", "local/tty"],
            vec!["boot", "set-token", "pi"],
            vec!["boot", "check-auth", "pi"],
            vec!["boot", "admin", "list-tokens"],
        ] {
            assert!(CLI::try_parse_from(args).is_err());
        }
    }
}
