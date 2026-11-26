// SPDX-License-Identifier: AGPL-3.0-or-later
use std::collections::{HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use boot_protocol::{
    FRAME_FLAG_SUPERSEDED, FrameDirection, FrameRecord, GapPayload, MarkPayload, RttRecordsPayload,
    SESSION_FRAME_HEADER_LEN, Session, SessionAcquire, SessionAcquireResult, SessionData,
    SessionDecode, SessionFrameHeader, SessionPatch, Subscriber, encode_frame,
};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "run")]
use crate::runtime::RuntimeArtifactResolver;
#[cfg(feature = "run")]
use boot_dap::{DapBackend, DapIo, DapLaunch, NativeDapBackend};

use crate::runtime::decode::{self, DecodeProfile, DecodeState};
use crate::runtime::lease::LeaseGuard;
use crate::runtime::session_log::{self, SessionLogReader, SessionLogWriter};
use crate::runtime::{Runtime, RuntimeError, RuntimeInner, lease, new_id, now_rfc3339, now_unix};
use crate::store::RuntimeStore;
use crate::store::SessionRecord;

#[derive(Debug, Clone)]
enum ThreadEvent {
    Active,
    Data { data: Vec<u8>, stream: u8 },
    Error(String),
    Ended,
}

#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub seq: u64,
    pub timestamp_ns: u64,
    pub stream: u8,
    pub direction: FrameDirection,
    pub record: FrameRecord,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl SessionEvent {
    fn header(&self) -> SessionFrameHeader {
        SessionFrameHeader {
            flags: self.flags,
            record: self.record,
            direction: self.direction,
            stream: self.stream,
            seq: self.seq,
            timestamp_ns: self.timestamp_ns,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(encode_frame(&self.header(), &self.payload)?)
    }
}

struct SessionRing {
    chunks: VecDeque<SessionEvent>,
    bytes: usize,
    max_bytes: usize,
    evicted_through: u64,
}

impl SessionRing {
    fn new(max_bytes: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            bytes: 0,
            max_bytes,
            evicted_through: 0,
        }
    }

    fn push(&mut self, event: SessionEvent) {
        let size = event.payload.len() + SESSION_FRAME_HEADER_LEN;
        self.bytes += size;
        self.chunks.push_back(event);
        // Always retain the newest frame, even if it alone exceeds the cap.
        while self.bytes > self.max_bytes && self.chunks.len() > 1 {
            if let Some(chunk) = self.chunks.pop_front() {
                self.evicted_through = chunk.seq;
                self.bytes = self
                    .bytes
                    .saturating_sub(chunk.payload.len() + SESSION_FRAME_HEADER_LEN);
            }
        }
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
    sequence: AtomicU64,
    last_timestamp_ns: AtomicU64,
    append_lock: Mutex<()>,
    pub notify: Notify,
    pub subscriber_changed: Notify,
    pub subscribers: Mutex<Vec<Subscriber>>,
    pub input_tx: Mutex<Option<std::sync::mpsc::SyncSender<Vec<u8>>>>,
    pub lease: Mutex<Option<LeaseGuard>>,
    pub shutdown: CancellationToken,
    pub log: Mutex<Option<SessionLogWriter>>,
    pub decode: Mutex<Option<DecodeState>>,
    pub lifecycle_tx: broadcast::Sender<Session>,
}

impl SessionHandle {
    fn new(session: Session, log_dir: std::path::PathBuf, options_json: String) -> Self {
        let max_bytes = match session.kind.as_str() {
            "rtt" => 1024 * 1024,
            "dap" => 4 * 1024 * 1024,
            _ => 256 * 1024,
        };
        let latest_seq = session.data.latest_seq;
        let (lifecycle_tx, _) = broadcast::channel(64);
        Self {
            id: session.id.clone(),
            kind: session.kind.clone(),
            resource_id: session.resource_id.clone(),
            log_dir,
            options_json,
            state: Mutex::new(session),
            ring: Mutex::new(SessionRing::new(max_bytes)),
            sequence: AtomicU64::new(latest_seq),
            last_timestamp_ns: AtomicU64::new(0),
            append_lock: Mutex::new(()),
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

    fn alloc_seq(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn now_timestamp_ns(&self) -> u64 {
        let now = crate::runtime::now_unix_ns();
        let previous = self.last_timestamp_ns.fetch_max(now, Ordering::SeqCst);
        previous.max(now)
    }

    fn event(
        &self,
        seq: u64,
        timestamp_ns: u64,
        stream: u8,
        direction: FrameDirection,
        record: FrameRecord,
        flags: u8,
        payload: Vec<u8>,
    ) -> SessionEvent {
        SessionEvent {
            seq,
            timestamp_ns,
            stream,
            direction,
            record,
            flags,
            payload,
        }
    }

    fn log_events(&self, events: &[&SessionEvent]) {
        let mut failed = false;
        if let Some(log) = self.log.lock().expect("session log").as_mut() {
            for event in events {
                if let Err(error) = log.append(&event.header(), &event.payload) {
                    eprintln!("session {} log write failed: {error}", self.id);
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            self.mark_degraded();
        }
    }

    fn mark_degraded(&self) {
        self.log.lock().expect("session log").take();
        let mut session = self.state.lock().expect("session state");
        if session.replayable {
            session.replayable = false;
            session.last_error = Some("persistence_failed".to_string());
            session.updated_at = now_rfc3339();
            let updated = session.clone();
            drop(session);
            let _ = self.lifecycle_tx.send(updated);
        }
    }

    fn push_display(&self, event: SessionEvent, canonical_bytes: u64) {
        let seq = event.seq;
        let (buffered_bytes, evicted_through) = {
            let mut ring = self.ring.lock().expect("session ring");
            ring.push(event);
            (ring.bytes as u64, ring.evicted_through)
        };
        let mut session = self.state.lock().expect("session state");
        if session.data.oldest_seq == 0 {
            session.data.oldest_seq = seq;
        }
        if !session.replayable && evicted_through > 0 {
            session.data.oldest_seq = evicted_through.saturating_add(1);
        }
        session.data.latest_seq = seq;
        session.data.bytes += canonical_bytes;
        session.data.buffered_bytes = buffered_bytes;
        drop(session);
        self.notify.notify_waiters();
    }

    fn emit_output(&self, stream: u8, data: Vec<u8>, decoded: Option<RttRecordsPayload>) {
        let _append = self.append_lock.lock().expect("session append");
        let seq = self.alloc_seq();
        let timestamp_ns = self.now_timestamp_ns();
        let canonical_len = data.len();
        let display = if let Some(decoded) = decoded {
            let payload = serde_json::to_vec(&decoded).unwrap_or_default();
            let decoded_event = self.event(
                seq,
                timestamp_ns,
                stream,
                FrameDirection::Output,
                FrameRecord::RttRecords,
                0,
                payload,
            );
            let raw_event = self.event(
                seq,
                timestamp_ns,
                stream,
                FrameDirection::Output,
                FrameRecord::Data,
                FRAME_FLAG_SUPERSEDED,
                data,
            );
            self.log_events(&[&decoded_event, &raw_event]);
            decoded_event
        } else {
            let raw_event = self.event(
                seq,
                timestamp_ns,
                stream,
                FrameDirection::Output,
                FrameRecord::Data,
                0,
                data,
            );
            self.log_events(&[&raw_event]);
            raw_event
        };
        self.push_display(display, canonical_len as u64);
    }

    pub fn push_output(&self, data: Vec<u8>, stream: u8) {
        let decoded = if self.kind == "rtt" {
            let mut guard = self.decode.lock().expect("session decode");
            guard.as_mut().map(|state| {
                let epoch = state.epoch();
                let chunk = state.decode(stream, &data);
                RttRecordsPayload {
                    v: 1,
                    epoch,
                    records: chunk.records,
                    raw_fallback: chunk.raw.map(|raw| base64_encode(&raw)),
                    error: chunk.error,
                }
            })
        } else {
            None
        };
        self.emit_output(stream, data, decoded);
    }

    fn push_raw_fallback(&self, stream: u8, data: Vec<u8>) {
        self.emit_output(stream, data, None);
    }

    pub fn set_decoder(&self, profile: Option<Arc<DecodeProfile>>, epoch: u64) {
        let buffers = {
            let mut guard = self.decode.lock().expect("session decode");
            let buffers = guard
                .as_mut()
                .map(DecodeState::take_buffers)
                .unwrap_or_default();
            *guard = profile.map(|profile| DecodeState::new(profile, epoch));
            buffers
        };
        for (stream, data) in buffers {
            self.push_raw_fallback(stream, data);
        }
    }

    pub fn push_input(&self, stream: u8, data: Vec<u8>) -> Result<(), RuntimeError> {
        let sender = self.input_tx.lock().expect("input").clone();
        let Some(sender) = sender else {
            return Err(RuntimeError::new(
                "session_closed",
                "session is not accepting input",
            ));
        };
        let _append = self.append_lock.lock().expect("session append");
        let payload_len = data.len() as u64;
        sender
            .try_send(data.clone())
            .map_err(|_| RuntimeError::new("input_overflow", "session input queue is full"))?;
        let seq = self.alloc_seq();
        let timestamp_ns = self.now_timestamp_ns();
        let event = self.event(
            seq,
            timestamp_ns,
            stream,
            FrameDirection::Input,
            FrameRecord::Data,
            0,
            data,
        );
        self.log_events(&[&event]);
        let mut session = self.state.lock().expect("session state");
        if session.data.oldest_seq == 0 {
            session.data.oldest_seq = seq;
        }
        session.data.latest_seq = seq;
        session.data.input_bytes += payload_len;
        Ok(())
    }

    pub fn replay(&self, after: u64, include_input: bool) -> Vec<SessionEvent> {
        let path = session_log::session_log_path(&self.log_dir, &self.id);
        let Some(mut reader) = SessionLogReader::open(&path).unwrap_or(None) else {
            return Vec::new();
        };
        if reader.header().session_id != self.id || reader.header().kind != self.kind {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut displayed = HashSet::new();
        loop {
            match reader.next_frame() {
                session_log::NextFrame::Frame(frame) => {
                    if frame.header.seq <= after {
                        continue;
                    }
                    let include = match frame.header.record {
                        FrameRecord::SessionHeader => false,
                        FrameRecord::Data if frame.header.direction == FrameDirection::Input => {
                            include_input
                        }
                        FrameRecord::Data
                            if frame.header.flags & FRAME_FLAG_SUPERSEDED != 0
                                && displayed.contains(&frame.header.seq) =>
                        {
                            false
                        }
                        FrameRecord::Data | FrameRecord::RttRecords => {
                            displayed.insert(frame.header.seq);
                            true
                        }
                        FrameRecord::Gap | FrameRecord::Mark => true,
                    };
                    if include {
                        out.push(SessionEvent {
                            seq: frame.header.seq,
                            timestamp_ns: frame.header.timestamp_ns,
                            stream: frame.header.stream,
                            direction: frame.header.direction,
                            record: frame.header.record,
                            flags: frame.header.flags,
                            payload: frame.payload,
                        });
                    }
                }
                session_log::NextFrame::End => break,
                session_log::NextFrame::Truncated { .. }
                | session_log::NextFrame::Corrupt { .. } => break,
            }
        }
        out
    }

    pub fn log_mark(&self, event: &str, detail: serde_json::Value) -> u64 {
        let _append = self.append_lock.lock().expect("session append");
        let seq = self.alloc_seq();
        let timestamp_ns = self.now_timestamp_ns();
        let payload = serde_json::to_vec(&MarkPayload::new(event, detail)).unwrap_or_default();
        let mark = self.event(
            seq,
            timestamp_ns,
            0,
            FrameDirection::Output,
            FrameRecord::Mark,
            0,
            payload,
        );
        self.log_events(&[&mark]);
        let mut session = self.state.lock().expect("session state");
        if session.data.oldest_seq == 0 {
            session.data.oldest_seq = seq;
        }
        session.data.latest_seq = seq;
        seq
    }

    pub fn log_decode_mark(&self, decode: &SessionDecode) -> u64 {
        self.log_mark(
            "session.decoder",
            serde_json::to_value(decode).unwrap_or(serde_json::Value::Null),
        )
    }

    pub fn log_gap(&self, from_seq: u64, to_seq: u64, dropped_bytes: u64) -> u64 {
        let _append = self.append_lock.lock().expect("session append");
        let seq = self.alloc_seq();
        let timestamp_ns = self.now_timestamp_ns();
        let gap = self.event(
            seq,
            timestamp_ns,
            0,
            FrameDirection::Output,
            FrameRecord::Gap,
            0,
            GapPayload {
                from_seq,
                to_seq,
                dropped_bytes,
            }
            .encode()
            .to_vec(),
        );
        self.log_events(&[&gap]);
        let mut session = self.state.lock().expect("session state");
        if session.data.oldest_seq == 0 {
            session.data.oldest_seq = seq;
        }
        session.data.latest_seq = seq;
        seq
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

    pub fn ring_evicted_through(&self) -> u64 {
        self.ring.lock().expect("session ring").evicted_through
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
        acquire: SessionAcquire,
        options: boot_protocol::SessionOptions,
        artifact_id: Option<&str>,
        inherit_artifact: bool,
        principal: &str,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<SessionAcquireResult, RuntimeError> {
        if kind != "rtt" && kind != "dap" {
            return Err(RuntimeError::new(
                "invalid_request",
                "probe session kind must be rtt or dap",
            ));
        }
        if kind == "dap" && max_subscribers != 1 {
            return Err(RuntimeError::new(
                "invalid_request",
                "DAP sessions require max_subscribers = 1",
            ));
        }
        self.create_session(
            resource_id,
            kind,
            acquire,
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
        acquire: SessionAcquire,
        options: boot_protocol::SessionOptions,
        principal: &str,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<SessionAcquireResult, RuntimeError> {
        self.create_session(
            resource_id,
            "serial",
            acquire,
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
        acquire: SessionAcquire,
        options: boot_protocol::SessionOptions,
        artifact_id: Option<&str>,
        inherit_artifact: bool,
        principal: &str,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<SessionAcquireResult, RuntimeError> {
        let resource = self.resource(resource_id).ok_or_else(|| {
            RuntimeError::not_found(format!("resource '{resource_id}' not found"))
        })?;
        if (kind == "serial") != (resource.kind == crate::runtime::ResourceKind::SerialPort) {
            return Err(RuntimeError::new(
                "invalid_request",
                "session kind does not match resource",
            ));
        }
        if let Some(existing) = self.active_session(resource_id, kind) {
            return match acquire {
                SessionAcquire::Create => Err(RuntimeError::new(
                    "session_active",
                    format!("resource already has an active session: {}", existing.id),
                )),
                SessionAcquire::Attach | SessionAcquire::AttachOrCreate => {
                    Ok(SessionAcquireResult {
                        session: existing,
                        created: false,
                    })
                }
            };
        }
        if acquire == SessionAcquire::Attach {
            return Err(RuntimeError::new(
                "session_not_found",
                format!("no active {kind} session for '{resource_id}'"),
            ));
        }
        let (options, decode, profile, dap_program) = if kind == "rtt" {
            let (options, decode, profile) = self
                .resolve_rtt_artifact(resource_id, artifact_id, inherit_artifact, options)
                .await?;
            (options, decode, profile, None)
        } else if kind == "dap" {
            let (decode, program) = self
                .resolve_dap_artifact(resource_id, artifact_id, inherit_artifact)
                .await?;
            (options, decode, None, program)
        } else {
            (options, SessionDecode::default(), None, None)
        };
        let id = new_id("ses");
        let lease = match lease::acquire(
            &self.inner,
            resource_id,
            &id,
            Duration::from_millis(wait_ms.min(30_000)),
        )
        .await
        {
            Ok(lease) => lease,
            Err(error) => {
                if matches!(
                    acquire,
                    SessionAcquire::Attach | SessionAcquire::AttachOrCreate
                ) {
                    if let Some(existing) = self.active_session(resource_id, kind) {
                        return Ok(SessionAcquireResult {
                            session: existing,
                            created: false,
                        });
                    }
                }
                return Err(error);
            }
        };
        let now = now_rfc3339();
        let mut session = Session {
            id: id.clone(),
            kind: kind.to_string(),
            resource_id: resource_id.to_string(),
            created_by: principal.to_string(),
            state: "starting".to_string(),
            created_at: now.clone(),
            updated_at: now,
            closed_at: None,
            subscribers: 0,
            max_subscribers: if kind == "dap" {
                1
            } else {
                max_subscribers.min(32)
            },
            detach_timeout_seconds: if kind == "dap" {
                0
            } else {
                detach_timeout_seconds.min(300)
            },
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
        let log = SessionLogWriter::open(
            &self.inner.config.session_dir,
            &id,
            kind,
            crate::runtime::now_unix_ns(),
        )
        .map_err(|error| RuntimeError::internal(error.to_string()))?;
        *handle.log.lock().expect("log") = Some(log);
        handle.set_decoder(profile.clone(), decode.epoch);
        if session.decode.artifact.is_some() {
            handle.log_decode_mark(&session.decode);
            session.data = handle.snapshot().data;
            handle.update(session.clone());
        }
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
            dap_program,
        );
        Ok(SessionAcquireResult {
            session,
            created: true,
        })
    }

    fn active_session(&self, resource_id: &str, kind: &str) -> Option<Session> {
        self.sessions_list().into_iter().find(|session| {
            session.resource_id == resource_id
                && session.kind == kind
                && matches!(session.state.as_str(), "starting" | "active" | "resuming")
        })
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

    async fn resolve_dap_artifact(
        &self,
        resource_id: &str,
        explicit_artifact: Option<&str>,
        inherit_artifact: bool,
    ) -> Result<(SessionDecode, Option<PathBuf>), RuntimeError> {
        let artifact_id = if let Some(id) = explicit_artifact {
            Some(id.to_string())
        } else if inherit_artifact {
            self.probe_artifact(resource_id).map(|artifact| artifact.id)
        } else {
            None
        };
        let Some(artifact_id) = artifact_id else {
            return Ok((SessionDecode::default(), None));
        };
        let path = crate::runtime::artifact::path(&self.inner, &artifact_id).await?;
        let artifact = crate::runtime::artifact::get(&self.inner, &artifact_id).await?;
        let decode = SessionDecode {
            mode: "raw".to_string(),
            epoch: 0,
            artifact: Some(boot_protocol::SessionArtifact {
                id: artifact.id,
                size: artifact.size,
                sha256: artifact.sha256,
                created_at: artifact.created_at,
                defmt: false,
                rtt_addr: None,
            }),
            rtt_addr: None,
            rtt_addr_source: "scan".to_string(),
        };
        Ok((decode, Some(path)))
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
        handle.log_decode_mark(&session.decode);
        session.data = handle.snapshot().data;
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
        handle.log_decode_mark(&session.decode);
        session.data = handle.snapshot().data;
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
        handle.log_mark("session.closed", serde_json::json!({}));
        session.data = handle.snapshot().data;
        self.inner
            .store
            .remove_artifact_refs_for_owner("session", id)
            .await?;
        self.persist_session(&session).await?;
        if purge {
            self.inner.store.purge_session_data(id).await?;
            let directory = self.inner.config.session_dir.join(id);
            if let Ok(entries) = std::fs::read_dir(&directory) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("session.log") {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            let _ = std::fs::remove_dir(&directory);
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
        handle.log_mark("session.resumed", serde_json::json!({}));
        if session.decode.artifact.is_some() {
            handle.log_decode_mark(&session.decode);
        }
        session.data = handle.snapshot().data;
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
            None,
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
            let scan = match session_log::scan(&path) {
                Ok(scan) => scan,
                Err(error) => {
                    eprintln!("session {} log is unreadable: {error}", record.id);
                    let corrupt = path.with_file_name(format!(
                        "session.log.corrupt-{}",
                        crate::runtime::now_unix()
                    ));
                    let _ = std::fs::rename(&path, corrupt);
                    session_log::LogScan {
                        header: None,
                        first_seq: 0,
                        latest_seq: 0,
                        input_bytes: 0,
                        last_good_offset: 0,
                        problem: Some(session_log::LogProblem::Corrupt {
                            offset: 0,
                            message: error.to_string(),
                        }),
                    }
                }
            };
            let mut log_corrupt = false;
            match &scan.problem {
                Some(session_log::LogProblem::Truncated { offset }) => {
                    eprintln!(
                        "session {} log is truncated at offset {}; truncating to {}",
                        record.id, offset, scan.last_good_offset
                    );
                    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
                        let _ = file.set_len(scan.last_good_offset);
                    }
                }
                Some(session_log::LogProblem::Corrupt { offset, message }) => {
                    eprintln!(
                        "session {} log is corrupt at offset {}: {}",
                        record.id, offset, message
                    );
                    log_corrupt = true;
                    let corrupt = path.with_file_name(format!(
                        "session.log.corrupt-{}",
                        crate::runtime::now_unix()
                    ));
                    let _ = std::fs::rename(&path, corrupt);
                }
                None => {}
            }
            let header_matches = scan
                .header
                .as_ref()
                .is_none_or(|header| header.session_id == record.id && header.kind == record.kind);
            let log_missing = scan.header.is_none();
            let latest = scan.latest_seq.max(record.latest_seq as u64);
            let oldest = if log_corrupt {
                latest.saturating_add(1)
            } else if scan.first_seq > 0 {
                scan.first_seq
            } else {
                record.first_seq as u64
            };
            let replayable = record.replayable != 0
                && header_matches
                && !log_corrupt
                && !(log_missing && record.latest_seq > 0);
            let lost_all = log_corrupt || (log_missing && record.latest_seq > 0);
            let gap_range = if lost_all {
                Some(((record.first_seq.max(1)) as u64, latest))
            } else if matches!(
                scan.problem,
                Some(session_log::LogProblem::Truncated { .. })
            ) && (record.latest_seq as u64) > scan.latest_seq
            {
                Some((scan.latest_seq.saturating_add(1), record.latest_seq as u64))
            } else {
                None
            };
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
                replayable,
                data: SessionData {
                    oldest_seq: oldest,
                    latest_seq: latest,
                    buffered_bytes: 0,
                    bytes: record.data_bytes as u64,
                    input_bytes: scan.input_bytes,
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
            let created_at_ns = (record.created_at as u64).saturating_mul(1_000_000_000);
            match SessionLogWriter::open(
                &inner.config.session_dir,
                &session.id,
                &session.kind,
                created_at_ns,
            ) {
                Ok(log) => {
                    *handle.log.lock().expect("log") = Some(log);
                    if let Some((from_seq, to_seq)) = gap_range
                        && from_seq <= to_seq
                    {
                        let dropped_bytes = if lost_all {
                            record.data_bytes as u64
                        } else {
                            0
                        };
                        let gap_seq = handle.log_gap(from_seq, to_seq, dropped_bytes);
                        if lost_all {
                            session.data.oldest_seq = gap_seq;
                            session.data.input_bytes = 0;
                            if !matches!(session.state.as_str(), "closed" | "failed") {
                                session.replayable = true;
                            }
                        }
                        session.data.latest_seq = gap_seq;
                        handle.update(session.clone());
                    }
                }
                Err(error) => {
                    eprintln!("session {} log is unreadable: {error}", session.id);
                    let mut state = handle.state.lock().expect("session state");
                    state.replayable = false;
                    state.last_error = Some("session_log_unreadable".to_string());
                }
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
        handle.log_mark("session.closed", serde_json::json!({}));
        session.data = handle.snapshot().data;
        handle.shutdown.cancel();
        let _ = handle.lease.lock().expect("lease").take();
        let _ = inner
            .store
            .remove_artifact_refs_for_owner("session", &session.id)
            .await;
        let _ = persist_session_record(&inner, &session, &handle.options_json).await;
    });
}

pub(crate) async fn fail_session(inner: &RuntimeInner, handle: &SessionHandle, error: String) {
    let mut session = handle.snapshot();
    session.state = if session.kind == "dap" {
        "failed".to_string()
    } else {
        "suspended".to_string()
    };
    session.last_error = Some(error);
    session.updated_at = now_rfc3339();
    handle.update(session.clone());
    handle.set_decoder(None, session.decode.epoch);
    let event = if session.kind == "dap" {
        "session.failed"
    } else {
        "session.suspended"
    };
    handle.log_mark(
        event,
        serde_json::json!({"error": session.last_error.clone()}),
    );
    session.data = handle.snapshot().data;
    let _ = handle.lease.lock().expect("lease").take();
    let _ = persist_session_record(inner, &session, &handle.options_json).await;
    handle.shutdown.cancel();
}

pub async fn close_dap_session(
    inner: &Arc<RuntimeInner>,
    handle: &Arc<SessionHandle>,
) -> Result<(), RuntimeError> {
    handle.shutdown.cancel();
    let mut session = handle.snapshot();
    session.state = "closed".to_string();
    session.closed_at = Some(now_rfc3339());
    session.updated_at = now_rfc3339();
    handle.set_decoder(None, session.decode.epoch);
    handle.update(session.clone());
    handle.log_mark("session.closed", serde_json::json!({}));
    session.data = handle.snapshot().data;
    let _ = handle.lease.lock().expect("lease").take();
    let _ = inner
        .store
        .remove_artifact_refs_for_owner("session", &session.id)
        .await;
    persist_session_record(inner, &session, &handle.options_json).await
}

fn spawn_session_thread(
    inner: Arc<RuntimeInner>,
    handle: Arc<SessionHandle>,
    selector: Option<String>,
    path: Option<String>,
    options: boot_protocol::SessionOptions,
    dap_program: Option<PathBuf>,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<ThreadEvent>();
    let shutdown = handle.shutdown.clone();
    let kind = handle.kind.clone();
    if kind == "serial" {
        let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
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
    } else if kind == "dap" {
        #[cfg(feature = "run")]
        {
            let Some(selector) = selector else {
                let handle = handle.clone();
                let inner = inner.clone();
                tokio::spawn(async move {
                    fail_session(&inner, &handle, "probe selector missing".into()).await;
                });
                return;
            };
            let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(64);
            let (worker_tx, mut worker_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
            *handle.input_tx.lock().expect("input") = Some(input_tx);
            let resolver = std::sync::Arc::new(RuntimeArtifactResolver {
                inner: inner.clone(),
            });
            let thread_event_tx = event_tx.clone();
            let thread_handle = handle.clone();
            let thread_shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name(format!("boot-dap-{}", handle.id))
                .stack_size(4 * 1024 * 1024)
                .spawn(move || {
                    let _ = thread_event_tx.send(ThreadEvent::Active);
                    let mut backend = NativeDapBackend;
                    let result = backend.run(
                        DapIo {
                            input: input_rx,
                            output: worker_tx,
                        },
                        DapLaunch {
                            probe_selector: selector,
                            program_override: dap_program,
                            artifacts: resolver,
                        },
                        thread_shutdown,
                    );
                    match result {
                        Ok(()) => {
                            let _ = thread_event_tx.send(ThreadEvent::Ended);
                        }
                        Err(error) => {
                            let _ = thread_event_tx.send(ThreadEvent::Error(error.to_string()));
                        }
                    }
                    drop(thread_handle);
                })
                .ok();

            let inner_for_task = inner.clone();
            let handle_for_task = handle.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        data = worker_rx.recv() => {
                            if let Some(data) = data {
                                handle_for_task.push_output(data, 0);
                            }
                        }
                        event = event_rx.recv() => {
                            let Some(event) = event else { break };
                            if !process_thread_event(&inner_for_task, &handle_for_task, event).await {
                                break;
                            }
                        }
                    }
                }
            });
            return;
        }
        #[cfg(not(feature = "run"))]
        {
            let handle = handle.clone();
            let inner = inner.clone();
            tokio::spawn(async move {
                fail_session(&inner, &handle, "DAP support is not compiled in".into()).await;
            });
            return;
        }
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
            if !process_thread_event(&inner_for_task, &handle_for_task, event).await {
                break;
            }
        }
    });
}

async fn process_thread_event(
    inner: &Arc<RuntimeInner>,
    handle: &Arc<SessionHandle>,
    event: ThreadEvent,
) -> bool {
    match event {
        ThreadEvent::Active => {
            let mut session = handle.snapshot();
            session.state = "active".to_string();
            session.updated_at = now_rfc3339();
            handle.update(session.clone());
            handle.log_mark("session.active", serde_json::json!({}));
            session.data = handle.snapshot().data;
            let _ = persist_session_record(inner, &session, &handle.options_json).await;
            if session.subscribers == 0 && session.kind != "dap" {
                schedule_close_if_idle(inner.clone(), handle.clone());
            }
            true
        }
        ThreadEvent::Data { data, stream } => {
            handle.push_output(data, stream);
            true
        }
        ThreadEvent::Error(error) => {
            fail_session(inner, handle, error).await;
            false
        }
        ThreadEvent::Ended => {
            let mut session = handle.snapshot();
            session.state = "closed".to_string();
            session.closed_at = Some(now_rfc3339());
            session.updated_at = now_rfc3339();
            handle.set_decoder(None, session.decode.epoch);
            handle.update(session.clone());
            handle.log_mark("session.closed", serde_json::json!({}));
            session.data = handle.snapshot().data;
            let _ = handle.lease.lock().expect("lease").take();
            let _ = inner
                .store
                .remove_artifact_refs_for_owner("session", &session.id)
                .await;
            let _ = persist_session_record(inner, &session, &handle.options_json).await;
            false
        }
    }
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
                    stream: 0,
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
                        stream: channel as u8,
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

// Keep the public constant accessible for handlers.
pub const _MAX_SESSION_LIFETIME: Duration = Duration::from_secs(8 * 3600);

#[cfg(test)]
mod tests {
    use super::*;
    use boot_protocol::{RttRecord, SESSION_FRAME_HEADER_LEN};

    fn test_session(kind: &str) -> Session {
        Session {
            id: format!("ses_{kind}"),
            kind: kind.into(),
            resource_id: "probe_test".into(),
            created_by: "principal".into(),
            state: "active".into(),
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
            closed_at: None,
            subscribers: 0,
            max_subscribers: 8,
            detach_timeout_seconds: 60,
            resume_count: 0,
            replayable: true,
            data: SessionData::default(),
            decode: SessionDecode::default(),
            last_error: None,
        }
    }

    fn test_event(
        seq: u64,
        record: FrameRecord,
        direction: FrameDirection,
        payload: &[u8],
    ) -> SessionEvent {
        SessionEvent {
            seq,
            timestamp_ns: seq * 1_000,
            stream: 0,
            direction,
            record,
            flags: 0,
            payload: payload.to_vec(),
        }
    }

    fn log_header(seq: u64, record: FrameRecord, flags: u8) -> SessionFrameHeader {
        SessionFrameHeader {
            flags,
            record,
            direction: FrameDirection::Output,
            stream: 0,
            seq,
            timestamp_ns: seq * 1_000,
        }
    }

    #[test]
    fn live_ring_evicts_oldest_and_preserves_cursor_order() {
        let mut ring = SessionRing::new(SESSION_FRAME_HEADER_LEN + 10);
        ring.push(test_event(
            1,
            FrameRecord::Data,
            FrameDirection::Output,
            b"abc",
        ));
        ring.push(test_event(
            2,
            FrameRecord::Data,
            FrameDirection::Output,
            b"def",
        ));
        // The first chunk is evicted to keep the live tail bounded.
        assert_eq!(ring.after(0).len(), 1);
        assert_eq!(ring.after(0)[0].seq, 2);
        assert_eq!(ring.after(1).len(), 1);
        assert_eq!(ring.after(1)[0].seq, 2);
        assert!(ring.after(2).is_empty());
        assert_eq!(ring.evicted_through, 1);
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
    fn dap_output_and_input_are_logged_for_replay() {
        let directory = std::env::temp_dir().join(format!(
            "boot-api-session-dap-log-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let handle = SessionHandle::new(test_session("dap"), directory.clone(), "{}".into());
        let writer = SessionLogWriter::open(&directory, "ses_dap", "dap", 1).unwrap();
        *handle.log.lock().expect("log") = Some(writer);
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        *handle.input_tx.lock().expect("input") = Some(tx);

        handle.push_input(0, b"request".to_vec()).unwrap();
        handle.push_output(b"response".to_vec(), 0);
        drop(rx);

        let events = handle.replay(0, true);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].record, FrameRecord::Data);
        assert_eq!(events[0].direction, FrameDirection::Input);
        assert_eq!(events[0].payload, b"request");
        assert_eq!(events[1].direction, FrameDirection::Output);
        assert_eq!(events[1].payload, b"response");
        assert_eq!(handle.snapshot().data.bytes, 8);
        assert_eq!(handle.snapshot().data.input_bytes, 7);
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
        let handle = SessionHandle::new(test_session("rtt"), directory.clone(), "{}".into());
        let path = session_log::session_log_path(&directory, "ses_rtt");
        let mut writer = SessionLogWriter::open(&directory, "ses_rtt", "rtt", 1).unwrap();
        let empty = RttRecordsPayload {
            v: 1,
            epoch: 1,
            records: Vec::new(),
            raw_fallback: None,
            error: None,
        };
        writer
            .append(
                &log_header(1, FrameRecord::RttRecords, 0),
                &serde_json::to_vec(&empty).unwrap(),
            )
            .unwrap();
        writer
            .append(
                &log_header(1, FrameRecord::Data, FRAME_FLAG_SUPERSEDED),
                b"partial",
            )
            .unwrap();
        let one = RttRecordsPayload {
            v: 1,
            epoch: 1,
            records: vec![RttRecord {
                timestamp: None,
                level: Some("info".into()),
                module: None,
                file: None,
                line: None,
                message: "hello".into(),
            }],
            raw_fallback: None,
            error: None,
        };
        writer
            .append(
                &log_header(2, FrameRecord::RttRecords, 0),
                &serde_json::to_vec(&one).unwrap(),
            )
            .unwrap();
        writer
            .append(
                &log_header(2, FrameRecord::Data, FRAME_FLAG_SUPERSEDED),
                b"frame",
            )
            .unwrap();
        drop(writer);

        let events = handle.replay(0, false);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[0].record, FrameRecord::RttRecords);
        let first: RttRecordsPayload = serde_json::from_slice(&events[0].payload).unwrap();
        assert!(first.records.is_empty());
        assert_eq!(events[1].seq, 2);
        let second: RttRecordsPayload = serde_json::from_slice(&events[1].payload).unwrap();
        assert_eq!(second.records.len(), 1);
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
