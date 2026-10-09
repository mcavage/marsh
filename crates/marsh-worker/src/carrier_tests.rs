//! Real exited producer + owned carrier + socket caller. No Docker effects.
use super::*;
use marsh_runtime::{
    AttachedProcess, Cancellation, CommandRunner, Invocation, SystemCommandRunner,
};
use std::os::{
    fd::OwnedFd,
    unix::{fs::DirBuilderExt, net::UnixStream},
};
use std::sync::atomic::{AtomicBool, AtomicUsize};

/// Deadline for a condition that holds within milliseconds on an idle machine
/// (a fixture process starting, both pipes filling). It is large so a starved
/// CI runner is never mistaken for a hang, and still ends a real hang.
const CONDITION_DEADLINE: Duration = Duration::from_secs(30);

struct OwnedDirectory(PathBuf);
impl OwnedDirectory {
    fn new() -> Self {
        // macOS clocks are microsecond-grained: parallel tests need a counter.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "marsh-carrier-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
}
impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ObservedRuntime {
    exit: RuntimeExit,
    attachment: Mutex<Option<Attachment>>,
    wait_done: Arc<AtomicUsize>,
    wait_delay: Duration,
    reaped: Arc<AtomicUsize>,
    threads: ThreadProof,
    start_fails: AtomicBool,
}
impl Drop for ObservedRuntime {
    fn drop(&mut self) {
        if let Some(mut attachment) = self.attachment.get_mut().unwrap().take() {
            let _ = attachment.process.cancel_io();
            let _ = attachment.process.terminate();
            let _ = attachment.process.wait();
        }
    }
}
impl JobRuntime for ObservedRuntime {
    fn create_cancellable(
        &self,
        spec: &JobSpec,
        cancel: &Cancellation,
    ) -> Result<ContainerId, RuntimeError> {
        cancel.check()?;
        self.create(spec)
    }
    fn delete_cancellable(
        &self,
        container: &ContainerId,
        cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.delete(container)
    }
    fn signal_cancellable(
        &self,
        container: &ContainerId,
        signal: JobSignal,
        cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.signal(container, signal)
    }
    fn resize_cancellable(
        &self,
        container: &ContainerId,
        size: TerminalSize,
        cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        cancel.check()?;
        self.resize(container, size)
    }
    fn create(&self, _: &JobSpec) -> Result<ContainerId, RuntimeError> {
        Ok(container())
    }
    fn attach(&self, _: &ContainerId, _: Option<TerminalSize>) -> Result<Attachment, RuntimeError> {
        Ok(self.attachment.lock().unwrap().take().unwrap())
    }
    fn start(&self, _: &ContainerId) -> Result<(), RuntimeError> {
        if self.start_fails.load(Ordering::SeqCst) {
            Err(io::Error::other("injected start rejection").into())
        } else {
            Ok(())
        }
    }
    fn wait(&self, _: &ContainerId) -> Result<RuntimeExit, RuntimeError> {
        self.threads.record();
        thread::sleep(self.wait_delay);
        self.wait_done.fetch_add(1, Ordering::SeqCst);
        Ok(self.exit)
    }
    fn wait_cancellable(
        &self,
        container: &ContainerId,
        _: &Cancellation,
    ) -> Result<RuntimeExit, RuntimeError> {
        self.wait(container)
    }
    fn signal(&self, _: &ContainerId, _: JobSignal) -> Result<(), RuntimeError> {
        Ok(())
    } // Producer was actually reaped before attachment.
    fn resize(&self, _: &ContainerId, _: TerminalSize) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn delete(&self, _: &ContainerId) -> Result<(), RuntimeError> {
        if self.reaped.load(Ordering::SeqCst) == 1 {
            Ok(())
        } else {
            Err(RuntimeError::DeletionUncertain)
        }
    }
}
fn container() -> ContainerId {
    ContainerId::parse("a".repeat(64)).unwrap()
}

#[derive(Clone, Default)]
struct ThreadProof {
    #[cfg(target_os = "linux")]
    identities: Arc<Mutex<std::collections::BTreeSet<PathBuf>>>,
}
impl ThreadProof {
    #[cfg_attr(not(target_os = "linux"), allow(clippy::unused_self))] // `self` is used only on Linux.
    fn record(&self) {
        #[cfg(target_os = "linux")]
        {
            let identity = fs::read_link("/proc/thread-self").unwrap();
            self.identities
                .lock()
                .unwrap()
                .insert(Path::new("/proc/self/task").join(identity.file_name().unwrap()));
        }
    }
    #[cfg_attr(not(target_os = "linux"), allow(clippy::unused_self))] // `self` is used only on Linux.
    fn assert_joined(&self) {
        #[cfg(target_os = "linux")]
        {
            let identities = self.identities.lock().unwrap();
            assert_eq!(
                identities.len(),
                4,
                "observe all three IO pumps and runtime waiter"
            );
            for identity in &*identities {
                assert!(
                    !identity.exists(),
                    "pump still alive: {}",
                    identity.display()
                );
            }
            println!("exact_pump_threads_reaped={identities:?}");
        }
    }
}

struct TrackIo<T>(T, Arc<AtomicUsize>, Option<Arc<AtomicUsize>>, ThreadProof);
impl<T> Drop for TrackIo<T> {
    fn drop(&mut self) {
        self.3.record();
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}
impl<T: Read> Read for TrackIo<T> {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        self.0.read(b)
    }
}
impl<T: Write> Write for TrackIo<T> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if let Some(progress) = &self.2 {
            progress.fetch_add(1, Ordering::SeqCst);
        }
        let result = self.0.write(b);
        if let Some(progress) = &self.2 {
            progress.fetch_add(1, Ordering::SeqCst);
        }
        result
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}
struct TrackProcess(Box<dyn AttachedProcess>, Arc<AtomicUsize>, Arc<AtomicBool>);
impl AttachedProcess for TrackProcess {
    fn supports_io_cancellation(&self) -> bool {
        self.0.supports_io_cancellation()
    }
    fn cancel_io(&self) -> io::Result<()> {
        self.0.cancel_io()
    }
    fn wait(&mut self) -> io::Result<i32> {
        let result = self.0.wait();
        if result.is_ok() {
            self.1.store(1, Ordering::SeqCst);
        }
        result
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        let result = self.0.try_wait();
        if matches!(result, Ok(Some(_))) {
            self.1.store(1, Ordering::SeqCst);
        }
        result
    }
    fn terminate(&mut self) -> io::Result<()> {
        self.0.terminate()?;
        if self.2.load(Ordering::SeqCst) {
            Err(io::Error::other("injected missing local cleanup receipt"))
        } else {
            Ok(())
        }
    }
}
struct Fixture {
    root: OwnedDirectory,
    runtime: Arc<ObservedRuntime>,
    io_dropped: Arc<AtomicUsize>,
    reaped: Arc<AtomicUsize>,
    carrier_pid: u32,
    input_progress: Arc<AtomicUsize>,
    termination_receipt_failed: Arc<AtomicBool>,
}
impl Fixture {
    fn new(code: i32, blocked_input: bool) -> Self {
        Self::with_linger(code, blocked_input, false)
    }
    fn with_linger(code: i32, blocked_input: bool, linger: bool) -> Self {
        let root = OwnedDirectory::new();
        let payload = root.0.join("payload");
        let runner = SystemCommandRunner::new("/tmp");
        let observed = runner.run(&Invocation {
            program: "/usr/bin/python3".into(), arguments: vec!["-c".into(), format!("import os,sys; open(sys.argv[1],'wb').write(bytes(range(256))*16384); os._exit({code})").into(), payload.as_os_str().to_owned()], working_directory: None,
        }).unwrap();
        assert_eq!(observed.exit_code, Some(code));
        let pid_path = root.0.join("pid");
        let script = if blocked_input {
            "import os,sys,time,threading\nopen(sys.argv[2],'w').write(str(os.getpid()))\ndef output():\n sys.stdout.buffer.write(open(sys.argv[1],'rb').read()); sys.stdout.buffer.flush()\nthreading.Thread(target=output).start()\nwhile not os.path.exists(sys.argv[2]+'.release'): time.sleep(.01)\nsys.stderr.buffer.write(b'e'*1048576); sys.stderr.buffer.flush(); time.sleep(60)"
        } else if linger {
            "import os,sys,time; open(sys.argv[2],'w').write(str(os.getpid())); sys.stdin.buffer.read(); sys.stdout.buffer.write(open(sys.argv[1],'rb').read()); sys.stdout.buffer.flush(); os.close(1); os.close(2); time.sleep(.5); os._exit(0)"
        } else {
            "import os,sys; open(sys.argv[2],'w').write(str(os.getpid())); sys.stdin.buffer.read(); sys.stdout.buffer.write(open(sys.argv[1],'rb').read()); sys.stdout.buffer.flush()"
        };
        let mut attachment = runner
            .spawn_attached(&Invocation {
                program: "/usr/bin/python3".into(),
                arguments: vec![
                    "-c".into(),
                    script.into(),
                    payload.into_os_string(),
                    pid_path.as_os_str().to_owned(),
                ],
                working_directory: None,
            })
            .unwrap();
        let deadline = Instant::now() + CONDITION_DEADLINE;
        let carrier_pid = loop {
            if let Ok(pid) = fs::read_to_string(&pid_path)
                && let Ok(pid) = pid.parse()
            {
                break pid;
            }
            if Instant::now() >= deadline {
                let _ = attachment.process.cancel_io();
                let _ = attachment.process.terminate();
                let _ = attachment.process.wait();
                panic!("owned carrier did not publish its PID");
            }
            thread::sleep(Duration::from_millis(5));
        };
        let io_dropped = Arc::new(AtomicUsize::new(0));
        let reaped = Arc::new(AtomicUsize::new(0));
        let input_progress = Arc::new(AtomicUsize::new(0));
        let threads = ThreadProof::default();
        attachment.stdin = Box::new(TrackIo(
            attachment.stdin,
            io_dropped.clone(),
            Some(input_progress.clone()),
            threads.clone(),
        ));
        attachment.stdout = Box::new(TrackIo(
            attachment.stdout,
            io_dropped.clone(),
            None,
            threads.clone(),
        ));
        attachment.stderr = Box::new(TrackIo(
            attachment.stderr,
            io_dropped.clone(),
            None,
            threads.clone(),
        ));
        let termination_receipt_failed = Arc::new(AtomicBool::new(false));
        attachment.process = Box::new(TrackProcess(
            attachment.process,
            reaped.clone(),
            termination_receipt_failed.clone(),
        ));
        let runtime = Arc::new(ObservedRuntime {
            exit: RuntimeExit {
                code: observed.exit_code.unwrap(),
                oom_killed: false,
                pids_max_events: Some(0),
                writable_bytes: 0,
                writable_exceeded: false,
            },
            attachment: Mutex::new(Some(attachment)),
            wait_done: Arc::new(AtomicUsize::new(0)),
            reaped: reaped.clone(),
            threads,
            start_fails: AtomicBool::new(false),
            wait_delay: if blocked_input {
                Duration::from_millis(100)
            } else {
                Duration::ZERO
            },
        });
        Self {
            root,
            runtime,
            io_dropped,
            reaped,
            carrier_pid,
            input_progress,
            termination_receipt_failed,
        }
    }
    fn assert_reaped(&self) {
        self.runtime.threads.assert_joined();
        assert_eq!(
            self.io_dropped.load(Ordering::SeqCst),
            3,
            "stdin/stdout/stderr owners must drop before report"
        );
        assert_eq!(self.runtime.wait_done.load(Ordering::SeqCst), 1);
        assert_eq!(self.reaped.load(Ordering::SeqCst), 1);
        #[cfg(target_os = "linux")]
        assert!(!Path::new(&format!("/proc/{}", self.carrier_pid)).exists());
        assert!(self.root.0.exists());
    }
}
fn socket_writer(socket: UnixStream, cancel: Cancellation) -> InterruptibleWriter {
    // Darwin ignores per-call MSG_DONTWAIT for a full AF_UNIX stream send and
    // blocks in the kernel; this test-owned description is safe to make
    // nonblocking, matching the Linux-only production worker's wake behavior.
    socket.set_nonblocking(true).unwrap();
    InterruptibleWriter::from_file(fs::File::from(OwnedFd::from(socket)), cancel).unwrap()
}
fn limits() -> SupervisionLimits {
    SupervisionLimits {
        wall_time: Duration::from_secs(30),
        output_bytes: 8 * 1024 * 1024,
        writable_bytes: u64::MAX,
    }
}
fn receipt(case: &str, report: &WorkerReport, bytes: Option<usize>, pid: u32) {
    println!(
        "WORKER_DRAIN_RECEIPT {}",
        serde_json::json!({"case": case, "report": report, "collected_stdout_bytes": bytes, "carrier_pid": pid, "carrier_reaped": true, "io_owners_dropped": 3, "wait_joined": true})
    );
}

