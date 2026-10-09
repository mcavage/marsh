use super::*;
use marsh_contracts::{JobIdentity, JobMount, JobResources, MountAccess, OciImage};
use marsh_runtime::{AttachedProcess, no_attachment_control};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{Cursor, Write},
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixStream,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
};

const INPUT_BURST_FRAMES: usize = 64;

fn serve_retained(
    worker: &Arc<Worker>,
    generation: u64,
    capacity: usize,
    input: UnixStream,
    output: InterruptibleWriter,
) -> Result<(), WorkerError> {
    // The original clone is no longer a transport writer. The combined
    // constructor owns cancellation coupling for both socket directions.
    drop(output);
    super::serve_retained(
        worker,
        generation,
        capacity,
        WorkerTransport::from_socket(input)?,
    )
}

#[test]
fn binary_transport_round_trips_multi_megabyte_input_without_json_array_expansion() {
    let bytes = (0..4 * 1024 * 1024)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect::<Vec<_>>();
    let mut framed = Vec::new();
    for chunk in bytes.chunks(MAX_STREAM_CHUNK) {
        write_frame(
            &mut framed,
            &WorkerRequest::Input {
                attempt: "attempt-1".into(),
                bytes: chunk.to_vec(),
            },
        )
        .unwrap();
    }
    assert!(framed.len() < bytes.len() * 3 / 2);
    let mut decoded = Vec::new();
    let mut reader = Cursor::new(framed);
    while usize::try_from(reader.position()).unwrap() < reader.get_ref().len() {
        let WorkerRequest::Input { bytes, .. } = read_frame(&mut reader).unwrap() else {
            panic!("unexpected transport frame");
        };
        decoded.extend(bytes);
    }
    assert_eq!(decoded, bytes);
}

#[test]
fn grant_capture_waits_for_bounded_guest_mount_visibility() {
    let root = std::env::temp_dir().join(format!(
        "marsh-worker-grant-ready-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let parent = root.join("grant-root");
    let source = parent.join("project");
    fs::create_dir_all(&source).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o000)).unwrap();
    let restore = parent.clone();
    let ready = thread::spawn(move || {
        thread::sleep(Duration::from_millis(60));
        fs::set_permissions(restore, fs::Permissions::from_mode(0o700)).unwrap();
    });

    let identity = GrantIdentity::capture_ready(&source).unwrap();
    ready.join().unwrap();
    assert_eq!(identity.path, source);
    fs::remove_dir_all(root).unwrap();
}

struct FakeProcess;

// Bounded in-memory IO fixture only; real cancellation/reaping coverage is in
// carrier_tests. No production attachment may use this fixture contract.
impl AttachedProcess for FakeProcess {
    fn supports_io_cancellation(&self) -> bool {
        true
    }
    fn cancel_io(&self) -> io::Result<()> {
        Ok(())
    }
    fn wait(&mut self) -> io::Result<i32> {
        Ok(0)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(Some(0))
    }

