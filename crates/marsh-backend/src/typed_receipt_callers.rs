//! Supporting real worker/stdio/socket/receipt callers. The runtime below runs
//! owned Python processes, NOT containers. PID evidence is an actual RLIMIT fork
//! refusal, not Docker cgroup qualification. The host-only stock UAT separately
//! exercises the real Docker runtime and mount/cleanup boundary.
use super::*;
use marsh_contracts::{ContainerId, JobMount, OciImage, SetupStage};
use marsh_daemon::{Client, Server, SessionAuthority};
use marsh_runtime::{
    AttachedProcess, AttachmentControl, CommandRunner, Invocation, JobRuntime, RuntimeError,
    RuntimeExit, SystemCommandRunner,
};
use marsh_worker::{
    GrantSnapshot, GrantVerifier, ThreadSupervisor, Worker, read_frame, serve_retained, write_frame,
};
use std::io;
use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::atomic::{AtomicBool, Ordering},
};

struct Granted;
impl GrantSnapshot for Granted {
    fn still_matches(&self) -> bool {
        true
    }
}
impl GrantVerifier for Granted {
    fn capture(&self, _: &JobSpec) -> io::Result<Box<dyn GrantSnapshot>> {
        Ok(Box::new(Self))
    }
}

struct ProcessState {
    process: Box<dyn AttachedProcess>,
    exit: Option<i32>,
}
#[derive(Clone)]
struct SharedProcess(Arc<Mutex<ProcessState>>);
impl AttachedProcess for SharedProcess {
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        let mut state = self.0.lock().unwrap();
        if state.exit.is_none() {
            state.exit = state.process.try_wait()?;
        }
        Ok(state.exit)
    }
    fn wait(&mut self) -> io::Result<i32> {
        loop {
            if let Some(code) = self.try_wait()? {
                return Ok(code);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn terminate(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().process.terminate()
    }
    fn supports_io_cancellation(&self) -> bool {
        self.0.lock().unwrap().process.supports_io_cancellation()
    }
    fn cancel_io(&self) -> io::Result<()> {
        self.0.lock().unwrap().process.cancel_io()
    }
}

struct ProcessRuntime {
    root: PathBuf,
    mode: &'static str,
    process: Mutex<Option<SharedProcess>>,
    control: Mutex<Option<Arc<dyn AttachmentControl>>>,
}
impl Drop for ProcessRuntime {
    fn drop(&mut self) {
        if let Some(process) = self.process.lock().unwrap().as_mut() {
            if process.try_wait().ok().flatten().is_none() {
                let _ = process.terminate();
            }
            let _ = process.wait();
        }
    }
}

impl JobRuntime for ProcessRuntime {
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
    fn create(&self, _: &JobSpec) -> Result<ContainerId, RuntimeError> {
        fs::write(self.root.join("runtime-created"), b"owned")?;
        Ok(ContainerId::parse("a".repeat(64)).unwrap())
    }
    fn attach(
        &self,
        _: &ContainerId,
        _: Option<TerminalSize>,
    ) -> Result<marsh_runtime::Attachment, RuntimeError> {
        if self.mode == "setup" {
            return Err(RuntimeError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "fixture attachment unavailable",
            )));
        }
        let script = r"
import errno, os, pathlib, resource, sys, time
root = pathlib.Path(sys.argv[1]); mode = sys.argv[2]
(root / 'process-identity').write_text(f'{os.getpid()} {os.getpgrp()} {os.getsid(0)}')
(root / 'effect').write_bytes(b'worker-process-effect')
if mode == 'output':
    while True: os.write(1, b'x' * 8192)
elif mode == 'wall':
    time.sleep(60)
elif mode == 'pids':
    resource.setrlimit(resource.RLIMIT_NPROC, (1, 1))
    try: pid = os.fork()
    except OSError as error:
        (root / 'fork-errno').write_text(str(error.errno)); sys.exit(23)
    if pid == 0: os._exit(99)
    os.waitpid(pid, 0); sys.exit(99)
elif mode == 'stderr':
    os.write(1, b'data\x00\n'); os.write(2, b'direct-stderr\x00\n'); sys.exit(0)
else:
    os.write(2, b'native-seven\n'); sys.exit(7)
";
        let mut attachment = SystemCommandRunner::new(&self.root).spawn_attached(&Invocation {
            program: "/usr/bin/python3".into(),
            arguments: vec![
                "-I".into(),
                "-S".into(),
                "-c".into(),
                script.into(),
                self.root.as_os_str().into(),
                self.mode.into(),
            ],
            working_directory: Some(self.root.clone()),
            environment: Vec::new(),
        })?;
        let process = SharedProcess(Arc::new(Mutex::new(ProcessState {
            process: attachment.process,
            exit: None,
        })));
        *self.process.lock().unwrap() = Some(process.clone());
        *self.control.lock().unwrap() = Some(attachment.control.clone());
        attachment.process = Box::new(process);
        Ok(attachment)
    }
    fn start(&self, _: &ContainerId) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn wait(&self, _: &ContainerId) -> Result<RuntimeExit, RuntimeError> {
        let code = self
            .process
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone()
            .wait()?;
        fs::write(self.root.join("runtime-exit-code"), code.to_string())?;
        let pids = fs::read_to_string(self.root.join("fork-errno"))
            .ok()
            .as_deref()
            == Some(eagain().as_str());
        Ok(RuntimeExit {
            code,
            oom_killed: false,
            pids_max_events: Some(u64::from(pids)),
            writable_bytes: 0,
            writable_exceeded: false,
        })
    }
    fn wait_cancellable(
        &self,
        container: &ContainerId,
        cancel: &marsh_runtime::Cancellation,
    ) -> Result<RuntimeExit, RuntimeError> {
        let mut process = self.process.lock().unwrap().as_ref().unwrap().clone();
        while process.try_wait()?.is_none() {
            cancel.check()?;
            thread::sleep(Duration::from_millis(5));
        }
        self.wait(container)
    }
    fn signal(&self, _: &ContainerId, signal: JobSignal) -> Result<(), RuntimeError> {
        self.control
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .signal(signal)
            .map_err(RuntimeError::Io)
    }
    fn resize(&self, _: &ContainerId, _: TerminalSize) -> Result<(), RuntimeError> {
        unreachable!()
    }
    fn delete(&self, _: &ContainerId) -> Result<(), RuntimeError> {
        if let Some(process) = self.process.lock().unwrap().as_mut() {
            process.wait()?;
        }
        if self.mode == "uncertain" {
            return Err(RuntimeError::DeletionUncertain);
        }
        fs::remove_file(self.root.join("runtime-created"))?;
        Ok(())
    }
}

