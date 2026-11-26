// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reusable change and session following with cursor replay and reconnect.

pub mod dap;

use std::time::Duration;

use boot_protocol::{
    Change, ChangeWsMessage, FrameDirection, FrameRecord, GapPayload, LogEntry, RttRecord,
    RttRecordsPayload, Session, SessionFrameHeader, SessionWsMessage, decode_frame, encode_frame,
};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{WebSocketStream, tungstenite};
use tokio_util::sync::CancellationToken;

use crate::error::{CliError, map_client_error};
use crate::protocol::convert;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectMode {
    None,
    Bounded(u32),
    UntilCancelled,
}

#[derive(Debug, Clone, Copy)]
pub struct ReconnectPolicy {
    pub mode: ReconnectMode,
    pub initial: Duration,
    pub max: Duration,
}

impl ReconnectPolicy {
    pub fn none() -> Self {
        Self {
            mode: ReconnectMode::None,
            initial: Duration::ZERO,
            max: Duration::ZERO,
        }
    }

    pub fn bounded(attempts: u32) -> Self {
        Self {
            mode: ReconnectMode::Bounded(attempts),
            initial: Duration::from_millis(250),
            max: Duration::from_secs(5),
        }
    }

    pub fn until_cancelled() -> Self {
        Self {
            mode: ReconnectMode::UntilCancelled,
            initial: Duration::from_millis(250),
            max: Duration::from_secs(5),
        }
    }

    fn delay(&self, attempt: u32) -> Option<Duration> {
        match self.mode {
            ReconnectMode::None => None,
            ReconnectMode::Bounded(max) if attempt > max => None,
            ReconnectMode::Bounded(_) | ReconnectMode::UntilCancelled => {
                let multiplier = 1u32 << attempt.min(5);
                Some((self.initial * multiplier).min(self.max))
            }
        }
    }

    fn exhausted(&self, attempt: u32) -> bool {
        matches!(self.mode, ReconnectMode::Bounded(max) if attempt > max)
    }
}

#[derive(Debug, Clone)]
pub enum ChangeEvent {
    Snapshot { change: Change, replayed: bool },
    Updated(Change),
    TaskUpdated(boot_protocol::Task),
    Log(Vec<LogEntry>),
    Completed(Change),
    Reconnecting { attempt: u32, delay: Duration },
}

