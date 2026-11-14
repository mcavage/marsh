//! The daemon's side of the services relay (E1): a typed, bounded client of
//! the agent in `relay.rs`. SECURITY-DESIGN A2, A6, A7 and A8.
//!
//! One [`Relay`] is one long-lived transport (the stdio of one
//! `sbx exec ... --internal-services relay`) bound to one engine generation.
//! It never parses HTTP: channel contents are opaque bytes. Everything the VM
//! sends is hostile. Frames are strictly typed and size-capped (`frame.rs`),
//! and a state machine over the channel and request tables decides whether
//! each one is legal at all: an unknown or stale channel id, a duplicate
//! `opened`, a response nobody asked for, data beyond the credit window, a
//! `ready` for another generation, or a port list that is not 1-65535 and
//! consistent, tears this relay (and only this relay) down with a recorded
//! reason. The daemon, other projects' relays and the host filesystem are never
//! involved: nothing here acts on VM output beyond updating its own tables.
//!
//! Bounds (A7): channels are multiplexed over the one transport, so no file
//! descriptor per channel. Caps per attempt, per project (this relay) and per
//! daemon (a budget shared by all relays); credits of 16 KiB chunks with a 64
//! chunk window per channel and direction; control frames before data and
//! round-robin between channels on the one write path; a progress deadline on
//! every blocking wait, on the write path and on a frame that stops half way.
//! Buffers are bounded by the credit window, so a reader that stops reading
//! stalls its own channel and nothing else.

use crate::breaks;
use crate::engine::Generation;
use crate::frame::{
    AgentFrame, Authority, CHUNK_BYTES, ChannelKind, FrameError, HostFrame, HostOutbound, Inbound,
    MAX_AGENT_JSON_BYTES, PROTOCOL, Settings, WINDOW_CHUNKS, read_frame, write_host_frame,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Credits go back to the agent in batches (the agent's own batch size).
const CREDIT_BATCH: u32 = 16;
/// Chunks a stream queues for sending before `write` waits. The agent's credit
/// window bounds what is in flight; this bounds what waits here.
const OUT_QUEUE_CHUNKS: usize = 16;
/// Requests in flight, answered or not (the agent refuses more).
const MAX_REQUESTS_IN_FLIGHT: usize = 8;
const MAX_SCOPES: usize = 256;
const MAX_PORTS_PER_SCOPE: usize = 512;
const MAX_SNAPSHOTS: usize = 4096;
const MAX_GAPS: usize = 256;
/// A workspace drop reports at most this many leftovers, and counts below this.
const MAX_DROP_LEFT: usize = 32;
const MAX_DROP_COUNT: u32 = 100_000;
const MAX_TEXT_BYTES: usize = 1024;
const MAX_RESULT_DEPTH: usize = 12;
/// Closed channel ids remembered, so frames that were already in flight when a
/// channel ended are not mistaken for forgeries.
const TOMBSTONES: usize = 8192;
/// A request nobody answered for this long ends the relay.
const ABANDONED_REQUEST_LIMIT: Duration = Duration::from_secs(300);
/// Added to every frame written under the `slow-relay` measurement control.
const SLOW_RELAY_DELAY: Duration = Duration::from_millis(2);
/// Under `relay-drop-chunk`, one data chunk in this many is lost.
const DROP_EVERY: u64 = 700;

/// The workspace of a project's base tree (lanes get their own in C3).
pub const BASE_WORKSPACE: &str = "base";

/// Settings with documented defaults (`docs/design/services.md`).
#[derive(Clone, Debug)]
pub struct Limits {
    /// Channels per attempt (A7: 64).
    pub per_attempt: usize,
    /// Channels per project, that is per relay (A7: 256).
    pub per_project: usize,
    /// No progress (a blocked write, a half frame, a full queue) for this long
    /// ends the channel or the relay (60 s).
    pub stall: Duration,
    /// A frame that has started arriving must finish within this (4 s).
    pub frame_stall: Duration,
    /// A channel with no traffic for this long is closed (30 min).
    pub idle: Duration,
    /// How long an `open` waits for the agent's answer (30 s).
    pub open_timeout: Duration,
    /// How long the agent may take to report `ready` (120 s).
    pub ready_timeout: Duration,
    /// How long a capture, drop or list may take (5 min).
    pub request_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_attempt: 64,
            per_project: 256,
            stall: Duration::from_secs(60),
            frame_stall: Duration::from_secs(4),
            idle: Duration::from_mins(30),
            open_timeout: Duration::from_secs(30),
            ready_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_mins(5),
        }
    }
}

/// The per-daemon channel cap (A7: 1024), shared by every relay.
#[derive(Debug)]
pub struct DaemonBudget {
    cap: usize,
    used: AtomicUsize,
}

impl DaemonBudget {
    #[must_use]
    pub fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            cap,
            used: AtomicUsize::new(0),
        })
    }

    fn acquire(&self) -> bool {
        self.used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < self.cap).then_some(used + 1)
            })
            .is_ok()
    }

    fn release(&self, count: usize) {
        let _ = self
            .used
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                Some(used.saturating_sub(count))
            });
    }

    #[must_use]
    pub fn in_use(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RelayError {
    /// The relay is not ready or is gone (the reason).
    Unavailable(String),
    /// A request that is malformed before anything is sent.
    Invalid(String),
    /// The daemon's own state does not authorize it (for example a TCP port
    /// that is not in the workspace's published map).
    NotAuthorized(String),
    /// A channel cap.
    Limit(String),
    /// The agent refused.
    Refused(String),
    Timeout(String),
    /// The agent answered a request with an error.
    Agent(String),
    /// The relay ended while it started because of what the VM sent: the
    /// project is quarantined.
    Hostile(String),
}

impl fmt::Display for RelayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(why) | Self::Hostile(why) => {
                write!(f, "marsh: services relay unavailable: {why}")
            }
            Self::Invalid(why) | Self::NotAuthorized(why) | Self::Limit(why) => {
                write!(f, "marsh: {why}")
            }
            Self::Refused(why) => write!(f, "marsh: the services agent refused: {why}"),
            Self::Timeout(what) => write!(f, "marsh: timed out waiting for {what}"),
            Self::Agent(why) => write!(f, "marsh: the services agent failed the request: {why}"),
        }
    }
}

impl std::error::Error for RelayError {}

/// Who a channel is for, from the daemon's own state: the project, the
/// workspace (the base workspace until lanes exist) and the job's attempt.
/// Never built from anything the VM or a job said.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptScope {
    project_root: PathBuf,
    workspace: String,
    attempt: String,
}

fn scope_id_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && !value.starts_with('.')
}

impl AttemptScope {
    /// # Errors
    /// `Invalid` when the root is not an absolute UTF-8 path or an id is not a
    /// valid scope id (the policy's own rule: 1..=64 of `[A-Za-z0-9_.-]`, no
    /// leading dot).
    pub fn new(
        project_root: impl Into<PathBuf>,
        workspace: &str,
        attempt: &str,
    ) -> Result<Self, RelayError> {
        let project_root = project_root.into();
        if !project_root.is_absolute() || project_root.to_str().is_none() {
            return Err(RelayError::Invalid(
                "the project root must be an absolute path".into(),
            ));
        }
        // Ordinary names only: the root is the folder bind sources resolve
        // beneath, and a lane's is inside the project's.
        if !project_root.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        }) {
            return Err(RelayError::Invalid(
                "the project root must be a plain path".into(),
            ));
        }
        if !scope_id_ok(workspace) {
            return Err(RelayError::Invalid(format!(
                "invalid workspace id {workspace:?}"
            )));
        }
        if !scope_id_ok(attempt) {
            return Err(RelayError::Invalid(format!(
                "invalid attempt id {attempt:?}"
            )));
        }
        Ok(Self {
            project_root,
            workspace: workspace.to_owned(),
            attempt: attempt.to_owned(),
        })
    }

    #[must_use]
    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    fn authority(&self, kind: String) -> Authority {
        Authority {
            project: self.project_root.to_string_lossy().into_owned(),
            workspace: self.workspace.clone(),
            attempt: self.attempt.clone(),
            kind,
        }
    }
}

/// What the transport gives the relay.
pub struct RelayConfig {
    pub project: PathBuf,
    /// The generation the daemon got from the engine's `ready`. The agent's
    /// `ready` must carry exactly this one.
    pub expected: Generation,
    pub limits: Limits,
    pub budget: Arc<DaemonBudget>,
    /// Test fault `fault-forge-tcp`: the port a TCP open is rewritten to.
    pub forge_tcp_port: Option<u16>,
}

/// The restore request fields, checked the way the agent checks them: a valid
/// workspace and fork, ordinary absolute paths, and the lane folder strictly
/// inside the project.
fn check_restore_fields(
    workspace: &str,
    project: &str,
    source: Option<&str>,
    lane_dir: &str,
    fork: &str,
) -> Result<(), RelayError> {
    if !scope_id_ok(workspace) {
        return Err(RelayError::Invalid(format!(
            "invalid workspace id {workspace:?}"
        )));
    }
    if !crate::restore::valid_fork(fork) {
        return Err(RelayError::Invalid(
            "invalid fork: a compose project name of at most 48 characters".into(),
        ));
    }
    let project_parts = crate::restore::components(project)
        .map_err(|why| RelayError::Invalid(format!("invalid project path: {why}")))?;
    if let Some(source) = source {
        let source_parts = crate::restore::components(source)
            .map_err(|why| RelayError::Invalid(format!("invalid source path: {why}")))?;
        if source_parts.len() < project_parts.len()
            || source_parts[..project_parts.len()] != project_parts[..]
        {
            return Err(RelayError::Invalid(
                "the source folder must be inside the project".into(),
            ));
        }
    }
    let lane_parts = crate::restore::components(lane_dir)
        .map_err(|why| RelayError::Invalid(format!("invalid lane folder: {why}")))?;
    if lane_parts.len() <= project_parts.len()
        || lane_parts[..project_parts.len()] != project_parts[..]
    {
        return Err(RelayError::Invalid(
            "the lane folder must be strictly inside the project".into(),
        ));
    }
    Ok(())
}

