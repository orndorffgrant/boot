use dropshot::HttpError;
use dropshot::HttpResponseAccepted;
use dropshot::HttpResponseCreated;
use dropshot::HttpResponseHeaders;
use dropshot::HttpResponseOk;
use dropshot::Path;
use dropshot::Query;
use dropshot::RequestContext;
use dropshot::TypedBody;
use dropshot::WebsocketEndpointResult;
use dropshot::WebsocketUpgrade;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub mod auth;

#[cfg(feature = "run")]
mod runtime;
#[cfg(feature = "run")]
pub mod store;

#[cfg(feature = "run")]
mod run;
#[cfg(feature = "run")]
pub use run::{RunOptions, is_loopback_host, run};
#[cfg(feature = "run")]
pub use runtime::{Runtime, RuntimeConfig};

pub use boot_protocol::*;

use auth::{Admin, Auth, Control, Read};

#[derive(Serialize, JsonSchema)]
pub struct Empty {}

#[derive(Deserialize, JsonSchema)]
pub struct IdPath {
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TokenPath {
    pub selector: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ArtifactPath {
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SubscriberPath {
    pub id: String,
    pub subscriber_id: String,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct ListQuery {
    #[serde(default)]
    pub select: Option<String>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct DeleteSessionQuery {
    #[serde(default)]
    pub purge: Option<bool>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct FollowQuery {
    #[serde(default)]
    pub after: Option<u64>,
    #[serde(default)]
    pub include_input: Option<bool>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct ExportQuery {
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub channel: Option<u8>,
    #[serde(default)]
    pub include_input: Option<bool>,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct IoQuery {
    #[serde(default)]
    pub after: Option<u64>,
    #[serde(default)]
    pub replay: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub include_input: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
pub struct AbortRequest {
    pub action: String,
}

#[derive(Serialize, JsonSchema)]
pub struct TokenSummary {
    pub id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub current: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct TokenCreateRequest {
    pub name: String,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    pub expires_in_seconds: Option<i64>,
    #[serde(default)]
    pub replace: bool,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub struct TokenCreated {
    pub id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub token: String,
}

#[derive(Serialize, JsonSchema)]
pub struct RevokeResult {
    pub revoked: usize,
}

#[derive(Deserialize, JsonSchema)]
pub struct RevokeAllQuery {
    pub all: Option<bool>,
}

/// Defines the trait that captures all the methods.
#[dropshot::api_description]
trait BootApi {
    /// The context type used within endpoints.
    type Context;

    /// Liveness probe. Public so monitoring and `boot check-auth` can test
    /// reachability without a token.
    #[endpoint {
        method = GET,
        path = "/v1/health",
    }]
    async fn health(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<ServerStatus>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/server",
    }]
    async fn server(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<ServerStatus>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/probes",
    }]
    async fn probes(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<Vec<Probe>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/probes/{id}",
    }]
    async fn probe(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Probe>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/serial-ports",
    }]
    async fn serial_ports(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<Vec<SerialPort>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/serial-ports/{id}",
    }]
    async fn serial_port(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<SerialPort>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/probes/{id}/flash",
    }]
    async fn flash_probe(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<FlashRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseAccepted<ChangeCreated>>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/probes/{id}/reset",
    }]
    async fn reset_probe(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<ResetRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseAccepted<ChangeCreated>>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/probes/{id}/inspect",
    }]
    async fn inspect_probe(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<InspectRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseAccepted<ChangeCreated>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/changes",
    }]
    async fn list_changes(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        query: Query<ListQuery>,
    ) -> Result<HttpResponseOk<Vec<Change>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/changes/{id}",
    }]
    async fn get_change(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Change>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/changes/{id}/follow",
    }]
    async fn follow_change(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        query: Query<FollowQuery>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult;

    #[endpoint {
        method = POST,
        path = "/v1/changes/{id}",
    }]
    async fn abort_change(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<AbortRequest>,
    ) -> Result<HttpResponseOk<Change>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/probes/{id}/sessions",
    }]
    async fn create_probe_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<ProbeSessionRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseCreated<Session>>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/serial-ports/{id}/sessions",
    }]
    async fn create_serial_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<SerialSessionRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseCreated<Session>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/sessions",
    }]
    async fn list_sessions(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
        query: Query<ListQuery>,
    ) -> Result<HttpResponseOk<Vec<Session>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/sessions/{id}",
    }]
    async fn get_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Session>, HttpError>;

    #[endpoint {
        method = PATCH,
        path = "/v1/sessions/{id}",
    }]
    async fn patch_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        body: TypedBody<SessionPatch>,
    ) -> Result<HttpResponseOk<Session>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/sessions/{id}/resume",
    }]
    async fn resume_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Session>, HttpError>;

    #[endpoint {
        method = DELETE,
        path = "/v1/sessions/{id}",
    }]
    async fn delete_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        query: Query<DeleteSessionQuery>,
    ) -> Result<HttpResponseOk<Empty>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/sessions/{id}/subscribers",
    }]
    async fn list_subscribers(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
    ) -> Result<HttpResponseOk<Vec<Subscriber>>, HttpError>;

    #[endpoint {
        method = DELETE,
        path = "/v1/sessions/{id}/subscribers/{subscriber_id}",
    }]
    async fn kick_subscriber(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<SubscriberPath>,
    ) -> Result<HttpResponseOk<Empty>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/sessions/{id}/io",
    }]
    async fn session_io(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        query: Query<IoQuery>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult;

    #[endpoint {
        method = GET,
        path = "/v1/sessions/{id}/export",
    }]
    async fn export_session(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<IdPath>,
        query: Query<ExportQuery>,
    ) -> Result<HttpResponseHeaders<HttpResponseOk<dropshot::FreeformBody>>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/artifacts",
        request_body_max_bytes = 67108864,
    }]
    async fn create_artifact(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        body: dropshot::StreamingBody,
    ) -> Result<HttpResponseHeaders<HttpResponseCreated<Artifact>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/artifacts/{id}",
    }]
    async fn get_artifact(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<ArtifactPath>,
    ) -> Result<HttpResponseOk<Artifact>, HttpError>;

    #[endpoint {
        method = DELETE,
        path = "/v1/artifacts/{id}",
    }]
    async fn delete_artifact(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<ArtifactPath>,
    ) -> Result<HttpResponseOk<Empty>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/tokens",
    }]
    async fn list_tokens(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Admin>,
    ) -> Result<HttpResponseOk<Vec<TokenSummary>>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/tokens",
    }]
    async fn create_token(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Admin>,
        body: TypedBody<TokenCreateRequest>,
    ) -> Result<HttpResponseHeaders<HttpResponseCreated<TokenCreated>>, HttpError>;

    #[endpoint {
        method = DELETE,
        path = "/v1/tokens/{selector}",
    }]
    async fn revoke_token(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Admin>,
        path: Path<TokenPath>,
    ) -> Result<HttpResponseOk<RevokeResult>, HttpError>;

    #[endpoint {
        method = DELETE,
        path = "/v1/tokens",
    }]
    async fn revoke_all_tokens(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Admin>,
        query: Query<RevokeAllQuery>,
    ) -> Result<HttpResponseOk<RevokeResult>, HttpError>;
}

