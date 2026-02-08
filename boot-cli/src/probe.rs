use anyhow::Result;
use base64::Engine;
use clap::Args;
use comfy_table::{Table, presets::NOTHING};
use indicatif::{ProgressBar, ProgressStyle};
use futures::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use crate::server_config;

#[derive(Args)]
pub struct ProbeRunArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    pub server_and_selector: String,
    /// Path to the ELF file
    pub elf_path: String,
    /// Don't attach and show defmt logs after flashing
    #[arg(long)]
    pub no_attach: bool,
    /// Open serial shell instead of attaching (format: {server-name}/{port-id})
    #[arg(short = 's', long = "serial-shell")]
    pub serial_shell: Option<String>,
}

#[derive(Args)]
pub struct ProbeAttachArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    pub server_and_selector: String,
    /// Optional path to the ELF file for defmt decoding
    #[arg(long)]
    pub elf: Option<String>,
}

pub async fn list_probes() -> Result<()> {
    let config = server_config::ServerConfig::load()?;
    let servers = config.list_servers();
    
    if servers.is_empty() {
        println!("No servers configured.");
        return Ok(());
    }
    
    let mut table = Table::new();
    table.load_preset(NOTHING);
    
    for server in servers {
        let client = boot_api_client::Client::new(&server.url);
        match client.probes().await {
            std::result::Result::Ok(response) => {
                let probes = response.into_inner();
                // Skip if no probes found
                if !probes.is_empty() {
                    for probe in probes {
                        let selector = format!(
                            "{:04x}:{:04x}:{}",
                            probe.vendor_id,
                            probe.product_id,
                            probe.serial_number
                                .as_ref()
                                .map(|s| format!("{}", s))
                                .unwrap_or_default()
                        );
                        table.add_row(vec![&server.name, &selector, &probe.name]);
                    }
                }
            }
            std::result::Result::Err(e) => {
                // Print errors to stderr
                eprintln!("Error connecting to server '{}' ({}): {}", server.name, server.url, e);
            }
        }
    }
    
    println!("{table}");
    Ok(())
}

fn extract_rtt_address(elf_bytes: &[u8]) -> Option<u64> {
    use object::{Object, ObjectSymbol};
    
    let elf_file = object::File::parse(elf_bytes).ok()?;
    
    // Look for the RTT control block symbol (usually _SEGGER_RTT)
    for symbol in elf_file.symbols() {
        if let std::result::Result::Ok(name) = symbol.name() {
            if name == "_SEGGER_RTT" {
                return Some(symbol.address());
            }
        }
    }
    
    None
}

async fn upgraded_to_websocket(
    upgraded: reqwest::Upgraded,
) -> tokio_tungstenite::WebSocketStream<reqwest::Upgraded> {
    // reqwest::Upgraded implements AsyncRead + AsyncWrite, so we can use it directly
    use tokio_tungstenite::WebSocketStream;
    
    // Convert the upgraded connection to a WebSocketStream
    // Note: The WebSocket handshake was already done by reqwest, so we create the stream directly
    WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    ).await
}

pub async fn probe_run(args: ProbeRunArgs) -> Result<()> {
    // Parse server name and selector
    let parts: Vec<&str> = args.server_and_selector.split('/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid format. Expected {{server-name}}/{{debugprobeselector}}");
    }
    let server_name = parts[0];
    let selector = parts[1];
    
    // Load server configuration
    let config = server_config::ServerConfig::load()?;
    let server = config.list_servers()
        .iter()
        .find(|s| s.name == server_name)
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
    
    // Create spinner for flashing
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap()
    );
    spinner.set_message(format!("Reading {}...", args.elf_path));
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));
    
    // Read ELF file
    let elf_bytes = std::fs::read(&args.elf_path)
        .map_err(|e| {
            spinner.finish_and_clear();
            anyhow::anyhow!("Failed to read ELF file '{}': {}", args.elf_path, e)
        })?;
    
    spinner.set_message("Encoding firmware...");
    
    // Encode to base64
    let elf_data = base64::prelude::BASE64_STANDARD.encode(&elf_bytes);
    
    spinner.set_message(format!("Flashing to {}...", selector));
    
    // Create client with longer timeout for large file transfers
    let reqwest_client = reqwest::ClientBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let client = boot_api_client::Client::new_with_client(&server.url, reqwest_client);
    
    client.flash_and_run(selector, &boot_api_client::types::FlashRequest { elf_data })
        .await
        .map_err(|e| {
            spinner.finish_and_clear();
            anyhow::anyhow!("Failed to flash firmware: {:#}", e)
        })?;
    
    spinner.finish_with_message(format!("✓ Flashed firmware ({} bytes)", elf_bytes.len()));
    
    // Handle post-flash action
    if let Some(serial_shell) = args.serial_shell {
        // Open serial shell instead of attaching
        println!();
        let serial_args = crate::serial_port::SerialShellArgs {
            server_and_port: serial_shell,
        };
        crate::serial_port::serial_shell(serial_args).await?;
    } else if !args.no_attach {
        // Attach by default unless --no-attach flag is set
        println!();
        let attach_args = ProbeAttachArgs {
            server_and_selector: args.server_and_selector.clone(),
            elf: Some(args.elf_path),
        };
        probe_attach(attach_args).await?;
    }
    
    Ok(())
}

