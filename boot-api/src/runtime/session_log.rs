//! Append-only session data log.
//!
//! Each session has one file. Records are length-prefixed and carry a monotonic
//! sequence, timestamp, type, and channel. The format is intentionally simple;
//! compression and segment rotation can be layered on later.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const RECORD_OUTPUT: u8 = 0;
pub const RECORD_INPUT: u8 = 1;
#[allow(dead_code)]
pub const RECORD_GAP: u8 = 2;
#[allow(dead_code)]
pub const RECORD_LIFECYCLE: u8 = 3;
pub const RECORD_DECODED: u8 = 4;

const MAGIC: &[u8; 4] = b"BLOG";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 4 + 1 + 1 + 1 + 2 + 8 + 8 + 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub kind: u8,
    pub channel: u8,
    pub payload: Vec<u8>,
}

pub fn session_log_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(session_id).join("session.log")
}

pub struct SessionLogWriter {
    file: File,
    path: PathBuf,
}

impl SessionLogWriter {
    pub fn open(root: &Path, session_id: &str) -> Result<Self> {
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
        let file = OpenOptions::new()
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
        Ok(Self { file, path })
    }

    pub fn append(&mut self, record: &LogRecord) -> Result<()> {
        let mut header = [0u8; HEADER_LEN];
        header[0..4].copy_from_slice(MAGIC);
        header[4] = VERSION;
        header[5] = record.kind;
        header[6] = record.channel;
        header[7..9].copy_from_slice(&0u16.to_be_bytes());
        header[9..17].copy_from_slice(&record.seq.to_be_bytes());
        header[17..25].copy_from_slice(&record.timestamp_ms.to_be_bytes());
        header[25..29].copy_from_slice(&(record.payload.len() as u32).to_be_bytes());
        self.file.write_all(&header)?;
        self.file.write_all(&record.payload)?;
        self.file.flush()?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn read_records(path: &Path) -> Result<Vec<LogRecord>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    let mut records = Vec::new();
    let mut offset = 0;
    while offset + HEADER_LEN <= data.len() {
        let header = &data[offset..offset + HEADER_LEN];
        if &header[0..4] != MAGIC || header[4] != VERSION {
            break;
        }
        let kind = header[5];
        let channel = header[6];
        let seq = u64::from_be_bytes(header[9..17].try_into().unwrap());
        let timestamp_ms = u64::from_be_bytes(header[17..25].try_into().unwrap());
        let length = u32::from_be_bytes(header[25..29].try_into().unwrap()) as usize;
        let start = offset + HEADER_LEN;
        let end = start.saturating_add(length);
        if end > data.len() {
            break;
        }
        records.push(LogRecord {
            seq,
            timestamp_ms,
            kind,
            channel,
            payload: data[start..end].to_vec(),
        });
        offset = end;
    }
    Ok(records)
}

pub fn latest_seq(path: &Path) -> u64 {
    read_records(path)
        .ok()
        .and_then(|records| records.last().map(|record| record.seq))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reads_records_in_order() {
        let dir =
            std::env::temp_dir().join(format!("boot-session-log-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut writer = SessionLogWriter::open(&dir, "ses_test").unwrap();
        writer
            .append(&LogRecord {
                seq: 1,
                timestamp_ms: 10,
                kind: RECORD_OUTPUT,
                channel: 0,
                payload: b"hello".to_vec(),
            })
            .unwrap();
        writer
            .append(&LogRecord {
                seq: 2,
                timestamp_ms: 20,
                kind: RECORD_INPUT,
                channel: 0,
                payload: b"\r".to_vec(),
            })
            .unwrap();
        let records = read_records(&session_log_path(&dir, "ses_test")).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].payload, b"hello");
        assert_eq!(records[1].kind, RECORD_INPUT);
        assert_eq!(latest_seq(&session_log_path(&dir, "ses_test")), 2);
        let _ = fs::remove_dir_all(&dir);
    }
}
