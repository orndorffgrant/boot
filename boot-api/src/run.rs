use dropshot::{ConfigDropshot, ConfigLogging, ConfigLoggingLevel, ServerBuilder};
use dropshot::{HttpError, HttpResponseOk, RequestContext, Path, TypedBody};
use probe_rs::probe;
use probe_rs::flashing;
use base64::prelude::*;

use crate::{BootApi, Empty, Probe, Target, SelectorPath, FlashRequest};

enum ServerImpl {}

impl BootApi for ServerImpl {
    type Context = ();

    async fn health(
        _rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        Ok(HttpResponseOk(Empty{}))
    }

    async fn probes(
        _rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Vec<Probe>>, HttpError> {
        let lister = probe::list::Lister::new();
        let probes_list = lister.list_all();
        let probes = probes_list.iter().map(|probe| {
            Probe {
                id: "TODO".to_string(),
                name: probe.identifier.clone(),
                vendor_id: probe.vendor_id,
                product_id: probe.product_id,
                serial_number: probe.serial_number.clone(),
                hid_interface: probe.hid_interface,
                probe_type: probe.probe_type(),
            }
        }).collect();
        Ok(HttpResponseOk(probes))
    }

    async fn probes_target(
        _rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
    ) -> Result<HttpResponseOk<Target>, HttpError> {
        let selector_str = &path.into_inner().selector;
        
        let selector = selector_str.parse::<probe_rs::probe::DebugProbeSelector>()
            .map_err(|e| HttpError::for_bad_request(None, format!("Invalid selector: {}", e)))?;
        
        let lister = probe::list::Lister::new();
        let probe = lister.open(selector)
            .map_err(|e| HttpError::for_not_found(None, format!("Failed to open probe: {}", e)))?;
        
        // Attach to probe and auto-detect target
        let session = probe.attach(probe_rs::config::TargetSelector::Auto, probe_rs::Permissions::default())
            .map_err(|e| HttpError::for_internal_error(format!("Failed to attach to probe: {}", e)))?;
        
        let target_name = session.target().name.clone();
        
        Ok(HttpResponseOk(Target { name: target_name }))
    }

    async fn flash_and_run(
        _rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
        body: TypedBody<FlashRequest>,
    ) -> Result<HttpResponseOk<Empty>, HttpError> {
        let selector_str = &path.into_inner().selector;
        let flash_request = body.into_inner();
        
        // Decode base64 ELF data
        let elf_bytes = BASE64_STANDARD
            .decode(flash_request.elf_data.as_bytes())
            .map_err(|e| HttpError::for_bad_request(None, format!("Invalid base64 data: {}", e)))?;
        
        // Parse selector
        let selector = selector_str.parse::<probe_rs::probe::DebugProbeSelector>()
            .map_err(|e| HttpError::for_bad_request(None, format!("Invalid selector: {}", e)))?;
        
        // Open probe
        let lister = probe::list::Lister::new();
        let probe = lister.open(selector)
            .map_err(|e| HttpError::for_not_found(None, format!("Failed to open probe: {}", e)))?;
        
        // Attach to probe with auto-detect
        let mut session = probe.attach(probe_rs::config::TargetSelector::Auto, probe_rs::Permissions::default())
            .map_err(|e| HttpError::for_internal_error(format!("Failed to attach to probe: {}", e)))?;
        
        // Write ELF data to a temporary file (probe-rs APIs typically work with files)
        let temp_dir = std::env::temp_dir();
        let temp_elf_path = temp_dir.join(format!("boot-api-{}.elf", std::process::id()));
        std::fs::write(&temp_elf_path, &elf_bytes)
            .map_err(|e| HttpError::for_internal_error(format!("Failed to write temporary ELF file: {}", e)))?;
        
        // Flash the ELF file with default options
        let result = flashing::download_file(
            &mut session, 
            &temp_elf_path, 
            flashing::Format::Elf(Default::default())
        );
        
        // Clean up temporary file
        let _ = std::fs::remove_file(&temp_elf_path);
        
        result.map_err(|e| HttpError::for_internal_error(format!("Failed to flash firmware: {}", e)))?;
        
        // Reset and run the target
        let mut core = session.core(0)
            .map_err(|e| HttpError::for_internal_error(format!("Failed to access core: {}", e)))?;
        
        core.reset()
            .map_err(|e| HttpError::for_internal_error(format!("Failed to reset core: {}", e)))?;
        
        Ok(HttpResponseOk(Empty{}))
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