    fn terminate(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FailedAttachProcess;

impl AttachedProcess for FailedAttachProcess {
    fn supports_io_cancellation(&self) -> bool {
        true
    }
    fn cancel_io(&self) -> io::Result<()> {
        Ok(())
    }
    fn wait(&mut self) -> io::Result<i32> {
        Ok(125)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(Some(125))
    }

    fn terminate(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DelayedCleanProcess {
    ready_at: Instant,
    terminated: Arc<AtomicBool>,
}

impl AttachedProcess for DelayedCleanProcess {
    fn supports_io_cancellation(&self) -> bool {
        true
    }
    fn cancel_io(&self) -> io::Result<()> {
        Ok(())
    }
    fn wait(&mut self) -> io::Result<i32> {
        if let Some(delay) = self.ready_at.checked_duration_since(Instant::now()) {
            thread::sleep(delay);
        }
        Ok(0)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok((Instant::now() >= self.ready_at).then_some(0))
    }

    fn terminate(&mut self) -> io::Result<()> {
        self.terminated.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct DelayedReader {
    ready_at: Instant,
    bytes: Cursor<Vec<u8>>,
}

struct PausedAfterChunk {
    chunk: Option<Vec<u8>>,
    release: mpsc::Receiver<()>,
}

impl io::Read for PausedAfterChunk {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let Some(chunk) = self.chunk.take() else {
            self.release.recv().unwrap();
            return Ok(0);
        };
        output[..chunk.len()].copy_from_slice(&chunk);
        Ok(chunk.len())
    }
}

struct FlushNotifier {
    bytes: Vec<u8>,
    flushed: mpsc::Sender<Vec<u8>>,
}

impl Write for FlushNotifier {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushed.send(self.bytes.clone()).unwrap();
        Ok(())
    }
}

#[test]
fn stream_copy_flushes_partial_chunk_before_input_closes() {
    let (release_send, release_receive) = mpsc::channel();
    let (flushed_send, flushed_receive) = mpsc::channel();
    let (event_send, _event_receive) = mpsc::channel();
    let task = drain::spawn_copy(
        Box::new(PausedAfterChunk {
            chunk: Some(b"partial prompt".to_vec()),
            release: release_receive,
        }),
        InterruptibleWriter::new(
            Box::new(FlushNotifier {
                bytes: Vec::new(),
                flushed: flushed_send,
            }),
            marsh_runtime::Cancellation::default(),
        ),
        Arc::new(AtomicU64::new(0)),
        1024,
        event_send,
    );

    assert_eq!(
        flushed_receive
            .recv_timeout(Duration::from_secs(1))
            .expect("partial output must be flushed while the stream remains open"),
        b"partial prompt"
    );
    release_send.send(()).unwrap();
    task.join().unwrap();
}

impl io::Read for DelayedReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if let Some(delay) = self.ready_at.checked_duration_since(Instant::now()) {
            thread::sleep(delay);
        }
        self.bytes.read(output)
    }
}

#[derive(Default)]
struct FakeRuntime {
    calls: Mutex<Vec<&'static str>>,
    setup_failure: Option<SetupStage>,
    create_error: Mutex<Option<RuntimeError>>,
    delete_fails: bool,
}

impl FakeRuntime {
    fn attachment() -> Attachment {
        Attachment {
            stdin: Box::new(io::sink()),
            stdout: Box::new(Cursor::new(Vec::<u8>::new())),
            stderr: Box::new(Cursor::new(Vec::<u8>::new())),
            process: Box::new(FakeProcess),
            control: no_attachment_control(),
        }
    }
}

impl JobRuntime for FakeRuntime {
    fn create_cancellable(
        &self,
        spec: &JobSpec,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<ContainerId, RuntimeError> {
        cancel.check()?;
        self.create(spec)
    }
    fn delete_cancellable(
        &self,
        container: &ContainerId,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.delete(container)
    }
    fn signal_cancellable(
        &self,
        container: &ContainerId,
        signal: JobSignal,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.signal(container, signal)
    }
    fn resize_cancellable(
        &self,
        container: &ContainerId,
        size: TerminalSize,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.resize(container, size)
    }
    fn create(&self, _spec: &JobSpec) -> Result<ContainerId, RuntimeError> {
        self.calls.lock().unwrap().push("create");
        if let Some(error) = self.create_error.lock().unwrap().take() {
            return Err(error);
        }
        if self.setup_failure == Some(SetupStage::Create) {
            Err(RuntimeError::CommandFailed {
                operation: "create",
                exit_code: Some(125),
                stderr: "image configuration rejected".into(),
            })
        } else {
            Ok(ContainerId::parse("a".repeat(64)).unwrap())
        }
    }

    fn attach(
        &self,
        _container: &ContainerId,
        _terminal_size: Option<TerminalSize>,
    ) -> Result<Attachment, RuntimeError> {
        self.calls.lock().unwrap().push("attach");
        if self.setup_failure == Some(SetupStage::Attach) {
            Err(RuntimeError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "docker executable unavailable",
            )))
        } else {
            Ok(Self::attachment())
        }
    }

    fn start(&self, _container: &ContainerId) -> Result<(), RuntimeError> {
        self.calls.lock().unwrap().push("start");
        if self.setup_failure == Some(SetupStage::Start) {
            Err(RuntimeError::CommandFailed {
                operation: "start",
                exit_code: Some(125),
                stderr: "start rejected".into(),
            })
        } else {
            Ok(())
        }
    }

    fn wait_cancellable(
        &self,
        container: &ContainerId,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<RuntimeExit, RuntimeError> {
        cancel.check()?;
        self.wait(container)
    }
    fn wait(&self, _container: &ContainerId) -> Result<RuntimeExit, RuntimeError> {
        self.calls.lock().unwrap().push("wait");
        Ok(runtime_exit(23))
    }

    fn signal(&self, _container: &ContainerId, _signal: JobSignal) -> Result<(), RuntimeError> {
        self.calls.lock().unwrap().push("signal");
        Ok(())
    }

    fn resize(&self, _container: &ContainerId, _size: TerminalSize) -> Result<(), RuntimeError> {
        self.calls.lock().unwrap().push("resize");
        Ok(())
    }

    fn delete(&self, _container: &ContainerId) -> Result<(), RuntimeError> {
        self.calls.lock().unwrap().push("delete");
        if self.delete_fails {
            Err(RuntimeError::DeletionUncertain)
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct UniqueRuntime(AtomicU64);

impl JobRuntime for UniqueRuntime {
    fn create_cancellable(
        &self,
        spec: &JobSpec,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<ContainerId, RuntimeError> {
        cancel.check()?;
        self.create(spec)
    }
    fn delete_cancellable(
        &self,
        container: &ContainerId,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.delete(container)
    }
    fn signal_cancellable(
        &self,
        container: &ContainerId,
        signal: JobSignal,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.signal(container, signal)
    }
    fn resize_cancellable(
        &self,
        container: &ContainerId,
        size: TerminalSize,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.resize(container, size)
    }
    fn create(&self, _spec: &JobSpec) -> Result<ContainerId, RuntimeError> {
        let sequence = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        ContainerId::parse(format!("{sequence:064x}"))
            .map_err(|_| RuntimeError::InvalidContainerIdentity)
    }

    fn attach(
        &self,
        _container: &ContainerId,
        _terminal_size: Option<TerminalSize>,
    ) -> Result<Attachment, RuntimeError> {
        Ok(FakeRuntime::attachment())
    }

    fn start(&self, _container: &ContainerId) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn wait_cancellable(
        &self,
        container: &ContainerId,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<RuntimeExit, RuntimeError> {
        cancel.check()?;
        self.wait(container)
    }
    fn wait(&self, _container: &ContainerId) -> Result<RuntimeExit, RuntimeError> {
        Ok(runtime_exit(0))
    }

    fn signal(&self, _container: &ContainerId, _signal: JobSignal) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn resize(&self, _container: &ContainerId, _size: TerminalSize) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn delete(&self, _container: &ContainerId) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[test]
fn retained_transport_multiplexes_fresh_containers_and_terminal_reports() {
    let (mut controller, worker_socket) = UnixStream::pair().unwrap();
    let worker_output = socket_output(worker_socket.try_clone().unwrap());
    let retained = Arc::new(worker(
        Arc::new(UniqueRuntime::default()),
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
    ));
    let server = thread::spawn(move || {
        serve_retained(&retained, 7, 8, worker_socket, worker_output).unwrap();
    });
    assert_eq!(
        read_frame::<WorkerResponse>(&mut controller).unwrap(),
        WorkerResponse::Ready { generation: 7 }
    );
    for attempt in ["attempt-1", "attempt-2"] {
        write_frame(
            &mut controller,
            &WorkerRequest::Start {
                attempt: attempt.into(),
                generation: 7,
                spec: job(),
            },
        )
        .unwrap();
    }
    let mut containers = BTreeSet::new();
    let mut terminal = BTreeSet::new();
    while terminal.len() < 2 {
        match read_frame::<WorkerResponse>(&mut controller).unwrap() {
            WorkerResponse::Started {
                attempt,
                container_id,
            } => {
                assert!(matches!(attempt.as_str(), "attempt-1" | "attempt-2"));
                containers.insert(container_id);
            }
            WorkerResponse::Terminal { attempt, report } => {
                assert_eq!(report.cleanup, CleanupOutcome::Verified);
                terminal.insert(attempt);
            }
            response => panic!("unexpected response: {response:?}"),
        }
    }
    assert_eq!(containers.len(), 2);
    drop(controller);
    server.join().unwrap();
}

fn write_start(controller: &mut UnixStream, attempt: &str) {
    write_frame(
        controller,
        &WorkerRequest::Start {
            attempt: attempt.into(),
            generation: 7,
            spec: job(),
        },
    )
    .unwrap();
}

#[test]
fn stalled_attempt_input_cannot_block_sibling_terminal_or_ping() {
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (mut controller, worker_socket) = UnixStream::pair().unwrap();
    controller
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let worker_output = socket_output(worker_socket.try_clone().unwrap());
    let retained = Arc::new(worker(
        Arc::new(UniqueRuntime::default()),
        Arc::new(SelectiveStallSupervisor(Arc::clone(&release))),
    ));
    let server = thread::spawn(move || {
        serve_retained(&retained, 7, 8, worker_socket, worker_output).unwrap();
    });
    assert_eq!(
        read_frame::<WorkerResponse>(&mut controller).unwrap(),
        WorkerResponse::Ready { generation: 7 }
    );
    write_start(&mut controller, "stalled");
    match read_frame::<WorkerResponse>(&mut controller).unwrap() {
        WorkerResponse::Started { attempt, .. } if attempt == "stalled" => {}
        WorkerResponse::Rejected { attempt, message } => {
            panic!("unexpected rejection for {attempt}: {message}");
        }
        response => panic!("unexpected response before stalled attempt starts: {response:?}"),
    }
    write_start(&mut controller, "sibling");
    for sequence in 0..INPUT_BURST_FRAMES {
        write_frame(
            &mut controller,
            &WorkerRequest::Input {
                attempt: "stalled".into(),
                bytes: vec![u8::try_from(sequence).unwrap(); MAX_STREAM_CHUNK],
            },
        )
        .unwrap();
    }
    write_frame(
        &mut controller,
        &WorkerRequest::Ping {
            generation: 7,
            nonce: 1,
        },
    )
    .unwrap();

    let mut pong = false;
    let mut sibling_terminal = false;
    while !pong || !sibling_terminal {
        match read_frame::<WorkerResponse>(&mut controller).unwrap() {
            WorkerResponse::Pong {
                generation: 7,
                nonce: 1,
            } => pong = true,
            WorkerResponse::Terminal {
                attempt, report, ..
            } if attempt == "sibling" => {
                assert_eq!(report.execution, ExecutionOutcome::Exited { code: 0 });
                sibling_terminal = true;
            }
            WorkerResponse::Rejected { attempt, message } => {
                panic!("unexpected rejection for {attempt}: {message}");
            }
            _ => {}
        }
    }
    write_frame(
        &mut controller,
        &WorkerRequest::Cancel {
            attempt: "stalled".into(),
        },
    )
    .unwrap();
    let (lock, ready) = &*release;
    *lock.lock().unwrap() = true;
    ready.notify_all();
    loop {
        match read_frame::<WorkerResponse>(&mut controller).unwrap() {
            WorkerResponse::Terminal {
                attempt, report, ..
            } if attempt == "stalled" => {
                assert_eq!(report.execution, ExecutionOutcome::SupervisionFailed);
                assert_eq!(report.cleanup, CleanupOutcome::Verified);
                break;
            }
            WorkerResponse::Rejected { attempt, message } => {
                panic!("unexpected rejection for {attempt}: {message}");
            }
            _ => {}
        }
    }
    drop(controller);
    server.join().unwrap();
}

#[test]
fn per_attempt_spool_preserves_multi_megabyte_input_for_a_slow_consumer() {
    let mailbox = Arc::new(AttemptMailbox::default());
    let mut expected = Vec::new();
    for sequence in 0_u8..64 {
        let bytes = vec![sequence; MAX_STREAM_CHUNK];
        expected.extend_from_slice(&bytes);
        mailbox.push(WorkerControl::Input { bytes });
    }
    mailbox.push(WorkerControl::CloseInput);
    let size = TerminalSize {
        rows: 41,
        columns: 119,
    };
    mailbox.push(WorkerControl::Resize { size });

    let mut source = MailboxControlSource::new(mailbox);
    assert_eq!(
        source.receive(Duration::ZERO).unwrap(),
        Some(WorkerControl::Resize { size })
    );
    let mut actual = Vec::new();
    loop {
        match source.receive(Duration::ZERO).unwrap() {
            Some(WorkerControl::Input { bytes }) => {
                thread::sleep(Duration::from_millis(1));
                source.release_input(bytes.len());
                actual.extend_from_slice(&bytes);
            }
            Some(WorkerControl::CloseInput) => break,
            control => panic!("unexpected spooled control: {control:?}"),
        }
    }
    assert_eq!(actual, expected);
}

#[test]
fn per_attempt_spool_bounds_zero_byte_and_control_message_count() {
    let mailbox = Arc::new(AttemptMailbox::default());
    for _ in 0..WORKER_CONTROL_SPOOL_MESSAGES {
        mailbox.push(WorkerControl::Input { bytes: Vec::new() });
    }
    mailbox.push(WorkerControl::Resize {
        size: TerminalSize {
            rows: 24,
            columns: 80,
        },
    });
    let state = mailbox.state.lock().unwrap();
    assert_eq!(state.reserved_messages, 1);
    assert!(state.ordered_input.is_empty());
    assert_eq!(state.priority.len(), 1);
    drop(state);
    let mut source = MailboxControlSource::new(mailbox);
    assert!(matches!(
        source.receive(Duration::ZERO),
        Err(WorkerError::ControlSpoolFull)
    ));
}

#[test]
fn late_controls_after_fast_terminal_are_ignored_without_false_rejection() {
    let (mut controller, worker_socket) = UnixStream::pair().unwrap();
    controller
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let worker_output = socket_output(worker_socket.try_clone().unwrap());
    let retained = Arc::new(worker(
        Arc::new(UniqueRuntime::default()),
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
    ));
    let server = thread::spawn(move || {
        serve_retained(&retained, 7, 8, worker_socket, worker_output).unwrap();
    });
    assert!(matches!(
        read_frame::<WorkerResponse>(&mut controller).unwrap(),
        WorkerResponse::Ready { generation: 7 }
    ));
    write_frame(
        &mut controller,
        &WorkerRequest::Start {
            attempt: "fast".into(),
            generation: 7,
            spec: job(),
        },
    )
    .unwrap();
    loop {
        if matches!(
            read_frame::<WorkerResponse>(&mut controller).unwrap(),
            WorkerResponse::Terminal { attempt, .. } if attempt == "fast"
        ) {
            break;
        }
    }
    for request in [
        WorkerRequest::CloseInput {
            attempt: "fast".into(),
        },
        WorkerRequest::Resize {
            attempt: "fast".into(),
            size: TerminalSize {
                rows: 24,
                columns: 80,
            },
        },
        WorkerRequest::Signal {
            attempt: "fast".into(),
            signal: JobSignal::Terminate,
        },
        WorkerRequest::Ping {
            generation: 7,
            nonce: 2,
        },
    ] {
        write_frame(&mut controller, &request).unwrap();
    }
    assert_eq!(
        read_frame::<WorkerResponse>(&mut controller).unwrap(),
        WorkerResponse::Pong {
            generation: 7,
            nonce: 2
        }
    );
    drop(controller);
    server.join().unwrap();
}

struct SelectiveStallSupervisor(Arc<(Mutex<bool>, Condvar)>);

impl Supervisor for SelectiveStallSupervisor {
    fn supervise(
        &self,
        _runtime: Arc<dyn JobRuntime>,
        container: &ContainerId,
        _attachment: Attachment,
        controls: &mut dyn ControlSource,
        _streams: JobStreams,
        _limits: SupervisionLimits,
    ) -> SupervisionReport {
        if !container.as_str().ends_with('1') {
            return SupervisionReport::complete(ExecutionOutcome::Exited { code: 0 });
        }
        let (lock, ready) = &*self.0;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = ready.wait(released).unwrap();
        }
        drop(released);
        loop {
            match controls.receive(Duration::from_millis(1)) {
                Ok(Some(WorkerControl::Signal {
                    signal: JobSignal::Kill,
                })) => return SupervisionReport::complete(ExecutionOutcome::SupervisionFailed),
                Ok(_) => {}
                Err(WorkerError::ControlClosed) => {
                    return SupervisionReport::complete(ExecutionOutcome::SupervisionFailed);
                }
                Err(error) => panic!("unexpected control error: {error}"),
            }
        }
    }
}

struct FixedSupervisor(ExecutionOutcome);

impl Supervisor for FixedSupervisor {
    fn supervise(
        &self,
        _runtime: Arc<dyn JobRuntime>,
        _container: &ContainerId,
        _attachment: Attachment,
        _controls: &mut dyn ControlSource,
        _streams: JobStreams,
        _limits: SupervisionLimits,
    ) -> SupervisionReport {
        SupervisionReport::complete(self.0.clone())
    }
}

struct EmptyControls;

impl ControlSource for EmptyControls {
    fn receive(&mut self, _timeout: Duration) -> Result<Option<WorkerControl>, WorkerError> {
        Ok(None)
    }
}

#[derive(Default)]
struct RecordingEvents(Vec<WorkerEvent>);

impl EventSink for RecordingEvents {
    fn publish(&mut self, event: &WorkerEvent) -> Result<(), WorkerError> {
        self.0.push(event.clone());
        Ok(())
    }
}

struct StableGrants;

impl GrantSnapshot for StableGrants {
    fn still_matches(&self) -> bool {
        true
    }
}

struct TestGrantVerifier;

impl GrantVerifier for TestGrantVerifier {
    fn capture(&self, _spec: &JobSpec) -> io::Result<Box<dyn GrantSnapshot>> {
        Ok(Box::new(StableGrants))
    }
}

struct ChangedGrants;

impl GrantSnapshot for ChangedGrants {
    fn still_matches(&self) -> bool {
        false
    }
}

struct ChangedGrantVerifier;

impl GrantVerifier for ChangedGrantVerifier {
    fn capture(&self, _spec: &JobSpec) -> io::Result<Box<dyn GrantSnapshot>> {
        Ok(Box::new(ChangedGrants))
    }
}

struct RejectedGrantVerifier;

impl GrantVerifier for RejectedGrantVerifier {
    fn capture(&self, _spec: &JobSpec) -> io::Result<Box<dyn GrantSnapshot>> {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "unsafe grant"))
    }
}

fn worker(runtime: Arc<dyn JobRuntime>, supervisor: Arc<dyn Supervisor>) -> Worker {
    Worker::with_grant_verifier(runtime, supervisor, Arc::new(TestGrantVerifier))
}

pub(super) fn job() -> JobSpec {
    JobSpec {
        image: OciImage::parse(format!("sha256:{}", "b".repeat(64))).unwrap(),
        argv: vec![b"/usr/bin/agent".to_vec()],
        identity: JobIdentity { uid: 501, gid: 20 },
        session_environment: BTreeMap::from([
            ("HOME".into(), "/Users/example".into()),
            ("USER".into(), "example".into()),
            ("LOGNAME".into(), "example".into()),
            ("MARSH_SELECTED_HOME".into(), "/Users/example".into()),
        ]),
        exported_environment: BTreeMap::new(),
        working_directory: "/Users/example/project".into(),
        mounts: vec![JobMount {
            source: "/run/marsh/grants/attempt-1/project".into(),
            target: "/Users/example/project".into(),
            access: MountAccess::ReadWrite,
            subpath: None,
        }],
        resources: JobResources {
            cpu_millis: 1000,
            memory_bytes: 128 * 1024 * 1024,
            pids: 32,
            writable_bytes: 1024 * 1024 * 1024,
            output_bytes: 1024 * 1024,
            wall_seconds: 60,
        },
        terminal: false,
        terminal_size: None,
        split_capability: None,
        capability: None,
    }
}

fn streams() -> JobStreams {
    JobStreams {
        stdout: InterruptibleWriter::sink(),
        stderr: InterruptibleWriter::sink(),
    }
}

fn runtime_exit(code: i32) -> RuntimeExit {
    RuntimeExit {
        code,
        oom_killed: false,
        pids_max_events: Some(0),
        writable_bytes: 0,
        writable_exceeded: false,
    }
}

#[test]
fn runtime_evidence_classifies_only_proven_resource_limits() {
    let mut exit = runtime_exit(137);
    exit.oom_killed = true;
    assert_eq!(
        classify_runtime_exit(exit, 1024),
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Memory
        }
    );

    let mut exit = runtime_exit(1);
    exit.pids_max_events = Some(1);
    assert_eq!(
        classify_runtime_exit(exit, 1024),
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Pids
        }
    );

    let mut exit = runtime_exit(1);
    exit.writable_bytes = 1024;
    assert_eq!(
        classify_runtime_exit(exit, 1024),
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Writable
        }
    );

    // The monitor's kill is the cause, even when SIGKILL also races a
    // pids rejection or the terminal SizeRw sample reads below the limit.
    let mut killed = runtime_exit(137);
    killed.writable_exceeded = true;
    killed.pids_max_events = Some(1);
    assert_eq!(
        classify_runtime_exit(killed, 1024),
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Writable
        }
    );

    assert_eq!(
        classify_runtime_exit(runtime_exit(42), 1024),
        ExecutionOutcome::Exited { code: 42 }
    );
    let mut successful = runtime_exit(0);
    successful.pids_max_events = Some(1);
    successful.writable_bytes = 1024;
    assert_eq!(
        classify_runtime_exit(successful, 1024),
        ExecutionOutcome::Exited { code: 0 }
    );
}

#[test]
fn execution_and_verified_cleanup_are_reported_separately() {
    let runtime = Arc::new(FakeRuntime::default());
    let worker = worker(
        runtime.clone(),
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 23 })),
    );
    let report = worker.run(&job(), &mut EmptyControls, streams());
    assert_eq!(report.execution, ExecutionOutcome::Exited { code: 23 });
    assert_eq!(report.cleanup, CleanupOutcome::Verified);
    assert!(!report.quarantine);
    assert_eq!(
        *runtime.calls.lock().unwrap(),
        ["create", "attach", "start", "delete"]
    );
}

