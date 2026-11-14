//! Runtime: resources, leases, changes, sessions, and persistence.

pub mod artifact;
pub mod change;
pub mod decode;
pub mod inventory;
pub mod lease;
pub mod session;
pub mod session_log;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use boot_protocol::ProbeArtifact;
use sha2::{Digest, Sha256};

use crate::auth::StoreError;
use crate::store::{RuntimeStore, Store};

pub use change::ChangeHandle;
pub use inventory::{ResourceEntry, ResourceKind};
pub use session::SessionHandle;

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub session_dir: PathBuf,
    pub artifact_dir: PathBuf,
    pub inventory_interval: Duration,
    pub idle_resource_ttl: Duration,
    pub lease_wait: Duration,
    pub max_subscribers: u32,
    pub max_change_history: usize,
}

impl RuntimeConfig {
    pub fn new(session_dir: PathBuf, artifact_dir: PathBuf) -> Self {
        Self {
            session_dir,
            artifact_dir,
            inventory_interval: Duration::from_secs(1),
            idle_resource_ttl: Duration::from_secs(60),
            lease_wait: Duration::from_secs(5),
            max_subscribers: 32,
            max_change_history: 256,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub code: &'static str,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
}

impl RuntimeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new("not_found", message)
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Self::new("resource_busy", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal_error", message)
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RuntimeError {}

impl From<StoreError> for RuntimeError {
    fn from(value: StoreError) -> Self {
        match value {
            StoreError::NotFound => Self::not_found("not found"),
            StoreError::Conflict => Self::new("conflict", "conflict"),
            StoreError::Invalid(message) => Self::new("invalid_request", message),
            StoreError::Database(message) | StoreError::Io(message) => {
                Self::internal(format!("database error: {message}"))
            }
        }
    }
}

pub struct RuntimeInner {
    pub store: Arc<Store>,
    pub config: RuntimeConfig,
    pub started: Instant,
    pub resources: Mutex<HashMap<String, ResourceEntry>>,
    pub leases: Mutex<HashMap<String, lease::LeaseEntry>>,
    pub changes: Mutex<HashMap<String, Arc<ChangeHandle>>>,
    pub sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    pub probe_artifacts: Mutex<HashMap<String, ProbeArtifact>>,
    pub decode_cache: Mutex<HashMap<String, Arc<decode::DecodeProfile>>>,
    pub generation: AtomicU64,
}

#[derive(Clone)]
pub struct Runtime {
    pub inner: Arc<RuntimeInner>,
}

impl Runtime {
    pub fn new(store: Arc<Store>, config: RuntimeConfig) -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                store,
                config,
                started: Instant::now(),
                resources: Mutex::new(HashMap::new()),
                leases: Mutex::new(HashMap::new()),
                changes: Mutex::new(HashMap::new()),
                sessions: Mutex::new(HashMap::new()),
                probe_artifacts: Mutex::new(HashMap::new()),
                decode_cache: Mutex::new(HashMap::new()),
                generation: AtomicU64::new(1),
            }),
        }
    }

    pub async fn start(&self) -> Result<()> {
        inventory::refresh(&self.inner).await?;
        change::restore(&self.inner).await?;
        artifact::restore_probe_artifacts(&self.inner).await?;
        self.inner
            .store
            .remove_artifact_refs_for_owner_kind("change")
            .await?;
        Self::restore_sessions(&self.inner).await?;
        for session in self.sessions_list() {
            if matches!(session.state.as_str(), "closed" | "failed") {
                let _ = self
                    .inner
                    .store
                    .remove_artifact_refs_for_owner("session", &session.id)
                    .await;
            }
        }
        let suspended: Vec<_> = self
            .sessions_list()
            .into_iter()
            .filter(|session| session.state == "suspended")
            .collect();
        for session in suspended {
            let _ = self.resume_session(&session.id).await;
        }
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(inner.config.inventory_interval);
            loop {
                interval.tick().await;
                if let Err(error) = inventory::refresh(&inner).await {
                    eprintln!("inventory refresh failed: {error:#}");
                }
            }
        });
        Ok(())
    }

    pub fn resources(&self) -> Vec<ResourceEntry> {
        let mut resources: Vec<_> = self
            .inner
            .resources
            .lock()
            .expect("resources lock")
            .values()
            .cloned()
            .collect();
        resources.sort_by(|a, b| a.id.cmp(&b.id));
        resources
    }

    pub fn resource(&self, id: &str) -> Option<ResourceEntry> {
        self.inner
            .resources
            .lock()
            .expect("resources lock")
            .get(id)
            .cloned()
    }

    pub fn probe_artifact(&self, resource_id: &str) -> Option<ProbeArtifact> {
        artifact::probe_artifact(&self.inner, resource_id)
    }

    pub async fn clear_probe_artifact(&self, resource_id: &str) -> Result<(), RuntimeError> {
        artifact::clear_probe_artifact(&self.inner, resource_id).await
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

pub fn rfc3339_from_unix(seconds: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .and_then(|time| {
            time.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn new_id(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{now:016x}{counter:04x}")
}

pub fn stable_id(prefix: &str, identity: &str) -> String {
    let digest = Sha256::digest(identity.as_bytes());
    format!("{prefix}_{}", hex(&digest[..8]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
