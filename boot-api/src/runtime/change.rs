use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use boot_protocol::{
    Change, ChangeError, ChangeWsMessage, FlashOptions, LogEntry, PROTOCOL_VERSION, Progress, Task,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::runtime::artifact;
use crate::runtime::{Runtime, RuntimeError, RuntimeInner, lease, new_id, now_rfc3339, now_unix};
use crate::store::ChangeRecord;
use crate::store::RuntimeStore;

pub struct ChangeHandle {
    pub id: String,
    pub kind: String,
    pub resource_id: String,
    pub created_by: String,
    pub state: Mutex<Change>,
    pub seq: AtomicU64,
    pub tx: broadcast::Sender<ChangeWsMessage>,
    pub history: Mutex<VecDeque<ChangeWsMessage>>,
    pub cancel: CancellationToken,
    pub finished: Mutex<bool>,
}

impl ChangeHandle {
    fn new(change: Change) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            id: change.id.clone(),
            kind: change.kind.clone(),
            resource_id: change.resource_id.clone(),
            created_by: change.created_by.clone(),
            state: Mutex::new(change),
            seq: AtomicU64::new(0),
            tx,
            history: Mutex::new(VecDeque::new()),
            cancel: CancellationToken::new(),
            finished: Mutex::new(false),
        }
    }

    pub fn snapshot(&self) -> Change {
        self.state.lock().expect("change state").clone()
    }

    fn publish(&self, message: ChangeWsMessage) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let message = with_seq(message, seq);
        {
            let mut history = self.history.lock().expect("change history");
            history.push_back(message.clone());
            while history.len() > 256 {
                history.pop_front();
            }
        }
        let _ = self.tx.send(message);
    }

    fn publish_change(&self, change: Change, completed: bool) {
        *self.state.lock().expect("change state") = change.clone();
        if completed {
            self.publish(ChangeWsMessage::ChangeCompleted {
                v: PROTOCOL_VERSION,
                seq: 0,
                change,
            });
            *self.finished.lock().expect("finished") = true;
        } else {
            self.publish(ChangeWsMessage::ChangeUpdated {
                v: PROTOCOL_VERSION,
                seq: 0,
                change,
            });
        }
    }

    fn publish_task(&self, task: Task) {
        self.publish(ChangeWsMessage::TaskUpdated {
            v: PROTOCOL_VERSION,
            seq: 0,
            task,
        });
    }

    pub fn follow(
        &self,
        after: Option<u64>,
    ) -> (Vec<ChangeWsMessage>, broadcast::Receiver<ChangeWsMessage>) {
        let history = self.history.lock().expect("change history");
        let messages = match after {
            Some(after) => history
                .iter()
                .filter(|message| message_seq(message) > after)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        (messages, self.tx.subscribe())
    }
}

fn with_seq(message: ChangeWsMessage, seq: u64) -> ChangeWsMessage {
    match message {
        ChangeWsMessage::Hello {
            v,
            protocol,
            change,
            replayed,
            ..
        } => ChangeWsMessage::Hello {
            v,
            protocol,
            seq,
            change,
            replayed,
        },
        ChangeWsMessage::ChangeUpdated { v, change, .. } => {
            ChangeWsMessage::ChangeUpdated { v, seq, change }
        }
        ChangeWsMessage::TaskUpdated { v, task, .. } => {
            ChangeWsMessage::TaskUpdated { v, seq, task }
        }
        ChangeWsMessage::ChangeLog { v, entries, .. } => {
            ChangeWsMessage::ChangeLog { v, seq, entries }
        }
        ChangeWsMessage::ChangeCompleted { v, change, .. } => {
            ChangeWsMessage::ChangeCompleted { v, seq, change }
        }
    }
}

fn message_seq(message: &ChangeWsMessage) -> u64 {
    match message {
        ChangeWsMessage::Hello { seq, .. }
        | ChangeWsMessage::ChangeUpdated { seq, .. }
        | ChangeWsMessage::TaskUpdated { seq, .. }
        | ChangeWsMessage::ChangeLog { seq, .. }
        | ChangeWsMessage::ChangeCompleted { seq, .. } => *seq,
    }
}

impl Runtime {
    pub async fn create_flash(
        &self,
        resource_id: &str,
        artifact_id: &str,
        options: FlashOptions,
        principal: &str,
    ) -> Result<Change, RuntimeError> {
        let path = artifact::path(&self.inner, artifact_id).await?;
        let change = self
            .create_change(
                "flash",
                resource_id,
                principal,
                format!("flash firmware to {resource_id}"),
                serde_json::json!({"artifact": artifact_id}),
            )
            .await?;
        let handle = self.change_handle(&change.id)?;
        let inner = self.inner.clone();
        tokio::spawn(async move {
            run_flash(inner, handle, path, options).await;
        });
        Ok(change)
    }