/// A snapshot id from the host: `[A-Za-z0-9_-]`, 1..=64, not starting with `-`.
fn snapshot_id_ok(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn json_depth(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Array(items) => 1 + items.iter().map(json_depth).max().unwrap_or(0),
        serde_json::Value::Object(map) => 1 + map.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// VM-supplied text made safe to store, log and show: control characters
/// (escapes, newlines) become spaces and the length is bounded.
#[must_use]
pub fn plain_text(text: &str) -> String {
    let mut cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.len() > MAX_TEXT_BYTES {
        let mut end = MAX_TEXT_BYTES;
        while !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned.truncate(end);
    }
    cleaned
}

#[derive(Debug)]
pub struct CaptureReply {
    pub id: String,
    /// `published`, `skipped` or `failed`.
    pub outcome: String,
    pub manifest: Option<serde_json::Value>,
    pub gaps: Vec<String>,
}

#[derive(Debug)]
pub struct SnapshotEntry {
    pub id: String,
    pub workspace: Option<String>,
    pub created_unix_ms: Option<u64>,
    pub complete: Option<bool>,
}

/// What `restore_prepare` and `restore_up` return: the agent's fields as a JSON
/// object (fields only; the restore line is built on the Mac). `prepare`'s
/// carry the *contents* of the lane files the daemon writes (A9) and are
/// therefore not stripped of newlines here: the caller validates every field
/// before it uses one, and writes files only with `O_NOFOLLOW` beneath a held
/// directory descriptor.
#[derive(Debug)]
pub struct RestoreReply {
    pub result: serde_json::Value,
}

/// What an attempt's reap did (E4). `complete` is false when something was left.
#[derive(Debug)]
pub struct ReapReply {
    pub removed: u32,
    pub kept: u32,
    pub gaps: Vec<String>,
    pub complete: bool,
}

/// What `drop_workspace` came to (C3): counts of what the agent removed, and
/// what is still there (display text, bounded and plain).
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct WorkspaceDrop {
    pub containers: u32,
    pub volumes: u32,
    pub networks: u32,
    pub left: Vec<String>,
}

/// The agent's answer to a fence or unfence request.
#[derive(Debug)]
pub struct FenceReply {
    /// Every requested workspace is frozen and stays so.
    pub held: bool,
    pub reason: Option<String>,
}

enum Reply {
    Captured(CaptureReply),
    Fenced(FenceReply),
    Dropped(bool),
    Listed(Vec<SnapshotEntry>),
    Reaped(ReapReply),
    Restore(RestoreReply),
    WorkspaceDropped(WorkspaceDrop),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RestorePhase {
    Prepare,
    Up,
}

enum RequestKind {
    Capture(String),
    Fence,
    Unfence,
    Drop(String),
    List,
    Reap,
    Restore(RestorePhase),
    DropWorkspace(String),
}

struct RequestSlot {
    result: Mutex<Option<Result<Reply, RelayError>>>,
    cv: Condvar,
}

struct PendingRequest {
    kind: RequestKind,
    slot: Arc<RequestSlot>,
    since: Instant,
    abandoned: bool,
}

#[derive(Default)]
struct Ports {
    received: bool,
    scopes: BTreeMap<String, BTreeSet<u16>>,
    version: u64,
}

/// Counters for evidence and `marsh services status`.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub opens_sent: u64,
    pub tcp_opens_sent: u64,
    pub opened: u64,
    pub refused: u64,
    pub data_frames_in: u64,
    pub data_frames_out: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub control_in: u64,
    pub control_out: u64,
    pub ports_frames: u64,
    pub dropped_by_break: u64,
}

enum Item {
    Data(Vec<u8>),
    Eof,
}

enum Phase {
    Pending,
    Open,
    /// Ended; the reason when it was not a clean end.
    Closed(Option<String>),
}

#[allow(clippy::struct_excessive_bools)]
struct ChanState {
    phase: Phase,
    refusal: Option<String>,
    inbound: VecDeque<Vec<u8>>,
    front_offset: usize,
    /// Chunks the agent sent that we have not returned credit for.
    outstanding: u32,
    consumed: u32,
    agent_eof: bool,
    out_credit: u32,
    out: VecDeque<Item>,
    in_ring: bool,
    host_eof_queued: bool,
    host_eof_sent: bool,
    sent: u64,
    last_activity: Instant,
    created: Instant,
}

struct Chan {
    id: u32,
    attempt: (String, String),
    st: Mutex<ChanState>,
    cv: Condvar,
}

impl Chan {
    fn lock(&self) -> MutexGuard<'_, ChanState> {
        self.st
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

type CloseHook = Box<dyn Fn(&str) + Send>;

enum RelayPhase {
    Handshake,
    Ready,
    Closed(String),
}

struct State {
    phase: RelayPhase,
    agent_breaks: Vec<String>,
    agent_pid: Option<u32>,
    channels: HashMap<u32, Arc<Chan>>,
    next_channel: u32,
    tomb: VecDeque<u32>,
    tomb_set: HashSet<u32>,
    per_attempt: HashMap<(String, String), usize>,
    control: VecDeque<HostFrame>,
    ring: VecDeque<Arc<Chan>>,
    requests: HashMap<u64, PendingRequest>,
    next_request: u64,
    ports: Ports,
    stats: Stats,
    writing_since: Option<Instant>,
    mid_frame_since: Option<Instant>,
    /// The relay ended because of what the VM sent (quarantine).
    violated: bool,
    close_hooks: Vec<CloseHook>,
}

struct Inner {
    project: String,
    expected: Generation,
    limits: Limits,
    budget: Arc<DaemonBudget>,
    forge_tcp_port: Option<u16>,
    closer: Box<dyn Fn() + Send + Sync>,
    st: Mutex<State>,
    /// Wakes the writer thread.
    wake: Condvar,
    /// Wakes `start` and every waiter on the relay's own state.
    changed: Condvar,
}

/// One relay to one engine generation of one project's services VM.
#[derive(Clone)]
pub struct Relay {
    inner: Arc<Inner>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.st
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn closed_reason(state: &State) -> Option<String> {
        match &state.phase {
            RelayPhase::Closed(reason) => Some(reason.clone()),
            _ => None,
        }
    }

    /// Ends the relay: the first reason wins. Every channel and request fails,
    /// the caps are given back, and the transport is closed.
    /// Ends the relay because of what the VM sent: the project is
    /// quarantined (no relay is started again without an explicit start).
    fn teardown_hostile(&self, reason: &str) {
        {
            let mut state = self.lock();
            if !matches!(state.phase, RelayPhase::Closed(_)) {
                state.violated = true;
            }
        }
        self.teardown(reason);
    }

    fn teardown(&self, reason: &str) {
        // The reason can carry VM text (serde echoes what it was sent): it is
        // made plain before it is stored, logged or shown.
        let reason = plain_text(reason);
        let reason = reason.as_str();
        let (channels, requests, hooks) = {
            let mut state = self.lock();
            if matches!(state.phase, RelayPhase::Closed(_)) {
                return;
            }
            state.phase = RelayPhase::Closed(reason.to_owned());
            let channels: Vec<Arc<Chan>> = state.channels.drain().map(|(_, c)| c).collect();
            self.budget.release(channels.len());
            state.per_attempt.clear();
            state.ring.clear();
            state.control.clear();
            let requests: Vec<PendingRequest> = state.requests.drain().map(|(_, r)| r).collect();
            (channels, requests, std::mem::take(&mut state.close_hooks))
        };
        eprintln!("marsh services relay (host): closed: {reason}");
        for channel in channels {
            let mut st = channel.lock();
            if !matches!(st.phase, Phase::Closed(_)) {
                st.phase = Phase::Closed(Some(reason.to_owned()));
            }
            drop(st);
            channel.cv.notify_all();
        }
        for request in requests {
            *request
                .slot
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(Err(RelayError::Unavailable(reason.to_owned())));
            request.slot.cv.notify_all();
        }
        (self.closer)();
        self.wake.notify_all();
        self.changed.notify_all();
        for hook in hooks {
            hook(reason);
        }
    }

    /// A frame or state the VM must never produce. With
    /// `vm-frame-validation-off` (a test defect) it is ignored.
    fn violation(&self, why: &str) -> bool {
        if breaks::break_mode("vm-frame-validation-off") {
            return false;
        }
        self.teardown_hostile(&format!("protocol violation by the VM: {why}"));
        true
    }

    fn remember_closed(state: &mut State, id: u32) {
        if state.tomb_set.insert(id) {
            state.tomb.push_back(id);
            while state.tomb.len() > TOMBSTONES {
                if let Some(old) = state.tomb.pop_front() {
                    state.tomb_set.remove(&old);
                }
            }
        }
    }

    /// Removes a channel from the tables and gives its caps back.
    fn remove_channel(&self, state: &mut State, id: u32) -> Option<Arc<Chan>> {
        let chan = state.channels.remove(&id)?;
        if let Some(count) = state.per_attempt.get_mut(&chan.attempt) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.per_attempt.remove(&chan.attempt);
            }
        }
        self.budget.release(1);
        Self::remember_closed(state, id);
        Some(chan)
    }

    fn end_channel(&self, id: u32, reason: Option<String>, tell_agent: bool) {
        let chan = {
            let mut state = self.lock();
            let chan = self.remove_channel(&mut state, id);
            if chan.is_some() && tell_agent {
                state.control.push_back(HostFrame::Close { channel: id });
            }
            chan
        };
        if let Some(chan) = chan {
            let mut st = chan.lock();
            if !matches!(st.phase, Phase::Closed(_)) {
                st.phase = Phase::Closed(reason);
            }
            st.out.clear();
            drop(st);
            chan.cv.notify_all();
            self.wake.notify_all();
        }
    }

    fn lookup(&self, id: u32) -> Lookup {
        let state = self.lock();
        if let Some(chan) = state.channels.get(&id) {
            Lookup::Live(Arc::clone(chan))
        } else if state.tomb_set.contains(&id) {
            Lookup::Closed
        } else {
            Lookup::Unknown
        }
    }

    // ------------------------------------------------------------ reading

    #[allow(clippy::too_many_lines)]
    fn handle(&self, frame: AgentFrame) -> bool {
        {
            let mut state = self.lock();
            state.stats.control_in += 1;
        }
        match frame {
            AgentFrame::Ready { .. } => self.violation("a second ready"),
            AgentFrame::Opened { channel } => match self.lookup(channel) {
                Lookup::Live(chan) => {
                    let mut st = chan.lock();
                    if matches!(st.phase, Phase::Pending) {
                        st.phase = Phase::Open;
                        st.last_activity = Instant::now();
                        drop(st);
                        self.lock().stats.opened += 1;
                        chan.cv.notify_all();
                        false
                    } else {
                        drop(st);
                        self.violation(&format!("opened for channel {channel} twice"))
                    }
                }
                Lookup::Closed => false,
                Lookup::Unknown => self.violation(&format!("opened for unknown channel {channel}")),
            },
            AgentFrame::Refused { channel, reason } => match self.lookup(channel) {
                Lookup::Live(chan) => {
                    let pending = matches!(chan.lock().phase, Phase::Pending);
                    if !pending {
                        return self.violation(&format!("refused for open channel {channel}"));
                    }
                    chan.lock().refusal = Some(plain_text(&reason));
                    {
                        let mut state = self.lock();
                        state.stats.refused += 1;
                    }
                    self.end_channel(channel, Some(plain_text(&reason)), false);
                    false
                }
                Lookup::Closed => false,
                Lookup::Unknown => {
                    self.violation(&format!("refused for unknown channel {channel}"))
                }
            },
            AgentFrame::Credit { channel, chunks } => match self.lookup(channel) {
                Lookup::Live(chan) => {
                    let mut st = chan.lock();
                    if !matches!(st.phase, Phase::Open) {
                        drop(st);
                        return self.violation(&format!("credit on channel {channel} before open"));
                    }
                    let total = st.out_credit.saturating_add(chunks);
                    if chunks == 0 || total > WINDOW_CHUNKS {
                        drop(st);
                        return self.violation(&format!(
                            "credit of {chunks} on channel {channel} exceeds the window"
                        ));
                    }
                    st.out_credit = total;
                    let ring = !st.out.is_empty() && !st.in_ring;
                    if ring {
                        st.in_ring = true;
                    }
                    drop(st);
                    chan.cv.notify_all();
                    if ring {
                        self.lock().ring.push_back(chan);
                    }
                    self.wake.notify_all();
                    false
                }
                Lookup::Closed => false,
                Lookup::Unknown => self.violation(&format!("credit for unknown channel {channel}")),
            },
            AgentFrame::Eof { channel } => match self.lookup(channel) {
                Lookup::Live(chan) => {
                    let mut st = chan.lock();
                    if !matches!(st.phase, Phase::Open) || st.agent_eof {
                        drop(st);
                        return self.violation(&format!("unexpected eof on channel {channel}"));
                    }
                    st.agent_eof = true;
                    let both = st.host_eof_sent;
                    drop(st);
                    chan.cv.notify_all();
                    if both {
                        self.end_channel(channel, None, false);
                    }
                    false
                }
                Lookup::Closed => false,
                Lookup::Unknown => self.violation(&format!("eof for unknown channel {channel}")),
            },
            AgentFrame::Close { channel, reason } => match self.lookup(channel) {
                Lookup::Live(_) => {
                    let reason =
                        reason.map_or_else(|| "closed by the agent".to_owned(), |r| plain_text(&r));
                    self.end_channel(channel, Some(reason), false);
                    false
                }
                Lookup::Closed => false,
                Lookup::Unknown => self.violation(&format!("close for unknown channel {channel}")),
            },
            AgentFrame::Ports {
                scopes,
                truncated: _,
            } => self.handle_ports(scopes),
            AgentFrame::Captured {
                request,
                id,
                outcome,
                manifest,
                gaps,
            } => self.complete(request, |kind| match kind {
                RequestKind::Capture(wanted) if *wanted == id => {
                    if !matches!(outcome.as_str(), "published" | "skipped" | "failed") {
                        return Err(format!("capture outcome {outcome:?}"));
                    }
                    if gaps.len() > MAX_GAPS {
                        return Err("too many capture gaps".into());
                    }
                    if manifest.as_ref().is_some_and(|m| !m.is_object()) {
                        return Err("capture manifest is not an object".into());
                    }
                    Ok(Reply::Captured(CaptureReply {
                        id: id.clone(),
                        outcome: outcome.clone(),
                        manifest: manifest.clone(),
                        gaps: gaps.iter().map(|g| plain_text(g)).collect(),
                    }))
                }
                _ => Err(format!(
                    "captured for a request that was not a capture of {id}"
                )),
            }),
            AgentFrame::Fenced {
                request,
                held,
                reason,
            } => self.complete(request, |kind| match kind {
                RequestKind::Fence if held || reason.is_some() => Ok(Reply::Fenced(FenceReply {
                    held,
                    reason: reason.as_deref().map(plain_text),
                })),
                RequestKind::Unfence if !held => {
                    Ok(Reply::Fenced(FenceReply { held, reason: None }))
                }
                _ => Err("a fence answer that does not fit its request".into()),
            }),
            AgentFrame::Dropped {
                request,
                id,
                existed,
            } => self.complete(request, |kind| match kind {
                RequestKind::Drop(wanted) if *wanted == id => Ok(Reply::Dropped(existed)),
                _ => Err(format!("dropped for a request that was not a drop of {id}")),
            }),
            AgentFrame::Reaped {
                request,
                removed,
                kept,
                gaps,
                complete,
            } => self.complete(request, |kind| {
                if !matches!(kind, RequestKind::Reap) {
                    return Err("reaped for a request that was not a reap".into());
                }
                if gaps.len() > MAX_GAPS || complete != gaps.is_empty() {
                    return Err("a reap report that does not add up".into());
                }
                Ok(Reply::Reaped(ReapReply {
                    removed,
                    kept,
                    gaps: gaps.iter().map(|gap| plain_text(gap)).collect(),
                    complete,
                }))
            }),
            AgentFrame::Listed { request, snapshots } => self.complete(request, |kind| {
                if !matches!(kind, RequestKind::List) {
                    return Err("listed for a request that was not a list".into());
                }
                if snapshots.len() > MAX_SNAPSHOTS {
                    return Err("too many snapshots listed".into());
                }
                let mut seen = HashSet::new();
                let mut entries = Vec::with_capacity(snapshots.len());
                for snapshot in &snapshots {
                    if !snapshot_id_ok(&snapshot.id) || !seen.insert(snapshot.id.clone()) {
                        return Err(format!("listed snapshot id {:?}", plain_text(&snapshot.id)));
                    }
                    if let Some(workspace) = &snapshot.workspace
                        && crate::frame::validate_label(workspace, "workspace").is_err()
                    {
                        return Err("listed snapshot workspace".into());
                    }
                    entries.push(SnapshotEntry {
                        id: snapshot.id.clone(),
                        workspace: snapshot.workspace.clone(),
                        created_unix_ms: snapshot.created_unix_ms,
                        complete: snapshot.complete,
                    });
                }
                Ok(Reply::Listed(entries))
            }),
            AgentFrame::Resolved { .. } => self.violation("resolved for a request never made"),
            AgentFrame::RestorePrepared { request, result } => {
                self.complete_restore(request, RestorePhase::Prepare, result)
            }
            AgentFrame::Restored { request, result } => {
                self.complete_restore(request, RestorePhase::Up, result)
            }
            AgentFrame::WorkspaceDropped {
                request,
                workspace,
                containers,
                volumes,
                networks,
                left,
            } => self.complete(request, |kind| match kind {
                RequestKind::DropWorkspace(wanted) if *wanted == workspace => {
                    if left.len() > MAX_DROP_LEFT
                        || [containers, volumes, networks]
                            .iter()
                            .any(|count| *count > MAX_DROP_COUNT)
                    {
                        return Err("a workspace drop report beyond its bounds".into());
                    }
                    Ok(Reply::WorkspaceDropped(WorkspaceDrop {
                        containers,
                        volumes,
                        networks,
                        left: left.iter().map(|text| plain_text(text)).collect(),
                    }))
                }
                _ => Err(format!(
                    "a workspace drop answer for a request that was not a drop of {workspace}"
                )),
            }),
            AgentFrame::Error { request, message } => match request {
                None => {
                    self.teardown(&format!(
                        "the agent ended the relay: {}",
                        plain_text(&message)
                    ));
                    true
                }
                Some(request) => {
                    let pending = self.lock().requests.remove(&request);
                    match pending {
                        Some(pending) => {
                            *pending
                                .slot
                                .result
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                Some(Err(RelayError::Agent(plain_text(&message))));
                            pending.slot.cv.notify_all();
                            false
                        }
                        None => self.violation("an error for a request never made"),
                    }
                }
            },
        }
    }

    fn complete(
        &self,
        request: u64,
        check: impl FnOnce(&RequestKind) -> Result<Reply, String>,
    ) -> bool {
        let pending = self.lock().requests.remove(&request);
        let Some(pending) = pending else {
            return self.violation(&format!(
                "a response to request {request}, which was never made"
            ));
        };
        match check(&pending.kind) {
            Ok(reply) => {
                *pending
                    .slot
                    .result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(reply));
                pending.slot.cv.notify_all();
                false
            }
            Err(why) => {
                // Answer the waiter first; the relay is torn down below.
                *pending
                    .slot
                    .result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(Err(RelayError::Agent("invalid reply".into())));
                pending.slot.cv.notify_all();
                self.violation(&why)
            }
        }
    }

    fn complete_restore(
        &self,
        request: u64,
        phase: RestorePhase,
        result: serde_json::Value,
    ) -> bool {
        self.complete(request, |kind| match kind {
            RequestKind::Restore(wanted) if *wanted == phase => {
                if !result.is_object() {
                    return Err("a restore result that is not an object".into());
                }
                if json_depth(&result) > MAX_RESULT_DEPTH {
                    return Err("a restore result nested too deeply".into());
                }
                Ok(Reply::Restore(RestoreReply { result }))
            }
            _ => Err(match phase {
                RestorePhase::Prepare => "a restore_prepare answer for another kind of request",
                RestorePhase::Up => "a restore_up answer for another kind of request",
            }
            .into()),
        })
    }

    fn handle_ports(&self, scopes: Vec<crate::frame::ScopePorts>) -> bool {
        if scopes.len() > MAX_SCOPES {
            return self.violation("a port report for too many scopes");
        }
        let mut map: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
        let mut owner: HashMap<u16, String> = HashMap::new();
        for scope in scopes {
            if !scope_id_ok(&scope.workspace) || map.contains_key(&scope.workspace) {
                return self.violation("a port report with an invalid or repeated workspace");
            }
            if scope.ports.len() > MAX_PORTS_PER_SCOPE {
                return self.violation("a port report with too many ports");
            }
            let mut ports = BTreeSet::new();
            for port in scope.ports {
                if port == 0 {
                    return self.violation("a port report listing port 0");
                }
                if !ports.insert(port) {
                    return self.violation("a port report listing a port twice");
                }
                if let Some(other) = owner.insert(port, scope.workspace.clone())
                    && other != scope.workspace
                {
                    return self.violation("a port report listing one port for two workspaces");
                }
            }
            map.insert(scope.workspace, ports);
        }
        let mut state = self.lock();
        state.ports.received = true;
        state.ports.scopes = map;
        state.ports.version += 1;
        state.stats.ports_frames += 1;
        drop(state);
        self.changed.notify_all();
        false
    }

    fn handle_data(&self, channel: u32, bytes: Vec<u8>) -> bool {
        let count = {
            let mut state = self.lock();
            state.stats.data_frames_in += 1;
            state.stats.bytes_in += bytes.len() as u64;
            state.stats.data_frames_in
        };
        match self.lookup(channel) {
            Lookup::Live(chan) => {
                let unbounded = breaks::break_mode("relay-unbounded");
                let mut st = chan.lock();
                if !matches!(st.phase, Phase::Open) || st.agent_eof {
                    drop(st);
                    return self.violation(&format!("data on channel {channel} out of order"));
                }
                st.outstanding += 1;
                if st.outstanding > WINDOW_CHUNKS && !unbounded {
                    drop(st);
                    return self.violation(&format!("data beyond the window on channel {channel}"));
                }
                if breaks::break_mode("relay-drop-chunk") && count.is_multiple_of(DROP_EVERY) {
                    // Defect: a chunk is lost. It still counts as consumed, so
                    // flow control stays in step and only the bytes are wrong.
                    st.consumed += 1;
                    drop(st);
                    self.lock().stats.dropped_by_break += 1;
                    return false;
                }
                st.inbound.push_back(bytes);
                st.last_activity = Instant::now();
                if unbounded {
                    // Defect: credit is given back on arrival, not on use, so a
                    // reader that stops reading makes the daemon buffer without bound.
                    st.outstanding = st.outstanding.saturating_sub(1);
                    drop(st);
                    self.lock()
                        .control
                        .push_back(HostFrame::Credit { channel, chunks: 1 });
                    self.wake.notify_all();
                } else {
                    drop(st);
                }
                chan.cv.notify_all();
                false
            }
            Lookup::Closed => false,
            Lookup::Unknown => self.violation(&format!("data for unknown channel {channel}")),
        }
    }
}

