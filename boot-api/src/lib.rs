use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::RequestContext;
use dropshot::Path;
use dropshot::Query;
use dropshot::TypedBody;
use dropshot::WebsocketUpgrade;
use dropshot::WebsocketEndpointResult;
use schemars::JsonSchema;
use serde::{Serialize, Deserialize};

#[cfg(feature = "run")]
mod run;
#[cfg(feature = "run")]
pub use run::run;

#[derive(Serialize, JsonSchema)]
struct Empty {
}

#[derive(Serialize, JsonSchema)]
struct Probe {
    pub id: String,
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
    pub hid_interface: Option<u8>,
    pub probe_type: String,
}

#[derive(Serialize, JsonSchema)]
struct Target {
    pub name: String,
}

#[derive(Serialize, JsonSchema)]
struct SerialPort {
    pub id: String,
    pub path: String,
    pub name: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct SelectorPath {
    pub selector: String,
}

#[derive(Deserialize, JsonSchema)]
struct PortIdPath {
    pub port_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct FlashRequest {
    pub elf_data: String,
}

#[derive(Deserialize, JsonSchema)]
struct AttachQuery {
    /// Optional RTT control block address (hex string, e.g., "0x20000000")
    pub rtt_addr: Option<String>,
}

/// Defines the trait that captures all the methods.
#[dropshot::api_description]
trait BootApi {
    /// The context type used within endpoints.
    type Context;

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
    ) -> Result<HttpResponseOk<Vec<Probe>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/serial-ports",
    }]
    async fn serial_ports(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Vec<SerialPort>>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/probes/{selector}/target",
    }]
    async fn probes_target(
        rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
    ) -> Result<HttpResponseOk<Target>, HttpError>;

    #[endpoint {
        method = POST,
        path = "/v1/probes/{selector}",
        request_body_max_bytes = 10485760, // 10 MB for ELF files
    }]
    async fn flash_and_run(
        rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
        body: TypedBody<FlashRequest>,
    ) -> Result<HttpResponseOk<Empty>, HttpError>;

    #[endpoint {
        method = GET,
        path = "/v1/serial-ports/{port_id}/shell",
    }]
    async fn serial_shell(
        rqctx: RequestContext<Self::Context>,
        path: Path<PortIdPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult;

    #[endpoint {
        method = GET,
        path = "/v1/probes/{selector}/attach",
    }]
    async fn probe_attach(
        rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
        query: Query<AttachQuery>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult;
}



pub fn openapi() -> String {
    let description = boot_api_mod::stub_api_description().unwrap();
    let openapi = description
        .openapi("Boot API", semver::Version::new(1, 0, 0));
    let json = openapi.json().unwrap();
    serde_json::to_string(&json).unwrap()
}