#[test]
fn created_container_identity_is_published_before_terminal_report() {
    let runtime = Arc::new(FakeRuntime::default());
    let worker = worker(
        runtime.clone(),
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 23 })),
    );
    let mut events = RecordingEvents::default();
    let report = worker.run_with_events(&job(), &mut EmptyControls, streams(), &mut events);

    let container_id = ContainerId::parse("a".repeat(64)).unwrap();
    assert_eq!(
        events.0,
        [WorkerEvent::Started {
            container_id: container_id.clone()
        }]
    );
    assert_eq!(report.container_id, Some(container_id));
}

#[test]
fn pre_create_failure_publishes_a_terminal_setup_event() {
    let runtime = Arc::new(FakeRuntime {
        setup_failure: Some(SetupStage::Create),
        ..FakeRuntime::default()
    });
    let worker = worker(
        runtime,
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
    );
    let mut events = RecordingEvents::default();
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let report = worker.run_with_events(
        &job(),
        &mut EmptyControls,
        JobStreams {
            stdout: InterruptibleWriter::sink(),
            stderr: output_stream(stderr.clone()),
        },
        &mut events,
    );

    assert_eq!(
        events.0,
        [WorkerEvent::SetupFailed {
            report: report.clone()
        }]
    );
    assert_eq!(report.container_id, None);
    assert_eq!(
        *stderr.lock().unwrap(),
        b"marsh-worker: create: container runtime create failed (exit code 125): image configuration rejected\n"
    );
}