pub async fn probe_attach(args: ProbeAttachArgs) -> Result<()> {
    use defmt_decoder::Table as DefmtTable;
    
    // Parse server name and selector
    let parts: Vec<&str> = args.server_and_selector.split('/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid format. Expected {{server-name}}/{{debugprobeselector}}");
    }
    let server_name = parts[0];
    let selector = parts[1];
    
    // Load server configuration
    let config = server_config::ServerConfig::load()?;
    let server = config.list_servers()
        .iter()
        .find(|s| s.name == server_name)
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
    
    // If ELF file is provided, load it and create defmt decoder
    let defmt_table = if let Some(elf_path) = &args.elf {
        let elf_bytes = std::fs::read(elf_path)
            .map_err(|e| anyhow::anyhow!("Failed to read ELF file '{}': {}", elf_path, e))?;
        
        let table = DefmtTable::parse(&elf_bytes)?
            .ok_or_else(|| anyhow::anyhow!("No defmt data found in ELF file"))?;
        
        Some((table, elf_bytes))
    } else {
        None
    };
    
    let mut defmt_decoder = defmt_table.as_ref().map(|(table, _)| table.new_stream_decoder());
    let defmt_locations = defmt_table.as_ref().and_then(|(table, elf_bytes)| {
        table.get_locations(elf_bytes).ok()
    });
    
    // Create spinner for RTT attachment
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap()
    );
    spinner.set_message("Loading ELF file...");
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));
    
    // Extract RTT address if we have the ELF file
    let rtt_addr = defmt_table.as_ref().and_then(|(_, elf_bytes)| extract_rtt_address(elf_bytes));
    
    if rtt_addr.is_some() {
        spinner.set_message("Finding RTT control block...");
    } else {
        spinner.set_message("Scanning for RTT control block...");
    }
    
    spinner.set_message(format!("Attaching to {}...", selector));
    
    // Use the generated client to connect
    let client = boot_api_client::Client::new(&server.url);
    
    let upgraded = if let Some(addr) = rtt_addr {
        // Use the endpoint with RTT address path parameter
        client.probe_attach_with_rtt(selector, &format!("0x{:x}", addr)).await
    } else {
        // Use the endpoint without RTT address
        client.probe_attach(selector).await
    }
    .map_err(|e| {
        spinner.finish_and_clear();
        anyhow::anyhow!("Failed to attach to probe: {:#}", e)
    })?
    .into_inner();
    
    // Convert the upgraded connection to a WebSocket
    let ws_stream = upgraded_to_websocket(upgraded).await;
    
    let encoding = if args.elf.is_some() { "defmt" } else { "UTF-8" };
    spinner.finish_with_message(format!("✓ Attached to {}/{} RTT ({})", server_name, selector, encoding));
    println!();
    
    let (_ws_tx, mut ws_rx) = ws_stream.split();
    
    // Read from WebSocket and write to stdout
    use tokio::io::{AsyncWriteExt, stdout};
    let mut stdout = stdout();
    
    while let Some(msg) = ws_rx.next().await {
        match msg {
            std::result::Result::Ok(Message::Binary(data)) => {
                if let Some(stream_decoder) = &mut defmt_decoder {
                    // Defmt decoding
                    stream_decoder.received(&data);
                    
                    loop {
                        match stream_decoder.decode() {
                            Result::Ok(frame) => {
                                // Format and print the decoded frame
                                let (file, line, module) = defmt_locations.as_ref()
                                    .and_then(|locs| locs.get(&frame.index()))
                                    .map(|location| {
                                        let file = location.file.display().to_string();
                                        let line = location.line as u32;
                                        let module = location.module.clone();
                                        (file, line, module)
                                    })
                                    .unwrap_or_else(|| ("unknown".to_string(), 0, String::new()));
                                
                                let timestamp = if let Some(ts) = frame.display_timestamp() {
                                    format!("{} ", ts)
                                } else {
                                    String::new()
                                };
                                
                                use defmt_parser::Level;
                                let level = match frame.level() {
                                    Some(Level::Trace) => "TRACE",
                                    Some(Level::Debug) => "DEBUG",
                                    Some(Level::Info) => "INFO",
                                    Some(Level::Warn) => "WARN",
                                    Some(Level::Error) => "ERROR",
                                    None => "",
                                };
                                
                                let output = if !module.is_empty() {
                                    format!("{}{} {}:{} > {}\n", timestamp, level, module, line, frame.display_message())
                                } else {
                                    format!("{}{} {}:{} > {}\n", timestamp, level, file, line, frame.display_message())
                                };
                                
                                stdout.write_all(output.as_bytes()).await?;
                                stdout.flush().await?;
                            }
                            Result::Err(defmt_decoder::DecodeError::UnexpectedEof) => break,
                            Result::Err(defmt_decoder::DecodeError::Malformed) => {
                                eprintln!("Warning: malformed defmt frame");
                                break;
                            }
                        }
                    }
                } else {
                    // UTF-8 decoding fallback
                    let text = String::from_utf8_lossy(&data);
                    stdout.write_all(text.as_bytes()).await?;
                    stdout.flush().await?;
                }
            }
            std::result::Result::Ok(Message::Text(text)) => {
                stdout.write_all(text.as_bytes()).await?;
                stdout.flush().await?;
            }
            std::result::Result::Ok(Message::Close(_)) => break,
            Err(_) => break,
            _ => {}
        }
    }
    
    println!("\nConnection closed");
    
    Ok(())
}
