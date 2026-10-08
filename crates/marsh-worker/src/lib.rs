//! Trusted orchestration for one already-admitted container job.
//!
//! A retained worker transport multiplexes control, job output, and terminal
//! reports for independently supervised containers.

use marsh_contracts::{ContainerId, JobSignal, JobSpec, TerminalSize};
pub use marsh_contracts::{ExecutionOutcome, ResourceLimit, SetupStage};
use marsh_runtime::{Attachment, JobRuntime, RuntimeError, RuntimeExit};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

mod capability;
mod output;
pub use output::InterruptibleWriter;
mod transport;
use transport::Publisher;
pub use transport::WorkerTransport;
mod control;
mod drain;
mod setup;
pub use drain::SupervisionReport;

const GRANT_READY_ATTEMPTS: usize = 20;
const GRANT_READY_DELAY: Duration = Duration::from_millis(25);

pub const MAX_CONTROL_FRAME: usize = 1024 * 1024;
pub const MAX_STREAM_CHUNK: usize = 64 * 1024;
pub const WORKER_INPUT_SPOOL_BYTES: usize = 64 * 1024 * 1024;
pub const WORKER_CONTROL_SPOOL_MESSAGES: usize = 4096;

/// Controller-to-worker messages after the initial [`JobSpec`] frame.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerControl {
    Input { bytes: Vec<u8> },
    CloseInput,
    Signal { signal: JobSignal },
    Resize { size: TerminalSize },
}

/// Multiplexed daemon-to-worker protocol for one retained worker transport.
#[allow(clippy::large_enum_variant)] // `Start` carries the job spec once per attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerRequest {
    Ping {
        generation: u64,
        nonce: u64,
    },
    Start {
        attempt: String,
        generation: u64,
        spec: JobSpec,
    },
    Input {
        attempt: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    CloseInput {
        attempt: String,
    },
    Signal {
        attempt: String,
        signal: JobSignal,
    },
    Resize {
        attempt: String,
        size: TerminalSize,
    },
    Cancel {
        attempt: String,
    },
    /// Daemon bytes for one split capability connection.
    CapData {
        attempt: String,
        channel: u32,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    CapClose {
        attempt: String,
        channel: u32,
    },
}

/// Multiplexed worker-to-daemon protocol for one retained worker transport.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerResponse {
    Ready {
        generation: u64,
    },
    Pong {
        generation: u64,
        nonce: u64,
    },
    Started {
        attempt: String,
        container_id: ContainerId,
    },
    Stdout {
        attempt: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    Stderr {
        attempt: String,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    Terminal {
        attempt: String,
        report: WorkerReport,
    },
    Rejected {
        attempt: String,
        message: String,
    },
    /// A job connected to its split capability socket.
    CapOpen {
        attempt: String,
        channel: u32,
    },
    CapData {
        attempt: String,
        channel: u32,
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
    },
    CapClose {
        attempt: String,
        channel: u32,
    },
}

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let a = chunk[0];
            let b = chunk.get(1).copied().unwrap_or(0);
            let c = chunk.get(2).copied().unwrap_or(0);
            encoded.push(char::from(ALPHABET[usize::from(a >> 2)]));
            encoded.push(char::from(ALPHABET[usize::from((a & 3) << 4 | b >> 4)]));
            encoded.push(if chunk.len() > 1 {
                char::from(ALPHABET[usize::from((b & 15) << 2 | c >> 6)])
            } else {
                '='
            });
            encoded.push(if chunk.len() > 2 {
                char::from(ALPHABET[usize::from(c & 63)])
            } else {
                '='
            });
        }
        serializer.serialize_str(&encoded)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() % 4 != 0 {
            return Err(serde::de::Error::custom("invalid base64 length"));
        }
        let mut decoded = Vec::with_capacity(encoded.len() / 4 * 3);
        for chunk in encoded.as_bytes().as_chunks::<4>().0 {
            let value = |byte: u8| -> Option<u8> {
                match byte {
                    b'A'..=b'Z' => Some(byte - b'A'),
                    b'a'..=b'z' => Some(byte - b'a' + 26),
                    b'0'..=b'9' => Some(byte - b'0' + 52),
                    b'+' => Some(62),
                    b'/' => Some(63),
                    _ => None,
                }
            };
            let a = value(chunk[0]).ok_or_else(|| serde::de::Error::custom("invalid base64"))?;
            let b = value(chunk[1]).ok_or_else(|| serde::de::Error::custom("invalid base64"))?;
            decoded.push(a << 2 | b >> 4);
            if chunk[2] == b'=' {
                if chunk[3] != b'=' {
                    return Err(serde::de::Error::custom("invalid base64 padding"));
                }
                continue;
            }
            let c = value(chunk[2]).ok_or_else(|| serde::de::Error::custom("invalid base64"))?;
            decoded.push(b << 4 | c >> 2);
            if chunk[3] != b'=' {
                let d =
                    value(chunk[3]).ok_or_else(|| serde::de::Error::custom("invalid base64"))?;
                decoded.push(c << 6 | d);
            }
        }
        Ok(decoded)
    }
}