enum Lookup {
    Live(Arc<Chan>),
    Closed,
    Unknown,
}

/// A reader that records when a frame is half read.
struct Tracked<'a, R> {
    inner: R,
    relay: &'a Inner,
    in_frame: bool,
}

impl<R: Read> Read for Tracked<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 && !self.in_frame {
            self.in_frame = true;
            self.relay.lock().mid_frame_since = Some(Instant::now());
        }
        Ok(read)
    }
}

#[allow(clippy::too_many_lines)]
fn reader_loop(inner: &Arc<Inner>, reader: Box<dyn Read + Send>) {
    let mut tracked = Tracked {
        inner: reader,
        relay: inner,
        in_frame: false,
    };
    let mut ready = false;
    loop {
        let frame = read_frame::<_, AgentFrame>(&mut tracked, MAX_AGENT_JSON_BYTES);
        tracked.in_frame = false;
        inner.lock().mid_frame_since = None;
        if matches!(inner.lock().phase, RelayPhase::Closed(_)) {
            return;
        }
        match frame {
            Ok(None) => {
                inner.teardown("the relay transport ended (end of stream)");
                return;
            }
            Err(FrameError::Io(error)) => {
                inner.teardown(&format!("the relay transport failed: {error}"));
                return;
            }
            Err(FrameError::Truncated) => {
                inner.teardown("the relay transport ended in the middle of a frame");
                return;
            }
            Err(FrameError::Malformed(why)) => {
                // The frame was read whole: the stream is still in step, so with
                // validation off (a test defect) it is skipped.
                if inner.violation(&format!("a malformed frame: {why}")) {
                    return;
                }
            }
            Err(error) => {
                // Oversized, empty or unknown: the stream cannot be followed.
                inner.teardown_hostile(&format!("protocol violation by the VM: {error}"));
                return;
            }
            Ok(Some(Inbound::Control(frame))) => {
                if !ready {
                    match frame {
                        AgentFrame::Ready {
                            protocol,
                            generation,
                            pid,
                            test_breaks_agent,
                            window_chunks,
                            chunk_bytes,
                        } => {
                            if protocol != PROTOCOL
                                || window_chunks != WINDOW_CHUNKS
                                || chunk_bytes != CHUNK_BYTES
                                || test_breaks_agent.len() > 64
                            {
                                inner.teardown_hostile(
                                    "protocol violation by the VM: an unsupported ready",
                                );
                                return;
                            }
                            // A10 tripwire: only a daemon of a test build ever
                            // hands the agent break modes, so a release
                            // daemon refuses an agent that reports any.
                            #[cfg(not(feature = "test-breaks"))]
                            if !test_breaks_agent.is_empty() {
                                inner.teardown_hostile(
                                    "protocol violation by the VM: an agent reporting test break modes to a release daemon",
                                );
                                return;
                            }
                            if generation != inner.expected
                                && !breaks::break_mode("relay-accept-stale-generation")
                                && !breaks::break_mode("ready-accept-stale-generation")
                            {
                                inner.teardown_hostile(&format!(
                                    "the agent reported generation {}/{}, not this engine's {}/{}",
                                    plain_text(&generation.boot_id),
                                    plain_text(&generation.engine_id),
                                    inner.expected.boot_id,
                                    inner.expected.engine_id
                                ));
                                return;
                            }
                            ready = true;
                            let mut state = inner.lock();
                            if matches!(state.phase, RelayPhase::Handshake) {
                                state.phase = RelayPhase::Ready;
                            }
                            state.agent_pid = Some(pid);
                            state.agent_breaks = test_breaks_agent
                                .iter()
                                .map(|mode| plain_text(mode))
                                .collect();
                            drop(state);
                            inner.changed.notify_all();
                        }
                        AgentFrame::Error {
                            request: None,
                            message,
                        } => {
                            inner.teardown(&format!(
                                "the agent refused to start: {}",
                                plain_text(&message)
                            ));
                            return;
                        }
                        _ => {
                            if inner.violation("a frame before ready") {
                                return;
                            }
                        }
                    }
                } else if inner.handle(frame) {
                    return;
                }
            }
            Ok(Some(Inbound::Data { channel, bytes })) => {
                if !ready {
                    if inner.violation("data before ready") {
                        return;
                    }
                } else if inner.handle_data(channel, bytes) {
                    return;
                }
            }
        }
    }
}

