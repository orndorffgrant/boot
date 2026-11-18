// SPDX-License-Identifier: AGPL-3.0-or-later
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
use boot_protocol::{Change, ProbeArtifact};
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

#[derive(Debug, Clone)]
pub enum IdempotencyEntry {
    Pending {
        request_hash: String,
        created_at: i64,
    },
    Ready {
        request_hash: String,
        change_id: String,
        created_at: i64,
    },
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
    pub idempotency: Mutex<HashMap<(String, String), IdempotencyEntry>>,
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
                idempotency: Mutex::new(HashMap::new()),
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

    pub fn resource_by_selector(
        &self,
        kind: ResourceKind,
        selector: &str,
    ) -> Result<ResourceEntry, RuntimeError> {
        let selector = selector.trim();
        if selector.is_empty() {
            return Err(RuntimeError::new(
                "invalid_selector",
                "resource selector must not be empty",
            ));
        }
        let resources = self.resources();
        if let Some(resource) = resources
            .iter()
            .find(|resource| resource.kind == kind && resource.id == selector)
        {
            return Ok(resource.clone());
        }
        let matches: Vec<ResourceEntry> = resources
            .into_iter()
            .filter(|resource| resource.kind == kind && selector_matches(resource, selector))
            .collect();
        match matches.len() {
            0 => Err(RuntimeError::not_found(format!(
                "no {} matches '{}'",
                resource_kind_name(kind),
                selector
            ))),
            1 => Ok(matches.into_iter().next().expect("one match")),
            _ => {
                let candidates = matches
                    .iter()
                    .map(|resource| resource.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(RuntimeError::new(
                    "ambiguous_selector",
                    format!("selector '{selector}' matches multiple resources: {candidates}"),
                ))
            }
        }
    }

    pub fn probe_artifact(&self, resource_id: &str) -> Option<ProbeArtifact> {
        artifact::probe_artifact(&self.inner, resource_id)
    }

    pub async fn clear_probe_artifact(&self, resource_id: &str) -> Result<(), RuntimeError> {
        artifact::clear_probe_artifact(&self.inner, resource_id).await
    }

    pub fn idempotent_change(
        &self,
        principal: &str,
        key: &str,
        request_hash: &str,
    ) -> Result<Option<Change>, RuntimeError> {
        let lookup_key = (principal.to_string(), key.to_string());
        let mut entries = self.inner.idempotency.lock().expect("idempotency lock");
        let Some(entry) = entries.get(&lookup_key).cloned() else {
            return Ok(None);
        };
        if now_unix().saturating_sub(entry_created_at(&entry)) > 24 * 60 * 60 {
            entries.remove(&lookup_key);
            return Ok(None);
        }
        match entry {
            IdempotencyEntry::Pending { .. } => Err(RuntimeError {
                code: "idempotency_in_progress",
                message: "a request with this idempotency key is already in progress".to_string(),
                retry_after_seconds: Some(1),
            }),
            IdempotencyEntry::Ready {
                request_hash: stored,
                change_id,
                ..
            } => {
                if stored != request_hash {
                    return Err(RuntimeError::new(
                        "idempotency_conflict",
                        "idempotency key was reused with a different request",
                    ));
                }
                drop(entries);
                match self.change_handle(&change_id) {
                    Ok(handle) => Ok(Some(handle.snapshot())),
                    Err(_) => {
                        self.inner
                            .idempotency
                            .lock()
                            .expect("idempotency lock")
                            .remove(&lookup_key);
                        Ok(None)
                    }
                }
            }
        }
    }

    pub fn reserve_idempotency(
        &self,
        principal: &str,
        key: &str,
        request_hash: &str,
    ) -> Result<(), RuntimeError> {
        let lookup_key = (principal.to_string(), key.to_string());
        let mut entries = self.inner.idempotency.lock().expect("idempotency lock");
        if let Some(entry) = entries.get(&lookup_key) {
            if now_unix().saturating_sub(entry_created_at(entry)) <= 24 * 60 * 60 {
                return Err(RuntimeError::new(
                    "idempotency_in_progress",
                    "a request with this idempotency key is already in progress",
                ));
            }
        }
        entries.insert(
            lookup_key,
            IdempotencyEntry::Pending {
                request_hash: request_hash.to_string(),
                created_at: now_unix(),
            },
        );
        Ok(())
    }

    pub fn complete_idempotency(&self, principal: &str, key: &str, change_id: &str) {
        let lookup_key = (principal.to_string(), key.to_string());
        let mut entries = self.inner.idempotency.lock().expect("idempotency lock");
        let request_hash = entries
            .get(&lookup_key)
            .map(|entry| match entry {
                IdempotencyEntry::Pending { request_hash, .. }
                | IdempotencyEntry::Ready { request_hash, .. } => request_hash.clone(),
            })
            .unwrap_or_default();
        entries.insert(
            lookup_key,
            IdempotencyEntry::Ready {
                request_hash,
                change_id: change_id.to_string(),
                created_at: now_unix(),
            },
        );
    }

    pub fn cancel_idempotency(&self, principal: &str, key: &str) {
        self.inner
            .idempotency
            .lock()
            .expect("idempotency lock")
            .remove(&(principal.to_string(), key.to_string()));
    }
}

fn entry_created_at(entry: &IdempotencyEntry) -> i64 {
    match entry {
        IdempotencyEntry::Pending { created_at, .. }
        | IdempotencyEntry::Ready { created_at, .. } => *created_at,
    }
}

fn selector_matches(resource: &ResourceEntry, selector: &str) -> bool {
    resource.selector.as_deref() == Some(selector)
        || resource.path.as_deref() == Some(selector)
        || resource.name == selector
}

fn resource_kind_name(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Probe => "probe",
        ResourceKind::SerialPort => "serial port",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    async fn runtime(name: &str) -> (Runtime, PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "boot-runtime-selector-test-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let store = Arc::new(Store::open(directory.join("boot.sqlite3")).await.unwrap());
        (
            Runtime::new(
                store,
                RuntimeConfig::new(directory.join("sessions"), directory.join("artifacts")),
            ),
            directory,
        )
    }

    fn resource(
        id: &str,
        kind: ResourceKind,
        name: &str,
        selector: Option<&str>,
        path: Option<&str>,
    ) -> ResourceEntry {
        ResourceEntry {
            id: id.to_string(),
            kind,
            name: name.to_string(),
            selector: selector.map(str::to_string),
            path: path.map(str::to_string),
            generation: 1,
            state: "present".to_string(),
            identity_stable: true,
            vendor_id: Some(1),
            product_id: Some(2),
            serial_number: Some("serial".to_string()),
            probe_type: Some("probe".to_string()),
        }
    }

    #[tokio::test]
    async fn resolves_resources_by_id_selector_path_and_name() {
        let (runtime, directory) = runtime("resolve").await;
        runtime.inner.resources.lock().unwrap().insert(
            "probe_1".to_string(),
            resource(
                "probe_1",
                ResourceKind::Probe,
                "probe one",
                Some("0001:0002:serial"),
                None,
            ),
        );
        runtime.inner.resources.lock().unwrap().insert(
            "port_1".to_string(),
            resource(
                "port_1",
                ResourceKind::SerialPort,
                "port one",
                None,
                Some("/dev/ttyACM0"),
            ),
        );

        assert_eq!(
            runtime
                .resource_by_selector(ResourceKind::Probe, "probe_1")
                .unwrap()
                .id,
            "probe_1"
        );
        assert_eq!(
            runtime
                .resource_by_selector(ResourceKind::Probe, "0001:0002:serial")
                .unwrap()
                .id,
            "probe_1"
        );
        assert_eq!(
            runtime
                .resource_by_selector(ResourceKind::SerialPort, "/dev/ttyACM0")
                .unwrap()
                .id,
            "port_1"
        );
        assert!(
            runtime
                .resource_by_selector(ResourceKind::Probe, "/dev/ttyACM0")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn idempotency_reservations_and_conflicts_are_tracked() {
        let (runtime, directory) = runtime("idempotency").await;
        runtime
            .reserve_idempotency("principal", "key", "hash")
            .unwrap();
        let error = runtime
            .idempotent_change("principal", "key", "hash")
            .unwrap_err();
        assert_eq!(error.code, "idempotency_in_progress");
        runtime.complete_idempotency("principal", "key", "chg_missing");
        let error = runtime
            .idempotent_change("principal", "key", "different")
            .unwrap_err();
        assert_eq!(error.code, "idempotency_conflict");
        assert!(
            runtime
                .idempotent_change("principal", "key", "hash")
                .unwrap()
                .is_none()
        );
        runtime.cancel_idempotency("principal", "key");
        assert!(
            runtime
                .idempotent_change("principal", "key", "hash")
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn rejects_ambiguous_selectors() {
        let (runtime, directory) = runtime("ambiguous").await;
        runtime.inner.resources.lock().unwrap().insert(
            "probe_1".to_string(),
            resource("probe_1", ResourceKind::Probe, "same", None, None),
        );
        runtime.inner.resources.lock().unwrap().insert(
            "probe_2".to_string(),
            resource("probe_2", ResourceKind::Probe, "same", None, None),
        );
        let error = runtime
            .resource_by_selector(ResourceKind::Probe, "same")
            .unwrap_err();
        assert_eq!(error.code, "ambiguous_selector");
        let _ = std::fs::remove_dir_all(directory);
    }
}
