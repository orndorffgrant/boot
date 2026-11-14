use std::collections::HashMap;
use std::sync::Arc;

use defmt_decoder::Encoding;
use defmt_parser::Level;
use object::{Object, ObjectSection, ObjectSymbol};

use boot_protocol::{RttRecord, SessionArtifact};

use crate::runtime::artifact;
use crate::runtime::{RuntimeError, RuntimeInner};

#[derive(Debug)]
pub struct DecodeProfile {
    pub artifact_id: String,
    pub sha256: String,
    pub size: u64,
    pub created_at: String,
    pub rtt_addr: Option<u64>,
    pub table: Option<defmt_decoder::Table>,
    pub locations: Option<defmt_decoder::Locations>,
}

impl DecodeProfile {
    pub fn usable(&self) -> bool {
        self.table.is_some() || self.rtt_addr.is_some()
    }

    pub fn defmt(&self) -> bool {
        self.table.is_some()
    }

    pub fn session_artifact(&self) -> SessionArtifact {
        SessionArtifact {
            id: self.artifact_id.clone(),
            size: self.size,
            sha256: self.sha256.clone(),
            created_at: self.created_at.clone(),
            defmt: self.defmt(),
            rtt_addr: self.rtt_addr.map(|address| format!("0x{address:x}")),
        }
    }
}

#[derive(Debug)]
pub struct DecodedChunk {
    pub records: Vec<RttRecord>,
    pub raw: Option<Vec<u8>>,
    pub error: Option<String>,
}

pub struct DecodeState {
    profile: Arc<DecodeProfile>,
    epoch: u64,
    channels: HashMap<u8, Vec<u8>>,
}

