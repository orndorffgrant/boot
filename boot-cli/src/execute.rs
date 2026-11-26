// SPDX-License-Identifier: AGPL-3.0-or-later
//! Plan executor: runs CLI tasks and dispatches capability-port actions.

use std::any::Any;
use std::sync::Arc;

use boot_protocol::{Change, Session, SessionAcquire, SessionOptions};
use futures::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::domain::{
    CreateTokenIntent, RevokeTokenIntent, ServerConfig, ServerOptions, SessionKind, SessionSelect,
};
use crate::error::CliError;
use crate::follow::{
    ChangeEvent, ReconnectPolicy, SessionEvent, SessionFollowOptions, follow_change, follow_session,
};
use crate::local::provision;
use crate::plan::{
    Action, ArtifactRef, ChangeKind, CollectRequest, Confirmation, Field, FollowSessionPolicy,
    Level, Outcome, Plan, PlanTask, ResourceHandle, ResourceKind, ServiceAction, SessionHandle,
    Table, TaskId, TokenChange, View,
};
use crate::ports::Ports;
use crate::ui::{TaskFinished, TaskProgress, Ui};

pub struct Executor<'a> {
    ports: &'a Ports,
    ui: Arc<dyn Ui>,
    cancel: CancellationToken,
}

impl<'a> Executor<'a> {
    pub fn new(ports: &'a Ports, ui: Arc<dyn Ui>, cancel: CancellationToken) -> Self {
        Self { ports, ui, cancel }
    }

    pub async fn execute(&self, plan: Plan) -> Result<Outcome, CliError> {
        let mut slots = Slots::default();
        let mut outcome = Outcome::Done;
        for task in plan.tasks {
            if self.cancel.is_cancelled()
                && task.policy.cancel != crate::plan::CancelPolicy::NotCancellable
            {
                return Err(CliError::Cancelled);
            }
            if task.policy.visible == crate::plan::Visibility::User {
                self.ui.task_started(task.id, &task.title, task.progress);
            }
            if let Some(confirmation) = &task.confirmation {
                if !self.ui.confirm(confirmation)? {
                    return Err(CliError::Cancelled);
                }
            }
            match self.execute_task(&task, &mut slots).await {
                Ok(task_outcome) => {
                    if !matches!(task_outcome, Outcome::Done) {
                        outcome = task_outcome;
                    }
                    if task.policy.visible == crate::plan::Visibility::User {
                        self.ui.task_finished(
                            task.id,
                            TaskFinished {
                                success: true,
                                message: None,
                            },
                        );
                    }
                }
                Err(error) => {
                    if task.policy.visible == crate::plan::Visibility::User {
                        self.ui.task_finished(
                            task.id,
                            TaskFinished {
                                success: false,
                                message: Some(error.to_string()),
                            },
                        );
                    }
                    if task.policy.continue_on_error {
                        continue;
                    }
                    return Err(error.with_task(task.title));
                }
            }
        }
        Ok(outcome)
    }

