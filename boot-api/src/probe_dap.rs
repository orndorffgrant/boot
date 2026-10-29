//! Debug Adapter Protocol (DAP) support.
//!
//! The DAP server backend is not implemented yet. [`DAP_SUPPORTED`] is `false` so the
//! `/v1/probes/{selector}/dap` endpoint can fail with a clear error instead of accepting
//! a WebSocket that would never produce any data.

/// Whether the DAP server backend is implemented.
pub const DAP_SUPPORTED: bool = false;

/// Runs a DAP session over the given reader/writer pair.
///
/// Not implemented yet; always returns an error.
#[allow(dead_code)]
pub async fn run_dap_session(
    _reader: impl std::io::Read + Send + 'static,
    _writer: impl std::io::Write + Send + 'static,
    _lister: &probe_rs::probe::list::Lister,
    _log_path: Option<std::path::PathBuf>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err("DAP support is not implemented yet".into())
}
