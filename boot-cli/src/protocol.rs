// SPDX-License-Identifier: AGPL-3.0-or-later
//! Conversions between generated client DTOs and shared protocol DTOs, plus
//! the small amount of wire framing the CLI must understand.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::CliError;

pub fn convert<T: DeserializeOwned>(value: impl Serialize) -> Result<T, CliError> {
    let value = serde_json::to_value(value)?;
    serde_json::from_value(value).map_err(CliError::from)
}

/// Reads complete DAP frames from a byte stream.
pub struct FrameReader<R> {
    reader: R,
    buffer: Vec<u8>,
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
        }
    }

    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, CliError> {
        loop {
            if let Some(frame) = take_frame(&mut self.buffer)? {
                return Ok(Some(frame));
            }
            let mut chunk = [0u8; 4096];
            let count = self.reader.read(&mut chunk).await?;
            if count == 0 {
                return Ok(None);
            }
            self.buffer.extend_from_slice(&chunk[..count]);
        }
    }
}

/// Returns the next DAP JSON body from a buffered stream.
pub fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>, CliError> {
    let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Ok(None);
    };
    let header = std::str::from_utf8(&buffer[..header_end])
        .map_err(|_| CliError::config("DAP header is not UTF-8"))?;
    let length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .ok_or_else(|| CliError::config("DAP message is missing Content-Length"))?;
    let start = header_end + 4;
    let end = start + length;
    if buffer.len() < end {
        return Ok(None);
    }
    let frame = buffer[start..end].to_vec();
    buffer.drain(..end);
    Ok(Some(frame))
}

pub fn to_frame(message: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(message).unwrap_or_default();
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    frame
}