/// Exact-container cleanup result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupOutcome {
    NotRequired,
    Verified,
    Uncertain,
}

/// Delivery is independent of the actual runtime execution outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeliveryOutcome {
    Complete,
    Failed,
    Cancelled,
    LimitExceeded { resource: ResourceLimit },
}

/// Final result delivered outside job stdout and stderr.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerReport {
    pub container_id: Option<ContainerId>,
    pub execution: ExecutionOutcome,
    pub delivery: DeliveryOutcome,
    pub cleanup: CleanupOutcome,
    pub quarantine: bool,
    pub control_errors: u32,
    pub retained_processes: Vec<marsh_runtime::RetainedProcess>,
}

/// Lifecycle event published before the final [`WorkerReport`].
///
/// A successful create publishes the runtime's real container identity
/// immediately. Failures which occur before a container exists publish their
/// terminal report so a controller waiting for `Started` cannot hang.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerEvent {
    Started { container_id: ContainerId },
    SetupFailed { report: WorkerReport },
}

/// Raw byte streams owned by the shell/controller.
pub struct JobStreams {
    pub stdout: InterruptibleWriter,
    pub stderr: InterruptibleWriter,
}
impl JobStreams {
    fn delivery(&self) -> DeliveryOutcome {
        if self.stdout.cancellation().is_cancelled() || self.stderr.cancellation().is_cancelled() {
            DeliveryOutcome::Failed
        } else {
            DeliveryOutcome::Complete
        }
    }
}

/// Limits enforced by the worker while supervising an attached process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisionLimits {
    pub wall_time: Duration,
    pub output_bytes: u64,
    pub writable_bytes: u64,
}

/// Blocking control source. Production adapters normally feed this from a
/// dedicated reader thread so timeout enforcement never depends on new input.
pub trait ControlSource: Send {
    /// # Errors
    /// Returns an error when the controller channel is lost or malformed.
    fn receive(&mut self, timeout: Duration) -> Result<Option<WorkerControl>, WorkerError>;

    /// Releases bytes reserved by a bounded input spool after container stdin
    /// has accepted them. Sources without byte accounting use the default.
    fn release_input(&mut self, _bytes: usize) {}

    /// Releases every queued or in-flight input reservation after the child
    /// closes stdin, while retaining signal and resize reservations.
    fn release_all_input(&mut self) {}

    /// Optional out-of-band cancellation used during bounded setup. Retained
    /// mailboxes expose their actual dispatcher-owned token.
    fn cancellation(&self) -> Option<marsh_runtime::Cancellation> {
        None
    }
}

/// Separate lifecycle-event channel observed while a job is active.
pub trait EventSink {
    /// # Errors
    /// Returns an error when the event cannot be durably published.
    fn publish(&mut self, event: &WorkerEvent) -> Result<(), WorkerError>;
}

struct IgnoreEvents;

impl EventSink for IgnoreEvents {
    fn publish(&mut self, _event: &WorkerEvent) -> Result<(), WorkerError> {
        Ok(())
    }
}

fn setup_failure(
    events: &mut dyn EventSink,
    stage: SetupStage,
    cleanup: CleanupOutcome,
    quarantine: bool,
    delivery: DeliveryOutcome,
) -> WorkerReport {
    let report = WorkerReport {
        container_id: None,
        execution: ExecutionOutcome::SetupFailed { stage },
        delivery,
        cleanup,
        quarantine,
        control_errors: 0,
        retained_processes: Vec::new(),
    };
    let _ = events.publish(&WorkerEvent::SetupFailed {
        report: report.clone(),
    });
    report
}

const MAX_SETUP_DIAGNOSTIC: usize = 4 * 1024;

fn write_setup_diagnostic(
    streams: &mut JobStreams,
    output_budget: u64,
    stage: SetupStage,
    error: &dyn std::fmt::Display,
) {
    let stage = match stage {
        SetupStage::Validate => "validate",
        SetupStage::Grant => "grant",
        SetupStage::Create => "create",
        SetupStage::Attach => "attach",
        SetupStage::Start => "start",
    };
    let mut diagnostic = format!("marsh-worker: {stage}: {error}\n")
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect::<String>();
    let budget = usize::try_from(output_budget)
        .unwrap_or(usize::MAX)
        .min(MAX_SETUP_DIAGNOSTIC);
    if diagnostic.len() > budget {
        let mut end = budget;
        while !diagnostic.is_char_boundary(end) {
            end -= 1;
        }
        diagnostic.truncate(end);
    }
    let cancel = streams.stderr.cancellation();
    let _deadline = output::OutputDeadline::new(Duration::from_secs(2), cancel.clone());
    if streams
        .stderr
        .write_all(diagnostic.as_bytes())
        .and_then(|()| streams.stderr.flush())
        .is_err()
    {
        cancel.cancel();
    }
}

