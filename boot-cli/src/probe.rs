use anyhow::Result;
use boot_protocol::{ChangeWsMessage, SessionWsMessage};
use clap::Args;
use comfy_table::{Table, presets::NOTHING};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use tokio_tungstenite::tungstenite::Message;

use crate::auth::{api_client, api_client_with_timeout, map_error};
use crate::cli_db::CliDb;

#[derive(Args)]
pub struct RunFirmwareArgs {
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
pub struct AttachProbeArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    pub server_and_selector: String,
    /// Optional path to the ELF file for defmt decoding
    #[arg(long)]
    pub elf: Option<String>,
    /// Attach to an existing session only
    #[arg(long, conflicts_with = "new")]
    pub follow: bool,
    /// Create a new session only
    #[arg(long)]
    pub new: bool,
    /// Attach read-only
    #[arg(long)]
    pub read_only: bool,
    /// Attach to a specific session ID
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Args)]
pub struct StartDapArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    pub server_and_selector: String,
    /// Port to listen on for DAP connections (default: 50000)
    #[arg(short, long, default_value = "50000")]
    pub port: u16,
}

pub async fn list_probes(cli_db: Option<&std::path::Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    let servers = config.list_servers().await?;

    if servers.is_empty() {
        println!("No servers configured.");
        return Ok(());
    }

    let mut table = Table::new();
    table.load_style(NOTHING);

    for server in servers {
        let client = match api_client(&server) {
            Ok(client) => client,
            Err(error) => {
                eprintln!(
                    "Error connecting to server '{}' ({}): {}",
                    server.name, server.url, error
                );
                continue;
            }
        };
        match client.probes().await {
            Ok(response) => {
                for probe in response.into_inner() {
                    let selector = format!(
                        "{:04x}:{:04x}:{}",
                        probe.vendor_id,
                        probe.product_id,
                        probe.serial_number.as_deref().unwrap_or_default()
                    );
                    table.add_row(vec![&server.name, &probe.id, &selector, &probe.name]);
                }
            }
            Err(error) => {
                let error = map_error(&server, error.status(), &format_args!("{error:#}"));
                eprintln!(
                    "Error connecting to server '{}' ({}): {}",
                    server.name, server.url, error
                );
            }
        }
    }

    println!("{table}");
    Ok(())
}