    async fn execute_task(&self, task: &PlanTask, slots: &mut Slots) -> Result<Outcome, CliError> {
        match &task.action {
            Action::LoadServer { selector, out } => {
                let server = self
                    .ports
                    .config
                    .server(selector.as_str())
                    .await?
                    .ok_or_else(|| {
                        CliError::config(format!("Server '{}' not found", selector.as_str()))
                    })?;
                slots.insert(*out, server);
                Ok(Outcome::Done)
            }
            Action::ResolveResource {
                server,
                selector,
                kind,
                out,
            } => {
                let server = slots.get(*server)?.clone();
                let resource = match kind {
                    ResourceKind::Probe => {
                        let probe = self.ports.api.resolve_probe(&server, selector).await?;
                        ResourceHandle {
                            id: probe.id,
                            kind: ResourceKind::Probe,
                        }
                    }
                    ResourceKind::SerialPort => {
                        let port = self
                            .ports
                            .api
                            .resolve_serial_port(&server, selector)
                            .await?;
                        ResourceHandle {
                            id: port.id,
                            kind: ResourceKind::SerialPort,
                        }
                    }
                };
                slots.insert(*out, resource);
                Ok(Outcome::Done)
            }
            Action::Collect(request) => self.collect(request).await,
            Action::InspectServer {
                selector,
                include_service,
            } => {
                self.inspect_server(selector.as_str(), *include_service)
                    .await
            }
            Action::ReadToken { source, out } => {
                let token = read_token(source)?;
                slots.insert(*out, token);
                Ok(Outcome::Done)
            }
            Action::AddServer {
                name,
                url,
                ca_cert,
                token,
            } => {
                if let Some(path) = ca_cert {
                    std::fs::File::open(path).map_err(|error| {
                        CliError::config(format!("Failed to read CA certificate {path}: {error}"))
                    })?;
                }
                let token = (*token)
                    .map(|key| slots.get(key).map(|secret| secret.0.as_str()))
                    .transpose()?;
                self.ports
                    .config
                    .add_server(name, url, ca_cert.as_deref(), token)
                    .await?;
                if token.is_none() {
                    self.ui.warning(&format!(
                        "no token supplied for '{name}'. Protected commands will fail until you run: boot edit-server {name} --token-stdin"
                    ));
                }
                self.server_view(name).await
            }
            Action::UpdateServer {
                name,
                url,
                ca_cert,
                token,
            } => {
                if let Some(url) = url {
                    validate_url(url)?;
                }
                if let Some(Some(path)) = ca_cert {
                    std::fs::File::open(path).map_err(|error| {
                        CliError::config(format!("Failed to read CA certificate {path}: {error}"))
                    })?;
                }
                let token = match token {
                    TokenChange::Keep => None,
                    TokenChange::Clear => Some(None),
                    TokenChange::Set(key) => Some(Some(slots.get(*key)?.0.as_str())),
                };
                self.ports
                    .config
                    .update_server(
                        name,
                        url.as_deref(),
                        ca_cert.as_ref().map(|value| value.as_deref()),
                        token,
                    )
                    .await?;
                self.server_view(name).await
            }
            Action::OpenServerEditor { server, out } => {
                let server = slots.get(*server)?;
                let document = EditableServer {
                    url: server.url.clone(),
                    ca_cert: server.ca_cert.clone(),
                    token: None,
                };
                let mut yaml = serde_yaml::to_string(&document).map_err(|error| {
                    CliError::config(format!("Failed to render server YAML: {error}"))
                })?;
                yaml = yaml.replace(
                    "token: null",
                    "# Set a value to replace the stored token, \"\" to clear it, or null to keep it.\ntoken: null",
                );
                slots.insert(
                    *out,
                    crate::plan::EditedDocument(self.ports.system.edit_document(&yaml)?),
                );
                Ok(Outcome::Done)
            }
            Action::ApplyServerDocument { server, document } => {
                self.apply_server_document(slots.get(*server)?, slots.get(*document)?)
                    .await
            }
            Action::RemoveServer { name } => {
                self.ports.config.remove_server(name).await?;
                Ok(Outcome::View(View::Text(format!(
                    "Removed server '{name}'.\n"
                ))))
            }
            Action::UploadArtifact { server, path, out } => {
                let server = slots.get(*server)?.clone();
                let ui = self.ui.clone();
                let task_id = task.id;
                let label = format!("Uploading {}", path.display());
                let artifact = self
                    .ports
                    .api
                    .upload_artifact(
                        &server,
                        path,
                        Arc::new(move |done, total| {
                            ui.task_progress(
                                task_id,
                                TaskProgress {
                                    done,
                                    total,
                                    unit: crate::plan::Unit::Bytes,
                                    message: Some(label.clone()),
                                },
                            );
                        }),
                    )
                    .await?;
                self.ui.task_message(
                    task.id,
                    Level::Info,
                    &format!(
                        "uploaded {} bytes (sha256 {}...)",
                        artifact.size,
                        artifact.sha256.chars().take(12).collect::<String>()
                    ),
                );
                slots.insert(*out, artifact);
                Ok(Outcome::Done)
            }
            Action::RunChange {
                server,
                resource,
                kind,
            } => {
                self.run_change(task.id, slots, *server, *resource, kind)
                    .await
            }
            Action::LoadSession {
                server,
                session_id,
                out,
            } => {
                let server = slots.get(*server)?.clone();
                let session = self.ports.api.get_session(&server, session_id).await?;
                slots.insert(*out, SessionHandle { server, session });
                Ok(Outcome::Done)
            }
            Action::StartDapBridge {
                server,
                resource,
                transport,
                artifact,
                inherit_artifact,
            } => {
                let server = slots.get(*server)?.clone();
                let resource = slots.get(*resource)?.clone();
                let artifact = artifact_id(slots, artifact)?;
                self.ui
                    .status(&format!("Starting DAP bridge for {}", resource.id));
                crate::dap_bridge::run_bridge(
                    self.ports,
                    self.ui.clone(),
                    self.cancel.clone(),
                    server,
                    resource.id,
                    *transport,
                    artifact,
                    *inherit_artifact,
                )
                .await?;
                Ok(Outcome::Streamed { summary: None })
            }
            Action::AttachSession {
                server,
                resource,
                kind,
                acquire,
                explicit_session,
                artifact,
                baud_rate,
                policy,
            } => {
                self.attach_session(
                    task.id,
                    slots,
                    *server,
                    *resource,
                    *kind,
                    *acquire,
                    explicit_session.as_deref(),
                    artifact,
                    *baud_rate,
                    policy,
                )
                .await
            }
            Action::FollowSession { session, policy } => {
                let handle = slots.get(*session)?.clone();
                self.follow_session_action(task.id, handle, policy).await
            }
            Action::ExportSession {
                server,
                session,
                out_path,
                format,
                include_input,
            } => {
                let server = slots.get(*server)?.clone();
                let handle = slots.get(*session)?;
                self.export_session(
                    task.id,
                    &server,
                    &handle.session,
                    out_path,
                    format,
                    *include_input,
                )
                .await
            }
            Action::CreateToken(intent) => self.create_token(intent).await,
            Action::RevokeToken(intent) => self.revoke_token(intent).await,
            Action::Service {
                action,
                options,
                local,
            } => self.service(*action, options.as_ref(), *local, slots).await,
            Action::PrepareLocalServer { options, out } => {
                let cli_db = self.ports.config.cli_db_path()?;
                let api_db = self.ports.config.api_db_path()?;
                let prepared = provision::prepare(options, Some(&cli_db), Some(&api_db)).await?;
                for warning in prepared.warnings {
                    self.ui.warning(&warning);
                }
                slots.insert(*out, prepared.local);
                Ok(Outcome::Done)
            }
            Action::RunServerForeground { options, local } => {
                let local = slots.get(*local)?.clone();
                provision::run_server(options, &local).await?;
                Ok(Outcome::Done)
            }
            Action::FetchRelease { repo, version, out } => {
                let release = self
                    .ports
                    .updater
                    .fetch_release(repo.as_deref(), version.as_deref())
                    .await?;
                slots.insert(*out, release.clone());
                Ok(Outcome::View(View::Fields(release_fields(&release))))
            }
            Action::InstallRelease {
                release,
                force,
                no_restart,
                yes,
            } => {
                let release = slots.get(*release)?.clone();
                self.ui.render(&View::Fields(release_fields(&release)))?;
                if !*yes {
                    let confirmation = Confirmation {
                        prompt: "Continue with the update?".to_string(),
                        default: false,
                        required: true,
                    };
                    if !self.ui.confirm(&confirmation)? {
                        return Err(CliError::Cancelled);
                    }
                }
                let installed = self
                    .ports
                    .updater
                    .install_release(&release, *force, *no_restart, &|done, total| {
                        self.ui.task_progress(
                            task.id,
                            TaskProgress {
                                done,
                                total,
                                unit: crate::plan::Unit::Bytes,
                                message: Some("Downloading release".to_string()),
                            },
                        );
                    })
                    .await?;
                Ok(Outcome::View(View::Text(format!(
                    "Updated boot to {installed}.\n"
                ))))
            }
            Action::ShowOpenApi => Ok(Outcome::View(View::Text(format!(
                "{}\n",
                boot_api::openapi()
            )))),
        }
    }

    async fn server_view(&self, name: &str) -> Result<Outcome, CliError> {
        let server = self
            .ports
            .config
            .server(name)
            .await?
            .ok_or_else(|| CliError::config(format!("Server '{name}' not found")))?;
        Ok(Outcome::View(View::Fields(server_fields(&server))))
    }