fn assert_payload(bytes: &[u8]) {
    assert_eq!(bytes.len(), 4_194_304, "carrier output truncated");
    assert!(
        bytes
            .iter()
            .enumerate()
            .all(|(i, b)| *b == u8::try_from(i % 256).unwrap())
    );
}

#[test]
fn observed_exit_does_not_expire_a_healthy_paused_output_carrier() {
    for code in [42, 0] {
        let fixture = Fixture::new(code, false);
        let (send, receive) = mpsc::channel();
        send.send(Ok(WorkerControl::CloseInput)).unwrap();
        let (output, mut reader) = UnixStream::pair().unwrap();
        let reading = thread::spawn(move || {
            thread::sleep(Duration::from_secs(7));
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let started = Instant::now();
        let worker = Worker::with_grant_verifier(
            fixture.runtime.clone(),
            Arc::new(ThreadSupervisor::default()),
            Arc::new(Granted),
        );
        let mut spec = super::tests::job();
        spec.resources.output_bytes = limits().output_bytes;
        let report = worker.run(
            &spec,
            &mut ChannelControlSource::new(receive),
            JobStreams {
                stdout: socket_writer(output, Cancellation::default()),
                stderr: InterruptibleWriter::sink(),
            },
        );
        let bytes = reading.join().unwrap();
        assert_payload(&bytes);
        assert_eq!(report.execution, ExecutionOutcome::Exited { code });
        assert_eq!(report.delivery, DeliveryOutcome::Complete);
        assert_eq!(report.cleanup, CleanupOutcome::Verified);
        fixture.assert_reaped();
        receipt(
            &format!("paused7s-exit{code}"),
            &report,
            Some(bytes.len()),
            fixture.carrier_pid,
        );
        println!(
            "positive code={code} bytes={} paused=7s elapsed={:?} carrier_pid={} owners_dropped=3 waiter_joined=true carrier_reaped=true report={report:?}",
            bytes.len(),
            started.elapsed(),
            fixture.carrier_pid
        );
    }
}

fn wait_both_blocked(
    mode: &str,
    progress: &AtomicUsize,
    input_progress: &AtomicUsize,
    worker_returned: &AtomicBool,
) {
    let deadline = Instant::now() + CONDITION_DEADLINE;
    loop {
        let before = progress.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(100));
        if before % 2 == 1
            && before == progress.load(Ordering::SeqCst)
            && input_progress.load(Ordering::SeqCst) % 2 == 1
        {
            return;
        }
        assert!(
            !worker_returned.load(Ordering::SeqCst),
            "{mode}: the worker returned before real stdin and socket writes both blocked; \
             stdout_progress={} stdin_progress={}",
            progress.load(Ordering::SeqCst),
            input_progress.load(Ordering::SeqCst)
        );
        assert!(
            Instant::now() < deadline,
            "real stdin and socket writes must both block before {mode}; stdout_progress={} stdin_progress={}",
            progress.load(Ordering::SeqCst),
            input_progress.load(Ordering::SeqCst)
        );
    }
}

/// Waits until the worker's stdin and stdout writes have both really blocked,
/// then does what `mode` calls for. Returns whether the worker returned while
/// this side still held the control channel open.
fn spawn_controller(
    mode: &'static str,
    send: mpsc::Sender<Result<WorkerControl, WorkerError>>,
    release: PathBuf,
    progress: Arc<AtomicUsize>,
    input_progress: Arc<AtomicUsize>,
    worker_returned: Arc<AtomicBool>,
) -> thread::JoinHandle<bool> {
    thread::spawn(move || {
        wait_both_blocked(mode, &progress, &input_progress, &worker_returned);
        match mode {
            "cancel" => send
                .send(Ok(WorkerControl::Signal {
                    signal: JobSignal::Kill,
                }))
                .unwrap(),
            // Dropping the sender is the loss itself: nothing is held open.
            "loss" => return true,
            "output" => fs::write(release, b"release stderr only after stdout blocks").unwrap(),
            _ => {}
        }
        // The cancel, the wall limit or the output limit must end the run on
        // their own. Keep the channel open until they have: a worker that
        // waited for it to close would only return once this deadline releases
        // it. Waiting on the worker instead of a fixed sleep keeps the check
        // about ordering, not about how fast the machine is.
        let deadline = Instant::now() + CONDITION_DEADLINE;
        while !worker_returned.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    })
}

#[test]
fn blocked_output_cancel_loss_wall_and_output_limit_preserve_actual42() {
    for mode in ["cancel", "loss", "wall", "output"] {
        let fixture = Fixture::new(42, true);
        let (send, receive) = mpsc::channel();
        for _ in 0..64 {
            send.send(Ok(WorkerControl::Input {
                bytes: vec![b'i'; MAX_STREAM_CHUNK],
            }))
            .unwrap();
        }
        let output_progress = Arc::new(AtomicUsize::new(0));
        let returned = Arc::new(AtomicBool::new(false));
        let controller = spawn_controller(
            mode,
            send,
            fixture.root.0.join("pid.release"),
            output_progress.clone(),
            fixture.input_progress.clone(),
            returned.clone(),
        );
        let (output, _paused_reader) = UnixStream::pair().unwrap();
        let mut policy = limits();
        if mode == "wall" {
            // The limit is the action in this mode, so it cannot wait for both
            // writers to block: it must be well past the time they need.
            policy.wall_time = Duration::from_secs(5);
        }
        if mode == "output" {
            policy.output_bytes = 512 * 1024;
        }
        let started = Instant::now();
        let worker = Worker::with_grant_verifier(
            fixture.runtime.clone(),
            Arc::new(ThreadSupervisor {
                termination_grace: Duration::from_millis(100),
            }),
            Arc::new(Granted),
        );
        let mut spec = super::tests::job();
        spec.resources.wall_seconds = policy.wall_time.as_secs();
        spec.resources.output_bytes = policy.output_bytes;
        let report = worker.run(
            &spec,
            &mut ChannelControlSource::new(receive),
            JobStreams {
                stdout: {
                    let cancel = Cancellation::default();
                    InterruptibleWriter::new(
                        Box::new(TrackIo(
                            socket_writer(output, cancel.clone()),
                            Arc::new(AtomicUsize::new(0)),
                            Some(output_progress),
                            ThreadProof::default(),
                        )),
                        cancel,
                    )
                },
                stderr: InterruptibleWriter::sink(),
            },
        );
        returned.store(true, Ordering::SeqCst);
        // Joined first: a precondition panic in the controller names the cause.
        assert!(
            controller.join().unwrap(),
            "{mode}: the worker returned only after its control channel was released: {report:?}"
        );
        assert_eq!(report.execution, ExecutionOutcome::Exited { code: 42 });
        let expected = match mode {
            "cancel" => DeliveryOutcome::Cancelled,
            "loss" => DeliveryOutcome::Failed,
            "wall" => DeliveryOutcome::LimitExceeded {
                resource: ResourceLimit::Wall,
            },
            _ => DeliveryOutcome::LimitExceeded {
                resource: ResourceLimit::Output,
            },
        };
        assert_eq!(report.delivery, expected);
        assert_eq!(report.cleanup, CleanupOutcome::Verified);
        fixture.assert_reaped();
        receipt(
            &format!("blocked-{mode}"),
            &report,
            None,
            fixture.carrier_pid,
        );
        println!(
            "blocked mode={mode} stdin_and_output_observed_blocked=true elapsed={:?} carrier_pid={} owners_dropped=3 waiter_joined=true carrier_reaped=true report={report:?}",
            started.elapsed(),
            fixture.carrier_pid
        );
    }
}

#[test]
fn retained_terminal_follows_all_output_and_joined_carrier() {
    let fixture = Fixture::new(42, false);
    let worker = Arc::new(Worker::with_grant_verifier(
        fixture.runtime.clone(),
        Arc::new(ThreadSupervisor::default()),
        Arc::new(Granted),
    ));
    let (mut client, peer) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let transport = WorkerTransport::from_socket(peer).unwrap();
    let serving = thread::spawn(move || serve_retained(&worker, 1, 1, transport));
    assert!(matches!(
        read_frame::<WorkerResponse>(&mut client).unwrap(),
        WorkerResponse::Ready { .. }
    ));
    let mut spec = super::tests::job();
    spec.resources.output_bytes = 8 * 1024 * 1024;
    write_frame(
        &mut client,
        &WorkerRequest::Start {
            attempt: "real".into(),
            generation: 1,
            spec,
        },
    )
    .unwrap();
    write_frame(
        &mut client,
        &WorkerRequest::CloseInput {
            attempt: "real".into(),
        },
    )
    .unwrap();
    thread::sleep(Duration::from_secs(7));
    let mut bytes = Vec::new();
    let report = loop {
        match read_frame::<WorkerResponse>(&mut client).unwrap() {
            WorkerResponse::Stdout { bytes: chunk, .. } => bytes.extend(chunk),
            WorkerResponse::Terminal { report, .. } => break report,
            WorkerResponse::Started { .. } => {}
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_payload(&bytes);
    assert_eq!(report.execution, ExecutionOutcome::Exited { code: 42 });
    assert_eq!(report.delivery, DeliveryOutcome::Complete);
    fixture.assert_reaped();
    receipt(
        "retained-terminal-order",
        &report,
        Some(bytes.len()),
        fixture.carrier_pid,
    );
    drop(client);
    serving.join().unwrap().unwrap();
    println!(
        "retained bytes={} terminal_after_output=true exact_carrier_reaped=true report={report:?}",
        bytes.len()
    );
}
#[test]
fn start_failure_preserves_local_cleanup_uncertainty_after_verified_deletion() {
    for uncertain in [false, true] {
        let fixture = Fixture::new(42, false);
        fixture.runtime.start_fails.store(true, Ordering::SeqCst);
        fixture
            .termination_receipt_failed
            .store(uncertain, Ordering::SeqCst);
        let worker = Worker::with_grant_verifier(
            fixture.runtime.clone(),
            Arc::new(ThreadSupervisor::default()),
            Arc::new(Granted),
        );
        let (_send, receive) = mpsc::channel();
        let report = worker.run(
            &super::tests::job(),
            &mut ChannelControlSource::new(receive),
            JobStreams {
                stdout: InterruptibleWriter::sink(),
                stderr: InterruptibleWriter::sink(),
            },
        );
        assert_eq!(fixture.reaped.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.io_dropped.load(Ordering::SeqCst), 3);
        assert_eq!(
            fixture.runtime.wait_done.load(Ordering::SeqCst),
            0,
            "no supervision pumps started"
        );
        #[cfg(target_os = "linux")]
        assert!(!Path::new(&format!("/proc/{}", fixture.carrier_pid)).exists());
        assert_eq!(
            report.execution,
            ExecutionOutcome::SetupFailed {
                stage: SetupStage::Start
            }
        );
        assert_eq!(
            report.cleanup,
            if uncertain {
                CleanupOutcome::Uncertain
            } else {
                CleanupOutcome::Verified
            }
        );
        assert_eq!(report.quarantine, uncertain);
        println!(
            "START_FAILURE_RECEIPT {}",
            serde_json::json!({"missing_local_termination_receipt": uncertain, "real_carrier_pid": fixture.carrier_pid, "carrier_reaped": true, "io_owners_dropped": 3, "container_delete_verified": true, "supervision_pumps_started": 0, "report": report})
        );
    }
}

#[test]
fn healthy_closed_stdio_carrier_still_uses_wall_not_cleanup_grace() {
    let fixture = Fixture::with_linger(42, false, true);
    let worker = Worker::with_grant_verifier(
        fixture.runtime.clone(),
        Arc::new(ThreadSupervisor {
            termination_grace: Duration::from_millis(50),
        }),
        Arc::new(Granted),
    );
    let mut spec = super::tests::job();
    spec.resources.output_bytes = 8 * 1024 * 1024;
    let (send, receive) = mpsc::channel();
    send.send(Ok(WorkerControl::CloseInput)).unwrap();
    let (output, mut reader) = UnixStream::pair().unwrap();
    let reading = thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let started = Instant::now();
    let report = worker.run(
        &spec,
        &mut ChannelControlSource::new(receive),
        JobStreams {
            stdout: socket_writer(output, Cancellation::default()),
            stderr: InterruptibleWriter::sink(),
        },
    );
    let bytes = reading.join().unwrap();
    assert_payload(&bytes);
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert_eq!(report.execution, ExecutionOutcome::Exited { code: 42 });
    assert_eq!(report.delivery, DeliveryOutcome::Complete);
    fixture.assert_reaped();
    receipt(
        "healthy-carrier-lingers-after-stdio-eof",
        &report,
        Some(bytes.len()),
        fixture.carrier_pid,
    );
}

#[test]
fn retained_cancel_finishes_started_frame_before_terminal() {
    let fixture = Fixture::new(42, false);
    let worker = Arc::new(Worker::with_grant_verifier(
        fixture.runtime.clone(),
        Arc::new(ThreadSupervisor {
            termination_grace: Duration::from_millis(100),
        }),
        Arc::new(Granted),
    ));
    let (mut client, peer) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let transport = WorkerTransport::from_socket(peer).unwrap();
    let cancel = transport.cancellation();
    let (done, result) = mpsc::channel();
    let serving = thread::spawn(move || {
        let result = serve_retained(&worker, 1, 8, transport);
        let _ = done.send(());
        result
    });
    assert!(matches!(
        read_frame::<WorkerResponse>(&mut client).unwrap(),
        WorkerResponse::Ready { .. }
    ));
    let mut spec = super::tests::job();
    spec.resources.output_bytes = 8 * 1024 * 1024;
    write_frame(
        &mut client,
        &WorkerRequest::Start {
            attempt: "blocked".into(),
            generation: 1,
            spec,
        },
    )
    .unwrap();
    write_frame(
        &mut client,
        &WorkerRequest::CloseInput {
            attempt: "blocked".into(),
        },
    )
    .unwrap();
    // Keep the peer connected and do NOT read any job output.
    thread::sleep(Duration::from_millis(500));
    let started = Instant::now();
    write_frame(
        &mut client,
        &WorkerRequest::Ping {
            generation: 1,
            nonce: 1,
        },
    )
    .unwrap();
    write_frame(
        &mut client,
        &WorkerRequest::Cancel {
            attempt: "blocked".into(),
        },
    )
    .unwrap();
    thread::sleep(Duration::from_millis(100));
    let alive = matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty));
    let observed = (|| -> Result<WorkerReport, WorkerError> {
        loop {
            if let WorkerResponse::Terminal { report, .. } =
                read_frame::<WorkerResponse>(&mut client)?
            {
                return Ok(report);
            }
        }
    })();
    drop(client);
    if observed.is_err() {
        cancel.cancel();
    }
    let serving_result = serving.join().unwrap();
    assert!(alive, "routine attempt cancellation closed the transport");
    let report = observed.unwrap();
    assert_eq!(report.execution, ExecutionOutcome::Exited { code: 42 });
    assert_ne!(report.delivery, DeliveryOutcome::Complete);
    assert!(serving_result.is_ok());
    fixture.assert_reaped();
    println!(
        "retained_cancel valid_frame_and_terminal=true carrier_pid={} reaped=true elapsed={:?}",
        fixture.carrier_pid,
        started.elapsed()
    );
}

struct Granted;
impl GrantSnapshot for Granted {
    fn still_matches(&self) -> bool {
        true
    }
}
impl GrantVerifier for Granted {
    fn capture(&self, _: &JobSpec) -> io::Result<Box<dyn GrantSnapshot>> {
        Ok(Box::new(Granted))
    }
}