#[test]
fn setup_diagnostic_cannot_exceed_the_job_output_budget() {
    let runtime = Arc::new(FakeRuntime {
        setup_failure: Some(SetupStage::Create),
        ..FakeRuntime::default()
    });
    let worker = worker(
        runtime,
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
    );
    let mut spec = job();
    spec.resources.output_bytes = 12;
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let report = worker.run(
        &spec,
        &mut EmptyControls,
        JobStreams {
            stdout: InterruptibleWriter::sink(),
            stderr: output_stream(stderr.clone()),
        },
    );
    assert!(matches!(
        report.execution,
        ExecutionOutcome::SetupFailed {
            stage: SetupStage::Create
        }
    ));
    assert_eq!(&*stderr.lock().unwrap(), b"marsh-worker");
}

#[test]
fn delete_uncertainty_quarantines_without_rewriting_execution() {
    let runtime = Arc::new(FakeRuntime {
        delete_fails: true,
        ..FakeRuntime::default()
    });
    let worker = worker(
        runtime,
        Arc::new(FixedSupervisor(ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Wall,
        })),
    );
    let report = worker.run(&job(), &mut EmptyControls, streams());
    assert_eq!(
        report.execution,
        ExecutionOutcome::LimitExceeded {
            resource: ResourceLimit::Wall
        }
    );
    assert_eq!(report.cleanup, CleanupOutcome::Uncertain);
    assert!(report.quarantine);
}

#[test]
fn byte_bridge_pre_effect_rejection_does_not_quarantine_but_unknown_create_does() {
    for (error, expected_cleanup, expected_quarantine) in [
        (
            RuntimeError::ByteBridge("immutable image platform mismatch"),
            CleanupOutcome::NotRequired,
            false,
        ),
        (
            RuntimeError::ByteCreateUncertain {
                attempt: "marsh-bytes-test".into(),
                stderr: "reply lost".into(),
            },
            CleanupOutcome::Uncertain,
            true,
        ),
        (
            RuntimeError::Io(io::Error::other("unknown create")),
            CleanupOutcome::Uncertain,
            true,
        ),
    ] {
        let runtime = Arc::new(FakeRuntime {
            create_error: Mutex::new(Some(error)),
            ..FakeRuntime::default()
        });
        let worker = worker(
            runtime.clone(),
            Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
        );
        let mut events = RecordingEvents::default();
        let report = worker.run_with_events(&job(), &mut EmptyControls, streams(), &mut events);
        assert_eq!(report.container_id, None);
        assert_eq!(report.cleanup, expected_cleanup);
        assert_eq!(report.quarantine, expected_quarantine);
        assert!(matches!(
            report.execution,
            ExecutionOutcome::SetupFailed {
                stage: SetupStage::Create
            }
        ));
        assert_eq!(*runtime.calls.lock().unwrap(), ["create"]);
        assert_eq!(events.0, [WorkerEvent::SetupFailed { report }]);
    }
}

#[test]
fn create_uncertainty_quarantines_without_inventing_a_container_id() {
    let runtime = Arc::new(FakeRuntime {
        setup_failure: Some(SetupStage::Create),
        ..FakeRuntime::default()
    });
    let worker = worker(
        runtime,
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
    );
    let report = worker.run(&job(), &mut EmptyControls, streams());
    assert_eq!(report.container_id, None);
    assert_eq!(
        report.execution,
        ExecutionOutcome::SetupFailed {
            stage: SetupStage::Create
        }
    );
    assert_eq!(report.cleanup, CleanupOutcome::Uncertain);
    assert!(report.quarantine);
}

#[test]
fn start_failure_still_deletes_the_created_container() {
    let runtime = Arc::new(FakeRuntime {
        setup_failure: Some(SetupStage::Start),
        ..FakeRuntime::default()
    });
    let worker = worker(
        runtime.clone(),
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
    );
    let report = worker.run(&job(), &mut EmptyControls, streams());
    assert_eq!(
        report.execution,
        ExecutionOutcome::SetupFailed {
            stage: SetupStage::Start
        }
    );
    assert_eq!(report.cleanup, CleanupOutcome::Verified);
    assert_eq!(runtime.calls.lock().unwrap().last(), Some(&"delete"));
}

#[test]
fn atomic_start_attach_spawn_failure_is_typed_and_never_calls_start() {
    let runtime = Arc::new(FakeRuntime {
        setup_failure: Some(SetupStage::Attach),
        ..FakeRuntime::default()
    });
    let worker = worker(
        runtime.clone(),
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
    );
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let report = worker.run(
        &job(),
        &mut EmptyControls,
        JobStreams {
            stdout: InterruptibleWriter::sink(),
            stderr: output_stream(stderr.clone()),
        },
    );

    assert_eq!(
        report.execution,
        ExecutionOutcome::SetupFailed {
            stage: SetupStage::Attach
        }
    );
    assert_eq!(
        *runtime.calls.lock().unwrap(),
        ["create", "attach", "delete"]
    );
    assert_eq!(
        *stderr.lock().unwrap(),
        b"marsh-worker: attach: runtime I/O failed: docker executable unavailable\n"
    );
}

#[test]
fn unsafe_grant_fails_before_container_creation() {
    let runtime = Arc::new(FakeRuntime::default());
    let worker = Worker::with_grant_verifier(
        runtime.clone(),
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
        Arc::new(RejectedGrantVerifier),
    );
    let report = worker.run(&job(), &mut EmptyControls, streams());
    assert_eq!(
        report.execution,
        ExecutionOutcome::SetupFailed {
            stage: SetupStage::Grant
        }
    );
    assert_eq!(report.cleanup, CleanupOutcome::NotRequired);
    assert!(!report.quarantine);
    assert!(runtime.calls.lock().unwrap().is_empty());
}

#[test]
fn grant_identity_change_after_create_deletes_exact_container() {
    let runtime = Arc::new(FakeRuntime::default());
    let worker = Worker::with_grant_verifier(
        runtime.clone(),
        Arc::new(FixedSupervisor(ExecutionOutcome::Exited { code: 0 })),
        Arc::new(ChangedGrantVerifier),
    );
    let report = worker.run(&job(), &mut EmptyControls, streams());
    assert_eq!(
        report.execution,
        ExecutionOutcome::SetupFailed {
            stage: SetupStage::Grant
        }
    );
    assert_eq!(report.cleanup, CleanupOutcome::Verified);
    assert!(!report.quarantine);
    assert_eq!(*runtime.calls.lock().unwrap(), ["create", "delete"]);
}

