// SPDX-License-Identifier: AGPL-3.0-or-later
//! CLI-side DAP transport bridge.

use std::sync::Arc;

use boot_protocol::{
    FrameDirection, FrameRecord, Session, SessionAcquire, SessionFrameHeader, SessionOptions,
    SessionWsMessage, decode_frame, encode_frame,
};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::error::CliError;
use crate::plan::DapTransport;
use crate::ports::Ports;
use crate::ports::dap::LaunchUploader;
use crate::protocol::FrameReader;
use crate::ui::Ui;

pub async fn run_bridge(
    ports: &Ports,
    ui: Arc<dyn Ui>,
    cancel: CancellationToken,
    server: crate::domain::ServerConfig,
    resource_id: String,
    transport: DapTransport,
    artifact_id: Option<String>,
    inherit_artifact: bool,
) -> Result<(), CliError> {
    match transport {
        DapTransport::Tcp { port } => {
            run_tcp(
                ports,
                ui,
                cancel,
                server,
                resource_id,
                port,
                artifact_id,
                inherit_artifact,
            )
            .await
        }
        DapTransport::Stdio => {
            run_stdio(
                ports,
                ui,
                cancel,
                server,
                resource_id,
                artifact_id,
                inherit_artifact,
            )
            .await
        }
    }
}

async fn run_tcp(
    ports: &Ports,
    ui: Arc<dyn Ui>,
    cancel: CancellationToken,
    server: crate::domain::ServerConfig,
    resource_id: String,
    port: u16,
    artifact_id: Option<String>,
    inherit_artifact: bool,
) -> Result<(), CliError> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| CliError::config(format!("Failed to bind 127.0.0.1:{port}: {error}")))?;
    ui.status(&format!(
        "DAP bridge listening on 127.0.0.1:{port} for probe {resource_id}"
    ));
    loop {
        let (stream, address) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            accepted = listener.accept() => accepted.map_err(|error| {
                CliError::config(format!("Failed to accept DAP client: {error}"))
            })?,
        };
        ui.status(&format!("DAP client connected from {address}"));
        if let Err(error) = serve_tcp_session(
            ports,
            &ui,
            &server,
            &resource_id,
            artifact_id.as_deref(),
            inherit_artifact,
            &cancel,
            stream,
        )
        .await
        {
            ui.error(&error.to_string());
        }
        ui.status("DAP client disconnected");
    }
}

async fn serve_tcp_session(
    ports: &Ports,
    ui: &Arc<dyn Ui>,
    server: &crate::domain::ServerConfig,
    resource_id: &str,
    artifact_id: Option<&str>,
    inherit_artifact: bool,
    cancel: &CancellationToken,
    stream: tokio::net::TcpStream,
) -> Result<(), CliError> {
    let session = create_session(ports, server, resource_id, artifact_id, inherit_artifact).await?;
    ui.status(&format!(
        "DAP session {} started; replay after disconnect: boot replay-session {}/{}",
        session.id, server.name, session.id
    ));
    let (read, write) = stream.into_split();
    let result = bridge_io(ports, ui, server, &session, cancel, read, write).await;
    let _ = ports.api.close_session(server, &session.id).await;
    result
}
async fn run_stdio(
    ports: &Ports,
    ui: Arc<dyn Ui>,
    cancel: CancellationToken,
    server: crate::domain::ServerConfig,
    resource_id: String,
    artifact_id: Option<String>,
    inherit_artifact: bool,
) -> Result<(), CliError> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    serve_session_stdio(
        ports,
        &ui,
        &server,
        &resource_id,
        artifact_id.as_deref(),
        inherit_artifact,
        &cancel,
        stdin,
        stdout,
    )
    .await
}