impl DecodeState {
    pub fn new(profile: Arc<DecodeProfile>, epoch: u64) -> Self {
        Self {
            profile,
            epoch,
            channels: HashMap::new(),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn profile(&self) -> &Arc<DecodeProfile> {
        &self.profile
    }

    pub fn take_buffers(&mut self) -> Vec<(u8, Vec<u8>)> {
        self.channels
            .drain()
            .filter_map(|(channel, data)| (!data.is_empty()).then_some((channel, data)))
            .collect()
    }

    pub fn decode(&mut self, channel: u8, data: &[u8]) -> DecodedChunk {
        let Some(table) = self.profile.table.as_ref() else {
            return DecodedChunk {
                records: Vec::new(),
                raw: Some(data.to_vec()),
                error: None,
            };
        };
        let buffer = self.channels.entry(channel).or_default();
        buffer.extend_from_slice(data);

        let mut records = Vec::new();
        let mut error = None;
        loop {
            let decoded: Result<(), ()> = match table.encoding() {
                Encoding::Raw => match table.decode(buffer) {
                    Ok((frame, consumed)) => {
                        let record = render_record(&self.profile, &frame);
                        buffer.drain(0..consumed);
                        records.push(record);
                        continue;
                    }
                    Err(defmt_decoder::DecodeError::UnexpectedEof) => break,
                    Err(defmt_decoder::DecodeError::Malformed) => Err(()),
                },
                Encoding::Rzcobs => {
                    if buffer.first() == Some(&0) {
                        let start = buffer
                            .iter()
                            .position(|&byte| byte != 0)
                            .unwrap_or(buffer.len());
                        buffer.drain(0..start);
                    }
                    let Some(zero) = buffer.iter().position(|&byte| byte == 0) else {
                        break;
                    };
                    let frame = rzcobs_decode(&buffer[..zero]);
                    if let Some(nonzero) = buffer[zero..].iter().position(|&byte| byte != 0) {
                        buffer.drain(0..zero + nonzero);
                    } else {
                        buffer.clear();
                    }
                    match frame {
                        Ok(frame) => match table.decode(&frame) {
                            Ok((decoded, _consumed)) => {
                                records.push(render_record(&self.profile, &decoded));
                                continue;
                            }
                            Err(_) => Err(()),
                        },
                        Err(()) => Err(()),
                    }
                }
                _ => Err(()),
            };

            if decoded.is_err() {
                error = Some("malformed defmt frame".to_string());
                break;
            }
        }

        let raw = if error.is_some() && !buffer.is_empty() {
            Some(std::mem::take(buffer))
        } else {
            None
        };
        DecodedChunk {
            records,
            raw,
            error,
        }
    }
}

pub async fn load_profile(
    inner: &RuntimeInner,
    artifact_id: &str,
) -> Result<Arc<DecodeProfile>, RuntimeError> {
    let artifact = artifact::get(inner, artifact_id).await?;
    if let Some(profile) = inner
        .decode_cache
        .lock()
        .expect("decode cache")
        .get(artifact_id)
        .cloned()
    {
        return Ok(profile);
    }
    let path = artifact::path(inner, artifact_id).await?;
    let bytes = tokio::fs::read(&path).await.map_err(|error| {
        RuntimeError::new(
            "invalid_artifact",
            format!("failed to read artifact '{}': {error}", artifact.id),
        )
    })?;
    let profile = Arc::new(parse_profile(&artifact, &bytes)?);
    inner
        .decode_cache
        .lock()
        .expect("decode cache")
        .insert(artifact_id.to_string(), profile.clone());
    Ok(profile)
}

fn parse_profile(
    artifact: &boot_protocol::Artifact,
    bytes: &[u8],
) -> Result<DecodeProfile, RuntimeError> {
    let object = object::File::parse(bytes).map_err(|error| {
        RuntimeError::new(
            "invalid_artifact",
            format!("artifact '{}' is not a readable ELF: {error}", artifact.id),
        )
    })?;
    let mut rtt_addr = None;
    for symbol in object.symbols() {
        if symbol.name().ok() == Some("_SEGGER_RTT") {
            rtt_addr = Some(symbol.address());
            break;
        }
    }
    let has_defmt_section = object.sections().any(|section| {
        section
            .name()
            .map(|name| name == ".defmt" || name.starts_with(".defmt."))
            .unwrap_or(false)
    });
    let table = if has_defmt_section {
        defmt_decoder::Table::parse(bytes).map_err(|error| {
            RuntimeError::new(
                "invalid_artifact",
                format!("artifact '{}' has invalid defmt data: {error}", artifact.id),
            )
        })?
    } else {
        None
    };
    let locations = table
        .as_ref()
        .and_then(|table| table.get_locations(bytes).ok());
    if table.is_none() && rtt_addr.is_none() {
        return Err(RuntimeError::new(
            "invalid_artifact",
            format!(
                "artifact '{}' has neither _SEGGER_RTT nor defmt data",
                artifact.id
            ),
        ));
    }
    Ok(DecodeProfile {
        artifact_id: artifact.id.clone(),
        sha256: artifact.sha256.clone(),
        size: artifact.size,
        created_at: artifact.created_at.clone(),
        rtt_addr,
        table,
        locations,
    })
}

fn render_record(profile: &DecodeProfile, frame: &defmt_decoder::Frame<'_>) -> RttRecord {
    let location = profile
        .locations
        .as_ref()
        .and_then(|locations| locations.get(&frame.index()));
    RttRecord {
        timestamp: frame.display_timestamp().map(|value| value.to_string()),
        level: frame.level().map(|level| {
            match level {
                Level::Trace => "trace",
                Level::Debug => "debug",
                Level::Info => "info",
                Level::Warn => "warn",
                Level::Error => "error",
            }
            .to_string()
        }),
        module: location.map(|location| location.module.clone()),
        file: location.map(|location| location.file.display().to_string()),
        line: location.map(|location| location.line as u32),
        message: frame.display_message().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot_protocol::Artifact;

    fn artifact() -> Artifact {
        Artifact {
            id: "art_test".into(),
            size: 1,
            sha256: "sha".into(),
            created_at: "now".into(),
            in_use: false,
        }
    }

    #[test]
    fn rejects_non_elf_artifact() {
        let error = parse_profile(&artifact(), b"not an elf").unwrap_err();
        assert_eq!(error.code, "invalid_artifact");
    }

    #[test]
    fn finds_segger_rtt_without_defmt() {
        let bytes = include_bytes!("../../tests/fixtures/rtt.elf");
        let profile = parse_profile(&artifact(), bytes).unwrap();
        assert!(profile.table.is_none());
        assert!(profile.usable());
        assert!(profile.rtt_addr.is_some());
        assert_eq!(
            profile.session_artifact().rtt_addr,
            profile.rtt_addr.map(|address| format!("0x{address:x}"))
        );
    }

    #[test]
    fn profile_without_defmt_falls_back_to_raw() {
        let profile = Arc::new(DecodeProfile {
            artifact_id: "art_test".into(),
            sha256: "sha".into(),
            size: 1,
            created_at: "now".into(),
            rtt_addr: Some(0x2000_0000),
            table: None,
            locations: None,
        });
        assert!(profile.usable());
        assert!(!profile.defmt());
        let mut state = DecodeState::new(profile, 1);
        let chunk = state.decode(0, b"hello");
        assert!(chunk.records.is_empty());
        assert_eq!(chunk.raw.as_deref(), Some(b"hello".as_slice()));
        assert!(chunk.error.is_none());
    }

    #[test]
    fn rzcobs_rejects_malformed_frames() {
        assert_eq!(rzcobs_decode(&[0]), Err(()));
        assert_eq!(rzcobs_decode(&[]), Ok(Vec::new()));
    }
}

fn rzcobs_decode(data: &[u8]) -> Result<Vec<u8>, ()> {
    let mut result = Vec::new();
    let mut data = data.iter().rev().cloned();
    while let Some(byte) = data.next() {
        match byte {
            0 => return Err(()),
            0x01..=0x7f => {
                for bit in 0..7 {
                    if byte & (1 << (6 - bit)) == 0 {
                        result.push(data.next().ok_or(())?);
                    } else {
                        result.push(0);
                    }
                }
            }
            0x80..=0xfe => {
                let count = (byte & 0x7f) as usize + 7;
                result.push(0);
                for _ in 0..count {
                    result.push(data.next().ok_or(())?);
                }
            }
            0xff => {
                for _ in 0..134 {
                    result.push(data.next().ok_or(())?);
                }
            }
        }
    }
    result.reverse();
    Ok(result)
}
