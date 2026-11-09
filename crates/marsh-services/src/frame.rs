//! Frames for the services relay: typed, length-delimited and size-capped
//! (SECURITY-DESIGN A7 and A8).
//!
//! Wire format, in both directions: a big-endian `u32` length, then that many
//! bytes: one tag byte and the body.
//!
//! - tag 1: a JSON control frame (`HostFrame` toward the agent, `AgentFrame`
//!   toward the host);
//! - tag 2: channel data: a big-endian `u32` channel id and 1..=16 KiB of
//!   payload.
//!
//! The reader checks the declared size against the cap for the frame's kind
//! before it allocates or reads the body, so a hostile peer cannot make the
//! agent buffer more than a cap's worth. Anything unknown, oversized or
//! malformed is an `Err` the relay turns into "close with an error"; nothing
//! here panics on input.

use crate::engine::Generation;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{self, Read, Write};

pub const PROTOCOL: u32 = 1;
/// Payload bytes per data frame, and the unit of flow-control credit.
pub const CHUNK_BYTES: usize = 16 * 1024;
/// Credits a sender starts with per channel direction (64 chunks, 1 MiB).
pub const WINDOW_CHUNKS: u32 = 64;
/// Host-to-agent control frames are small requests.
pub const MAX_HOST_JSON_BYTES: usize = 64 * 1024;
/// Agent-to-host control frames carry manifests and listings.
pub const MAX_AGENT_JSON_BYTES: usize = 1024 * 1024;
/// Longest accepted identifier-like string (project, workspace, attempt).
pub const MAX_LABEL_BYTES: usize = 256;

const TAG_JSON: u8 = 1;
const TAG_DATA: u8 = 2;

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    TooLarge { length: usize, max: usize },
    Truncated,
    Empty,
    UnknownTag(u8),
    Malformed(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "transport error: {error}"),
            Self::TooLarge { length, max } => {
                write!(f, "oversized frame: {length} bytes (limit {max})")
            }
            Self::Truncated => write!(f, "truncated frame"),
            Self::Empty => write!(f, "empty frame"),
            Self::UnknownTag(tag) => write!(f, "unknown frame tag {tag}"),
            Self::Malformed(why) => write!(f, "malformed frame: {why}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(error)
        }
    }
}

#[derive(Debug)]
pub enum Inbound<T> {
    Control(T),
    Data { channel: u32, bytes: Vec<u8> },
}

#[derive(Debug)]
pub enum Outbound {
    Control(AgentFrame),
    Data { channel: u32, bytes: Vec<u8> },
}

/// Reads one frame. `Ok(None)` is a clean end of stream between frames.
///
/// # Errors
/// Returns a `FrameError` for transport failures and for frames that are
/// truncated, oversized, of an unknown kind, or whose body does not parse.
pub fn read_frame<R: Read, T: DeserializeOwned>(
    reader: &mut R,
    max_json: usize,
) -> Result<Option<Inbound<T>>, FrameError> {
    let mut first = [0_u8; 1];
    loop {
        match reader.read(&mut first) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    let mut rest = [0_u8; 3];
    reader.read_exact(&mut rest)?;
    let length = u32::from_be_bytes([first[0], rest[0], rest[1], rest[2]]) as usize;
    let widest = 1 + max_json.max(4 + CHUNK_BYTES);
    if length == 0 {
        return Err(FrameError::Empty);
    }
    if length > widest {
        return Err(FrameError::TooLarge {
            length,
            max: widest,
        });
    }
    let mut tag = [0_u8; 1];
    reader.read_exact(&mut tag)?;
    let body_length = length - 1;
    match tag[0] {
        TAG_JSON => {
            if body_length > max_json {
                return Err(FrameError::TooLarge {
                    length: body_length,
                    max: max_json,
                });
            }
            let mut body = vec![0_u8; body_length];
            reader.read_exact(&mut body)?;
            serde_json::from_slice(&body)
                .map(|value| Some(Inbound::Control(value)))
                .map_err(|error| FrameError::Malformed(error.to_string()))
        }
        TAG_DATA => {
            if body_length < 5 {
                return Err(FrameError::Malformed("data frame without payload".into()));
            }
            if body_length > 4 + CHUNK_BYTES {
                return Err(FrameError::TooLarge {
                    length: body_length - 4,
                    max: CHUNK_BYTES,
                });
            }
            let mut channel = [0_u8; 4];
            reader.read_exact(&mut channel)?;
            let mut bytes = vec![0_u8; body_length - 4];
            reader.read_exact(&mut bytes)?;
            Ok(Some(Inbound::Data {
                channel: u32::from_be_bytes(channel),
                bytes,
            }))
        }
        other => Err(FrameError::UnknownTag(other)),
    }
}

/// Writes one frame with a single `write_all` and a flush.
///
/// # Errors
/// Fails if the frame exceeds the agent-to-host caps or the write fails.
pub fn write_frame<W: Write>(writer: &mut W, frame: &Outbound) -> Result<(), FrameError> {
    let mut buffer: Vec<u8>;
    match frame {
        Outbound::Control(control) => {
            let body = serde_json::to_vec(control)
                .map_err(|error| FrameError::Malformed(error.to_string()))?;
            if body.len() > MAX_AGENT_JSON_BYTES {
                return Err(FrameError::TooLarge {
                    length: body.len(),
                    max: MAX_AGENT_JSON_BYTES,
                });
            }
            buffer = Vec::with_capacity(5 + body.len());
            buffer.extend_from_slice(&frame_length(1 + body.len())?);
            buffer.push(TAG_JSON);
            buffer.extend_from_slice(&body);
        }
        Outbound::Data { channel, bytes } => {
            if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
                return Err(FrameError::Malformed("data payload size".into()));
            }
            buffer = Vec::with_capacity(9 + bytes.len());
            buffer.extend_from_slice(&frame_length(5 + bytes.len())?);
            buffer.push(TAG_DATA);
            buffer.extend_from_slice(&channel.to_be_bytes());
            buffer.extend_from_slice(bytes);
        }
    }
    writer.write_all(&buffer)?;
    writer.flush()?;
    Ok(())
}

