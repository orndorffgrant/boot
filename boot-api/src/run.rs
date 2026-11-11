use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use boot_protocol::*;
use dropshot::{
    ConfigDropshot, ConfigLogging, ConfigLoggingLevel, ConfigTls, HttpError, HttpResponseAccepted,
    HttpResponseCreated, HttpResponseHeaders, HttpResponseOk, Path, Query, RequestContext,
    ServerBuilder, StreamingBody, TypedBody, WebsocketEndpointResult, WebsocketUpgrade,
};
use futures::{SinkExt, StreamExt};
use http::header::{CACHE_CONTROL, LOCATION, PRAGMA};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::auth::{
    Admin, Auth, AuthState, Capability, Control, CreateTokenRequest, Read, Scopes, TokenStore,
    validate_token_name,
};
use crate::runtime::{self, Runtime, RuntimeConfig, RuntimeError, session};
use crate::store::{Store, default_data_dir, resolve_api_db_path};
use crate::{
    AbortRequest, Artifact, ArtifactPath, BootApi, Change, ChangeCreated, ChangeWsMessage,
    DeleteSessionQuery, Empty, ExportQuery, FlashRequest, FollowQuery, IdPath, InspectRequest,
    IoQuery, ListQuery, Probe, ProbeSessionRequest, ResetRequest, RevokeAllQuery, RevokeResult,
    SerialPort, SerialSessionRequest, ServerStatus, Session, SessionPatch, SessionWsMessage,
    Subscriber, SubscriberPath, TokenCreateRequest, TokenCreated, TokenPath, TokenSummary,
};

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_db: Option<PathBuf>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub allow_insecure_http: bool,
}

enum ServerImpl {}

fn runtime(rqctx: &RequestContext<AuthState>) -> Result<&Runtime, HttpError> {
    rqctx
        .context()
        .runtime()
        .map_err(|message| HttpError::for_internal_error(message))
}

fn map_error(error: RuntimeError) -> HttpError {
    let status = match error.code {
        "not_found" => dropshot::ClientErrorStatusCode::NOT_FOUND,
        "resource_busy" | "conflict" | "not_resumable" | "dap_exclusive" | "session_active" => {
            dropshot::ClientErrorStatusCode::CONFLICT
        }
        "invalid_request" => dropshot::ClientErrorStatusCode::BAD_REQUEST,
        "artifact_expired" => dropshot::ClientErrorStatusCode::GONE,
        "storage_exhausted" => dropshot::ClientErrorStatusCode::TOO_MANY_REQUESTS,
        _ => return HttpError::for_internal_error(error.message),
    };
    HttpError::for_client_error(None, status, error.message)
}

fn ensure_change_access(auth: &Auth<Control>, change: &Change) -> Result<(), HttpError> {
    if change.created_by == auth.principal().id || auth.principal().has(Capability::Admin) {
        Ok(())
    } else {
        Err(HttpError::for_client_error(
            None,
            dropshot::ClientErrorStatusCode::FORBIDDEN,
            "change belongs to another principal".to_string(),
        ))
    }
}

fn location(uri: String) -> http::HeaderValue {
    http::HeaderValue::from_str(&uri).expect("valid location header")
}

fn probe_from_resource(resource: &runtime::ResourceEntry) -> Probe {
    Probe {
        id: resource.id.clone(),
        name: resource.name.clone(),
        vendor_id: resource.vendor_id.unwrap_or(0),
        product_id: resource.product_id.unwrap_or(0),
        serial_number: resource.serial_number.clone(),
        hid_interface: None,
        probe_type: resource
            .probe_type
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        generation: resource.generation,
        state: resource.state.clone(),
        identity_stable: resource.identity_stable,
    }
}

fn serial_from_resource(resource: &runtime::ResourceEntry) -> SerialPort {
    SerialPort {
        id: resource.id.clone(),
        path: resource.path.clone().unwrap_or_default(),
        name: Some(resource.name.clone()),
        vendor_id: resource.vendor_id,
        product_id: resource.product_id,
        serial_number: resource.serial_number.clone(),
        generation: resource.generation,
        state: resource.state.clone(),
    }
}

impl BootApi for ServerImpl {
    type Context = AuthState;

    async fn health(
        _rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<ServerStatus>, HttpError> {
        Ok(HttpResponseOk(ServerStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            uptime_seconds: 0,
            health: "ok".to_string(),
            probes: 0,
            serial_ports: 0,
            changes: 0,
            sessions: 0,
            subscribers: 0,
            storage_bytes: 0,
        }))
    }