async fn serve_session_stdio<R, W>(
    ports: &Ports,
    ui: &Arc<dyn Ui>,
    server: &crate::domain::ServerConfig,
    resource_id: &str,
    artifact_id: Option<&str>,
    inherit_artifact: bool,
    cancel: &CancellationToken,
    input: R,
    output: W,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let session = create_session(ports, server, resource_id, artifact_id, inherit_artifact).await?;
    ui.status(&format!(
        "DAP session {} started; replay after disconnect: boot replay-session {}/{}",
        session.id, server.name, session.id
    ));
    let result = bridge_io(ports, ui, server, &session, cancel, input, output).await;
    let _ = ports.api.close_session(server, &session.id).await;
    result
}

async fn create_session(
    ports: &Ports,
    server: &crate::domain::ServerConfig,
    resource_id: &str,
    artifact_id: Option<&str>,
    inherit_artifact: bool,
) -> Result<Session, CliError> {
    let result = ports
        .api
        .ensure_probe_session(
            server,
            resource_id,
            "dap",
            SessionAcquire::Create,
            SessionOptions::default(),
            artifact_id,
            inherit_artifact,
            1,
            0,
            5000,
        )
        .await?;
    Ok(result.session)
}

async fn bridge_io<R, W>(
    ports: &Ports,
    ui: &Arc<dyn Ui>,
    server: &crate::domain::ServerConfig,
    session: &Session,
    cancel: &CancellationToken,
    input: R,
    output: W,
) -> Result<(), CliError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let client = ports.api.client(server)?;
    let upgraded = client
        .session_io(&session.id, None, None, Some("write"), None)
        .await
        .map_err(|error| crate::error::map_client_error(&server.name, error))?
        .into_inner();
    let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    let (mut ws_tx, mut ws_rx) = ws.split();
    let (input, mut output) = (input, output);

    let uploader = LaunchUploader::new(ports.api.as_ref(), ui, server);
    let input_task = async {
        let mut reader = FrameReader::new(input);
        while let Some(frame) = reader.next().await? {
            let frame = match uploader.rewrite(&frame).await {
                Ok(frame) => frame,
                Err(error) => {
                    if let Some(response) =
                        LaunchUploader::error_response(&frame, &error.to_string())
                    {
                        let response = input_frame(&response)?;
                        let _ = ws_tx.send(Message::Binary(response.into())).await;
                    }
                    return Err(error);
                }
            };
            let frame = input_frame(&frame)?;
            if ws_tx.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
        }
        Ok::<(), CliError>(())
    };

    let output_task = async {
        while let Some(message) = ws_rx.next().await {
            match message {
                Ok(Message::Binary(data)) => {
                    let frame =
                        decode_frame(&data).map_err(|error| CliError::config(error.to_string()))?;
                    match frame.header.record {
                        FrameRecord::Data if frame.header.direction == FrameDirection::Output => {
                            output.write_all(frame.payload).await?;
                            output.flush().await?;
                        }
                        FrameRecord::Gap => {
                            let gap = boot_protocol::GapPayload::decode(frame.payload)
                                .map_err(|error| CliError::config(error.to_string()))?;
                            ui.error(&format!(
                                "DAP session gap from {} to {}",
                                gap.from_seq, gap.to_seq
                            ));
                        }
                        _ => {}
                    }
                }
                Ok(Message::Text(text)) => {
                    if let Ok(SessionWsMessage::Error { code, message }) =
                        serde_json::from_str::<SessionWsMessage>(&text)
                    {
                        ui.error(&format!("{code}: {message}"));
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        Ok::<(), CliError>(())
    };

    tokio::select! {
        _ = cancel.cancelled() => Ok(()),
        result = input_task => result,
        result = output_task => result,
    }
}

fn input_frame(payload: &[u8]) -> Result<Vec<u8>, CliError> {
    encode_frame(
        &SessionFrameHeader {
            flags: 0,
            record: FrameRecord::Data,
            direction: FrameDirection::Input,
            stream: 0,
            seq: 0,
            timestamp_ns: 0,
        },
        payload,
    )
    .map_err(|error| CliError::config(error.to_string()))
}
