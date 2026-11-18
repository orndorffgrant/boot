// SPDX-License-Identifier: AGPL-3.0-or-later
use std::path::PathBuf;

use clap::Args;

use crate::domain::{
    ArtifactSource, AttachPolicy, AttachProbeIntent, AttachSerialIntent, ResourceSelector,
    RunFirmwareIntent, SessionAccess, SessionAcquire,
};
use crate::error::CliError;

#[derive(Args)]
pub struct RunFirmwareArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    pub server_and_selector: String,
    /// Path to the ELF file
    pub elf_path: PathBuf,
    /// Don't attach and show defmt logs after flashing
    #[arg(long, conflicts_with = "serial_shell")]
    pub no_attach: bool,
    /// Open serial shell instead of attaching (format: {server-name}/{port-id})
    #[arg(short = 's', long = "serial-shell")]
    pub serial_shell: Option<String>,
}

impl TryFrom<RunFirmwareArgs> for RunFirmwareIntent {
    type Error = CliError;

    fn try_from(args: RunFirmwareArgs) -> Result<Self, Self::Error> {
        let attach = if args.no_attach {
            AttachPolicy::None
        } else if let Some(target) = args.serial_shell {
            AttachPolicy::Serial(ResourceSelector::parse(&target)?)
        } else {
            AttachPolicy::Rtt
        };
        Ok(Self {
            target: ResourceSelector::parse(&args.server_and_selector)?,
            firmware: args.elf_path,
            attach,
        })
    }
}

#[derive(Args)]
pub struct AttachProbeArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    pub server_and_selector: String,
    /// Upload this ELF and use it for RTT address lookup and defmt decoding
    #[arg(long, conflicts_with_all = ["artifact", "clear_artifact"])]
    pub elf: Option<PathBuf>,
    /// Use an existing artifact for RTT address lookup and defmt decoding
    #[arg(long, conflicts_with_all = ["elf", "clear_artifact"])]
    pub artifact: Option<String>,
    /// Clear the session decoder, or opt out of the inherited probe artifact
    #[arg(long, conflicts_with_all = ["elf", "artifact"])]
    pub clear_artifact: bool,
    /// Attach to an existing session only
    #[arg(long, conflicts_with = "new")]
    pub follow: bool,
    /// Create a new session only
    #[arg(long)]
    pub new: bool,
    /// Attach read-only
    #[arg(long)]
    pub read_only: bool,
    /// Attach to a specific session ID
    #[arg(long, conflicts_with_all = ["follow", "new"])]
    pub session: Option<String>,
}

impl TryFrom<AttachProbeArgs> for AttachProbeIntent {
    type Error = CliError;

    fn try_from(args: AttachProbeArgs) -> Result<Self, Self::Error> {
        let artifact = if let Some(path) = args.elf {
            ArtifactSource::Upload(path)
        } else if let Some(artifact) = args.artifact {
            ArtifactSource::Existing(artifact)
        } else if args.clear_artifact {
            ArtifactSource::Clear
        } else {
            ArtifactSource::Inherit
        };
        let acquire = if args.session.is_some() {
            SessionAcquire::AttachOrCreate
        } else if args.follow {
            SessionAcquire::AttachOnly
        } else if args.new {
            SessionAcquire::CreateOnly
        } else {
            SessionAcquire::AttachOrCreate
        };
        Ok(Self {
            target: ResourceSelector::parse(&args.server_and_selector)?,
            artifact,
            acquire,
            access: if args.read_only {
                SessionAccess::Read
            } else {
                SessionAccess::Write
            },
            session: args.session,
        })
    }
}

#[derive(Args)]
pub struct StartDapArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    pub server_and_selector: String,
    /// Port to listen on for DAP connections (default: 50000)
    #[arg(short, long, default_value = "50000")]
    pub port: u16,
}

impl TryFrom<StartDapArgs> for (ResourceSelector, u16) {
    type Error = CliError;

    fn try_from(args: StartDapArgs) -> Result<Self, Self::Error> {
        Ok((
            ResourceSelector::parse(&args.server_and_selector)?,
            args.port,
        ))
    }
}

#[derive(Args)]
pub struct AttachSerialArgs {
    /// Server name and serial port ID in format: {server-name}/{port-id}
    pub server_and_port: String,
    /// Attach to an existing session only
    #[arg(long, conflicts_with = "new")]
    pub follow: bool,
    /// Create a new session only
    #[arg(long)]
    pub new: bool,
    /// Attach read-only
    #[arg(long)]
    pub read_only: bool,
    /// Attach to a specific session ID
    #[arg(long, conflicts_with_all = ["follow", "new"])]
    pub session: Option<String>,
}

impl TryFrom<AttachSerialArgs> for AttachSerialIntent {
    type Error = CliError;

    fn try_from(args: AttachSerialArgs) -> Result<Self, Self::Error> {
        let acquire = if args.session.is_some() {
            SessionAcquire::AttachOrCreate
        } else if args.follow {
            SessionAcquire::AttachOnly
        } else if args.new {
            SessionAcquire::CreateOnly
        } else {
            SessionAcquire::AttachOrCreate
        };
        Ok(Self {
            target: ResourceSelector::parse(&args.server_and_port)?,
            acquire,
            access: if args.read_only {
                SessionAccess::Read
            } else {
                SessionAccess::Write
            },
            session: args.session,
            baud_rate: 115200,
        })
    }
}
