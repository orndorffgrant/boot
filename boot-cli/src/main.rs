use anyhow::Ok;
use base64::Engine;
use clap::{Args, Parser, Subcommand};
use comfy_table::{Attribute, Cell, Table, presets::NOTHING};
use indicatif::{ProgressBar, ProgressStyle};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use boot_api_client;

mod server_config;
mod systemd_service;

#[derive(Parser)]
#[command(name = "boot")]
struct CLI {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(name = "start-server")]
    StartServer(StartServerArgs),
    #[command(name = "stop-server")]
    StopServer,
    #[command(name = "restart-server")]
    RestartServer,
    #[command(name = "run-server")]
    RunServer(RunServerArgs),
    #[command(name = "api-desc")]
    APIDesc,
    #[command(name = "list-probes")]
    ListProbes,
    #[command(name = "list-serial-ports")]
    ListSerialPorts,
    #[command(name = "list-servers")]
    ListServers,
    #[command(name = "add-server")]
    AddServer(AddServerArgs),
    #[command(name = "remove-server")]
    RemoveServer(RemoveServerArgs),
    Run(RunArgs),
    Shell(ShellArgs),
    Attach(AttachArgs),
}

#[derive(Args)]
struct StartServerArgs {
    /// Host to listen on (defaults to 127.0.0.1)
    #[arg(long)]
    host: Option<String>,
    /// Port to run the server on (defaults to 46673)
    #[arg(short, long)]
    port: Option<u16>,
}

#[derive(Args)]
struct RunServerArgs {
    /// Host to listen on (defaults to 127.0.0.1)
    #[arg(long)]
    host: Option<String>,
    /// Port to run the server on (defaults to 46673)
    #[arg(short, long)]
    port: Option<u16>,
}

#[derive(Args)]
struct AddServerArgs {
    /// Name of the server
    name: String,
    /// Connection URL (e.g., http://localhost:46673)
    url: String,
}

#[derive(Args)]
struct RemoveServerArgs {
    /// Name of the server to remove
    name: String,
}

#[derive(Args)]
struct RunArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    server_and_selector: String,
    /// Path to the ELF file
    elf_path: String,
    /// Don't attach and show defmt logs after flashing
    #[arg(long)]
    no_attach: bool,
}

#[derive(Args)]
struct ShellArgs {
    /// Server name and serial port ID in format: {server-name}/{port-id}
    server_and_port: String,
}

#[derive(Args)]
struct AttachArgs {
    /// Server name and debug probe selector in format: {server-name}/{debugprobeselector}
    server_and_selector: String,
    /// Optional path to the ELF file for defmt decoding
    #[arg(long)]
    elf: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CLI::parse();
    match cli.command {
        Command::StartServer(args) => start_server(args).await,
        Command::StopServer => stop_server().await,
        Command::RestartServer => restart_server().await,
        Command::RunServer(args) => run_server(args).await,
        Command::APIDesc => api_desc().await,
        Command::ListServers => list_servers().await,
        Command::AddServer(args) => add_server(args).await,
        Command::RemoveServer(args) => remove_server(args).await,
        Command::ListProbes => list_probes().await,
        Command::ListSerialPorts => list_serial_ports().await,
        Command::Run(args) => run(args).await,
        Command::Shell(args) => shell(args).await,
        Command::Attach(args) => attach(args).await,
    }
}

fn print_servers_table(servers: &[server_config::Server], highlight_name: Option<&str>, highlight_style: Option<Attribute>) {
    if servers.is_empty() {
        println!("No servers configured.");
        return;
    }
    
    let mut table = Table::new();
    table.load_preset(NOTHING);
    
    for server in servers {
        if let Some(name) = highlight_name {
            if server.name == name {
                if let Some(style) = highlight_style {
                    table.add_row(vec![
                        Cell::new(&server.name).add_attribute(style),
                        Cell::new(&server.url).add_attribute(style),
                    ]);
                    continue;
                }
            }
        }
        table.add_row(vec![&server.name, &server.url]);
    }
    
    println!("{table}");
}

async fn start_server(args: StartServerArgs) -> anyhow::Result<()> {
    let host = args.host.clone().unwrap_or("localhost".to_string());
    let port = args.port.clone().unwrap_or(46673);
    systemd_service::start_service(args.host, args.port)?;
    
    // Add local server to the servers list
    let mut config = server_config::ServerConfig::load()?;
    
    // Remove existing "local" entry if it exists
    config.servers.retain(|s| s.name != "local");
    
    // Determine the URL based on the host
    let url = if host == "0.0.0.0".to_string() {
        // If listening on all interfaces, use localhost for client connection
        format!("http://localhost:{}", port)
    } else {
        format!("http://{}:{}", host, port)
    };
    
    // Add the local server
    config.add_server("local".to_string(), url)?;
    config.save()?;
    
    // Display the updated servers list with the new local server in bold
    print_servers_table(config.list_servers(), Some("local"), Some(Attribute::Bold));
    Ok(())
}

async fn stop_server() -> anyhow::Result<()> {
    systemd_service::stop_service()?;
    
    // Remove local server from the servers list
    let mut config = server_config::ServerConfig::load()?;
    
    // Check if local server exists
    let local_exists = config.list_servers().iter().any(|s| s.name == "local");
    
    if local_exists {
        // Display the servers list with local crossed out before removal
        print_servers_table(config.list_servers(), Some("local"), Some(Attribute::CrossedOut));
        
        // Now remove the local server
        config.remove_server("local")?;
        config.save()?;
    }
    
    Ok(())
}