pub async fn follow_change(
    server: &str,
    client: &boot_api_client::Client,
    change_id: &str,
    after: Option<u64>,
    policy: ReconnectPolicy,
    cancel: &CancellationToken,
    observer: &mut dyn FnMut(ChangeEvent),
) -> Result<Change, CliError> {
    let mut cursor = after;
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(CliError::Cancelled);
        }
        let upgraded = client
            .follow_change(change_id, cursor, None)
            .await
            .map_err(|error| map_client_error(server, error))?
            .into_inner();
        let ws = upgrade(upgraded).await;
        let (_tx, mut rx) = ws.split();
        let mut done = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err(CliError::Cancelled),
                message = rx.next() => {
                    let Some(message) = message else { break };
                    match message {
                        Ok(Message::Text(text)) => {
                            let update: ChangeWsMessage = serde_json::from_str(&text)?;
                            cursor = Some(cursor.unwrap_or(0).max(change_seq(&update)));
                            match update {
                                ChangeWsMessage::Hello { change, replayed, .. } => {
                                    observer(ChangeEvent::Snapshot { change: change.clone(), replayed });
                                    if change.ready {
                                        observer(ChangeEvent::Completed(change.clone()));
                                        done = Some(change);
                                        break;
                                    }
                                }
                                ChangeWsMessage::ChangeUpdated { change, .. } => {
                                    observer(ChangeEvent::Updated(change));
                                }
                                ChangeWsMessage::TaskUpdated { task, .. } => {
                                    observer(ChangeEvent::TaskUpdated(task));
                                }
                                ChangeWsMessage::ChangeLog { entries, .. } => {
                                    observer(ChangeEvent::Log(entries));
                                }
                                ChangeWsMessage::ChangeCompleted { change, .. } => {
                                    observer(ChangeEvent::Completed(change.clone()));
                                    done = Some(change);
                                    break;
                                }
                            }
                        }
                        Ok(Message::Close(_))
                        | Ok(Message::Ping(_))
                        | Ok(Message::Pong(_))
                        | Ok(Message::Binary(_))
                        | Ok(Message::Frame(_)) => {}
                        Err(_) => break,
                    }
                }
            }
        }
        if let Some(change) = done {
            return Ok(change);
        }
        attempt += 1;
        let Some(delay) = policy.delay(attempt) else {
            return Err(CliError::Transport {
                server: server.to_string(),
                message: format!("change follow for {change_id} ended unexpectedly"),
            });
        };
        if policy.exhausted(attempt) {
            return Err(CliError::Transport {
                server: server.to_string(),
                message: format!("change follow for {change_id} ended unexpectedly"),
            });
        }
        observer(ChangeEvent::Reconnecting { attempt, delay });
        tokio::select! {
            _ = cancel.cancelled() => return Err(CliError::Cancelled),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn change_seq(message: &ChangeWsMessage) -> u64 {
    match message {
        ChangeWsMessage::Hello { seq, .. }
        | ChangeWsMessage::ChangeUpdated { seq, .. }
        | ChangeWsMessage::TaskUpdated { seq, .. }
        | ChangeWsMessage::ChangeLog { seq, .. }
        | ChangeWsMessage::ChangeCompleted { seq, .. } => *seq,
    }
}

#[derive(Debug, Clone)]
pub struct SessionGap {
    pub from_seq: u64,
    pub to_seq: u64,
    pub dropped_bytes: u64,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Hello {
        session: Session,
        subscriber_id: String,
        seq: u64,
    },
    State(Session),
    DecoderChanged(Session),
    Raw {
        stream: u8,
        data: Vec<u8>,
    },
    RttRecords {
        stream: u8,
        records: Vec<RttRecord>,
    },
    RttDecodeError {
        stream: u8,
        message: String,
    },
    Input(Vec<u8>),
    Gap(SessionGap),
    Error {
        code: String,
        message: String,
    },
    Ended(Session),
    Reconnecting {
        attempt: u32,
        delay: Duration,
    },
}

#[derive(Debug, Clone)]
pub struct SessionOutcome {
    pub session: Session,
    pub detached: bool,
}

#[derive(Debug, Clone)]
pub struct SessionFollowOptions {
    pub after: Option<u64>,
    pub mode: &'static str,
    pub include_input: bool,
    pub replay: &'static str,
    pub policy: ReconnectPolicy,
    pub resume_on_suspend: bool,
    pub strict_replay: bool,
}

pub async fn follow_session(
    server: &str,
    client: &boot_api_client::Client,
    session_id: &str,
    options: &SessionFollowOptions,
    cancel: &CancellationToken,
    input: Option<&mut mpsc::Receiver<Vec<u8>>>,
    observer: &mut dyn FnMut(SessionEvent),
) -> Result<SessionOutcome, CliError> {
    let mut cursor = options.after;
    let mut attempt = 0u32;
    let mut input = input;
    let mut session: Option<Session> = None;
    loop {
        if cancel.is_cancelled() {
            return Err(CliError::Cancelled);
        }
        let upgraded = client
            .session_io(
                session_id,
                cursor,
                Some(options.include_input),
                Some(options.mode),
                Some(options.replay),
            )
            .await
            .map_err(|error| map_client_error(server, error))?
            .into_inner();
        let ws = upgrade(upgraded).await;
        let (mut tx, mut rx) = ws.split();
        let mut ended = false;
        let mut reconnect = true;
        loop {
            let input_future = async {
                match input.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = cancel.cancelled() => {
                    return Ok(SessionOutcome {
                        session: session.unwrap_or_else(|| placeholder_session(session_id)),
                        detached: true,
                    });
                }
                chunk = input_future => {
                    if let Some(chunk) = chunk {
                        let frame = encode_frame(
                            &SessionFrameHeader {
                                flags: 0,
                                record: FrameRecord::Data,
                                direction: FrameDirection::Input,
                                stream: 0,
                                seq: 0,
                                timestamp_ns: 0,
                            },
                            &chunk,
                        )
                        .map_err(|error| CliError::Config(error.to_string()))?;
                        if tx.send(Message::Binary(frame.into())).await.is_err() {
                            break;
                        }
                    } else {
                        input = None;
                    }
                }
                message = rx.next() => {
                    let Some(message) = message else { break };
                    match message {
                        Ok(Message::Text(text)) => {
                            let update: SessionWsMessage = serde_json::from_str(&text)?;
                            match update {
                                SessionWsMessage::Hello { subscriber_id, session: hello, data_seq, .. } => {
                                    cursor = Some(cursor.unwrap_or(0).max(data_seq));
                                    session = Some(hello.clone());
                                    observer(SessionEvent::Hello { session: hello, subscriber_id, seq: data_seq });
                                }
                                SessionWsMessage::Active { session: state }
                                | SessionWsMessage::Resumed { session: state }
                                | SessionWsMessage::Degraded { session: state, .. } => {
                                    session = Some(state.clone());
                                    observer(SessionEvent::State(state));
                                }
                                SessionWsMessage::Suspended { session: state } => {
                                    session = Some(state.clone());
                                    observer(SessionEvent::State(state.clone()));
                                    if options.resume_on_suspend {
                                        match client.resume_session(&state.id).await {
                                            Ok(response) => {
                                                let resumed: Session =
                                                    convert(response.into_inner())?;
                                                session = Some(resumed.clone());
                                                observer(SessionEvent::State(resumed));
                                            }
                                            Err(error) => {
                                                observer(SessionEvent::Error {
                                                    code: "resume_failed".to_string(),
                                                    message: error.to_string(),
                                                });
                                                return Err(map_client_error(server, error));
                                            }
                                        }
                                    } else {
                                        ended = true;
                                        reconnect = false;
                                        observer(SessionEvent::Ended(state));
                                        break;
                                    }
                                }
                                SessionWsMessage::Failed { session: state, error } => {
                                    observer(SessionEvent::Error {
                                        code: "session_failed".to_string(),
                                        message: error,
                                    });
                                    ended = true;
                                    reconnect = false;
                                    observer(SessionEvent::Ended(state));
                                    break;
                                }
                                SessionWsMessage::Ended { session: state } => {
                                    session = Some(state.clone());
                                    ended = true;
                                    reconnect = false;
                                    observer(SessionEvent::Ended(state));
                                    break;
                                }
                                SessionWsMessage::DecoderChanged { session: state } => {
                                    session = Some(state.clone());
                                    observer(SessionEvent::DecoderChanged(state));
                                }
                                SessionWsMessage::Error { code, message } => {
                                    observer(SessionEvent::Error { code, message });
                                }
                                SessionWsMessage::Ping | SessionWsMessage::Pong => {}
                            }
                        }
                        Ok(Message::Binary(data)) => {
                            match decode_frame(&data) {
                                Ok(frame) => {
                                    match frame.header.record {
                                        FrameRecord::Data => {
                                            cursor = Some(cursor.unwrap_or(0).max(frame.header.seq));
                                            match frame.header.direction {
                                                FrameDirection::Input => {
                                                    observer(SessionEvent::Input(frame.payload.to_vec()));
                                                }
                                                FrameDirection::Output => {
                                                    observer(SessionEvent::Raw {
                                                        stream: frame.header.stream,
                                                        data: frame.payload.to_vec(),
                                                    });
                                                }
                                            }
                                        }
                                        FrameRecord::RttRecords => {
                                            cursor = Some(cursor.unwrap_or(0).max(frame.header.seq));
                                            match serde_json::from_slice::<RttRecordsPayload>(frame.payload) {
                                                Ok(payload) => {
                                                    let stream = frame.header.stream;
                                                    if !payload.records.is_empty() {
                                                        observer(SessionEvent::RttRecords {
                                                            stream,
                                                            records: payload.records,
                                                        });
                                                    }
                                                    if let Some(raw) = payload.raw_fallback {
                                                        use base64::Engine;
                                                        if let Ok(bytes) = base64::prelude::BASE64_STANDARD.decode(raw) {
                                                            observer(SessionEvent::Raw { stream, data: bytes });
                                                        }
                                                    }
                                                    if let Some(message) = payload.error {
                                                        observer(SessionEvent::RttDecodeError { stream, message });
                                                    }
                                                }
                                                Err(error) => {
                                                    observer(SessionEvent::Error {
                                                        code: "bad_rtt_payload".to_string(),
                                                        message: error.to_string(),
                                                    });
                                                }
                                            }
                                        }
                                        FrameRecord::Gap => {
                                            match GapPayload::decode(frame.payload) {
                                                Ok(gap) => {
                                                    let event = SessionGap {
                                                        from_seq: gap.from_seq,
                                                        to_seq: gap.to_seq,
                                                        dropped_bytes: gap.dropped_bytes,
                                                    };
                                                    observer(SessionEvent::Gap(event));
                                                    if options.strict_replay {
                                                        return Err(CliError::Config(format!(
                                                            "session replay gap: missing data from {} to {}",
                                                            gap.from_seq, gap.to_seq
                                                        )));
                                                    }
                                                    cursor = Some(cursor.unwrap_or(0).max(gap.to_seq));
                                                }
                                                Err(error) => {
                                                    observer(SessionEvent::Error {
                                                        code: "bad_gap".to_string(),
                                                        message: error.to_string(),
                                                    });
                                                }
                                            }
                                        }
                                        FrameRecord::Mark | FrameRecord::SessionHeader => {}
                                    }
                                }
                                Err(error) => {
                                    observer(SessionEvent::Error {
                                        code: "bad_frame".to_string(),
                                        message: error.to_string(),
                                    });
                                }
                            }
                        }
                        Ok(Message::Close(_))
                        | Ok(Message::Ping(_))
                        | Ok(Message::Pong(_))
                        | Ok(Message::Frame(_)) => {}
                        Err(_) => break,
                    }
                }
            }
        }
        if ended || !reconnect {
            return Ok(SessionOutcome {
                session: session.unwrap_or_else(|| placeholder_session(session_id)),
                detached: false,
            });
        }
        attempt += 1;
        let Some(delay) = options.policy.delay(attempt) else {
            return Err(CliError::Transport {
                server: server.to_string(),
                message: format!("session follow for {session_id} ended unexpectedly"),
            });
        };
        if options.policy.exhausted(attempt) {
            return Err(CliError::Transport {
                server: server.to_string(),
                message: format!("session follow for {session_id} ended unexpectedly"),
            });
        }
        observer(SessionEvent::Reconnecting { attempt, delay });
        tokio::select! {
            _ = cancel.cancelled() => {
                return Ok(SessionOutcome {
                    session: session.unwrap_or_else(|| placeholder_session(session_id)),
                    detached: true,
                });
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn placeholder_session(id: &str) -> Session {
    Session {
        id: id.to_string(),
        kind: "unknown".to_string(),
        resource_id: String::new(),
        created_by: String::new(),
        state: "unknown".to_string(),
        created_at: String::new(),
        updated_at: String::new(),
        closed_at: None,
        subscribers: 0,
        max_subscribers: 0,
        detach_timeout_seconds: 0,
        resume_count: 0,
        replayable: false,
        data: Default::default(),
        decode: Default::default(),
        last_error: None,
    }
}

async fn upgrade(upgraded: reqwest::Upgraded) -> WebSocketStream<reqwest::Upgraded> {
    WebSocketStream::from_raw_socket(upgraded, tungstenite::protocol::Role::Client, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_none_stops_immediately() {
        let policy = ReconnectPolicy::none();
        assert!(policy.delay(1).is_none());
    }

    #[test]
    fn reconnect_bounded_exhausts() {
        let policy = ReconnectPolicy::bounded(2);
        assert!(policy.delay(1).is_some());
        assert!(policy.delay(2).is_some());
        assert!(policy.delay(3).is_none());
        assert!(policy.exhausted(3));
    }

    #[test]
    fn reconnect_backoff_is_capped() {
        let policy = ReconnectPolicy::until_cancelled();
        assert!(policy.delay(10).unwrap() <= policy.max);
    }
}
