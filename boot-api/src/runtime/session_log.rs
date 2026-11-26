// SPDX-License-Identifier: AGPL-3.0-or-later
//! Append-only session data log.
//!
//! Each session has one `session.log` file. It starts with a session-header
//! frame and is followed by length-delimited, CRC-checked session frames. The
//! same frame codec is used by the live WebSocket data plane.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use boot_protocol::{
    FrameDirection, FrameRecord, OwnedSessionFrame, SESSION_FRAME_HEADER_LEN,
    SESSION_FRAME_MAX_PAYLOAD, SessionFrameHeader, SessionHeaderPayload, decode_frame,
    encode_frame,
};

pub fn session_log_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(session_id).join("session.log")
}

#[derive(Debug, Clone)]
pub struct LogScan {
    pub header: Option<SessionHeaderPayload>,
    pub first_seq: u64,
    pub latest_seq: u64,
    pub input_bytes: u64,
    pub last_good_offset: u64,
    pub problem: Option<LogProblem>,
}

#[derive(Debug, Clone)]
pub enum LogProblem {
    Truncated { offset: u64 },
    Corrupt { offset: u64, message: String },
}

#[derive(Debug)]
pub enum NextFrame {
    End,
    Frame(OwnedSessionFrame),
    Truncated { offset: u64 },
    Corrupt { offset: u64, message: String },
}

pub struct SessionLogWriter {
    file: File,
    path: PathBuf,
}