// ------------------------------------------------------------------ writing

enum Next {
    Control(HostFrame),
    Data { channel: u32, bytes: Vec<u8> },
    Eof { channel: u32, chan: Arc<Chan> },
    Quit,
}

fn pick(inner: &Inner) -> Next {
    let mut state = inner.lock();
    loop {
        if matches!(state.phase, RelayPhase::Closed(_)) {
            return Next::Quit;
        }
        if let Some(frame) = state.control.pop_front() {
            state.stats.control_out += 1;
            return Next::Control(frame);
        }
        let fair = !breaks::break_mode("relay-fairness-off");
        let mut rounds = state.ring.len();
        while rounds > 0 {
            rounds -= 1;
            let Some(chan) = state.ring.pop_front() else {
                break;
            };
            let mut st = chan.lock();
            if matches!(st.phase, Phase::Closed(_)) {
                st.in_ring = false;
                continue;
            }
            match st.out.front() {
                Some(Item::Eof) => {
                    st.out.pop_front();
                    st.host_eof_queued = false;
                    st.in_ring = !st.out.is_empty();
                    let again = st.in_ring;
                    drop(st);
                    if again {
                        state.ring.push_back(Arc::clone(&chan));
                    }
                    return Next::Eof {
                        channel: chan.id,
                        chan,
                    };
                }
                Some(Item::Data(_))
                    if st.out_credit > 0 || breaks::break_mode("relay-unbounded") =>
                {
                    let Some(Item::Data(bytes)) = st.out.pop_front() else {
                        continue;
                    };
                    st.out_credit = st.out_credit.saturating_sub(1);
                    st.sent += 1;
                    st.last_activity = Instant::now();
                    st.in_ring = !st.out.is_empty() && st.out_credit > 0;
                    let again = st.in_ring;
                    drop(st);
                    chan.cv.notify_all();
                    if again {
                        if fair {
                            state.ring.push_back(Arc::clone(&chan));
                        } else {
                            // Defect: serve the same channel until it is dry.
                            state.ring.push_front(Arc::clone(&chan));
                        }
                    }
                    state.stats.data_frames_out += 1;
                    state.stats.bytes_out += bytes.len() as u64;
                    return Next::Data {
                        channel: chan.id,
                        bytes,
                    };
                }
                Some(Item::Data(_)) if !fair => {
                    // Defect (`relay-fairness-off`): the channel at the head is
                    // served to completion, so the writer waits for its
                    // credit and no other channel's data goes first.
                    drop(st);
                    state.ring.push_front(chan);
                    break;
                }
                _ => {
                    // Nothing sendable (no data, or no credit): it is put back
                    // in the ring when data or credit arrives.
                    st.in_ring = false;
                }
            }
        }
        state = inner
            .wake
            .wait(state)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

fn writer_loop(inner: &Arc<Inner>, mut writer: Box<dyn Write + Send>) {
    loop {
        let next = pick(inner);
        let (result, finished) = match &next {
            Next::Quit => return,
            Next::Control(frame) => {
                inner.lock().writing_since = Some(Instant::now());
                (
                    write_host_frame(&mut writer, &HostOutbound::Control(frame)),
                    None,
                )
            }
            Next::Data { channel, bytes } => {
                let count = inner.lock().stats.data_frames_out;
                if breaks::break_mode("relay-drop-chunk") && count.is_multiple_of(DROP_EVERY) {
                    inner.lock().stats.dropped_by_break += 1;
                    continue;
                }
                inner.lock().writing_since = Some(Instant::now());
                (
                    write_host_frame(
                        &mut writer,
                        &HostOutbound::Data {
                            channel: *channel,
                            bytes,
                        },
                    ),
                    None,
                )
            }
            Next::Eof { channel, chan } => {
                inner.lock().writing_since = Some(Instant::now());
                (
                    write_host_frame(
                        &mut writer,
                        &HostOutbound::Control(&HostFrame::Eof { channel: *channel }),
                    ),
                    Some(Arc::clone(chan)),
                )
            }
        };
        inner.lock().writing_since = None;
        if breaks::break_mode("slow-relay") {
            std::thread::sleep(SLOW_RELAY_DELAY);
        }
        if let Err(error) = result {
            inner.teardown(&format!("writing to the relay transport failed: {error}"));
            return;
        }
        if let Some(chan) = finished {
            let mut st = chan.lock();
            st.host_eof_sent = true;
            let both = st.agent_eof;
            let id = chan.id;
            drop(st);
            chan.cv.notify_all();
            if both {
                inner.end_channel(id, None, false);
            }
        }
    }
}

// ----------------------------------------------------------------- watchdog

fn janitor(inner: &Arc<Inner>) {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let (channels, verdict) = {
            let state = inner.lock();
            if matches!(state.phase, RelayPhase::Closed(_)) {
                return;
            }
            let now = Instant::now();
            let mut verdict = None;
            let mut hostile = false;
            if state
                .writing_since
                .is_some_and(|since| now.duration_since(since) > inner.limits.stall)
            {
                verdict = Some("a write to the agent made no progress (the agent is not reading)");
            }
            if state
                .mid_frame_since
                .is_some_and(|since| now.duration_since(since) > inner.limits.frame_stall)
            {
                hostile = true;
                verdict = Some("the agent stopped in the middle of a frame");
            }
            if state
                .requests
                .values()
                .any(|r| r.abandoned && now.duration_since(r.since) > ABANDONED_REQUEST_LIMIT)
            {
                verdict = Some("a request was never answered");
            }
            (
                state.channels.values().cloned().collect::<Vec<_>>(),
                verdict.map(|why| (why, hostile)),
            )
        };
        if let Some((why, hostile)) = verdict {
            if hostile {
                inner.teardown_hostile(why);
            } else {
                inner.teardown(why);
            }
            return;
        }
        let now = Instant::now();
        for chan in channels {
            let (pending_too_long, idle) = {
                let st = chan.lock();
                (
                    matches!(st.phase, Phase::Pending)
                        && now.duration_since(st.created) > inner.limits.open_timeout,
                    matches!(st.phase, Phase::Open)
                        && now.duration_since(st.last_activity) > inner.limits.idle,
                )
            };
            if pending_too_long {
                inner.end_channel(
                    chan.id,
                    Some("the open was not answered in time".into()),
                    true,
                );
            } else if idle {
                inner.end_channel(chan.id, Some("idle".into()), true);
            }
        }
    }
}

// --------------------------------------------------------------------- API

impl Relay {
    /// Starts the relay over a transport and waits for the agent's `ready`.
    ///
    /// # Errors
    /// `Unavailable` when the agent does not report ready in time, reports
    /// another generation, or the transport fails.
    pub fn start(
        reader: Box<dyn Read + Send>,
        mut writer: Box<dyn Write + Send>,
        closer: Box<dyn Fn() + Send + Sync>,
        config: RelayConfig,
    ) -> Result<Self, RelayError> {
        let project = config
            .project
            .to_str()
            .filter(|p| config.project.is_absolute() && !p.is_empty())
            .ok_or_else(|| RelayError::Invalid("the project root must be absolute".into()))?
            .to_owned();
        let limits = config.limits;
        let settings = Settings {
            stall_ms: u64::try_from(limits.stall.as_millis()).ok(),
            idle_ms: u64::try_from(limits.idle.as_millis()).ok(),
            max_channels: u32::try_from(limits.per_project).ok(),
            max_channels_per_attempt: u32::try_from(limits.per_attempt).ok(),
            ..Settings::default()
        };
        let init = HostFrame::Init {
            protocol: PROTOCOL,
            generation: Some(config.expected.clone()),
            settings,
        };
        let inner = Arc::new(Inner {
            project,
            expected: config.expected,
            limits,
            budget: config.budget,
            forge_tcp_port: config.forge_tcp_port,
            closer,
            st: Mutex::new(State {
                phase: RelayPhase::Handshake,
                agent_breaks: Vec::new(),
                agent_pid: None,
                channels: HashMap::new(),
                next_channel: 1,
                tomb: VecDeque::new(),
                tomb_set: HashSet::new(),
                per_attempt: HashMap::new(),
                control: VecDeque::new(),
                ring: VecDeque::new(),
                requests: HashMap::new(),
                next_request: 1,
                ports: Ports::default(),
                stats: Stats::default(),
                writing_since: None,
                mid_frame_since: None,
                violated: false,
                close_hooks: Vec::new(),
            }),
            wake: Condvar::new(),
            changed: Condvar::new(),
        });
        // `init` is the one frame written before the writer thread exists. The
        // reader starts first so a fast `ready` is never missed.
        {
            let inner = Arc::clone(&inner);
            std::thread::spawn(move || reader_loop(&inner, reader));
        }
        if let Err(error) = write_host_frame(&mut writer, &HostOutbound::Control(&init)) {
            inner.teardown(&format!("writing init failed: {error}"));
            return Err(RelayError::Unavailable(format!(
                "writing init failed: {error}"
            )));
        }
        {
            let inner = Arc::clone(&inner);
            std::thread::spawn(move || writer_loop(&inner, writer));
        }
        let deadline = Instant::now() + inner.limits.ready_timeout;
        let mut state = inner.lock();
        loop {
            match &state.phase {
                RelayPhase::Ready => break,
                RelayPhase::Closed(reason) => {
                    return Err(if state.violated {
                        RelayError::Hostile(reason.clone())
                    } else {
                        RelayError::Unavailable(reason.clone())
                    });
                }
                RelayPhase::Handshake => {}
            }
            let now = Instant::now();
            if now >= deadline {
                drop(state);
                inner.teardown("the agent did not report ready in time");
                return Err(RelayError::Unavailable(
                    "the agent did not report ready in time".into(),
                ));
            }
            state = inner
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        drop(state);
        {
            let inner = Arc::clone(&inner);
            std::thread::spawn(move || janitor(&inner));
        }
        Ok(Self { inner })
    }

    /// Why the relay ended, if it has.
    #[must_use]
    pub fn closed(&self) -> Option<String> {
        Inner::closed_reason(&self.inner.lock())
    }

    /// Whether the relay ended because of what the VM sent (the project is
    /// quarantined until an explicit start).
    #[must_use]
    pub fn violated(&self) -> bool {
        self.inner.lock().violated
    }

    /// Ends the relay (the daemon's own decision: the engine or the VM is
    /// going away). Every channel closes.
    pub fn close(&self, reason: &str) {
        self.inner.teardown(reason);
    }

    /// Runs `hook` once, with the reason, when the relay ends (at once if it
    /// already has).
    pub fn on_close(&self, hook: impl Fn(&str) + Send + 'static) {
        let mut state = self.inner.lock();
        if let RelayPhase::Closed(reason) = &state.phase {
            let reason = reason.clone();
            drop(state);
            hook(&reason);
        } else {
            state.close_hooks.push(Box::new(hook));
        }
    }

    #[must_use]
    pub fn project(&self) -> &str {
        &self.inner.project
    }

    #[must_use]
    pub fn generation(&self) -> &Generation {
        &self.inner.expected
    }

    /// The test-break modes the agent acknowledged in its `ready`.
    #[must_use]
    pub fn agent_breaks(&self) -> Vec<String> {
        self.inner.lock().agent_breaks.clone()
    }

    #[must_use]
    pub fn agent_pid(&self) -> Option<u32> {
        self.inner.lock().agent_pid
    }

    #[must_use]
    pub fn stats(&self) -> Stats {
        self.inner.lock().stats.clone()
    }

    #[must_use]
    pub fn open_channels(&self) -> usize {
        self.inner.lock().channels.len()
    }

    fn check_ready(state: &State) -> Result<(), RelayError> {
        match &state.phase {
            RelayPhase::Ready => Ok(()),
            RelayPhase::Closed(reason) => Err(RelayError::Unavailable(reason.clone())),
            RelayPhase::Handshake => Err(RelayError::Unavailable("not ready".into())),
        }
    }

    /// A channel to the project's filtered Docker endpoint (`docker` kind).
    ///
    /// # Errors
    /// Refusals before anything is sent (not ready, a cap), and the agent's.
    pub fn open_docker(&self, scope: &AttemptScope) -> Result<Stream, RelayError> {
        self.open(scope, ChannelKind::Docker, None, "docker".to_owned())
    }

    /// A channel to a published port of the workspace's containers. The port
    /// must be in the workspace's published-port map the agent reported; that
    /// check is the daemon's own and happens before any frame is sent.
    ///
    /// # Errors
    /// `NotAuthorized` when the port is not in the map, plus `open_docker`'s.
    pub fn open_tcp(&self, scope: &AttemptScope, port: u16) -> Result<Stream, RelayError> {
        let port = if breaks::break_mode("fault-forge-tcp") {
            // Test fault: the daemon "forgets" which port it was asked for.
            self.inner.forge_tcp_port.unwrap_or(port)
        } else {
            port
        };
        if port == 0 {
            return Err(RelayError::Invalid("port 0".into()));
        }
        if !breaks::break_mode("tcp-map-check-off") {
            // The agent sends the first port report right after `ready`; wait
            // for it rather than refuse a connection that raced it.
            let deadline = Instant::now() + self.inner.limits.open_timeout;
            let mut state = self.inner.lock();
            loop {
                Self::check_ready(&state)?;
                if state.ports.received {
                    break;
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(RelayError::NotAuthorized(
                        "the published-port map is not known yet".into(),
                    ));
                }
                state = self
                    .inner
                    .changed
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            let allowed = state
                .ports
                .scopes
                .get(&scope.workspace)
                .is_some_and(|ports| ports.contains(&port));
            if !allowed {
                return Err(RelayError::NotAuthorized(format!(
                    "port {port} is not published for workspace {}",
                    scope.workspace
                )));
            }
        }
        self.open(scope, ChannelKind::Tcp, Some(port), format!("tcp-{port}"))
    }

    #[allow(clippy::too_many_lines)]
    fn open(
        &self,
        scope: &AttemptScope,
        kind: ChannelKind,
        port: Option<u16>,
        authority_kind: String,
    ) -> Result<Stream, RelayError> {
        let inner = &self.inner;
        let authority = scope.authority(authority_kind);
        // The project itself, or a folder inside it (a lane's own folder, C3):
        // the root a scope's bind sources resolve beneath. Component-wise, so
        // `/p/x` is not inside `/p-other`.
        if !std::path::Path::new(&authority.project).starts_with(&inner.project) {
            return Err(RelayError::Invalid(
                "that scope belongs to another project".into(),
            ));
        }
        let attempt_key = (scope.workspace.clone(), scope.attempt.clone());
        let limits_off = breaks::break_mode("channel-limit-off");
        let chan = {
            let mut state = inner.lock();
            Self::check_ready(&state)?;
            if !limits_off {
                let same_attempt = state.per_attempt.get(&attempt_key).copied().unwrap_or(0);
                if same_attempt >= inner.limits.per_attempt {
                    return Err(RelayError::Limit(format!(
                        "too many open channels for this attempt (limit {})",
                        inner.limits.per_attempt
                    )));
                }
                if state.channels.len() >= inner.limits.per_project {
                    return Err(RelayError::Limit(format!(
                        "too many open channels for this project (limit {})",
                        inner.limits.per_project
                    )));
                }
            }
            if !inner.budget.acquire() {
                if !limits_off {
                    return Err(RelayError::Limit(
                        "too many open channels for this daemon".into(),
                    ));
                }
                // Defect (`channel-limit-off`): over the cap, but the books
                // stay balanced for the release in `remove_channel`.
                inner.budget.used.fetch_add(1, Ordering::SeqCst);
            }
            let id = state.next_channel;
            let Some(next) = id.checked_add(1) else {
                inner.budget.release(1);
                return Err(RelayError::Unavailable(
                    "the relay has used all its channel ids; it is restarted".into(),
                ));
            };
            state.next_channel = next;
            *state.per_attempt.entry(attempt_key.clone()).or_insert(0) += 1;
            let now = Instant::now();
            let chan = Arc::new(Chan {
                id,
                attempt: attempt_key,
                st: Mutex::new(ChanState {
                    phase: Phase::Pending,
                    refusal: None,
                    inbound: VecDeque::new(),
                    front_offset: 0,
                    outstanding: 0,
                    consumed: 0,
                    agent_eof: false,
                    out_credit: WINDOW_CHUNKS,
                    out: VecDeque::new(),
                    in_ring: false,
                    host_eof_queued: false,
                    host_eof_sent: false,
                    sent: 0,
                    last_activity: now,
                    created: now,
                }),
                cv: Condvar::new(),
            });
            state.channels.insert(id, Arc::clone(&chan));
            state.control.push_back(HostFrame::Open {
                channel: id,
                kind,
                port,
                authority,
            });
            state.stats.opens_sent += 1;
            if kind == ChannelKind::Tcp {
                state.stats.tcp_opens_sent += 1;
            }
            chan
        };
        inner.wake.notify_all();
        let mut st = chan.lock();
        let failure = loop {
            match &st.phase {
                Phase::Open => break None,
                Phase::Closed(reason) => {
                    break Some(
                        st.refusal
                            .clone()
                            .or_else(|| reason.clone())
                            .unwrap_or_else(|| "closed".into()),
                    );
                }
                Phase::Pending => {}
            }
            // The janitor ends a pending open that is not answered in time.
            st = chan
                .cv
                .wait_timeout(st, Duration::from_millis(500))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        };
        drop(st);
        if let Some(message) = failure {
            let relay_gone = Inner::closed_reason(&inner.lock()).is_some();
            return Err(if relay_gone {
                RelayError::Unavailable(message)
            } else {
                RelayError::Refused(message)
            });
        }
        Ok(Stream {
            inner: Arc::new(StreamInner {
                relay: Arc::clone(&self.inner),
                chan,
            }),
        })
    }

    /// The published ports of one workspace, as a subscription.
    ///
    /// # Errors
    /// `Unavailable` when the relay is not ready.
    pub fn ports(&self, workspace: &str) -> Result<PortsSubscription, RelayError> {
        if !scope_id_ok(workspace) {
            return Err(RelayError::Invalid(format!(
                "invalid workspace id {workspace:?}"
            )));
        }
        let state = self.inner.lock();
        Self::check_ready(&state)?;
        Ok(PortsSubscription {
            relay: Arc::clone(&self.inner),
            workspace: workspace.to_owned(),
            seen: 0,
        })
    }

    fn request(
        &self,
        kind: RequestKind,
        frame: impl FnOnce(u64) -> HostFrame,
    ) -> Result<Reply, RelayError> {
        self.request_within(kind, frame, None)
    }

    /// `request`, giving up after `wait` (default: the relay's request
    /// timeout). The request stays counted until the agent answers.
    fn request_within(
        &self,
        kind: RequestKind,
        frame: impl FnOnce(u64) -> HostFrame,
        wait: Option<Duration>,
    ) -> Result<Reply, RelayError> {
        let inner = &self.inner;
        let slot = Arc::new(RequestSlot {
            result: Mutex::new(None),
            cv: Condvar::new(),
        });
        let id = {
            let mut state = inner.lock();
            Self::check_ready(&state)?;
            if state.requests.len() >= MAX_REQUESTS_IN_FLIGHT {
                return Err(RelayError::Limit("too many requests in progress".into()));
            }
            let id = state.next_request;
            state.next_request += 1;
            state.requests.insert(
                id,
                PendingRequest {
                    kind,
                    slot: Arc::clone(&slot),
                    since: Instant::now(),
                    abandoned: false,
                },
            );
            state.control.push_back(frame(id));
            id
        };
        inner.wake.notify_all();
        let deadline = Instant::now()
            + wait.map_or(inner.limits.request_timeout, |w| {
                w.min(inner.limits.request_timeout)
            });
        let mut result = slot
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(answer) = result.take() {
                return answer;
            }
            let now = Instant::now();
            if now >= deadline {
                drop(result);
                // Stays counted until answered (the agent counts it too).
                if let Some(request) = inner.lock().requests.get_mut(&id) {
                    request.abandoned = true;
                }
                return Err(RelayError::Timeout("the services agent's answer".into()));
            }
            result = slot
                .cv
                .wait_timeout(result, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Takes a snapshot of one workspace's containers and volumes (C1 calls
    /// this). The id and the workspace are checked here as well as in the agent.
    /// `reserve_bytes` rides in the request: the agent reads the pool's free
    /// space on its own pool descriptor when the capture is admitted and skips
    /// the capture (a gap) when less is free, or when it cannot tell.
    ///
    /// # Errors
    /// `Invalid` for a bad id or workspace; the agent's failure otherwise.
    pub fn capture(
        &self,
        id: &str,
        workspace: &str,
        reserve_bytes: u64,
    ) -> Result<CaptureReply, RelayError> {
        self.capture_within(id, workspace, reserve_bytes, None)
    }

    /// `capture`, but the daemon stops waiting for the agent's answer after
    /// `wait` (P2: a capture must not hold up the next turn). The capture may
    /// still finish in the VM; nothing references what it publishes, and the
    /// orphan collector drops it.
    ///
    /// # Errors
    /// As `capture`; `Timeout` when `wait` ran out.
    pub fn capture_within(
        &self,
        id: &str,
        workspace: &str,
        reserve_bytes: u64,
        wait: Option<Duration>,
    ) -> Result<CaptureReply, RelayError> {
        if !snapshot_id_ok(id) {
            return Err(RelayError::Invalid(format!("invalid snapshot id {id:?}")));
        }
        if !scope_id_ok(workspace) {
            return Err(RelayError::Invalid(format!(
                "invalid workspace id {workspace:?}"
            )));
        }
        let (idc, wsc) = (id.to_owned(), workspace.to_owned());
        match self.request_within(
            RequestKind::Capture(id.to_owned()),
            move |request| HostFrame::Capture {
                request,
                id: idc,
                workspace: wsc,
                reserve_bytes,
            },
            wait,
        )? {
            Reply::Captured(reply) => Ok(reply),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// The write fence of a stop cutover: the agent freezes every listed
    /// workspace through the filtered endpoint's gate and holds the freeze until
    /// [`Self::unfence`] or until it exits with the VM. `Ok` means every
    /// workspace is frozen now.
    ///
    /// # Errors
    /// `Invalid` for a bad workspace; the agent's refusal (a request that would
    /// not finish) or failure otherwise. Nothing stays frozen after an error.
    pub fn fence(&self, workspaces: &[String]) -> Result<(), RelayError> {
        if workspaces.is_empty() || workspaces.len() > MAX_SCOPES {
            return Err(RelayError::Invalid(
                "a fence needs 1 to 256 workspaces".into(),
            ));
        }
        if let Some(bad) = workspaces.iter().find(|ws| !scope_id_ok(ws)) {
            return Err(RelayError::Invalid(format!("invalid workspace id {bad:?}")));
        }
        let list = workspaces.to_vec();
        match self.request(RequestKind::Fence, move |request| HostFrame::Fence {
            request,
            workspaces: list,
        })? {
            Reply::Fenced(reply) if reply.held => Ok(()),
            Reply::Fenced(reply) => Err(RelayError::Agent(
                reply
                    .reason
                    .unwrap_or_else(|| "the fence was not taken".into()),
            )),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// Releases the fence (the stop did not happen).
    ///
    /// # Errors
    /// The relay is gone or the agent failed.
    pub fn unfence(&self) -> Result<(), RelayError> {
        match self.request(RequestKind::Unfence, |request| HostFrame::Unfence {
            request,
        })? {
            Reply::Fenced(_) => Ok(()),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// Drops a snapshot; `Ok(existed)`.
    ///
    /// # Errors
    /// `Invalid` for a bad id; the agent's failure otherwise.
    pub fn drop_snapshot(&self, id: &str) -> Result<bool, RelayError> {
        if !snapshot_id_ok(id) {
            return Err(RelayError::Invalid(format!("invalid snapshot id {id:?}")));
        }
        let idc = id.to_owned();
        match self.request(RequestKind::Drop(id.to_owned()), move |request| {
            HostFrame::DropSnapshot { request, id: idc }
        })? {
            Reply::Dropped(existed) => Ok(existed),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// Removes the containers the ledger and the labels place in `workspace` for
    /// `attempt` (E4; the job's or the shell's attempt ended). Idempotent: a
    /// report with `complete` false is asked again later.
    ///
    /// # Errors
    /// `Invalid` for a bad workspace or attempt; the agent's failure otherwise.
    pub fn reap(&self, workspace: &str, attempt: &str) -> Result<ReapReply, RelayError> {
        if !scope_id_ok(workspace) || !scope_id_ok(attempt) {
            return Err(RelayError::Invalid("invalid workspace or attempt".into()));
        }
        let (workspace, attempt) = (workspace.to_owned(), attempt.to_owned());
        match self.request(RequestKind::Reap, move |request| HostFrame::Reap {
            request,
            workspace,
            attempt,
        })? {
            Reply::Reaped(reply) => Ok(reply),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// Drops everything of one workspace (C3: its lane is closed, cancelled or
    /// removed): the agent removes the workspace's containers, volumes and
    /// networks through the filter of that workspace, then forgets its scope.
    /// By workspace only: no path and no other identifier crosses.
    ///
    /// # Errors
    /// `Invalid` for a bad workspace id; the agent's failure otherwise.
    pub fn drop_workspace(&self, workspace: &str) -> Result<WorkspaceDrop, RelayError> {
        if !scope_id_ok(workspace) {
            return Err(RelayError::Invalid(format!(
                "invalid workspace id {workspace:?}"
            )));
        }
        let name = workspace.to_owned();
        match self.request(
            RequestKind::DropWorkspace(workspace.to_owned()),
            move |request| HostFrame::DropWorkspace {
                request,
                workspace: name,
            },
        )? {
            Reply::WorkspaceDropped(report) => Ok(report),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// The snapshots the agent holds.
    ///
    /// # Errors
    /// The agent's failure.
    pub fn list(&self) -> Result<Vec<SnapshotEntry>, RelayError> {
        match self.request(RequestKind::List, |request| HostFrame::List { request })? {
            Reply::Listed(entries) => Ok(entries),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// Restore, phase one (C3 calls this): creates the fork's volumes in the
    /// lane's workspace, installs the snapshot's data, and returns the fields
    /// and the lane files' contents for the daemon to write. Every argument is
    /// checked here as well as in the agent; `lane_dir` must lie strictly
    /// inside `project`, as the services VM mounts it.
    ///
    /// # Errors
    /// `Invalid` for a bad argument; the agent's failure otherwise.
    pub fn restore_prepare(
        &self,
        id: &str,
        workspace: &str,
        project: &str,
        source: Option<&str>,
        lane_dir: &str,
        fork: &str,
    ) -> Result<RestoreReply, RelayError> {
        if !snapshot_id_ok(id) {
            return Err(RelayError::Invalid(format!("invalid snapshot id {id:?}")));
        }
        check_restore_fields(workspace, project, source, lane_dir, fork)?;
        let (id, workspace, project, source, lane_dir, fork) = (
            id.to_owned(),
            workspace.to_owned(),
            project.to_owned(),
            source.map(str::to_owned),
            lane_dir.to_owned(),
            fork.to_owned(),
        );
        match self.request(
            RequestKind::Restore(RestorePhase::Prepare),
            move |request| HostFrame::RestorePrepare {
                request,
                id,
                workspace,
                project,
                source,
                lane_dir,
                fork,
            },
        )? {
            Reply::Restore(reply) => Ok(reply),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// Restore, phase two: after the daemon wrote the lane files, the agent
    /// brings the stacks up through the filter and runs the post-restore check.
    ///
    /// # Errors
    /// `Invalid` for a bad argument; the agent's failure otherwise.
    pub fn restore_up(
        &self,
        workspace: &str,
        project: &str,
        source: Option<&str>,
        lane_dir: &str,
        fork: &str,
    ) -> Result<RestoreReply, RelayError> {
        check_restore_fields(workspace, project, source, lane_dir, fork)?;
        let (workspace, project, source, lane_dir, fork) = (
            workspace.to_owned(),
            project.to_owned(),
            source.map(str::to_owned),
            lane_dir.to_owned(),
            fork.to_owned(),
        );
        match self.request(RequestKind::Restore(RestorePhase::Up), move |request| {
            HostFrame::RestoreUp {
                request,
                workspace,
                project,
                source,
                lane_dir,
                fork,
            }
        })? {
            Reply::Restore(reply) => Ok(reply),
            _ => Err(RelayError::Agent("unexpected reply".into())),
        }
    }

    /// Asks the agent to exit cleanly (it answers by closing the transport).
    pub fn shutdown(&self) {
        {
            let mut state = self.inner.lock();
            state.control.push_back(HostFrame::Shutdown);
        }
        self.inner.wake.notify_all();
    }

    /// The project path as a `Path`.
    #[must_use]
    pub fn project_path(&self) -> &Path {
        Path::new(&self.inner.project)
    }
}

/// A workspace's published ports, with change notification.
pub struct PortsSubscription {
    relay: Arc<Inner>,
    workspace: String,
    seen: u64,
}

impl PortsSubscription {
    /// The ports now.
    #[must_use]
    pub fn current(&self) -> Vec<u16> {
        self.relay
            .lock()
            .ports
            .scopes
            .get(&self.workspace)
            .map(|ports| ports.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Waits until the agent reports a port state different from the last one
    /// this subscription returned (or the first one), up to `timeout`. `None`
    /// on timeout or when the relay ended.
    pub fn wait_change(&mut self, timeout: Duration) -> Option<Vec<u16>> {
        let deadline = Instant::now() + timeout;
        let mut state = self.relay.lock();
        loop {
            if matches!(state.phase, RelayPhase::Closed(_)) {
                return None;
            }
            if state.ports.received && state.ports.version != self.seen {
                self.seen = state.ports.version;
                return Some(
                    state
                        .ports
                        .scopes
                        .get(&self.workspace)
                        .map(|ports| ports.iter().copied().collect())
                        .unwrap_or_default(),
                );
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            state = self
                .relay
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

// ------------------------------------------------------------------- stream

struct StreamInner {
    relay: Arc<Inner>,
    chan: Arc<Chan>,
}

impl Drop for StreamInner {
    fn drop(&mut self) {
        let closed = matches!(self.chan.lock().phase, Phase::Closed(_));
        if !closed {
            self.relay.end_channel(self.chan.id, None, true);
        }
    }
}

/// One channel: opaque bytes both ways, with half close. Cloning gives another
/// handle to the same channel (read in one thread, write in another); the
/// channel closes when the last handle is dropped.
#[derive(Clone)]
pub struct Stream {
    inner: Arc<StreamInner>,
}

impl Stream {
    #[must_use]
    pub fn id(&self) -> u32 {
        self.inner.chan.id
    }

    /// Half close: the agent sees end of stream after everything written.
    ///
    /// # Errors
    /// The channel's failure when it has ended.
    pub fn shutdown_write(&self) -> io::Result<()> {
        let chan = &self.inner.chan;
        let mut st = chan.lock();
        if let Phase::Closed(reason) = &st.phase {
            return Err(closed_error(reason.as_deref()));
        }
        if st.host_eof_queued || st.host_eof_sent {
            return Ok(());
        }
        st.host_eof_queued = true;
        st.out.push_back(Item::Eof);
        let ring = !st.in_ring;
        st.in_ring = true;
        drop(st);
        if ring {
            self.inner.relay.lock().ring.push_back(Arc::clone(chan));
        }
        self.inner.relay.wake.notify_all();
        Ok(())
    }

    /// Closes the channel now.
    pub fn close(&self) {
        self.inner.relay.end_channel(self.inner.chan.id, None, true);
    }
}

fn closed_error(reason: Option<&str>) -> io::Error {
    match reason {
        Some(reason) => io::Error::new(io::ErrorKind::ConnectionAborted, reason.to_owned()),
        None => io::Error::new(io::ErrorKind::BrokenPipe, "the channel is closed"),
    }
}

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let chan = &self.inner.chan;
        let mut st = chan.lock();
        loop {
            if !st.inbound.is_empty() {
                let offset = st.front_offset;
                let (take, finished) = {
                    let front = &st.inbound[0];
                    let take = buffer.len().min(front.len() - offset);
                    buffer[..take].copy_from_slice(&front[offset..offset + take]);
                    (take, offset + take == front.len())
                };
                st.front_offset += take;
                st.last_activity = Instant::now();
                if finished {
                    st.inbound.pop_front();
                    st.front_offset = 0;
                    st.consumed += 1;
                    st.outstanding = st.outstanding.saturating_sub(1);
                    let flush = st.consumed >= CREDIT_BATCH
                        || (st.inbound.is_empty() && st.consumed >= CREDIT_BATCH / 2);
                    if flush {
                        let give = st.consumed;
                        st.consumed = 0;
                        let open = matches!(st.phase, Phase::Open);
                        drop(st);
                        if open && !breaks::break_mode("relay-unbounded") {
                            self.inner
                                .relay
                                .lock()
                                .control
                                .push_back(HostFrame::Credit {
                                    channel: chan.id,
                                    chunks: give,
                                });
                            self.inner.relay.wake.notify_all();
                        }
                    }
                }
                return Ok(take);
            }
            if st.agent_eof {
                return Ok(0);
            }
            if let Phase::Closed(reason) = &st.phase {
                return match reason {
                    Some(reason) => Err(closed_error(Some(reason))),
                    None => Ok(0),
                };
            }
            st = chan
                .cv
                .wait(st)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let chan = &self.inner.chan;
        let relay = &self.inner.relay;
        let unbounded = breaks::break_mode("relay-unbounded");
        let mut written = 0;
        while written < buffer.len() {
            let take = (buffer.len() - written).min(CHUNK_BYTES);
            let mut st = chan.lock();
            let mut deadline = Instant::now() + relay.limits.stall;
            let mut progress = st.sent;
            loop {
                if let Phase::Closed(reason) = &st.phase {
                    return Err(closed_error(reason.as_deref()));
                }
                if st.host_eof_queued || st.host_eof_sent {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the write side is closed",
                    ));
                }
                if st.out.len() < OUT_QUEUE_CHUNKS || unbounded {
                    break;
                }
                if st.sent != progress {
                    progress = st.sent;
                    deadline = Instant::now() + relay.limits.stall;
                }
                let now = Instant::now();
                if now >= deadline {
                    drop(st);
                    relay.end_channel(chan.id, Some("stalled".into()), true);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the channel made no progress",
                    ));
                }
                st = chan
                    .cv
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            st.out
                .push_back(Item::Data(buffer[written..written + take].to_vec()));
            st.last_activity = Instant::now();
            let ring = !st.in_ring && st.out_credit > 0;
            if ring {
                st.in_ring = true;
            }
            drop(st);
            if ring {
                relay.lock().ring.push_back(Arc::clone(chan));
            }
            relay.wake.notify_all();
            written += take;
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_ids_follow_the_policy_rule() {
        for good in ["base", "w1", "lane-3.x", "a_b"] {
            assert!(scope_id_ok(good), "{good}");
        }
        for bad in ["", ".hidden", "a/b", "w/../1", "a b", "é", &"x".repeat(65)] {
            assert!(!scope_id_ok(bad), "{bad}");
        }
    }

    #[test]
    fn snapshot_ids_are_opaque_tokens() {
        for good in ["c1", "ckpt_2-a", &"a".repeat(64)] {
            assert!(snapshot_id_ok(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "../../x",
            "a/b",
            ".",
            "..",
            &"a".repeat(65),
            "a\0b",
        ] {
            assert!(!snapshot_id_ok(bad), "{bad}");
        }
    }

    #[test]
    fn display_text_loses_control_characters_and_is_bounded() {
        assert_eq!(plain_text("a\x1b[31mb\nc"), "a [31mb c");
        assert!(plain_text(&"é".repeat(2000)).len() <= MAX_TEXT_BYTES);
    }

    #[test]
    fn the_daemon_budget_never_exceeds_its_cap() {
        let budget = DaemonBudget::new(2);
        assert!(budget.acquire());
        assert!(budget.acquire());
        assert!(!budget.acquire());
        budget.release(1);
        assert!(budget.acquire());
        assert_eq!(budget.in_use(), 2);
    }
}

/// One in-process pass over the relay's state machine with a scripted agent on
/// a socket pair: handshake, flow-controlled echo over more than a window, the
/// caps, the daemon's own TCP check, and the tear-down on a forged frame. The
/// real agent and a real VM are exercised by the E1 live run.
#[cfg(test)]
mod relay_tests {
    use super::*;
    use crate::frame::{AgentFrame, Inbound, Outbound, read_frame, write_frame};
    use std::os::unix::net::UnixStream;

    fn generation() -> Generation {
        Generation {
            boot_id: "boot".into(),
            engine_id: "e".repeat(64),
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quirk {
        Honest,
        /// A second `opened` for the same channel.
        DuplicateOpened,
        /// A `restored` answer to a request nobody made, after `ready`.
        UnrequestedRestored,
        /// `restore_prepare` is answered with `restored`.
        WrongKindAnswer,
        /// `ready` reports test break modes.
        BreaksInReady,
    }

    /// One channel of the stand-in agent: data waiting for the daemon's credit,
    /// the credit the daemon has given, and chunks echoed but not yet credited
    /// back. The real agent also sends only what the daemon has credited.
    struct EchoChannel {
        waiting: std::collections::VecDeque<Vec<u8>>,
        credit: u32,
        echoed: u32,
        eof: bool,
    }

    impl EchoChannel {
        fn new() -> Self {
            EchoChannel {
                waiting: std::collections::VecDeque::new(),
                credit: WINDOW_CHUNKS,
                echoed: 0,
                eof: false,
            }
        }

        /// Echoes what the credit allows, then the end of stream once all data is out.
        fn flush(&mut self, channel: u32, writer: &mut UnixStream) {
            while self.credit > 0
                && let Some(bytes) = self.waiting.pop_front()
            {
                self.credit -= 1;
                write_frame(writer, &Outbound::Data { channel, bytes }).unwrap();
                self.echoed += 1;
                if self.echoed == 16 {
                    let chunks = std::mem::take(&mut self.echoed);
                    let frame = AgentFrame::Credit { channel, chunks };
                    write_frame(writer, &Outbound::Control(frame)).unwrap();
                }
            }
            if self.eof && self.waiting.is_empty() {
                self.eof = false;
                write_frame(writer, &Outbound::Control(AgentFrame::Eof { channel })).unwrap();
            }
        }
    }

    /// Echoes data, answers opens and requests, and misbehaves as `quirk` says.
    #[allow(clippy::too_many_lines)]
    fn agent(stream: UnixStream, quirk: Quirk) {
        let mut reader = stream.try_clone().unwrap();
        let mut writer = stream;
        std::thread::spawn(move || {
            let mut channels: HashMap<u32, EchoChannel> = HashMap::new();
            loop {
                match read_frame::<_, HostFrame>(&mut reader, 64 * 1024) {
                    Ok(Some(Inbound::Control(HostFrame::Init { .. }))) => {
                        for frame in [
                            AgentFrame::Ready {
                                protocol: PROTOCOL,
                                generation: generation(),
                                pid: 7,
                                test_breaks_agent: if quirk == Quirk::BreaksInReady {
                                    vec!["demo".into()]
                                } else {
                                    vec![]
                                },
                                window_chunks: WINDOW_CHUNKS,
                                chunk_bytes: CHUNK_BYTES,
                            },
                            AgentFrame::Ports {
                                scopes: vec![crate::frame::ScopePorts {
                                    workspace: "base".into(),
                                    ports: vec![5432],
                                }],
                                truncated: false,
                            },
                        ] {
                            write_frame(&mut writer, &Outbound::Control(frame)).unwrap();
                        }
                        if quirk == Quirk::UnrequestedRestored {
                            let frame = AgentFrame::Restored {
                                request: 5,
                                result: serde_json::json!({}),
                            };
                            let _ = write_frame(&mut writer, &Outbound::Control(frame));
                        }
                    }
                    Ok(Some(Inbound::Control(HostFrame::RestorePrepare { request, .. }))) => {
                        let result =
                            serde_json::json!({"restored": [], "files": {"a": "line\nline\n"}});
                        let frame = if quirk == Quirk::WrongKindAnswer {
                            AgentFrame::Restored { request, result }
                        } else {
                            AgentFrame::RestorePrepared { request, result }
                        };
                        let _ = write_frame(&mut writer, &Outbound::Control(frame));
                    }
                    Ok(Some(Inbound::Control(HostFrame::RestoreUp { request, .. }))) => {
                        let frame = AgentFrame::Restored {
                            request,
                            result: serde_json::json!({"ok": true}),
                        };
                        let _ = write_frame(&mut writer, &Outbound::Control(frame));
                    }
                    Ok(Some(Inbound::Control(HostFrame::Open { channel, .. }))) => {
                        channels.insert(channel, EchoChannel::new());
                        write_frame(
                            &mut writer,
                            &Outbound::Control(AgentFrame::Opened { channel }),
                        )
                        .unwrap();
                        if quirk == Quirk::DuplicateOpened {
                            // A second `opened` for the same channel.
                            write_frame(
                                &mut writer,
                                &Outbound::Control(AgentFrame::Opened { channel }),
                            )
                            .unwrap();
                        }
                    }
                    Ok(Some(Inbound::Data { channel, bytes })) => {
                        let chan = channels.entry(channel).or_insert_with(EchoChannel::new);
                        chan.waiting.push_back(bytes);
                        chan.flush(channel, &mut writer);
                    }
                    Ok(Some(Inbound::Control(HostFrame::Credit { channel, chunks }))) => {
                        let chan = channels.entry(channel).or_insert_with(EchoChannel::new);
                        chan.credit += chunks;
                        chan.flush(channel, &mut writer);
                    }
                    Ok(Some(Inbound::Control(HostFrame::Eof { channel }))) => {
                        let chan = channels.entry(channel).or_insert_with(EchoChannel::new);
                        chan.eof = true;
                        chan.flush(channel, &mut writer);
                    }
                    Ok(Some(Inbound::Control(_))) => {}
                    _ => return,
                }
            }
        });
    }

    fn start(forge: bool, limits: Limits) -> Relay {
        let quirk = if forge {
            Quirk::DuplicateOpened
        } else {
            Quirk::Honest
        };
        start_with(quirk, limits).unwrap()
    }

    fn start_with(quirk: Quirk, limits: Limits) -> Result<Relay, RelayError> {
        let (host, far) = UnixStream::pair().unwrap();
        agent(far, quirk);
        let closer_side = host.try_clone().unwrap();
        Relay::start(
            Box::new(host.try_clone().unwrap()),
            Box::new(host),
            Box::new(move || {
                let _ = closer_side.shutdown(std::net::Shutdown::Both);
            }),
            RelayConfig {
                project: "/p".into(),
                expected: generation(),
                limits,
                budget: DaemonBudget::new(1024),
                forge_tcp_port: None,
            },
        )
    }

    #[test]
    fn echo_over_credit_windows_caps_tcp_map_and_forged_frames() {
        let relay = start(
            false,
            Limits {
                per_attempt: 2,
                ..Limits::default()
            },
        );
        let scope = AttemptScope::new("/p", "base", "a1").unwrap();
        // 3 MiB through a channel with a 1 MiB window: needs credits both ways.
        let stream = relay.open_tcp(&scope, 5432).unwrap();
        let data: Vec<u8> = (0..3_usize << 20)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let mut sender = stream.clone();
        let payload = data.clone();
        let writer = std::thread::spawn(move || {
            sender.write_all(&payload).unwrap();
            sender.shutdown_write().unwrap();
        });
        // A reader that has not started yet, as on a loaded machine: the
        // agent must not echo past the credit the daemon has given it.
        std::thread::sleep(Duration::from_millis(300));
        let mut echoed = Vec::new();
        stream.clone().read_to_end(&mut echoed).unwrap();
        writer.join().unwrap();
        assert!(echoed == data, "echo differs");
        // The daemon's own check: another port and another workspace.
        assert!(matches!(
            relay.open_tcp(&scope, 5433),
            Err(RelayError::NotAuthorized(_))
        ));
        let other = AttemptScope::new("/p", "lane-2", "a1").unwrap();
        assert!(matches!(
            relay.open_tcp(&other, 5432),
            Err(RelayError::NotAuthorized(_))
        ));
        // Another project's scope is refused before a frame is sent.
        let foreign = AttemptScope::new("/other", "base", "a1").unwrap();
        assert!(matches!(
            relay.open_docker(&foreign),
            Err(RelayError::Invalid(_))
        ));
        // Caps: two for the attempt (the echo channel is finished), a third refused.
        let a = relay.open_docker(&scope).unwrap();
        let b = relay.open_docker(&scope).unwrap();
        assert!(matches!(
            relay.open_docker(&scope),
            Err(RelayError::Limit(_))
        ));
        let c = relay
            .open_docker(&AttemptScope::new("/p", "base", "a2").unwrap())
            .unwrap();
        drop((a, b, c));
        assert!(relay.closed().is_none());
    }

    fn wait_closed(relay: &Relay) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        while relay.closed().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        relay.closed().expect("the relay must end")
    }

    #[test]
    fn restore_requests_are_checked_on_the_mac_and_answers_matched_to_them() {
        let relay = start(false, Limits::default());
        // Arguments the agent would refuse are refused before a frame is sent.
        for (workspace, project, lane, fork) in [
            ("w/../x", "/p", "/p/lane", "f1"),
            ("lane-1", "/p", "/p/lane", "Bad Fork"),
            ("lane-1", "/p", "/p", "f1"),
            ("lane-1", "/p", "/elsewhere/lane", "f1"),
            ("lane-1", "/p", "/p/../q", "f1"),
            ("lane-1", "p", "/p/lane", "f1"),
        ] {
            assert!(
                matches!(
                    relay.restore_up(workspace, project, None, lane, fork),
                    Err(RelayError::Invalid(_))
                ),
                "{workspace} {project} {lane} {fork}"
            );
        }
        assert!(matches!(
            relay.restore_prepare("../../x", "lane-1", "/p", None, "/p/lane", "f1"),
            Err(RelayError::Invalid(_))
        ));
        let prepared = relay
            .restore_prepare("snap_1", "lane-1", "/p", None, "/p/lane", "f1")
            .unwrap();
        // Lane file contents keep their newlines: the daemon writes them.
        assert_eq!(prepared.result["files"]["a"], "line\nline\n");
        let up = relay
            .restore_up("lane-1", "/p", None, "/p/lane", "f1")
            .unwrap();
        assert_eq!(up.result["ok"], true);
        assert!(relay.closed().is_none());
    }

    #[test]
    fn an_unrequested_or_mismatched_restore_answer_ends_the_relay() {
        // The unrequested answer follows `ready` at once, so the relay may meet
        // it during the handshake (start refuses) or just after (it ends).
        match start_with(Quirk::UnrequestedRestored, Limits::default()) {
            Ok(relay) => {
                assert!(wait_closed(&relay).contains("protocol violation"));
                assert!(relay.violated());
            }
            Err(error) => assert!(
                matches!(&error, RelayError::Hostile(why) if why.contains("protocol violation")),
                "{error:?}"
            ),
        }
        let relay = start_with(Quirk::WrongKindAnswer, Limits::default()).unwrap();
        let answer = relay.restore_prepare("snap_1", "lane-1", "/p", None, "/p/lane", "f1");
        assert!(answer.is_err());
        assert!(wait_closed(&relay).contains("protocol violation"));
        assert!(relay.violated());
    }

    #[test]
    #[cfg(not(feature = "test-breaks"))]
    fn a_release_daemon_refuses_an_agent_that_reports_test_break_modes() {
        let error = start_with(Quirk::BreaksInReady, Limits::default()).err();
        assert!(
            matches!(&error, Some(RelayError::Hostile(why)) if why.contains("test break modes")),
            "{error:?}"
        );
    }

    #[test]
    fn a_forged_frame_ends_that_relay_with_a_reason() {
        let relay = start(true, Limits::default());
        let scope = AttemptScope::new("/p", "base", "a1").unwrap();
        // The stream stays open: a frame for a channel the daemon has closed
        // is tolerated, one for an open channel is not.
        let _open = relay.open_docker(&scope);
        let deadline = Instant::now() + Duration::from_secs(5);
        while relay.closed().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let reason = relay.closed().expect("the relay must end");
        assert!(reason.contains("protocol violation"), "{reason}");
        assert!(matches!(
            relay.open_docker(&scope),
            Err(RelayError::Unavailable(_))
        ));
    }
}