/// Injectable execution supervisor for streams, controls, and deadline policy.
pub trait Supervisor: Send + Sync {
    fn supervise(
        &self,
        runtime: Arc<dyn JobRuntime>,
        container: &ContainerId,
        attachment: Attachment,
        controls: &mut dyn ControlSource,
        streams: JobStreams,
        limits: SupervisionLimits,
    ) -> SupervisionReport;
}

/// Stable snapshot of prepared grant sources taken before container creation.
pub trait GrantSnapshot: Send {
    fn still_matches(&self) -> bool;
}

/// Worker-side verification of daemon-prepared grant type and identity.
pub trait GrantVerifier: Send + Sync {
    /// # Errors
    /// Returns an error unless every source is a safe prepared directory.
    fn capture(&self, spec: &JobSpec) -> io::Result<Box<dyn GrantSnapshot>>;
}

/// Trusted one-job worker. Admission and placement happen before this boundary.
pub struct Worker {
    runtime: Arc<dyn JobRuntime>,
    supervisor: Arc<dyn Supervisor>,
    grants: Arc<dyn GrantVerifier>,
}

impl Worker {
    #[must_use]
    pub fn new(runtime: Arc<dyn JobRuntime>, supervisor: Arc<dyn Supervisor>) -> Self {
        Self {
            runtime,
            supervisor,
            grants: Arc::new(FilesystemGrantVerifier),
        }
    }

    #[must_use]
    pub fn with_grant_verifier(
        runtime: Arc<dyn JobRuntime>,
        supervisor: Arc<dyn Supervisor>,
        grants: Arc<dyn GrantVerifier>,
    ) -> Self {
        Self {
            runtime,
            supervisor,
            grants,
        }
    }

    /// Runs one job and always attempts exact-container deletion after create.
    #[must_use]
    pub fn run(
        &self,
        spec: &JobSpec,
        controls: &mut dyn ControlSource,
        streams: JobStreams,
    ) -> WorkerReport {
        self.run_with_events(spec, controls, streams, &mut IgnoreEvents)
    }

    fn prepare_container(
        &self,
        spec: &JobSpec,
        controls: &mut setup::PreparedControls<'_>,
        streams: &mut JobStreams,
        events: &mut dyn EventSink,
    ) -> Result<(Box<dyn GrantSnapshot>, ContainerId), WorkerReport> {
        if let Err(error) = spec.validate() {
            write_setup_diagnostic(
                streams,
                spec.resources.output_bytes,
                SetupStage::Validate,
                &error,
            );
            return Err(setup_failure(
                events,
                SetupStage::Validate,
                CleanupOutcome::NotRequired,
                false,
                streams.delivery(),
            ));
        }
        let grants = self.grants.capture(spec).map_err(|error| {
            write_setup_diagnostic(
                streams,
                spec.resources.output_bytes,
                SetupStage::Grant,
                &error,
            );
            setup_failure(
                events,
                SetupStage::Grant,
                CleanupOutcome::NotRequired,
                false,
                streams.delivery(),
            )
        })?;
        let container = controls
            .create(self.runtime.clone(), spec)
            .map_err(|error| {
                write_setup_diagnostic(
                    streams,
                    spec.resources.output_bytes,
                    SetupStage::Create,
                    &error,
                );
                // ByteBridge is reserved for a known rejection BEFORE Docker
                // create. Unknown/post-create errors, including carrier binding,
                // must retain ownership and quarantine even if deterministic.
                let rejected_before_effect = matches!(error, RuntimeError::ByteBridge(_));
                setup_failure(
                    events,
                    SetupStage::Create,
                    if rejected_before_effect {
                        CleanupOutcome::NotRequired
                    } else {
                        CleanupOutcome::Uncertain
                    },
                    !rejected_before_effect,
                    streams.delivery(),
                )
            })?;
        if let Some(delivery) = &controls.stopped {
            let cleanup = self.delete(&container);
            return Err(WorkerReport {
                container_id: Some(container),
                execution: ExecutionOutcome::NotStarted,
                delivery: delivery.clone(),
                cleanup,
                quarantine: cleanup == CleanupOutcome::Uncertain,
                control_errors: 0,
                retained_processes: Vec::new(),
            });
        }
        if events
            .publish(&WorkerEvent::Started {
                container_id: container.clone(),
            })
            .is_err()
        {
            let cleanup = self.delete(&container);
            return Err(WorkerReport {
                container_id: Some(container),
                execution: ExecutionOutcome::SupervisionFailed,
                delivery: DeliveryOutcome::Failed,
                cleanup,
                quarantine: cleanup == CleanupOutcome::Uncertain,
                control_errors: 0,
                retained_processes: Vec::new(),
            });
        }
        Ok((grants, container))
    }

