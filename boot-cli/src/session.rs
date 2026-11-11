use anyhow::Result;
use clap::Args;
use comfy_table::{Table, presets::NOTHING};
use futures::StreamExt;
use tokio_tungstenite::tungstenite::Message;

use crate::auth::{api_client, map_error};
use crate::cli_db::CliDb;

#[derive(Args)]
pub struct SessionReplayArgs {
    /// Server name and session ID in format: {server-name}/{session-id}
    pub server_and_session: String,
    /// Start replaying after this data sequence
    #[arg(long, default_value = "0")]
    pub from: u64,
    /// Include retained serial input records
    #[arg(long)]
    pub include_input: bool,
}

pub async fn list_sessions(cli_db: Option<&std::path::Path>) -> Result<()> {
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
        match client.list_sessions(Some("all")).await {
            Ok(response) => {
                for session in response.into_inner() {
                    table.add_row(vec![
                        server.name.clone(),
                        session.id,
                        session.kind,
                        session.state,
                        session.resource_id,
                        session.subscribers.to_string(),
                        session.data.bytes.to_string(),
                    ]);
                }
            }
            Err(error) => {
                let message = map_error(&server, error.status(), &format_args!("{error:#}"));
                eprintln!(
                    "Error connecting to server '{}' ({}): {}",
                    server.name, server.url, message
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

pub async fn replay_session(
    args: SessionReplayArgs,
    cli_db: Option<&std::path::Path>,
) -> Result<()> {
    let parts: Vec<&str> = args.server_and_session.splitn(2, '/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid format. Expected {{server-name}}/{{session-id}}");
    }
    let server_name = parts[0];
    let session_id = parts[1];
    let config = CliDb::open_default(cli_db).await?;
    let server = config
        .get_server(server_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
    let client = api_client(&server)?;
    let session = client.get_session(session_id).await?.into_inner();
    let upgraded = client
        .session_io(
            session_id,
            Some(args.from),
            Some(args.include_input),
            Some("read"),
            None,
        )
        .await
        .map_err(|error| map_error(&server, error.status(), &format_args!("{error:#}")))?
        .into_inner();
    let ws_stream = upgraded_to_websocket(upgraded).await;
    let (_tx, mut rx) = ws_stream.split();

    use tokio::io::{AsyncWriteExt, stdout};
    let mut stdout = stdout();
    while let Some(message) = rx.next().await {
        match message? {
            Message::Binary(data) => {
                let header = if session.kind == "rtt" { 9 } else { 8 };
                if data.len() > header {
                    stdout.write_all(&data[header..]).await?;
                    stdout.flush().await?;
                }
            }
            Message::Text(text) => {
                if let Ok(boot_protocol::SessionWsMessage::Input { data, .. }) =
                    serde_json::from_str::<boot_protocol::SessionWsMessage>(&text)
                {
                    use base64::Engine;
                    if let Ok(bytes) = base64::prelude::BASE64_STANDARD.decode(data) {
                        stdout.write_all(b"> ").await?;
                        stdout.write_all(&bytes).await?;
                        stdout.write_all(b"\n").await?;
                        stdout.flush().await?;
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

#[derive(Args)]
pub struct SessionExportArgs {
    /// Server name and session ID in format: {server-name}/{session-id}
    pub server_and_session: String,
    /// Output file (.cast)
    #[arg(long, value_name = "PATH")]
    pub out: std::path::PathBuf,
    /// Export format
    #[arg(long, default_value = "asciicast-v3")]
    pub format: String,
    /// Include retained serial input
    #[arg(long)]
    pub include_input: bool,
}

pub async fn export_session(
    args: SessionExportArgs,
    cli_db: Option<&std::path::Path>,
) -> Result<()> {
    let parts: Vec<&str> = args.server_and_session.splitn(2, '/').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid format. Expected {{server-name}}/{{session-id}}");
    }
    let config = CliDb::open_default(cli_db).await?;
    let server = config
        .get_server(parts[0])
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", parts[0]))?;
    let client = api_client(&server)?;
    let mut response = client
        .export_session(parts[1], None, Some(&args.format), Some(args.include_input))
        .await
        .map_err(|error| map_error(&server, error.status(), &format_args!("{error:#}")))?
        .into_inner();
    let mut file = tokio::fs::File::create(&args.out).await?;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = response.next().await {
        file.write_all(&chunk?).await?;
    }
    file.flush().await?;
    println!("Wrote {}", args.out.display());
    Ok(())
}
