// SPDX-License-Identifier: AGPL-3.0-or-later
//! Typed CLI errors and exit-code mapping.

use std::fmt;

use reqwest::StatusCode;

#[derive(Debug)]
pub enum CliError {
    Usage(String),
    Config(String),
    Transport {
        server: String,
        message: String,
    },
    Api {
        server: String,
        status: Option<StatusCode>,
        code: Option<String>,
        message: String,
    },
    Cancelled,
    TaskFailed {
        task: String,
        source: Box<CliError>,
    },
    Internal(anyhow::Error),
}

impl CliError {
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(anyhow::anyhow!(message.into()))
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Cancelled => 130,
            Self::TaskFailed { source, .. } => source.exit_code(),
            _ => 1,
        }
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || matches!(self, Self::TaskFailed { source, .. } if source.is_cancelled())
    }

    pub fn with_task(self, task: impl Into<String>) -> Self {
        Self::TaskFailed {
            task: task.into(),
            source: Box::new(self),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage(message) | Self::Config(message) => f.write_str(message),
            Self::Transport { server, message } => {
                write!(f, "Could not reach server '{server}': {message}")
            }
            Self::Api {
                server,
                status,
                message,
                ..
            } => {
                if let Some(status) = status {
                    write!(f, "Server '{server}' returned {status}: {message}")
                } else {
                    write!(f, "Server '{server}' failed: {message}")
                }
            }
            Self::Cancelled => f.write_str("cancelled"),
            Self::TaskFailed { task, source } => write!(f, "{task}: {source}"),
            Self::Internal(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for CliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Internal(error) => Some(error.as_ref()),
            Self::TaskFailed { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<anyhow::Error> for CliError {
    fn from(value: anyhow::Error) -> Self {
        Self::Internal(value)
    }
}

impl From<std::io::Error> for CliError {
    fn from(value: std::io::Error) -> Self {
        Self::Internal(anyhow::Error::new(value))
    }
}

impl From<serde_json::Error> for CliError {
    fn from(value: serde_json::Error) -> Self {
        Self::Internal(anyhow::Error::new(value))
    }
}

impl From<reqwest::Error> for CliError {
    fn from(value: reqwest::Error) -> Self {
        Self::Internal(anyhow::Error::new(value))
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for CliError {
    fn from(value: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::Internal(anyhow::Error::new(value))
    }
}

pub fn map_client_error(
    server: &str,
    error: boot_api_client::Error<boot_api_client::types::Error>,
) -> CliError {
    let status = error.status();
    let message = error.to_string();
    match status {
        Some(StatusCode::UNAUTHORIZED) => CliError::Config(format!(
            "Server '{server}' rejected the token. Run: boot edit-server {server} --token-stdin"
        )),
        Some(StatusCode::FORBIDDEN) => CliError::Config(format!(
            "The token for '{server}' lacks the required scope."
        )),
        Some(StatusCode::CONFLICT) => CliError::Api {
            server: server.to_string(),
            status,
            code: None,
            message,
        },
        _ => CliError::Api {
            server: server.to_string(),
            status,
            code: None,
            message,
        },
    }
}