#[test]
fn framed_spec_is_bounded_and_revalidated_after_deserialization() {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &job()).unwrap();
    let decoded: JobSpec = read_frame(&mut Cursor::new(bytes)).unwrap();
    decoded.validate().unwrap();

    let malicious = r#"{"image":"agent:latest","argv":[[47,98,105,110,47,116,114,117,101]],"identity":{"uid":501,"gid":20},"session_environment":{"HOME":"/Users/example","USER":"example","LOGNAME":"example"},"working_directory":"/workspace","mounts":[{"source":"/run/marsh/grants/attempt-1/project","target":"/workspace","access":"read_write"}],"resources":{"cpu_millis":1,"memory_bytes":1,"pids":1,"writable_bytes":1,"output_bytes":1,"wall_seconds":1},"terminal":false}"#.to_string();
    let mut frame = (u32::try_from(malicious.len()).unwrap())
        .to_be_bytes()
        .to_vec();
    frame.extend(malicious.bytes());
    assert!(read_frame::<JobSpec>(&mut Cursor::new(frame)).is_err());

    let oversized = (u32::try_from(MAX_CONTROL_FRAME + 1).unwrap())
        .to_be_bytes()
        .to_vec();
    assert!(matches!(
        read_frame::<JobSpec>(&mut Cursor::new(oversized)),
        Err(WorkerError::FrameTooLarge)
    ));
}

#[test]
fn channel_eof_is_controller_loss_even_after_explicit_input_close() {
    let (send, receive) = mpsc::channel();
    send.send(Ok(WorkerControl::CloseInput)).unwrap();
    drop(send);
    let mut controls = ChannelControlSource::new(receive);
    assert_eq!(
        controls.receive(Duration::ZERO).unwrap(),
        Some(WorkerControl::CloseInput)
    );
    assert!(matches!(
        controls.receive(Duration::ZERO),
        Err(WorkerError::ControlClosed)
    ));

    let (send, receive) = mpsc::channel();
    drop(send);
    let mut controls = ChannelControlSource::new(receive);
    assert!(matches!(
        controls.receive(Duration::ZERO),
        Err(WorkerError::ControlClosed)
    ));
}

#[derive(Default)]
struct LifecycleState {
    signals: Vec<JobSignal>,
    resizes: Vec<TerminalSize>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux-only test
    started: bool,
    complete: bool,
    deleted: bool,
}

#[derive(Clone, Copy, Default)]
enum ControlBehavior {
    #[default]
    Normal,
    CompleteOnResize,
    SignalAfterExit,
    ResizeAfterExit,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // Linux-only test
    ResizeFails,
}

struct RecordingRuntime {
    state: Mutex<LifecycleState>,
    changed: Condvar,
    control_behavior: ControlBehavior,
    wait_fails: bool,
    exit_code: i32,
}

impl Default for RecordingRuntime {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            changed: Condvar::default(),
            control_behavior: ControlBehavior::Normal,
            wait_fails: false,
            exit_code: 7,
        }
    }
}

impl JobRuntime for RecordingRuntime {
    fn create_cancellable(
        &self,
        spec: &JobSpec,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<ContainerId, RuntimeError> {
        cancel.check()?;
        self.create(spec)
    }
    fn delete_cancellable(
        &self,
        container: &ContainerId,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.delete(container)
    }
    fn signal_cancellable(
        &self,
        container: &ContainerId,
        signal: JobSignal,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.signal(container, signal)
    }
    fn resize_cancellable(
        &self,
        container: &ContainerId,
        size: TerminalSize,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.resize(container, size)
    }
    fn create(&self, _spec: &JobSpec) -> Result<ContainerId, RuntimeError> {
        Ok(ContainerId::parse("a".repeat(64)).unwrap())
    }

    fn attach(
        &self,
        _container: &ContainerId,
        _terminal_size: Option<TerminalSize>,
    ) -> Result<Attachment, RuntimeError> {
        Ok(FakeRuntime::attachment())
    }

    fn start(&self, _container: &ContainerId) -> Result<(), RuntimeError> {
        self.state.lock().unwrap().started = true;
        Ok(())
    }

    fn wait(&self, container: &ContainerId) -> Result<RuntimeExit, RuntimeError> {
        self.wait_cancellable(container, &marsh_runtime::Cancellation::default())
    }
    fn wait_cancellable(
        &self,
        _container: &ContainerId,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<RuntimeExit, RuntimeError> {
        let mut state = self.state.lock().unwrap();
        while !state.complete {
            cancel.check()?;
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        if self.wait_fails {
            Err(RuntimeError::CommandFailed {
                operation: "inspect terminal resource state",
                exit_code: Some(125),
                stderr: format!("invalid\0{}", "x".repeat(5000)),
            })
        } else {
            Ok(runtime_exit(self.exit_code))
        }
    }

    fn signal(&self, _container: &ContainerId, signal: JobSignal) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap();
        state.signals.push(signal);
        if signal == JobSignal::Kill
            || matches!(self.control_behavior, ControlBehavior::SignalAfterExit)
        {
            state.complete = true;
            self.changed.notify_all();
        }
        if matches!(self.control_behavior, ControlBehavior::SignalAfterExit) {
            return Err(RuntimeError::CommandFailed {
                operation: "signal exited container",
                exit_code: Some(1),
                stderr: "container is not running".into(),
            });
        }
        Ok(())
    }

    fn resize(&self, _container: &ContainerId, size: TerminalSize) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap();
        state.resizes.push(size);
        if matches!(
            self.control_behavior,
            ControlBehavior::CompleteOnResize | ControlBehavior::ResizeAfterExit
        ) {
            state.complete = true;
            self.changed.notify_all();
        }
        if matches!(
            self.control_behavior,
            ControlBehavior::ResizeAfterExit | ControlBehavior::ResizeFails
        ) {
            return Err(RuntimeError::EngineResizeFailed {
                status: 500,
                body: "resize rejected".into(),
            });
        }
        Ok(())
    }

    fn delete(&self, _container: &ContainerId) -> Result<(), RuntimeError> {
        self.state.lock().unwrap().deleted = true;
        Ok(())
    }
}

#[test]
fn delivered_interrupt_preserves_runtime_reported_exit() {
    for exit_code in [0, 130] {
        let runtime = Arc::new(RecordingRuntime {
            control_behavior: ControlBehavior::CompleteOnResize,
            exit_code,
            ..RecordingRuntime::default()
        });
        let mut controls = ScriptedControls {
            controls: VecDeque::from([
                Ok(Some(WorkerControl::Signal {
                    signal: JobSignal::Interrupt,
                })),
                Ok(Some(WorkerControl::Resize {
                    size: TerminalSize {
                        rows: 24,
                        columns: 80,
                    },
                })),
            ]),
        };
        let outcome = ThreadSupervisor::default().supervise(
            runtime,
            &ContainerId::parse("a".repeat(64)).unwrap(),
            recording_attachment(
                Arc::new(Mutex::new(Vec::new())),
                Arc::new(AtomicBool::new(false)),
            ),
            &mut controls,
            streams(),
            SupervisionLimits {
                wall_time: Duration::from_secs(5),
                output_bytes: 1024,
                writable_bytes: 1024,
            },
        );
        assert_eq!(
            outcome.execution,
            ExecutionOutcome::Exited { code: exit_code }
        );
    }
}

#[test]
fn late_signal_failure_prefers_runtime_exit_evidence() {
    let runtime = Arc::new(RecordingRuntime {
        control_behavior: ControlBehavior::SignalAfterExit,
        exit_code: 23,
        ..RecordingRuntime::default()
    });
    let mut controls = ScriptedControls {
        controls: VecDeque::from([Ok(Some(WorkerControl::Signal {
            signal: JobSignal::Interrupt,
        }))]),
    };

    let outcome = ThreadSupervisor::default().supervise(
        runtime,
        &ContainerId::parse("b".repeat(64)).unwrap(),
        recording_attachment(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
        ),
        &mut controls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 23 });
}

#[test]
fn late_resize_failure_prefers_runtime_exit_evidence() {
    let runtime = Arc::new(RecordingRuntime {
        control_behavior: ControlBehavior::ResizeAfterExit,
        exit_code: 24,
        ..RecordingRuntime::default()
    });
    let mut controls = ScriptedControls {
        controls: VecDeque::from([Ok(Some(WorkerControl::Resize {
            size: TerminalSize {
                rows: 24,
                columns: 80,
            },
        }))]),
    };

    let outcome = ThreadSupervisor::default().supervise(
        runtime,
        &ContainerId::parse("c".repeat(64)).unwrap(),
        recording_attachment(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
        ),
        &mut controls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 24 });
}

struct RecordingWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
    dropped: Option<Arc<AtomicBool>>,
}

struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("injected output failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("injected output failure"))
    }
}

impl Write for RecordingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for RecordingWriter {
    fn drop(&mut self) {
        if let Some(dropped) = &self.dropped {
            dropped.store(true, Ordering::SeqCst);
        }
    }
}