    fn delete(&self, container: &ContainerId) -> CleanupOutcome {
        let cancel = marsh_runtime::Cancellation::default().with_timeout(Duration::from_secs(5));
        if self.runtime.delete_cancellable(container, &cancel).is_ok() {
            CleanupOutcome::Verified
        } else {
            CleanupOutcome::Uncertain
        }
    }

    /// Runs one job while publishing the create boundary independently from
    /// the terminal report.
    #[must_use]
    pub fn run_with_events(
        &self,
        spec: &JobSpec,
        controls: &mut dyn ControlSource,
        streams: JobStreams,
        events: &mut dyn EventSink,
    ) -> WorkerReport {
        let mut controls = setup::PreparedControls::new(controls);
        let mut report = self.run_prepared(spec, &mut controls, streams, events);
        if let Some(delivery) = &controls.stopped {
            report.delivery = delivery.clone();
        }
        report.retained_processes = marsh_runtime::poll_retained_processes(Duration::ZERO);
        if !report.retained_processes.is_empty() {
            report.cleanup = CleanupOutcome::Uncertain;
            report.quarantine = true;
        }
        report
    }

    fn run_prepared(
        &self,
        spec: &JobSpec,
        controls: &mut setup::PreparedControls<'_>,
        mut streams: JobStreams,
        events: &mut dyn EventSink,
    ) -> WorkerReport {
        let (grants, container) = match self.prepare_container(spec, controls, &mut streams, events)
        {
            Ok(prepared) => prepared,
            Err(report) => return report,
        };
        if !grants.still_matches() {
            write_setup_diagnostic(
                &mut streams,
                spec.resources.output_bytes,
                SetupStage::Grant,
                &"prepared grant identity changed",
            );
            let cleanup = self.delete(&container);
            return WorkerReport {
                container_id: Some(container),
                execution: ExecutionOutcome::SetupFailed {
                    stage: SetupStage::Grant,
                },
                delivery: streams.delivery(),
                cleanup,
                quarantine: cleanup == CleanupOutcome::Uncertain,
                control_errors: 0,
                retained_processes: Vec::new(),
            };
        }
        let execution = match self.runtime.attach(&container, spec.terminal_size) {
            Ok(attachment) => match self.runtime.start(&container) {
                Ok(()) => self.supervisor.supervise(
                    Arc::clone(&self.runtime),
                    &container,
                    attachment,
                    controls,
                    streams,
                    SupervisionLimits {
                        wall_time: Duration::from_secs(spec.resources.wall_seconds),
                        output_bytes: spec.resources.output_bytes,
                        writable_bytes: spec.resources.writable_bytes,
                    },
                ),
                Err(error) => {
                    write_setup_diagnostic(
                        &mut streams,
                        spec.resources.output_bytes,
                        SetupStage::Start,
                        &error,
                    );
                    let clean = marsh_runtime::finish_owned_process(
                        attachment.process,
                        Duration::from_secs(2),
                    );
                    SupervisionReport {
                        execution: ExecutionOutcome::SetupFailed {
                            stage: SetupStage::Start,
                        },
                        delivery: streams.delivery(),
                        cleanup: if clean {
                            CleanupOutcome::Verified
                        } else {
                            CleanupOutcome::Uncertain
                        },
                        control_errors: 0,
                    }
                }
            },
            Err(error) => {
                write_setup_diagnostic(
                    &mut streams,
                    spec.resources.output_bytes,
                    SetupStage::Attach,
                    &error,
                );
                SupervisionReport {
                    execution: ExecutionOutcome::SetupFailed {
                        stage: SetupStage::Attach,
                    },
                    delivery: streams.delivery(),
                    cleanup: CleanupOutcome::NotRequired,
                    control_errors: 0,
                }
            }
        };
        let deleted = self.delete(&container);
        let cleanup = if execution.cleanup == CleanupOutcome::Uncertain {
            CleanupOutcome::Uncertain
        } else {
            deleted
        };
        WorkerReport {
            container_id: Some(container),
            execution: execution.execution,
            delivery: execution.delivery,
            cleanup,
            quarantine: cleanup == CleanupOutcome::Uncertain,
            control_errors: execution.control_errors,
            retained_processes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GrantIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
}

impl GrantIdentity {
    fn capture(path: &Path) -> io::Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "grant source is not a directory",
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            gid: metadata.gid(),
        })
    }

    fn capture_ready(path: &Path) -> io::Result<Self> {
        for attempt in 0..GRANT_READY_ATTEMPTS {
            match Self::capture(path) {
                Ok(identity) => return Ok(identity),
                Err(error)
                    if attempt + 1 < GRANT_READY_ATTEMPTS
                        && matches!(
                            error.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                        ) =>
                {
                    thread::sleep(GRANT_READY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("grant readiness loop always returns")
    }

    fn still_matches(&self) -> bool {
        Self::capture(&self.path).is_ok_and(|current| current == *self)
    }
}

struct GrantSet(Vec<GrantIdentity>);

impl GrantSet {
    fn capture(spec: &JobSpec) -> io::Result<Self> {
        spec.mounts
            .iter()
            .map(|mount| GrantIdentity::capture_ready(&mount.source))
            .collect::<io::Result<Vec<_>>>()
            .map(Self)
    }

    fn still_match(&self) -> bool {
        self.0.iter().all(GrantIdentity::still_matches)
    }
}

impl GrantSnapshot for GrantSet {
    fn still_matches(&self) -> bool {
        self.still_match()
    }
}

#[derive(Clone, Copy, Debug)]
struct FilesystemGrantVerifier;

impl GrantVerifier for FilesystemGrantVerifier {
    fn capture(&self, spec: &JobSpec) -> io::Result<Box<dyn GrantSnapshot>> {
        Ok(Box::new(GrantSet::capture(spec)?))
    }
}

/// Channel-backed source used by the stock framed-input adapter.
pub struct ChannelControlSource {
    receiver: mpsc::Receiver<Result<WorkerControl, WorkerError>>,
    controls_ended: bool,
}

impl ChannelControlSource {
    #[must_use]
    pub fn new(receiver: mpsc::Receiver<Result<WorkerControl, WorkerError>>) -> Self {
        Self {
            receiver,
            controls_ended: false,
        }
    }
}

impl ControlSource for ChannelControlSource {
    fn receive(&mut self, timeout: Duration) -> Result<Option<WorkerControl>, WorkerError> {
        if self.controls_ended {
            thread::sleep(timeout);
            return Ok(None);
        }
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(control)) => Ok(Some(control)),
            Ok(Err(WorkerError::ControlClosed)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.controls_ended = true;
                Err(WorkerError::ControlClosed)
            }
            Ok(Err(error)) => Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
        }
    }
}

/// Default thread-based stream and lifecycle supervisor.
#[derive(Clone, Copy, Debug)]
pub struct ThreadSupervisor {
    pub termination_grace: Duration,
}

impl Default for ThreadSupervisor {
    fn default() -> Self {
        Self {
            termination_grace: Duration::from_secs(2),
        }
    }
}

fn classify_runtime_exit(exit: RuntimeExit, writable_limit: u64) -> ExecutionOutcome {
    // The writable monitor's own kill is the cause of whatever exit follows.
    if exit.writable_exceeded {
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Writable,
        }
    } else if exit.oom_killed {
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Memory,
        }
    } else if exit.code != 0 && exit.pids_max_events.is_some_and(|events| events > 0) {
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Pids,
        }
    } else if exit.code != 0 && exit.writable_bytes >= writable_limit {
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Writable,
        }
    } else {
        ExecutionOutcome::Exited { code: exit.code }
    }
}