    async fn inspect_server(&self, name: &str, include_service: bool) -> Result<Outcome, CliError> {
        let server = self.ports.config.server(name).await?;
        let mut fields = Vec::new();
        match &server {
            Some(server) => {
                fields.push(Field {
                    label: "Name".to_string(),
                    value: server.name.clone(),
                });
                fields.push(Field {
                    label: "URL".to_string(),
                    value: server.url.clone(),
                });
            }
            None if name == "local" => {
                fields.push(Field {
                    label: "Name".to_string(),
                    value: "local (not configured)".to_string(),
                });
            }
            None => return Err(CliError::config(format!("Server '{name}' not found"))),
        }
        if include_service {
            let status = self
                .ports
                .system
                .service_status()?
                .unwrap_or_else(|| "unknown".to_string());
            let bind = self
                .ports
                .system
                .service_bind()?
                .unwrap_or_else(|| "unknown".to_string());
            fields.push(Field {
                label: "Service".to_string(),
                value: status,
            });
            fields.push(Field {
                label: "Bind".to_string(),
                value: bind,
            });
        }
        if let Some(server) = &server {
            fields.push(Field {
                label: "TLS".to_string(),
                value: if server.tls() { "https" } else { "http" }.to_string(),
            });
            fields.push(Field {
                label: "CA".to_string(),
                value: server.ca_cert.clone().unwrap_or_else(|| "none".to_string()),
            });
            fields.push(Field {
                label: "Token".to_string(),
                value: if server.token.is_some() {
                    "stored".to_string()
                } else {
                    "missing".to_string()
                },
            });
            let status = self.ports.api.auth_status(server).await?;
            fields.push(Field {
                label: "Server".to_string(),
                value: if status.reachable {
                    "reachable".to_string()
                } else {
                    "unreachable".to_string()
                },
            });
            fields.push(Field {
                label: "Read".to_string(),
                value: status.read,
            });
            fields.push(Field {
                label: "Admin".to_string(),
                value: status.admin,
            });
        }
        Ok(Outcome::View(View::Fields(fields)))
    }

    async fn apply_server_document(
        &self,
        server: &ServerConfig,
        document: &crate::plan::EditedDocument,
    ) -> Result<Outcome, CliError> {
        let parsed: EditableServer = serde_yaml::from_str(&document.0)
            .map_err(|error| CliError::config(format!("Invalid server YAML document: {error}")))?;
        validate_url(&parsed.url)?;
        let ca_cert = match parsed.ca_cert.as_deref() {
            None | Some("") => None,
            Some(path) => {
                std::fs::File::open(path).map_err(|error| {
                    CliError::config(format!("Failed to read CA certificate {path}: {error}"))
                })?;
                Some(path.to_string())
            }
        };
        let token = match parsed.token.as_deref() {
            None => None,
            Some("") => Some(None),
            Some(token) => Some(Some(validate_token_text(token)?)),
        };
        let url_changed = parsed.url != server.url;
        let ca_changed = ca_cert.as_deref() != server.ca_cert.as_deref();
        let token_changed = match &token {
            None => false,
            Some(None) => server.token.is_some(),
            Some(Some(token)) => server.token.as_deref() != Some(token.as_str()),
        };
        if !url_changed && !ca_changed && !token_changed {
            return Ok(Outcome::View(View::Text("No changes.\n".to_string())));
        }
        self.ports
            .config
            .update_server(
                &server.name,
                Some(&parsed.url),
                Some(ca_cert.as_deref()),
                token.as_ref().map(|value| value.as_deref()),
            )
            .await?;
        Ok(Outcome::View(View::Text(format!(
            "Updated server '{}'.\n",
            server.name
        ))))
    }

    async fn collect(&self, request: &CollectRequest) -> Result<Outcome, CliError> {
        match request {
            CollectRequest::Servers => {
                let servers = self.ports.config.servers().await?;
                if servers.is_empty() {
                    return Ok(no_servers());
                }
                let rows = servers
                    .iter()
                    .map(|server| {
                        vec![
                            server.name.clone(),
                            server.url.clone(),
                            if server.token.is_some() {
                                "stored"
                            } else {
                                "missing"
                            }
                            .to_string(),
                            if server.tls() { "https" } else { "http" }.to_string(),
                        ]
                    })
                    .collect();
                Ok(Outcome::View(View::Table(Table {
                    headers: vec![
                        "NAME".to_string(),
                        "URL".to_string(),
                        "TOKEN".to_string(),
                        "TLS".to_string(),
                    ],
                    rows,
                })))
            }
            CollectRequest::Probes { server } => {
                let servers = self.servers_for(server.as_deref()).await?;
                if servers.is_empty() {
                    return Ok(no_servers());
                }
                self.collect_probes(&servers).await
            }
            CollectRequest::SerialPorts { server } => {
                let servers = self.servers_for(server.as_deref()).await?;
                if servers.is_empty() {
                    return Ok(no_servers());
                }
                self.collect_serial_ports(&servers).await
            }
            CollectRequest::Sessions { server, select } => {
                let servers = self.servers_for(server.as_deref()).await?;
                if servers.is_empty() {
                    return Ok(no_servers());
                }
                self.collect_sessions(&servers, select).await
            }
            CollectRequest::Tokens { server, direct } => {
                if *direct {
                    self.collect_direct_tokens().await
                } else {
                    let servers = self.servers_for(server.as_deref()).await?;
                    if servers.is_empty() {
                        return Ok(no_servers());
                    }
                    self.collect_api_tokens(&servers).await
                }
            }
        }
    }

    async fn servers_for(&self, scope: Option<&str>) -> Result<Vec<ServerConfig>, CliError> {
        match scope {
            Some(name) => {
                Ok(vec![self.ports.config.server(name).await?.ok_or_else(
                    || CliError::config(format!("Server '{name}' not found")),
                )?])
            }
            None => self.ports.config.servers().await,
        }
    }