async fn restart_server() -> anyhow::Result<()> {
    systemd_service::restart_service()?;
    println!("Server restarted");
    Ok(())
}

async fn run_server(args: RunServerArgs) -> anyhow::Result<()> {
    boot_api::run(args.host, args.port).await
}

async fn api_desc() -> anyhow::Result<()> {
    let openapi = boot_api::openapi();
    println!("{}", openapi);
    Ok(())
}

async fn list_probes() -> anyhow::Result<()> {
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

async fn list_serial_ports() -> anyhow::Result<()> {
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
        match client.serial_ports().await {
            std::result::Result::Ok(response) => {
                let ports = response.into_inner();
                // Skip if no ports found
                if !ports.is_empty() {
                    for port in ports {
                        let name_display = port.name.as_deref().unwrap_or(&port.path);
                        table.add_row(vec![&server.name, &port.id, name_display]);
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

async fn list_servers() -> anyhow::Result<()> {
    let config = server_config::ServerConfig::load()?;
    print_servers_table(config.list_servers(), None, None);
    Ok(())
}

async fn add_server(args: AddServerArgs) -> anyhow::Result<()> {
    let mut config = server_config::ServerConfig::load()?;
    config.add_server(args.name.clone(), args.url.clone())?;
    config.save()?;
    
    print_servers_table(config.list_servers(), Some(&args.name), Some(Attribute::Bold));
    Ok(())
}

async fn remove_server(args: RemoveServerArgs) -> anyhow::Result<()> {
    let config = server_config::ServerConfig::load()?;
    
    // Check if server exists before removal
    if !config.list_servers().iter().any(|s| s.name == args.name) {
        anyhow::bail!("Server '{}' not found", args.name);
    }
    
    // Display the servers list with the server being removed crossed out
    print_servers_table(config.list_servers(), Some(&args.name), Some(Attribute::CrossedOut));
    
    // Now actually remove and save
    let mut config = config;
    config.remove_server(&args.name)?;
    config.save()?;
    
    Ok(())
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
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
    
    // Attach by default unless --no-attach flag is set
    if !args.no_attach {
        println!();
        let attach_args = AttachArgs {
            server_and_selector: args.server_and_selector.clone(),
            elf: Some(args.elf_path),
        };
        attach(attach_args).await?;
    }
    
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

async fn attach(args: AttachArgs) -> anyhow::Result<()> {
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
    
    // Build WebSocket URL
    let ws_url = server.url.replace("http://", "ws://").replace("https://", "wss://");
    let ws_url = if let Some(addr) = rtt_addr {
        format!("{}/v1/probes/{}/attach?rtt_addr=0x{:x}", ws_url, selector, addr)
    } else {
        format!("{}/v1/probes/{}/attach", ws_url, selector)
    };
    
    spinner.set_message(format!("Attaching to {}...", selector));
    
    // Connect to WebSocket
    let (ws_stream, _) = connect_async(&ws_url).await
        .map_err(|e| {
            spinner.finish_and_clear();
            anyhow::anyhow!("Failed to connect to WebSocket: {}", e)
        })?;
    
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

async fn shell(args: ShellArgs) -> anyhow::Result<()> {
    // Parse server name and port ID
    let parts: Vec<&str> = args.server_and_port.split('/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid format. Expected {{server-name}}/{{port-id}}");
    }
    let server_name = parts[0];
    let port_id = parts[1];
    
    // Load server configuration
    let config = server_config::ServerConfig::load()?;
    let server = config.list_servers()
        .iter()
        .find(|s| s.name == server_name)
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
    
    // Build WebSocket URL
    let ws_url = server.url.replace("http://", "ws://").replace("https://", "wss://");
    let ws_url = format!("{}/v1/serial-ports/{}/shell", ws_url, port_id);
    
    println!("Connecting to {} ({})", port_id, server_name);
    println!("Press Ctrl+C to exit");
    println!();
    
    // Connect to WebSocket
    let (ws_stream, _) = connect_async(&ws_url).await
        .map_err(|e| anyhow::anyhow!("Failed to connect to WebSocket: {}", e))?;
    
    let (mut ws_tx, mut ws_rx) = ws_stream.split();
    
    // Spawn task to read from stdin and send to WebSocket
    let stdin_handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, stdin};
        let mut stdin = stdin();
        let mut buffer = [0u8; 1024];
        
        loop {
            match stdin.read(&mut buffer).await {
                std::result::Result::Ok(0) => break,
                std::result::Result::Ok(n) => {
                    if ws_tx.send(Message::Binary(buffer[..n].to_vec())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    
    // Read from WebSocket and write to stdout
    use tokio::io::{AsyncWriteExt, stdout};
    let mut stdout = stdout();
    
    while let Some(msg) = ws_rx.next().await {
        match msg {
            std::result::Result::Ok(Message::Binary(data)) => {
                stdout.write_all(&data).await?;
                stdout.flush().await?;
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
    
    stdin_handle.abort();
    
    println!("\nConnection closed");
    
    Ok(())
}