    async fn server(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<ServerStatus>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let resources = runtime.resources();
        let sessions = runtime.sessions_list();
        Ok(HttpResponseOk(ServerStatus {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_version: PROTOCOL_VERSION,
            uptime_seconds: runtime.inner.started.elapsed().as_secs(),
            health: "ok".to_string(),
            probes: resources
                .iter()
                .filter(|r| r.kind == runtime::ResourceKind::Probe)
                .count(),
            serial_ports: resources
                .iter()
                .filter(|r| r.kind == runtime::ResourceKind::SerialPort)
                .count(),
            changes: runtime.changes().len(),
            sessions: sessions.len(),
            subscribers: sessions.iter().map(|s| s.subscribers as usize).sum(),
            storage_bytes: sessions.iter().map(|s| s.data.bytes).sum(),
        }))
    }

    async fn probes(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<Vec<Probe>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        Ok(HttpResponseOk(
            runtime
                .resources()
                .iter()
                .filter(|resource| resource.kind == runtime::ResourceKind::Probe)
                .map(probe_from_resource)
                .collect(),
        ))
    }

    async fn probe(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Probe>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let resource = runtime
            .resource(&path.into_inner().id)
            .ok_or_else(|| HttpError::for_not_found(None, "probe not found".to_string()))?;
        Ok(HttpResponseOk(probe_from_resource(&resource)))
    }

    async fn serial_ports(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<Vec<SerialPort>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        Ok(HttpResponseOk(
            runtime
                .resources()
                .iter()
                .filter(|resource| resource.kind == runtime::ResourceKind::SerialPort)
                .map(serial_from_resource)
                .collect(),
        ))
    }

    async fn serial_port(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<SerialPort>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let resource = runtime
            .resource(&path.into_inner().id)
            .ok_or_else(|| HttpError::for_not_found(None, "serial port not found".to_string()))?;
        Ok(HttpResponseOk(serial_from_resource(&resource)))
    }

    async fn flash_probe(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<FlashRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseAccepted<ChangeCreated>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let request = body.into_inner();
        let change = runtime
            .create_flash(
                &path.into_inner().id,
                &request.artifact,
                request.options,
                &auth.principal().id,
            )
            .await
            .map_err(map_error)?;
        let created = ChangeCreated {
            change: change.id.clone(),
            status: change.status,
        };
        let mut response = HttpResponseHeaders::new_unnamed(HttpResponseAccepted(created));
        response
            .headers_mut()
            .insert(LOCATION, location(format!("/v1/changes/{}", change.id)));
        Ok(response)
    }

    async fn reset_probe(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<IdPath>,
        _body: TypedBody<ResetRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseAccepted<ChangeCreated>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let change = runtime
            .create_reset(&path.into_inner().id, &auth.principal().id)
            .await
            .map_err(map_error)?;
        let created = ChangeCreated {
            change: change.id.clone(),
            status: change.status,
        };
        let mut response = HttpResponseHeaders::new_unnamed(HttpResponseAccepted(created));
        response
            .headers_mut()
            .insert(LOCATION, location(format!("/v1/changes/{}", change.id)));
        Ok(response)
    }

    async fn inspect_probe(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<IdPath>,
        _body: TypedBody<InspectRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseAccepted<ChangeCreated>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let change = runtime
            .create_inspect(&path.into_inner().id, &auth.principal().id)
            .await
            .map_err(map_error)?;
        let created = ChangeCreated {
            change: change.id.clone(),
            status: change.status,
        };
        let mut response = HttpResponseHeaders::new_unnamed(HttpResponseAccepted(created));
        response
            .headers_mut()
            .insert(LOCATION, location(format!("/v1/changes/{}", change.id)));
        Ok(response)
    }

