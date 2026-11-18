// SPDX-License-Identifier: AGPL-3.0-or-later
use clap::Args;

use crate::domain::UpdateIntent;

#[derive(Args)]
pub struct UpdateArgs {
    /// Report the installed and available versions without changing anything
    #[arg(long)]
    pub check: bool,
    /// Install a specific release tag instead of the latest release
    #[arg(long, value_name = "TAG")]
    pub version: Option<String>,
    /// Reinstall the same version or allow a downgrade
    #[arg(long)]
    pub force: bool,
    /// Skip the confirmation prompt
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Replace the binary but leave the running service on the old process
    #[arg(long)]
    pub no_restart: bool,
    /// Use a fork or mirror (OWNER/REPO)
    #[arg(long, value_name = "OWNER/REPO")]
    pub repo: Option<String>,
}

impl From<UpdateArgs> for UpdateIntent {
    fn from(args: UpdateArgs) -> Self {
        Self {
            check: args.check,
            version: args.version,
            force: args.force,
            yes: args.yes,
            no_restart: args.no_restart,
            repo: args.repo,
        }
    }
}
