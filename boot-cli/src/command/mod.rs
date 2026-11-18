// SPDX-License-Identifier: AGPL-3.0-or-later
//! Clap definitions and conversion into terminal-independent command intents.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::domain::CommandIntent;
use crate::error::CliError;

pub mod help;
pub mod probe;
pub mod server;
pub mod session;
pub mod system;
pub mod token;

#[derive(Parser)]
#[command(name = "boot", version)]
pub struct CLI {
    /// Path to the CLI database (defaults to BOOT_CLI_DB or the boot data directory)
    #[arg(long, global = true, value_name = "PATH")]
    pub cli_db: Option<PathBuf>,

    /// Path to the API database (defaults to BOOT_API_DB or the boot data directory)
    #[arg(long, global = true, value_name = "PATH")]
    pub api_db: Option<PathBuf>,

    /// Suppress progress and informational output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Show the plan without executing it
    #[arg(long, global = true)]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Default)]
pub struct ListFilter {
    /// Only query this server
    #[arg(long, value_name = "SERVER")]
    pub server: Option<String>,
}

#[derive(Subcommand)]
pub enum Command {
    // Server management
    #[command(name = "start-server")]
    /// Start the boot server as a systemd service
    StartServer(server::ServerArgs),
    #[command(name = "stop-server")]
    /// Stop the boot server systemd service
    StopServer,
    #[command(name = "restart-server")]
    /// Restart the boot server systemd service
    RestartServer,
    #[command(name = "run-server")]
    /// Run the boot server in the foreground
    RunServer(server::ServerArgs),
    #[command(name = "list-servers")]
    /// List configured servers
    ListServers,
    #[command(name = "show-server")]
    /// Show a server's configuration, auth status, and local service state
    ShowServer(server::ShowServerArgs),
    #[command(name = "add-server")]
    /// Add a server and optionally store its token
    AddServer(server::AddServerArgs),
    #[command(name = "edit-server")]
    /// Edit a server's URL, CA certificate, or token
    EditServer(server::EditServerArgs),
    #[command(name = "remove-server")]
    /// Remove a server and its stored token
    RemoveServer(server::RemoveServerArgs),

    // Token administration
    #[command(name = "create-token")]
    /// Create a server-side API token
    CreateToken(token::CreateTokenArgs),
    #[command(name = "list-tokens")]
    /// List server-side API token metadata
    ListTokens(token::ListTokensArgs),
    #[command(name = "revoke-token")]
    /// Revoke a server-side API token, or all tokens
    RevokeToken(token::RevokeTokenArgs),

    // Debug probes
    #[command(name = "list-probes")]
    /// List available debug probes
    ListProbes(ListFilter),
    #[command(name = "run-firmware")]
    /// Flash firmware and optionally attach
    RunFirmware(probe::RunFirmwareArgs),
    #[command(name = "attach-probe")]
    /// Attach to probe and stream RTT output
    AttachProbe(probe::AttachProbeArgs),
    #[command(name = "start-dap")]
    /// Start a DAP (Debug Adapter Protocol) server for a probe
    StartDap(probe::StartDapArgs),

    // Serial ports
    #[command(name = "list-serial-ports")]
    /// List available serial ports
    ListSerialPorts(ListFilter),
    #[command(name = "attach-serial")]
    /// Attach to a serial session and open an interactive shell
    AttachSerial(probe::AttachSerialArgs),

    // Sessions
    #[command(name = "list-sessions")]
    /// List active sessions (use --all for retained sessions)
    ListSessions(session::ListSessionsArgs),
    #[command(name = "replay-session")]
    /// Replay a retained session
    ReplaySession(session::ReplaySessionArgs),
    #[command(name = "export-session")]
    /// Export a retained session as asciicast v3
    ExportSession(session::ExportSessionArgs),

    // System
    #[command(name = "update")]
    /// Update the locally installed boot binary
    Update(system::UpdateArgs),
    #[command(name = "show-openapi")]
    /// Print the OpenAPI specification
    ShowOpenapi,

