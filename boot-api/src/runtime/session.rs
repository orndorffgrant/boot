use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use boot_protocol::{Session, SessionData, SessionPatch, Subscriber};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

use crate::runtime::lease::LeaseGuard;
use crate::runtime::session_log::{self, LogRecord, RECORD_INPUT, RECORD_OUTPUT, SessionLogWriter};
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

struct RingChunk {
    seq: u64,
    channel: u8,
    data: Vec<u8>,
}

struct SessionRing {
    chunks: VecDeque<RingChunk>,
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

    fn push(&mut self, data: Vec<u8>, channel: u8) -> u64 {
        let seq = self.latest_seq + 1;
        self.latest_seq = seq;
        self.bytes += data.len();
        self.chunks.push_back(RingChunk { seq, channel, data });
        while self.bytes > self.max_bytes {
            if let Some(chunk) = self.chunks.pop_front() {
                self.bytes = self.bytes.saturating_sub(chunk.data.len());
            } else {
                break;
            }
        }
        seq
    }

    fn after(&self, cursor: u64) -> Vec<(u64, u8, Vec<u8>)> {
        self.chunks
            .iter()
            .filter(|chunk| chunk.seq > cursor)
            .map(|chunk| (chunk.seq, chunk.channel, chunk.data.clone()))
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
        let seq = {
            let mut ring = self.ring.lock().expect("session ring");
            ring.push(data.clone(), channel)
        };
        if let Some(log) = self.log.lock().expect("session log").as_mut() {
            let _ = log.append(&LogRecord {
                seq,
                timestamp_ms: now_unix() as u64 * 1000,
                kind: RECORD_OUTPUT,
                channel,
                payload: data.clone(),
            });
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

    pub fn replay_records(&self, after: u64) -> Vec<LogRecord> {
        let log_path = session_log::session_log_path(&self.log_dir, &self.id);
        session_log::read_records(&log_path)
            .unwrap_or_default()
            .into_iter()
            .filter(|record| record.seq > after)
            .collect()
    }

    pub fn replay(&self, after: u64, include_input: bool) -> Vec<(u64, u8, Vec<u8>, bool)> {
        let log_path = session_log::session_log_path(&self.log_dir, &self.id);
        let records = session_log::read_records(&log_path).unwrap_or_default();
        let mut out = Vec::new();
        for record in records {
            if record.seq <= after {
                continue;
            }
            if record.kind == RECORD_INPUT {
                if include_input {
                    out.push((record.seq, record.channel, record.payload, true));
                }
            } else if record.kind == RECORD_OUTPUT {
                out.push((record.seq, record.channel, record.payload, false));
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

    pub fn read_after(&self, cursor: u64) -> Vec<(u64, u8, Vec<u8>)> {
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
            principal,
            max_subscribers,
            detach_timeout_seconds,
            wait_ms,
        )
        .await
    }

    async fn create_session(
        &self,
        resource_id: &str,
        kind: &str,
        options: boot_protocol::SessionOptions,
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
        self.inner
            .sessions
            .lock()
            .expect("sessions lock")
            .insert(id.clone(), handle.clone());
        self.persist_session(&session).await?;
        spawn_session_thread(
            self.inner.clone(),
            handle,
            resource.selector,
            resource.path,
            options,
        );
        Ok(session)
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

    pub async fn close_session(&self, id: &str, purge: bool) -> Result<(), RuntimeError> {
        let handle = self.session_handle(id)?;
        handle.shutdown.cancel();
        let mut session = handle.snapshot();
        session.state = "closed".to_string();
        session.closed_at = Some(now_rfc3339());
        session.updated_at = now_rfc3339();
        handle.update(session.clone());
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
        handle.update(session.clone());
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
        handle.update(session.clone());
        handle.shutdown.cancel();
        let _ = handle.lease.lock().expect("lease").take();
        let _ = persist_session_record(&inner, &session, &handle.options_json).await;
    });
}

async fn fail_session(inner: &RuntimeInner, handle: &SessionHandle, error: String) {
    let mut session = handle.snapshot();
    session.state = "suspended".to_string();
    session.last_error = Some(error);
    session.updated_at = now_rfc3339();
    handle.update(session.clone());
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
                    handle_for_task.update(session.clone());
                    let _ = handle_for_task.lease.lock().expect("lease").take();
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

// Keep the public constant accessible for handlers.
pub const _MAX_SESSION_LIFETIME: Duration = Duration::from_secs(8 * 3600);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_ring_evicts_oldest_and_preserves_cursor_order() {
        let mut ring = SessionRing::new(4);
        assert_eq!(ring.push(b"abc".to_vec(), 0), 1);
        assert_eq!(ring.push(b"def".to_vec(), 0), 2);
        // The first chunk is evicted to keep the live tail bounded.
        assert_eq!(ring.after(0).len(), 1);
        assert_eq!(ring.after(0)[0].0, 2);
        assert_eq!(ring.after(1).len(), 1);
        assert_eq!(ring.after(1)[0].0, 2);
        assert!(ring.after(2).is_empty());
    }
}