struct ScriptedControls {
    controls: VecDeque<Result<Option<WorkerControl>, WorkerError>>,
}

struct RuntimeBlockingWriter {
    runtime: Arc<RecordingRuntime>,
    cancel: marsh_runtime::Cancellation,
}

struct CancellableMemoryProcess(marsh_runtime::Cancellation);
impl AttachedProcess for CancellableMemoryProcess {
    fn supports_io_cancellation(&self) -> bool {
        true
    }
    fn cancel_io(&self) -> io::Result<()> {
        self.0.cancel();
        Ok(())
    }
    fn wait(&mut self) -> io::Result<i32> {
        Ok(0)
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(Some(0))
    }
    fn terminate(&mut self) -> io::Result<()> {
        self.0.cancel();
        Ok(())
    }
}

struct EarlyCloseWriter {
    runtime: Arc<RecordingRuntime>,
}

struct DelayedEarlyCloseWriter {
    runtime: Arc<RecordingRuntime>,
    delay: Duration,
}

impl Write for DelayedEarlyCloseWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        let runtime = Arc::clone(&self.runtime);
        let delay = self.delay;
        thread::spawn(move || {
            thread::sleep(delay);
            let mut state = runtime.state.lock().unwrap();
            state.complete = true;
            runtime.changed.notify_all();
        });
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "command closed stdin before its later successful exit",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for EarlyCloseWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.runtime.state.lock().unwrap();
        state.complete = true;
        self.runtime.changed.notify_all();
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "command exited and closed stdin",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Write for RuntimeBlockingWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        let mut state = self.runtime.state.lock().unwrap();
        while !state.signals.contains(&JobSignal::Kill) {
            self.cancel.check()?;
            state = self
                .runtime
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "container stdin closed after cancellation",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ControlSource for ScriptedControls {
    fn receive(&mut self, _timeout: Duration) -> Result<Option<WorkerControl>, WorkerError> {
        self.controls.pop_front().unwrap_or(Ok(None))
    }
}

/// A controller that is lost only after the runtime has started the job, so
/// the loss reaches supervision rather than cancelling setup.
#[cfg(target_os = "linux")]
struct LostAfterStart(Arc<RecordingRuntime>);

#[cfg(target_os = "linux")]
impl ControlSource for LostAfterStart {
    fn receive(&mut self, timeout: Duration) -> Result<Option<WorkerControl>, WorkerError> {
        if self.0.state.lock().unwrap().started {
            return Err(WorkerError::ControlClosed);
        }
        thread::sleep(timeout.min(Duration::from_millis(10)));
        Ok(None)
    }
}

fn recording_attachment(
    stdin_bytes: Arc<Mutex<Vec<u8>>>,
    stdin_dropped: Arc<AtomicBool>,
) -> Attachment {
    Attachment {
        stdin: Box::new(RecordingWriter {
            bytes: stdin_bytes,
            dropped: Some(stdin_dropped),
        }),
        stdout: Box::new(Cursor::new(b"raw stdout\n".to_vec())),
        stderr: Box::new(Cursor::new(b"raw stderr\n".to_vec())),
        process: Box::new(FakeProcess),
        control: no_attachment_control(),
    }
}

fn output_stream(bytes: Arc<Mutex<Vec<u8>>>) -> InterruptibleWriter {
    InterruptibleWriter::new(
        Box::new(RecordingWriter {
            bytes,
            dropped: None,
        }),
        marsh_runtime::Cancellation::default(),
    )
}

fn socket_output(socket: UnixStream) -> InterruptibleWriter {
    InterruptibleWriter::from_file(
        std::fs::File::from(std::os::fd::OwnedFd::from(socket)),
        marsh_runtime::Cancellation::default(),
    )
    .unwrap()
}

#[test]
fn supervisor_forwards_controls_closes_stdin_and_copies_raw_streams() {
    let runtime = Arc::new(RecordingRuntime {
        control_behavior: ControlBehavior::CompleteOnResize,
        ..RecordingRuntime::default()
    });
    let stdin_bytes = Arc::new(Mutex::new(Vec::new()));
    let stdin_dropped = Arc::new(AtomicBool::new(false));
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let size = TerminalSize {
        rows: 24,
        columns: 80,
    };
    let mut controls = ScriptedControls {
        controls: VecDeque::from([
            Ok(Some(WorkerControl::Input {
                bytes: b"job input".to_vec(),
            })),
            Ok(Some(WorkerControl::CloseInput)),
            Ok(Some(WorkerControl::Signal {
                signal: JobSignal::Interrupt,
            })),
            Ok(Some(WorkerControl::Resize { size })),
        ]),
    };
    let container = ContainerId::parse("c".repeat(64)).unwrap();
    let outcome = ThreadSupervisor::default().supervise(
        runtime.clone(),
        &container,
        recording_attachment(stdin_bytes.clone(), stdin_dropped.clone()),
        &mut controls,
        JobStreams {
            stdout: output_stream(stdout.clone()),
            stderr: output_stream(stderr.clone()),
        },
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 7 });
    assert_eq!(*stdin_bytes.lock().unwrap(), b"job input");
    assert!(stdin_dropped.load(Ordering::SeqCst));
    assert_eq!(*stdout.lock().unwrap(), b"raw stdout\n");
    assert_eq!(*stderr.lock().unwrap(), b"raw stderr\n");
    let state = runtime.state.lock().unwrap();
    assert_eq!(state.signals, [JobSignal::Interrupt]);
    assert_eq!(state.resizes, [size]);
}

#[test]
fn blocked_container_stdin_cannot_prevent_explicit_cancellation() {
    let runtime = Arc::new(RecordingRuntime::default());
    let mut controls = ScriptedControls {
        controls: (0..INPUT_BURST_FRAMES)
            .map(|_| {
                Ok(Some(WorkerControl::Input {
                    bytes: vec![b'x'; MAX_STREAM_CHUNK],
                }))
            })
            .chain([Ok(Some(WorkerControl::Signal {
                signal: JobSignal::Kill,
            }))])
            .collect(),
    };
    let cancel = marsh_runtime::Cancellation::default();
    let attachment = Attachment {
        stdin: Box::new(RuntimeBlockingWriter {
            runtime: Arc::clone(&runtime),
            cancel: cancel.clone(),
        }),
        stdout: Box::new(Cursor::new(Vec::new())),
        stderr: Box::new(Cursor::new(Vec::new())),
        process: Box::new(CancellableMemoryProcess(cancel)),
        control: no_attachment_control(),
    };
    let outcome = ThreadSupervisor::default().supervise(
        runtime.clone(),
        &ContainerId::parse("c".repeat(64)).unwrap(),
        attachment,
        &mut controls,
        streams(),
        // The wall limit only has to be out of reach: if blocked stdin kept the
        // Kill from being acted on, the run would end at the wall limit and
        // report that, not `Cancelled`. That outcome is the proof, so there is
        // no stopwatch to lose to a slow machine.
        SupervisionLimits {
            wall_time: Duration::from_mins(1),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 7 });
    assert_eq!(outcome.delivery, DeliveryOutcome::Cancelled);
    assert!(
        runtime
            .state
            .lock()
            .unwrap()
            .signals
            .contains(&JobSignal::Kill)
    );
}

#[test]
fn early_successful_exit_outranks_broken_pipe_from_large_queued_input() {
    let runtime = Arc::new(RecordingRuntime {
        exit_code: 0,
        ..RecordingRuntime::default()
    });
    let mut controls = ScriptedControls {
        controls: (0..INPUT_BURST_FRAMES)
            .map(|sequence| {
                Ok(Some(WorkerControl::Input {
                    bytes: vec![u8::try_from(sequence).unwrap(); MAX_STREAM_CHUNK],
                }))
            })
            .collect(),
    };
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let attachment = Attachment {
        stdin: Box::new(EarlyCloseWriter {
            runtime: Arc::clone(&runtime),
        }),
        stdout: Box::new(Cursor::new(b"head output\n".to_vec())),
        stderr: Box::new(Cursor::new(Vec::new())),
        process: Box::new(FakeProcess),
        control: no_attachment_control(),
    };

    let outcome = ThreadSupervisor::default().supervise(
        runtime,
        &ContainerId::parse("c".repeat(64)).unwrap(),
        attachment,
        &mut controls,
        JobStreams {
            stdout: output_stream(Arc::clone(&stdout)),
            stderr: InterruptibleWriter::sink(),
        },
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 0 });
    assert_eq!(*stdout.lock().unwrap(), b"head output\n");
}

