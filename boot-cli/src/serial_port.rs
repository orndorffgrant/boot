use anyhow::Result;
use clap::Args;
use comfy_table::{Table, presets::NOTHING};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use crate::auth::{api_client, map_error};
use crate::cli_db::CliDb;

#[derive(Args)]
pub struct AttachSerialArgs {
    /// Server name and serial port ID in format: {server-name}/{port-id}
    pub server_and_port: String,
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

pub async fn list_serial_ports(cli_db: Option<&std::path::Path>) -> Result<()> {
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
        match client.serial_ports().await {
            Ok(response) => {
                for port in response.into_inner() {
                    let name = port.name.as_deref().unwrap_or(&port.path);
                    table.add_row(vec![&server.name, &port.id, name]);
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

pub async fn attach_serial(args: AttachSerialArgs, cli_db: Option<&std::path::Path>) -> Result<()> {
    let parts: Vec<&str> = args.server_and_port.splitn(2, '/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid format. Expected {{server-name}}/{{port-id}}");
    }
    let server_name = parts[0];
    let port_selector = parts[1];

    let config = CliDb::open_default(cli_db).await?;
    let server = config
        .get_server(server_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;

    let client = api_client(&server)?;
    let ports = client.serial_ports().await?.into_inner();
    let port_id = ports
        .iter()
        .find(|port| port.id == port_selector || port.path == port_selector)
        .map(|port| port.id.clone())
        .ok_or_else(|| anyhow::anyhow!("Serial port '{}' not found", port_selector))?;

    let session_id = if let Some(session) = &args.session {
        session.clone()
    } else {
        let active = client
            .list_sessions(Some("active"))
            .await?
            .into_inner()
            .into_iter()
            .find(|session| session.resource_id == port_id && session.kind == "serial");
        if let Some(existing) = active {
            if args.new {
                anyhow::bail!("Port already has an active session: {}", existing.id);
            }
            existing.id
        } else {
            if args.follow {
                anyhow::bail!("No active serial session for port '{}'", port_selector);
            }
            client
                .create_serial_session(
                    &port_id,
                    &boot_api_client::types::SerialSessionRequest {
                        options: boot_api_client::types::SessionOptions {
                            rtt_addr: None,
                            channels: Vec::new(),
                            baud_rate: Some(115200),
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
        }
    };

    println!("Connecting to {} ({})", session_id, server_name);
    println!("Press Ctrl+C to exit");
    println!();

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
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    let stdin_handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, stdin};
        let mut stdin = stdin();
        let mut buffer = [0u8; 1024];
        loop {
            match stdin.read(&mut buffer).await {
                Ok(0) => break,
                Ok(n) => {
                    if ws_tx
                        .send(Message::Binary(buffer[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    use tokio::io::{AsyncWriteExt, stdout};
    let mut stdout = stdout();
    while let Some(message) = ws_rx.next().await {
        match message? {
            Message::Binary(data) => {
                let payload = if data.len() >= 8 {
                    &data[8..]
                } else {
                    &data[..]
                };
                stdout.write_all(payload).await?;
                stdout.flush().await?;
            }
            Message::Text(text) => {
                if let Ok(boot_protocol::SessionWsMessage::Error { message, .. }) =
                    serde_json::from_str::<boot_protocol::SessionWsMessage>(&text)
                {
                    eprintln!("{}", message);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    stdin_handle.abort();
    println!("\nConnection closed");
    Ok(())
}