fn reserve_output(used: &AtomicU64, limit: u64, requested: usize) -> usize {
    loop {
        let current = used.load(Ordering::Acquire);
        let remaining = limit.saturating_sub(current);
        let allowed = usize::try_from(remaining.min(requested as u64)).unwrap_or(requested);
        if used
            .compare_exchange(
                current,
                current + allowed as u64,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            return allowed;
        }
    }
}

type AttemptControls = Arc<Mutex<BTreeMap<String, Arc<AttemptMailbox>>>>;

#[derive(Default)]
struct AttemptMailboxState {
    priority: VecDeque<Result<WorkerControl, WorkerError>>,
    ordered_input: VecDeque<WorkerControl>,
    reserved_input_bytes: usize,
    reserved_messages: usize,
    closed: bool,
    failed: bool,
    input_discarded: bool,
}

#[derive(Default)]
struct AttemptMailbox {
    state: Mutex<AttemptMailboxState>,
    ready: Condvar,
    attempt_cancel: Option<marsh_runtime::Cancellation>,
}

impl AttemptMailbox {
    fn push(&self, control: WorkerControl) {
        let stopping = matches!(
            control,
            WorkerControl::Signal {
                signal: JobSignal::Terminate | JobSignal::Kill | JobSignal::Hangup
            }
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed || state.failed {
            return;
        }
        if state.input_discarded
            && matches!(
                &control,
                WorkerControl::Input { .. } | WorkerControl::CloseInput
            )
        {
            return;
        }
        if state.reserved_messages >= WORKER_CONTROL_SPOOL_MESSAGES {
            state.ordered_input.clear();
            state.priority.clear();
            state.reserved_input_bytes = 0;
            state.reserved_messages = 1;
            state.failed = true;
            state
                .priority
                .push_front(Err(WorkerError::ControlSpoolFull));
            self.ready.notify_one();
            return;
        }
        state.reserved_messages += 1;
        match control {
            WorkerControl::Input { bytes } => {
                if state.reserved_input_bytes.saturating_add(bytes.len()) > WORKER_INPUT_SPOOL_BYTES
                {
                    state.ordered_input.clear();
                    state.priority.clear();
                    state.reserved_input_bytes = 0;
                    state.reserved_messages = 1;
                    state.failed = true;
                    state.priority.push_front(Err(WorkerError::InputSpoolFull));
                } else {
                    state.reserved_input_bytes += bytes.len();
                    state
                        .ordered_input
                        .push_back(WorkerControl::Input { bytes });
                }
            }
            WorkerControl::CloseInput => state.ordered_input.push_back(WorkerControl::CloseInput),
            control => state.priority.push_back(Ok(control)),
        }
        // Publish control intent before waking cancelled output producers.
        if stopping && let Some(cancel) = &self.attempt_cancel {
            cancel.cancel();
        }
        self.ready.notify_one();
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        if let Some(cancel) = &self.attempt_cancel {
            cancel.cancel();
        }
        self.ready.notify_all();
    }
}

struct MailboxControlSource {
    mailbox: Arc<AttemptMailbox>,
    controls_ended: bool,
}

impl MailboxControlSource {
    fn new(mailbox: Arc<AttemptMailbox>) -> Self {
        Self {
            mailbox,
            controls_ended: false,
        }
    }
}

impl ControlSource for MailboxControlSource {
    fn cancellation(&self) -> Option<marsh_runtime::Cancellation> {
        self.mailbox.attempt_cancel.clone()
    }
    fn receive(&mut self, timeout: Duration) -> Result<Option<WorkerControl>, WorkerError> {
        if self.controls_ended {
            thread::sleep(timeout);
            return Ok(None);
        }
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.closed {
                self.controls_ended = true;
                return Err(WorkerError::ControlClosed);
            }
            if let Some(result) = state.priority.pop_front() {
                state.reserved_messages = state.reserved_messages.saturating_sub(1);
                return result.map(Some);
            }
            if let Some(control) = state.ordered_input.pop_front() {
                if control == WorkerControl::CloseInput {
                    state.reserved_messages = state.reserved_messages.saturating_sub(1);
                }
                return Ok(Some(control));
            }
            let Some(deadline) = deadline else {
                return Ok(None);
            };
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            let (next, wait) = self
                .mailbox
                .ready
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if wait.timed_out() {
                return Ok(None);
            }
        }
    }

