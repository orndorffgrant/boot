//! Shared wire types for the boot HTTP and WebSocket APIs.
//!
//! This crate has no dependency on the server runtime, hardware crates, or
//! Dropshot. Both `boot-api` and `boot-api-client` use it for WebSocket
//! messages, and `boot-api` re-exports the HTTP DTOs for OpenAPI generation.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Probe {
    pub id: String,
    pub name: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
    pub hid_interface: Option<u8>,
    pub probe_type: String,
    pub generation: u64,
    pub state: String,
    pub identity_stable: bool,
    /// Last artifact successfully flashed through boot for this probe.
    pub last_artifact: Option<ProbeArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ProbeArtifact {
    pub id: String,
    pub size: u64,
    pub sha256: String,
    pub flashed_at: String,
    pub change: Option<String>,
    pub created_by: Option<String>,
    pub defmt: bool,
    pub rtt_addr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SerialPort {
    pub id: String,
    pub path: String,
    pub name: Option<String>,
    pub vendor_id: Option<u16>,
    pub product_id: Option<u16>,
    pub serial_number: Option<String>,
    pub generation: u64,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Target {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct LogEntry {
    pub time: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
    pub percent: Option<f64>,
    pub unit: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub summary: String,
    pub status: String,
    pub progress: Option<Progress>,
    pub spawn_time: String,
    pub ready_time: Option<String>,
    pub data: serde_json::Value,
    pub log: Vec<LogEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ChangeError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct Change {
    pub id: String,
    pub kind: String,
    pub summary: String,
    pub resource_id: String,
    pub created_by: String,
    pub status: String,
    pub ready: bool,
    pub spawn_time: String,
    pub ready_time: Option<String>,
    pub tasks: Vec<Task>,
    pub error: Option<ChangeError>,
    pub data: serde_json::Value,
    pub log: Vec<LogEntry>,
    pub log_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ChangeCreated {
    pub change: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct TaskUpdate {
    pub task: Task,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct FlashOptions {
    #[serde(default = "default_true")]
    pub reset: bool,
    #[serde(default = "default_true")]
    pub verify: bool,
}

impl Default for FlashOptions {
    fn default() -> Self {
        Self {
            reset: true,
            verify: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct FlashRequest {
    pub artifact: String,
    #[serde(default)]
    pub options: FlashOptions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ResetRequest {
    #[serde(default)]
    pub halt: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct InspectRequest {}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SessionOptions {
    /// RTT control block address, hexadecimal with or without `0x`.
    #[serde(default)]
    pub rtt_addr: Option<String>,
    /// RTT channels to expose. Empty means all up channels.
    #[serde(default)]
    pub channels: Vec<u8>,
    #[serde(default)]
    pub baud_rate: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ProbeSessionRequest {
    /// `rtt` or `dap`.
    pub kind: String,
    #[serde(default)]
    pub options: SessionOptions,
    /// Firmware artifact to use for RTT address lookup and defmt decoding.
    /// Overrides the probe's last flashed artifact.
    #[serde(default)]
    pub artifact: Option<String>,
    /// Inherit the probe's last flashed artifact when `artifact` is absent.
    #[serde(default = "default_true")]
    pub inherit_artifact: bool,
    #[serde(default = "default_max_subscribers")]
    pub max_subscribers: u32,
    #[serde(default = "default_detach_timeout")]
    pub detach_timeout_seconds: u64,
    #[serde(default = "default_wait_ms")]
    pub wait_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SerialSessionRequest {
    #[serde(default)]
    pub options: SessionOptions,
    #[serde(default = "default_max_subscribers")]
    pub max_subscribers: u32,
    #[serde(default = "default_detach_timeout")]
    pub detach_timeout_seconds: u64,
    #[serde(default = "default_wait_ms")]
    pub wait_ms: u64,
}

fn default_max_subscribers() -> u32 {
    8
}
fn default_detach_timeout() -> u64 {
    60
}
fn default_wait_ms() -> u64 {
    5000
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SessionData {
    pub oldest_seq: u64,
    pub latest_seq: u64,
    pub buffered_bytes: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SessionArtifact {
    pub id: String,
    pub size: u64,
    pub sha256: String,
    pub created_at: String,
    pub defmt: bool,
    pub rtt_addr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct SessionDecode {
    /// `defmt` or `raw`.
    pub mode: String,
    pub epoch: u64,
    pub artifact: Option<SessionArtifact>,
    /// RTT control block address actually used by the producer.
    pub rtt_addr: Option<String>,
    /// `explicit`, `artifact`, or `scan`.
    pub rtt_addr_source: String,
}

impl Default for SessionDecode {
    fn default() -> Self {
        Self {
            mode: "raw".to_string(),
            epoch: 0,
            artifact: None,
            rtt_addr: None,
            rtt_addr_source: "scan".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub kind: String,
    pub resource_id: String,
    pub created_by: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub subscribers: u32,
    pub max_subscribers: u32,
    pub detach_timeout_seconds: u64,
    pub resume_count: u64,
    pub replayable: bool,
    pub data: SessionData,
    pub decode: SessionDecode,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Subscriber {
    pub id: String,
    pub session_id: String,
    pub principal: String,
    pub mode: String,
    pub cursor: u64,
    pub attached_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct Artifact {
    pub id: String,
    pub size: u64,
    pub sha256: String,
    pub created_at: String,
    /// Whether deleting this artifact would conflict with a session, probe
    /// last-flash record, or in-flight change.
    pub in_use: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(default)]
pub struct SessionPatch {
    pub max_subscribers: Option<u32>,
    pub detach_timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ServerStatus {
    pub version: String,
    pub protocol_version: u8,
    pub uptime_seconds: u64,
    pub health: String,
    pub probes: usize,
    pub serial_ports: usize,
    pub changes: usize,
    pub sessions: usize,
    pub subscribers: usize,
    pub storage_bytes: u64,
}

/// Change-follow WebSocket messages. All frames are text JSON.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(tag = "type")]
pub enum ChangeWsMessage {
    #[serde(rename = "hello")]
    Hello {
        v: u8,
        protocol: u8,
        seq: u64,
        change: Change,
        #[serde(default)]
        replayed: bool,
    },
    #[serde(rename = "change.updated")]
    ChangeUpdated { v: u8, seq: u64, change: Change },
    #[serde(rename = "task.updated")]
    TaskUpdated { v: u8, seq: u64, task: Task },
    #[serde(rename = "change.log")]
    ChangeLog {
        v: u8,
        seq: u64,
        entries: Vec<LogEntry>,
    },
    #[serde(rename = "change.completed")]
    ChangeCompleted { v: u8, seq: u64, change: Change },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct RttRecord {
    /// Defmt timestamp rendering, when the table has timestamp data.
    pub timestamp: Option<String>,
    /// Lowercase level: trace, debug, info, warn, error.
    pub level: Option<String>,
    pub module: Option<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub message: String,
}

impl RttRecord {
    /// Render one record in the same shape the CLI has historically printed.
    pub fn display_line(&self) -> String {
        let level = self
            .level
            .as_deref()
            .map(str::to_ascii_uppercase)
            .unwrap_or_default();
        let location =
            if let Some(module) = self.module.as_deref().filter(|value| !value.is_empty()) {
                format!("{module}:{}", self.line.unwrap_or(0))
            } else if let Some(file) = self.file.as_deref() {
                format!("{file}:{}", self.line.unwrap_or(0))
            } else {
                "unknown:0".to_string()
            };
        let timestamp = self
            .timestamp
            .as_deref()
            .map(|value| format!("{value} "))
            .unwrap_or_default();
        format!("{timestamp}{level} {location} > {}\n", self.message)
    }
}

/// Text control messages for a session data WebSocket. Binary frames are data.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum SessionWsMessage {
    #[serde(rename = "hello")]
    Hello {
        v: u8,
        protocol: u8,
        subscriber_id: String,
        mode: String,
        session: Session,
        data_seq: u64,
        oldest_seq: u64,
        latest_seq: u64,
    },
    #[serde(rename = "session.active")]
    Active { session: Session },
    #[serde(rename = "session.suspended")]
    Suspended { session: Session },
    #[serde(rename = "session.resumed")]
    Resumed { session: Session },
    #[serde(rename = "session.degraded")]
    Degraded { session: Session, reason: String },
    #[serde(rename = "session.failed")]
    Failed { session: Session, error: String },
    #[serde(rename = "session.ended")]
    Ended { session: Session },
    #[serde(rename = "gap")]
    Gap {
        from_seq: u64,
        to_seq: u64,
        dropped_bytes: u64,
    },
    #[serde(rename = "session.input")]
    Input { seq: u64, data: String },
    #[serde(rename = "rtt.records")]
    RttRecords {
        seq: u64,
        channel: u8,
        epoch: u64,
        records: Vec<RttRecord>,
    },
    #[serde(rename = "rtt.raw")]
    RttRaw {
        seq: u64,
        channel: u8,
        epoch: u64,
        /// Base64; the bytes are not necessarily UTF-8.
        data: String,
    },
    #[serde(rename = "rtt.decode_error")]
    RttDecodeError {
        seq: u64,
        channel: u8,
        epoch: u64,
        message: String,
    },
    #[serde(rename = "session.decoder")]
    DecoderChanged { session: Session },
    #[serde(rename = "error")]
    Error { code: String, message: String },
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtt_record_messages_round_trip() {
        let message = SessionWsMessage::RttRecords {
            seq: 7,
            channel: 0,
            epoch: 2,
            records: vec![RttRecord {
                timestamp: Some("1.5".into()),
                level: Some("info".into()),
                module: Some("app".into()),
                file: Some("src/main.rs".into()),
                line: Some(7),
                message: "ready".into(),
            }],
        };
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("\"type\":\"rtt.records\""));
        assert_eq!(
            serde_json::from_str::<SessionWsMessage>(&json).unwrap(),
            message
        );

        let raw = SessionWsMessage::RttRaw {
            seq: 8,
            channel: 1,
            epoch: 0,
            data: "aGk=".into(),
        };
        let json = serde_json::to_string(&raw).unwrap();
        assert_eq!(
            serde_json::from_str::<SessionWsMessage>(&json).unwrap(),
            raw
        );
    }

    #[test]
    fn session_decode_defaults_to_raw() {
        let decode = SessionDecode::default();
        assert_eq!(decode.mode, "raw");
        assert_eq!(decode.epoch, 0);
        assert!(decode.artifact.is_none());
    }
}
