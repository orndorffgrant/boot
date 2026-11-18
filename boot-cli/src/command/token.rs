// SPDX-License-Identifier: AGPL-3.0-or-later
use clap::Args;

use crate::domain::{CreateTokenIntent, RevokeTokenIntent};

#[derive(Args)]
pub struct CreateTokenArgs {
    /// Server name. Omit with --direct.
    #[arg(value_name = "SERVER")]
    pub server: Option<String>,
    /// Open the local API database directly instead of using the HTTP API
    #[arg(long, conflicts_with = "server")]
    pub direct: bool,
    /// Human-readable token name
    #[arg(long)]
    pub name: String,
    /// Create a read-only token
    #[arg(long)]
    pub read_only: bool,
    /// Scope to grant (repeatable: read, control, admin)
    #[arg(long = "scope", value_name = "SCOPE")]
    pub scopes: Vec<String>,
    /// Expiry duration such as 30d, 12h, or 3600s
    #[arg(long, value_name = "DURATION")]
    pub expires_in: Option<String>,
    /// Replace an existing token with the same name
    #[arg(long)]
    pub replace: bool,
    /// Store the new token in the CLI DB for this server entry
    #[arg(long, value_name = "SERVER")]
    pub save_to: Option<String>,
}

impl From<CreateTokenArgs> for CreateTokenIntent {
    fn from(args: CreateTokenArgs) -> Self {
        Self {
            server: args.server,
            direct: args.direct,
            name: args.name,
            read_only: args.read_only,
            scopes: args.scopes,
            expires_in: args.expires_in,
            replace: args.replace,
            save_to: args.save_to,
        }
    }
}

#[derive(Args, Default)]
pub struct ListTokensArgs {
    /// Only list tokens for this server (defaults to all configured servers)
    #[arg(long, value_name = "SERVER")]
    pub server: Option<String>,
    /// Open the local API database directly instead of using the HTTP API
    #[arg(long, conflicts_with = "server")]
    pub direct: bool,
}

#[derive(Args)]
pub struct RevokeTokenArgs {
    /// Server/token selector, or the server name together with --all
    #[arg(value_name = "SERVER/TOKEN")]
    pub target: Option<String>,
    /// Open the local API database directly instead of using the HTTP API
    #[arg(long)]
    pub direct: bool,
    /// Revoke every token
    #[arg(long)]
    pub all: bool,
    /// Skip the interactive confirmation
    #[arg(long)]
    pub yes: bool,
}

impl From<RevokeTokenArgs> for RevokeTokenIntent {
    fn from(args: RevokeTokenArgs) -> Self {
        Self {
            target: args.target,
            direct: args.direct,
            all: args.all,
            yes: args.yes,
        }
    }
}