    async fn collect_probes(&self, servers: &[ServerConfig]) -> Result<Outcome, CliError> {
        let mut rows = Vec::new();
        let mut failures = Vec::new();
        for server in servers {
            match self.ports.api.list_probes(server).await {
                Ok(probes) => {
                    for probe in probes {
                        let selector = format!(
                            "{:04x}:{:04x}:{}",
                            probe.vendor_id,
                            probe.product_id,
                            probe.serial_number.as_deref().unwrap_or_default()
                        );
                        let firmware = probe
                            .last_artifact
                            .as_ref()
                            .map(|artifact| {
                                format!(
                                    "{}... ({})",
                                    artifact.id.chars().take(12).collect::<String>(),
                                    if artifact.defmt { "defmt" } else { "raw" }
                                )
                            })
                            .unwrap_or_default();
                        rows.push(vec![
                            server.name.clone(),
                            probe.id,
                            selector,
                            probe.name,
                            firmware,
                        ]);
                    }
                }
                Err(error) => failures.push(error),
            }
        }
        let view = View::Table(Table {
            headers: vec![
                "SERVER".to_string(),
                "ID".to_string(),
                "SELECTOR".to_string(),
                "NAME".to_string(),
                "FIRMWARE".to_string(),
            ],
            rows,
        });
        Ok(partial(view, failures))
    }

    async fn collect_serial_ports(&self, servers: &[ServerConfig]) -> Result<Outcome, CliError> {
        let mut rows = Vec::new();
        let mut failures = Vec::new();
        for server in servers {
            match self.ports.api.list_serial_ports(server).await {
                Ok(ports) => {
                    for port in ports {
                        rows.push(vec![
                            server.name.clone(),
                            port.id,
                            port.name.unwrap_or(port.path),
                        ]);
                    }
                }
                Err(error) => failures.push(error),
            }
        }
        let view = View::Table(Table {
            headers: vec!["SERVER".to_string(), "ID".to_string(), "NAME".to_string()],
            rows,
        });
        Ok(partial(view, failures))
    }

    async fn collect_sessions(
        &self,
        servers: &[ServerConfig],
        select: &SessionSelect,
    ) -> Result<Outcome, CliError> {
        let select = match select {
            SessionSelect::Active => "active",
            SessionSelect::Past => "past",
            SessionSelect::All => "all",
        };
        let mut rows = Vec::new();
        let mut failures = Vec::new();
        for server in servers {
            match self.ports.api.list_sessions(server, select).await {
                Ok(sessions) => {
                    for session in sessions {
                        let decoder = session
                            .decode
                            .artifact
                            .as_ref()
                            .map(|artifact| {
                                format!(
                                    "{}:{}...",
                                    session.decode.mode,
                                    artifact.id.chars().take(12).collect::<String>()
                                )
                            })
                            .unwrap_or_else(|| "raw".to_string());
                        rows.push(vec![
                            server.name.clone(),
                            session.id,
                            session.kind,
                            session.state,
                            session.resource_id,
                            session.subscribers.to_string(),
                            session.data.bytes.to_string(),
                            decoder,
                        ]);
                    }
                }
                Err(error) => failures.push(error),
            }
        }
        let view = View::Table(Table {
            headers: vec![
                "SERVER".to_string(),
                "ID".to_string(),
                "KIND".to_string(),
                "STATE".to_string(),
                "RESOURCE".to_string(),
                "SUBS".to_string(),
                "BYTES".to_string(),
                "DECODER".to_string(),
            ],
            rows,
        });
        Ok(partial(view, failures))
    }

    async fn collect_direct_tokens(&self) -> Result<Outcome, CliError> {
        let tokens = self.ports.config.direct_list_tokens().await?;
        let rows = tokens
            .into_iter()
            .map(|token| {
                vec![
                    "direct".to_string(),
                    token.id,
                    token.name,
                    token.scopes.display(),
                    format_timestamp(token.created_at),
                    token
                        .expires_at
                        .map(format_timestamp)
                        .unwrap_or_else(|| "never".to_string()),
                    String::new(),
                ]
            })
            .collect();
        Ok(Outcome::View(View::Table(Table {
            headers: vec![
                "SERVER".to_string(),
                "ID".to_string(),
                "NAME".to_string(),
                "SCOPES".to_string(),
                "CREATED".to_string(),
                "EXPIRES".to_string(),
                "".to_string(),
            ],
            rows,
        })))
    }

    async fn collect_api_tokens(&self, servers: &[ServerConfig]) -> Result<Outcome, CliError> {
        let mut rows = Vec::new();
        let mut failures = Vec::new();
        for server in servers {
            match self.ports.api.list_tokens(server).await {
                Ok(tokens) => {
                    for token in tokens {
                        rows.push(vec![
                            server.name.clone(),
                            token.id,
                            token.name,
                            token.scopes.join(","),
                            format_timestamp(token.created_at),
                            token
                                .expires_at
                                .map(format_timestamp)
                                .unwrap_or_else(|| "never".to_string()),
                            if token.current {
                                "current".to_string()
                            } else {
                                String::new()
                            },
                        ]);
                    }
                }
                Err(error) => failures.push(error),
            }
        }
        let view = View::Table(Table {
            headers: vec![
                "SERVER".to_string(),
                "ID".to_string(),
                "NAME".to_string(),
                "SCOPES".to_string(),
                "CREATED".to_string(),
                "EXPIRES".to_string(),
                "".to_string(),
            ],
            rows,
        });
        Ok(partial(view, failures))
    }

