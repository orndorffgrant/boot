// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native DAP backend for boot.
//!
//! This crate owns DAP framing, request dispatch, and debugger state. It does
//! not define the boot session model; that lives in `boot-api`.

mod codec;
mod native;
mod svd;

use std::path::PathBuf;
use std::sync::Arc;

pub use native::NativeDapBackend;

#[derive(Debug, thiserror::Error)]
pub enum DapError {
    #[error("{0}")]
    Other(String),
    #[error("artifact '{0}' is unavailable")]
    Artifact(String),
}

pub struct DapIo {
    /// DAP bytes from the bridge.
    pub input: std::sync::mpsc::Receiver<Vec<u8>>,
    /// DAP bytes to the bridge.
    pub output: tokio::sync::mpsc::Sender<Vec<u8>>,
}

pub struct DapLaunch {
    /// Authoritative probe selector from the boot resource.
    pub probe_selector: String,
    /// Optional server-side ELF/artifact path.
    pub program_override: Option<PathBuf>,
    /// Resolves `bootArtifacts` IDs from the launch configuration.
    pub artifacts: Arc<dyn DapArtifactResolver>,
}

pub trait DapArtifactResolver: Send + Sync {
    fn resolve(&self, artifact_id: &str) -> Result<PathBuf, DapError>;
}

pub trait DapBackend: Send {
    fn run(
        &mut self,
        io: DapIo,
        launch: DapLaunch,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), DapError>;
}