    fn release_input(&mut self, bytes: usize) {
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reserved_input_bytes = state.reserved_input_bytes.saturating_sub(bytes);
        state.reserved_messages = state.reserved_messages.saturating_sub(1);
        self.mailbox.ready.notify_all();
    }

    fn release_all_input(&mut self) {
        let mut state = self
            .mailbox
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.ordered_input.clear();
        state.reserved_input_bytes = 0;
        state.reserved_messages = state.priority.len();
        state.input_discarded = true;
        self.mailbox.ready.notify_all();
    }
}

struct MultiplexedEventSink {
    attempt: String,
    output: Publisher,
}

impl EventSink for MultiplexedEventSink {
    fn publish(&mut self, event: &WorkerEvent) -> Result<(), WorkerError> {
        let response = match event {
            WorkerEvent::Started { container_id } => WorkerResponse::Started {
                attempt: self.attempt.clone(),
                container_id: container_id.clone(),
            },
            WorkerEvent::SetupFailed { .. } => return Ok(()),
        };
        self.output.send(&response)
    }
}

struct MultiplexedStream {
    attempt: String,
    stderr: bool,
    output: Publisher,
    cancel: marsh_runtime::Cancellation,
}

impl Write for MultiplexedStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len().min(MAX_STREAM_CHUNK);
        let response = if self.stderr {
            WorkerResponse::Stderr {
                attempt: self.attempt.clone(),
                bytes: bytes[..count].to_vec(),
            }
        } else {
            WorkerResponse::Stdout {
                attempt: self.attempt.clone(),
                bytes: bytes[..count].to_vec(),
            }
        };
        self.output.send(&response).map_err(io::Error::other)?;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.cancel.check()?;
        if self.output.cancelled() {
            Err(io::ErrorKind::ConnectionAborted.into())
        } else {
            Ok(())
        }
    }
}