    async fn run_change(
        &self,
        task_id: TaskId,
        slots: &mut Slots,
        server: crate::plan::Key<ServerConfig>,
        resource: crate::plan::Key<ResourceHandle>,
        kind: &ChangeKind,
    ) -> Result<Outcome, CliError> {
        let server = slots.get(server)?.clone();
        let resource = slots.get(resource)?.clone();
        let client = self.ports.api.client(&server)?;
        let idempotency = idempotency_key();
        let created = match kind {
            ChangeKind::Flash { artifact, options } => {
                let artifact =
                    (*artifact).ok_or_else(|| CliError::internal("flash requires an artifact"))?;
                let artifact = slots.get(artifact)?;
                self.ports
                    .api
                    .create_flash(
                        &server,
                        &resource.id,
                        &artifact.id,
                        *options,
                        Some(&idempotency),
                    )
                    .await?
            }
            ChangeKind::Reset => {
                self.ports
                    .api
                    .create_reset(&server, &resource.id, Some(&idempotency))
                    .await?
            }
            ChangeKind::Inspect => {
                self.ports
                    .api
                    .create_inspect(&server, &resource.id, Some(&idempotency))
                    .await?
            }
        };
        let mut observer = |event: ChangeEvent| match event {
            ChangeEvent::Snapshot { change, replayed } => {
                if replayed {
                    self.ui
                        .task_message(task_id, Level::Info, "resumed change history");
                }
                self.ui.task_message(
                    task_id,
                    Level::Info,
                    &format!("{}: {}", change.status, change.summary),
                );
            }
            ChangeEvent::Updated(change) => {
                self.ui.task_message(
                    task_id,
                    Level::Info,
                    &format!("{}: {}", change.status, change.summary),
                );
            }
            ChangeEvent::TaskUpdated(task) => self.ui.child_task(task_id, &task),
            ChangeEvent::Log(entries) => {
                for entry in entries {
                    self.ui.task_message(task_id, Level::Info, &entry.message);
                }
            }
            ChangeEvent::Completed(change) => {
                for entry in &change.log {
                    self.ui.task_message(task_id, Level::Info, &entry.message);
                }
            }
            ChangeEvent::Reconnecting { attempt, delay } => {
                self.ui.task_message(
                    task_id,
                    Level::Warn,
                    &format!(
                        "connection lost; reconnecting (attempt {attempt}) in {:.1}s",
                        delay.as_secs_f32()
                    ),
                );
            }
        };
        let change = match follow_change(
            &server.name,
            &client,
            &created.change,
            None,
            ReconnectPolicy::bounded(5),
            &self.cancel,
            &mut observer,
        )
        .await
        {
            Ok(change) => change,
            Err(CliError::Cancelled) => {
                self.ui.status(&format!(
                    "Detached from change {}; it continues on the server.",
                    created.change
                ));
                return Err(CliError::Cancelled);
            }
            Err(error) => return Err(error),
        };
        if change.status == "done" {
            Ok(Outcome::Done)
        } else {
            Err(CliError::config(change_error(&change)))
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attach_session(
        &self,
        task_id: TaskId,
        slots: &mut Slots,
        server: crate::plan::Key<ServerConfig>,
        resource: Option<crate::plan::Key<ResourceHandle>>,
        kind: SessionKind,
        acquire: crate::domain::SessionAcquire,
        explicit_session: Option<&str>,
        artifact: &ArtifactRef,
        baud_rate: Option<u32>,
        policy: &FollowSessionPolicy,
    ) -> Result<Outcome, CliError> {
        let server = slots.get(server)?.clone();
        let resource = resource.map(|key| slots.get(key).cloned()).transpose()?;
        let artifact_id = artifact_id(slots, artifact)?;
        let (session, created) = if let Some(session_id) = explicit_session {
            let session = self.ports.api.get_session(&server, session_id).await?;
            if session.kind != kind.api_name() {
                return Err(CliError::config(format!(
                    "session {session_id} is a {} session, not {}",
                    session.kind,
                    kind.label()
                )));
            }
            if let Some(resource) = &resource {
                if session.resource_id != resource.id {
                    return Err(CliError::config(format!(
                        "session {session_id} belongs to resource {}, not {}",
                        session.resource_id, resource.id
                    )));
                }
            }
            let session = match artifact {
                ArtifactRef::Uploaded(_) | ArtifactRef::Id(_) => {
                    let id = artifact_id.as_deref().expect("artifact id");
                    self.ports
                        .api
                        .set_session_artifact(&server, &session.id, id)
                        .await?
                }
                ArtifactRef::Clear => {
                    self.ports
                        .api
                        .delete_session_artifact(&server, &session.id)
                        .await?
                }
                ArtifactRef::Inherit | ArtifactRef::None => session,
            };
            (session, false)
        } else {
            let resource =
                resource.ok_or_else(|| CliError::internal("attach requires a resource"))?;
            let kind_matches = matches!(
                (kind, resource.kind),
                (SessionKind::Rtt, ResourceKind::Probe)
                    | (SessionKind::Dap, ResourceKind::Probe)
                    | (SessionKind::Serial, ResourceKind::SerialPort)
            );
            if !kind_matches {
                return Err(CliError::config(format!(
                    "{} session does not match {} resource {}",
                    kind.label(),
                    match resource.kind {
                        ResourceKind::Probe => "probe",
                        ResourceKind::SerialPort => "serial port",
                    },
                    resource.id
                )));
            }
            let acquire = match acquire {
                crate::domain::SessionAcquire::AttachOrCreate => SessionAcquire::AttachOrCreate,
                crate::domain::SessionAcquire::AttachOnly => SessionAcquire::Attach,
                crate::domain::SessionAcquire::CreateOnly => SessionAcquire::Create,
            };
            let result = match kind {
                SessionKind::Rtt => {
                    self.ports
                        .api
                        .ensure_probe_session(
                            &server,
                            &resource.id,
                            "rtt",
                            acquire,
                            SessionOptions::default(),
                            artifact_id.as_deref(),
                            matches!(artifact, ArtifactRef::Inherit | ArtifactRef::None),
                            8,
                            60,
                            5000,
                        )
                        .await?
                }
                SessionKind::Serial => {
                    self.ports
                        .api
                        .ensure_serial_session(
                            &server,
                            &resource.id,
                            acquire,
                            SessionOptions {
                                baud_rate: Some(baud_rate.unwrap_or(115200)),
                                ..SessionOptions::default()
                            },
                            8,
                            60,
                            5000,
                        )
                        .await?
                }
                SessionKind::Dap => {
                    return Err(CliError::config(
                        "DAP sessions must be started with `boot start-dap`",
                    ));
                }
            };
            if !result.created {
                match artifact {
                    ArtifactRef::Uploaded(_) | ArtifactRef::Id(_) => {
                        if let Some(id) = artifact_id.as_deref() {
                            let _ = self
                                .ports
                                .api
                                .set_session_artifact(&server, &result.session.id, id)
                                .await;
                        }
                    }
                    ArtifactRef::Clear => {
                        let _ = self
                            .ports
                            .api
                            .delete_session_artifact(&server, &result.session.id)
                            .await;
                    }
                    ArtifactRef::Inherit | ArtifactRef::None => {}
                }
            }
            (result.session, result.created)
        };
        self.ui.status(&format!(
            "{} {} session {}",
            if created { "Started" } else { "Attached to" },
            kind.label(),
            session.id
        ));
        if let Some(decoder) = decoder_label(&session) {
            self.ui.status(&format!("decoder: {decoder}"));
        }
        self.follow_session_action(task_id, SessionHandle { server, session }, policy)
            .await
    }

    async fn follow_session_action(
        &self,
        _task_id: TaskId,
        handle: SessionHandle,
        policy: &FollowSessionPolicy,
    ) -> Result<Outcome, CliError> {
        if handle.session.kind == "dap" {
            let after = match policy.cursor {
                crate::domain::FollowCursor::Latest => None,
                crate::domain::FollowCursor::Start => Some(0),
                crate::domain::FollowCursor::After(value) => Some(value),
            };
            crate::follow::dap::replay(
                self.ports,
                self.ui.clone(),
                handle.server,
                handle.session,
                after,
            )
            .await?;
            return Ok(Outcome::Streamed { summary: None });
        }
        let client = self.ports.api.client(&handle.server)?;
        let after = match policy.cursor {
            crate::domain::FollowCursor::Latest => None,
            crate::domain::FollowCursor::Start => Some(0),
            crate::domain::FollowCursor::After(value) => Some(value),
        };
        let reconnect = match policy.reconnect {
            crate::plan::ReconnectMode::None => ReconnectPolicy::none(),
            crate::plan::ReconnectMode::Bounded(attempts) => ReconnectPolicy::bounded(attempts),
            crate::plan::ReconnectMode::UntilCancelled => ReconnectPolicy::until_cancelled(),
        };
        let options = SessionFollowOptions {
            after,
            mode: policy.access.api_name(),
            include_input: policy.include_input,
            replay: if policy.strict_replay {
                "strict"
            } else {
                "best-effort"
            },
            policy: reconnect,
            resume_on_suspend: policy.resume_on_suspend,
            strict_replay: policy.strict_replay,
        };
        let mut sink = self.ui.session_sink(match handle.session.kind.as_str() {
            "rtt" => SessionKind::Rtt,
            "serial" => SessionKind::Serial,
            "dap" => SessionKind::Dap,
            _ => SessionKind::Serial,
        });
        let mut input = if policy.pump_input {
            self.ui.input()
        } else {
            None
        };
        self.ui.enter_stream();
        let mut observer = |event: SessionEvent| match event {
            SessionEvent::Hello {
                session,
                subscriber_id,
                seq,
            } => {
                sink.status(&format!(
                    "Attached to {} session {} (subscriber {}, seq {})",
                    session.kind, session.id, subscriber_id, seq
                ));
            }
            SessionEvent::State(session) => match session.state.as_str() {
                "active" => sink.status(&format!("Session {} active", session.id)),
                "suspended" => sink.status(&format!("Session {} suspended", session.id)),
                _ => {}
            },
            SessionEvent::DecoderChanged(session) => {
                if let Some(decoder) = decoder_label(&session) {
                    sink.status(&format!("decoder changed: {decoder}"));
                }
            }
            SessionEvent::Raw { stream, data } => {
                let _ = sink.raw(stream, &data);
            }
            SessionEvent::RttRecords { stream, records } => {
                let _ = sink.rtt_records(stream, &records);
            }
            SessionEvent::RttDecodeError { stream, message } => {
                sink.status(&format!("decoder[{stream}]: {message}"));
            }
            SessionEvent::Input(data) => {
                let _ = sink.input(&data);
            }
            SessionEvent::Gap(gap) => sink.gap(&gap),
            SessionEvent::Error { code, message } => {
                sink.status(&format!("{code}: {message}"));
            }
            SessionEvent::Ended(session) => {
                sink.status(&format!("Session {} ended", session.id));
            }
            SessionEvent::Reconnecting { attempt, delay } => {
                sink.status(&format!(
                    "connection lost; reconnecting (attempt {attempt}) in {:.1}s",
                    delay.as_secs_f32()
                ));
            }
        };
        let outcome = follow_session(
            &handle.server.name,
            &client,
            &handle.session.id,
            &options,
            &self.cancel,
            input.as_mut(),
            &mut observer,
        )
        .await;
        self.ui.exit_stream();
        let outcome = outcome?;
        if outcome.detached {
            self.ui.status(&format!(
                "Detached from session {}; it remains active until its detach timeout.",
                outcome.session.id
            ));
        }
        Ok(Outcome::Streamed { summary: None })
    }

    async fn export_session(
        &self,
        task_id: TaskId,
        server: &ServerConfig,
        session: &Session,
        out_path: &std::path::Path,
        format: &str,
        include_input: bool,
    ) -> Result<Outcome, CliError> {
        let mut stream = self
            .ports
            .api
            .export_session(server, &session.id, format, include_input)
            .await?;
        let temp = out_path.with_extension(format!("tmp-{}", std::process::id()));
        let mut file = tokio::fs::File::create(&temp).await.map_err(|error| {
            CliError::config(format!("Failed to create {}: {error}", temp.display()))
        })?;
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|error| CliError::config(format!("Export failed: {error}")))?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            self.ui.task_progress(
                task_id,
                TaskProgress {
                    done: written,
                    total: None,
                    unit: crate::plan::Unit::Bytes,
                    message: Some("Exporting session".to_string()),
                },
            );
        }
        file.flush().await?;
        drop(file);
        if let Err(error) = tokio::fs::rename(&temp, out_path).await {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(CliError::config(format!(
                "Failed to write {}: {error}",
                out_path.display()
            )));
        }
        Ok(Outcome::View(View::Text(format!(
            "Wrote {}\n",
            out_path.display()
        ))))
    }

