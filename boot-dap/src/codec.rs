// SPDX-License-Identifier: AGPL-3.0-or-later
//! Minimal DAP `Content-Length` framing over the boot session byte channels.

use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::Sender;

const HEADER_LIMIT: usize = 16 * 1024;
const MESSAGE_LIMIT: usize = 32 * 1024 * 1024;

pub enum Next {
    Message(Value),
    Timeout,
    Ended,
}

pub struct DapReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
}

impl DapReader {
    pub fn new(rx: std::sync::mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            rx,
            buffer: Vec::new(),
        }
    }

    pub fn next(&mut self, timeout: Duration) -> Result<Next, String> {
        if let Some(message) = self.take_message()? {
            return Ok(Next::Message(message));
        }

        match self.rx.recv_timeout(timeout) {
            Ok(bytes) => {
                self.buffer.extend_from_slice(&bytes);
                if self.buffer.len() > MESSAGE_LIMIT + HEADER_LIMIT {
                    return Err("DAP input exceeds message limit".to_string());
                }
                match self.take_message()? {
                    Some(message) => Ok(Next::Message(message)),
                    None => Ok(Next::Timeout),
                }
            }
            Err(RecvTimeoutError::Timeout) => Ok(Next::Timeout),
            Err(RecvTimeoutError::Disconnected) => Ok(Next::Ended),
        }
    }

    fn take_message(&mut self) -> Result<Option<Value>, String> {
        loop {
            let Some(header_end) = find_header_end(&self.buffer) else {
                if self.buffer.len() > HEADER_LIMIT {
                    return Err("DAP header exceeds limit".to_string());
                }
                return Ok(None);
            };

            let header = &self.buffer[..header_end];
            let header_text =
                std::str::from_utf8(header).map_err(|_| "DAP header is not UTF-8".to_string())?;
            let length = header_text
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .ok_or_else(|| "DAP message is missing Content-Length".to_string())?;
            if length > MESSAGE_LIMIT {
                return Err("DAP message exceeds limit".to_string());
            }

            let body_start = header_end + 4;
            let body_end = body_start + length;
            if self.buffer.len() < body_end {
                return Ok(None);
            }

            let body = self.buffer[body_start..body_end].to_vec();
            self.buffer.drain(..body_end);
            let message = serde_json::from_slice(&body)
                .map_err(|error| format!("invalid DAP JSON: {error}"))?;
            return Ok(Some(message));
        }
    }
}

pub fn encode(message: &Value) -> Result<Vec<u8>, String> {
    let body = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    Ok(frame)
}

pub fn send(tx: &Sender<Vec<u8>>, message: &Value) -> Result<(), String> {
    let frame = encode(message)?;
    tx.try_send(frame)
        .map_err(|_| "DAP output queue is full".to_string())
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}
