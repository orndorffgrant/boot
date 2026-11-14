use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use boot_protocol::{RttRecord, Session, SessionData, SessionDecode, SessionPatch, Subscriber};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

use crate::runtime::decode::{self, DecodeProfile, DecodeState};
use crate::runtime::lease::LeaseGuard;
use crate::runtime::session_log::{
    self, LogRecord, RECORD_DECODED, RECORD_INPUT, RECORD_OUTPUT, SessionLogWriter,
};
use crate::runtime::{Runtime, RuntimeError, RuntimeInner, lease, new_id, now_rfc3339, now_unix};
use crate::store::RuntimeStore;
use crate::store::SessionRecord;

#[derive(Debug, Clone)]
enum ThreadEvent {
    Active,
    Data { data: Vec<u8>, channel: u8 },
    Error(String),
    Ended,
}

#[derive(Debug, Clone)]
pub enum SessionEventData {
    Bytes(Vec<u8>),
    RttRecords {
        epoch: u64,
        records: Vec<RttRecord>,
        raw: Option<Vec<u8>>,
        error: Option<String>,
    },
    RttRaw {
        epoch: u64,
        data: Vec<u8>,
        error: Option<String>,
    },
    Input(Vec<u8>),
}

impl SessionEventData {
    fn len(&self) -> usize {
        match self {
            Self::Bytes(data) | Self::RttRaw { data, .. } | Self::Input(data) => data.len(),
            Self::RttRecords { records, raw, .. } => {
                records
                    .iter()
                    .map(|record| record.message.len() + 64)
                    .sum::<usize>()
                    + raw.as_ref().map(Vec::len).unwrap_or(0)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub seq: u64,
    pub channel: u8,
    pub timestamp_ms: u64,
    pub data: SessionEventData,
}

#[derive(Debug, serde::Deserialize)]
struct DecodedPayload {
    #[serde(default)]
    epoch: u64,
    #[serde(default)]
    records: Vec<RttRecord>,
    #[serde(default)]
    raw: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

struct SessionRing {
    chunks: VecDeque<SessionEvent>,
    bytes: usize,
    max_bytes: usize,
    latest_seq: u64,
}

impl SessionRing {
    fn new(max_bytes: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
            max_bytes,
            latest_seq: 0,
        }
    }

    fn push(&mut self, channel: u8, data: SessionEventData) -> u64 {
        let seq = self.latest_seq + 1;
        self.latest_seq = seq;
        let timestamp_ms = now_unix() as u64 * 1000;
        self.bytes += data.len().max(64);
        self.chunks.push_back(SessionEvent {
            seq,
            channel,
            timestamp_ms,
            data,
        });
        while self.bytes > self.max_bytes {
            if let Some(chunk) = self.chunks.pop_front() {
                self.bytes = self.bytes.saturating_sub(chunk.data.len().max(64));
            } else {
                break;
            }
        }
        seq
    }

    fn after(&self, cursor: u64) -> Vec<SessionEvent> {
        self.chunks
            .iter()
            .filter(|chunk| chunk.seq > cursor)
            .cloned()
            .collect()
    }
}

pub struct SessionHandle {
    pub id: String,
    pub kind: String,
    pub resource_id: String,
    pub log_dir: std::path::PathBuf,
    pub options_json: String,
    pub state: Mutex<Session>,
    ring: Mutex<SessionRing>,
    pub notify: Notify,
    pub subscriber_changed: Notify,
    pub subscribers: Mutex<Vec<Subscriber>>,
    pub input_tx: Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>>,
    pub lease: Mutex<Option<LeaseGuard>>,
    pub shutdown: CancellationToken,
    pub log: Mutex<Option<SessionLogWriter>>,
    pub decode: Mutex<Option<DecodeState>>,
    pub lifecycle_tx: broadcast::Sender<Session>,
}

impl SessionHandle {
    fn new(session: Session, log_dir: std::path::PathBuf, options_json: String) -> Self {
        let max_bytes = if session.kind == "rtt" {
            1024 * 1024
        } else {
            256 * 1024
        };
        let (lifecycle_tx, _) = broadcast::channel(64);
        Self {
            id: session.id.clone(),
            kind: session.kind.clone(),
            resource_id: session.resource_id.clone(),
            log_dir,
            options_json,
            state: Mutex::new(session),
            ring: Mutex::new(SessionRing::new(max_bytes)),
            notify: Notify::new(),
            subscriber_changed: Notify::new(),
            subscribers: Mutex::new(Vec::new()),
            input_tx: Mutex::new(None),
            lease: Mutex::new(None),
            shutdown: CancellationToken::new(),
            log: Mutex::new(None),
            decode: Mutex::new(None),
            lifecycle_tx,
        }
    }

    pub fn snapshot(&self) -> Session {
        self.state.lock().expect("session state").clone()
    }

    fn update(&self, session: Session) {
        *self.state.lock().expect("session state") = session.clone();
        let _ = self.lifecycle_tx.send(session);
    }

    fn push_output(&self, inner: &RuntimeInner, data: Vec<u8>, channel: u8) {
        let mut log_epoch = self.snapshot().decode.epoch;
        let (event, decoded) = if self.kind == "rtt" {
            let mut guard = self.decode.lock().expect("session decode");
            if let Some(state) = guard.as_mut() {
                let epoch = state.epoch();
                log_epoch = epoch;
                let chunk = state.decode(channel, &data);
                let event = SessionEventData::RttRecords {
                    epoch,
                    records: chunk.records.clone(),
                    raw: chunk.raw.clone(),
                    error: chunk.error.clone(),
                };
                (event, Some(chunk))
            } else {
                (
                    SessionEventData::RttRaw {
                        epoch: log_epoch,
                        data: data.clone(),
                        error: None,
                    },
                    None,
                )
            }
        } else {
            (SessionEventData::Bytes(data.clone()), None)
        };

        let seq = {
            let mut ring = self.ring.lock().expect("session ring");
            ring.push(channel, event)
        };
        let timestamp_ms = now_unix() as u64 * 1000;
        if let Some(log) = self.log.lock().expect("session log").as_mut() {
            let _ = log.append(&LogRecord {
                seq,
                timestamp_ms,
                kind: RECORD_OUTPUT,
                channel,
                payload: data.clone(),
            });
            if let Some(decoded) = decoded.as_ref() {
                let payload = serde_json::json!({
                    "epoch": log_epoch,
                    "records": &decoded.records,
                    "raw": decoded.raw.as_ref().map(|raw| base64_encode(raw)),
                    "error": &decoded.error,
                });
                let _ = log.append(&LogRecord {
                    seq,
                    timestamp_ms,
                    kind: RECORD_DECODED,
                    channel,
                    payload: serde_json::to_vec(&payload).unwrap_or_default(),
                });
            }
        }
        {
            let mut session = self.state.lock().expect("session state");
            session.data.latest_seq = seq;
            session.data.bytes += data.len() as u64;
            session.data.buffered_bytes = self.ring.lock().expect("ring").bytes as u64;
        }
        self.notify.notify_waiters();
        let _ = inner;
    }

    fn push_rtt_fallback(&self, channel: u8, data: Vec<u8>, epoch: u64) {
        let seq = {
            let mut ring = self.ring.lock().expect("session ring");
            ring.push(
                channel,
                SessionEventData::RttRaw {
                    epoch,
                    data: data.clone(),
                    error: None,
                },
            )
        };
        let timestamp_ms = now_unix() as u64 * 1000;
        if let Some(log) = self.log.lock().expect("session log").as_mut() {
            let _ = log.append(&LogRecord {
                seq,
                timestamp_ms,
                kind: RECORD_OUTPUT,
                channel,
                payload: data.clone(),
            });
            let payload = serde_json::json!({
                "epoch": epoch,
                "records": Vec::<RttRecord>::new(),
                "raw": base64_encode(&data),
                "error": serde_json::Value::Null,
            });
            let _ = log.append(&LogRecord {
                seq,
                timestamp_ms,
                kind: RECORD_DECODED,
                channel,
                payload: serde_json::to_vec(&payload).unwrap_or_default(),
            });
        }
        let mut session = self.state.lock().expect("session state");
        session.data.latest_seq = seq;
        session.data.bytes += data.len() as u64;
        session.data.buffered_bytes = self.ring.lock().expect("ring").bytes as u64;
        self.notify.notify_waiters();
    }

    pub fn set_decoder(&self, profile: Option<Arc<DecodeProfile>>, epoch: u64) {
        let (old_epoch, buffers) = {
            let mut guard = self.decode.lock().expect("session decode");
            let old_epoch = guard.as_ref().map(DecodeState::epoch).unwrap_or(epoch);
            let buffers = guard
                .as_mut()
                .map(DecodeState::take_buffers)
                .unwrap_or_default();
            *guard = profile.map(|profile| DecodeState::new(profile, epoch));
            (old_epoch, buffers)
        };
        for (channel, data) in buffers {
            self.push_rtt_fallback(channel, data, old_epoch);
        }
    }

    pub fn push_input(&self, data: &[u8]) {
        let seq = {
            let ring = self.ring.lock().expect("session ring");
            ring.latest_seq + 1
        };
        // Input records must not advance the output cursor. Keep a separate
        // high-water record sequence by advancing the ring's latest_seq.
        {
            let mut ring = self.ring.lock().expect("session ring");
            ring.latest_seq = seq;
        }
        if let Some(log) = self.log.lock().expect("session log").as_mut() {
            let _ = log.append(&LogRecord {
                seq,
                timestamp_ms: now_unix() as u64 * 1000,
                kind: RECORD_INPUT,
                channel: 0,
                payload: data.to_vec(),
            });
        }
    }

    pub fn replay(&self, after: u64, include_input: bool) -> Vec<SessionEvent> {
        let log_path = session_log::session_log_path(&self.log_dir, &self.id);
        let records = session_log::read_records(&log_path).unwrap_or_default();
        let mut decoded = HashMap::new();
        for record in &records {
            if record.kind == RECORD_DECODED {
                if let Ok(payload) = serde_json::from_slice::<DecodedPayload>(&record.payload) {
                    decoded.insert(record.seq, payload);
                }
            }
        }
        let mut out = Vec::new();
        for record in records {
            if record.seq <= after {
                continue;
            }
            let data = match record.kind {
                RECORD_INPUT if include_input => Some(SessionEventData::Input(record.payload)),
                RECORD_INPUT => None,
                RECORD_OUTPUT if self.kind == "rtt" => {
                    if let Some(payload) = decoded.remove(&record.seq) {
                        Some(SessionEventData::RttRecords {
                            epoch: payload.epoch,
                            records: payload.records,
                            raw: payload.raw.and_then(|raw| base64_decode(&raw)),
                            error: payload.error,
                        })
                    } else {
                        Some(SessionEventData::RttRaw {
                            epoch: 0,
                            data: record.payload,
                            error: None,
                        })
                    }
                }
                RECORD_OUTPUT => Some(SessionEventData::Bytes(record.payload)),
                _ => None,
            };
            if let Some(data) = data {
                out.push(SessionEvent {
                    seq: record.seq,
                    channel: record.channel,
                    timestamp_ms: record.timestamp_ms,
                    data,
                });
            }
        }
        out
    }

    pub fn attach(&self, principal: &str, mode: &str) -> Result<Subscriber, RuntimeError> {
        if mode != "read" && mode != "write" {
            return Err(RuntimeError::new(
                "invalid_request",
                "mode must be read or write",
            ));
        }
        let state = self.state.lock().expect("session state").clone();
        if state.subscribers >= state.max_subscribers {
            return Err(RuntimeError::new(
                "too_many_subscribers",
                "session subscriber limit reached",
            ));
        }
        let subscriber = Subscriber {
            id: new_id("sub"),
            session_id: self.id.clone(),
            principal: principal.to_string(),
            mode: mode.to_string(),
            cursor: 0,
            attached_at: now_rfc3339(),
        };
        self.subscribers
            .lock()
            .expect("subscribers")
            .push(subscriber.clone());
        let mut state = self.state.lock().expect("session state");
        state.subscribers = self.subscribers.lock().expect("subscribers").len() as u32;
        self.subscriber_changed.notify_waiters();
        Ok(subscriber)
    }

    pub fn read_after(&self, cursor: u64) -> Vec<SessionEvent> {
        self.ring.lock().expect("session ring").after(cursor)
    }

    pub fn detach(&self, subscriber_id: &str) {
        self.subscribers
            .lock()
            .expect("subscribers")
            .retain(|sub| sub.id != subscriber_id);
        let mut state = self.state.lock().expect("session state");
        state.subscribers = self.subscribers.lock().expect("subscribers").len() as u32;
        self.subscriber_changed.notify_waiters();
    }
}

async fn persist_session_record(
    inner: &RuntimeInner,
    session: &Session,
    options_json: &str,
) -> Result<(), RuntimeError> {
    let existing = inner.store.load_session(&session.id).await?;
    let now = crate::runtime::now_unix();
    let record = SessionRecord {
        id: session.id.clone(),
        kind: session.kind.clone(),
        resource_id: session.resource_id.clone(),
        generation: 0,
        options_json: options_json.to_string(),
        max_subscribers: session.max_subscribers as i64,
        detach_timeout_seconds: session.detach_timeout_seconds as i64,
        created_by: session.created_by.clone(),
        created_at: existing
            .as_ref()
            .map(|record| record.created_at)
            .unwrap_or(now),
        updated_at: now,
        closed_at: session
            .closed_at
            .as_ref()
            .map(|_| crate::runtime::now_unix()),
        state: session.state.clone(),
        first_seq: session.data.oldest_seq as i64,
        latest_seq: session.data.latest_seq as i64,
        data_bytes: session.data.bytes as i64,
        replayable: if session.replayable { 1 } else { 0 },
        resume_count: session.resume_count as i64,
        artifact_id: session
            .decode
            .artifact
            .as_ref()
            .map(|artifact| artifact.id.clone()),
        decode_json: serde_json::to_string(&session.decode)
            .map_err(|error| RuntimeError::internal(error.to_string()))?,
        last_error: session.last_error.clone(),
    };
    if existing.is_some() {
        inner.store.update_session(&record).await?;
    } else {
        inner.store.insert_session(&record).await?;
    }
    Ok(())
}

impl Runtime {
    pub async fn create_probe_session(
        &self,
        resource_id: &str,
        kind: &str,
        options: boot_protocol::SessionOptions,
        artifact_id: Option<&str>,
        inherit_artifact: bool,
        principal: &str,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<Session, RuntimeError> {
        if kind != "rtt" && kind != "dap" {
            return Err(RuntimeError::new(
                "invalid_request",
                "probe session kind must be rtt or dap",
            ));
        }
        if kind == "dap" {
            return Err(RuntimeError::new(
                "invalid_request",
                "DAP is not implemented",
            ));
        }
        self.create_session(
            resource_id,
            kind,
            options,
            artifact_id,
            inherit_artifact,
            principal,
            max_subscribers,
            detach_timeout_seconds,
            wait_ms,
        )
        .await
    }

    pub async fn create_serial_session(
        &self,
        resource_id: &str,
        options: boot_protocol::SessionOptions,
        principal: &str,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<Session, RuntimeError> {
        self.create_session(
            resource_id,
            "serial",
            options,
            None,
            false,
            principal,
            max_subscribers,
            detach_timeout_seconds,
            wait_ms,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn create_session(
        &self,
        resource_id: &str,
        kind: &str,
        options: boot_protocol::SessionOptions,
        artifact_id: Option<&str>,
        inherit_artifact: bool,
        principal: &str,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<Session, RuntimeError> {
        let resource = self.resource(resource_id).ok_or_else(|| {
            RuntimeError::not_found(format!("resource '{resource_id}' not found"))
        })?;
        if (kind == "serial") != (resource.kind == crate::runtime::ResourceKind::SerialPort) {
            return Err(RuntimeError::new(
                "invalid_request",
                "session kind does not match resource",
            ));
        }
        let (options, decode, profile) = if kind == "rtt" {
            self.resolve_rtt_artifact(resource_id, artifact_id, inherit_artifact, options)
                .await?
        } else {
            (options, SessionDecode::default(), None)
        };
        let id = new_id("ses");
        let lease = lease::acquire(
            &self.inner,
            resource_id,
            &id,
            Duration::from_millis(wait_ms.min(30_000)),
        )
        .await?;
        let now = now_rfc3339();
        let session = Session {
            id: id.clone(),
            kind: kind.to_string(),
            resource_id: resource_id.to_string(),
            created_by: principal.to_string(),
            state: "starting".to_string(),
            created_at: now.clone(),
            updated_at: now,
            closed_at: None,
            subscribers: 0,
            max_subscribers: max_subscribers.min(32),
            detach_timeout_seconds: detach_timeout_seconds.min(300),
            resume_count: 0,
            replayable: true,
            data: SessionData::default(),
            decode: decode.clone(),
            last_error: None,
        };
        let options_json = serde_json::to_string(&options).unwrap_or_else(|_| "{}".to_string());
        let handle = Arc::new(SessionHandle::new(
            session.clone(),
            self.inner.config.session_dir.clone(),
            options_json,
        ));
        *handle.lease.lock().expect("lease") = Some(lease);
        let log = SessionLogWriter::open(&self.inner.config.session_dir, &id)
            .map_err(|error| RuntimeError::internal(error.to_string()))?;
        *handle.log.lock().expect("log") = Some(log);
        handle.set_decoder(profile.clone(), decode.epoch);
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .insert(id.clone(), handle.clone());
        if let Some(artifact_id) = session
            .decode
            .artifact
            .as_ref()
            .map(|artifact| artifact.id.clone())
        {
            if let Err(error) = self
                .inner
                .store
                .add_artifact_ref(&artifact_id, "session", &id, now_unix())
                .await
            {
                self.inner
                    .sessions
                    .lock()
                    .expect("sessions lock")
                    .remove(&id);
                return Err(error.into());
            }
        }
        if let Err(error) = self.persist_session(&session).await {
            if let Some(artifact_id) = session
                .decode
                .artifact
                .as_ref()
                .map(|artifact| artifact.id.clone())
            {
                let _ = self
                    .inner
                    .store
                    .remove_artifact_ref(&artifact_id, "session", &id)
                    .await;
            }
            self.inner
                .sessions
                .lock()
                .expect("sessions lock")
                .remove(&id);
            return Err(error);
        }
        spawn_session_thread(
            self.inner.clone(),
            handle,
            resource.selector,
            resource.path,
            options,
        );
        Ok(session)
    }

    async fn resolve_rtt_artifact(
        &self,
        resource_id: &str,
        explicit_artifact: Option<&str>,
        inherit_artifact: bool,
        mut options: boot_protocol::SessionOptions,
    ) -> Result<
        (
            boot_protocol::SessionOptions,
            SessionDecode,
            Option<Arc<DecodeProfile>>,
        ),
        RuntimeError,
    > {
        let profile = if let Some(artifact_id) = explicit_artifact {
            Some(decode::load_profile(&self.inner, artifact_id).await?)
        } else if inherit_artifact {
            if let Some(probe) = self.probe_artifact(resource_id) {
                match decode::load_profile(&self.inner, &probe.id).await {
                    Ok(profile) if profile.usable() => Some(profile),
                    Ok(_) => None,
                    Err(error) => {
                        eprintln!(
                            "probe {} last artifact {} is unavailable: {}",
                            resource_id, probe.id, error
                        );
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let Some(profile) = profile else {
            return Ok((
                options.clone(),
                SessionDecode {
                    mode: "raw".to_string(),
                    epoch: 0,
                    artifact: None,
                    rtt_addr: options.rtt_addr.clone(),
                    rtt_addr_source: if options.rtt_addr.is_some() {
                        "explicit".to_string()
                    } else {
                        "scan".to_string()
                    },
                },
                None,
            ));
        };
        let rtt_addr_source = if options.rtt_addr.is_some() {
            "explicit"
        } else if profile.rtt_addr.is_some() {
            "artifact"
        } else {
            "scan"
        }
        .to_string();
        if options.rtt_addr.is_none() {
            if let Some(address) = profile.rtt_addr {
                options.rtt_addr = Some(format!("0x{address:x}"));
            }
        }
        let decode = SessionDecode {
            mode: if profile.defmt() {
                "defmt".to_string()
            } else {
                "raw".to_string()
            },
            epoch: 1,
            artifact: Some(profile.session_artifact()),
            rtt_addr: options.rtt_addr.clone(),
            rtt_addr_source,
        };
        Ok((options, decode, Some(profile)))
    }

    pub fn session_handle(&self, id: &str) -> Result<Arc<SessionHandle>, RuntimeError> {
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::not_found(format!("session '{id}' not found")))
    }

    pub fn sessions_list(&self) -> Vec<Session> {
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .values()
            .map(|handle| handle.snapshot())
            .collect()
    }

    pub async fn patch_session(
        &self,
        id: &str,
        patch: SessionPatch,
    ) -> Result<Session, RuntimeError> {
        let handle = self.session_handle(id)?;
        let mut session = handle.snapshot();
        if let Some(max) = patch.max_subscribers {
            if max < session.subscribers {
                return Err(RuntimeError::new(
                    "conflict",
                    "max_subscribers is below the current subscriber count",
                ));
            }
            session.max_subscribers = max.min(32);
        }
        if let Some(timeout) = patch.detach_timeout_seconds {
            session.detach_timeout_seconds = timeout.min(300);
        }
        session.updated_at = now_rfc3339();
        handle.update(session.clone());
        self.persist_session(&session).await?;
        Ok(session)
    }

    pub async fn set_session_artifact(
        &self,
        id: &str,
        artifact_id: &str,
    ) -> Result<Session, RuntimeError> {
        let handle = self.session_handle(id)?;
        let mut session = handle.snapshot();
        if session.kind != "rtt" {
            return Err(RuntimeError::new(
                "not_rtt_session",
                "artifacts can only be set on RTT sessions",
            ));
        }
        if matches!(session.state.as_str(), "closed" | "failed") {
            return Err(RuntimeError::new(
                "session_closed",
                "session is not accepting artifact changes",
            ));
        }
        if session
            .decode
            .artifact
            .as_ref()
            .is_some_and(|artifact| artifact.id == artifact_id)
        {
            return Ok(session);
        }
        let profile = decode::load_profile(&self.inner, artifact_id).await?;
        if !profile.usable() {
            return Err(RuntimeError::new(
                "invalid_artifact",
                "artifact has neither an RTT control block nor defmt data",
            ));
        }
        if let (Some(current), Some(next)) = (
            session.decode.rtt_addr.as_deref(),
            profile.rtt_addr.map(|address| format!("0x{address:x}")),
        ) {
            if current != next {
                eprintln!(
                    "session {} RTT address mismatch: producer uses {}, artifact has {}",
                    session.id, current, next
                );
            }
        }
        let old_artifact = session
            .decode
            .artifact
            .as_ref()
            .map(|artifact| artifact.id.clone());
        self.inner
            .store
            .add_artifact_ref(artifact_id, "session", id, now_unix())
            .await?;
        if let Some(old_artifact) = old_artifact {
            self.inner
                .store
                .remove_artifact_ref(&old_artifact, "session", id)
                .await?;
        }
        let epoch = session.decode.epoch.saturating_add(1);
        let rtt_addr = session.decode.rtt_addr.clone();
        let rtt_addr_source = session.decode.rtt_addr_source.clone();
        session.decode = SessionDecode {
            mode: if profile.defmt() {
                "defmt".to_string()
            } else {
                "raw".to_string()
            },
            epoch,
            artifact: Some(profile.session_artifact()),
            rtt_addr,
            rtt_addr_source,
        };
        session.updated_at = now_rfc3339();
        handle.set_decoder(Some(profile), session.decode.epoch);
        handle.update(session.clone());
        self.persist_session(&session).await?;
        Ok(session)
    }

    pub async fn clear_session_artifact(&self, id: &str) -> Result<Session, RuntimeError> {
        let handle = self.session_handle(id)?;
        let mut session = handle.snapshot();
        if session.kind != "rtt" {
            return Err(RuntimeError::new(
                "not_rtt_session",
                "artifacts can only be set on RTT sessions",
            ));
        }
        if matches!(session.state.as_str(), "closed" | "failed") {
            return Err(RuntimeError::new(
                "session_closed",
                "session is not accepting artifact changes",
            ));
        }
        let Some(old_artifact) = session
            .decode
            .artifact
            .as_ref()
            .map(|artifact| artifact.id.clone())
        else {
            return Ok(session);
        };
        self.inner
            .store
            .remove_artifact_ref(&old_artifact, "session", id)
            .await?;
        let epoch = session.decode.epoch.saturating_add(1);
        let rtt_addr = session.decode.rtt_addr.clone();
        let rtt_addr_source = session.decode.rtt_addr_source.clone();
        session.decode = SessionDecode {
            mode: "raw".to_string(),
            epoch,
            artifact: None,
            rtt_addr,
            rtt_addr_source,
        };
        session.updated_at = now_rfc3339();
        handle.set_decoder(None, session.decode.epoch);
        handle.update(session.clone());
        self.persist_session(&session).await?;
        Ok(session)
    }

    pub async fn close_session(&self, id: &str, purge: bool) -> Result<(), RuntimeError> {
        let handle = self.session_handle(id)?;
        handle.shutdown.cancel();
        let mut session = handle.snapshot();
        session.state = "closed".to_string();
        session.closed_at = Some(now_rfc3339());
        session.updated_at = now_rfc3339();
        handle.set_decoder(None, session.decode.epoch);
        handle.update(session.clone());
        self.inner
            .store
            .remove_artifact_refs_for_owner("session", id)
            .await?;
        self.persist_session(&session).await?;
        if purge {
            self.inner.store.purge_session_data(id).await?;
            let path = session_log::session_log_path(&self.inner.config.session_dir, id);
            let _ = std::fs::remove_file(path);
            self.inner
                .sessions
                .lock()
                .expect("sessions lock")
                .remove(id);
        }
        Ok(())
    }

    pub async fn resume_session(&self, id: &str) -> Result<Session, RuntimeError> {
        let handle = self.session_handle(id)?;
        let session = handle.snapshot();
        if session.state != "suspended" {
            return Err(RuntimeError::new(
                "not_resumable",
                "session is not suspended",
            ));
        }
        if session.kind == "dap" {
            return Err(RuntimeError::new(
                "not_resumable",
                "DAP sessions cannot be resumed",
            ));
        }
        let resource = self
            .resource(&session.resource_id)
            .ok_or_else(|| RuntimeError::not_found("resource not found"))?;
        let lease = lease::acquire(
            &self.inner,
            &session.resource_id,
            &session.id,
            self.inner.config.lease_wait,
        )
        .await?;
        let mut session = session;
        session.state = "resuming".to_string();
        session.resume_count += 1;
        let profile = if let Some(artifact) = session.decode.artifact.as_ref() {
            match decode::load_profile(&self.inner, &artifact.id).await {
                Ok(profile) => Some(profile),
                Err(error) => {
                    eprintln!(
                        "session {} artifact {} is unavailable: {}",
                        session.id, artifact.id, error
                    );
                    session.decode.mode = "raw".to_string();
                    None
                }
            }
        } else {
            None
        };
        if let Some(artifact) = session.decode.artifact.as_ref() {
            let _ = self
                .inner
                .store
                .add_artifact_ref(&artifact.id, "session", &session.id, now_unix())
                .await;
        }
        handle.update(session.clone());
        handle.set_decoder(profile, session.decode.epoch);
        let _ = persist_session_record(&self.inner, &session, &handle.options_json).await;
        *handle.lease.lock().expect("lease") = Some(lease);
        let options = serde_json::from_str::<boot_protocol::SessionOptions>(&handle.options_json)
            .unwrap_or_default();
        spawn_session_thread(
            self.inner.clone(),
            handle,
            resource.selector,
            resource.path,
            options,
        );
        Ok(session)
    }

    pub async fn persist_session(&self, session: &Session) -> Result<(), RuntimeError> {
        let options_json = self
            .inner
            .sessions
            .lock()
            .expect("sessions lock")
            .get(&session.id)
            .map(|handle| handle.options_json.clone())
            .unwrap_or_else(|| "{}".to_string());
        persist_session_record(&self.inner, session, &options_json).await
    }

    pub async fn restore_sessions(inner: &Arc<RuntimeInner>) -> Result<(), RuntimeError> {
        let records = inner.store.load_sessions().await?;
        for record in records {
            let path = session_log::session_log_path(&inner.config.session_dir, &record.id);
            let latest = session_log::latest_seq(&path);
            let mut session = Session {
                id: record.id.clone(),
                kind: record.kind.clone(),
                resource_id: record.resource_id.clone(),
                created_by: record.created_by.clone(),
                state: record.state.clone(),
                created_at: rfc3339_from_unix(record.created_at),
                updated_at: rfc3339_from_unix(record.updated_at),
                closed_at: record.closed_at.map(rfc3339_from_unix),
                subscribers: 0,
                max_subscribers: record.max_subscribers as u32,
                detach_timeout_seconds: record.detach_timeout_seconds as u64,
                resume_count: record.resume_count as u64,
                replayable: record.replayable != 0,
                data: SessionData {
                    oldest_seq: record.first_seq as u64,
                    latest_seq: latest.max(record.latest_seq as u64),
                    buffered_bytes: 0,
                    bytes: record.data_bytes as u64,
                },
                decode: serde_json::from_str(&record.decode_json).unwrap_or_default(),
                last_error: record.last_error,
            };
            if !matches!(session.state.as_str(), "closed" | "failed") {
                if session.kind == "dap" {
                    session.state = "failed".to_string();
                    session.last_error = Some("server_restart".to_string());
                } else {
                    session.state = "suspended".to_string();
                }
                session.updated_at = now_rfc3339();
            }
            let handle = Arc::new(SessionHandle::new(
                session.clone(),
                inner.config.session_dir.clone(),
                record.options_json.clone(),
            ));
            if let Ok(log) = SessionLogWriter::open(&inner.config.session_dir, &session.id) {
                *handle.log.lock().expect("log") = Some(log);
            }
            inner
                .sessions
                .lock()
                .expect("sessions lock")
                .insert(session.id.clone(), handle);
        }
        Ok(())
    }
}

fn rfc3339_from_unix(seconds: i64) -> String {
    crate::runtime::rfc3339_from_unix(seconds)
}

pub fn schedule_close_if_idle(inner: Arc<RuntimeInner>, handle: Arc<SessionHandle>) {
    let timeout = Duration::from_secs(handle.snapshot().detach_timeout_seconds.max(1));
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        if !handle.subscribers.lock().expect("subscribers").is_empty() {
            return;
        }
        if handle.shutdown.is_cancelled() {
            return;
        }
        let mut session = handle.snapshot();
        if !matches!(session.state.as_str(), "active" | "starting" | "resuming") {
            return;
        }
        session.state = "closed".to_string();
        session.closed_at = Some(now_rfc3339());
        session.updated_at = now_rfc3339();
        handle.set_decoder(None, session.decode.epoch);
        handle.update(session.clone());
        handle.shutdown.cancel();
        let _ = handle.lease.lock().expect("lease").take();
        let _ = inner
            .store
            .remove_artifact_refs_for_owner("session", &session.id)
            .await;
        let _ = persist_session_record(&inner, &session, &handle.options_json).await;
    });
}

async fn fail_session(inner: &RuntimeInner, handle: &SessionHandle, error: String) {
    let mut session = handle.snapshot();
    session.state = "suspended".to_string();
    session.last_error = Some(error);
    session.updated_at = now_rfc3339();
    handle.update(session.clone());
    handle.set_decoder(None, session.decode.epoch);
    let _ = handle.lease.lock().expect("lease").take();
    let _ = persist_session_record(inner, &session, &handle.options_json).await;
    handle.shutdown.cancel();
}

fn spawn_session_thread(
    inner: Arc<RuntimeInner>,
    handle: Arc<SessionHandle>,
    selector: Option<String>,
    path: Option<String>,
    options: boot_protocol::SessionOptions,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ThreadEvent>();
    let shutdown = handle.shutdown.clone();
    let kind = handle.kind.clone();
    if kind == "serial" {
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        *handle.input_tx.lock().expect("input") = Some(input_tx);
        let Some(path) = path else {
            let handle = handle.clone();
            let inner = inner.clone();
            tokio::spawn(async move {
                fail_session(&inner, &handle, "serial path missing".into()).await;
            });
            return;
        };
        std::thread::Builder::new()
            .name(format!("boot-serial-{}", handle.id))
            .spawn(move || serial_thread(path, options, input_rx, event_tx, shutdown))
            .ok();
    } else if kind == "rtt" {
        let Some(selector) = selector else {
            let handle = handle.clone();
            let inner = inner.clone();
            tokio::spawn(async move {
                fail_session(&inner, &handle, "probe selector missing".into()).await;
            });
            return;
        };
        std::thread::Builder::new()
            .name(format!("boot-rtt-{}", handle.id))
            .spawn(move || rtt_thread(selector, options, event_tx, shutdown))
            .ok();
    } else {
        let handle = handle.clone();
        let inner = inner.clone();
        tokio::spawn(async move {
            fail_session(&inner, &handle, "unsupported session kind".into()).await;
        });
        return;
    }

    let inner_for_task = inner.clone();
    let handle_for_task = handle.clone();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            match event {
                ThreadEvent::Active => {
                    let mut session = handle_for_task.snapshot();
                    session.state = "active".to_string();
                    session.updated_at = now_rfc3339();
                    handle_for_task.update(session.clone());
                    let _ = persist_session_record(
                        &inner_for_task,
                        &session,
                        &handle_for_task.options_json,
                    )
                    .await;
                    if session.subscribers == 0 {
                        schedule_close_if_idle(inner_for_task.clone(), handle_for_task.clone());
                    }
                }
                ThreadEvent::Data { data, channel } => {
                    handle_for_task.push_output(&inner_for_task, data, channel);
                }
                ThreadEvent::Error(error) => {
                    fail_session(&inner_for_task, &handle_for_task, error).await;
                    break;
                }
                ThreadEvent::Ended => {
                    let mut session = handle_for_task.snapshot();
                    session.state = "closed".to_string();
                    session.closed_at = Some(now_rfc3339());
                    session.updated_at = now_rfc3339();
                    handle_for_task.set_decoder(None, session.decode.epoch);
                    handle_for_task.update(session.clone());
                    let _ = handle_for_task.lease.lock().expect("lease").take();
                    let _ = inner_for_task
                        .store
                        .remove_artifact_refs_for_owner("session", &session.id)
                        .await;
                    let _ = persist_session_record(
                        &inner_for_task,
                        &session,
                        &handle_for_task.options_json,
                    )
                    .await;
                    break;
                }
            }
        }
    });
}

fn serial_thread(
    path: String,
    options: boot_protocol::SessionOptions,
    input_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<ThreadEvent>,
    shutdown: CancellationToken,
) {
    let baud = options.baud_rate.unwrap_or(115200);
    let mut port = match serialport::new(&path, baud)
        .timeout(Duration::from_millis(100))
        .open()
    {
        Ok(port) => port,
        Err(error) => {
            let _ = event_tx.send(ThreadEvent::Error(format!(
                "failed to open serial port: {error}"
            )));
            return;
        }
    };
    let _ = event_tx.send(ThreadEvent::Active);
    let mut buffer = [0u8; 1024];
    while !shutdown.is_cancelled() {
        while let Ok(data) = input_rx.try_recv() {
            let _ = port.write_all(&data);
            let _ = port.flush();
        }
        match port.read(&mut buffer) {
            Ok(n) if n > 0 => {
                let _ = event_tx.send(ThreadEvent::Data {
                    data: buffer[..n].to_vec(),
                    channel: 0,
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => {
                let _ = event_tx.send(ThreadEvent::Error(format!("serial read failed: {error}")));
                return;
            }
        }
    }
    let _ = event_tx.send(ThreadEvent::Ended);
}

fn rtt_thread(
    selector: String,
    options: boot_protocol::SessionOptions,
    event_tx: tokio::sync::mpsc::UnboundedSender<ThreadEvent>,
    shutdown: CancellationToken,
) {
    let selector = match selector.parse::<probe_rs::probe::DebugProbeSelector>() {
        Ok(selector) => selector,
        Err(error) => {
            let _ = event_tx.send(ThreadEvent::Error(format!("invalid selector: {error}")));
            return;
        }
    };
    let lister = probe_rs::probe::list::Lister::new();
    let probe = match lister.open(selector) {
        Ok(probe) => probe,
        Err(error) => {
            let _ = event_tx.send(ThreadEvent::Error(format!("failed to open probe: {error}")));
            return;
        }
    };
    let mut session = match probe.attach(
        probe_rs::config::TargetSelector::Auto,
        probe_rs::Permissions::default(),
    ) {
        Ok(session) => session,
        Err(error) => {
            let _ = event_tx.send(ThreadEvent::Error(format!("failed to attach: {error}")));
            return;
        }
    };
    let mut core = match session.core(0) {
        Ok(core) => core,
        Err(error) => {
            let _ = event_tx.send(ThreadEvent::Error(format!(
                "failed to access core: {error}"
            )));
            return;
        }
    };
    let mut rtt = if let Some(address) = options.rtt_addr.as_deref() {
        let parsed = if let Some(value) = address
            .strip_prefix("0x")
            .or_else(|| address.strip_prefix("0X"))
        {
            u64::from_str_radix(value, 16)
        } else {
            u64::from_str_radix(address, 16)
        };
        match parsed
            .map_err(|error| error.to_string())
            .and_then(|address| {
                probe_rs::rtt::Rtt::attach_at(&mut core, address).map_err(|error| error.to_string())
            }) {
            Ok(rtt) => rtt,
            Err(error) => {
                let _ = event_tx.send(ThreadEvent::Error(format!("failed to attach RTT: {error}")));
                return;
            }
        }
    } else {
        match probe_rs::rtt::Rtt::attach(&mut core) {
            Ok(rtt) => rtt,
            Err(error) => {
                let _ = event_tx.send(ThreadEvent::Error(format!("failed to attach RTT: {error}")));
                return;
            }
        }
    };
    let _ = event_tx.send(ThreadEvent::Active);
    let mut buffer = [0u8; 1024];
    while !shutdown.is_cancelled() {
        let mut any = false;
        for (channel, up) in rtt.up_channels().iter_mut().enumerate() {
            if !options.channels.is_empty() && !options.channels.contains(&(channel as u8)) {
                continue;
            }
            match up.read(&mut core, &mut buffer) {
                Ok(count) if count > 0 => {
                    any = true;
                    let _ = event_tx.send(ThreadEvent::Data {
                        data: buffer[..count].to_vec(),
                        channel: channel as u8,
                    });
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = event_tx.send(ThreadEvent::Error(format!("RTT read failed: {error}")));
                    return;
                }
            }
        }
        if !any {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let _ = event_tx.send(ThreadEvent::Ended);
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::prelude::BASE64_STANDARD.encode(data)
}

fn base64_decode(data: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::prelude::BASE64_STANDARD.decode(data).ok()
}

// Keep the public constant accessible for handlers.
pub const _MAX_SESSION_LIFETIME: Duration = Duration::from_secs(8 * 3600);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_ring_evicts_oldest_and_preserves_cursor_order() {
        let mut ring = SessionRing::new(100);
        assert_eq!(ring.push(0, SessionEventData::Bytes(b"abc".to_vec())), 1);
        assert_eq!(ring.push(0, SessionEventData::Bytes(b"def".to_vec())), 2);
        // The first chunk is evicted to keep the live tail bounded.
        assert_eq!(ring.after(0).len(), 1);
        assert_eq!(ring.after(0)[0].seq, 2);
        assert_eq!(ring.after(1).len(), 1);
        assert_eq!(ring.after(1)[0].seq, 2);
        assert!(ring.after(2).is_empty());
    }

    #[tokio::test]
    async fn inherited_probe_artifact_is_selected_and_can_be_disabled() {
        use crate::runtime::RuntimeConfig;
        use crate::runtime::artifact;
        use crate::store::Store;

        let directory = std::env::temp_dir().join(format!(
            "boot-api-session-inherit-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let store = std::sync::Arc::new(Store::open(directory.join("boot.sqlite3")).await.unwrap());
        let runtime = Runtime::new(
            store,
            RuntimeConfig::new(directory.join("sessions"), directory.join("artifacts")),
        );
        let artifact = artifact::create(
            &runtime.inner,
            include_bytes!("../../tests/fixtures/rtt.elf"),
        )
        .await
        .unwrap();
        artifact::set_probe_artifact(
            &runtime.inner,
            "probe_test",
            &artifact.id,
            Some("chg_test"),
            Some("principal"),
        )
        .await
        .unwrap();

        let (options, decode, profile) = runtime
            .resolve_rtt_artifact(
                "probe_test",
                None,
                true,
                boot_protocol::SessionOptions::default(),
            )
            .await
            .unwrap();
        assert!(profile.is_some());
        assert!(options.rtt_addr.is_some());
        assert_eq!(decode.mode, "raw");
        assert_eq!(
            decode
                .artifact
                .as_ref()
                .map(|artifact| artifact.id.as_str()),
            Some(artifact.id.as_str())
        );

        let (_, decode, profile) = runtime
            .resolve_rtt_artifact(
                "probe_test",
                None,
                false,
                boot_protocol::SessionOptions::default(),
            )
            .await
            .unwrap();
        assert!(profile.is_none());
        assert!(decode.artifact.is_none());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn replay_prefers_decoded_records_and_preserves_empty_markers() {
        let directory = std::env::temp_dir().join(format!(
            "boot-api-session-replay-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let session = Session {
            id: "ses_test".into(),
            kind: "rtt".into(),
            resource_id: "probe_test".into(),
            created_by: "principal".into(),
            state: "closed".into(),
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            closed_at: Some(now_rfc3339()),
            subscribers: 0,
            max_subscribers: 8,
            detach_timeout_seconds: 60,
            resume_count: 0,
            replayable: true,
            data: SessionData::default(),
            decode: SessionDecode::default(),
            last_error: None,
        };
        let handle = SessionHandle::new(session, directory.clone(), "{}".into());
        let path = session_log::session_log_path(&directory, "ses_test");
        let mut writer = SessionLogWriter::open(&directory, "ses_test").unwrap();
        writer
            .append(&LogRecord {
                seq: 1,
                timestamp_ms: 1,
                kind: RECORD_OUTPUT,
                channel: 0,
                payload: b"partial".to_vec(),
            })
            .unwrap();
        writer
            .append(&LogRecord {
                seq: 1,
                timestamp_ms: 1,
                kind: RECORD_DECODED,
                channel: 0,
                payload: br#"{"epoch":1,"records":[],"raw":null,"error":null}"#.to_vec(),
            })
            .unwrap();
        writer
            .append(&LogRecord {
                seq: 2,
                timestamp_ms: 2,
                kind: RECORD_OUTPUT,
                channel: 0,
                payload: b"frame".to_vec(),
            })
            .unwrap();
        writer
            .append(&LogRecord {
                seq: 2,
                timestamp_ms: 2,
                kind: RECORD_DECODED,
                channel: 0,
                payload: br#"{"epoch":1,"records":[{"timestamp":null,"level":"info","module":null,"file":null,"line":null,"message":"hello"}],"raw":null,"error":null}"#.to_vec(),
            })
            .unwrap();
        drop(writer);
        let events = handle.replay(0, false);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 1);
        assert!(matches!(
            events[0].data,
            SessionEventData::RttRecords { ref records, .. } if records.is_empty()
        ));
        assert_eq!(events[1].seq, 2);
        assert!(matches!(
            events[1].data,
            SessionEventData::RttRecords { ref records, .. } if records.len() == 1
        ));
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