    async fn create_token(&self, intent: &CreateTokenIntent) -> Result<Outcome, CliError> {
        let scopes = target_scopes(intent.read_only, &intent.scopes)?;
        if scopes.has(boot_api::auth::Capability::Admin) {
            self.ui.warning(
                "this token has the admin scope and can mint or revoke every other token.",
            );
        }
        let expires_in = intent
            .expires_in
            .as_deref()
            .map(parse_duration)
            .transpose()?;
        let (token, name, id, scopes_label) = if intent.direct {
            let expires_at = match expires_in {
                Some(seconds) => Some(
                    unix_now()
                        .checked_add(seconds)
                        .ok_or_else(|| CliError::config("expiry is out of range"))?,
                ),
                None => None,
            };
            let created = self
                .ports
                .config
                .direct_create_token(boot_api::auth::CreateTokenRequest {
                    name: intent.name.clone(),
                    scopes,
                    expires_at,
                    replace: intent.replace,
                })
                .await?;
            (
                created.token,
                created.record.name,
                created.record.id,
                created.record.scopes.display(),
            )
        } else {
            let server_name = intent
                .server
                .as_deref()
                .ok_or_else(|| CliError::usage("specify a server name or use --direct"))?;
            let server = self
                .ports
                .config
                .server(server_name)
                .await?
                .ok_or_else(|| CliError::config(format!("Server '{server_name}' not found")))?;
            let created = self
                .ports
                .api
                .create_token(
                    &server,
                    boot_api_client::types::TokenCreateRequest {
                        name: intent.name.clone(),
                        scopes: Some(scopes.names()),
                        expires_in_seconds: expires_in,
                        replace: intent.replace,
                    },
                )
                .await?;
            (
                created.token,
                created.name,
                created.id,
                created.scopes.join(","),
            )
        };
        self.ui.print_data(&format!("{token}\n"))?;
        self.ui.status(&format!(
            "Token '{name}' ({id}) created with scopes: {scopes_label}. It will not be shown again."
        ));
        if let Some(save_to) = &intent.save_to {
            if let Err(error) = self.ports.config.set_token(save_to, &token).await {
                self.ui.warning(&format!(
                    "could not store the token for '{save_to}': {error}. The token is valid; run: boot edit-server {save_to} --token-stdin"
                ));
            }
        }
        Ok(Outcome::Done)
    }