    pub async fn create_reset(
        &self,
        resource_id: &str,
        principal: &str,
    ) -> Result<Change, RuntimeError> {
        let change = self
            .create_change(
                "reset",
                resource_id,
                principal,
                format!("reset {resource_id}"),
                serde_json::json!({}),
            )
            .await?;
        let handle = self.change_handle(&change.id)?;
        let inner = self.inner.clone();
        tokio::spawn(async move {
            run_reset(inner, handle).await;
        });
        Ok(change)
    }

    pub async fn create_inspect(
        &self,
        resource_id: &str,
        principal: &str,
    ) -> Result<Change, RuntimeError> {
        let change = self
            .create_change(
                "probe.inspect",
                resource_id,
                principal,
                format!("inspect {resource_id}"),
                serde_json::json!({}),
            )
            .await?;
        let handle = self.change_handle(&change.id)?;
        let inner = self.inner.clone();
        tokio::spawn(async move {
            run_inspect(inner, handle).await;
        });
        Ok(change)
    }

    async fn create_change(
        &self,
        kind: &str,
        resource_id: &str,
        principal: &str,
        summary: String,
        data: serde_json::Value,
    ) -> Result<Change, RuntimeError> {
        let resource = self.resource(resource_id).ok_or_else(|| {
            RuntimeError::not_found(format!("resource '{resource_id}' not found"))
        })?;
        if resource.kind != crate::runtime::ResourceKind::Probe {
            return Err(RuntimeError::new(
                "invalid_request",
                "change requires a probe resource",
            ));
        }
        let now = now_rfc3339();
        let change = Change {
            id: new_id("chg"),
            kind: kind.to_string(),
            summary,
            resource_id: resource_id.to_string(),
            created_by: principal.to_string(),
            status: "queued".to_string(),
            ready: false,
            spawn_time: now,
            ready_time: None,
            tasks: Vec::new(),
            error: None,
            data,
            log: Vec::new(),
            log_truncated: false,
        };
        let handle = Arc::new(ChangeHandle::new(change.clone()));
        self.inner
            .changes
            .lock()
            .expect("changes lock")
            .insert(change.id.clone(), handle);
        persist_change(&self.inner, &change).await?;
        Ok(change)
    }

    pub fn change_handle(&self, id: &str) -> Result<Arc<ChangeHandle>, RuntimeError> {
        self.inner
            .changes
            .lock()
            .expect("changes lock")
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::not_found(format!("change '{id}' not found")))
    }

    pub fn changes(&self) -> Vec<Change> {
        let mut changes: Vec<_> = self
            .inner
            .changes
            .lock()
            .expect("changes lock")
            .values()
            .map(|handle| handle.snapshot())
            .collect();
        changes.sort_by(|a, b| b.spawn_time.cmp(&a.spawn_time));
        changes
    }

    pub async fn abort_change(&self, id: &str) -> Result<Change, RuntimeError> {
        let handle = self.change_handle(id)?;
        let mut change = handle.snapshot();
        if change.ready {
            return Ok(change);
        }
        handle.cancel.cancel();
        change.status = "cancelling".to_string();
        handle.publish_change(change.clone(), false);
        persist_change(&self.inner, &change).await?;
        Ok(change)
    }
}

async fn run_flash(
    inner: Arc<RuntimeInner>,
    handle: Arc<ChangeHandle>,
    path: std::path::PathBuf,
    options: FlashOptions,
) {
    let _lease = match lease::acquire(
        &inner,
        &handle.resource_id,
        &handle.id,
        inner.config.lease_wait,
    )
    .await
    {
        Ok(lease) => lease,
        Err(error) => {
            fail(&inner, &handle, "lease_failed", error.message).await;
            return;
        }
    };
    if handle.cancel.is_cancelled() {
        cancel(&inner, &handle).await;
        return;
    }
    let mut change = handle.snapshot();
    change.status = "doing".to_string();
    handle.publish_change(change.clone(), false);
    let _ = persist_change(&inner, &change).await;

    let task = new_task("program", "Program firmware");
    change.tasks = vec![task.clone()];
    handle.publish_change(change.clone(), false);
    handle.publish_task(task.clone());
    let selector = inner
        .resources
        .lock()
        .expect("resources lock")
        .get(&handle.resource_id)
        .and_then(|resource| resource.selector.clone());
    let Some(selector) = selector else {
        fail(
            &inner,
            &handle,
            "invalid_resource",
            "probe selector is missing".to_string(),
        )
        .await;
        return;
    };
    let path_clone = path.clone();
    let result =
        tokio::task::spawn_blocking(move || flash_probe(&selector, &path_clone, options.reset))
            .await
            .unwrap_or_else(|error| Err(format!("worker join failed: {error}")));
    match result {
        Ok(()) => {
            if handle.cancel.is_cancelled() {
                cancel(&inner, &handle).await;
                return;
            }
            let mut task = task;
            task.status = "done".to_string();
            task.progress = Some(Progress {
                done: 1,
                total: Some(1),
                percent: Some(100.0),
                unit: "stage".into(),
                message: Some("complete".into()),
            });
            task.ready_time = Some(now_rfc3339());
            handle.publish_task(task.clone());
            let mut change = handle.snapshot();
            change.status = "done".to_string();
            change.ready = true;
            change.ready_time = Some(now_rfc3339());
            change.tasks = vec![task];
            handle.publish_change(change.clone(), true);
            let _ = persist_change(&inner, &change).await;
        }
        Err(error) => fail(&inner, &handle, "flash_failed", error).await,
    }
}

