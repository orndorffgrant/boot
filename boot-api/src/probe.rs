use base64::prelude::*;
use dropshot::{
    HttpError, HttpResponseOk, Path, RequestContext, TypedBody, WebsocketEndpointResult,
    WebsocketUpgrade,
};
use futures::{SinkExt, StreamExt};
use probe_rs::flashing;
use probe_rs::probe;
use probe_rs::rtt::Rtt;
use std::io::{Read, Write};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::auth::{Auth, AuthState, Capability, Control, Principal};
use crate::{Empty, FlashRequest, Probe, SelectorAndRttPath, SelectorPath, Target};

pub async fn list_probes(
    _rqctx: RequestContext<AuthState>,
) -> Result<HttpResponseOk<Vec<Probe>>, HttpError> {
    let lister = probe::list::Lister::new();
    let probes_list = lister.list_all();
    let probes = probes_list
        .iter()
        .map(|probe| {
            Probe {
                id: "TODO".to_string(),
                name: probe.identifier.clone(),
                vendor_id: probe.vendor_id,
                product_id: probe.product_id,
                serial_number: probe.serial_number.clone(),
                hid_interface: None, // probe-rs 0.31 doesn't expose this field directly
                probe_type: probe.probe_type(),
            }
        })
        .collect();
    Ok(HttpResponseOk(probes))
}

pub async fn get_target(
    rqctx: RequestContext<AuthState>,
    path: Path<SelectorPath>,
    auth: &Auth<Control>,
) -> Result<HttpResponseOk<Target>, HttpError> {
    let selector_str = &path.into_inner().selector;
    slog::info!(
        rqctx.log,
        "control operation";
        "actor" => %auth.principal().id,
        "operation" => "get_target",
        "selector" => %selector_str,
    );

    let selector = selector_str
        .parse::<probe_rs::probe::DebugProbeSelector>()
        .map_err(|e| HttpError::for_bad_request(None, format!("Invalid selector: {}", e)))?;

    let lister = probe::list::Lister::new();
    let probe = lister
        .open(selector)
        .map_err(|e| HttpError::for_not_found(None, format!("Failed to open probe: {}", e)))?;

    // Attach to probe and auto-detect target
    let session = probe
        .attach(
            probe_rs::config::TargetSelector::Auto,
            probe_rs::Permissions::default(),
        )
        .map_err(|e| HttpError::for_internal_error(format!("Failed to attach to probe: {}", e)))?;

    let target_name = session.target().name.clone();

    Ok(HttpResponseOk(Target { name: target_name }))
}

pub async fn flash_and_run(
    rqctx: RequestContext<AuthState>,
    path: Path<SelectorPath>,
    body: TypedBody<FlashRequest>,
    auth: &Auth<Control>,
) -> Result<HttpResponseOk<Empty>, HttpError> {
    let selector_str = &path.into_inner().selector;
    slog::info!(
        rqctx.log,
        "control operation";
        "actor" => %auth.principal().id,
        "operation" => "flash",
        "selector" => %selector_str,
    );
    let flash_request = body.into_inner();

    // Decode base64 ELF data
    let elf_bytes = BASE64_STANDARD
        .decode(flash_request.elf_data.as_bytes())
        .map_err(|e| HttpError::for_bad_request(None, format!("Invalid base64 data: {}", e)))?;

    // Parse selector
    let selector = selector_str
        .parse::<probe_rs::probe::DebugProbeSelector>()
        .map_err(|e| HttpError::for_bad_request(None, format!("Invalid selector: {}", e)))?;

    // Open probe
    let lister = probe::list::Lister::new();
    let probe = lister
        .open(selector)
        .map_err(|e| HttpError::for_not_found(None, format!("Failed to open probe: {}", e)))?;

    // Attach to probe with auto-detect
    let mut session = probe
        .attach(
            probe_rs::config::TargetSelector::Auto,
            probe_rs::Permissions::default(),
        )
        .map_err(|e| HttpError::for_internal_error(format!("Failed to attach to probe: {}", e)))?;

    // Write ELF data to a temporary file (probe-rs APIs typically work with files)
    let temp_dir = std::env::temp_dir();
    let temp_elf_path = temp_dir.join(format!("boot-api-{}.elf", std::process::id()));
    std::fs::write(&temp_elf_path, &elf_bytes).map_err(|e| {
        HttpError::for_internal_error(format!("Failed to write temporary ELF file: {}", e))
    })?;

    // Flash the ELF file with default options
    let result = flashing::download_file(
        &mut session,
        &temp_elf_path,
        flashing::ElfLoader(flashing::ElfOptions::default()),
    );

    // Clean up temporary file
    let _ = std::fs::remove_file(&temp_elf_path);

    result
        .map_err(|e| HttpError::for_internal_error(format!("Failed to flash firmware: {}", e)))?;

    // Reset and run the target
    let mut core = session
        .core(0)
        .map_err(|e| HttpError::for_internal_error(format!("Failed to access core: {}", e)))?;

    core.reset()
        .map_err(|e| HttpError::for_internal_error(format!("Failed to reset core: {}", e)))?;

    Ok(HttpResponseOk(Empty {}))
}