fn extract_rtt_address(elf_bytes: &[u8]) -> Option<u64> {
    use object::{Object, ObjectSymbol};
    let elf_file = object::File::parse(elf_bytes).ok()?;
    for symbol in elf_file.symbols() {
        if let Ok(name) = symbol.name() {
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
    use tokio_tungstenite::WebSocketStream;
    WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await
}

fn split_server_resource(value: &str) -> Result<(&str, &str)> {
    let parts: Vec<&str> = value.splitn(2, '/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        anyhow::bail!("Invalid format. Expected {{server-name}}/{{resource}}");
    }
    Ok((parts[0], parts[1]))
}

async fn resolve_probe_id(client: &boot_api_client::Client, selector: &str) -> Result<String> {
    let probes = client.probes().await?.into_inner();
    if let Some(probe) = probes.iter().find(|probe| probe.id == selector) {
        return Ok(probe.id.clone());
    }
    if let Some(probe) = probes.iter().find(|probe| {
        format!(
            "{:04x}:{:04x}:{}",
            probe.vendor_id,
            probe.product_id,
            probe.serial_number.as_deref().unwrap_or_default()
        ) == selector
            || probe.name == selector
    }) {
        return Ok(probe.id.clone());
    }
    anyhow::bail!("Probe '{}' not found", selector)
}

async fn find_active_session(
    client: &boot_api_client::Client,
    resource_id: &str,
    kind: &str,
) -> Result<Option<boot_api_client::types::Session>> {
    let sessions = client.list_sessions(Some("active")).await?.into_inner();
    Ok(sessions
        .into_iter()
        .find(|session| session.resource_id == resource_id && session.kind == kind))
}

pub async fn run_firmware(args: RunFirmwareArgs, cli_db: Option<&std::path::Path>) -> Result<()> {
    let (server_name, selector) = split_server_resource(&args.server_and_selector)?;
    let config = CliDb::open_default(cli_db).await?;
    let server = config
        .get_server(server_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    spinner.set_message(format!("Reading {}...", args.elf_path));
    spinner.enable_steady_tick(std::time::Duration::from_millis(100));

    let elf_bytes = std::fs::read(&args.elf_path).map_err(|error| {
        spinner.finish_and_clear();
        anyhow::anyhow!("Failed to read ELF file '{}': {}", args.elf_path, error)
    })?;
    spinner.set_message("Uploading firmware...");
    let client = api_client_with_timeout(&server, std::time::Duration::from_secs(120))?;
    let probe_id = resolve_probe_id(&client, selector).await.map_err(|error| {
        spinner.finish_and_clear();
        error
    })?;
    let artifact = client
        .create_artifact(elf_bytes.clone())
        .await
        .map_err(|error| {
            spinner.finish_and_clear();
            map_error(&server, error.status(), &format_args!("{error:#}"))
        })?
        .into_inner();

    spinner.set_message(format!("Flashing to {}...", selector));
    let created = client
        .flash_probe(
            &probe_id,
            &boot_api_client::types::FlashRequest {
                artifact: artifact.id.clone(),
                options: boot_api_client::types::FlashOptions {
                    reset: true,
                    verify: true,
                },
            },
        )
        .await
        .map_err(|error| {
            spinner.finish_and_clear();
            map_error(&server, error.status(), &format_args!("{error:#}"))
        })?
        .into_inner();

    let upgraded = client
        .follow_change(&created.change, None, None)
        .await
        .map_err(|error| {
            spinner.finish_and_clear();
            map_error(&server, error.status(), &format_args!("{error:#}"))
        })?
        .into_inner();
    let ws_stream = upgraded_to_websocket(upgraded).await;
    let (_tx, mut rx) = ws_stream.split();
    while let Some(message) = rx.next().await {
        let Message::Text(text) = message? else {
            continue;
        };
        let update: ChangeWsMessage = serde_json::from_str(&text)?;
        match update {
            ChangeWsMessage::Hello { change, .. } => {
                spinner.set_message(format!("{:?}: {}", change.status, change.summary));
            }
            ChangeWsMessage::TaskUpdated { task, .. } => {
                spinner.set_message(format!("{}: {}", task.summary, task.status));
            }
            ChangeWsMessage::ChangeCompleted { change, .. } => {
                if change.status == "done" {
                    spinner.finish_with_message(format!(
                        "✓ Flashed firmware ({} bytes)",
                        elf_bytes.len()
                    ));
                } else {
                    spinner.finish_and_clear();
                    anyhow::bail!(
                        "Flash failed: {}",
                        change
                            .error
                            .map(|error| error.message)
                            .unwrap_or_else(|| "unknown error".to_string())
                    );
                }
                break;
            }
            _ => {}
        }
    }

    if let Some(serial_shell) = args.serial_shell {
        println!();
        crate::serial_port::attach_serial(
            crate::serial_port::AttachSerialArgs {
                server_and_port: serial_shell,
                follow: false,
                new: true,
                read_only: false,
                session: None,
            },
            cli_db,
        )
        .await?;
    } else if !args.no_attach {
        println!();
        attach_probe(
            AttachProbeArgs {
                server_and_selector: args.server_and_selector,
                elf: Some(args.elf_path),
                follow: false,
                new: true,
                read_only: false,
                session: None,
            },
            cli_db,
        )
        .await?;
    }

    Ok(())
}

pub async fn attach_probe(args: AttachProbeArgs, cli_db: Option<&std::path::Path>) -> Result<()> {
    use defmt_decoder::Table as DefmtTable;

    let (server_name, selector) = split_server_resource(&args.server_and_selector)?;
    let config = CliDb::open_default(cli_db).await?;
    let server = config
        .get_server(server_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;

    let defmt_table = if let Some(elf_path) = &args.elf {
        let elf_bytes = std::fs::read(elf_path).map_err(|error| {
            anyhow::anyhow!("Failed to read ELF file '{}': {}", elf_path, error)
        })?;
        let table = DefmtTable::parse(&elf_bytes)?
            .ok_or_else(|| anyhow::anyhow!("No defmt data found in ELF file"))?;
        Some((table, elf_bytes))
    } else {
        None
    };
    let mut defmt_decoder = defmt_table
        .as_ref()
        .map(|(table, _)| table.new_stream_decoder());
    let defmt_locations = defmt_table
        .as_ref()
        .and_then(|(table, elf_bytes)| table.get_locations(elf_bytes).ok());

    let client = api_client(&server)?;
    let probe_id = resolve_probe_id(&client, selector).await?;
    let session_id = if let Some(session) = &args.session {
        session.clone()
    } else if let Some(existing) = find_active_session(&client, &probe_id, "rtt").await? {
        if args.new {
            anyhow::bail!("Probe already has an active session: {}", existing.id);
        }
        existing.id
    } else {
        if args.follow {
            anyhow::bail!("No active RTT session for probe '{}'", selector);
        }
        let rtt_addr = defmt_table
            .as_ref()
            .and_then(|(_, elf_bytes)| extract_rtt_address(elf_bytes))
            .map(|address| format!("0x{address:x}"));
        client
            .create_probe_session(
                &probe_id,
                &boot_api_client::types::ProbeSessionRequest {
                    kind: "rtt".to_string(),
                    options: boot_api_client::types::SessionOptions {
                        rtt_addr,
                        channels: Vec::new(),
                        baud_rate: None,
                    },
                    max_subscribers: 8,
                    detach_timeout_seconds: 60,
                    wait_ms: 5000,
                },
            )
            .await
            .map_err(|error| map_error(&server, error.status(), &format_args!("{error:#}")))?
            .into_inner()
            .id
    };

    println!("Attaching to {}...", session_id);
    let upgraded = client
        .session_io(
            &session_id,
            None,
            None,
            Some(if args.read_only { "read" } else { "write" }),
            None,
        )
        .await
        .map_err(|error| map_error(&server, error.status(), &format_args!("{error:#}")))?
        .into_inner();
    let ws_stream = upgraded_to_websocket(upgraded).await;
    let (_ws_tx, mut ws_rx) = ws_stream.split();

    let encoding = if args.elf.is_some() { "defmt" } else { "UTF-8" };
    println!(
        "✓ Attached to {}/{} RTT ({})",
        server_name, selector, encoding
    );
    println!();

    use tokio::io::{AsyncWriteExt, stdout};
    let mut stdout = stdout();
    while let Some(message) = ws_rx.next().await {
        match message? {
            Message::Binary(data) => {
                let data = if data.len() >= 9 {
                    &data[9..]
                } else {
                    continue;
                };
                if let Some(stream_decoder) = &mut defmt_decoder {
                    stream_decoder.received(data);
                    loop {
                        match stream_decoder.decode() {
                            Ok(frame) => {
                                let (file, line, module) = defmt_locations
                                    .as_ref()
                                    .and_then(|locations| locations.get(&frame.index()))
                                    .map(|location| {
                                        (
                                            location.file.display().to_string(),
                                            location.line as u32,
                                            location.module.clone(),
                                        )
                                    })
                                    .unwrap_or_else(|| ("unknown".into(), 0, String::new()));
                                let timestamp = frame
                                    .display_timestamp()
                                    .map(|timestamp| format!("{timestamp} "))
                                    .unwrap_or_default();
                                let level = match frame.level() {
                                    Some(defmt_parser::Level::Trace) => "TRACE",
                                    Some(defmt_parser::Level::Debug) => "DEBUG",
                                    Some(defmt_parser::Level::Info) => "INFO",
                                    Some(defmt_parser::Level::Warn) => "WARN",
                                    Some(defmt_parser::Level::Error) => "ERROR",
                                    None => "",
                                };
                                let output = if !module.is_empty() {
                                    format!(
                                        "{}{} {}:{} > {}\n",
                                        timestamp,
                                        level,
                                        module,
                                        line,
                                        frame.display_message()
                                    )
                                } else {
                                    format!(
                                        "{}{} {}:{} > {}\n",
                                        timestamp,
                                        level,
                                        file,
                                        line,
                                        frame.display_message()
                                    )
                                };
                                stdout.write_all(output.as_bytes()).await?;
                                stdout.flush().await?;
                            }
                            Err(defmt_decoder::DecodeError::UnexpectedEof) => break,
                            Err(defmt_decoder::DecodeError::Malformed) => {
                                eprintln!("Warning: malformed defmt frame");
                                break;
                            }
                        }
                    }
                } else {
                    stdout
                        .write_all(&String::from_utf8_lossy(data).into_owned().into_bytes())
                        .await?;
                    stdout.flush().await?;
                }
            }
            Message::Text(text) => {
                if let Ok(SessionWsMessage::Error { message, .. }) =
                    serde_json::from_str::<SessionWsMessage>(&text)
                {
                    eprintln!("{}", message);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    println!("\nConnection closed");
    Ok(())
}

pub async fn start_dap(args: StartDapArgs, cli_db: Option<&std::path::Path>) -> Result<()> {
    let _ = (args, cli_db);
    anyhow::bail!("DAP sessions are not implemented yet")
}
