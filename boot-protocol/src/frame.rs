// SPDX-License-Identifier: AGPL-3.0-or-later
//! Versioned session data frames shared by the live data plane and the log.
//!
//! Wire layout (big-endian):
//!
//! ```text
//! offset size field
//! 0      4    magic "BFRM"
//! 4      1    format version
//! 5      1    flags
//! 6      1    record type
//! 7      1    direction
//! 8      1    stream
//! 9      3    reserved (zero)
//! 12     8    seq
//! 20     8    timestamp_ns
//! 28     4    payload length
//! 32     4    CRC32C
//! 36     N    payload
//! ```

use serde::{Deserialize, Serialize};

pub const SESSION_FRAME_MAGIC: [u8; 4] = *b"BFRM";
pub const SESSION_FRAME_VERSION: u8 = 1;
pub const SESSION_FRAME_HEADER_LEN: usize = 36;
pub const SESSION_FRAME_MAX_PAYLOAD: usize = 64 * 1024 * 1024;

pub const FRAME_FLAG_SUPERSEDED: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDirection {
    Output,
    Input,
}

impl FrameDirection {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Output => 0,
            Self::Input => 1,
        }
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Output),
            1 => Some(Self::Input),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRecord {
    Data,
    RttRecords,
    Gap,
    Mark,
    SessionHeader,
}

impl FrameRecord {
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::RttRecords => 1,
            Self::Gap => 2,
            Self::Mark => 3,
            Self::SessionHeader => 4,
        }
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Data),
            1 => Some(Self::RttRecords),
            2 => Some(Self::Gap),
            3 => Some(Self::Mark),
            4 => Some(Self::SessionHeader),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFrameHeader {
    pub flags: u8,
    pub record: FrameRecord,
    pub direction: FrameDirection,
    pub stream: u8,
    pub seq: u64,
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionFrame<'a> {
    pub header: SessionFrameHeader,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSessionFrame {
    pub header: SessionFrameHeader,
    pub payload: Vec<u8>,
}

impl OwnedSessionFrame {
    pub fn as_frame(&self) -> SessionFrame<'_> {
        SessionFrame {
            header: self.header,
            payload: &self.payload,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    TooShort,
    BadMagic,
    UnsupportedVersion,
    ReservedBytesSet,
    UnknownRecord,
    InvalidDirection,
    PayloadTooLarge,
    LengthMismatch,
    BadCrc,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::TooShort => "session frame is too short",
            Self::BadMagic => "session frame has bad magic",
            Self::UnsupportedVersion => "session frame version is unsupported",
            Self::ReservedBytesSet => "session frame reserved bytes are not zero",
            Self::UnknownRecord => "session frame has an unknown record type",
            Self::InvalidDirection => "session frame has an invalid direction",
            Self::PayloadTooLarge => "session frame payload is too large",
            Self::LengthMismatch => "session frame length does not match its payload",
            Self::BadCrc => "session frame CRC does not match",
        };
        f.write_str(message)
    }
}

impl std::error::Error for FrameError {}

pub fn encode_frame(header: &SessionFrameHeader, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > SESSION_FRAME_MAX_PAYLOAD {
        return Err(FrameError::PayloadTooLarge);
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::PayloadTooLarge)?;
    let mut frame = Vec::with_capacity(SESSION_FRAME_HEADER_LEN + payload.len());
    frame.extend_from_slice(&SESSION_FRAME_MAGIC);
    frame.push(SESSION_FRAME_VERSION);
    frame.push(header.flags);
    frame.push(header.record.as_u8());
    frame.push(header.direction.as_u8());
    frame.push(header.stream);
    frame.extend_from_slice(&[0, 0, 0]);
    frame.extend_from_slice(&header.seq.to_be_bytes());
    frame.extend_from_slice(&header.timestamp_ns.to_be_bytes());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&0u32.to_be_bytes());
    frame.extend_from_slice(payload);
    let crc = crc32c(&frame[..32], payload);
    frame[32..36].copy_from_slice(&crc.to_be_bytes());
    Ok(frame)
}

pub fn decode_frame(bytes: &[u8]) -> Result<SessionFrame<'_>, FrameError> {
    if bytes.len() < SESSION_FRAME_HEADER_LEN {
        return Err(FrameError::TooShort);
    }
    if bytes[..4] != SESSION_FRAME_MAGIC {
        return Err(FrameError::BadMagic);
    }
    if bytes[4] != SESSION_FRAME_VERSION {
        return Err(FrameError::UnsupportedVersion);
    }
    if bytes[9..12] != [0, 0, 0] {
        return Err(FrameError::ReservedBytesSet);
    }
    let record = FrameRecord::from_u8(bytes[6]).ok_or(FrameError::UnknownRecord)?;
    let direction = FrameDirection::from_u8(bytes[7]).ok_or(FrameError::InvalidDirection)?;
    let stream = bytes[8];
    let seq = u64::from_be_bytes(bytes[12..20].try_into().expect("checked length"));
    let timestamp_ns = u64::from_be_bytes(bytes[20..28].try_into().expect("checked length"));
    let payload_len =
        u32::from_be_bytes(bytes[28..32].try_into().expect("checked length")) as usize;
    if payload_len > SESSION_FRAME_MAX_PAYLOAD {
        return Err(FrameError::PayloadTooLarge);
    }
    if bytes.len() != SESSION_FRAME_HEADER_LEN + payload_len {
        return Err(FrameError::LengthMismatch);
    }
    let payload = &bytes[SESSION_FRAME_HEADER_LEN..];
    let expected = u32::from_be_bytes(bytes[32..36].try_into().expect("checked length"));
    if crc32c(&bytes[..32], payload) != expected {
        return Err(FrameError::BadCrc);
    }
    Ok(SessionFrame {
        header: SessionFrameHeader {
            flags: bytes[5],
            record,
            direction,
            stream,
            seq,
            timestamp_ns,
        },
        payload,
    })
}