/// `RLIMIT_NPROC` fork failure errno (`EAGAIN`: 11 on Linux, 35 on macOS).
fn eagain() -> String {
    rustix::io::Errno::AGAIN.raw_os_error().to_string()
}

fn worker_observation(root: &Path, mode: &'static str) -> (WorkerReport, Vec<u8>, Vec<u8>) {
    let runtime = Arc::new(ProcessRuntime {
        root: root.into(),
        mode,
        process: Mutex::new(None),
        control: Mutex::new(None),
    });
    let worker = Arc::new(Worker::with_grant_verifier(
        runtime,
        Arc::new(ThreadSupervisor::default()),
        Arc::new(Granted),
    ));
    let (mut controller, peer) = UnixStream::pair().unwrap();
    controller
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let transport = marsh_worker::WorkerTransport::from_socket(peer).unwrap();
    let serving = thread::spawn(move || {
        serve_retained(
            &worker,
            1,
            usize::from(WORKER_CONTAINER_CAPACITY),
            transport,
        )
        .unwrap();
    });
    assert!(matches!(
        read_frame::<WorkerResponse>(&mut controller).unwrap(),
        WorkerResponse::Ready { generation: 1 }
    ));
    let spec = JobSpec {
        image: OciImage::parse(format!("sha256:{}", "b".repeat(64))).unwrap(),
        argv: vec![],
        identity: JobIdentity {
            uid: 1000,
            gid: 1000,
        },
        session_environment: BTreeMap::from([
            ("HOME".into(), "/home/fixture".into()),
            ("MARSH_SELECTED_HOME".into(), "/home/fixture".into()),
            ("USER".into(), "fixture".into()),
            ("LOGNAME".into(), "fixture".into()),
        ]),
        exported_environment: BTreeMap::new(),
        working_directory: "/project".into(),
        mounts: vec![JobMount {
            source: "/run/marsh/grants/attempt-1/project".into(),
            target: "/project".into(),
            access: MountAccess::ReadWrite,
            subpath: None,
        }],
        resources: JobResources {
            cpu_millis: 1000,
            memory_bytes: 134_217_728,
            pids: 32,
            writable_bytes: 16_777_216,
            output_bytes: 32768,
            // Only the wall mode is meant to hit its budget. Every other mode must
            // outlast a cold /usr/bin/python3 start on a loaded machine, or the
            // process is killed before it writes the effect the test observes.
            wall_seconds: if mode == "wall" { 3 } else { 60 },
        },
        terminal: false,
        terminal_size: None,
        split_capability: None,
        capability: None,
    };
    write_frame(
        &mut controller,
        &WorkerRequest::Start {
            attempt: "attempt-1".into(),
            generation: 1,
            spec,
        },
    )
    .unwrap();
    write_frame(
        &mut controller,
        &WorkerRequest::CloseInput {
            attempt: "attempt-1".into(),
        },
    )
    .unwrap();
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let report = loop {
        match read_frame::<WorkerResponse>(&mut controller).unwrap() {
            WorkerResponse::Started { .. } => {}
            WorkerResponse::Stdout { bytes, .. } => stdout.extend(bytes),
            WorkerResponse::Stderr { bytes, .. } => stderr.extend(bytes),
            WorkerResponse::Terminal { report, .. } => break report,
            other => panic!("unexpected worker frame {other:?}"),
        }
    };
    controller.shutdown(Shutdown::Both).unwrap();
    serving.join().unwrap();
    (report, stdout, stderr)
}