    // Grouped help
    #[command(name = "help-server")]
    /// Show grouped help for server commands
    HelpServer,
    #[command(name = "help-token")]
    /// Show grouped help for token commands
    HelpToken,
    #[command(name = "help-probe")]
    /// Show grouped help for probe commands
    HelpProbe,
    #[command(name = "help-serial")]
    /// Show grouped help for serial commands
    HelpSerial,
    #[command(name = "help-session")]
    /// Show grouped help for session commands
    HelpSession,
    #[command(name = "help-firmware")]
    /// Show grouped help for firmware commands
    HelpFirmware,
    #[command(name = "help-dap")]
    /// Show grouped help for DAP commands
    HelpDap,
    #[command(name = "help-openapi")]
    /// Show grouped help for OpenAPI commands
    HelpOpenapi,
}

impl Command {
    pub fn help_group(&self) -> Option<&'static str> {
        match self {
            Self::HelpServer => Some("help-server"),
            Self::HelpToken => Some("help-token"),
            Self::HelpProbe => Some("help-probe"),
            Self::HelpSerial => Some("help-serial"),
            Self::HelpSession => Some("help-session"),
            Self::HelpFirmware => Some("help-firmware"),
            Self::HelpDap => Some("help-dap"),
            Self::HelpOpenapi => Some("help-openapi"),
            _ => None,
        }
    }

    pub fn intent(self) -> Result<CommandIntent, CliError> {
        Ok(match self {
            Self::StartServer(args) => CommandIntent::StartServer(args.into()),
            Self::StopServer => CommandIntent::StopServer,
            Self::RestartServer => CommandIntent::RestartServer,
            Self::RunServer(args) => CommandIntent::RunServer(args.into()),
            Self::ListServers => CommandIntent::ListServers,
            Self::ShowServer(args) => CommandIntent::ShowServer(args.into()),
            Self::AddServer(args) => CommandIntent::AddServer(args.into()),
            Self::EditServer(args) => CommandIntent::EditServer(args.into()),
            Self::RemoveServer(args) => CommandIntent::RemoveServer(args.name),
            Self::CreateToken(args) => CommandIntent::CreateToken(args.into()),
            Self::ListTokens(args) => CommandIntent::ListTokens {
                server: args.server,
                direct: args.direct,
            },
            Self::RevokeToken(args) => CommandIntent::RevokeToken(args.into()),
            Self::ListProbes(args) => CommandIntent::ListProbes {
                server: args.server,
            },
            Self::RunFirmware(args) => CommandIntent::RunFirmware(args.try_into()?),
            Self::AttachProbe(args) => CommandIntent::AttachProbe(args.try_into()?),
            Self::StartDap(args) => {
                let (target, port) = args.try_into()?;
                CommandIntent::StartDap { target, port }
            }
            Self::ListSerialPorts(args) => CommandIntent::ListSerialPorts {
                server: args.server,
            },
            Self::AttachSerial(args) => CommandIntent::AttachSerial(args.try_into()?),
            Self::ListSessions(args) => {
                let (server, select) = args.into();
                CommandIntent::ListSessions { server, select }
            }
            Self::ReplaySession(args) => CommandIntent::ReplaySession(args.try_into()?),
            Self::ExportSession(args) => CommandIntent::ExportSession(args.try_into()?),
            Self::Update(args) => CommandIntent::Update(args.into()),
            Self::ShowOpenapi => CommandIntent::ShowOpenApi,
            Self::HelpServer
            | Self::HelpToken
            | Self::HelpProbe
            | Self::HelpSerial
            | Self::HelpSession
            | Self::HelpFirmware
            | Self::HelpDap
            | Self::HelpOpenapi => {
                return Err(CliError::usage(
                    "help command is handled before intent conversion",
                ));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_commands_and_global_flags() {
        for args in [
            vec!["boot", "run-firmware", "local/probe", "firmware.elf"],
            vec!["boot", "attach-probe", "local/probe", "--follow"],
            vec!["boot", "attach-serial", "local/tty", "--new"],
            vec!["boot", "list-probes", "--server", "pi"],
            vec!["boot", "list-serial-ports", "--server", "pi"],
            vec!["boot", "list-sessions", "--all"],
            vec!["boot", "create-token", "local", "--name", "ci"],
            vec!["boot", "update", "--check"],
            vec!["boot", "--dry-run", "list-servers"],
            vec!["boot", "--quiet", "show-server"],
        ] {
            CLI::try_parse_from(args).unwrap();
        }
    }

    #[test]
    fn rejects_conflicting_session_and_attach_flags() {
        assert!(
            CLI::try_parse_from([
                "boot",
                "attach-probe",
                "local/probe",
                "--session",
                "ses_1",
                "--follow",
            ])
            .is_err()
        );
        assert!(
            CLI::try_parse_from([
                "boot",
                "run-firmware",
                "local/probe",
                "firmware.elf",
                "--no-attach",
                "--serial-shell",
                "local/tty",
            ])
            .is_err()
        );
    }
}
