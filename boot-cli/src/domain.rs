// SPDX-License-Identifier: AGPL-3.0-or-later
//! Terminal-independent command intents and selector types.

use std::path::PathBuf;

use crate::error::CliError;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub name: String,
    pub url: String,
    pub ca_cert: Option<String>,
    pub token: Option<String>,
}

impl ServerConfig {
    pub fn tls(&self) -> bool {
        self.url.starts_with("https://")
    }
}

#[derive(Debug, Clone)]
pub struct ServerSelector(pub String);

impl Default for ServerSelector {
    fn default() -> Self {
        Self("local".to_string())
    }
}

impl ServerSelector {
    pub fn named(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct ResourceSelector {
    pub server: String,
    pub selector: String,
}

impl ResourceSelector {
    pub fn parse(value: &str) -> Result<Self, CliError> {
        let (server, selector) = value
            .split_once('/')
            .filter(|(server, selector)| !server.is_empty() && !selector.is_empty())
            .ok_or_else(|| {
                CliError::usage(format!(
                    "invalid selector '{value}'; expected <server>/<resource>"
                ))
            })?;
        Ok(Self {
            server: server.to_string(),
            selector: selector.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Rtt,
    Serial,
    Dap,
}

impl SessionKind {
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Rtt => "rtt",
            Self::Serial => "serial",
            Self::Dap => "dap",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Rtt => "RTT",
            Self::Serial => "serial",
            Self::Dap => "DAP",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAcquire {
    AttachOrCreate,
    AttachOnly,
    CreateOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAccess {
    Read,
    Write,
}

impl SessionAccess {
    pub fn api_name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowCursor {
    Latest,
    Start,
    After(u64),
}

#[derive(Debug, Clone)]
pub enum ArtifactSource {
    Upload(PathBuf),
    Existing(String),
    Inherit,
    Clear,
}

#[derive(Debug, Clone)]
pub enum AttachPolicy {
    None,
    Rtt,
    Serial(ResourceSelector),
}

#[derive(Debug, Clone, Default)]
pub struct TokenSource {
    pub stdin: bool,
    pub file: Option<PathBuf>,
    pub value: Option<String>,
}

impl TokenSource {
    pub fn is_empty(&self) -> bool {
        !self.stdin && self.file.is_none() && self.value.is_none()
    }

    pub fn validate(&self) -> Result<(), CliError> {
        let sources = usize::from(self.stdin)
            + usize::from(self.file.is_some())
            + usize::from(self.value.is_some());
        if sources > 1 {
            return Err(CliError::usage(
                "use only one of --token-stdin, --token-file, or --token",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub no_provision: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub allow_insecure_http: bool,
    pub advertise_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSelect {
    Active,
    Past,
    All,
}

#[derive(Debug, Clone)]
pub struct RunFirmwareIntent {
    pub target: ResourceSelector,
    pub firmware: PathBuf,
    pub attach: AttachPolicy,
}

#[derive(Debug, Clone)]
pub struct AttachProbeIntent {
    pub target: ResourceSelector,
    pub artifact: ArtifactSource,
    pub acquire: SessionAcquire,
    pub access: SessionAccess,
    pub session: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AttachSerialIntent {
    pub target: ResourceSelector,
    pub acquire: SessionAcquire,
    pub access: SessionAccess,
    pub session: Option<String>,
    pub baud_rate: u32,
}

#[derive(Debug, Clone)]
pub struct ReplaySessionIntent {
    pub target: ResourceSelector,
    pub from: u64,
    pub include_input: bool,
}

#[derive(Debug, Clone)]
pub struct ExportSessionIntent {
    pub target: ResourceSelector,
    pub out: PathBuf,
    pub format: String,
    pub include_input: bool,
}

#[derive(Debug, Clone)]
pub struct AddServerIntent {
    pub name: String,
    pub url: String,
    pub token: TokenSource,
    pub ca_cert: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct EditServerIntent {
    pub name: String,
    pub url: Option<String>,
    pub ca_cert: Option<Option<PathBuf>>,
    pub token: TokenSource,
    pub clear_token: bool,
}

#[derive(Debug, Clone)]
pub struct CreateTokenIntent {
    pub server: Option<String>,
    pub direct: bool,
    pub name: String,
    pub read_only: bool,
    pub scopes: Vec<String>,
    pub expires_in: Option<String>,
    pub replace: bool,
    pub save_to: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RevokeTokenIntent {
    pub target: Option<String>,
    pub direct: bool,
    pub all: bool,
    pub yes: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateIntent {
    pub check: bool,
    pub version: Option<String>,
    pub force: bool,
    pub yes: bool,
    pub no_restart: bool,
    pub repo: Option<String>,
}

impl CommandIntent {
    pub fn token_api_without_direct(&self) -> bool {
        match self {
            Self::CreateToken(intent) => !intent.direct,
            Self::ListTokens { direct, .. } => !direct,
            Self::RevokeToken(intent) => !intent.direct,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CommandIntent {
    StartServer(ServerOptions),
    StopServer,
    RestartServer,
    RunServer(ServerOptions),
    ListServers,
    ShowServer(ServerSelector),
    AddServer(AddServerIntent),
    EditServer(EditServerIntent),
    RemoveServer(String),
    CreateToken(CreateTokenIntent),
    ListTokens {
        server: Option<String>,
        direct: bool,
    },
    RevokeToken(RevokeTokenIntent),
    ListProbes {
        server: Option<String>,
    },
    RunFirmware(RunFirmwareIntent),
    AttachProbe(AttachProbeIntent),
    StartDap {
        target: ResourceSelector,
        port: u16,
    },
    ListSerialPorts {
        server: Option<String>,
    },
    AttachSerial(AttachSerialIntent),
    ListSessions {
        server: Option<String>,
        select: SessionSelect,
    },
    ReplaySession(ReplaySessionIntent),
    ExportSession(ExportSessionIntent),
    Update(UpdateIntent),
    ShowOpenApi,
}