async fn run_reset(inner: Arc<RuntimeInner>, handle: Arc<ChangeHandle>) {
    let _lease = match lease::acquire(
        &inner,
        &handle.resource_id,
        &handle.id,
        inner.config.lease_wait,
    )
    .await
    {
        Ok(lease) => lease,
        Err(error) => {
            fail(&inner, &handle, "lease_failed", error.message).await;
            return;
        }
    };
    let selector = inner
        .resources
        .lock()
        .expect("resources lock")
        .get(&handle.resource_id)
        .and_then(|resource| resource.selector.clone());
    let Some(selector) = selector else {
        fail(
            &inner,
            &handle,
            "invalid_resource",
            "probe selector is missing".to_string(),
        )
        .await;
        return;
    };
    let result = tokio::task::spawn_blocking(move || reset_probe(&selector))
        .await
        .unwrap_or_else(|error| Err(format!("worker join failed: {error}")));
    match result {
        Ok(()) => {
            let mut change = handle.snapshot();
            change.status = "done".to_string();
            change.ready = true;
            change.ready_time = Some(now_rfc3339());
            handle.publish_change(change.clone(), true);
            let _ = persist_change(&inner, &change).await;
        }
        Err(error) => fail(&inner, &handle, "reset_failed", error).await,
    }
}

async fn run_inspect(inner: Arc<RuntimeInner>, handle: Arc<ChangeHandle>) {
    let _lease = match lease::acquire(
        &inner,
        &handle.resource_id,
        &handle.id,
        inner.config.lease_wait,
    )
    .await
    {
        Ok(lease) => lease,
        Err(error) => {
            fail(&inner, &handle, "lease_failed", error.message).await;
            return;
        }
    };
    let selector = inner
        .resources
        .lock()
        .expect("resources lock")
        .get(&handle.resource_id)
        .and_then(|resource| resource.selector.clone());
    let Some(selector) = selector else {
        fail(
            &inner,
            &handle,
            "invalid_resource",
            "probe selector is missing".to_string(),
        )
        .await;
        return;
    };
    let result = tokio::task::spawn_blocking(move || inspect_probe(&selector))
        .await
        .unwrap_or_else(|error| Err(format!("worker join failed: {error}")));
    match result {
        Ok(target) => {
            let mut change = handle.snapshot();
            change.status = "done".to_string();
            change.ready = true;
            change.ready_time = Some(now_rfc3339());
            change.data = serde_json::json!({"target": target});
            handle.publish_change(change.clone(), true);
            let _ = persist_change(&inner, &change).await;
        }
        Err(error) => fail(&inner, &handle, "inspect_failed", error).await,
    }
}

fn new_task(kind: &str, summary: &str) -> Task {
    Task {
        id: new_id("tsk"),
        kind: kind.to_string(),
        summary: summary.to_string(),
        status: "doing".to_string(),
        progress: None,
        spawn_time: now_rfc3339(),
        ready_time: None,
        data: serde_json::json!({}),
        log: Vec::new(),
    }
}

async fn fail(inner: &Arc<RuntimeInner>, handle: &Arc<ChangeHandle>, code: &str, message: String) {
    let mut change = handle.snapshot();
    change.status = "failed".to_string();
    change.ready = true;
    change.ready_time = Some(now_rfc3339());
    change.error = Some(ChangeError {
        code: code.to_string(),
        message: message.clone(),
        retryable: true,
    });
    change.log.push(LogEntry {
        time: now_rfc3339(),
        level: "error".into(),
        message,
    });
    handle.publish_change(change.clone(), true);
    let _ = persist_change(inner, &change).await;
}

