use dropshot::{ConfigDropshot, ConfigLogging, ConfigLoggingLevel, ServerBuilder};
use dropshot::{HttpError, HttpResponseOk, RequestContext};
use probe_rs::probe;

use crate::{BootApi, Empty, Programmer};

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

pub async fn run(port: Option<u16>, host: Option<String>) -> anyhow::Result<()> {
    let port = port.unwrap_or(46673);
    let host = host.unwrap_or_else(|| "127.0.0.1".to_string());
    let bind_address = format!("{}:{}", host, port);
    
    let log = ConfigLogging::StderrTerminal {
        level: ConfigLoggingLevel::Info,
    }
    .to_logger("boot-api")?;
    
    let api = crate::boot_api_mod::api_description::<ServerImpl>().unwrap();
    let server = ServerBuilder::new(api, (), log).config(ConfigDropshot{bind_address: bind_address.parse().unwrap(), ..Default::default()}).start()?;
    server.await.map_err(|e| anyhow::Error::msg(e))
}