pub fn attach(
    rqctx: RequestContext<AuthState>,
    path: Path<SelectorPath>,
    upgraded: WebsocketUpgrade,
    auth: Auth<Control>,
) -> WebsocketEndpointResult {
    let selector_str = path.into_inner().selector;
    slog::info!(
        rqctx.log,
        "control operation";
        "actor" => %auth.principal().id,
        "operation" => "attach",
        "selector" => %selector_str,
    );
    let state = rqctx.context().clone();
    attach_impl(
        selector_str,
        None,
        upgraded,
        state,
        auth.principal().clone(),
    )
}

pub fn attach_with_rtt(
    rqctx: RequestContext<AuthState>,
    path: Path<SelectorAndRttPath>,
    upgraded: WebsocketUpgrade,
    auth: Auth<Control>,
) -> WebsocketEndpointResult {
    let path_params = path.into_inner();
    slog::info!(
        rqctx.log,
        "control operation";
        "actor" => %auth.principal().id,
        "operation" => "attach_with_rtt",
        "selector" => %path_params.selector,
    );
    let state = rqctx.context().clone();
    attach_impl(
        path_params.selector,
        Some(path_params.rtt_addr),
        upgraded,
        state,
        auth.principal().clone(),
    )
}

fn attach_impl(
    selector_str: String,
    rtt_addr: Option<String>,
    upgraded: WebsocketUpgrade,
    auth_state: AuthState,
    principal: Principal,
) -> WebsocketEndpointResult {
    upgraded.handle(move |ws_stream| async move {
        // Convert to WebSocket stream
        let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
            ws_stream.into_inner(),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;

        let (mut ws_tx, mut ws_rx) = ws_stream.split();
        let mut revalidate = tokio::time::interval(Duration::from_secs(5));
        revalidate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // Channel for RTT data from the blocking probe thread
        let (rtt_tx, mut rtt_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        // Move all probe-rs types into a blocking thread to avoid Send issues
        let poll_handle = tokio::task::spawn_blocking(
            move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                let to_err = |msg: String| -> Box<dyn std::error::Error + Send + Sync> {
                    Box::new(std::io::Error::new(std::io::ErrorKind::Other, msg))
                };

                // Parse selector
                let selector = selector_str
                    .parse::<probe_rs::probe::DebugProbeSelector>()
                    .map_err(|e| to_err(format!("Invalid selector: {}", e)))?;

                // Open probe
                let lister = probe::list::Lister::new();
                let probe = lister
                    .open(selector)
                    .map_err(|e| to_err(format!("Failed to open probe: {}", e)))?;

                // Attach to probe with auto-detect
                let mut session = probe
                    .attach(
                        probe_rs::config::TargetSelector::Auto,
                        probe_rs::Permissions::default(),
                    )
                    .map_err(|e| to_err(format!("Failed to attach to probe: {}", e)))?;

                // Get core
                let mut core = session
                    .core(0)
                    .map_err(|e| to_err(format!("Failed to access core: {}", e)))?;

                // Attempt to attach to RTT
                let mut rtt = if let Some(addr_str) = rtt_addr {
                    // Parse the hex address
                    let addr = if addr_str.starts_with("0x") || addr_str.starts_with("0X") {
                        u64::from_str_radix(&addr_str[2..], 16)
                    } else {
                        u64::from_str_radix(&addr_str, 16)
                    }
                    .map_err(|e| to_err(format!("Invalid RTT address '{}': {}", addr_str, e)))?;

                    eprintln!("Attaching to RTT at 0x{:x}...", addr);
                    let scan_start = std::time::Instant::now();
                    let rtt = Rtt::attach_at(&mut core, addr).map_err(|e| {
                        to_err(format!("Failed to attach to RTT at 0x{:x}: {}", addr, e))
                    })?;
                    eprintln!("RTT attached in {:?}", scan_start.elapsed());
                    rtt
                } else {
                    // Scan for RTT control block (slow for large RAM)
                    eprintln!("Scanning for RTT control block...");
                    let scan_start = std::time::Instant::now();
                    let rtt = Rtt::attach(&mut core)
                        .map_err(|e| to_err(format!("Failed to attach to RTT: {}", e)))?;
                    eprintln!("RTT attached in {:?}", scan_start.elapsed());
                    rtt
                };

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
            },
        );

        // Handle bidirectional communication
        loop {
            tokio::select! {
                _ = revalidate.tick() => {
                    if !auth_state.is_authorized(&principal, Capability::Control).await {
                        let _ = ws_tx
                            .send(Message::Close(Some(CloseFrame {
                                code: CloseCode::Policy,
                                reason: "token revoked or expired".into(),
                            })))
                            .await;
                        break;
                    }
                }

                rtt_msg = rtt_rx.recv() => {
                    match rtt_msg {
                        Some(data) => {
                            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
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

        // Clean up: drop the receiver to signal the blocking thread to stop
        drop(rtt_rx);

        // Abort the blocking task to ensure it stops immediately
        poll_handle.abort();

        // Give it a moment to clean up, but don't wait forever
        let _ = tokio::time::timeout(Duration::from_secs(1), poll_handle).await;

        Ok(())
    })
}

pub fn dap_server(
    rqctx: RequestContext<AuthState>,
    path: Path<SelectorPath>,
    upgraded: WebsocketUpgrade,
    auth: Auth<Control>,
) -> WebsocketEndpointResult {
    let selector_str = path.into_inner().selector;
    slog::info!(
        rqctx.log,
        "control operation";
        "actor" => %auth.principal().id,
        "operation" => "dap",
        "selector" => %selector_str,
    );
    let auth_state = rqctx.context().clone();
    let principal = auth.principal().clone();

    if !crate::probe_dap::DAP_SUPPORTED {
        return Err(HttpError::for_internal_error(
            "DAP support is not implemented yet".to_string(),
        ));
    }

    upgraded.handle(move |ws_stream| async move {
        // Convert to WebSocket stream
        let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
            ws_stream.into_inner(),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        ).await;

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        // Create in-memory pipes for DAP communication
        let (dap_reader, mut pipe_writer) = os_pipe::pipe()
            .map_err(|e| anyhow::anyhow!("Failed to create pipe: {}", e))?;
        let (pipe_reader, dap_writer) = os_pipe::pipe()
            .map_err(|e| anyhow::anyhow!("Failed to create pipe: {}", e))?;

        // Channel for coordinating shutdown
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // Spawn DAP server in a blocking thread (probe-rs types are not Send)
        let dap_handle = tokio::task::spawn_blocking(move || -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            let lister = probe::list::Lister::new();

            // Run the DAP session using probe-rs library
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(crate::probe_dap::run_dap_session(
                dap_reader,
                dap_writer,
                &lister,
                None,  // No log file
            ))?;

            Ok(())
        });

        // Spawn task to forward WebSocket -> DAP (pipe writer)
        let ws_to_dap_handle = tokio::task::spawn(async move {
            while let Some(msg) = ws_rx.next().await {
                match msg {
                    Ok(Message::Binary(data)) => {
                        // Forward binary WebSocket message to DAP pipe
                        if pipe_writer.write_all(&data).is_err() {
                            break;
                        }
                        if pipe_writer.flush().is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            let _ = shutdown_tx.send(());
        });

        // Spawn task to forward DAP (pipe reader) -> WebSocket
        let dap_to_ws_handle = tokio::task::spawn(async move {
            loop {
                tokio::select! {
                    // Try to read from DAP pipe
                    read_result = tokio::task::spawn_blocking({
                        let mut pipe_reader = pipe_reader.try_clone().unwrap();
                        move || {
                            let mut buffer = vec![0u8; 4096];
                            let n = pipe_reader.read(&mut buffer)?;
                            std::io::Result::Ok((buffer, n))
                        }
                    }) => {
                        match read_result {
                            Ok(Ok((buffer, n))) if n > 0 => {
                                // Forward to WebSocket
                                if ws_tx.send(Message::Binary(buffer[..n].to_vec().into())).await.is_err() {
                                    break;
                                }
                            }
                            _ => break,
                        }
                    }

                    // Check for shutdown signal
                    _ = &mut shutdown_rx => {
                        break;
                    }
                }
            }
        });

        // Wait for either the DAP server to exit or the WebSocket to close.
        // Revalidate the token periodically so revocation closes DAP sessions.
        let mut revalidate = tokio::time::interval(Duration::from_secs(5));
        revalidate.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut dap_handle = dap_handle;
        let mut ws_to_dap_handle = ws_to_dap_handle;
        let mut dap_to_ws_handle = dap_to_ws_handle;
        loop {
            tokio::select! {
                _ = revalidate.tick() => {
                    if !auth_state.is_authorized(&principal, Capability::Control).await {
                        dap_handle.abort();
                        ws_to_dap_handle.abort();
                        dap_to_ws_handle.abort();
                        break;
                    }
                }
                _ = &mut dap_handle => break,
                _ = &mut ws_to_dap_handle => break,
                _ = &mut dap_to_ws_handle => break,
            }
        }

        Ok(())
    })
}
