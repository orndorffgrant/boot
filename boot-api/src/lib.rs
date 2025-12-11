use dropshot::ConfigDropshot;
use dropshot::ConfigLogging;
use dropshot::ConfigLoggingLevel;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::RequestContext;
use dropshot::ServerBuilder;
use schemars::JsonSchema;
use serde::Serialize;
use probe_rs::probe;

#[derive(Serialize, JsonSchema)]
struct Empty {
}

#[derive(Serialize, JsonSchema)]
struct Programmer {
    pub id: String,
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
    pub hid_interface: Option<u8>,
    pub probe_type: String,
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
        path = "/v1/programmers",
    }]
    async fn programmers(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Vec<Programmer>>, HttpError>;
}

enum ServerImpl {}

impl BootApi for ServerImpl {
    type Context = ();

    async fn health(
        _rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        Ok(HttpResponseOk(Empty{}))
    }

    async fn programmers(
        _rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Vec<Programmer>>, HttpError> {
        let lister = probe::list::Lister::new();
        let probes = lister.list_all();
        let programmers = probes.iter().map(|probe| {
            Programmer {
                id: "TODO".to_string(),
                name: probe.identifier.clone(),
                vendor_id: probe.vendor_id,
                product_id: probe.product_id,
                serial_number: probe.serial_number.clone(),
                hid_interface: probe.hid_interface,
                probe_type: probe.probe_type(),
            }
        }).collect();
        Ok(HttpResponseOk(programmers))
    }
}

pub async fn run(port: Option<u16>) -> anyhow::Result<()> {
    let port = port.unwrap_or(46673);
    let bind_address = format!("127.0.0.1:{}", port);
    
    let log = ConfigLogging::StderrTerminal {
        level: ConfigLoggingLevel::Info,
    }
    .to_logger("boot-api")?;
    
    let api = boot_api_mod::api_description::<ServerImpl>().unwrap();
    let server = ServerBuilder::new(api, (), log).config(ConfigDropshot{bind_address: bind_address.parse().unwrap(), ..Default::default()}).start()?;
    server.await.map_err(|e| anyhow::Error::msg(e))
}

pub fn openapi() -> String {
    let description = boot_api_mod::stub_api_description().unwrap();
    let openapi = description
        .openapi("Boot API", semver::Version::new(1, 0, 0));
    let json = openapi.json().unwrap();
    serde_json::to_string(&json).unwrap()
}
