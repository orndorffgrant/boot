use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use dropshot::{
    ConfigDropshot, ConfigLogging, ConfigLoggingLevel, ConfigTls, HttpError, HttpResponseCreated,
    HttpResponseHeaders, HttpResponseOk, Path, Query, RequestContext, ServerBuilder, TypedBody,
    WebsocketEndpointResult, WebsocketUpgrade,
};
use http::header::{CACHE_CONTROL, PRAGMA};

use crate::api_db::ApiDb;
use crate::auth::{
    Admin, Auth, Control, CreateTokenRequest, Read, Scopes, TokenStore, validate_token_name,
};
use crate::{
    BootApi, Empty, FlashRequest, PortIdPath, Probe, RevokeAllQuery, RevokeResult,
    SelectorAndRttPath, SelectorPath, SerialPort, Target, TokenCreateRequest, TokenCreated,
    TokenPath, TokenSummary,
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

impl BootApi for ServerImpl {
    type Context = crate::auth::AuthState;

    async fn health(
        _rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        Ok(HttpResponseOk(Empty {}))
    }

    async fn probes(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<Vec<Probe>>, HttpError> {
        crate::probe::list_probes(rqctx).await
    }

    async fn serial_ports(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<Vec<SerialPort>>, HttpError> {
        crate::serial_port::list_serial_ports(rqctx).await
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

    async fn probes_target(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<SelectorPath>,
    ) -> Result<HttpResponseOk<Target>, HttpError> {
        crate::probe::get_target(rqctx, path, &auth).await
    }

    async fn flash_and_run(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<SelectorPath>,
        body: TypedBody<FlashRequest>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        crate::probe::flash_and_run(rqctx, path, body, &auth).await
    }

    async fn serial_shell(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<PortIdPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        crate::serial_port::serial_shell(rqctx, path, upgraded, auth)
    }

    async fn probe_attach(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<SelectorPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        crate::probe::attach(rqctx, path, upgraded, auth)
    }

    async fn probe_attach_with_rtt(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<SelectorAndRttPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        crate::probe::attach_with_rtt(rqctx, path, upgraded, auth)
    }

    async fn probe_dap(
        rqctx: RequestContext<Self::Context>,
        auth: Auth<Control>,
        path: Path<SelectorPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        crate::probe::dap_server(rqctx, path, upgraded, auth)
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

    let api_db_path = crate::api_db::resolve_api_db_path(options.api_db.as_deref())?;
    let db = Arc::new(
        ApiDb::open(&api_db_path)
            .await
            .with_context(|| format!("Failed to open API database {}", api_db_path.display()))?,
    );
    let store: Arc<dyn TokenStore> = db.clone();
    let state = crate::auth::AuthState::new(store);

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

#[cfg(test)]
mod tests {
    use super::*;

    use dropshot::Body;
    use dropshot::test_util::{TestContext, read_json};
    use http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
    use http::{Method, Request, Response, StatusCode};

    use crate::api_db::ApiDb;
    use crate::auth::{AuthState, CreateTokenRequest, Scopes};

    struct TestServer {
        context: TestContext<AuthState>,
        db: Arc<ApiDb>,
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

            let db = Arc::new(ApiDb::open(&path).await.unwrap());
            let store: Arc<dyn TokenStore> = db.clone();
            let state = AuthState::new(store);
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
            // Use the underlying client directly: Dropshot's test helper
            // rejects response headers such as WWW-Authenticate and
            // Cache-Control that this API intentionally sets.
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
            let created = self
                .db
                .create_token(CreateTokenRequest {
                    name: name.to_string(),
                    scopes,
                    expires_at: None,
                    replace: false,
                })
                .await
                .unwrap();
            created.token
        }

        async fn teardown(self) {
            self.context.teardown().await;
            let _ = std::fs::remove_dir_all(self.directory);
        }
    }

    #[tokio::test]
    async fn health_is_public_and_hardware_requires_a_token() {
        let server = TestServer::new("public").await;
        let response = server
            .send(
                server.request(Method::GET, "/v1/health", None, ""),
                StatusCode::OK,
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = server
            .send(
                server.request(Method::GET, "/v1/probes", None, ""),
                StatusCode::UNAUTHORIZED,
            )
            .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
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

        // A read token can enumerate but cannot flash.
        server
            .send(
                server.request(Method::GET, "/v1/probes", Some(&read), ""),
                StatusCode::OK,
            )
            .await;

        // Use a valid body so the result depends on the token scope rather than
        // on the order in which the framework parses the body and checks auth.
        server
            .send(
                server.request(
                    Method::POST,
                    "/v1/probes/foo",
                    Some(&read),
                    r#"{"elf_data":""}"#,
                ),
                StatusCode::FORBIDDEN,
            )
            .await;

        // A control token with a bad body gets 400, not 401.
        server
            .send(
                server.request(Method::POST, "/v1/probes/foo", Some(&control), ""),
                StatusCode::BAD_REQUEST,
            )
            .await;

        // Read and control tokens cannot use the admin API.
        for token in [&read, &control] {
            server
                .send(
                    server.request(Method::GET, "/v1/tokens", Some(token), ""),
                    StatusCode::FORBIDDEN,
                )
                .await;
        }

        // Admin can create a token and use it immediately.
        let body = serde_json::json!({
            "name": "created",
            "scopes": ["read"]
        })
        .to_string();
        let mut response = server
            .send(
                server.request(Method::POST, "/v1/tokens", Some(&admin), &body),
                StatusCode::CREATED,
            )
            .await;
        assert_eq!(
            response.headers().get(http::header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let created: crate::TokenCreated = read_json(&mut response).await;
        assert!(created.token.starts_with(crate::auth::TOKEN_PREFIX));

        server
            .send(
                server.request(Method::GET, "/v1/probes", Some(&created.token), ""),
                StatusCode::OK,
            )
            .await;

        // Revocation takes effect on the next request through the same server.
        server
            .send(
                server.request(Method::DELETE, "/v1/tokens/created", Some(&admin), ""),
                StatusCode::OK,
            )
            .await;

        server
            .send(
                server.request(Method::GET, "/v1/probes", Some(&created.token), ""),
                StatusCode::UNAUTHORIZED,
            )
            .await;

        // Refuse a bare DELETE /v1/tokens.
        server
            .send(
                server.request(Method::DELETE, "/v1/tokens", Some(&admin), ""),
                StatusCode::BAD_REQUEST,
            )
            .await;

        // Unknown selectors are 404.
        server
            .send(
                server.request(
                    Method::DELETE,
                    "/v1/tokens/does-not-exist",
                    Some(&admin),
                    "",
                ),
                StatusCode::NOT_FOUND,
            )
            .await;

        server.teardown().await;
    }

    #[tokio::test]
    async fn no_provision_still_has_admin_api() {
        // The API DB is created and migrated at startup even when the CLI
        // does not provision a local token.
        let server = TestServer::new("no-provision").await;
        let admin = server.create("admin", Scopes::admin()).await;
        server
            .send(
                server.request(Method::GET, "/v1/tokens", Some(&admin), ""),
                StatusCode::OK,
            )
            .await;
        server.teardown().await;
    }
}