fn observed_effects(task: &Path, mode: &'static str) -> WorkerReport {
    let (report, stdout, stderr) = worker_observation(task, mode);
    assert_eq!(task.join("runtime-created").exists(), mode == "uncertain");
    if mode == "setup" {
        assert!(!task.join("effect").exists());
        assert!(stderr.starts_with(b"marsh-worker: attach:"));
    } else {
        assert_eq!(
            fs::read(task.join("effect")).unwrap(),
            b"worker-process-effect"
        );
        let identity = fs::read_to_string(task.join("process-identity")).unwrap();
        let ids: Vec<_> = identity.split_whitespace().collect();
        assert_eq!(ids[0], ids[1]);
        assert_eq!(ids.len(), 3);
        eprintln!(
            "owned_fixture mode={mode} pid={} pgid={} sid={}",
            ids[0], ids[1], ids[2]
        );
    }
    match mode {
        "stderr" => {
            assert_eq!(stdout, b"data\0\n");
            assert_eq!(stderr, b"direct-stderr\0\n");
        }
        "seven" => {
            assert!(stdout.is_empty());
            assert_eq!(stderr, b"native-seven\n");
        }
        "output" => {
            assert!(!stdout.is_empty() && stdout.len() <= 32768);
            assert!(stdout.iter().all(|byte| *byte == b'x'));
        }
        "pids" => assert_eq!(
            fs::read_to_string(task.join("fork-errno")).unwrap(),
            eagain()
        ),
        _ => {}
    }
    eprintln!(
        "worker_observation mode={mode} execution={} cleanup={:?} stdout_bytes={} stderr_bytes={}",
        serde_json::to_string(&report.execution).unwrap(),
        report.cleanup,
        stdout.len(),
        stderr.len(),
    );
    report
}

fn persist_observation(
    store: &DaemonStore,
    session: &str,
    mode: &str,
    report: WorkerReport,
    host_delivery_complete: bool,
) -> String {
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session.into(),
            command: mode.into(),
            kit_ref: "fixture".into(),
            workload_image: "fixture-image".into(),
            mounts: vec![],
        })
        .unwrap();
    let cleanup = match report.cleanup {
        CleanupOutcome::Verified => CleanupState::Verified,
        CleanupOutcome::Uncertain => CleanupState::Uncertain,
        CleanupOutcome::NotRequired => CleanupState::NotRequired,
    };
    let complete =
        host_delivery_complete && report.delivery == marsh_worker::DeliveryOutcome::Complete;
    let (code, cause) = worker_public_exit(&report, cleanup == CleanupState::Uncertain, complete);
    let now = Instant::now();
    let boundaries = PhaseBoundaries {
        started: now,
        started_unix_ms: unix_millis(SystemTime::now()),
        vm_ready_at: now,
        admitted_at: now,
        mount_ready_at: now,
        worker_progress_at: now,
        output_relay_at: now,
        first_output_at: None,
        process_exit_at: now,
        output_drained_at: now,
        capture_at: now,
        cleanup_at: now,
    };
    finish_job_with_timing(
        store,
        &job,
        report.execution,
        ExitStatus { code, cause },
        complete,
        cleanup,
        &boundaries,
    )
    .unwrap();
    job
}

