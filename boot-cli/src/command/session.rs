// SPDX-License-Identifier: AGPL-3.0-or-later
use std::path::PathBuf;

use clap::Args;

use crate::domain::{ExportSessionIntent, ReplaySessionIntent, ResourceSelector, SessionSelect};
use crate::error::CliError;

#[derive(Args)]
pub struct ReplaySessionArgs {
    /// Server name and session ID in format: {server-name}/{session-id}
    pub server_and_session: String,
    /// Start replaying after this data sequence
    #[arg(long, default_value = "0")]
    pub from: u64,
    /// Include retained serial input records
    #[arg(long)]
    pub include_input: bool,
}

impl TryFrom<ReplaySessionArgs> for ReplaySessionIntent {
    type Error = CliError;

    fn try_from(args: ReplaySessionArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            target: ResourceSelector::parse(&args.server_and_session)?,
            from: args.from,
            include_input: args.include_input,
        })
    }
}

#[derive(Args)]
pub struct ExportSessionArgs {
    /// Server name and session ID in format: {server-name}/{session-id}
    pub server_and_session: String,
    /// Output file (.cast)
    #[arg(long, value_name = "PATH")]
    pub out: PathBuf,
    /// Export format
    #[arg(long, default_value = "asciicast-v3")]
    pub format: String,
    /// Include retained serial input
    #[arg(long)]
    pub include_input: bool,
}

impl TryFrom<ExportSessionArgs> for ExportSessionIntent {
    type Error = CliError;

    fn try_from(args: ExportSessionArgs) -> Result<Self, Self::Error> {
        Ok(Self {
            target: ResourceSelector::parse(&args.server_and_session)?,
            out: args.out,
            format: args.format,
            include_input: args.include_input,
        })
    }
}

#[derive(Args, Default)]
pub struct ListSessionsArgs {
    /// Only list sessions for this server
    #[arg(long, value_name = "SERVER")]
    pub server: Option<String>,
    /// Show only active sessions
    #[arg(long, conflicts_with_all = ["past", "all"])]
    pub active: bool,
    /// Show only retained/ended sessions
    #[arg(long, conflicts_with_all = ["active", "all"])]
    pub past: bool,
    /// Show active and retained sessions
    #[arg(long, conflicts_with_all = ["active", "past"])]
    pub all: bool,
}

impl From<ListSessionsArgs> for (Option<String>, SessionSelect) {
    fn from(args: ListSessionsArgs) -> Self {
        let select = if args.all {
            SessionSelect::All
        } else if args.past {
            SessionSelect::Past
        } else {
            SessionSelect::Active
        };
        (args.server, select)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_sessions_defaults_to_active() {
        let (_, select) = ListSessionsArgs::default().into();
        assert_eq!(select, SessionSelect::Active);
    }

    #[test]
    fn list_sessions_filters_map_to_select_values() {
        let (_, select) = ListSessionsArgs {
            all: true,
            ..Default::default()
        }
        .into();
        assert_eq!(select, SessionSelect::All);
        let (_, select) = ListSessionsArgs {
            past: true,
            ..Default::default()
        }
        .into();
        assert_eq!(select, SessionSelect::Past);
    }
}
