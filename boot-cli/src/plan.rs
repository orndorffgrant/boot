// SPDX-License-Identifier: AGPL-3.0-or-later
//! Linear CLI plans, tasks, actions, and typed result views.

use std::marker::PhantomData;
use std::path::PathBuf;

use boot_protocol::{FlashOptions, Session};

use crate::domain::{
    CreateTokenIntent, FollowCursor, RevokeTokenIntent, ServerConfig, ServerOptions,
    ServerSelector, SessionAccess, SessionAcquire, SessionKind, SessionSelect, TokenSource,
};
use crate::error::CliError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub usize);

impl TaskId {
    pub fn number(self) -> usize {
        self.0 + 1
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.number())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    User,
    Verbose,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelPolicy {
    SafePoint,
    Immediate,
    NotCancellable,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Bytes,
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressMode {
    None,
    Spinner,
    Bar(Unit),
    ChildTasks,
    Stream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct Confirmation {
    pub prompt: String,
    pub default: bool,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct TaskPolicy {
    pub visible: Visibility,
    pub continue_on_error: bool,
    pub cancel: CancelPolicy,
}

impl Default for TaskPolicy {
    fn default() -> Self {
        Self {
            visible: Visibility::User,
            continue_on_error: false,
            cancel: CancelPolicy::SafePoint,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanWarning {
    pub level: Level,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct PlanTask {
    pub id: TaskId,
    pub title: String,
    pub detail: Option<String>,
    pub action: Action,
    pub progress: ProgressMode,
    pub policy: TaskPolicy,
    pub confirmation: Option<Confirmation>,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub title: String,
    pub tasks: Vec<PlanTask>,
    pub warnings: Vec<PlanWarning>,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
pub enum View {
    Fields(Vec<Field>),
    Table(Table),
    Text(String),
}

#[derive(Debug)]
pub enum Outcome {
    Done,
    View(View),
    Streamed { summary: Option<View> },
    Partial { view: View, failures: Vec<CliError> },
}

pub struct Key<T> {
    slot: usize,
    _marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Key<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Key<T> {}

impl<T> PartialEq for Key<T> {
    fn eq(&self, other: &Self) -> bool {
        self.slot == other.slot
    }
}

impl<T> Eq for Key<T> {}

impl<T> std::fmt::Debug for Key<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Key({})", self.slot)
    }
}

impl<T> Key<T> {
    pub fn slot(self) -> usize {
        self.slot
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Probe,
    SerialPort,
}

#[derive(Debug, Clone)]
pub struct ResourceHandle {
    pub id: String,
    pub kind: ResourceKind,
}

#[derive(Debug, Clone)]
pub struct ArtifactHandle {
    pub id: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactRef {
    None,
    Inherit,
    Clear,
    Id(String),
    Uploaded(Key<ArtifactHandle>),
}

#[derive(Debug, Clone)]
pub struct SessionHandle {
    pub server: ServerConfig,
    pub session: Session,
}

#[derive(Debug, Clone)]
pub struct LocalServer {
    pub cli_db: PathBuf,
    pub api_db: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub current: String,
    pub available: String,
    pub tag: String,
    pub html_url: String,
    pub prerelease: bool,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone)]
pub struct EditedDocument(pub String);

#[derive(Debug, Clone)]
pub struct TokenSecret(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenChange {
    Keep,
    Clear,
    Set(Key<TokenSecret>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectMode {
    None,
    Bounded(u32),
    UntilCancelled,
}

#[derive(Debug, Clone)]
pub struct FollowSessionPolicy {
    pub cursor: FollowCursor,
    pub access: SessionAccess,
    pub include_input: bool,
    pub reconnect: ReconnectMode,
    pub resume_on_suspend: bool,
    pub strict_replay: bool,
    pub pump_input: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ChangeKind {
    Flash {
        artifact: Option<Key<ArtifactHandle>>,
        options: FlashOptions,
    },
    Reset,
    Inspect,
}

#[derive(Debug, Clone)]
pub enum CollectRequest {
    Probes {
        server: Option<String>,
    },
    SerialPorts {
        server: Option<String>,
    },
    Sessions {
        server: Option<String>,
        select: SessionSelect,
    },
    Servers,
    Tokens {
        server: Option<String>,
        direct: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone)]
pub enum Action {
    LoadServer {
        selector: ServerSelector,
        out: Key<ServerConfig>,
    },
    ResolveResource {
        server: Key<ServerConfig>,
        selector: String,
        kind: ResourceKind,
        out: Key<ResourceHandle>,
    },
    Collect(CollectRequest),
    InspectServer {
        selector: ServerSelector,
        include_service: bool,
    },
    ReadToken {
        source: TokenSource,
        out: Key<TokenSecret>,
    },
    AddServer {
        name: String,
        url: String,
        ca_cert: Option<String>,
        token: Option<Key<TokenSecret>>,
    },
    UpdateServer {
        name: String,
        url: Option<String>,
        ca_cert: Option<Option<String>>,
        token: TokenChange,
    },
    OpenServerEditor {
        server: Key<ServerConfig>,
        out: Key<EditedDocument>,
    },
    ApplyServerDocument {
        server: Key<ServerConfig>,
        document: Key<EditedDocument>,
    },
    RemoveServer {
        name: String,
    },
    UploadArtifact {
        server: Key<ServerConfig>,
        path: PathBuf,
        out: Key<ArtifactHandle>,
    },
    RunChange {
        server: Key<ServerConfig>,
        resource: Key<ResourceHandle>,
        kind: ChangeKind,
    },
    LoadSession {
        server: Key<ServerConfig>,
        session_id: String,
        out: Key<SessionHandle>,
    },
    AttachSession {
        server: Key<ServerConfig>,
        resource: Option<Key<ResourceHandle>>,
        kind: SessionKind,
        acquire: SessionAcquire,
        explicit_session: Option<String>,
        artifact: ArtifactRef,
        baud_rate: Option<u32>,
        policy: FollowSessionPolicy,
    },
    FollowSession {
        session: Key<SessionHandle>,
        policy: FollowSessionPolicy,
    },
    ExportSession {
        server: Key<ServerConfig>,
        session: Key<SessionHandle>,
        out_path: PathBuf,
        format: String,
        include_input: bool,
    },
    CreateToken(CreateTokenIntent),
    RevokeToken(RevokeTokenIntent),
    Service {
        action: ServiceAction,
        options: Option<ServerOptions>,
        local: Option<Key<LocalServer>>,
    },
    PrepareLocalServer {
        options: ServerOptions,
        out: Key<LocalServer>,
    },
    RunServerForeground {
        options: ServerOptions,
        local: Key<LocalServer>,
    },
    FetchRelease {
        repo: Option<String>,
        version: Option<String>,
        out: Key<ReleaseInfo>,
    },
    InstallRelease {
        release: Key<ReleaseInfo>,
        force: bool,
        no_restart: bool,
        yes: bool,
    },
    ShowOpenApi,
}

pub struct PlanBuilder {
    plan: Plan,
    next_slot: usize,
}

impl PlanBuilder {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            plan: Plan {
                title: title.into(),
                tasks: Vec::new(),
                warnings: Vec::new(),
            },
            next_slot: 0,
        }
    }

    pub fn slot<T>(&mut self) -> Key<T> {
        let slot = self.next_slot;
        self.next_slot += 1;
        Key {
            slot,
            _marker: PhantomData,
        }
    }

    pub fn push(
        &mut self,
        title: impl Into<String>,
        action: Action,
        progress: ProgressMode,
    ) -> TaskId {
        self.push_with_policy(title, action, progress, TaskPolicy::default())
    }

    pub fn push_hidden(
        &mut self,
        title: impl Into<String>,
        action: Action,
        progress: ProgressMode,
    ) -> TaskId {
        let policy = TaskPolicy {
            visible: Visibility::Verbose,
            ..TaskPolicy::default()
        };
        self.push_with_policy(title, action, progress, policy)
    }

    pub fn push_with_policy(
        &mut self,
        title: impl Into<String>,
        action: Action,
        progress: ProgressMode,
        policy: TaskPolicy,
    ) -> TaskId {
        let id = TaskId(self.plan.tasks.len());
        self.plan.tasks.push(PlanTask {
            id,
            title: title.into(),
            detail: None,
            action,
            progress,
            policy,
            confirmation: None,
        });
        id
    }

    pub fn warn(&mut self, message: impl Into<String>) {
        self.plan.warnings.push(PlanWarning {
            level: Level::Warn,
            message: message.into(),
        });
    }

    pub fn finish(self) -> Plan {
        self.plan
    }
}