fn check_legacy_receipt(store: &DaemonStore, client: &Client, session: &str) {
    let (legacy, _) = store
        .begin_job(NewJob {
            session_id: session.into(),
            command: "legacy".into(),
            kit_ref: "fixture".into(),
            workload_image: "fixture-image".into(),
            mounts: vec![],
        })
        .unwrap();
    store
        .finish_job(
            &legacy,
            ExitStatus {
                code: Some(0),
                cause: "limit:pids".into(),
            },
            true,
            CleanupState::Verified,
            TimingReport::default(),
        )
        .unwrap();
    assert_eq!(
        client.job(legacy).unwrap().execution,
        ExecutionOutcome::Unknown
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One scenario crosses worker report, receipt, and public query.
fn actual_worker_reports_survive_local_receipt_boundary_and_public_query() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("daemon");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::bind(&home).unwrap();
    let store = server.store();
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let serving =
        thread::spawn(move || server.serve_until(|| done.load(Ordering::Relaxed)).unwrap());
    let client = Client::connect(&home).unwrap();
    let session = store.attach_shell(
        1,
        SessionAuthority {
            username: "fixture".into(),
            uid: 1000,
            gid: 1000,
            launch_directory: root.path().into(),
            guest_home: "/home/fixture".into(),
            home_backing: root.path().into(),
            ephemeral_home: false,
        },
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for (mode, execution) in [
            ("seven", ExecutionOutcome::Exited { code: 7 }),
            ("stderr", ExecutionOutcome::Exited { code: 0 }),
            ("output", ExecutionOutcome::Exited { code: 125 }),
            (
                "pids",
                ExecutionOutcome::LimitExceeded {
                    resource: ResourceLimit::Pids,
                },
            ),
            ("wall", ExecutionOutcome::Exited { code: 125 }),
            (
                "setup",
                ExecutionOutcome::SetupFailed {
                    stage: SetupStage::Attach,
                },
            ),
            ("uncertain", ExecutionOutcome::Exited { code: 7 }),
        ] {
            let task = root.path().join(mode);
            fs::create_dir(&task).unwrap();
            let report = observed_effects(&task, mode);
            let execution = if matches!(mode, "output" | "wall") {
                let code = fs::read_to_string(task.join("runtime-exit-code"))
                    .unwrap()
                    .parse()
                    .unwrap();
                assert_eq!(
                    report.delivery,
                    marsh_worker::DeliveryOutcome::LimitExceeded {
                        resource: if mode == "output" {
                            ResourceLimit::Output
                        } else {
                            ResourceLimit::Wall
                        }
                    }
                );
                ExecutionOutcome::Exited { code }
            } else {
                execution
            };
            assert_eq!(report.execution, execution, "{mode}");
            if mode == "seven" {
                // Real host-side delivery failure AFTER independent worker exit,
                // not a fabricated replacement execution outcome.
                let (mut output, peer) = UnixStream::pair().unwrap();
                drop(peer);
                let delivered = std::io::Write::write_all(&mut output, b"native-seven\n").is_ok();
                assert!(!delivered);
                let lost = client
                    .job(persist_observation(
                        &store,
                        &session,
                        "seven-host-loss",
                        report.clone(),
                        delivered,
                    ))
                    .unwrap();
                assert_eq!(lost.execution, ExecutionOutcome::Exited { code: 7 });
                assert!(!lost.output_complete);
                assert_eq!(lost.exit.as_ref().unwrap().code, Some(125));
                eprintln!(
                    "receipt mode=seven-host-loss actual7_public125=true {}",
                    serde_json::to_string(&lost).unwrap()
                );
            }
            let cleanup = if mode == "uncertain" {
                CleanupState::Uncertain
            } else {
                CleanupState::Verified
            };
            let receipt = client
                .job(persist_observation(&store, &session, mode, report, true))
                .unwrap();
            assert_eq!(receipt.execution, execution);
            assert_eq!(receipt.cleanup, cleanup);
            assert_eq!(receipt.output_complete, !matches!(mode, "wall" | "output"));
            let wire = serde_json::to_value(receipt).unwrap();
            eprintln!(
                "receipt mode={mode} execution={} exit={} cleanup={}",
                wire["execution"], wire["exit"], wire["cleanup"]
            );
            assert!(wire["execution"]["status"].is_string());
            if mode == "uncertain" {
                assert_eq!(wire["execution"]["code"], 7);
                assert_eq!(wire["cleanup"], "uncertain");
            }
        }
        // A legacy-only caller cannot manufacture execution from cause/code.
        check_legacy_receipt(&store, &client, &session);
    }));
    drop(client);
    stop.store(true, Ordering::Relaxed);
    serving.join().unwrap();
    result.unwrap();
}