    async fn list_changes(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        query: Query<ListQuery>,
    ) -> Result<HttpResponseOk<Vec<Change>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let select = query
            .into_inner()
            .select
            .unwrap_or_else(|| "all".to_string());
        let mut changes = runtime.changes();
        if !_auth.principal().has(Capability::Admin) {
            changes.retain(|change| change.created_by == _auth.principal().id);
        }
        if select == "in-progress" {
            changes.retain(|change| !change.ready);
        } else if select == "ready" {
            changes.retain(|change| change.ready);
        }
        Ok(HttpResponseOk(changes))
    }

    async fn get_change(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Change>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let handle = runtime
            .change_handle(&path.into_inner().id)
            .map_err(map_error)?;
        let change = handle.snapshot();
        ensure_change_access(&_auth, &change)?;
        Ok(HttpResponseOk(change))
    }

    async fn follow_change(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        query: Query<FollowQuery>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        let runtime = runtime(&rqctx)?.clone();
        let id = path.into_inner().id;
        let query = query.into_inner();
        let handle = match runtime.change_handle(&id) {
            Ok(handle) => handle,
            Err(error) => return Err(map_error(error)),
        };
        if let Err(error) = ensure_change_access(&_auth, &handle.snapshot()) {
            return Err(error);
        }
        upgraded.handle(
            move |ws_stream| async move { follow_change_ws(ws_stream, handle, query).await },
        )
    }

    async fn abort_change(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<AbortRequest>,
    ) -> Result<HttpResponseOk<Change>, HttpError> {
        if body.into_inner().action != "abort" {
            return Err(HttpError::for_bad_request(
                None,
                "action must be abort".to_string(),
            ));
        }
        let runtime = runtime(&rqctx)?;
        let change_id = path.into_inner().id;
        let handle = runtime.change_handle(&change_id).map_err(map_error)?;
        ensure_change_access(&_auth, &handle.snapshot())?;
        let change = runtime.abort_change(&change_id).await.map_err(map_error)?;
        Ok(HttpResponseOk(change))
    }

    async fn create_probe_session(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<ProbeSessionRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseCreated<Session>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let request = body.into_inner();
        let session = runtime
            .create_probe_session(
                &path.into_inner().id,
                &request.kind,
                request.options,
                &auth.principal().id,
                request.max_subscribers,
                request.detach_timeout_seconds,
                request.wait_ms,
            )
            .await
            .map_err(map_error)?;
        let mut response = HttpResponseHeaders::new_unnamed(HttpResponseCreated(session.clone()));
        response
            .headers_mut()
            .insert(LOCATION, location(format!("/v1/sessions/{}", session.id)));
        Ok(response)
    }

    async fn create_serial_session(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<SerialSessionRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseCreated<Session>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let request = body.into_inner();
        let session = runtime
            .create_serial_session(
                &path.into_inner().id,
                request.options,
                &auth.principal().id,
                request.max_subscribers,
                request.detach_timeout_seconds,
                request.wait_ms,
            )
            .await
            .map_err(map_error)?;
        let mut response = HttpResponseHeaders::new_unnamed(HttpResponseCreated(session.clone()));
        response
            .headers_mut()
            .insert(LOCATION, location(format!("/v1/sessions/{}", session.id)));
        Ok(response)
    }

    async fn list_sessions(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
        query: Query<ListQuery>,
    ) -> Result<HttpResponseOk<Vec<Session>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let select = query
            .into_inner()
            .select
            .unwrap_or_else(|| "all".to_string());
        let mut sessions = runtime.sessions_list();
        match select.as_str() {
            "active" => sessions.retain(|session| {
                matches!(session.state.as_str(), "starting" | "active" | "resuming")
            }),
            "past" => sessions.retain(|session| {
                matches!(session.state.as_str(), "closed" | "failed" | "suspended")
            }),
            _ => {}
        }
        Ok(HttpResponseOk(sessions))
    }

    async fn get_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Session>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let handle = runtime
            .session_handle(&path.into_inner().id)
            .map_err(map_error)?;
        Ok(HttpResponseOk(handle.snapshot()))
    }

    async fn patch_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<SessionPatch>,
    ) -> Result<HttpResponseOk<Session>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let session = runtime
            .patch_session(&path.into_inner().id, body.into_inner())
            .await
            .map_err(map_error)?;
        Ok(HttpResponseOk(session))
    }

    async fn resume_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Session>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let session = runtime
            .resume_session(&path.into_inner().id)
            .await
            .map_err(map_error)?;
        Ok(HttpResponseOk(session))
    }

    async fn delete_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        query: Query<DeleteSessionQuery>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        let runtime = runtime(&rqctx)?;
        runtime
            .close_session(
                &path.into_inner().id,
                query.into_inner().purge.unwrap_or(false),
            )
            .await
            .map_err(map_error)?;
        Ok(HttpResponseOk(Empty {}))
    }

    async fn list_subscribers(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Vec<Subscriber>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let handle = runtime
            .session_handle(&path.into_inner().id)
            .map_err(map_error)?;
        let subscribers = handle.subscribers.lock().expect("subscribers").clone();
        Ok(HttpResponseOk(subscribers))
    }

    async fn kick_subscriber(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<SubscriberPath>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let path = path.into_inner();
        let handle = runtime.session_handle(&path.id).map_err(map_error)?;
        handle.detach(&path.subscriber_id);
        Ok(HttpResponseOk(Empty {}))
    }

    async fn session_io(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<IdPath>,
        query: Query<IoQuery>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        let runtime = runtime(&rqctx)?.clone();
        let id = path.into_inner().id;
        let query = query.into_inner();
        let handle = match runtime.session_handle(&id) {
            Ok(handle) => handle,
            Err(error) => return Err(map_error(error)),
        };
        let principal = auth.principal().id.clone();
        let mode = query.mode.clone().unwrap_or_else(|| "write".to_string());
        let subscriber = match handle.attach(&principal, &mode) {
            Ok(subscriber) => subscriber,
            Err(error) => return Err(map_error(error)),
        };
        upgraded.handle(move |ws_stream| async move {
            session_io_ws(ws_stream, runtime.inner.clone(), handle, subscriber, query).await
        })
    }

    async fn export_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        query: Query<ExportQuery>,
    ) -> Result<HttpResponseHeaders<HttpResponseOk<dropshot::FreeformBody>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let handle = runtime
            .session_handle(&path.into_inner().id)
            .map_err(map_error)?;
        let session = handle.snapshot();
        let query = query.into_inner();
        if query.format.as_deref().unwrap_or("asciicast-v3") != "asciicast-v3" {
            return Err(HttpError::for_bad_request(
                None,
                "only asciicast-v3 is supported".to_string(),
            ));
        }
        if session.kind == "dap" || !session.replayable {
            return Err(HttpError::for_client_error(
                None,
                dropshot::ClientErrorStatusCode::CONFLICT,
                "session has no asciicast-compatible data".to_string(),
            ));
        }
        let include_input = query.include_input.unwrap_or(false);
        let records = handle.replay_records(0);
        let mut lines = Vec::new();
        lines.push(
            serde_json::json!({
                "version": 3,
                "term": {"cols": 80, "rows": 24, "type": "xterm-256color"},
                "timestamp": crate::runtime::now_unix(),
                "title": format!("{} {}", session.kind, session.resource_id),
                "env": {"BOOT_SESSION_ID": session.id}
            })
            .to_string(),
        );
        let mut last_ms = records
            .first()
            .map(|record| record.timestamp_ms)
            .unwrap_or(0);
        for record in records {
            let text = match String::from_utf8(record.payload.clone()) {
                Ok(text) => text,
                Err(_) => {
                    return Err(HttpError::for_client_error(
                        None,
                        dropshot::ClientErrorStatusCode::CONFLICT,
                        "session contains non-UTF-8 data; use a binary export".to_string(),
                    ));
                }
            };
            let code = match record.kind {
                crate::runtime::session_log::RECORD_OUTPUT => "o",
                crate::runtime::session_log::RECORD_INPUT if include_input => "i",
                _ => continue,
            };
            let interval = (record.timestamp_ms.saturating_sub(last_ms)) as f64 / 1000.0;
            last_ms = record.timestamp_ms;
            lines.push(serde_json::json!([interval, code, text]).to_string());
        }
        let cast = format!("{}\n", lines.join("\n"));
        let mut response = HttpResponseHeaders::new_unnamed(HttpResponseOk(
            dropshot::FreeformBody(dropshot::Body::from(cast)),
        ));
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-asciicast"),
        );
        Ok(response)
    }

    async fn create_artifact(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        body: StreamingBody,
    ) -> Result<HttpResponseHeaders<HttpResponseCreated<Artifact>>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let stream = body.into_stream();
        tokio::pin!(stream);
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            bytes.extend_from_slice(&chunk);
        }
        let artifact = runtime::artifact::create(&runtime.inner, &bytes)
            .await
            .map_err(map_error)?;
        let mut response = HttpResponseHeaders::new_unnamed(HttpResponseCreated(artifact.clone()));
        response
            .headers_mut()
            .insert(LOCATION, location(format!("/v1/artifacts/{}", artifact.id)));
        Ok(response)
    }

    async fn get_artifact(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<ArtifactPath>,
    ) -> Result<HttpResponseOk<Artifact>, HttpError> {
        let runtime = runtime(&rqctx)?;
        let artifact = runtime::artifact::get(&runtime.inner, &path.into_inner().id)
            .await
            .map_err(map_error)?;
        Ok(HttpResponseOk(artifact))
    }

    async fn delete_artifact(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<ArtifactPath>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        let runtime = runtime(&rqctx)?;
        runtime::artifact::delete(&runtime.inner, &path.into_inner().id)
            .await
            .map_err(map_error)?;
        Ok(HttpResponseOk(Empty {}))
    }

    async fn list_tokens(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Admin>,
    ) -> Result<HttpResponseOk<Vec<TokenSummary>>, HttpError> {
        let state = rqctx.context();
        let store = state.store();
        let records = store.list_tokens().await.map_err(HttpError::from)?;
        let current_id = &auth.principal().id;
        Ok(HttpResponseOk(
            records
                .into_iter()
                .map(|record| TokenSummary {
                    id: record.id.clone(),
                    name: record.name.clone(),
                    scopes: record.scopes.names(),
                    created_at: record.created_at,
                    expires_at: record.expires_at,
                    current: &record.id == current_id,
                })
                .collect(),
        ))
    }

    async fn create_token(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Admin>,
        body: TypedBody<TokenCreateRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseCreated<TokenCreated>>, HttpError> {
        let request = body.into_inner();
        validate_token_name(&request.name)
            .map_err(|message| HttpError::for_bad_request(None, message))?;
        let scopes = match request.scopes {
            None => Scopes::read(),
            Some(names) if names.is_empty() => {
                return Err(HttpError::for_bad_request(
                    None,
                    "at least one scope is required".to_string(),
                ));
            }
            Some(names) => Scopes::from_names(names)
                .map_err(|message| HttpError::for_bad_request(None, message))?,
        };
        let expires_at = match request.expires_in_seconds {
            None => None,
            Some(seconds) if seconds <= 0 => {
                return Err(HttpError::for_bad_request(
                    None,
                    "expires_in_seconds must be positive".to_string(),
                ));
            }
            Some(seconds) => Some(unix_now().checked_add(seconds).ok_or_else(|| {
                HttpError::for_bad_request(None, "expiry is out of range".to_string())
            })?),
        };
        let state = rqctx.context();
        let store = state.store();
        let created = store
            .create_token(CreateTokenRequest {
                name: request.name,
                scopes,
                expires_at,
                replace: request.replace,
            })
            .await
            .map_err(HttpError::from)?;
        slog::info!(
            rqctx.log,
            "admin token operation";
            "actor" => %auth.principal().id,
            "operation" => "create",
            "target" => %created.record.name,
            "target_id" => %created.record.id,
        );
        let response = TokenCreated {
            id: created.record.id,
            name: created.record.name,
            scopes: created.record.scopes.names(),
            created_at: created.record.created_at,
            expires_at: created.record.expires_at,
            token: created.token,
        };
        let mut response = HttpResponseHeaders::new_unnamed(HttpResponseCreated(response));
        response
            .headers_mut()
            .insert(CACHE_CONTROL, http::HeaderValue::from_static("no-store"));
        response
            .headers_mut()
            .insert(PRAGMA, http::HeaderValue::from_static("no-cache"));
        Ok(response)
    }

    async fn revoke_token(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Admin>,
        path: Path<TokenPath>,
    ) -> Result<HttpResponseOk<RevokeResult>, HttpError> {
        let selector = path.into_inner().selector;
        let state = rqctx.context();
        let store = state.store();
        let revoked = store
            .revoke_token(&selector)
            .await
            .map_err(HttpError::from)?;
        slog::info!(
            rqctx.log,
            "admin token operation";
            "actor" => %auth.principal().id,
            "operation" => "revoke",
            "target" => %selector,
            "count" => revoked,
        );
        Ok(HttpResponseOk(RevokeResult { revoked }))
    }

    async fn revoke_all_tokens(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Admin>,
        query: Query<RevokeAllQuery>,
    ) -> Result<HttpResponseOk<RevokeResult>, HttpError> {
        if query.into_inner().all != Some(true) {
            return Err(HttpError::for_bad_request(
                None,
                "refusing to revoke every token without all=true".to_string(),
            ));
        }
        let state = rqctx.context();
        let store = state.store();
        let revoked = store.revoke_all().await.map_err(HttpError::from)?;
        slog::info!(
            rqctx.log,
            "admin token operation";
            "actor" => %auth.principal().id,
            "operation" => "revoke_all",
            "count" => revoked,
        );
        Ok(HttpResponseOk(RevokeResult { revoked }))
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

fn resolve_bind_address(host: &str, port: u16) -> Result<SocketAddr> {
    let authority = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    authority
        .to_socket_addrs()
        .with_context(|| format!("Failed to resolve {authority}"))?
        .next()
        .with_context(|| format!("{authority} did not resolve to an address"))
}

pub async fn run(options: RunOptions) -> Result<()> {
    let host = options.host.unwrap_or_else(|| "127.0.0.1".to_string());
    let port = options.port.unwrap_or(46673);
    let bind_address = resolve_bind_address(&host, port)?;
    let loopback = is_loopback_host(&host);
    match (&options.tls_cert, &options.tls_key) {
        (Some(_), Some(_)) => {}
        (None, None) => {
            if !loopback && !options.allow_insecure_http {
                bail!(
                    "refusing to bind {host} without TLS; pass --tls-cert/--tls-key or \
                     --allow-insecure-http (only on an encrypted network)"
                );
            }
            if !loopback {
                eprintln!(
                    "WARNING: boot-api is listening on {host} without TLS. Bearer tokens \
                     and firmware will be sent in plaintext."
                );
            }
        }
        _ => bail!("--tls-cert and --tls-key must be provided together"),
    }

    let api_db_path = resolve_api_db_path(options.api_db.as_deref())?;
    let db = Arc::new(
        Store::open(&api_db_path)
            .await
            .with_context(|| format!("Failed to open API database {}", api_db_path.display()))?,
    );
    let data_dir = default_data_dir()?;
    let runtime_config = RuntimeConfig::new(data_dir.join("sessions"), data_dir.join("artifacts"));
    let runtime = Runtime::new(db.clone(), runtime_config);
    runtime.start().await?;
    let store: Arc<dyn TokenStore> = db.clone();
    let state = AuthState::new_with_runtime(store, runtime);

    let log = ConfigLogging::StderrTerminal {
        level: ConfigLoggingLevel::Info,
    }
    .to_logger("boot-api")?;

    let api = crate::boot_api_mod::api_description::<ServerImpl>()
        .map_err(|error| anyhow::anyhow!("failed to build API description: {error}"))?;
    let tls = match (&options.tls_cert, &options.tls_key) {
        (Some(cert_file), Some(key_file)) => Some(ConfigTls::AsFile {
            cert_file: cert_file.clone(),
            key_file: key_file.clone(),
        }),
        _ => None,
    };
    let mut builder = ServerBuilder::new(api, state, log).config(ConfigDropshot {
        bind_address,
        ..Default::default()
    });
    if let Some(tls) = tls {
        builder = builder.tls(Some(tls));
    }
    let server = builder.start()?;
    server.await.map_err(|error| anyhow::anyhow!(error))
}

async fn follow_change_ws(
    ws_stream: dropshot::WebsocketConnection,
    handle: Arc<runtime::ChangeHandle>,
    query: FollowQuery,
) -> WsResult {
    let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
        ws_stream.into_inner(),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;
    let (mut tx, mut rx) = ws_stream.split();
    let snapshot = handle.snapshot();
    let (messages, mut events) = handle.follow(query.after);
    let hello = ChangeWsMessage::Hello {
        v: PROTOCOL_VERSION,
        protocol: PROTOCOL_VERSION,
        seq: handle.seq.load(Ordering::SeqCst),
        change: snapshot.clone(),
        replayed: query.after.is_some(),
    };
    send_json(&mut tx, &hello).await?;
    for message in messages {
        send_json(&mut tx, &message).await?;
    }
    if snapshot.ready {
        let _ = tx
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "change complete".into(),
            })))
            .await;
        return Ok(());
    }
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(message) => {
                        let terminal = matches!(message, ChangeWsMessage::ChangeCompleted { .. });
                        send_json(&mut tx, &message).await?;
                        if terminal { break; }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let hello = ChangeWsMessage::Hello {
                            v: PROTOCOL_VERSION,
                            protocol: PROTOCOL_VERSION,
                            seq: handle.seq.load(Ordering::SeqCst),
                            change: handle.snapshot(),
                            replayed: false,
                        };
                        send_json(&mut tx, &hello).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = rx.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

async fn session_io_ws(
    ws_stream: dropshot::WebsocketConnection,
    inner: Arc<runtime::RuntimeInner>,
    handle: Arc<runtime::SessionHandle>,
    subscriber: Subscriber,
    query: IoQuery,
) -> WsResult {
    let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
        ws_stream.into_inner(),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;
    let (mut tx, mut rx) = ws_stream.split();
    let snapshot = handle.snapshot();
    let include_input = query.include_input.unwrap_or(false);
    let after = query.after.unwrap_or_else(|| {
        if snapshot.replayable
            && matches!(snapshot.state.as_str(), "closed" | "failed" | "suspended")
        {
            0
        } else {
            snapshot.data.latest_seq
        }
    });
    let hello = SessionWsMessage::Hello {
        v: PROTOCOL_VERSION,
        protocol: PROTOCOL_VERSION,
        subscriber_id: subscriber.id.clone(),
        mode: subscriber.mode.clone(),
        session: snapshot.clone(),
        data_seq: after,
        oldest_seq: snapshot.data.oldest_seq,
        latest_seq: snapshot.data.latest_seq,
    };
    send_json(&mut tx, &hello).await?;
    let mut cursor = after;
    for (seq, channel, data, input) in handle.replay(after, include_input) {
        if input {
            send_json(
                &mut tx,
                &SessionWsMessage::Input {
                    seq,
                    data: base64_encode(&data),
                },
            )
            .await?;
        } else {
            send_binary(&mut tx, &handle.kind, seq, channel, &data).await?;
        }
        cursor = cursor.max(seq);
    }
    if matches!(snapshot.state.as_str(), "closed" | "failed" | "suspended") {
        let _ = send_json(
            &mut tx,
            &SessionWsMessage::Ended {
                session: handle.snapshot(),
            },
        )
        .await;
        let _ = tx
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "session ended".into(),
            })))
            .await;
        handle.detach(&subscriber.id);
        return Ok(());
    }
    let mut lifecycle = handle.lifecycle_tx.subscribe();
    'session_loop: loop {
        let chunks = handle.read_after(cursor);
        if !chunks.is_empty() {
            for (seq, channel, data) in chunks {
                send_binary(&mut tx, &handle.kind, seq, channel, &data).await?;
                cursor = seq;
            }
            continue;
        }
        tokio::select! {
            _ = handle.notify.notified() => {}
            _ = handle.subscriber_changed.notified() => {
                if !handle.subscribers.lock().expect("subscribers").iter().any(|sub| sub.id == subscriber.id) {
                    break 'session_loop;
                }
            }
            event = lifecycle.recv() => {
                match event {
                    Ok(session) => {
                        if session.state == "closed" {
                            send_json(&mut tx, &SessionWsMessage::Ended { session }).await?;
                            break 'session_loop;
                        }
                        match session.state.as_str() {
                            "active" => {
                                send_json(&mut tx, &SessionWsMessage::Active { session }).await?;
                            }
                            "suspended" => {
                                send_json(&mut tx, &SessionWsMessage::Suspended { session }).await?;
                                break 'session_loop;
                            }
                            "failed" => {
                                send_json(
                                    &mut tx,
                                    &SessionWsMessage::Failed {
                                        session,
                                        error: "session failed".into(),
                                    },
                                )
                                .await?;
                                break 'session_loop;
                            }
                            _ => {}
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = rx.next() => {
                match incoming {
                    Some(Ok(Message::Binary(data))) => {
                        if subscriber.mode != "write" {
                            send_json(&mut tx, &SessionWsMessage::Error {
                                code: "write_forbidden".into(),
                                message: "subscriber is read-only".into(),
                            }).await?;
                        } else if handle.kind == "serial" {
                            handle.push_input(&data);
                            let sender = handle.input_tx.lock().expect("input").clone();
                            if let Some(sender) = sender {
                                let _ = sender.send(data.to_vec());
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }
    handle.detach(&subscriber.id);
    session::schedule_close_if_idle(inner.clone(), handle.clone());
    Ok(())
}

async fn send_json<S>(tx: &mut S, message: &impl serde::Serialize) -> WsResult
where
    S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let text = serde_json::to_string(message)?;
    tx.send(Message::Text(text.into())).await?;
    Ok(())
}

async fn send_binary<S>(tx: &mut S, kind: &str, seq: u64, channel: u8, data: &[u8]) -> WsResult
where
    S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut frame = Vec::with_capacity(9 + data.len());
    frame.extend_from_slice(&seq.to_be_bytes());
    if kind == "rtt" {
        frame.push(channel);
    }
    frame.extend_from_slice(data);
    tx.send(Message::Binary(frame.into())).await?;
    Ok(())
}

type WsResult = Result<(), Box<dyn std::error::Error + Send + Sync + 'static>>;

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::prelude::BASE64_STANDARD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    use dropshot::Body;
    use dropshot::test_util::{TestContext, read_json};
    use http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
    use http::{Method, Request, Response, StatusCode};

    use crate::auth::{AuthState, CreateTokenRequest, Scopes};
    use crate::runtime::RuntimeConfig;

    struct TestServer {
        context: TestContext<AuthState>,
        db: Arc<Store>,
        directory: PathBuf,
    }

    impl TestServer {
        async fn new(name: &str) -> Self {
            let directory = std::env::temp_dir().join(format!(
                "boot-api-http-test-{}-{}",
                std::process::id(),
                name
            ));
            let _ = std::fs::remove_dir_all(&directory);
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("boot.sqlite3");
            let db = Arc::new(Store::open(&path).await.unwrap());
            let runtime = crate::Runtime::new(
                db.clone(),
                RuntimeConfig::new(directory.join("sessions"), directory.join("artifacts")),
            );
            runtime.start().await.unwrap();
            let store: Arc<dyn TokenStore> = db.clone();
            let state = AuthState::new_with_runtime(store, runtime);
            let logger = slog::Logger::root(slog::Discard, slog::o!());
            let api = crate::boot_api_mod::api_description::<ServerImpl>().unwrap();
            let config = ConfigDropshot {
                bind_address: "127.0.0.1:0".parse().unwrap(),
                ..Default::default()
            };
            let context = TestContext::new(api, state, &config, None, logger);
            Self {
                context,
                db,
                directory,
            }
        }

        fn request(
            &self,
            method: Method,
            path: &str,
            token: Option<&str>,
            body: &str,
        ) -> Request<Body> {
            let mut builder = Request::builder()
                .method(method)
                .uri(self.context.client_testctx.url(path));
            if let Some(token) = token {
                builder = builder.header(AUTHORIZATION, format!("Bearer {token}"));
            }
            builder.body(body.to_string().into()).unwrap()
        }

        async fn send(&self, request: Request<Body>, expected: StatusCode) -> Response<Body> {
            let response = self
                .context
                .client_testctx
                .client
                .request(request)
                .await
                .expect("request failed")
                .map(Body::wrap);
            assert_eq!(response.status(), expected);
            response
        }

        async fn create(&self, name: &str, scopes: Scopes) -> String {
            self.db
                .create_token(CreateTokenRequest {
                    name: name.to_string(),
                    scopes,
                    expires_at: None,
                    replace: false,
                })
                .await
                .unwrap()
                .token
        }

        async fn teardown(self) {
            self.context.teardown().await;
            let _ = std::fs::remove_dir_all(self.directory);
        }
    }

    #[tokio::test]
    async fn health_is_public_and_hardware_requires_a_token() {
        let server = TestServer::new("public").await;
        server
            .send(
                server.request(Method::GET, "/v1/health", None, ""),
                StatusCode::OK,
            )
            .await;
        let response = server
            .send(
                server.request(Method::GET, "/v1/probes", None, ""),
                StatusCode::UNAUTHORIZED,
            )
            .await;
        assert_eq!(
            response.headers().get(WWW_AUTHENTICATE).unwrap(),
            "Bearer realm=\"boot-api\""
        );
        server.teardown().await;
    }

    #[tokio::test]
    async fn scopes_and_admin_endpoints_are_enforced() {
        let server = TestServer::new("scopes").await;
        let admin = server.create("admin", Scopes::admin()).await;
        let read = server.create("reader", Scopes::read()).await;
        let control = server.create("control", Scopes::control()).await;

        server
            .send(
                server.request(Method::GET, "/v1/probes", Some(&read), ""),
                StatusCode::OK,
            )
            .await;
        server
            .send(
                server.request(
                    Method::POST,
                    "/v1/probes/missing/flash",
                    Some(&read),
                    r#"{"artifact":"missing"}"#,
                ),
                StatusCode::FORBIDDEN,
            )
            .await;
        server
            .send(
                server.request(
                    Method::POST,
                    "/v1/probes/missing/flash",
                    Some(&control),
                    r#"{"artifact":"missing"}"#,
                ),
                StatusCode::NOT_FOUND,
            )
            .await;
        for token in [&read, &control] {
            server
                .send(
                    server.request(Method::GET, "/v1/tokens", Some(token), ""),
                    StatusCode::FORBIDDEN,
                )
                .await;
        }
        let body = serde_json::json!({"name":"created","scopes":["read"]}).to_string();
        let mut response = server
            .send(
                server.request(Method::POST, "/v1/tokens", Some(&admin), &body),
                StatusCode::CREATED,
            )
            .await;
        let created: crate::TokenCreated = read_json(&mut response).await;
        assert!(created.token.starts_with(crate::auth::TOKEN_PREFIX));
        server.teardown().await;
    }

    #[tokio::test]
    async fn artifacts_are_owned_by_http_surface() {
        let server = TestServer::new("artifacts").await;
        let control = server.create("control", Scopes::control()).await;
        let mut response = server
            .send(
                server.request(
                    Method::POST,
                    "/v1/artifacts",
                    Some(&control),
                    "firmware-bytes",
                ),
                StatusCode::CREATED,
            )
            .await;
        let artifact: crate::Artifact = read_json(&mut response).await;
        assert_eq!(artifact.size, 14);
        let mut response = server
            .send(
                server.request(
                    Method::GET,
                    &format!("/v1/artifacts/{}", artifact.id),
                    Some(&control),
                    "",
                ),
                StatusCode::OK,
            )
            .await;
        let fetched: crate::Artifact = read_json(&mut response).await;
        assert_eq!(fetched.sha256, artifact.sha256);
        server.teardown().await;
    }
}