    async fn revoke_token(&self, intent: &RevokeTokenIntent) -> Result<Outcome, CliError> {
        if intent.direct && intent.all && intent.target.is_some() {
            return Err(CliError::usage(
                "specify either a token selector or --all, not both",
            ));
        }
        if intent.direct && !intent.all && intent.target.is_none() {
            return Err(CliError::usage("specify a token id/name or --all"));
        }
        if intent.direct {
            if intent.all {
                if !intent.yes {
                    let confirmation = Confirmation {
                        prompt: "Revoke all tokens from the local API database?".to_string(),
                        default: false,
                        required: true,
                    };
                    if !self.ui.confirm(&confirmation)? {
                        return Err(CliError::Cancelled);
                    }
                }
                let revoked = self.ports.config.direct_revoke_all().await?;
                return Ok(Outcome::View(View::Text(format!(
                    "Revoked {revoked} token(s)\n"
                ))));
            }
            let target = intent
                .target
                .as_deref()
                .ok_or_else(|| CliError::usage("specify a token id/name or --all"))?;
            if target.contains('/') {
                return Err(CliError::usage(
                    "with --direct, pass a token id or name without a server prefix",
                ));
            }
            let revoked = self.ports.config.direct_revoke_token(target).await?;
            return Ok(Outcome::View(View::Text(format!(
                "Revoked {revoked} token(s)\n"
            ))));
        }
        if intent.all {
            let server_name = intent
                .target
                .as_deref()
                .ok_or_else(|| CliError::usage("specify a server name with --all"))?;
            if server_name.contains('/') {
                return Err(CliError::usage(
                    "with --all, pass the server name without a token selector",
                ));
            }
            let server = self
                .ports
                .config
                .server(server_name)
                .await?
                .ok_or_else(|| CliError::config(format!("Server '{server_name}' not found")))?;
            if !intent.yes {
                let confirmation = Confirmation {
                    prompt: format!(
                        "Revoke every token on '{}'? This cannot be undone.",
                        server.name
                    ),
                    default: false,
                    required: true,
                };
                if !self.ui.confirm(&confirmation)? {
                    return Err(CliError::Cancelled);
                }
            }
            let revoked = self.ports.api.revoke_all_tokens(&server).await?;
            return Ok(Outcome::View(View::Text(format!(
                "Revoked {revoked} token(s)\n"
            ))));
        }
        let target = intent
            .target
            .as_deref()
            .ok_or_else(|| CliError::usage("specify <server>/<token-id> or use --all"))?;
        let (server_name, selector) = target
            .split_once('/')
            .filter(|(server, token)| !server.is_empty() && !token.is_empty())
            .ok_or_else(|| CliError::usage("expected <server>/<token-id>"))?;
        let server = self
            .ports
            .config
            .server(server_name)
            .await?
            .ok_or_else(|| CliError::config(format!("Server '{server_name}' not found")))?;
        if let Ok(tokens) = self.ports.api.list_tokens(&server).await {
            if tokens
                .iter()
                .any(|token| token.current && (token.id == selector || token.name == selector))
            {
                self.ui.warning(&format!(
                    "'{selector}' is the token used by this command. Revoking it will immediately end this admin session."
                ));
                if !intent.yes {
                    let confirmation = Confirmation {
                        prompt:
                            "The token you are revoking is the one used by this command. Continue?"
                                .to_string(),
                        default: false,
                        required: true,
                    };
                    if !self.ui.confirm(&confirmation)? {
                        return Err(CliError::Cancelled);
                    }
                }
            }
        }
        let revoked = self.ports.api.revoke_token(&server, selector).await?;
        Ok(Outcome::View(View::Text(format!(
            "Revoked {revoked} token(s)\n"
        ))))
    }

    async fn service(
        &self,
        action: ServiceAction,
        options: Option<&ServerOptions>,
        local: Option<crate::plan::Key<crate::plan::LocalServer>>,
        slots: &Slots,
    ) -> Result<Outcome, CliError> {
        match action {
            ServiceAction::Start => {
                let options =
                    options.ok_or_else(|| CliError::internal("start requires options"))?;
                let local = local
                    .map(|key| slots.get(key))
                    .transpose()?
                    .ok_or_else(|| CliError::internal("start requires local paths"))?;
                self.ports
                    .system
                    .start_service(&provision::service_config(options, local))?;
                Ok(Outcome::View(View::Text("Server started.\n".to_string())))
            }
            ServiceAction::Stop => {
                self.ports.system.stop_service()?;
                Ok(Outcome::View(View::Text("Server stopped.\n".to_string())))
            }
            ServiceAction::Restart => {
                self.ports.system.restart_service()?;
                Ok(Outcome::View(View::Text("Server restarted.\n".to_string())))
            }
        }
    }
}

#[derive(Default)]
struct Slots {
    values: Vec<Option<Box<dyn Any + Send + Sync>>>,
}

impl Slots {
    fn insert<T: Any + Send + Sync>(&mut self, key: crate::plan::Key<T>, value: T) {
        let index = key.slot();
        if self.values.len() <= index {
            self.values.resize_with(index + 1, || None);
        }
        self.values[index] = Some(Box::new(value));
    }

