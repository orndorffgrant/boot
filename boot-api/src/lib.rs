use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::RequestContext;
use schemars::JsonSchema;
use serde::Serialize;

#[cfg(feature = "run")]
mod run;
#[cfg(feature = "run")]
pub use run::run;

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



pub fn openapi() -> String {
    let description = boot_api_mod::stub_api_description().unwrap();
    let openapi = description
        .openapi("Boot API", semver::Version::new(1, 0, 0));
    let json = openapi.json().unwrap();
    serde_json::to_string(&json).unwrap()
}
