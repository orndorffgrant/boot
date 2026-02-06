use dropshot::{ConfigDropshot, ConfigLogging, ConfigLoggingLevel, ServerBuilder};
use dropshot::{HttpError, HttpResponseOk, RequestContext, Path, TypedBody, WebsocketUpgrade, WebsocketEndpointResult};
use probe_rs::probe;
use probe_rs::flashing;
use probe_rs::rtt::Rtt;
use base64::prelude::*;
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use std::io::{Read, Write};
use std::time::Duration;

use crate::{BootApi, Empty, Probe, Target, SerialPort, SelectorPath, PortIdPath, FlashRequest};

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

    async fn serial_ports(
        _rqctx: RequestContext<Self::Context>,
    ) -> Result<HttpResponseOk<Vec<SerialPort>>, HttpError> {
        let ports = serialport::available_ports()
            .map_err(|e| HttpError::for_internal_error(format!("Failed to enumerate serial ports: {}", e)))?;
        
        let serial_ports = ports.iter()
            .filter(|port| {
                // Filter out virtual ttyS* ports - only show USB devices
                match &port.port_type {
                    serialport::SerialPortType::UsbPort(_) => true,
                    _ => false,
                }
            })
            .map(|port| {
                // Extract a short ID from the path (last component after the final /)
                let id = port.port_name
                    .rsplit('/')
                    .next()
                    .unwrap_or(&port.port_name)
                    .to_string();
                
                // Get a friendly name from USB info if available
                let name = match &port.port_type {
                    serialport::SerialPortType::UsbPort(info) => {
                        Some(format!("{} ({})", 
                            info.product.as_deref().unwrap_or("USB Serial"),
                            info.serial_number.as_deref().unwrap_or("no serial")))
                    }
                    _ => None,
                };
                
                SerialPort {
                    id,
                    path: port.port_name.clone(),
                    name,
                }
            }).collect();
        
        Ok(HttpResponseOk(serial_ports))
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

    async fn serial_shell(
        _rqctx: RequestContext<Self::Context>,
        path: Path<PortIdPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        let port_id = path.into_inner().port_id;
        
        upgraded.handle(move |ws_stream| async move {
            // Find the serial port by ID
            let ports = serialport::available_ports()
                .map_err(|e| format!("Failed to enumerate ports: {}", e))?;
            
            let port_info = ports.iter()
                .find(|p| {
                    p.port_name.rsplit('/').next().unwrap_or(&p.port_name) == port_id
                        || p.port_name == port_id
                })
                .ok_or_else(|| format!("Port '{}' not found", port_id))?;
            
            // Open serial port
            let mut serial_port = serialport::new(&port_info.port_name, 115200)
                .timeout(std::time::Duration::from_millis(100))
                .open()
                .map_err(|e| format!("Failed to open serial port: {}", e))?;
            
            // Convert to WebSocket stream
            let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
                ws_stream.into_inner(),
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            ).await;
            
            let (mut ws_tx, mut ws_rx) = ws_stream.split();
            
            // Create a channel for serial data
            let (serial_tx, mut serial_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            
            // Spawn task to continuously read from serial and send to channel
            let port_name = port_info.port_name.clone();
            let read_handle = tokio::task::spawn_blocking(move || {
                let mut buffer = [0u8; 1024];
                loop {
                    match serial_port.read(&mut buffer) {
                        Ok(n) if n > 0 => {
                            // Send data through channel
                            if serial_tx.send(buffer[..n].to_vec()).is_err() {
                                break; // Channel closed, exit task
                            }
                        }
                        Ok(_) => continue,
                        Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                        Err(_) => break,
                    }
                }
            });
            
            // Handle bidirectional communication
            loop {
                tokio::select! {
                    // Read from serial port channel
                    Some(data) = serial_rx.recv() => {
                        if ws_tx.send(Message::Binary(data)).await.is_err() {
                            break;
                        }
                    }
                    
                    // Read from WebSocket (user input)
                    ws_msg = ws_rx.next() => {
                        match ws_msg {
                            Some(Ok(Message::Binary(data))) => {
                                if let Ok(mut port) = serialport::new(&port_name, 115200).open() {
                                    let _ = port.write_all(&data);
                                }
                            }
                            Some(Ok(Message::Text(text))) => {
                                if let Ok(mut port) = serialport::new(&port_name, 115200).open() {
                                    let _ = port.write_all(text.as_bytes());
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Err(_)) => break,
                            _ => {}
                        }
                    }
                }
            }
            
            // Clean up the read task
            read_handle.abort();
            
            Ok(())
        })
    }

    async fn probe_attach(
        _rqctx: RequestContext<Self::Context>,
        path: Path<SelectorPath>,
        upgraded: WebsocketUpgrade,
    ) -> WebsocketEndpointResult {
        let selector_str = path.into_inner().selector;
        
        upgraded.handle(move |ws_stream| async move {
            // Convert to WebSocket stream
            let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
                ws_stream.into_inner(),
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            ).await;
            
            let (mut ws_tx, mut ws_rx) = ws_stream.split();
            
            // Channel for RTT data from the blocking probe thread
            let (rtt_tx, mut rtt_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            
            // Move all probe-rs types into a blocking thread to avoid Send issues
            let poll_handle = tokio::task::spawn_blocking(move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                let to_err = |msg: String| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::new(std::io::Error::new(std::io::ErrorKind::Other, msg))
                };
                
                // Parse selector
                let selector = selector_str.parse::<probe_rs::probe::DebugProbeSelector>()
                    .map_err(|e| to_err(format!("Invalid selector: {}", e)))?;
                
                // Open probe
                let lister = probe::list::Lister::new();
                let probe = lister.open(selector)
                    .map_err(|e| to_err(format!("Failed to open probe: {}", e)))?;
                
                // Attach to probe with auto-detect
                let mut session = probe.attach(probe_rs::config::TargetSelector::Auto, probe_rs::Permissions::default())
                    .map_err(|e| to_err(format!("Failed to attach to probe: {}", e)))?;
                
                // Get core
                let mut core = session.core(0)
                    .map_err(|e| to_err(format!("Failed to access core: {}", e)))?;
                
                // Attempt to attach to RTT
                let mut rtt = Rtt::attach(&mut core)
                    .map_err(|e| to_err(format!("Failed to attach to RTT: {}", e)))?;
                
                let mut buffer = [0u8; 1024];
                loop {
                    let mut any_data = false;
                    for channel in rtt.up_channels().iter_mut() {
                        match channel.read(&mut core, &mut buffer) {
                            Ok(count) if count > 0 => {
                                any_data = true;
                                // Send raw bytes - let the client decode (defmt or UTF-8)
                                if rtt_tx.send(buffer[..count].to_vec()).is_err() {
                                    return Ok(()); // WebSocket closed
                                }
                            }
                            Ok(_) => {}
                            Err(e) => return Err(to_err(format!("RTT read error: {}", e))),
                        }
                    }
                    if !any_data {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            });
            
            // Handle bidirectional communication
            loop {
                tokio::select! {
                    rtt_msg = rtt_rx.recv() => {
                        match rtt_msg {
                            Some(data) => {
                                if ws_tx.send(Message::Binary(data)).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    
                    // Read from WebSocket (currently just monitoring for close)
                    ws_msg = ws_rx.next() => {
                        match ws_msg {
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Err(_)) => break,
                            _ => {}
                        }
                    }
                }
            }
            
            if let Ok(Err(e)) = poll_handle.await {
                return Err(e);
            }
            
            Ok(())
        })
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
