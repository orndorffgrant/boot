use anyhow::Result;
use clap::Args;
use comfy_table::{Table, presets::NOTHING};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::server_config;

#[derive(Args)]
pub struct SerialShellArgs {
    /// Server name and serial port ID in format: {server-name}/{port-id}
    pub server_and_port: String,
}

pub async fn list_serial_ports() -> Result<()> {
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

pub async fn serial_shell(args: SerialShellArgs) -> Result<()> {
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
    
    println!("Connecting to {} ({})", port_id, server_name);
    println!("Press Ctrl+C to exit");
    println!();
    
    // Use the generated client to connect
    let client = boot_api_client::Client::new(&server.url);
    let upgraded = client.serial_shell(port_id).await
        .map_err(|e| anyhow::anyhow!("Failed to connect to serial port: {:#}", e))?
        .into_inner();
    
    // Convert the upgraded connection to a WebSocket
    let ws_stream = upgraded_to_websocket(upgraded).await;
    
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