fn valid_attempt(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn send_worker_response(output: &Publisher, response: &WorkerResponse) -> Result<(), WorkerError> {
    output.send(response)
}

fn send_worker_terminal(
    output: &Publisher,
    controls: &AttemptControls,
    attempt: String,
    report: WorkerReport,
) -> Result<(), WorkerError> {
    let releasing = controls.clone();
    let slot = attempt.clone();
    output.terminal(
        &WorkerResponse::Terminal { attempt, report },
        Box::new(move || {
            releasing
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&slot);
        }),
    )
}

struct RetainedTasks {
    handles: Vec<thread::JoinHandle<()>>,
    controls: AttemptControls,
    output: Publisher,
}
impl RetainedTasks {
    fn respond(&self, response: &WorkerResponse) -> Result<(), WorkerError> {
        self.output.control(response)
    }
}
impl Drop for RetainedTasks {
    fn drop(&mut self) {
        self.output.close();
        for mailbox in self
            .controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            mailbox.close();
        }
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Serves concurrent jobs over one retained, bounded framed transport.
///
/// Input is read serially and dispatched through bounded per-attempt queues.
/// One bounded fair writer owns the transport. Attempt cancellation revokes
/// only unstarted frames; a started frame follows transport liveness, never
/// the attempt cancellation or wall clock.
///
/// # Errors
/// Returns a protocol, capacity, generation, or transport error. Losing the
/// controller cancels all active attempts before the server returns.
#[allow(clippy::too_many_lines)]
pub fn serve_retained(
    worker: &Arc<Worker>,
    generation: u64,
    capacity: usize,
    transport: WorkerTransport,
) -> Result<(), WorkerError> {
    if capacity == 0 || capacity > usize::from(marsh_contracts::WORKER_CONTAINER_CAPACITY) {
        return Err(WorkerError::InvalidCapacity);
    }
    let (mut input, transport_writer) = transport.start();
    let output = transport_writer.publisher();
    let controls: AttemptControls = Arc::new(Mutex::new(BTreeMap::new()));
    let capabilities: capability::Streams = Arc::new(Mutex::new(BTreeMap::new()));
    let mut tasks = RetainedTasks {
        handles: Vec::new(),
        controls: controls.clone(),
        output: output.clone(),
    };
    send_worker_response(&output, &WorkerResponse::Ready { generation })?;
    loop {
        if output.cancelled() {
            return Err(WorkerError::ControlClosed);
        }
        let request = match read_frame::<WorkerRequest>(&mut input) {
            Ok(request) => request,
            Err(WorkerError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                let mailboxes = controls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                for mailbox in mailboxes {
                    mailbox.close();
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        match request {
            WorkerRequest::Ping {
                generation: requested,
                nonce,
            } if requested == generation => {
                tasks.respond(&WorkerResponse::Pong { generation, nonce })?;
            }
            WorkerRequest::Ping { .. } => return Err(WorkerError::GenerationMismatch),
            WorkerRequest::CapData {
                attempt,
                channel,
                bytes,
            } => capability::deliver(&capabilities, &attempt, channel, &bytes),
            WorkerRequest::CapClose { attempt, channel } => {
                capability::close(&capabilities, &attempt, channel);
            }
            WorkerRequest::Start {
                attempt,
                generation: requested,
                mut spec,
            } => {
                let rejection =
                    if !marsh_runtime::poll_retained_processes(Duration::ZERO).is_empty() {
                        Some("worker has retained local cleanup uncertainty")
                    } else if requested != generation {
                        Some("worker generation mismatch")
                    } else if !valid_attempt(&attempt) {
                        Some("invalid attempt identity")
                    } else {
                        let active = controls
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if active.contains_key(&attempt) {
                            Some("duplicate attempt identity")
                        } else if active.len() >= capacity {
                            Some("worker capacity exceeded")
                        } else {
                            None
                        }
                    };
                if let Some(message) = rejection {
                    tasks.respond(&WorkerResponse::Rejected {
                        attempt,
                        message: message.into(),
                    })?;
                    continue;
                }
                let attempt_cancel = marsh_runtime::Cancellation::default();
                let mailbox = Arc::new(AttemptMailbox {
                    attempt_cancel: Some(attempt_cancel.clone()),
                    ..AttemptMailbox::default()
                });
                controls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(attempt.clone(), Arc::clone(&mailbox));
                // Every Kit job gets its own capability (`docs/design/processes.md` s4).
                spec.split_capability = None;
                let capability = match spec.capability.clone().map(|job| {
                    capability::Capability::start(
                        &attempt,
                        spec.identity.uid,
                        spec.identity.gid,
                        output.clone(),
                        Arc::clone(&capabilities),
                        &job,
                    )
                }) {
                    None => None,
                    Some(Ok(capability)) => Some(capability),
                    Some(Err(_)) => {
                        // Never run a job without its `/run/marsh`.
                        let report = setup_failure(
                            &mut IgnoreEvents,
                            SetupStage::Create,
                            CleanupOutcome::NotRequired,
                            false,
                            DeliveryOutcome::Complete,
                        );
                        send_worker_terminal(&output, &controls, attempt, report)?;
                        continue;
                    }
                };
                spec.split_capability = capability.as_ref().map(|cap| cap.dir().to_path_buf());
                let task_worker = Arc::clone(worker);
                let task_output = output.attempt(attempt_cancel.clone());
                let task_controls = Arc::clone(&controls);
                let mut index = 0;
                while index < tasks.handles.len() {
                    if tasks.handles[index].is_finished() {
                        let _ = tasks.handles.swap_remove(index).join();
                    } else {
                        index += 1;
                    }
                }
                tasks.handles.push(thread::spawn(move || {
                    let mut source = MailboxControlSource::new(Arc::clone(&mailbox));
                    let mut events = MultiplexedEventSink {
                        attempt: attempt.clone(),
                        output: task_output.clone(),
                    };
                    let report = task_worker.run_with_events(
                        &spec,
                        &mut source,
                        JobStreams {
                            stdout: InterruptibleWriter::new(
                                Box::new(MultiplexedStream {
                                    attempt: attempt.clone(),
                                    stderr: false,
                                    output: task_output.clone(),
                                    cancel: attempt_cancel.clone(),
                                }),
                                attempt_cancel.clone(),
                            ),
                            stderr: InterruptibleWriter::new(
                                Box::new(MultiplexedStream {
                                    attempt: attempt.clone(),
                                    stderr: true,
                                    output: task_output.clone(),
                                    cancel: attempt_cancel.clone(),
                                }),
                                attempt_cancel.clone(),
                            ),
                        },
                        &mut events,
                    );
                    drop(source);
                    drop(capability);
                    // Do not cancel the transport on healthy completion. All
                    // per-job pumps are joined before terminal publication.
                    let _ = send_worker_terminal(&task_output, &task_controls, attempt, report);
                }));
            }
            request => {
                let (attempt, control) = match request {
                    WorkerRequest::Input { attempt, bytes } => {
                        (attempt, WorkerControl::Input { bytes })
                    }
                    WorkerRequest::CloseInput { attempt } => (attempt, WorkerControl::CloseInput),
                    WorkerRequest::Signal { attempt, signal } => {
                        (attempt, WorkerControl::Signal { signal })
                    }
                    WorkerRequest::Resize { attempt, size } => {
                        (attempt, WorkerControl::Resize { size })
                    }
                    WorkerRequest::Cancel { attempt } => (
                        attempt,
                        WorkerControl::Signal {
                            signal: JobSignal::Kill,
                        },
                    ),
                    WorkerRequest::Ping { .. }
                    | WorkerRequest::Start { .. }
                    | WorkerRequest::CapData { .. }
                    | WorkerRequest::CapClose { .. } => unreachable!(),
                };
                let active = controls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(route) = active.get(&attempt) {
                    route.push(control);
                } else {
                    // Controls may race with a fast job's terminal frame. An
                    // unknown control has no effect and is safe to ignore;
                    // rejecting it could be mistaken for a second terminal
                    // outcome for an already-completed attempt.
                }
            }
        }
    }
}

/// Reads one bounded length-prefixed JSON control frame.
///
/// # Errors
/// Returns an error for truncated, oversized, or invalid JSON frames.
pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, WorkerError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length =
        usize::try_from(u32::from_be_bytes(length)).map_err(|_| WorkerError::FrameTooLarge)?;
    if length == 0 || length > MAX_CONTROL_FRAME {
        return Err(WorkerError::FrameTooLarge);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Writes one bounded length-prefixed JSON control frame.
///
/// # Errors
/// Returns an error when serialization or output fails, or the frame is too large.
pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), WorkerError> {
    let bytes = serde_json::to_vec(value)?;
    let length = u32::try_from(bytes.len()).map_err(|_| WorkerError::FrameTooLarge)?;
    if bytes.is_empty() || bytes.len() > MAX_CONTROL_FRAME {
        return Err(WorkerError::FrameTooLarge);
    }
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("worker control I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("worker control JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("worker control frame exceeds its bound")]
    FrameTooLarge,
    #[error("worker control input closed")]
    ControlClosed,
    #[error("worker input spool exceeded its bounded byte budget")]
    InputSpoolFull,
    #[error("worker control spool exceeded its bounded message budget")]
    ControlSpoolFull,
    #[error("worker generation does not match the retained transport")]
    GenerationMismatch,
    #[error("worker transport capacity must be positive")]
    InvalidCapacity,
    #[error("container runtime failed: {0}")]
    Runtime(#[from] RuntimeError),
}

impl From<rustix::io::Errno> for WorkerError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}

#[cfg(test)]
mod carrier_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod wait_cancellation_tests;