/// What the host writes: a control frame or channel data.
#[derive(Debug)]
pub enum HostOutbound<'a> {
    Control(&'a HostFrame),
    Data { channel: u32, bytes: &'a [u8] },
}

/// Writes one host-to-agent frame (`write_frame`'s counterpart for the host
/// side) with a single `write_all` and a flush.
///
/// # Errors
/// Fails if the frame exceeds the host-to-agent caps or the write fails.
pub fn write_host_frame<W: Write>(
    writer: &mut W,
    frame: &HostOutbound<'_>,
) -> Result<(), FrameError> {
    let mut buffer: Vec<u8>;
    match frame {
        HostOutbound::Control(control) => {
            let body = serde_json::to_vec(control)
                .map_err(|error| FrameError::Malformed(error.to_string()))?;
            if body.len() > MAX_HOST_JSON_BYTES {
                return Err(FrameError::TooLarge {
                    length: body.len(),
                    max: MAX_HOST_JSON_BYTES,
                });
            }
            buffer = Vec::with_capacity(5 + body.len());
            buffer.extend_from_slice(&frame_length(1 + body.len())?);
            buffer.push(TAG_JSON);
            buffer.extend_from_slice(&body);
        }
        HostOutbound::Data { channel, bytes } => {
            if bytes.is_empty() || bytes.len() > CHUNK_BYTES {
                return Err(FrameError::Malformed("data payload size".into()));
            }
            buffer = Vec::with_capacity(9 + bytes.len());
            buffer.extend_from_slice(&frame_length(5 + bytes.len())?);
            buffer.push(TAG_DATA);
            buffer.extend_from_slice(&channel.to_be_bytes());
            buffer.extend_from_slice(bytes);
        }
    }
    writer.write_all(&buffer)?;
    writer.flush()?;
    Ok(())
}

fn frame_length(length: usize) -> Result<[u8; 4], FrameError> {
    u32::try_from(length)
        .map(u32::to_be_bytes)
        .map_err(|_| FrameError::TooLarge {
            length,
            max: u32::MAX as usize,
        })
}

/// Host-attested context for a channel (A2): derived by the daemon from its
/// own state, never from request contents.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Authority {
    pub project: String,
    pub workspace: String,
    pub attempt: String,
    pub kind: String,
}

impl Authority {
    /// # Errors
    /// Returns the reason when a field is empty, too long or has control
    /// characters.
    pub fn validate(&self) -> Result<(), String> {
        validate_label(&self.project, "project")?;
        validate_label(&self.workspace, "workspace")?;
        validate_label(&self.attempt, "attempt")?;
        let kind = self.kind.as_bytes();
        let ok = (1..=32).contains(&kind.len())
            && kind[0].is_ascii_lowercase()
            && kind
                .iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
        if ok {
            Ok(())
        } else {
            Err("invalid authority kind".into())
        }
    }
}