#[test]
fn intentional_stdin_close_can_precede_successful_exit_by_more_than_cleanup_grace() {
    let runtime = Arc::new(RecordingRuntime {
        exit_code: 0,
        ..RecordingRuntime::default()
    });
    let mailbox = Arc::new(AttemptMailbox::default());
    for sequence in 0..1023 {
        mailbox.push(WorkerControl::Input {
            bytes: vec![u8::try_from(sequence % 251).unwrap(); MAX_STREAM_CHUNK],
        });
    }
    for _ in 0..3000 {
        mailbox.push(WorkerControl::Input { bytes: Vec::new() });
    }
    let later_mailbox = Arc::clone(&mailbox);
    let later_controls = thread::spawn(move || {
        let mut state = later_mailbox.state.lock().unwrap();
        while !state.input_discarded {
            state = later_mailbox.ready.wait(state).unwrap();
        }
        drop(state);
        later_mailbox.push(WorkerControl::Input {
            bytes: vec![b'x'; MAX_STREAM_CHUNK],
        });
        later_mailbox.push(WorkerControl::Signal {
            signal: JobSignal::Interrupt,
        });
        later_mailbox.push(WorkerControl::Resize {
            size: TerminalSize {
                rows: 33,
                columns: 101,
            },
        });
    });
    let mut controls = MailboxControlSource::new(Arc::clone(&mailbox));
    let attachment = Attachment {
        stdin: Box::new(DelayedEarlyCloseWriter {
            runtime: Arc::clone(&runtime),
            delay: Duration::from_millis(2_100),
        }),
        stdout: Box::new(Cursor::new(b"later output\n".to_vec())),
        stderr: Box::new(Cursor::new(Vec::new())),
        process: Box::new(FakeProcess),
        control: no_attachment_control(),
    };
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let started = Instant::now();
    let outcome = ThreadSupervisor::default().supervise(
        runtime.clone(),
        &ContainerId::parse("c".repeat(64)).unwrap(),
        attachment,
        &mut controls,
        JobStreams {
            stdout: output_stream(Arc::clone(&stdout)),
            stderr: InterruptibleWriter::sink(),
        },
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    later_controls.join().unwrap();
    assert!(started.elapsed() >= Duration::from_millis(2_100));
    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 0 });
    assert_eq!(*stdout.lock().unwrap(), b"later output\n");
    let state = runtime.state.lock().unwrap();
    assert!(state.signals.contains(&JobSignal::Interrupt));
    assert!(state.resizes.contains(&TerminalSize {
        rows: 33,
        columns: 101,
    }));
    let mailbox_state = mailbox.state.lock().unwrap();
    assert_eq!(mailbox_state.reserved_input_bytes, 0);
    assert_eq!(mailbox_state.reserved_messages, 0);
}

#[test]
// Guest-only (the worker ships as a Linux binary); macOS scheduling
// cancels setup / misses the resize before attach. Unverified off Linux.
#[cfg(target_os = "linux")]
fn resize_failure_is_a_nonfatal_control_error_on_a_bounded_job() {
    let runtime = Arc::new(RecordingRuntime {
        control_behavior: ControlBehavior::ResizeFails,
        ..RecordingRuntime::default()
    });
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let mut controls = ScriptedControls {
        controls: VecDeque::from([Ok(Some(WorkerControl::Resize {
            size: TerminalSize {
                rows: 40,
                columns: 120,
            },
        }))]),
    };
    let outcome = ThreadSupervisor::default().supervise(
        runtime.clone(),
        &ContainerId::parse("d".repeat(64)).unwrap(),
        recording_attachment(
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
        ),
        &mut controls,
        JobStreams {
            stdout: InterruptibleWriter::sink(),
            stderr: output_stream(stderr.clone()),
        },
        SupervisionLimits {
            wall_time: Duration::from_millis(500),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    // A rejected resize is counted, never a reason to end the job: it runs
    // to its own bound.
    assert_eq!(outcome.control_errors, 1);
    assert_eq!(
        outcome.delivery,
        DeliveryOutcome::LimitExceeded {
            resource: ResourceLimit::Wall
        }
    );
    assert_eq!(
        runtime.state.lock().unwrap().resizes,
        [TerminalSize {
            rows: 40,
            columns: 120,
        }]
    );
}

#[test]
fn attached_start_cli_failure_cannot_be_reported_as_job_success() {
    let runtime = Arc::new(RecordingRuntime::default());
    runtime.state.lock().unwrap().complete = true;
    let container = ContainerId::parse("e".repeat(64)).unwrap();
    let attachment = Attachment {
        stdin: Box::new(io::sink()),
        stdout: Box::new(Cursor::new(Vec::<u8>::new())),
        stderr: Box::new(Cursor::new(b"docker start failed\n".to_vec())),
        process: Box::new(FailedAttachProcess),
        control: no_attachment_control(),
    };

    let outcome = ThreadSupervisor::default().supervise(
        runtime,
        &container,
        attachment,
        &mut EmptyControls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.delivery, DeliveryOutcome::Failed);
    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 7 });
}

#[test]
fn terminal_evidence_failure_is_reported_without_blocking_job_stderr() {
    let runtime = Arc::new(RecordingRuntime {
        wait_fails: true,
        ..RecordingRuntime::default()
    });
    runtime.state.lock().unwrap().complete = true;
    let stderr = Arc::new(Mutex::new(Vec::new()));

    let outcome = ThreadSupervisor::default().supervise(
        runtime,
        &ContainerId::parse("8".repeat(64)).unwrap(),
        FakeRuntime::attachment(),
        &mut EmptyControls,
        JobStreams {
            stdout: InterruptibleWriter::sink(),
            stderr: output_stream(Arc::clone(&stderr)),
        },
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 128,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.execution, ExecutionOutcome::SupervisionFailed);
    assert_eq!(outcome.delivery, DeliveryOutcome::Failed);
    assert!(stderr.lock().unwrap().is_empty());
}

#[test]
fn exact_wait_winning_does_not_kill_attach_before_output_drains() {
    let runtime = Arc::new(RecordingRuntime::default());
    runtime.state.lock().unwrap().complete = true;
    let ready_at = Instant::now() + Duration::from_millis(40);
    let terminated = Arc::new(AtomicBool::new(false));
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let attachment = Attachment {
        stdin: Box::new(io::sink()),
        stdout: Box::new(DelayedReader {
            ready_at,
            bytes: Cursor::new(b"fast output\n".to_vec()),
        }),
        stderr: Box::new(Cursor::new(Vec::<u8>::new())),
        process: Box::new(DelayedCleanProcess {
            ready_at,
            terminated: Arc::clone(&terminated),
        }),
        control: no_attachment_control(),
    };

    let outcome = ThreadSupervisor {
        termination_grace: Duration::from_millis(200),
    }
    .supervise(
        runtime,
        &ContainerId::parse("9".repeat(64)).unwrap(),
        attachment,
        &mut EmptyControls,
        JobStreams {
            stdout: output_stream(Arc::clone(&stdout)),
            stderr: InterruptibleWriter::sink(),
        },
        SupervisionLimits {
            wall_time: Duration::from_secs(1),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 7 });
    assert_eq!(*stdout.lock().unwrap(), b"fast output\n");
    assert!(!terminated.load(Ordering::SeqCst));
}