pub fn openapi() -> String {
    let description = boot_api_mod::stub_api_description().unwrap();
    let openapi = description.openapi("Boot API", semver::Version::new(1, 0, 0));
    let mut json: serde_json::Value = openapi.json().unwrap();
    apply_security(&mut json);
    serde_json::to_string(&json).unwrap()
}

/// Add the HTTP bearer security scheme and the 401/403 responses that Dropshot
/// does not infer from the extractor.
fn apply_security(document: &mut serde_json::Value) {
    let Some(paths) = document
        .get_mut("paths")
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };

    for (path, item) in paths.iter_mut() {
        let Some(operation) = item.as_object_mut() else {
            continue;
        };
        let public = path == "/v1/health";
        for method in ["get", "post", "put", "delete", "patch"] {
            let Some(operation) = operation
                .get_mut(method)
                .and_then(|value| value.as_object_mut())
            else {
                continue;
            };
            operation.insert(
                "security".to_string(),
                if public {
                    serde_json::json!([])
                } else {
                    serde_json::json!([{ "bootAuth": [] }])
                },
            );
            if !public {
                let responses = operation
                    .entry("responses")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(responses) = responses.as_object_mut() {
                    responses.entry("401").or_insert_with(
                        || serde_json::json!({ "$ref": "#/components/responses/Error" }),
                    );
                    responses.entry("403").or_insert_with(
                        || serde_json::json!({ "$ref": "#/components/responses/Error" }),
                    );
                }
            }
        }
    }

    if !document
        .get("components")
        .is_some_and(|components| components.is_object())
    {
        document["components"] = serde_json::json!({});
    }
    document["components"]["securitySchemes"] = serde_json::json!({
        "bootAuth": {
            "type": "http",
            "scheme": "bearer",
            "bearerFormat": "boot_v1"
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_documents_bearer_auth() {
        let document: serde_json::Value =
            serde_json::from_str(&openapi()).expect("valid generated OpenAPI");
        assert_eq!(
            document["components"]["securitySchemes"]["bootAuth"]["scheme"],
            "bearer"
        );
        assert_eq!(
            document["paths"]["/v1/health"]["get"]["security"],
            serde_json::json!([])
        );
        assert_eq!(
            document["paths"]["/v1/probes"]["get"]["security"],
            serde_json::json!([{ "bootAuth": [] }])
        );
        assert!(document["paths"]["/v1/tokens"]["post"]["responses"]["401"].is_object());
        assert!(document["paths"]["/v1/tokens"]["post"]["responses"]["403"].is_object());
    }
}
