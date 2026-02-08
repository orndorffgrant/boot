use dropshot::{ConfigDropshot, ConfigLogging, ConfigLoggingLevel, ServerBuilder};
use dropshot::{HttpError, HttpResponseOk, RequestContext, Path, TypedBody, WebsocketUpgrade, WebsocketEndpointResult};

use crate::{BootApi, Empty, Probe, Target, SerialPort, SelectorPath, SelectorAndRttPath, PortIdPath, FlashRequest};

enum ServerImpl {}

impl BootApi for ServerImpl {
    type Context = ();

    async fn health(
        _rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        Ok(HttpResponseOk(Empty{}))
    }

    async fn probes(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Vec<Probe>>, HttpError> {
        crate::probe::list_probes(rqctx).await
    }

    async fn serial_ports(
        rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Vec<SerialPort>>, HttpError> {
        crate::serial_port::list_serial_ports(rqctx).await
    }

    async fn probes_target(
        rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
    ) -> Result<HttpResponseOk<Target>, HttpError> {
        crate::probe::get_target(rqctx, path).await
    }

    async fn flash_and_run(
        rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
        body: TypedBody<FlashRequest>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        crate::probe::flash_and_run(rqctx, path, body).await
    }

    async fn serial_shell(
        rqctx: RequestContext<Self::Context>,
        path: Path<PortIdPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        crate::serial_port::serial_shell(rqctx, path, upgraded)
    }

    async fn probe_attach(
        rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        crate::probe::attach(rqctx, path, upgraded)
    }

    async fn probe_attach_with_rtt(
        rqctx: RequestContext<Self::Context>,
        path: Path<SelectorAndRttPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        crate::probe::attach_with_rtt(rqctx, path, upgraded)
    }
}

pub async fn run(host: Option<String>, port: Option<u16>) -> anyhow::Result<()> {
    let host = host.unwrap_or_else(|| "127.0.0.1".to_string());
    let port = port.unwrap_or(46673);
    let bind_address = format!("{}:{}", host, port);
    
    let log = ConfigLogging::StderrTerminal {
        level: ConfigLoggingLevel::Info,
    }
    .to_logger("boot-api")?;
    
    let api = crate::boot_api_mod::api_description::<ServerImpl>().unwrap();
    let server = ServerBuilder::new(api, (), log).config(ConfigDropshot{
        bind_address: bind_address.parse().unwrap(),
        ..Default::default()
    }).start()?;
    server.await.map_err(|e| anyhow::Error::msg(e))
}