impl SessionLogWriter {
    pub fn open(root: &Path, session_id: &str, kind: &str, created_at_ns: u64) -> Result<Self> {
        let path = session_log_path(root, session_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .with_context(|| format!("Failed to open {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        let length = file.metadata()?.len();
        if length == 0 {
            let header = SessionFrameHeader {
                flags: 0,
                record: FrameRecord::SessionHeader,
                direction: FrameDirection::Output,
                stream: 0,
                seq: 0,
                timestamp_ns: created_at_ns,
            };
            let payload = serde_json::to_vec(&SessionHeaderPayload {
                v: 1,
                protocol: boot_protocol::PROTOCOL_VERSION,
                session_id: session_id.to_string(),
                kind: kind.to_string(),
                created_at_ns,
            })?;
            let frame = encode_frame(&header, &payload)?;
            file.write_all(&frame)?;
            file.flush()?;
        } else {
            let existing = read_header(&path)?
                .ok_or_else(|| anyhow!("session log has no header: {}", path.display()))?;
            if existing.session_id != session_id {
                return Err(anyhow!(
                    "session log belongs to {}, not {}",
                    existing.session_id,
                    session_id
                ));
            }
            if existing.kind != kind {
                return Err(anyhow!(
                    "session log kind is {}, not {}",
                    existing.kind,
                    kind
                ));
            }
        }
        Ok(Self { file, path })
    }

    pub fn append(&mut self, header: &SessionFrameHeader, payload: &[u8]) -> Result<()> {
        let frame = encode_frame(header, payload)?;
        self.file.write_all(&frame)?;
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub struct SessionLogReader {
    file: File,
    header: SessionHeaderPayload,
    offset: u64,
}

impl SessionLogReader {
    pub fn open(path: &Path) -> Result<Option<Self>> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut data = Vec::new();
        let first = read_one_frame(&mut file, 0, &mut data)?;
        let frame = match first {
            NextFrame::End => return Ok(None),
            NextFrame::Frame(frame) => frame,
            NextFrame::Truncated { offset } => {
                return Err(anyhow!("session log header is truncated at {offset}"));
            }
            NextFrame::Corrupt { offset, message } => {
                return Err(anyhow!(
                    "session log header is corrupt at {offset}: {message}"
                ));
            }
        };
        if frame.header.record != FrameRecord::SessionHeader {
            return Err(anyhow!("session log does not start with a header frame"));
        }
        let header: SessionHeaderPayload = serde_json::from_slice(&frame.payload)
            .context("session log header is not valid JSON")?;
        let offset = (SESSION_FRAME_HEADER_LEN + frame.payload.len()) as u64;
        Ok(Some(Self {
            file,
            header,
            offset,
        }))
    }

    pub fn header(&self) -> &SessionHeaderPayload {
        &self.header
    }

    pub fn next_frame(&mut self) -> NextFrame {
        let mut data = Vec::new();
        match read_one_frame(&mut self.file, self.offset, &mut data) {
            Ok(NextFrame::Frame(frame)) => {
                self.offset += (SESSION_FRAME_HEADER_LEN + frame.payload.len()) as u64;
                NextFrame::Frame(frame)
            }
            Ok(other) => other,
            Err(error) => NextFrame::Corrupt {
                offset: self.offset,
                message: error.to_string(),
            },
        }
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }
}

fn read_one_frame(file: &mut File, offset: u64, buffer: &mut Vec<u8>) -> Result<NextFrame> {
    let mut header = [0u8; SESSION_FRAME_HEADER_LEN];
    let read = read_up_to(file, &mut header)?;
    if read == 0 {
        return Ok(NextFrame::End);
    }
    if read < SESSION_FRAME_HEADER_LEN {
        return Ok(NextFrame::Truncated { offset });
    }
    if header[..4] != boot_protocol::SESSION_FRAME_MAGIC {
        return Ok(NextFrame::Corrupt {
            offset,
            message: "bad frame magic".to_string(),
        });
    }
    if header[4] != boot_protocol::SESSION_FRAME_VERSION {
        return Ok(NextFrame::Corrupt {
            offset,
            message: "unsupported frame version".to_string(),
        });
    }
    let payload_len =
        u32::from_be_bytes(header[28..32].try_into().expect("checked length")) as usize;
    if payload_len > SESSION_FRAME_MAX_PAYLOAD {
        return Ok(NextFrame::Corrupt {
            offset,
            message: "frame payload exceeds limit".to_string(),
        });
    }
    buffer.clear();
    buffer.extend_from_slice(&header);
    buffer.resize(SESSION_FRAME_HEADER_LEN + payload_len, 0);
    let payload_read = read_up_to(file, &mut buffer[SESSION_FRAME_HEADER_LEN..])?;
    if payload_read < payload_len {
        return Ok(NextFrame::Truncated { offset });
    }
    match decode_frame(buffer) {
        Ok(frame) => Ok(NextFrame::Frame(OwnedSessionFrame {
            header: frame.header,
            payload: frame.payload.to_vec(),
        })),
        Err(error) => Ok(NextFrame::Corrupt {
            offset,
            message: error.to_string(),
        }),
    }
}

fn read_up_to(file: &mut File, buffer: &mut [u8]) -> Result<usize> {
    let mut total = 0;
    while total < buffer.len() {
        match file.read(&mut buffer[total..]) {
            Ok(0) => break,
            Ok(count) => total += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(total)
}

fn read_header(path: &Path) -> Result<Option<SessionHeaderPayload>> {
    let Some(reader) = SessionLogReader::open(path)? else {
        return Ok(None);
    };
    Ok(Some(reader.header().clone()))
}

pub fn scan(path: &Path) -> Result<LogScan> {
    let Some(mut reader) = SessionLogReader::open(path)? else {
        return Ok(LogScan {
            header: None,
            first_seq: 0,
            latest_seq: 0,
            input_bytes: 0,
            last_good_offset: 0,
            problem: None,
        });
    };
    let header = Some(reader.header().clone());
    let mut first_seq = 0u64;
    let mut latest_seq = 0u64;
    let mut input_bytes = 0u64;
    let mut last_good_offset = reader.offset();
    let problem = loop {
        match reader.next_frame() {
            NextFrame::Frame(frame) => {
                if frame.header.seq > 0 {
                    if first_seq == 0 || frame.header.seq < first_seq {
                        first_seq = frame.header.seq;
                    }
                    latest_seq = latest_seq.max(frame.header.seq);
                }
                if frame.header.record == FrameRecord::Data
                    && frame.header.direction == FrameDirection::Input
                {
                    input_bytes += frame.payload.len() as u64;
                }
                last_good_offset = reader.offset();
            }
            NextFrame::End => break None,
            NextFrame::Truncated { offset } => {
                break Some(LogProblem::Truncated { offset });
            }
            NextFrame::Corrupt { offset, message } => {
                break Some(LogProblem::Corrupt { offset, message });
            }
        }
    };
    Ok(LogScan {
        header,
        first_seq,
        latest_seq,
        input_bytes,
        last_good_offset,
        problem,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot_protocol::{
        FRAME_FLAG_SUPERSEDED, FrameDirection, FrameRecord, SESSION_FRAME_VERSION,
        SessionFrameHeader,
    };

    fn header(record: FrameRecord, seq: u64) -> SessionFrameHeader {
        SessionFrameHeader {
            flags: 0,
            record,
            direction: FrameDirection::Output,
            stream: 0,
            seq,
            timestamp_ns: seq * 1_000,
        }
    }

    #[test]
    fn appends_and_reads_frames_in_order() {
        let dir =
            std::env::temp_dir().join(format!("boot-session-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut writer = SessionLogWriter::open(&dir, "ses_test", "serial", 1).unwrap();
        writer
            .append(&header(FrameRecord::Data, 1), b"hello")
            .unwrap();
        let mut input = header(FrameRecord::Data, 2);
        input.direction = FrameDirection::Input;
        writer.append(&input, b"\r").unwrap();
        writer
            .append(&header(FrameRecord::Data, 3), b"world")
            .unwrap();
        drop(writer);

        let scan = scan(&session_log_path(&dir, "ses_test")).unwrap();
        assert_eq!(scan.first_seq, 1);
        assert_eq!(scan.latest_seq, 3);
        assert_eq!(scan.input_bytes, 1);
        assert!(scan.problem.is_none());

        let mut reader = SessionLogReader::open(&session_log_path(&dir, "ses_test"))
            .unwrap()
            .unwrap();
        let mut payloads = Vec::new();
        loop {
            match reader.next_frame() {
                NextFrame::Frame(frame) => payloads.push(frame.payload),
                NextFrame::End => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(
            payloads,
            vec![b"hello".to_vec(), b"\r".to_vec(), b"world".to_vec()]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_mismatched_header_and_detects_corruption() {
        let dir = std::env::temp_dir().join(format!(
            "boot-session-log-corrupt-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = session_log_path(&dir, "ses_test");
        let mut writer = SessionLogWriter::open(&dir, "ses_test", "rtt", 1).unwrap();
        assert!(SessionLogWriter::open(&dir, "ses_test", "serial", 1).is_err());
        writer
            .append(
                &SessionFrameHeader {
                    flags: FRAME_FLAG_SUPERSEDED,
                    record: FrameRecord::Data,
                    direction: FrameDirection::Output,
                    stream: 2,
                    seq: 1,
                    timestamp_ns: 2,
                },
                b"data",
            )
            .unwrap();
        drop(writer);

        let raw = fs::read(&path).unwrap();
        let mut damaged = raw.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0x01;
        fs::write(&path, damaged).unwrap();
        let corrupt = scan(&path).unwrap();
        assert!(matches!(corrupt.problem, Some(LogProblem::Corrupt { .. })));
        assert_eq!(corrupt.latest_seq, 0);

        let mut truncated = raw;
        truncated.truncate(truncated.len() - 2);
        fs::write(&path, truncated).unwrap();
        let truncated = scan(&path).unwrap();
        assert!(matches!(
            truncated.problem,
            Some(LogProblem::Truncated { .. })
        ));
        assert_eq!(SESSION_FRAME_VERSION, 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
