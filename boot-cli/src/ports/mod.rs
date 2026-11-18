// SPDX-License-Identifier: AGPL-3.0-or-later
//! Capability ports and their real API/local/OS adapters.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use boot_protocol::{
    ChangeCreated, FlashOptions, FlashRequest, Probe, Session, SessionAcquireResult, SessionOptions,
};
use futures::TryStreamExt;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use tokio_util::io::ReaderStream;

use crate::domain::ServerConfig;
use crate::error::{CliError, map_client_error};
use crate::local::cli_db::{CliDb, ServerEntry};
use crate::plan::{ArtifactHandle, ReleaseInfo};
use crate::protocol::convert;

impl From<ServerEntry> for ServerConfig {
    fn from(entry: ServerEntry) -> Self {
        Self {
            name: entry.name,
            url: entry.url,
            ca_cert: entry.ca_cert,
            token: entry.token,
        }
    }
}

pub struct Ports {
    pub config: Arc<dyn Config>,
    pub api: Arc<dyn Api>,
    pub system: Arc<dyn System>,
    pub updater: Arc<dyn Updater>,
}

impl Ports {
    pub fn new(cli_db: Option<PathBuf>, api_db: Option<PathBuf>) -> Self {
        Self {
            config: Arc::new(RealConfig { cli_db, api_db }),
            api: Arc::new(RealApi),
            system: Arc::new(RealSystem),
            updater: Arc::new(RealUpdater),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuthStatus {
    pub reachable: bool,
    pub read: String,
    pub admin: String,
}

#[async_trait]
pub trait Config: Send + Sync {
    async fn servers(&self) -> Result<Vec<ServerConfig>, CliError>;
    async fn server(&self, name: &str) -> Result<Option<ServerConfig>, CliError>;
    async fn add_server(
        &self,
        name: &str,
        url: &str,
        ca_cert: Option<&str>,
        token: Option<&str>,
    ) -> Result<(), CliError>;
    async fn update_server(
        &self,
        name: &str,
        url: Option<&str>,
        ca_cert: Option<Option<&str>>,
        token: Option<Option<&str>>,
    ) -> Result<(), CliError>;
    async fn remove_server(&self, name: &str) -> Result<(), CliError>;
    async fn set_token(&self, name: &str, token: &str) -> Result<(), CliError>;
    async fn direct_create_token(
        &self,
        request: boot_api::auth::CreateTokenRequest,
    ) -> Result<boot_api::auth::CreatedToken, CliError>;
    async fn direct_list_tokens(&self) -> Result<Vec<boot_api::auth::TokenRecord>, CliError>;
    async fn direct_revoke_token(&self, selector: &str) -> Result<u64, CliError>;
    async fn direct_revoke_all(&self) -> Result<u64, CliError>;
    fn cli_db_path(&self) -> Result<PathBuf, CliError>;
    fn api_db_path(&self) -> Result<PathBuf, CliError>;
}

#[async_trait]
pub trait Api: Send + Sync {
    fn client(&self, server: &ServerConfig) -> Result<boot_api_client::Client, CliError>;

    async fn list_probes(&self, server: &ServerConfig) -> Result<Vec<Probe>, CliError>;
    async fn resolve_probe(&self, server: &ServerConfig, selector: &str)
    -> Result<Probe, CliError>;
    async fn list_serial_ports(
        &self,
        server: &ServerConfig,
    ) -> Result<Vec<boot_protocol::SerialPort>, CliError>;
    async fn resolve_serial_port(
        &self,
        server: &ServerConfig,
        selector: &str,
    ) -> Result<boot_protocol::SerialPort, CliError>;
    async fn list_sessions(
        &self,
        server: &ServerConfig,
        select: &str,
    ) -> Result<Vec<Session>, CliError>;
    async fn get_session(&self, server: &ServerConfig, id: &str) -> Result<Session, CliError>;
    async fn upload_artifact(
        &self,
        server: &ServerConfig,
        path: &Path,
        progress: Arc<dyn Fn(u64, Option<u64>) + Send + Sync>,
    ) -> Result<ArtifactHandle, CliError>;
    async fn create_flash(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        artifact_id: &str,
        options: FlashOptions,
        idempotency_key: Option<&str>,
    ) -> Result<ChangeCreated, CliError>;
    async fn create_reset(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<ChangeCreated, CliError>;
    async fn create_inspect(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<ChangeCreated, CliError>;
    async fn ensure_probe_session(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        kind: &str,
        acquire: boot_protocol::SessionAcquire,
        options: SessionOptions,
        artifact: Option<&str>,
        inherit_artifact: bool,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<SessionAcquireResult, CliError>;
    async fn ensure_serial_session(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        acquire: boot_protocol::SessionAcquire,
        options: SessionOptions,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<SessionAcquireResult, CliError>;
    async fn set_session_artifact(
        &self,
        server: &ServerConfig,
        session_id: &str,
        artifact: &str,
    ) -> Result<Session, CliError>;
    async fn delete_session_artifact(
        &self,
        server: &ServerConfig,
        session_id: &str,
    ) -> Result<Session, CliError>;
    async fn export_session(
        &self,
        server: &ServerConfig,
        session_id: &str,
        format: &str,
        include_input: bool,
    ) -> Result<boot_api_client::ByteStream, CliError>;
    async fn list_tokens(
        &self,
        server: &ServerConfig,
    ) -> Result<Vec<boot_api_client::types::TokenSummary>, CliError>;
    async fn create_token(
        &self,
        server: &ServerConfig,
        request: boot_api_client::types::TokenCreateRequest,
    ) -> Result<boot_api_client::types::TokenCreated, CliError>;
    async fn revoke_token(&self, server: &ServerConfig, selector: &str) -> Result<u64, CliError>;
    async fn revoke_all_tokens(&self, server: &ServerConfig) -> Result<u64, CliError>;
    async fn auth_status(&self, server: &ServerConfig) -> Result<AuthStatus, CliError>;
}

#[async_trait]
pub trait System: Send + Sync {
    fn start_service(&self, config: &crate::local::systemd::ServiceConfig) -> Result<(), CliError>;
    fn stop_service(&self) -> Result<(), CliError>;
    fn restart_service(&self) -> Result<(), CliError>;
    fn service_status(&self) -> Result<Option<String>, CliError>;
    fn service_bind(&self) -> Result<Option<String>, CliError>;
    fn edit_document(&self, initial: &str) -> Result<String, CliError>;
}

#[async_trait]
pub trait Updater: Send + Sync {
    async fn fetch_release(
        &self,
        repo: Option<&str>,
        version: Option<&str>,
    ) -> Result<ReleaseInfo, CliError>;
    async fn install_release(
        &self,
        release: &ReleaseInfo,
        force: bool,
        no_restart: bool,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> Result<String, CliError>;
}

struct RealConfig {
    cli_db: Option<PathBuf>,
    api_db: Option<PathBuf>,
}

impl RealConfig {
    async fn open(&self) -> Result<CliDb, CliError> {
        Ok(CliDb::open_default(self.cli_db.as_deref()).await?)
    }
}

#[async_trait]
impl Config for RealConfig {
    async fn servers(&self) -> Result<Vec<ServerConfig>, CliError> {
        Ok(self
            .open()
            .await?
            .list_servers()
            .await?
            .into_iter()
            .map(ServerConfig::from)
            .collect())
    }

    async fn server(&self, name: &str) -> Result<Option<ServerConfig>, CliError> {
        Ok(self
            .open()
            .await?
            .get_server(name)
            .await?
            .map(ServerConfig::from))
    }

    async fn add_server(
        &self,
        name: &str,
        url: &str,
        ca_cert: Option<&str>,
        token: Option<&str>,
    ) -> Result<(), CliError> {
        self.open()
            .await?
            .add_server(name, url, ca_cert, token)
            .await?;
        Ok(())
    }

    async fn update_server(
        &self,
        name: &str,
        url: Option<&str>,
        ca_cert: Option<Option<&str>>,
        token: Option<Option<&str>>,
    ) -> Result<(), CliError> {
        self.open()
            .await?
            .update_server(name, url, ca_cert, token)
            .await?;
        Ok(())
    }

    async fn remove_server(&self, name: &str) -> Result<(), CliError> {
        self.open().await?.remove_server(name).await?;
        Ok(())
    }

    async fn set_token(&self, name: &str, token: &str) -> Result<(), CliError> {
        self.open().await?.set_token(name, token).await?;
        Ok(())
    }

    async fn direct_create_token(
        &self,
        request: boot_api::auth::CreateTokenRequest,
    ) -> Result<boot_api::auth::CreatedToken, CliError> {
        let path = self.api_db_path()?;
        crate::local::direct_admin::create_token(&path, request).await
    }

    async fn direct_list_tokens(&self) -> Result<Vec<boot_api::auth::TokenRecord>, CliError> {
        let path = self.api_db_path()?;
        crate::local::direct_admin::list_tokens(&path).await
    }

    async fn direct_revoke_token(&self, selector: &str) -> Result<u64, CliError> {
        let path = self.api_db_path()?;
        crate::local::direct_admin::revoke_token(&path, selector).await
    }

    async fn direct_revoke_all(&self) -> Result<u64, CliError> {
        let path = self.api_db_path()?;
        crate::local::direct_admin::revoke_all(&path).await
    }

    fn cli_db_path(&self) -> Result<PathBuf, CliError> {
        Ok(crate::local::cli_db::resolve_cli_db_path(
            self.cli_db.as_deref(),
        )?)
    }

    fn api_db_path(&self) -> Result<PathBuf, CliError> {
        Ok(boot_api::store::resolve_api_db_path(
            self.api_db.as_deref(),
        )?)
    }
}

struct RealApi;

impl RealApi {
    fn http_client(server: &ServerConfig) -> Result<reqwest::Client, CliError> {
        let token = lookup_token(server)?;
        build_http_client(server, Some(&token), None)
    }
}

#[async_trait]
impl Api for RealApi {
    fn client(&self, server: &ServerConfig) -> Result<boot_api_client::Client, CliError> {
        let token = lookup_token(server)?;
        let client = build_http_client(server, Some(&token), None)?;
        Ok(boot_api_client::Client::new_with_client(
            &server.url,
            client,
        ))
    }

    async fn list_probes(&self, server: &ServerConfig) -> Result<Vec<Probe>, CliError> {
        let probes = self
            .client(server)?
            .probes()
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(probes)
    }

    async fn resolve_probe(
        &self,
        server: &ServerConfig,
        selector: &str,
    ) -> Result<Probe, CliError> {
        let probe = self
            .client(server)?
            .resolve_probe(selector)
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(probe)
    }

    async fn list_serial_ports(
        &self,
        server: &ServerConfig,
    ) -> Result<Vec<boot_protocol::SerialPort>, CliError> {
        let ports = self
            .client(server)?
            .serial_ports()
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(ports)
    }

    async fn resolve_serial_port(
        &self,
        server: &ServerConfig,
        selector: &str,
    ) -> Result<boot_protocol::SerialPort, CliError> {
        let port = self
            .client(server)?
            .resolve_serial_port(selector)
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(port)
    }

    async fn list_sessions(
        &self,
        server: &ServerConfig,
        select: &str,
    ) -> Result<Vec<Session>, CliError> {
        let sessions = self
            .client(server)?
            .list_sessions(Some(select))
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(sessions)
    }

    async fn get_session(&self, server: &ServerConfig, id: &str) -> Result<Session, CliError> {
        let session = self
            .client(server)?
            .get_session(id)
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(session)
    }

    async fn upload_artifact(
        &self,
        server: &ServerConfig,
        path: &Path,
        progress: Arc<dyn Fn(u64, Option<u64>) + Send + Sync>,
    ) -> Result<ArtifactHandle, CliError> {
        let file = tokio::fs::File::open(path).await.map_err(|error| {
            CliError::config(format!("Failed to read '{}': {error}", path.display()))
        })?;
        let total = file.metadata().await.ok().map(|metadata| metadata.len());
        progress(0, total);
        let progress_for_stream = progress.clone();
        let stream = ReaderStream::new(file).inspect_ok(move |chunk| {
            progress_for_stream(chunk.len() as u64, None);
        });
        let artifact = self
            .client(server)?
            .create_artifact(reqwest::Body::wrap_stream(stream))
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        Ok(ArtifactHandle {
            id: artifact.id,
            size: artifact.size,
            sha256: artifact.sha256,
        })
    }

    async fn create_flash(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        artifact_id: &str,
        options: FlashOptions,
        idempotency_key: Option<&str>,
    ) -> Result<ChangeCreated, CliError> {
        create_change(
            server,
            &format!("/v1/probes/{resource_id}/flash"),
            &FlashRequest {
                artifact: artifact_id.to_string(),
                options,
            },
            idempotency_key,
        )
        .await
    }

    async fn create_reset(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<ChangeCreated, CliError> {
        create_change(
            server,
            &format!("/v1/probes/{resource_id}/reset"),
            &boot_protocol::ResetRequest::default(),
            idempotency_key,
        )
        .await
    }

    async fn create_inspect(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        idempotency_key: Option<&str>,
    ) -> Result<ChangeCreated, CliError> {
        create_change(
            server,
            &format!("/v1/probes/{resource_id}/inspect"),
            &boot_protocol::InspectRequest::default(),
            idempotency_key,
        )
        .await
    }

    async fn ensure_probe_session(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        kind: &str,
        acquire: boot_protocol::SessionAcquire,
        options: SessionOptions,
        artifact: Option<&str>,
        inherit_artifact: bool,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<SessionAcquireResult, CliError> {
        let request = boot_api_client::types::ProbeSessionRequest {
            kind: kind.to_string(),
            acquire: match acquire {
                boot_protocol::SessionAcquire::AttachOrCreate => {
                    boot_api_client::types::SessionAcquire::AttachOrCreate
                }
                boot_protocol::SessionAcquire::Attach => {
                    boot_api_client::types::SessionAcquire::Attach
                }
                boot_protocol::SessionAcquire::Create => {
                    boot_api_client::types::SessionAcquire::Create
                }
            },
            options: boot_api_client::types::SessionOptions {
                rtt_addr: options.rtt_addr,
                channels: options.channels,
                baud_rate: options.baud_rate,
            },
            artifact: artifact.map(str::to_string),
            inherit_artifact,
            max_subscribers,
            detach_timeout_seconds,
            wait_ms,
        };
        let result = self
            .client(server)?
            .create_probe_session(resource_id, &request)
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(result)
    }

    async fn ensure_serial_session(
        &self,
        server: &ServerConfig,
        resource_id: &str,
        acquire: boot_protocol::SessionAcquire,
        options: SessionOptions,
        max_subscribers: u32,
        detach_timeout_seconds: u64,
        wait_ms: u64,
    ) -> Result<SessionAcquireResult, CliError> {
        let request = boot_api_client::types::SerialSessionRequest {
            acquire: match acquire {
                boot_protocol::SessionAcquire::AttachOrCreate => {
                    boot_api_client::types::SessionAcquire::AttachOrCreate
                }
                boot_protocol::SessionAcquire::Attach => {
                    boot_api_client::types::SessionAcquire::Attach
                }
                boot_protocol::SessionAcquire::Create => {
                    boot_api_client::types::SessionAcquire::Create
                }
            },
            options: boot_api_client::types::SessionOptions {
                rtt_addr: options.rtt_addr,
                channels: options.channels,
                baud_rate: options.baud_rate,
            },
            max_subscribers,
            detach_timeout_seconds,
            wait_ms,
        };
        let result = self
            .client(server)?
            .create_serial_session(resource_id, &request)
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(result)
    }

    async fn set_session_artifact(
        &self,
        server: &ServerConfig,
        session_id: &str,
        artifact: &str,
    ) -> Result<Session, CliError> {
        let session = self
            .client(server)?
            .set_session_artifact(
                session_id,
                &boot_api_client::types::SetArtifactRequest {
                    artifact: artifact.to_string(),
                },
            )
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(session)
    }

    async fn delete_session_artifact(
        &self,
        server: &ServerConfig,
        session_id: &str,
    ) -> Result<Session, CliError> {
        let session = self
            .client(server)?
            .delete_session_artifact(session_id)
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner();
        convert(session)
    }

    async fn export_session(
        &self,
        server: &ServerConfig,
        session_id: &str,
        format: &str,
        include_input: bool,
    ) -> Result<boot_api_client::ByteStream, CliError> {
        Ok(self
            .client(server)?
            .export_session(session_id, None, Some(format), Some(include_input))
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner())
    }

    async fn list_tokens(
        &self,
        server: &ServerConfig,
    ) -> Result<Vec<boot_api_client::types::TokenSummary>, CliError> {
        Ok(self
            .client(server)?
            .list_tokens()
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner())
    }

    async fn create_token(
        &self,
        server: &ServerConfig,
        request: boot_api_client::types::TokenCreateRequest,
    ) -> Result<boot_api_client::types::TokenCreated, CliError> {
        Ok(self
            .client(server)?
            .create_token(&request)
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner())
    }

    async fn revoke_token(&self, server: &ServerConfig, selector: &str) -> Result<u64, CliError> {
        Ok(self
            .client(server)?
            .revoke_token(selector)
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner()
            .revoked as u64)
    }

    async fn revoke_all_tokens(&self, server: &ServerConfig) -> Result<u64, CliError> {
        Ok(self
            .client(server)?
            .revoke_all_tokens(Some(true))
            .await
            .map_err(|error| map_client_error(&server.name, error))?
            .into_inner()
            .revoked as u64)
    }

    async fn auth_status(&self, server: &ServerConfig) -> Result<AuthStatus, CliError> {
        let base = server.url.trim_end_matches('/');
        let health = build_http_client(server, None, Some(Duration::from_secs(5)))?;
        match health.get(format!("{base}/v1/health")).send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                return Ok(AuthStatus {
                    reachable: false,
                    read: format!("health returned {}", response.status()),
                    admin: "unknown".to_string(),
                });
            }
            Err(error) => {
                return Ok(AuthStatus {
                    reachable: false,
                    read: format!("request failed ({error})"),
                    admin: "unknown".to_string(),
                });
            }
        }
        let Some(token) = lookup_token_optional(server)? else {
            return Ok(AuthStatus {
                reachable: true,
                read: "missing token".to_string(),
                admin: "missing token".to_string(),
            });
        };
        let client = build_http_client(server, Some(&token), Some(Duration::from_secs(5)))?;
        let read = match client.get(format!("{base}/v1/probes")).send().await {
            Ok(response) if response.status().is_success() => "ok".to_string(),
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                "invalid token".to_string()
            }
            Ok(response) if response.status() == reqwest::StatusCode::FORBIDDEN => {
                "token lacks read scope".to_string()
            }
            Ok(response) => format!("unexpected status {}", response.status()),
            Err(error) => format!("request failed ({error})"),
        };
        let admin = match client.get(format!("{base}/v1/tokens")).send().await {
            Ok(response) if response.status().is_success() => "ok".to_string(),
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                "invalid token".to_string()
            }
            Ok(response) if response.status() == reqwest::StatusCode::FORBIDDEN => {
                "token is not an admin token".to_string()
            }
            Ok(response) => format!("unexpected status {}", response.status()),
            Err(error) => format!("request failed ({error})"),
        };
        Ok(AuthStatus {
            reachable: true,
            read,
            admin,
        })
    }
}

struct RealSystem;

#[async_trait]
impl System for RealSystem {
    fn start_service(&self, config: &crate::local::systemd::ServiceConfig) -> Result<(), CliError> {
        crate::local::systemd::start_service(config)?;
        Ok(())
    }

    fn stop_service(&self) -> Result<(), CliError> {
        crate::local::systemd::stop_service()?;
        Ok(())
    }

    fn restart_service(&self) -> Result<(), CliError> {
        crate::local::systemd::restart_service()?;
        Ok(())
    }

    fn service_status(&self) -> Result<Option<String>, CliError> {
        Ok(crate::local::systemd::service_status()?)
    }

    fn service_bind(&self) -> Result<Option<String>, CliError> {
        Ok(crate::local::systemd::service_bind()?)
    }

    fn edit_document(&self, initial: &str) -> Result<String, CliError> {
        crate::local::systemd::edit_document(initial)
    }
}

struct RealUpdater;

#[async_trait]
impl Updater for RealUpdater {
    async fn fetch_release(
        &self,
        repo: Option<&str>,
        version: Option<&str>,
    ) -> Result<ReleaseInfo, CliError> {
        crate::local::updater::fetch_release(repo, version).await
    }

    async fn install_release(
        &self,
        release: &ReleaseInfo,
        force: bool,
        no_restart: bool,
        progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
    ) -> Result<String, CliError> {
        crate::local::updater::install_release(release, force, no_restart, progress).await
    }
}

async fn create_change(
    server: &ServerConfig,
    path: &str,
    body: &impl serde::Serialize,
    idempotency_key: Option<&str>,
) -> Result<ChangeCreated, CliError> {
    let client = RealApi::http_client(server)?;
    let url = format!("{}{}", server.url.trim_end_matches('/'), path);
    let mut request = client.post(url).json(body);
    if let Some(key) = idempotency_key {
        request = request.header("Idempotency-Key", key);
    }
    let response = request.send().await.map_err(|error| CliError::Transport {
        server: server.name.clone(),
        message: error.to_string(),
    })?;
    let status = response.status();
    if status.is_success() {
        return response.json().await.map_err(CliError::from);
    }
    let message = response.text().await.unwrap_or_else(|_| status.to_string());
    Err(CliError::Api {
        server: server.name.clone(),
        status: Some(status),
        code: None,
        message,
    })
}

fn lookup_token(server: &ServerConfig) -> Result<String, CliError> {
    lookup_token_optional(server)?.ok_or_else(|| {
        CliError::config(format!(
            "Server '{}' has no token. Run: boot edit-server {} --token-stdin",
            server.name, server.name
        ))
    })
}

fn lookup_token_optional(server: &ServerConfig) -> Result<Option<String>, CliError> {
    if let Ok(token) = std::env::var("BOOT_TOKEN") {
        if !token.is_empty() {
            return Ok(Some(validate_token(token)?));
        }
    }
    if let Ok(path) = std::env::var("BOOT_TOKEN_FILE") {
        if !path.is_empty() {
            let value = std::fs::read_to_string(path)?;
            return Ok(Some(validate_token(value)?));
        }
    }
    Ok(server.token.clone())
}

fn validate_token(mut token: String) -> Result<String, CliError> {
    while token.ends_with('\n') || token.ends_with('\r') {
        token.pop();
    }
    if token.trim().is_empty() {
        return Err(CliError::config("token must not be empty"));
    }
    if token.chars().any(char::is_whitespace) {
        return Err(CliError::config("token must not contain whitespace"));
    }
    Ok(token)
}

fn build_http_client(
    server: &ServerConfig,
    token: Option<&str>,
    timeout: Option<Duration>,
) -> Result<reqwest::Client, CliError> {
    let mut headers = HeaderMap::new();
    if let Some(token) = token {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| CliError::config("token contains invalid HTTP header characters"))?;
        headers.insert(AUTHORIZATION, value);
    }
    let mut builder = reqwest::ClientBuilder::new()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(ca) = &server.ca_cert {
        let pem = std::fs::read(ca).map_err(|error| {
            CliError::config(format!("Failed to read CA certificate {ca}: {error}"))
        })?;
        let certificate = reqwest::Certificate::from_pem(&pem).map_err(|error| {
            CliError::config(format!("Failed to parse CA certificate {ca}: {error}"))
        })?;
        builder = builder.add_root_certificate(certificate);
    }
    builder
        .build()
        .map_err(|error| CliError::config(format!("Failed to build HTTP client: {error}")))
}
