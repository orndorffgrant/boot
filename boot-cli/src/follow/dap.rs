// SPDX-License-Identifier: AGPL-3.0-or-later
//! DAP retained-session replay rendering.
//!
//! The bridge owns transport lifecycle; this module projects the retained DAP
//! message stream into human-readable output.

use std::collections::HashMap;
use std::sync::Arc;

use boot_protocol::{Session, SessionWsMessage};
use futures::StreamExt;
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use crate::error::CliError;
use crate::ports::Ports;
use crate::protocol::take_frame;
use crate::ui::Ui;

pub async fn replay(
    ports: &Ports,
    ui: Arc<dyn Ui>,
    server: crate::domain::ServerConfig,
    session: Session,
    after: Option<u64>,
) -> Result<(), CliError> {
    let client = ports.api.client(&server)?;
    let upgraded = client
        .session_io(
            &session.id,
            after,
            Some(true),
            Some("read"),
            Some("best-effort"),
        )
        .await
        .map_err(|error| crate::error::map_client_error(&server.name, error))?
        .into_inner();
    let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        upgraded,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    let (_tx, mut rx) = ws.split();
    let mut parser = DapMessageParser::default();
    let mut renderer = DapRenderer::default();
    while let Some(message) = rx.next().await {
        match message {
            Ok(Message::Binary(data)) => {
                let frame = boot_protocol::decode_session_frame("dap", &data)
                    .map_err(|error| CliError::config(error.to_string()))?;
                let direction = frame.channel.unwrap_or(0);
                for message in parser.push(direction, frame.data)? {
                    renderer.render(&message, &ui);
                }
            }
            Ok(Message::Text(text)) => {
                if let Ok(SessionWsMessage::Ended { .. }) =
                    serde_json::from_str::<SessionWsMessage>(&text)
                {
                    break;
                }
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
    Ok(())
}

#[derive(Default)]
struct DapMessageParser {
    buffers: HashMap<u8, Vec<u8>>,
}

impl DapMessageParser {
    fn push(&mut self, direction: u8, data: &[u8]) -> Result<Vec<Value>, CliError> {
        let buffer = self.buffers.entry(direction).or_default();
        buffer.extend_from_slice(data);
        let mut out = Vec::new();
        while let Some(frame) = take_frame(buffer)? {
            if let Ok(value) = serde_json::from_slice(&frame) {
                out.push(value);
            }
        }
        Ok(out)
    }
}

#[derive(Default)]
struct DapRenderer {
    pending_thread: Option<i64>,
}

impl DapRenderer {
    fn render(&mut self, message: &Value, ui: &Arc<dyn Ui>) {
        let kind = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "request" => {
                let command = message
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if command == "launch" || command == "attach" {
                    let args = message.get("arguments").unwrap_or(&Value::Null);
                    let chip = args.get("chip").and_then(Value::as_str).unwrap_or("-");
                    let program = args
                        .get("bootArtifacts")
                        .and_then(|value| value.get("program"))
                        .and_then(Value::as_str)
                        .or_else(|| args.get("program").and_then(Value::as_str))
                        .unwrap_or("-");
                    let _ = ui.print_data(&format!("{command}: chip={chip} program={program}\n"));
                }
            }
            "event" => {
                let event = message
                    .get("event")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let body = message.get("body").unwrap_or(&Value::Null);
                match event {
                    "stopped" => {
                        let reason = body.get("reason").and_then(Value::as_str).unwrap_or("stop");
                        self.pending_thread = body.get("threadId").and_then(Value::as_i64);
                        let _ = ui.print_data(&format!(
                            "stopped: {} thread {}\n",
                            reason,
                            self.pending_thread.unwrap_or(1)
                        ));
                    }
                    "continued" => {
                        let _ = ui.print_data(&format!(
                            "continued thread {}\n",
                            body.get("threadId").and_then(Value::as_i64).unwrap_or(1)
                        ));
                    }
                    "output" => {
                        let category = body
                            .get("category")
                            .and_then(Value::as_str)
                            .unwrap_or("console");
                        let output = body
                            .get("output")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let _ = ui.print_data(&format!("[{category}] {output}"));
                    }
                    "terminated" | "exited" => {
                        let _ = ui.print_data(&format!("{event}\n"));
                    }
                    _ => {}
                }
            }
            "response" => {
                let command = message
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let success = message
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !success {
                    let text = message
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed");
                    let _ = ui.print_data(&format!("{command} failed: {text}\n"));
                    return;
                }
                match command {
                    "setBreakpoints" => {
                        let count = message
                            .get("body")
                            .and_then(|body| body.get("breakpoints"))
                            .and_then(Value::as_array)
                            .map(Vec::len)
                            .unwrap_or(0);
                        let _ = ui.print_data(&format!("breakpoints: {count} requested\n"));
                    }
                    "stackTrace" => {
                        if let Some(frames) = message
                            .get("body")
                            .and_then(|body| body.get("stackFrames"))
                            .and_then(Value::as_array)
                            && let Some(frame) = frames.first()
                        {
                            let name = frame
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("<unknown>");
                            let source = frame
                                .get("source")
                                .and_then(|source| source.get("path"))
                                .and_then(Value::as_str)
                                .unwrap_or("<unknown>");
                            let line = frame.get("line").and_then(Value::as_i64).unwrap_or(0);
                            let _ = ui.print_data(&format!("  at {name} ({source}:{line})\n"));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::to_frame;
    use serde_json::json;

    #[test]
    fn extracts_dap_frames_across_chunks() {
        let body = br#"{"type":"request","seq":1,"command":"initialize"}"#;
        let frame = to_frame(&serde_json::from_slice(body).unwrap());
        let split = frame.len() / 2;
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&frame[..split]);
        assert!(take_frame(&mut buffer).unwrap().is_none());
        buffer.extend_from_slice(&frame[split..]);
        let body = take_frame(&mut buffer).unwrap().unwrap();
        assert!(serde_json::from_slice::<Value>(&body).is_ok());
        assert!(buffer.is_empty());
    }

    #[test]
    fn parser_keeps_directions_separate() {
        let mut parser = DapMessageParser::default();
        let a = to_frame(&json!({"type":"event","event":"stopped"}));
        let b = to_frame(&json!({"type":"event","event":"continued"}));
        let messages = parser.push(0, &a).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["event"], "stopped");
        let messages = parser.push(1, &b).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["event"], "continued");
    }
}