#[test]
fn supervisor_timeout_escalates_from_term_to_kill() {
    let runtime = Arc::new(RecordingRuntime::default());
    let mut controls = EmptyControls;
    let container = ContainerId::parse("d".repeat(64)).unwrap();
    let outcome = ThreadSupervisor {
        termination_grace: Duration::from_millis(100),
    }
    .supervise(
        runtime.clone(),
        &container,
        FakeRuntime::attachment(),
        &mut controls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::ZERO,
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(
        outcome.delivery,
        DeliveryOutcome::LimitExceeded {
            resource: ResourceLimit::Wall
        }
    );
    assert_eq!(
        runtime.state.lock().unwrap().signals,
        [JobSignal::Terminate, JobSignal::Kill]
    );
}

#[test]
fn unrepresentable_deadline_fails_closed_without_panicking() {
    let runtime = Arc::new(RecordingRuntime::default());
    let mut controls = EmptyControls;
    let container = ContainerId::parse("4".repeat(64)).unwrap();
    let outcome = ThreadSupervisor::default().supervise(
        runtime.clone(),
        &container,
        FakeRuntime::attachment(),
        &mut controls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::from_secs(u64::MAX),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(
        outcome.delivery,
        DeliveryOutcome::LimitExceeded {
            resource: ResourceLimit::Wall
        }
    );
    assert_eq!(
        runtime.state.lock().unwrap().signals,
        [JobSignal::Terminate, JobSignal::Kill]
    );
}

#[test]
fn supervisor_enforces_one_combined_stdout_stderr_budget() {
    let runtime = Arc::new(RecordingRuntime::default());
    let mut controls = EmptyControls;
    let container = ContainerId::parse("f".repeat(64)).unwrap();
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let attachment = Attachment {
        stdin: Box::new(io::sink()),
        stdout: Box::new(Cursor::new(b"stdout-bytes".to_vec())),
        stderr: Box::new(Cursor::new(b"stderr-bytes".to_vec())),
        process: Box::new(FakeProcess),
        control: no_attachment_control(),
    };

    let outcome = ThreadSupervisor {
        termination_grace: Duration::from_millis(100),
    }
    .supervise(
        runtime.clone(),
        &container,
        attachment,
        &mut controls,
        JobStreams {
            stdout: output_stream(stdout.clone()),
            stderr: output_stream(stderr.clone()),
        },
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 10,
            writable_bytes: 1024,
        },
    );

    assert_eq!(
        outcome.delivery,
        DeliveryOutcome::LimitExceeded {
            resource: ResourceLimit::Output
        }
    );
    // Crossing the budget cancels the writers at once (a consumer that has
    // stopped reading cannot hold the limit hostage), so how much of the last
    // chunk lands before the cancel is a race: all of it, or none. What holds
    // is that the two streams never deliver more than one budget between them
    // (`output_pumps_share_one_exact_budget` pins the exact split without a
    // supervisor to cancel anything).
    assert!(stdout.lock().unwrap().len() + stderr.lock().unwrap().len() <= 10);
    assert!(b"stdout-bytes".starts_with(&stdout.lock().unwrap()));
    assert!(b"stderr-bytes".starts_with(&stderr.lock().unwrap()));
    assert_eq!(runtime.state.lock().unwrap().signals, [JobSignal::Kill]);
}

#[test]
fn output_pumps_share_one_exact_budget() {
    let used = Arc::new(AtomicU64::new(0));
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let (events, received) = mpsc::channel();
    let pumps = [(b"stdout-bytes", &stdout), (b"stderr-bytes", &stderr)].map(|(bytes, sink)| {
        drain::spawn_copy(
            Box::new(Cursor::new(bytes.to_vec())),
            output_stream(sink.clone()),
            used.clone(),
            10,
            events.clone(),
        )
    });
    for pump in pumps {
        pump.join().unwrap();
    }
    drop(events);

    // Nothing cancels a writer here, so the budget is delivered exactly:
    // whichever stream reserved first got its 10 bytes, the other none.
    let (stdout, stderr) = (stdout.lock().unwrap(), stderr.lock().unwrap());
    assert_eq!(stdout.len() + stderr.len(), 10);
    assert!(b"stdout-bytes".starts_with(&stdout));
    assert!(b"stderr-bytes".starts_with(&stderr));
    let events: Vec<_> = received.iter().collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, drain::PumpEvent::Limit))
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, drain::PumpEvent::Output(true)))
            .count(),
        2
    );
}

#[test]
fn output_limit_remains_typed_when_process_exit_wins_the_race() {
    let runtime = Arc::new(RecordingRuntime::default());
    runtime.state.lock().unwrap().complete = true;
    let mut controls = EmptyControls;
    let container = ContainerId::parse("1".repeat(64)).unwrap();
    let attachment = Attachment {
        stdin: Box::new(io::sink()),
        stdout: Box::new(Cursor::new(vec![b'x'; 1024])),
        stderr: Box::new(Cursor::new(Vec::<u8>::new())),
        process: Box::new(FakeProcess),
        control: no_attachment_control(),
    };

    let outcome = ThreadSupervisor::default().supervise(
        runtime,
        &container,
        attachment,
        &mut controls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 10,
            writable_bytes: 1024,
        },
    );

    assert_eq!(
        outcome.delivery,
        DeliveryOutcome::LimitExceeded {
            resource: ResourceLimit::Output
        }
    );
    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 7 });
}

#[test]
fn output_limit_remains_typed_when_runtime_wait_fails() {
    let runtime = Arc::new(RecordingRuntime {
        wait_fails: true,
        ..RecordingRuntime::default()
    });
    let mut controls = EmptyControls;
    let container = ContainerId::parse("2".repeat(64)).unwrap();
    let attachment = Attachment {
        stdin: Box::new(io::sink()),
        stdout: Box::new(Cursor::new(vec![b'x'; 1024])),
        stderr: Box::new(Cursor::new(Vec::<u8>::new())),
        process: Box::new(FakeProcess),
        control: no_attachment_control(),
    };

    let outcome = ThreadSupervisor::default().supervise(
        runtime,
        &container,
        attachment,
        &mut controls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 10,
            writable_bytes: 1024,
        },
    );

    assert_eq!(
        outcome.delivery,
        DeliveryOutcome::LimitExceeded {
            resource: ResourceLimit::Output
        }
    );
}

#[test]
fn output_sink_failure_stops_the_container_immediately() {
    let runtime = Arc::new(RecordingRuntime::default());
    let mut controls = EmptyControls;
    let container = ContainerId::parse("3".repeat(64)).unwrap();
    let attachment = Attachment {
        stdin: Box::new(io::sink()),
        stdout: Box::new(Cursor::new(b"cannot deliver".to_vec())),
        stderr: Box::new(Cursor::new(Vec::<u8>::new())),
        process: Box::new(FakeProcess),
        control: no_attachment_control(),
    };

    let outcome = ThreadSupervisor {
        termination_grace: Duration::from_millis(100),
    }
    .supervise(
        runtime.clone(),
        &container,
        attachment,
        &mut controls,
        JobStreams {
            stdout: InterruptibleWriter::new(
                Box::new(FailingWriter),
                marsh_runtime::Cancellation::default(),
            ),
            stderr: InterruptibleWriter::sink(),
        },
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.delivery, DeliveryOutcome::Failed);
    assert_eq!(
        runtime.state.lock().unwrap().signals,
        [JobSignal::Terminate, JobSignal::Kill]
    );
}

#[test]
fn supervisor_cancels_container_when_controller_is_lost() {
    let runtime = Arc::new(RecordingRuntime::default());
    let mut controls = ScriptedControls {
        controls: VecDeque::from([Err(WorkerError::ControlClosed)]),
    };
    let container = ContainerId::parse("e".repeat(64)).unwrap();
    let grace = Duration::from_millis(40);
    let started = Instant::now();
    let outcome = ThreadSupervisor {
        termination_grace: grace,
    }
    .supervise(
        runtime.clone(),
        &container,
        FakeRuntime::attachment(),
        &mut controls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.delivery, DeliveryOutcome::Failed);
    assert_eq!(outcome.execution, ExecutionOutcome::Exited { code: 7 });
    assert!(started.elapsed() >= grace);
    assert_eq!(
        runtime.state.lock().unwrap().signals,
        [JobSignal::Terminate, JobSignal::Kill]
    );
}

#[test]
fn controller_loss_does_not_repeat_an_explicit_terminate() {
    let runtime = Arc::new(RecordingRuntime::default());
    let mut controls = ScriptedControls {
        controls: VecDeque::from([
            Ok(Some(WorkerControl::Signal {
                signal: JobSignal::Terminate,
            })),
            Err(WorkerError::ControlClosed),
        ]),
    };
    let container = ContainerId::parse("f".repeat(64)).unwrap();

    let outcome = ThreadSupervisor {
        termination_grace: Duration::from_millis(20),
    }
    .supervise(
        runtime.clone(),
        &container,
        FakeRuntime::attachment(),
        &mut controls,
        streams(),
        SupervisionLimits {
            wall_time: Duration::from_secs(5),
            output_bytes: 1024,
            writable_bytes: 1024,
        },
    );

    assert_eq!(outcome.delivery, DeliveryOutcome::Cancelled);
    assert_eq!(
        runtime.state.lock().unwrap().signals,
        [JobSignal::Terminate, JobSignal::Kill]
    );
}

#[test]
// Guest-only (the worker ships as a Linux binary); macOS scheduling
// cancels setup / misses the resize before attach. Unverified off Linux.
#[cfg(target_os = "linux")]
fn controller_loss_terminates_kills_and_verifies_container_cleanup() {
    let runtime = Arc::new(RecordingRuntime::default());
    let worker = worker(
        runtime.clone(),
        Arc::new(ThreadSupervisor {
            termination_grace: Duration::from_millis(20),
        }),
    );
    let mut controls = LostAfterStart(runtime.clone());

    let report = worker.run(&job(), &mut controls, streams());

    assert_eq!(report.execution, ExecutionOutcome::Exited { code: 7 });
    assert_eq!(report.delivery, DeliveryOutcome::Failed);
    assert_eq!(report.cleanup, CleanupOutcome::Verified);
    assert!(!report.quarantine);
    let state = runtime.state.lock().unwrap();
    assert_eq!(state.signals, [JobSignal::Terminate, JobSignal::Kill]);
    assert!(state.deleted);
}