    fn get<T: Any + Send + Sync>(&self, key: crate::plan::Key<T>) -> Result<&T, CliError> {
        self.values
            .get(key.slot())
            .and_then(|value| value.as_ref())
            .and_then(|value| value.downcast_ref::<T>())
            .ok_or_else(|| CliError::internal("plan value was not produced before use"))
    }
}

fn no_servers() -> Outcome {
    Outcome::View(View::Text("No servers configured.\n".to_string()))
}

fn release_fields(release: &crate::plan::ReleaseInfo) -> Vec<Field> {
    vec![
        Field {
            label: "Installed".to_string(),
            value: release.current.clone(),
        },
        Field {
            label: "Available".to_string(),
            value: release.available.clone(),
        },
        Field {
            label: "Release".to_string(),
            value: release.html_url.clone(),
        },
    ]
}

fn server_fields(server: &ServerConfig) -> Vec<Field> {
    vec![
        Field {
            label: "Name".to_string(),
            value: server.name.clone(),
        },
        Field {
            label: "URL".to_string(),
            value: server.url.clone(),
        },
        Field {
            label: "TOKEN".to_string(),
            value: if server.token.is_some() {
                "stored".to_string()
            } else {
                "missing".to_string()
            },
        },
        Field {
            label: "TLS".to_string(),
            value: if server.tls() { "https" } else { "http" }.to_string(),
        },
    ]
}

fn partial(view: View, failures: Vec<CliError>) -> Outcome {
    if failures.is_empty() {
        Outcome::View(view)
    } else {
        Outcome::Partial { view, failures }
    }
}

fn artifact_id(slots: &Slots, artifact: &ArtifactRef) -> Result<Option<String>, CliError> {
    match artifact {
        ArtifactRef::Uploaded(key) => Ok(Some(slots.get(*key)?.id.clone())),
        ArtifactRef::Id(id) => Ok(Some(id.clone())),
        ArtifactRef::Clear | ArtifactRef::Inherit | ArtifactRef::None => Ok(None),
    }
}

fn decoder_label(session: &Session) -> Option<String> {
    session.decode.artifact.as_ref().map(|artifact| {
        format!(
            "{} ({}...)",
            session.decode.mode,
            artifact.id.chars().take(12).collect::<String>()
        )
    })
}

fn change_error(change: &Change) -> String {
    change
        .error
        .as_ref()
        .map(|error| format!("{}: {}", error.code, error.message))
        .unwrap_or_else(|| format!("change {} ended with status {}", change.id, change.status))
}

fn read_token(source: &crate::domain::TokenSource) -> Result<crate::plan::TokenSecret, CliError> {
    let token = if let Some(value) = &source.value {
        validate_token_text(value)?
    } else if let Some(path) = &source.file {
        std::fs::read_to_string(path).map_err(|error| {
            CliError::config(format!(
                "Failed to read token file {}: {error}",
                path.display()
            ))
        })?
    } else if source.stdin {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut value = String::new();
        stdin.read_to_string(&mut value).map_err(|error| {
            CliError::config(format!("Failed to read token from standard input: {error}"))
        })?;
        value
    } else {
        return Err(CliError::usage("no token source supplied"));
    };
    Ok(crate::plan::TokenSecret(validate_token_text(&token)?))
}

fn validate_token_text(token: impl Into<String>) -> Result<String, CliError> {
    let mut token = token.into();
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

fn validate_url(url: &str) -> Result<(), CliError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(CliError::config(
            "Server URL must start with http:// or https://",
        ));
    }
    Ok(())
}

fn target_scopes(read_only: bool, names: &[String]) -> Result<boot_api::auth::Scopes, CliError> {
    if read_only && !names.is_empty() {
        return Err(CliError::usage(
            "--read-only cannot be combined with --scope",
        ));
    }
    if read_only {
        return Ok(boot_api::auth::Scopes::read());
    }
    boot_api::auth::Scopes::from_names(names.iter().map(String::as_str))
        .map_err(|message| CliError::usage(message))
}

fn parse_duration(value: &str) -> Result<i64, CliError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(CliError::usage("expiry duration must not be empty"));
    }
    let (digits, unit) = value.split_at(
        value
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    if digits.is_empty() {
        return Err(CliError::usage(format!(
            "invalid expiry duration '{value}'"
        )));
    }
    let amount: i64 = digits
        .parse()
        .map_err(|_| CliError::usage(format!("invalid expiry duration '{value}'")))?;
    let multiplier = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => return Err(CliError::usage(format!("unknown expiry unit '{unit}'"))),
    };
    amount
        .checked_mul(multiplier)
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| CliError::usage("expiry must be positive"))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn format_timestamp(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|value| {
            value
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .map(|value| value.replace('T', " ").trim_end_matches('Z').to_string())
        .unwrap_or_else(|| unix.to_string())
}

fn idempotency_key() -> String {
    format!(
        "cli-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    )
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EditableServer {
    url: String,
    #[serde(default)]
    ca_cert: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanBuilder;

    #[test]
    fn slots_are_typed_and_use_before_produce_is_an_error() {
        let mut builder = PlanBuilder::new("test");
        let number = builder.slot::<u32>();
        let text = builder.slot::<String>();
        let mut slots = Slots::default();
        assert!(slots.get(number).is_err());
        slots.insert(number, 7u32);
        assert_eq!(*slots.get(number).unwrap(), 7);
        assert!(slots.get(text).is_err());
    }

    #[test]
    fn token_validation_rejects_whitespace_and_empty_values() {
        assert_eq!(validate_token_text("abc\n").unwrap(), "abc");
        assert!(validate_token_text("").is_err());
        assert!(validate_token_text("a b").is_err());
    }

    #[test]
    fn expiry_durations_are_parsed() {
        assert_eq!(parse_duration("30d").unwrap(), 30 * 24 * 60 * 60);
        assert_eq!(parse_duration("12h").unwrap(), 12 * 60 * 60);
        assert!(parse_duration("0d").is_err());
        assert!(parse_duration("1y").is_err());
    }

    #[test]
    fn no_servers_view_is_plain_text() {
        match no_servers() {
            Outcome::View(View::Text(text)) => assert!(text.contains("No servers")),
            _ => panic!("expected a text view"),
        }
    }
}
