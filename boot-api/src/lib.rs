use dropshot::HttpError;
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
pub mod api_db;
#[cfg(feature = "run")]
mod probe;
#[cfg(feature = "run")]
mod probe_dap;
#[cfg(feature = "run")]
mod run;
#[cfg(feature = "run")]
mod serial_port;
#[cfg(feature = "run")]
pub use run::{RunOptions, is_loopback_host, run};

use auth::{Admin, Auth, Control, Read};

#[derive(Serialize, JsonSchema)]
pub struct Empty {}

#[derive(Serialize, JsonSchema)]
pub struct Probe {
    pub id: String,
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
    pub hid_interface: Option<u8>,
    pub probe_type: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Target {
    pub name: String,
}

#[derive(Serialize, JsonSchema)]
pub struct SerialPort {
    pub id: String,
    pub path: String,
    pub name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SelectorPath {
    pub selector: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SelectorAndRttPath {
    pub selector: String,
    pub rtt_addr: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct PortIdPath {
    pub port_id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TokenPath {
    pub selector: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct FlashRequest {
    pub elf_data: String,
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
    ) -> Result<HttpResponseOk<Empty>, HttpError>;

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
        path = "/v1/serial-ports",
    }]
    async fn serial_ports(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Read>,
    ) -> Result<HttpResponseOk<Vec<SerialPort>>, HttpError>;

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

    #[endpoint {
        method = GET,
        path = "/v1/probes/{selector}/target",
    }]
    async fn probes_target(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<SelectorPath>,
    ) -> Result<HttpResponseOk<Target>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/probes/{selector}",
        request_body_max_bytes = 10485760, // 10 MB for ELF files
    }]
    async fn flash_and_run(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<SelectorPath>,
        body: TypedBody<FlashRequest>,
    ) -> Result<HttpResponseOk<Empty>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/serial-ports/{port_id}/shell",
    }]
    async fn serial_shell(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<PortIdPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult;

    #[endpoint {
        method = GET,
        path = "/v1/probes/{selector}/attach",
    }]
    async fn probe_attach(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<SelectorPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult;

    #[endpoint {
        method = GET,
        path = "/v1/probes/{selector}/attach/{rtt_addr}",
    }]
    async fn probe_attach_with_rtt(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<SelectorAndRttPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult;

    #[endpoint {
        method = GET,
        path = "/v1/probes/{selector}/dap",
    }]
    async fn probe_dap(
        rqctx: RequestContext<Self::Context>,
        _auth: Auth<Control>,
        path: Path<SelectorPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult;
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
                    // Reference the same error body type as Dropshot's
                    // generated 4XX response so generated clients have a
                    // single error type for the operation.
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