async fn cancel(inner: &Arc<RuntimeInner>, handle: &Arc<ChangeHandle>) {
    let mut change = handle.snapshot();
    change.status = "cancelled".to_string();
    change.ready = true;
    change.ready_time = Some(now_rfc3339());
    handle.publish_change(change.clone(), true);
    let _ = persist_change(inner, &change).await;
}

pub async fn restore(inner: &Arc<RuntimeInner>) -> Result<()> {
    let records = inner.store.load_changes().await?;
    for record in records {
        let Ok(mut change) = serde_json::from_str::<Change>(&record.snapshot_json) else {
            continue;
        };
        if !change.ready {
            change.status = "failed".to_string();
            change.ready = true;
            change.ready_time = Some(now_rfc3339());
            change.error = Some(ChangeError {
                code: "server_restart".into(),
                message: "server restarted while change was running".into(),
                retryable: false,
            });
            let _ = persist_change(inner, &change).await;
        }
        let handle = Arc::new(ChangeHandle::new(change));
        inner
            .changes
            .lock()
            .expect("changes lock")
            .insert(handle.id.clone(), handle);
    }
    Ok(())
}

async fn persist_change(inner: &RuntimeInner, change: &Change) -> Result<(), RuntimeError> {
    let now = now_unix();
    let existing = inner.store.load_change(&change.id).await?;
    let created_at = existing
        .as_ref()
        .map(|record| record.created_at)
        .unwrap_or(now);
    let record = ChangeRecord {
        id: change.id.clone(),
        kind: change.kind.clone(),
        resource_id: change.resource_id.clone(),
        status: change.status.clone(),
        created_by: change.created_by.clone(),
        created_at,
        updated_at: now,
        ready_at: if change.ready { Some(now) } else { None },
        snapshot_json: serde_json::to_string(change)
            .map_err(|error| RuntimeError::internal(error.to_string()))?,
        log_json: serde_json::to_string(&change.log)
            .map_err(|error| RuntimeError::internal(error.to_string()))?,
    };
    if existing.is_some() {
        inner.store.update_change(&record).await?;
    } else {
        inner.store.insert_change(&record).await?;
    }
    Ok(())
}

fn flash_probe(selector: &str, path: &std::path::Path, reset: bool) -> Result<(), String> {
    use probe_rs::flashing;
    let selector = selector
        .parse::<probe_rs::probe::DebugProbeSelector>()
        .map_err(|error| format!("invalid selector: {error}"))?;
    let lister = probe_rs::probe::list::Lister::new();
    let probe = lister
        .open(selector)
        .map_err(|error| format!("failed to open probe: {error}"))?;
    let mut session = probe
        .attach(
            probe_rs::config::TargetSelector::Auto,
            probe_rs::Permissions::default(),
        )
        .map_err(|error| format!("failed to attach: {error}"))?;
    flashing::download_file(
        &mut session,
        path,
        flashing::ElfLoader(flashing::ElfOptions::default()),
    )
    .map_err(|error| format!("flash failed: {error}"))?;
    if reset {
        let mut core = session
            .core(0)
            .map_err(|error| format!("failed to access core: {error}"))?;
        core.reset()
            .map_err(|error| format!("reset failed: {error}"))?;
    }
    Ok(())
}

fn reset_probe(selector: &str) -> Result<(), String> {
    let selector = selector
        .parse::<probe_rs::probe::DebugProbeSelector>()
        .map_err(|error| format!("invalid selector: {error}"))?;
    let lister = probe_rs::probe::list::Lister::new();
    let probe = lister
        .open(selector)
        .map_err(|error| format!("failed to open probe: {error}"))?;
    let mut session = probe
        .attach(
            probe_rs::config::TargetSelector::Auto,
            probe_rs::Permissions::default(),
        )
        .map_err(|error| format!("failed to attach: {error}"))?;
    let mut core = session
        .core(0)
        .map_err(|error| format!("failed to access core: {error}"))?;
    core.reset()
        .map_err(|error| format!("reset failed: {error}"))?;
    Ok(())
}

fn inspect_probe(selector: &str) -> Result<String, String> {
    let selector = selector
        .parse::<probe_rs::probe::DebugProbeSelector>()
        .map_err(|error| format!("invalid selector: {error}"))?;
    let lister = probe_rs::probe::list::Lister::new();
    let probe = lister
        .open(selector)
        .map_err(|error| format!("failed to open probe: {error}"))?;
    let session = probe
        .attach(
            probe_rs::config::TargetSelector::Auto,
            probe_rs::Permissions::default(),
        )
        .map_err(|error| format!("failed to attach: {error}"))?;
    Ok(session.target().name.clone())
}