/// CRC32C (Castagnoli) over `head` followed by `payload`.
pub fn crc32c(head: &[u8], payload: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in head.iter().chain(payload) {
        let index = ((crc ^ u32::from(*byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[index];
    }
    crc ^ 0xFFFF_FFFF
}

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RttRecordsPayload {
    #[serde(default = "default_payload_version")]
    pub v: u8,
    #[serde(default)]
    pub epoch: u64,
    #[serde(default)]
    pub records: Vec<crate::RttRecord>,
    /// Base64 bytes emitted after a malformed defmt frame.
    #[serde(default)]
    pub raw_fallback: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

fn default_payload_version() -> u8 {
    1
}

impl Default for RttRecordsPayload {
    fn default() -> Self {
        Self {
            v: 1,
            epoch: 0,
            records: Vec::new(),
            raw_fallback: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapPayload {
    pub from_seq: u64,
    pub to_seq: u64,
    pub dropped_bytes: u64,
}

impl GapPayload {
    pub const ENCODED_LEN: usize = 24;

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut payload = [0u8; Self::ENCODED_LEN];
        payload[0..8].copy_from_slice(&self.from_seq.to_be_bytes());
        payload[8..16].copy_from_slice(&self.to_seq.to_be_bytes());
        payload[16..24].copy_from_slice(&self.dropped_bytes.to_be_bytes());
        payload
    }

    pub fn decode(payload: &[u8]) -> Result<Self, FrameError> {
        if payload.len() != Self::ENCODED_LEN {
            return Err(FrameError::LengthMismatch);
        }
        Ok(Self {
            from_seq: u64::from_be_bytes(payload[0..8].try_into().expect("checked length")),
            to_seq: u64::from_be_bytes(payload[8..16].try_into().expect("checked length")),
            dropped_bytes: u64::from_be_bytes(payload[16..24].try_into().expect("checked length")),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MarkPayload {
    #[serde(default = "default_payload_version")]
    pub v: u8,
    pub event: String,
    #[serde(default)]
    pub detail: serde_json::Value,
}

impl MarkPayload {
    pub fn new(event: impl Into<String>, detail: serde_json::Value) -> Self {
        Self {
            v: 1,
            event: event.into(),
            detail,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionHeaderPayload {
    #[serde(default = "default_payload_version")]
    pub v: u8,
    pub protocol: u8,
    pub session_id: String,
    pub kind: String,
    pub created_at_ns: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> SessionFrameHeader {
        SessionFrameHeader {
            flags: FRAME_FLAG_SUPERSEDED,
            record: FrameRecord::Data,
            direction: FrameDirection::Input,
            stream: 3,
            seq: 0x0102_0304_0506_0708,
            timestamp_ns: 0x1112_1314_1516_1718,
        }
    }

    #[test]
    fn round_trips_a_frame() {
        let encoded = encode_frame(&header(), b"hello").unwrap();
        assert_eq!(encoded.len(), SESSION_FRAME_HEADER_LEN + 5);
        let decoded = decode_frame(&encoded).unwrap();
        assert_eq!(decoded.header, header());
        assert_eq!(decoded.payload, b"hello");
    }

    #[test]
    fn rejects_short_and_bad_frames() {
        assert_eq!(decode_frame(&[0; 35]).unwrap_err(), FrameError::TooShort);
        let encoded = encode_frame(&header(), b"hello").unwrap();
        let mut bad = encoded.clone();
        bad[0] = b'X';
        assert_eq!(decode_frame(&bad).unwrap_err(), FrameError::BadMagic);
        let mut bad = encoded.clone();
        bad[4] = 99;
        assert_eq!(
            decode_frame(&bad).unwrap_err(),
            FrameError::UnsupportedVersion
        );
        let mut bad = encoded.clone();
        bad[9] = 1;
        assert_eq!(
            decode_frame(&bad).unwrap_err(),
            FrameError::ReservedBytesSet
        );
        let mut bad = encoded.clone();
        bad[6] = 99;
        assert_eq!(decode_frame(&bad).unwrap_err(), FrameError::UnknownRecord);
        let mut bad = encoded;
        bad[SESSION_FRAME_HEADER_LEN] ^= 1;
        assert_eq!(decode_frame(&bad).unwrap_err(), FrameError::BadCrc);
    }

    #[test]
    fn encodes_and_decodes_gap_payload() {
        let gap = GapPayload {
            from_seq: 3,
            to_seq: 8,
            dropped_bytes: 1024,
        };
        assert_eq!(GapPayload::decode(&gap.encode()).unwrap(), gap);
        assert_eq!(
            GapPayload::decode(&[0; 23]).unwrap_err(),
            FrameError::LengthMismatch
        );
    }

    #[test]
    fn crc_matches_reference_vector() {
        // CRC32C of the ASCII string "123456789" is 0xE3069283.
        assert_eq!(crc32c(b"123456789", &[]), 0xE306_9283);
    }
}