/// A bounded display-safe string: 1..=256 bytes, no control characters.
///
/// # Errors
/// Returns the reason the value was rejected.
pub fn validate_label(value: &str, what: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES {
        return Err(format!("invalid {what}: length"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("invalid {what}: control character"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Docker,
    Tcp,
}

/// Optional tunables the daemon sets at `init` (from marsh settings). Every
/// value is clamped by the agent.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub freeze_budget_ms: Option<u64>,
    pub watchdog_ms: Option<u64>,
    pub lock_wait_ms: Option<u64>,
    pub stall_ms: Option<u64>,
    pub idle_ms: Option<u64>,
    pub max_channels: Option<u32>,
    pub max_channels_per_attempt: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostFrame {
    Init {
        protocol: u32,
        /// The generation the daemon expects (from `engine-up`'s `ready`).
        /// The agent derives its own and refuses on a mismatch.
        #[serde(default)]
        generation: Option<Generation>,
        #[serde(default)]
        settings: Settings,
    },
    Open {
        channel: u32,
        kind: ChannelKind,
        #[serde(default)]
        #[serde(skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        authority: Authority,
    },
    Credit {
        channel: u32,
        chunks: u32,
    },
    Eof {
        channel: u32,
    },
    Close {
        channel: u32,
    },
    /// `reserve_bytes`: the pool's free space (`fstatvfs` on the agent's own
    /// pool descriptor) must be at least this when the capture is admitted,
    /// checked by the agent immediately before it writes anything. The agent
    /// fails closed: an unreadable pool is not captured into.
    Capture {
        request: u64,
        id: String,
        workspace: String,
        reserve_bytes: u64,
    },
    /// The write fence of a stop cutover: freeze every listed workspace through
    /// the filtered endpoint's own freeze gate (mutations are refused, the
    /// requests already admitted finish first) and keep them frozen until
    /// `unfence` or until the agent exits (the VM stops).
    Fence {
        request: u64,
        workspaces: Vec<String>,
    },
    /// Release a fence (a stop that did not happen).
    Unfence {
        request: u64,
    },
    #[serde(rename = "drop")]
    DropSnapshot {
        request: u64,
        id: String,
    },
    List {
        request: u64,
    },
    /// Reap (E4): at an attempt's end, stop and remove the containers that
    /// the ledger and the labels both place in `workspace` for `attempt`
    /// (Compose and restart-policy containers are kept, DECISIONS 11).
    Reap {
        request: u64,
        workspace: String,
        attempt: String,
    },
    /// Restore, phase one (S4): create the fork's volumes in the lane's scope,
    /// install the snapshot's data, and return the lane files' contents (the
    /// daemon writes them, A9). `project` is the folder the checkpointed
    /// stacks' folders are relative to (the checkpointed project's root, or a
    /// lane's folder for a lane's own checkpoint), as the services VM mounts it;
    /// `lane_dir` is an absolute path strictly inside it, where the daemon has
    /// put the lane's copy of those folders. `workspace` is the lane's scope.
    RestorePrepare {
        request: u64,
        id: String,
        workspace: String,
        project: String,
        /// Where the checkpointed stacks' folders are relative to, when that
        /// is not `project`: a lane's own checkpoint records the first lane's
        /// folder, and the second lane is not inside it (C3, fork of a fork).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        lane_dir: String,
        fork: String,
    },
    /// Restore, phase two (S4): after the daemon wrote the lane files, bring
    /// the stacks up through the filter and run the post-restore check.
    RestoreUp {
        request: u64,
        workspace: String,
        project: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        lane_dir: String,
        fork: String,
    },
    /// Drop a workspace's objects (C3: a lane is closed, cancelled or
    /// removed): its containers, volumes and networks, then its ledger scope.
    /// By workspace only, through the same filter a job of that workspace
    /// would use, so nothing of another workspace can be reached.
    DropWorkspace {
        request: u64,
        workspace: String,
    },
    Resolve {
        request: u64,
        paths: Vec<String>,
    },
    Shutdown,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopePorts {
    pub workspace: String,
    pub ports: Vec<u16>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedPath {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotInfo {
    pub id: String,
    pub workspace: Option<String>,
    pub created_unix_ms: Option<u64>,
    pub complete: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentFrame {
    Ready {
        protocol: u32,
        generation: Generation,
        /// The agent's own process id inside the engine container.
        pid: u32,
        #[serde(default)]
        test_breaks_agent: Vec<String>,
        window_chunks: u32,
        chunk_bytes: usize,
    },
    Opened {
        channel: u32,
    },
    Refused {
        channel: u32,
        reason: String,
    },
    Credit {
        channel: u32,
        chunks: u32,
    },
    Eof {
        channel: u32,
    },
    Close {
        channel: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Full state: published loopback TCP ports per workspace scope.
    Ports {
        scopes: Vec<ScopePorts>,
        truncated: bool,
    },
    Captured {
        request: u64,
        id: String,
        /// `published`, `skipped` or `failed`.
        outcome: String,
        #[serde(default)]
        manifest: Option<serde_json::Value>,
        gaps: Vec<String>,
    },
    Dropped {
        request: u64,
        id: String,
        existed: bool,
    },
    /// The answer to `fence` (`held`: every workspace is frozen and stays so)
    /// and to `unfence` (`held: false`).
    Fenced {
        request: u64,
        held: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Listed {
        request: u64,
        snapshots: Vec<SnapshotInfo>,
    },
    /// The answer to `reap`. `complete` is false when something was left (a
    /// gap); the daemon keeps its record and asks again.
    Reaped {
        request: u64,
        removed: u32,
        kept: u32,
        gaps: Vec<String>,
        complete: bool,
    },
    Resolved {
        request: u64,
        paths: Vec<ResolvedPath>,
    },
    /// The answer to `restore_prepare`: fields only (the restore line is built
    /// on the Mac).
    RestorePrepared {
        request: u64,
        result: serde_json::Value,
    },
    /// The answer to `restore_up`: the same fields plus `check` and `ok`.
    Restored {
        request: u64,
        result: serde_json::Value,
    },
    /// The answer to `drop_workspace`: what was removed, and anything that is
    /// still there afterwards (display text, bounded).
    WorkspaceDropped {
        request: u64,
        workspace: String,
        containers: u32,
        volumes: u32,
        networks: u32,
        left: Vec<String>,
    },
    /// A failed request (`request` set) or a fatal relay error (`None`),
    /// after which the agent exits.
    Error {
        request: Option<u64>,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(frame: &Outbound) -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, frame).unwrap();
        out
    }

    #[test]
    fn host_frames_round_trip_and_unknown_types_are_rejected() {
        let json = br#"{"type":"close","channel":7}"#;
        let mut wire = Vec::new();
        wire.extend_from_slice(&u32::try_from(1 + json.len()).unwrap().to_be_bytes());
        wire.push(TAG_JSON);
        wire.extend_from_slice(json);
        let frame: Inbound<HostFrame> = read_frame(&mut wire.as_slice(), MAX_HOST_JSON_BYTES)
            .unwrap()
            .unwrap();
        assert!(matches!(
            frame,
            Inbound::Control(HostFrame::Close { channel: 7 })
        ));

        let bad = br#"{"type":"exec","channel":7}"#;
        let mut wire = Vec::new();
        wire.extend_from_slice(&u32::try_from(1 + bad.len()).unwrap().to_be_bytes());
        wire.push(TAG_JSON);
        wire.extend_from_slice(bad);
        assert!(matches!(
            read_frame::<_, HostFrame>(&mut wire.as_slice(), MAX_HOST_JSON_BYTES),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn oversized_and_unknown_frames_are_refused_before_reading_the_body() {
        // Declared length far beyond the cap; no body follows, and none is read.
        let wire = u32::MAX.to_be_bytes();
        assert!(matches!(
            read_frame::<_, HostFrame>(&mut wire.as_slice(), MAX_HOST_JSON_BYTES),
            Err(FrameError::TooLarge { .. })
        ));
        // A data frame over the chunk size.
        let mut wire = Vec::new();
        wire.extend_from_slice(&u32::try_from(5 + CHUNK_BYTES + 1).unwrap().to_be_bytes());
        wire.push(TAG_DATA);
        assert!(matches!(
            read_frame::<_, HostFrame>(&mut wire.as_slice(), MAX_HOST_JSON_BYTES),
            Err(FrameError::TooLarge { .. })
        ));
        // JSON over the host cap but under the widest cap.
        let mut wire = Vec::new();
        wire.extend_from_slice(
            &u32::try_from(1 + MAX_HOST_JSON_BYTES + 1)
                .unwrap()
                .to_be_bytes(),
        );
        wire.push(TAG_JSON);
        assert!(matches!(
            read_frame::<_, HostFrame>(&mut wire.as_slice(), MAX_HOST_JSON_BYTES),
            Err(FrameError::TooLarge { .. })
        ));
        let wire = [0, 0, 0, 2, 9, 0];
        assert!(matches!(
            read_frame::<_, HostFrame>(&mut wire.as_slice(), MAX_HOST_JSON_BYTES),
            Err(FrameError::UnknownTag(9))
        ));
        assert!(matches!(
            read_frame::<_, HostFrame>(&mut [0_u8, 0, 0, 0].as_slice(), MAX_HOST_JSON_BYTES),
            Err(FrameError::Empty)
        ));
    }

    #[test]
    fn data_frames_round_trip() {
        let wire = bytes_of(&Outbound::Data {
            channel: 3,
            bytes: vec![1, 2, 3],
        });
        let frame = read_frame::<_, HostFrame>(&mut wire.as_slice(), MAX_HOST_JSON_BYTES)
            .unwrap()
            .unwrap();
        assert!(matches!(frame, Inbound::Data { channel: 3, ref bytes } if bytes == &[1, 2, 3]));
        assert!(
            write_frame(
                &mut Vec::new(),
                &Outbound::Data {
                    channel: 1,
                    bytes: vec![0; CHUNK_BYTES + 1]
                }
            )
            .is_err()
        );
    }
}
