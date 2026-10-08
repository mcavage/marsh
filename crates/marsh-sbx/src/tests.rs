use super::*;
use marsh_contracts::JobSignal;
mod source_grant_regressions;
mod vm_ownership_rules;
use marsh_runtime::{AttachedProcess, no_attachment_control};
use std::{
    collections::VecDeque,
    io::{self, Cursor},
    sync::{
        Barrier, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
};

#[test]
fn shell_cleanup_never_waits_behind_busy_mount_or_session_transitions() {
    let adapter = StockSbx::new(
        "/not-a-stock-cli",
        Arc::new(marsh_runtime::SystemCommandRunner::new("/tmp")),
    );
    let mounts = ShellMounts {
        vm: "owned-shell".into(),
        acquisition: 0,
        mounts: vec![],
    };
    let held = adapter.shell_mount_references.lock().unwrap();
    let start = Instant::now();
    adapter.revoke_shell_mounts(&mounts).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(held);
    let lifecycle = adapter.session_grant_lifecycle("owned-session");
    let held = lifecycle.lock().unwrap();
    let start = Instant::now();
    assert!(matches!(
        adapter.close_shell_session_grants("owned-session"),
        Err(SbxError::LifecycleDeadline { .. })
    ));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(
        !*held,
        "busy admission must not manufacture a completed close"
    );
}

#[test]
fn shell_close_waits_briefly_for_a_concurrent_grant_transition() {
    let adapter = Arc::new(StockSbx::new(
        "/not-a-stock-cli",
        Arc::new(marsh_runtime::SystemCommandRunner::new("/tmp")),
    ));
    // Another session's grant transition holds the shared map for one stock
    // call; closing this shell waits for it instead of quarantining.
    let held = adapter.grant_mount_references.lock().unwrap();
    let closer = {
        let adapter = Arc::clone(&adapter);
        thread::spawn(move || adapter.close_shell_session_grants("closing-session"))
    };
    thread::sleep(Duration::from_millis(200));
    drop(held);
    closer.join().unwrap().unwrap();
}

#[test]
fn shell_quarantine_retains_admitted_authority_and_rejects_before_stock_effects() {
    let root = tempfile::tempdir_in(canonical_temp_dir()).unwrap();
    let adapter = StockSbx::new(
        "/not-a-stock-cli",
        Arc::new(marsh_runtime::SystemCommandRunner::new("/tmp")),
    );
    let spec = shell_spec(root.path().join("missing-binary"));
    let vm = ReadyShellVm {
        name: spec.name.clone(),
        user: spec.user.clone(),
        cold_started: false,
    };
    let grant = AdmittedHostGrant::open(
        root.path().to_owned(),
        "/project".into(),
        MountAccess::ReadWrite,
    )
    .unwrap();
    adapter.quarantine_shell(&vm, vec![grant]);
    assert_eq!(
        adapter.uncertain_shell_grants.lock().unwrap()[&vm.name].len(),
        1
    );
    assert!(matches!(
        adapter.ensure_shell_vm(&spec),
        Err(SbxError::QuarantinedShellVm(_))
    ));
    assert!(matches!(
        adapter.attach_shell(&vm, root.path(), false, None, &[]),
        Err(SbxError::QuarantinedShellVm(_))
    ));
}

#[test]
fn supported_versions_accept_stable_and_coherent_nightly_from_045() {
    assert!(supported_sbx_version(
        "sbx version: v0.45.0 0000000000000000000000000000000000000000"
    ));
    assert!(supported_sbx_version(
        "sbx version: v0.45.0-239-g3fd12a12a 3fd12a12a0000000000000000000000000000000"
    ));
    assert!(supported_sbx_version(
        "sbx version: v0.45.0-240-g3fd12a1 3fd12a1000000000000000000000000000000000"
    ));
    assert!(supported_sbx_version(
        "sbx version: v0.45.0-241-g1111111 1111111111111111111111111111111111111111"
    ));
    assert!(!supported_sbx_version(
        "sbx version: v0.44.9-999-g1111111 1111111111111111111111111111111111111111"
    ));
    assert!(supported_sbx_version(
        "sbx version: v0.45.1 1111111111111111111111111111111111111111"
    ));
    assert!(supported_sbx_version(
        "sbx version: v0.45.1-1-g1111111 1111111111111111111111111111111111111111"
    ));
    assert!(supported_sbx_version(
        "sbx version: v0.46.0 1111111111111111111111111111111111111111"
    ));
    assert!(supported_sbx_version(
        "sbx version: v0.46.0-1-g1111111 1111111111111111111111111111111111111111"
    ));
    assert!(supported_sbx_version(
        "sbx version: v1.0.0 1111111111111111111111111111111111111111"
    ));
    assert!(supported_sbx_version(
        "sbx version: v0.45.1-282-g290e5e792 290e5e7927d86336edeab1d90e8d14461944596f"
    ));
    assert!(!supported_sbx_version(
        "sbx version: v0.45.1 111111111111111111111111111111111111111"
    ));
    assert!(!supported_sbx_version(
        "sbx version: v0.45.1-1-g2222222 1111111111111111111111111111111111111111"
    ));
    assert!(!supported_sbx_version("not an sbx version"));
    assert!(!supported_sbx_version(
        "warning mentions v99.0.0 but no version record"
    ));
}

#[test]
fn supported_version_validation_probes_required_public_capabilities() {
    let runner = FakeRunner::with_outputs([
        stdout(b"sbx version: v0.45.0-240-g3fd12a1 3fd12a1000000000000000000000000000000000\n"),
        stdout(b"Create from a local sandbox kit\nreference.\n"),
    ]);
    StockSbx::new("/opt/homebrew/bin/sbx", runner.clone())
        .validate_supported_version()
        .unwrap();
    let calls = runner.runs.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].arguments, ["version"]);
    assert_eq!(calls[1].arguments, ["create", "--help"]);
}

#[test]
fn supported_version_validation_rejects_before_045() {
    let runner = FakeRunner::with_outputs([stdout(
        b"sbx version: v0.44.9 0000000000000000000000000000000000000000\n",
    )]);
    let error = StockSbx::new("/opt/homebrew/bin/sbx", runner)
        .validate_supported_version()
        .unwrap_err();
    assert!(matches!(error, SbxError::UnsupportedVersion { .. }));
}

#[derive(Default)]
struct FakeRunner {
    outputs: Mutex<VecDeque<CommandOutput>>,
    runs: Mutex<Vec<Invocation>>,
    spawns: Mutex<Vec<Invocation>>,
    spawn_failure: Mutex<Option<String>>,
    build_failure: Mutex<Option<CommandOutput>>,
    create_failure: Mutex<Option<CommandOutput>>,
    create_timeout: AtomicBool,
    inspect_timeout: AtomicBool,
    exec_timeout: AtomicBool,
    supervisor_frames: Arc<Mutex<Vec<shell_supervisor::Down>>>,
    spawn_states: Mutex<Vec<Arc<AtomicBool>>>,
    spawn_stdout: Mutex<VecDeque<Vec<u8>>>,
    spawn_inputs: Mutex<Vec<Arc<Mutex<Vec<u8>>>>>,
    pty_sizes: Mutex<Vec<TerminalSize>>,
    bad_shell_keepalive: AtomicBool,
    serve_ready_gate: Mutex<Option<Arc<ReadyGate>>>,
    /// Modeled stock inventory answered for `ls --json` (name -> UUID, status).
    inventory: Mutex<BTreeMap<String, (String, String)>>,
    ls_calls: AtomicU64,
    inventory_failure: AtomicBool,
}

/// In-process stand-in for `marsh --internal-supervisor`: acknowledges the
/// generation, answers pings, starts attempts (a shell exits 0 and is
/// cleaned in-band), acknowledges controls, and records every host frame.
fn fake_supervisor(
    frames: Arc<Mutex<Vec<shell_supervisor::Down>>>,
    wrong_generation: bool,
) -> io::Result<(
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
)> {
    use shell_supervisor::{Down, StartKind, Up, read_frame, write_frame};
    let (controller, mut guest) = std::os::unix::net::UnixStream::pair()?;
    let controller_input = controller.try_clone()?;
    std::thread::spawn(move || {
        let mut reader = guest.try_clone().unwrap();
        while let Ok((frame, _)) = read_frame::<Down>(&mut reader) {
            frames.lock().unwrap().push(frame.clone());
            let replies = match frame {
                Down::Hello { generation } => vec![Up::Ready {
                    generation: generation + u64::from(wrong_generation),
                }],
                Down::Ping { generation, nonce } => vec![Up::Pong { generation, nonce }],
                Down::Start { attempt, spec } => match spec.kind {
                    StartKind::Shell { .. } => vec![
                        Up::Started { attempt },
                        Up::OutputEof { attempt, stream: 1 },
                        Up::OutputEof { attempt, stream: 2 },
                        Up::Exited { attempt, code: 0 },
                        Up::Cleaned {
                            attempt,
                            error: None,
                        },
                    ],
                    StartKind::Child { .. } => vec![Up::Started { attempt }],
                },
                Down::Control { attempt, seq, .. } => vec![Up::Done {
                    attempt,
                    seq,
                    error: None,
                }],
                _ => Vec::new(),
            };
            for reply in replies {
                if write_frame(&mut guest, &reply, &[]).is_err() {
                    return;
                }
            }
        }
    });
    Ok((controller, controller_input))
}

#[derive(Default)]
struct ReadyGate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FlushFailWriter(Arc<Mutex<Vec<u8>>>);

impl Write for FlushFailWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "flush completion unknown",
        ))
    }
}

#[derive(Default)]
struct SlowInstallRunner {
    runs: Mutex<Vec<Invocation>>,
    copies: AtomicU64,
}

struct ConcurrentPreparationRunner {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
    overlapped: AtomicBool,
    manifest: String,
}

#[derive(Default)]
struct ConcurrentGrantRunner {
    mounted_vms: Mutex<BTreeSet<String>>,
    changed: Condvar,
    overlapped: AtomicBool,
}

struct BlockedGrantResetRunner {
    mount_state: Mutex<(bool, bool)>,
    changed: Condvar,
    runs: Mutex<Vec<Invocation>>,
    marker: Vec<u8>,
}

#[derive(Default)]
struct SerializedStockRunner {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
    max_active: AtomicU64,
    active: AtomicU64,
}

impl CommandRunner for SerializedStockRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "mount")
        {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            let mut state = self.state.lock().unwrap();
            state.0 += 1;
            self.changed.notify_all();
            while state.0 == 1 && !state.1 {
                state = self.changed.wait(state).unwrap();
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(ok())
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, _invocation: &Invocation) -> io::Result<Attachment> {
        Err(io::Error::other("unexpected attachment"))
    }
}

impl CommandRunner for BlockedGrantResetRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        if invocation.arguments == [OsString::from("ls"), OsString::from("--json")] {
            return Ok(running_inventory(&[&vm().name]));
        }
        self.runs.lock().unwrap().push(invocation.clone());
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "mount")
        {
            let mut state = self.mount_state.lock().unwrap();
            state.0 = true;
            self.changed.notify_all();
            while !state.1 {
                state = self.changed.wait(state).unwrap();
            }
            return Ok(ok());
        }
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "exec")
        {
            return Ok(stdout(&self.marker));
        }
        Ok(ok())
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, _invocation: &Invocation) -> io::Result<Attachment> {
        Err(io::Error::other("unexpected attachment"))
    }
}

impl CommandRunner for ConcurrentGrantRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "mount")
        {
            let vm = invocation.arguments[1].to_string_lossy().into_owned();
            let mut mounted = self.mounted_vms.lock().unwrap();
            mounted.insert(vm);
            self.changed.notify_all();
            let (mounted, _) = self
                .changed
                .wait_timeout_while(mounted, Duration::from_secs(1), |mounted| mounted.len() < 2)
                .unwrap();
            if mounted.len() == 2 {
                self.overlapped.store(true, Ordering::SeqCst);
            }
        }
        Ok(ok())
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, _invocation: &Invocation) -> io::Result<Attachment> {
        Err(io::Error::other("unexpected attachment"))
    }
}

impl ConcurrentPreparationRunner {
    fn rendezvous(&self, create: bool) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if create {
            state.0 = true;
        } else {
            state.1 = true;
        }
        self.changed.notify_all();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(1), |(created, built)| {
                !(*created && *built)
            })
            .unwrap();
        if !(state.0 && state.1) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "create and build did not overlap",
            ));
        }
        self.overlapped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl CommandRunner for ConcurrentPreparationRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        let command = invocation
            .arguments
            .first()
            .and_then(|argument| argument.to_str());
        if invocation.program == Path::new("docker") && command == Some("buildx") {
            self.rendezvous(false)?;
            for destination in invocation
                .arguments
                .iter()
                .filter_map(|argument| argument.to_str()?.strip_prefix("type=docker,dest="))
            {
                fs::write(destination, b"fake image archive")?;
            }
            let metadata_index = invocation
                .arguments
                .iter()
                .position(|argument| argument == "--metadata-file")
                .ok_or_else(|| io::Error::other("missing metadata destination"))?;
            fs::write(
                &invocation.arguments[metadata_index + 1],
                format!("{{\"containerimage.digest\":\"{}\"}}", self.manifest),
            )?;
            return Ok(ok());
        }
        match command {
            Some("create") => {
                self.rendezvous(true)?;
                Ok(ok())
            }
            Some("inspect") => Ok(local_inspect(&self.manifest)),
            // Stock inventory reflects the VM once its create has arrived.
            Some("ls") if self.state.lock().unwrap().0 => {
                Ok(running_inventory(&["marsh-kit-local-source"]))
            }
            Some("ls") => Ok(running_inventory(&[])),
            _ => Err(io::Error::other("unexpected command")),
        }
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, _invocation: &Invocation) -> io::Result<Attachment> {
        Err(io::Error::other("unexpected attached command"))
    }
}

impl CommandRunner for SlowInstallRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        self.runs.lock().unwrap().push(invocation.clone());
        let arguments = invocation
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        if arguments.first().is_some_and(|argument| argument == "cp") {
            self.copies.fetch_add(1, Ordering::SeqCst);
        }
        if arguments
            .iter()
            .any(|argument| argument == "/usr/local/libexec")
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(ok())
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment> {
        self.runs.lock().unwrap().push(invocation.clone());
        let exited = Arc::new(AtomicBool::new(false));
        let (controller, controller_input) = fake_supervisor(Arc::default(), false)?;
        Ok(Attachment {
            stdin: Box::new(controller_input),
            stdout: Box::new(controller),
            stderr: Box::new(Cursor::new(Vec::new())),
            process: Box::new(FakeProcess { exited }),
            control: no_attachment_control(),
        })
    }
}

impl FakeRunner {
    fn with_outputs(outputs: impl IntoIterator<Item = CommandOutput>) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.into_iter().collect()),
            ..Self::default()
        })
    }

    fn arguments(&self) -> Vec<Vec<String>> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .map(|invocation| {
                invocation
                    .arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }

    fn invocations(&self) -> Vec<Invocation> {
        self.runs.lock().unwrap().clone()
    }

    fn fail_next_spawn(&self, message: &str) {
        *self.spawn_failure.lock().unwrap() = Some(message.into());
    }

    fn fail_next_build(&self, message: &str) {
        *self.build_failure.lock().unwrap() = Some(failed(message));
    }

    fn fail_next_create(&self, message: &str) {
        *self.create_failure.lock().unwrap() = Some(failed(message));
    }

    fn timeout_next_create(&self) {
        self.create_timeout.store(true, Ordering::SeqCst);
    }

    fn exit_spawn(&self, index: usize) {
        self.spawn_states.lock().unwrap()[index].store(true, Ordering::SeqCst);
    }

    fn reject_next_shell_keepalive_readiness(&self) {
        self.bad_shell_keepalive.store(true, Ordering::SeqCst);
    }
}

impl FakeRunner {
    /// Model a stock VM this daemon created and recorded in its ownership map.
    fn seed_owned(&self, adapter: &StockSbx, name: &str, status: &str) -> String {
        let uuid = self.seed_present(name, status);
        adapter.ownership.record(name, &uuid).unwrap();
        uuid
    }

    /// Model a stock VM present under `name` (foreign unless recorded).
    fn seed_present(&self, name: &str, status: &str) -> String {
        let uuid = fake_uuid(name);
        self.inventory
            .lock()
            .unwrap()
            .insert(name.to_owned(), (uuid.clone(), status.to_owned()));
        uuid
    }

    fn present(&self, name: &str) -> bool {
        self.inventory.lock().unwrap().contains_key(name)
    }

    fn inventory_listing(&self) -> CommandOutput {
        self.ls_calls.fetch_add(1, Ordering::SeqCst);
        if self.inventory_failure.load(Ordering::SeqCst) {
            return failed("connection refused");
        }
        let rows = self
            .inventory
            .lock()
            .unwrap()
            .iter()
            .map(|(name, (id, status))| {
                serde_json::json!({"name": name, "id": id, "status": status, "agent": "shell"})
            })
            .collect::<Vec<_>>();
        stdout(&serde_json::to_vec(&serde_json::json!({ "sandboxes": rows })).unwrap())
    }

    fn apply_lifecycle(&self, arguments: &[String], output: &CommandOutput) {
        if !output.succeeded() {
            return;
        }
        let mut inventory = self.inventory.lock().unwrap();
        match arguments.first().map(String::as_str) {
            Some("create") => {
                if let Some(index) = arguments.iter().position(|argument| argument == "--name") {
                    let name = arguments[index + 1].clone();
                    inventory.insert(name.clone(), (fake_uuid(&name), "running".into()));
                }
            }
            Some("rm") => {
                let target = arguments.last().cloned().unwrap_or_default();
                inventory.retain(|name, (id, _)| *id != target && *name != target);
            }
            Some("stop") => {
                let target = arguments.last().cloned().unwrap_or_default();
                for (name, (id, status)) in inventory.iter_mut() {
                    if *id == target || *name == target {
                        *status = "stopped".into();
                    }
                }
            }
            _ => {}
        }
    }
}

/// One `sbx ls --json` document listing `names` as running VMs.
fn running_inventory(names: &[&str]) -> CommandOutput {
    let rows = names
        .iter()
        .map(|name| serde_json::json!({"name": name, "id": fake_uuid(name), "status": "running"}))
        .collect::<Vec<_>>();
    stdout(&serde_json::to_vec(&serde_json::json!({ "sandboxes": rows })).unwrap())
}

fn fake_uuid(name: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(name.as_bytes()));
    format!(
        "{}-{}-4{}-8{}-{}",
        &digest[..8],
        &digest[8..12],
        &digest[13..16],
        &digest[17..20],
        &digest[20..32]
    )
}

impl CommandRunner for FakeRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        if invocation.arguments == [OsString::from("ls"), OsString::from("--json")] {
            // A test may script an exact (possibly hostile) inventory document.
            let mut outputs = self.outputs.lock().unwrap();
            if outputs.front().is_some_and(|output| {
                serde_json::from_slice::<serde_json::Value>(&output.stdout)
                    .is_ok_and(|value| value.get("sandboxes").is_some())
            }) {
                self.ls_calls.fetch_add(1, Ordering::SeqCst);
                self.runs.lock().unwrap().push(invocation.clone());
                return Ok(outputs.pop_front().unwrap());
            }
            drop(outputs);
            return Ok(self.inventory_listing());
        }
        self.runs.lock().unwrap().push(invocation.clone());
        let is_build = invocation.program == Path::new("docker")
            && invocation
                .arguments
                .first()
                .is_some_and(|argument| argument == "buildx");
        if is_build && let Some(output) = self.build_failure.lock().unwrap().take() {
            return Ok(output);
        }
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "create")
            && let Some(output) = self.create_failure.lock().unwrap().take()
        {
            return Ok(output);
        }
        let output = {
            let mut outputs = self.outputs.lock().unwrap();
            // An outer inspect may overlap a concurrent build or inventory
            // call: hand it its scripted document regardless of pop order.
            let inspect = invocation
                .arguments
                .first()
                .is_some_and(|argument| argument == "inspect")
                .then(|| {
                    outputs.iter().position(|output| {
                        serde_json::from_slice::<serde_json::Value>(&output.stdout)
                            .is_ok_and(|value| value.get("image_digest").is_some())
                    })
                })
                .flatten();
            match inspect {
                Some(index) => outputs.remove(index),
                None => outputs.pop_front(),
            }
            .ok_or_else(|| io::Error::other("unexpected command"))?
        };
        let arguments = invocation
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        self.apply_lifecycle(&arguments, &output);
        if output.succeeded() && is_build {
            for destination in invocation.arguments.iter().filter_map(|argument| {
                let argument = argument.to_str()?;
                argument.strip_prefix("type=docker,dest=")
            }) {
                fs::write(destination, b"fake image archive")?;
            }
            if let Some(index) = invocation
                .arguments
                .iter()
                .position(|argument| argument == "--metadata-file")
            {
                fs::write(
                    &invocation.arguments[index + 1],
                    format!(
                        "{{\"containerimage.digest\":\"sha256:{}\"}}",
                        "f".repeat(64)
                    ),
                )?;
            }
        }
        Ok(output)
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "inspect")
            && self.inspect_timeout.swap(false, Ordering::SeqCst)
        {
            self.runs.lock().unwrap().push(invocation.clone());
            return Err(io::Error::new(io::ErrorKind::TimedOut, "inspect stalled"));
        }
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "exec")
            && self.exec_timeout.swap(false, Ordering::SeqCst)
        {
            self.runs.lock().unwrap().push(invocation.clone());
            return Err(io::Error::new(io::ErrorKind::TimedOut, "exec stalled"));
        }
        if invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "create")
            && self.create_timeout.swap(false, Ordering::SeqCst)
        {
            self.runs.lock().unwrap().push(invocation.clone());
            return Err(io::Error::new(io::ErrorKind::TimedOut, "create stalled"));
        }
        self.run(invocation)
    }

    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment> {
        self.spawns.lock().unwrap().push(invocation.clone());
        if let Some(message) = self.spawn_failure.lock().unwrap().take() {
            return Err(io::Error::other(message));
        }
        let exited = Arc::new(AtomicBool::new(invocation.arguments.ends_with(&[
            "docker".into(),
            "image".into(),
            "load".into(),
        ])));
        self.spawn_states.lock().unwrap().push(Arc::clone(&exited));
        if invocation
            .arguments
            .iter()
            .any(|argument| argument == "--serve")
        {
            let generation = invocation
                .arguments
                .last()
                .and_then(|argument| argument.to_str())
                .and_then(|argument| argument.parse::<u64>().ok())
                .ok_or_else(|| io::Error::other("missing test worker generation"))?;
            let (controller, mut worker) = std::os::unix::net::UnixStream::pair()?;
            let controller_input = controller.try_clone()?;
            let ready_gate = self.serve_ready_gate.lock().unwrap().clone();
            std::thread::spawn(move || {
                if let Some(gate) = ready_gate {
                    let mut state = gate.state.lock().unwrap();
                    state.0 = true;
                    gate.changed.notify_all();
                    while !state.1 {
                        state = gate.changed.wait(state).unwrap();
                    }
                }
                let _ = marsh_worker::write_frame(
                    &mut worker,
                    &marsh_worker::WorkerResponse::Ready { generation },
                );
                while let Ok(request) =
                    marsh_worker::read_frame::<marsh_worker::WorkerRequest>(&mut worker)
                {
                    if let marsh_worker::WorkerRequest::Ping { generation, nonce } = request {
                        let _ = marsh_worker::write_frame(
                            &mut worker,
                            &marsh_worker::WorkerResponse::Pong { generation, nonce },
                        );
                    }
                }
            });
            let stdin = Arc::new(Mutex::new(Vec::new()));
            self.spawn_inputs.lock().unwrap().push(stdin);
            return Ok(Attachment {
                stdin: Box::new(controller_input),
                stdout: Box::new(controller),
                stderr: Box::new(Cursor::new(Vec::new())),
                process: Box::new(FakeProcess { exited }),
                control: no_attachment_control(),
            });
        }
        if invocation
            .arguments
            .last()
            .is_some_and(|argument| argument == "--internal-supervisor")
        {
            let (controller, controller_input) = fake_supervisor(
                Arc::clone(&self.supervisor_frames),
                self.bad_shell_keepalive.swap(false, Ordering::SeqCst),
            )?;
            return Ok(Attachment {
                stdin: Box::new(controller_input),
                stdout: Box::new(controller),
                stderr: Box::new(Cursor::new(Vec::new())),
                process: Box::new(FakeProcess { exited }),
                control: no_attachment_control(),
            });
        }
        let stdout = self
            .spawn_stdout
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default();
        let stdin = Arc::new(Mutex::new(Vec::new()));
        self.spawn_inputs.lock().unwrap().push(Arc::clone(&stdin));
        Ok(Attachment {
            stdin: Box::new(CapturedWriter(stdin)),
            stdout: Box::new(Cursor::new(stdout)),
            stderr: Box::new(Cursor::new(Vec::new())),
            process: Box::new(FakeProcess { exited }),
            control: no_attachment_control(),
        })
    }

    fn spawn_pty_sized(
        &self,
        invocation: &Invocation,
        size: TerminalSize,
    ) -> io::Result<Attachment> {
        self.pty_sizes.lock().unwrap().push(size);
        self.spawn_attached(invocation)
    }
}

struct FakeProcess {
    exited: Arc<AtomicBool>,
}

impl AttachedProcess for FakeProcess {
    fn wait(&mut self) -> io::Result<i32> {
        Ok(0)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.exited.load(Ordering::SeqCst).then_some(0))
    }

    fn terminate(&mut self) -> io::Result<()> {
        self.exited.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct FlagReader {
    started: Arc<AtomicBool>,
    bytes: Cursor<Vec<u8>>,
}

impl io::Read for FlagReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.started.store(true, Ordering::SeqCst);
        self.bytes.read(buffer)
    }
}

struct PressureWriter {
    stdout_started: Arc<AtomicBool>,
    stderr_started: Arc<AtomicBool>,
    fail: bool,
}

impl io::Write for PressureWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.fail {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "transfer failed"));
        }
        for _ in 0..100 {
            if self.stdout_started.load(Ordering::SeqCst)
                && self.stderr_started.load(Ordering::SeqCst)
            {
                return Ok(buffer.len());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "output pipes were not drained while stdin was written",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct TrackingProcess {
    terminated: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
    complete_immediately: bool,
}

struct WedgedTerminationProcess {
    terminate_started: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl AttachedProcess for WedgedTerminationProcess {
    fn wait(&mut self) -> io::Result<i32> {
        panic!("bounded retained teardown must not call blocking wait")
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(None)
    }

    fn terminate(&mut self) -> io::Result<()> {
        self.terminate_started.store(true, Ordering::SeqCst);
        while !self.release.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
}

impl AttachedProcess for TrackingProcess {
    fn wait(&mut self) -> io::Result<i32> {
        self.reaped.store(true, Ordering::SeqCst);
        Ok(0)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if self.complete_immediately || self.terminated.load(Ordering::SeqCst) {
            self.reaped.store(true, Ordering::SeqCst);
            Ok(Some(0))
        } else {
            Ok(None)
        }
    }

    fn terminate(&mut self) -> io::Result<()> {
        self.terminated.store(true, Ordering::SeqCst);
        Ok(())
    }
}

struct ArchivePressureRunner {
    fail_write: bool,
    terminated: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
}

struct UntilTerminated {
    terminated: Arc<AtomicBool>,
}

impl io::Read for UntilTerminated {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        while !self.terminated.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(0)
    }
}

impl io::Write for UntilTerminated {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        while !self.terminated.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "terminated"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BlockingLoadRunner {
    terminated: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
}

impl CommandRunner for BlockingLoadRunner {
    fn run(&self, _invocation: &Invocation) -> io::Result<CommandOutput> {
        Err(io::Error::other("unexpected synchronous command"))
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, _invocation: &Invocation) -> io::Result<Attachment> {
        Ok(Attachment {
            stdin: Box::new(UntilTerminated {
                terminated: Arc::clone(&self.terminated),
            }),
            stdout: Box::new(UntilTerminated {
                terminated: Arc::clone(&self.terminated),
            }),
            stderr: Box::new(UntilTerminated {
                terminated: Arc::clone(&self.terminated),
            }),
            process: Box::new(TrackingProcess {
                terminated: Arc::clone(&self.terminated),
                reaped: Arc::clone(&self.reaped),
                complete_immediately: false,
            }),
            control: no_attachment_control(),
        })
    }
}

impl CommandRunner for ArchivePressureRunner {
    fn run(&self, _invocation: &Invocation) -> io::Result<CommandOutput> {
        Err(io::Error::other("unexpected synchronous command"))
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, _invocation: &Invocation) -> io::Result<Attachment> {
        let stdout_started = Arc::new(AtomicBool::new(false));
        let stderr_started = Arc::new(AtomicBool::new(false));
        Ok(Attachment {
            stdin: Box::new(PressureWriter {
                stdout_started: Arc::clone(&stdout_started),
                stderr_started: Arc::clone(&stderr_started),
                fail: self.fail_write,
            }),
            stdout: Box::new(FlagReader {
                started: stdout_started,
                bytes: Cursor::new(vec![b'o'; 256 * 1024]),
            }),
            stderr: Box::new(FlagReader {
                started: stderr_started,
                bytes: Cursor::new(vec![b'e'; 256 * 1024]),
            }),
            process: Box::new(TrackingProcess {
                terminated: Arc::clone(&self.terminated),
                reaped: Arc::clone(&self.reaped),
                complete_immediately: true,
            }),
            control: no_attachment_control(),
        })
    }
}

fn ok() -> CommandOutput {
    CommandOutput {
        exit_code: Some(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

fn missing() -> CommandOutput {
    CommandOutput {
        exit_code: Some(1),
        stdout: Vec::new(),
        stderr: b"sandbox not found".to_vec(),
    }
}

fn stdout(bytes: &[u8]) -> CommandOutput {
    CommandOutput {
        exit_code: Some(0),
        stdout: bytes.to_vec(),
        stderr: Vec::new(),
    }
}

fn failed(stderr: &str) -> CommandOutput {
    CommandOutput {
        exit_code: Some(1),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

fn local_inspect(image: &str) -> CommandOutput {
    stdout(
        format!("{{\"name\":\"fixture\",\"status\":\"running\",\"image_digest\":\"{image}\"}}\n")
            .as_bytes(),
    )
}

fn local_kit_source() -> PathBuf {
    let source = test_directory("local-v3-kit");
    fs::write(
        source.join("fixture.yaml"),
        "# syntax=docker/sandbox-kit:3\nschemaVersion: \"3\"\nkind: workload\ndockerfile: ./fixture.dockerfile\n",
    )
    .unwrap();
    fs::write(source.join("fixture.dockerfile"), "FROM scratch\n").unwrap();
    fs::canonicalize(source).unwrap()
}

fn local_vm(source: &Path, home: PathBuf, image: &str) -> ReadyKitVm {
    ReadyKitVm {
        name: "marsh-kit-local-source".into(),
        kit_ref: NativeKitRef::local_v3_source(source.to_owned())
            .unwrap()
            .identity()
            .into(),
        lifecycle_workspace: home,
        cold_started: false,
        job_image: OciImage::parse(image.to_owned()).unwrap(),
        worker_binary: "/tmp/unused-marsh-worker".into(),
    }
}

/// create, copy shell, one root setup exec.
fn cold_shell_outputs() -> Vec<CommandOutput> {
    vec![ok(), ok(), ok()]
}

fn existing_shell_outputs(spec: &ShellVmSpec) -> Vec<CommandOutput> {
    let marker = serde_json::to_vec(&StockSbx::marker(&format!(
        "shell:{}:{}",
        spec.name,
        spec.image.as_str()
    )))
    .unwrap();
    vec![stdout(&marker), ok(), ok()]
}

fn shell_spec(shell_binary: PathBuf) -> ShellVmSpec {
    ShellVmSpec {
        name: "marsh-shell-cache".into(),
        image: OciImage::parse(format!("dhi.example/shell@sha256:{}", "e".repeat(64))).unwrap(),
        shell_binary,
        user: ShellUser {
            name: "alice".into(),
            uid: 501,
            gid: 20,
            home: "/Users/alice".into(),
        },
    }
}

fn artifact() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let path = canonical_temp_dir().join(format!(
        "marsh-worker-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::write(path.join("marsh-byte-exec-linux-arm64"), b"fixture helper").unwrap();
    fs::write(path.join("marsh-local-linux-arm64"), b"job artifact").unwrap();
    let worker = path.join("marsh-worker-linux-arm64");
    fs::write(&worker, b"worker").unwrap();
    worker
}

fn workload_ref() -> NativeKitRef {
    NativeKitRef::immutable_oci(format!(
        "registry.example/sbx-kit-fixture@sha256:{}",
        "d".repeat(64)
    ))
    .unwrap()
}

fn selected_home() -> PathBuf {
    let path = canonical_temp_dir().join(format!(
        "marsh-kit-home-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn test_directory(label: &str) -> PathBuf {
    let path = canonical_temp_dir().join(format!(
        "marsh-{label}-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    path
}

fn grant(source: PathBuf, target: &str, access: MountAccess) -> AdmittedHostGrant {
    AdmittedHostGrant::open(source, target.into(), access).unwrap()
}

fn retained_test_spec() -> marsh_contracts::JobSpec {
    marsh_contracts::JobSpec {
        image: marsh_contracts::OciImage::parse(format!(
            "registry.example/job@sha256:{}",
            "d".repeat(64)
        ))
        .unwrap(),
        argv: vec![b"true".to_vec()],
        identity: marsh_contracts::JobIdentity { uid: 501, gid: 20 },
        exported_environment: BTreeMap::new(),
        session_environment: BTreeMap::from([
            ("HOME".into(), "/Users/alice".into()),
            ("LOGNAME".into(), "alice".into()),
            ("MARSH_SELECTED_HOME".into(), "/Users/alice".into()),
            ("USER".into(), "alice".into()),
        ]),
        working_directory: "/Users/alice".into(),
        mounts: Vec::new(),
        resources: marsh_contracts::JobResources {
            cpu_millis: 1000,
            memory_bytes: 1024 * 1024,
            pids: 8,
            writable_bytes: 1024 * 1024,
            output_bytes: 1024 * 1024,
            wall_seconds: 10,
        },
        terminal: false,
        terminal_size: None,
        split_capability: None,
        capability: None,
    }
}

fn stock_grant_effects(runner: &FakeRunner) -> usize {
    runner
        .arguments()
        .into_iter()
        .filter(|arguments| {
            arguments
                .first()
                .is_some_and(|argument| matches!(argument.as_str(), "mount" | "umount"))
        })
        .count()
}

fn shell_grants(project: &Path, selected_home: &Path) -> Vec<AdmittedHostGrant> {
    vec![
        grant(
            project.to_owned(),
            project.to_str().unwrap(),
            MountAccess::ReadWrite,
        ),
        grant(
            selected_home.to_owned(),
            "/Users/alice",
            MountAccess::ReadWrite,
        ),
    ]
}

fn vm() -> ReadyKitVm {
    ReadyKitVm {
        name: "marsh-kit-fixture-abc".into(),
        kit_ref: workload_ref().identity().into(),
        lifecycle_workspace: "/Users/alice/.marsh/kit-lifecycle/fixture".into(),
        cold_started: false,
        job_image: workload_ref().workload_image().unwrap().clone(),
        worker_binary: "/tmp/unused-marsh-worker".into(),
    }
}

#[test]
fn cold_vm_uses_native_workload_and_installs_worker() {
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        stdout(b"0:0:700\n"),
    ]);
    let adapter = StockSbx::new("/opt/homebrew/bin/sbx", runner.clone());
    let worker = artifact();
    let home = selected_home();
    let ready = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: vm().name,
            worker_binary: worker.clone(),
            workload_kit: workload_ref(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap();
    assert!(ready.cold_started);
    // Absent name: create, then record the newly observed UUID.
    assert_eq!(
        adapter.ownership.uuid("marsh-kit-fixture-abc"),
        Some(fake_uuid("marsh-kit-fixture-abc"))
    );
    let commands = runner.arguments();
    assert_eq!(
        commands[0],
        vec![
            "create".to_owned(),
            "--quiet".to_owned(),
            "--name".to_owned(),
            "marsh-kit-fixture-abc".to_owned(),
            "--pull".to_owned(),
            "missing".to_owned(),
            "--skills".to_owned(),
            "off".to_owned(),
            "-e".to_owned(),
            format!("MARSH_SELECTED_HOME={}", home.display()),
            format!(
                "oci://registry.example/sbx-kit-fixture@sha256:{}",
                "d".repeat(64)
            ),
            home.display().to_string(),
        ]
    );
    assert_eq!(
        commands[1][..4],
        ["exec", "-u", "root", "-e"],
        "ownership marker first"
    );
    // Worker, byte-exec helper, and the mandatory static job artifact.
    assert_eq!(commands[2][0], "cp");
    assert_eq!(commands[3][0], "cp");
    assert_eq!(commands[4][0], "cp");
    // One bounded exec waits for Docker, then installs the worker.
    assert!(commands[5].iter().any(|argument| argument == WORKER_PATH));
    assert!(
        commands[5]
            .iter()
            .any(|argument| argument.contains("docker info"))
    );
    fs::remove_file(worker).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn cold_vm_marker_failure_removes_only_incomplete_vm() {
    let marker_failure = CommandOutput {
        exit_code: Some(1),
        stdout: Vec::new(),
        stderr: b"marker write failed".to_vec(),
    };
    let runner = FakeRunner::with_outputs([ok(), marker_failure, ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let worker = artifact();
    let home = selected_home();
    let error = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: vm().name,
            worker_binary: worker.clone(),
            workload_kit: workload_ref(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap_err();
    assert!(matches!(error, SbxError::CommandFailed { .. }));
    // Only the VM this daemon just created is removed, by its fenced name.
    assert_eq!(
        runner.arguments().last().unwrap(),
        &["rm", "--force", "marsh-kit-fixture-abc"]
    );
    assert!(!runner.present("marsh-kit-fixture-abc"));
    assert_eq!(adapter.ownership.uuid("marsh-kit-fixture-abc"), None);
    fs::remove_file(worker).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn cached_kit_revalidation_rejects_lost_lease_and_wrong_owned_identity() {
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        stdout(b"0:0:700\n"),
    ]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let worker = artifact();
    let home = selected_home();
    let spec = KitVmSpec {
        name: vm().name,
        worker_binary: worker.clone(),
        workload_kit: workload_ref(),
        lifecycle_workspace: home.clone(),
    };
    let ready = adapter.ensure_kit_vm(&spec).unwrap();
    let mut foreign = ready.clone();
    foreign.kit_ref = format!("registry.example/foreign@sha256:{}", "a".repeat(64));
    assert!(!adapter.revalidate_cached_kit(&spec, &foreign).unwrap());

    runner.exit_spawn(0);
    assert!(!adapter.revalidate_cached_kit(&spec, &ready).unwrap());
    assert!(adapter.worker_leases.lock().unwrap().is_empty());
    fs::remove_file(worker).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn quarantine_stop_waits_for_an_inflight_retained_start_frame() {
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        stdout(b"0:0:700\n"),
        ok(),
    ]);
    let adapter = Arc::new(StockSbx::new("sbx", runner));
    let worker = artifact();
    let home = selected_home();
    let ready = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: vm().name,
            worker_binary: worker.clone(),
            workload_kit: workload_ref(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap();
    let generation = adapter.worker_generations.lock().unwrap()[&ready.name];
    let lease = adapter.worker_leases.lock().unwrap()[&ready.name].clone();
    let input = lease.input.lock().unwrap();
    let grants = PreparedGrants {
        vm: ready.name.clone(),
        attempt: "attempt-start-fence".into(),
        worker_generation: generation,
        mounts: Vec::new(),
    };
    let spec = retained_test_spec();
    let launch_adapter = Arc::clone(&adapter);
    let launch_vm = ready.clone();
    let (launch_send, launch_receive) = mpsc::channel();
    let launch = thread::spawn(move || {
        launch_send
            .send(launch_adapter.launch_worker(&launch_vm, &grants, "attempt-start-fence", spec))
            .unwrap();
    });
    while !lease
        .routes
        .lock()
        .unwrap()
        .contains_key("attempt-start-fence")
    {
        thread::yield_now();
    }

    let stop_adapter = Arc::clone(&adapter);
    let stop_vm = ready.clone();
    let (stop_send, stop_receive) = mpsc::channel();
    let stop = thread::spawn(move || {
        stop_send
            .send(stop_adapter.stop_quarantined(&stop_vm))
            .unwrap();
    });
    assert!(matches!(
        stop_receive.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(input);

    drop(
        launch_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap(),
    );
    launch.join().unwrap();
    stop_receive
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    stop.join().unwrap();
    assert!(matches!(
        adapter.reject_quarantined(&ready.name),
        Err(SbxError::QuarantinedVm(_))
    ));
    assert!(
        !adapter
            .worker_generations
            .lock()
            .unwrap()
            .contains_key(&ready.name)
    );
    fs::remove_file(worker).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn ambiguous_retained_start_write_quarantines_and_stops_exact_vm() {
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        stdout(b"0:0:700\n"),
        ok(),
    ]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let worker_binary = artifact();
    let home = selected_home();
    let ready = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: vm().name,
            worker_binary: worker_binary.clone(),
            workload_kit: workload_ref(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap();
    let generation = adapter.worker_generations.lock().unwrap()[&ready.name];
    let lease = adapter.worker_leases.lock().unwrap()[&ready.name].clone();
    let accepted = Arc::new(Mutex::new(Vec::new()));
    *lease.input.lock().unwrap() = Box::new(FlushFailWriter(Arc::clone(&accepted)));
    let grants = PreparedGrants {
        vm: ready.name.clone(),
        attempt: "attempt-ambiguous-start".into(),
        worker_generation: generation,
        mounts: Vec::new(),
    };

    let Err(error) = adapter.launch_worker(
        &ready,
        &grants,
        "attempt-ambiguous-start",
        retained_test_spec(),
    ) else {
        panic!("ambiguous Start completion must fail closed");
    };

    assert!(matches!(
        error,
        SbxError::AmbiguousWorkerStart { ref vm, ref detail }
            if vm == &ready.name && detail.contains("flush completion unknown")
    ));
    let request = marsh_worker::read_frame::<marsh_worker::WorkerRequest>(&mut Cursor::new(
        accepted.lock().unwrap().clone(),
    ))
    .unwrap();
    assert!(matches!(
        request,
        marsh_worker::WorkerRequest::Start { attempt, .. }
            if attempt == "attempt-ambiguous-start"
    ));
    assert!(matches!(
        adapter.reject_quarantined(&ready.name),
        Err(SbxError::QuarantinedVm(_))
    ));
    assert!(
        !adapter
            .worker_leases
            .lock()
            .unwrap()
            .contains_key(&ready.name)
    );
    assert!(
        !adapter
            .worker_generations
            .lock()
            .unwrap()
            .contains_key(&ready.name)
    );
    assert_eq!(
        runner.arguments().last().unwrap(),
        &["stop", "marsh-kit-fixture-abc"]
    );
    fs::remove_file(worker_binary).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn quarantine_stop_drains_worker_initialization_before_lease_publication() {
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        stdout(b"0:0:700\n"),
        ok(),
    ]);
    let ready_gate = Arc::new(ReadyGate::default());
    *runner.serve_ready_gate.lock().unwrap() = Some(Arc::clone(&ready_gate));
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let worker = artifact();
    let home = selected_home();
    let spec = KitVmSpec {
        name: vm().name,
        worker_binary: worker.clone(),
        workload_kit: workload_ref(),
        lifecycle_workspace: home.clone(),
    };
    let ensure_adapter = Arc::clone(&adapter);
    let ensure_spec = spec.clone();
    let ensure = thread::spawn(move || ensure_adapter.ensure_kit_vm(&ensure_spec));
    {
        let mut state = ready_gate.state.lock().unwrap();
        while !state.0 {
            state = ready_gate.changed.wait(state).unwrap();
        }
    }

    let stop_adapter = Arc::clone(&adapter);
    let stop_vm = vm();
    let (stop_send, stop_receive) = mpsc::channel();
    let stop = thread::spawn(move || {
        stop_send
            .send(stop_adapter.stop_quarantined(&stop_vm))
            .unwrap();
    });
    while adapter.quarantined_workers.lock().unwrap().is_empty() {
        thread::yield_now();
    }
    assert!(matches!(
        stop_receive.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    {
        let mut state = ready_gate.state.lock().unwrap();
        state.1 = true;
        ready_gate.changed.notify_all();
    }

    assert!(matches!(
        ensure.join().unwrap(),
        Err(SbxError::QuarantinedVm(_))
    ));
    stop_receive
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    stop.join().unwrap();
    assert!(adapter.worker_leases.lock().unwrap().is_empty());
    assert!(adapter.worker_generations.lock().unwrap().is_empty());
    let effects = runner.arguments().len();
    assert!(matches!(
        adapter.ensure_kit_vm(&spec),
        Err(SbxError::QuarantinedVm(_))
    ));
    assert_eq!(runner.arguments().len(), effects);
    fs::remove_file(worker).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn three_pinned_warm_preparations_and_starts_share_one_vm_without_stock_effects() {
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        stdout(b"0:0:700\n"),
        ok(),
        ok(),
    ]);
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let worker = artifact();
    let home = selected_home();
    let ready = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: vm().name,
            worker_binary: worker.clone(),
            workload_kit: workload_ref(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap();
    let source = test_directory("three-pinned-warm-starts");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    adapter
        .prepare_session_grants(&ready, "session-warm", std::slice::from_ref(&admitted))
        .unwrap();
    let before = stock_grant_effects(&runner);
    let barrier = Arc::new(Barrier::new(4));
    let tasks = (0..3)
        .map(|sequence| {
            let adapter = Arc::clone(&adapter);
            let ready = ready.clone();
            let admitted = admitted.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let attempt = format!("attempt-warm-{sequence}");
                barrier.wait();
                let grants = adapter
                    .prepare_job_grants(
                        &ready,
                        "session-warm",
                        &attempt,
                        std::slice::from_ref(&admitted),
                    )
                    .unwrap();
                let channel = adapter
                    .launch_worker(&ready, &grants, &attempt, retained_test_spec())
                    .unwrap();
                (grants, channel)
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let mut active = tasks
        .into_iter()
        .map(|task| task.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(stock_grant_effects(&runner), before);
    let lease = adapter.worker_leases.lock().unwrap()[&ready.name].clone();
    assert_eq!(lease.active.load(Ordering::SeqCst), 3);
    for (grants, channel) in active.drain(..) {
        drop(channel);
        adapter.revoke_grants(&grants).unwrap();
    }
    assert_eq!(stock_grant_effects(&runner), before);
    adapter
        .release_session_grants(&ready.name, "session-warm")
        .unwrap();
    assert!(adapter.grant_transition_locks.lock().unwrap().is_empty());
    assert!(adapter.grant_vm_transition_locks.lock().unwrap().is_empty());
    assert!(
        adapter
            .grant_stock_operation_locks
            .lock()
            .unwrap()
            .is_empty()
    );
    fs::remove_dir(source).unwrap();
    fs::remove_file(worker).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn ambiguous_inventory_failure_never_creates_a_vm() {
    let runner = FakeRunner::with_outputs([]);
    runner.inventory_failure.store(true, Ordering::SeqCst);
    let adapter = StockSbx::new("sbx", runner.clone());
    let worker = artifact();
    let home = selected_home();
    let error = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: vm().name,
            worker_binary: worker.clone(),
            workload_kit: workload_ref(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap_err();
    assert!(matches!(
        error,
        SbxError::CommandFailed {
            operation: "list stock VMs",
            ..
        }
    ));
    assert!(runner.arguments().is_empty(), "no create without inventory");
    assert!(adapter.ownership.cached_view().is_none());
    fs::remove_file(worker).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn reset_removes_only_the_exact_owned_kit_vm_and_clears_generation_state() {
    let home = selected_home();
    let spec = KitVmSpec {
        name: vm().name,
        worker_binary: "/tmp/unused-worker".into(),
        workload_kit: workload_ref(),
        lifecycle_workspace: home.clone(),
    };
    let lifecycle_identity = selected_home_identity(&home).unwrap();
    let marker = serde_json::to_vec(&StockSbx::marker(&format!(
        "kit:{}:{lifecycle_identity}",
        spec.workload_kit.identity()
    )))
    .unwrap();
    let runner = FakeRunner::with_outputs([stdout(&marker), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    runner.seed_owned(&adapter, &spec.name, "running");
    adapter
        .worker_generations
        .lock()
        .unwrap()
        .insert(spec.name.clone(), 7);
    adapter
        .quarantined_workers
        .lock()
        .unwrap()
        .insert(spec.name.clone());

    assert!(adapter.reset_kit_vm(&spec).unwrap());
    assert!(adapter.worker_generations.lock().unwrap().is_empty());
    assert!(adapter.quarantined_workers.lock().unwrap().is_empty());
    assert_eq!(
        runner.arguments().last().unwrap(),
        &["rm", "--force", spec.name.as_str()]
    );
    assert!(!runner.present(&spec.name));
    assert_eq!(adapter.ownership.uuid(&spec.name), None);
    fs::remove_dir(home).unwrap();
}

#[test]
fn reset_rejects_foreign_vm_without_removing_it() {
    let home = selected_home();
    let spec = KitVmSpec {
        name: vm().name,
        worker_binary: "/tmp/unused-worker".into(),
        workload_kit: workload_ref(),
        lifecycle_workspace: home.clone(),
    };
    // Present under our name but never recorded by this daemon: foreign.
    let runner = FakeRunner::with_outputs([]);
    let adapter = StockSbx::new("sbx", runner.clone());
    runner.seed_present(&spec.name, "running");

    assert!(matches!(
        adapter.reset_kit_vm(&spec),
        Err(SbxError::ForeignVm(_))
    ));
    assert!(
        !runner
            .arguments()
            .iter()
            .any(|arguments| arguments.first().is_some_and(|arg| arg == "rm"))
    );
    fs::remove_dir(home).unwrap();
}

#[test]
fn reset_rejects_an_exact_vm_while_a_session_grant_is_pinned() {
    let home = selected_home();
    let spec = KitVmSpec {
        name: vm().name,
        worker_binary: "/tmp/unused-worker".into(),
        workload_kit: workload_ref(),
        lifecycle_workspace: home.clone(),
    };
    let lifecycle_identity = selected_home_identity(&home).unwrap();
    let marker = serde_json::to_vec(&StockSbx::marker(&format!(
        "kit:{}:{lifecycle_identity}",
        spec.workload_kit.identity()
    )))
    .unwrap();
    let runner = FakeRunner::with_outputs([ok(), stdout(&marker), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    runner.seed_owned(&adapter, &spec.name, "running");
    let source = test_directory("reset-pinned-session-grant");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    adapter
        .prepare_session_grants(&vm(), "session-one", std::slice::from_ref(&admitted))
        .unwrap();

    assert!(matches!(
        adapter.reset_kit_vm(&spec),
        Err(SbxError::ActiveWorkerState(_))
    ));
    assert!(
        !runner
            .arguments()
            .iter()
            .any(|arguments| { arguments.first().is_some_and(|argument| argument == "rm") })
    );
    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    fs::remove_dir(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn reset_waits_for_inflight_mount_and_then_observes_its_reference() {
    let home = selected_home();
    let spec = KitVmSpec {
        name: vm().name,
        worker_binary: "/tmp/unused-worker".into(),
        workload_kit: workload_ref(),
        lifecycle_workspace: home.clone(),
    };
    let lifecycle_identity = selected_home_identity(&home).unwrap();
    let marker = serde_json::to_vec(&StockSbx::marker(&format!(
        "kit:{}:{lifecycle_identity}",
        spec.workload_kit.identity()
    )))
    .unwrap();
    let runner = Arc::new(BlockedGrantResetRunner {
        mount_state: Mutex::new((false, false)),
        changed: Condvar::new(),
        runs: Mutex::new(Vec::new()),
        marker,
    });
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    adapter
        .ownership
        .record(&spec.name, &fake_uuid(&spec.name))
        .unwrap();
    let source = test_directory("reset-inflight-grant");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    let grant_adapter = Arc::clone(&adapter);
    let grant_task = thread::spawn(move || {
        grant_adapter.prepare_session_grants(&vm(), "session-one", &[admitted])
    });
    {
        let mut state = runner.mount_state.lock().unwrap();
        while !state.0 {
            state = runner.changed.wait(state).unwrap();
        }
    }
    let reset_adapter = Arc::clone(&adapter);
    let (reset_send, reset_receive) = mpsc::channel();
    let reset_task = thread::spawn(move || {
        let _ = reset_send.send(reset_adapter.reset_kit_vm(&spec));
    });
    assert!(matches!(
        reset_receive.recv_timeout(Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    {
        let mut state = runner.mount_state.lock().unwrap();
        state.1 = true;
        runner.changed.notify_all();
    }
    grant_task.join().unwrap().unwrap();
    assert!(matches!(
        reset_receive.recv_timeout(Duration::from_secs(1)).unwrap(),
        Err(SbxError::ActiveWorkerState(_))
    ));
    reset_task.join().unwrap();
    assert!(
        !runner
            .runs
            .lock()
            .unwrap()
            .iter()
            .any(|invocation| invocation.arguments.first().is_some_and(|arg| arg == "rm"))
    );
    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    fs::remove_dir(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn foreign_existing_name_fails_closed() {
    let runner = FakeRunner::with_outputs([]);
    runner.seed_present(&vm().name, "running");
    let adapter = StockSbx::new("sbx", runner.clone());
    let worker = artifact();
    let home = selected_home();
    let error = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: vm().name.clone(),
            worker_binary: worker.clone(),
            workload_kit: workload_ref(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap_err();
    assert!(matches!(error, SbxError::ForeignVm(_)));
    assert!(
        runner.arguments().is_empty(),
        "a foreign VM is never touched"
    );
    assert!(runner.present(&vm().name));
    fs::remove_file(worker).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn grants_use_opaque_vm_sources_and_natural_targets() {
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let project = canonical_temp_dir().join(format!("marsh-project-{}", std::process::id()));
    let home = canonical_temp_dir().join(format!("marsh-home-{}", std::process::id()));
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&home).unwrap();
    let prepared = adapter
        .prepare_grants(
            &vm(),
            "attempt_01",
            &[
                grant(
                    project.clone(),
                    "/Users/alice/project",
                    MountAccess::ReadWrite,
                ),
                grant(home.clone(), "/Users/alice", MountAccess::ReadWrite),
            ],
        )
        .unwrap();
    let mounts = prepared.job_mounts();
    assert_eq!(mounts[0].target, Path::new("/Users/alice/project"));
    assert_eq!(mounts[1].target, Path::new("/Users/alice"));
    assert_eq!(mounts[0].access, MountAccess::ReadWrite);
    assert_eq!(mounts[1].access, MountAccess::ReadWrite);
    assert!(
        mounts
            .iter()
            .all(|mount| mount.source.starts_with("/run/marsh/grants/shared"))
    );
    assert_ne!(mounts[0].source, mounts[1].source);
    let commands = runner.arguments();
    assert!(
        commands
            .iter()
            .all(|command| command[0..2] == ["mount", "marsh-kit-fixture-abc"])
    );
    assert!(
        commands
            .iter()
            .all(|command| command[2].starts_with(".:/run/marsh/grants/shared/"))
    );
    let invocations = runner.invocations();
    assert!(
        invocations
            .iter()
            .any(|invocation| invocation.working_directory.as_deref() == Some(project.as_path()))
    );
    assert!(
        invocations
            .iter()
            .any(|invocation| invocation.working_directory.as_deref() == Some(home.as_path()))
    );
    assert_eq!(commands.len(), 2);
    fs::remove_dir(project).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn admitted_grant_rejects_symlink_and_directory_replacements_before_mount() {
    for replace_with_symlink in [true, false] {
        let root = test_directory("admitted-grant-swap");
        let source = root.join("project");
        let admitted_source = root.join("admitted-project");
        let broader = root.join("broader");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&broader).unwrap();
        let grant = grant(
            source.clone(),
            "/Users/alice/project",
            MountAccess::ReadWrite,
        );
        let admitted_metadata = fs::metadata(&source).unwrap();
        let admitted_identity = (admitted_metadata.dev(), admitted_metadata.ino());
        assert_eq!(grant.source_identity(), admitted_identity);
        fs::rename(&source, &admitted_source).unwrap();
        if replace_with_symlink {
            std::os::unix::fs::symlink(&broader, &source).unwrap();
        } else {
            fs::create_dir(&source).unwrap();
        }
        assert_eq!(grant.source_identity(), admitted_identity);
        let replacement_metadata = fs::metadata(&source).unwrap();
        assert_ne!(
            grant.source_identity(),
            (replacement_metadata.dev(), replacement_metadata.ino())
        );
        let runner = Arc::new(FakeRunner::default());
        let adapter = StockSbx::new("sbx", runner.clone());

        let error = adapter
            .prepare_grants(&vm(), "attempt-swapped", &[grant])
            .unwrap_err();

        assert!(matches!(
            error,
            SbxError::UnsafePath(path) | SbxError::SourceChanged(path) if path == source
        ));
        assert!(runner.arguments().is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn admitted_shell_mount_rejects_replacement_before_mount() {
    let root = test_directory("admitted-shell-swap");
    let project = root.join("project");
    let selected = root.join("selected");
    let admitted_project = root.join("admitted-project");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&selected).unwrap();
    let grants = shell_grants(&project, &selected);
    fs::rename(&project, admitted_project).unwrap();
    fs::create_dir(&project).unwrap();
    let runner = Arc::new(FakeRunner::default());
    let adapter = StockSbx::new("sbx", runner.clone());
    let shell = ReadyShellVm {
        name: "marsh-shell-user".into(),
        user: ShellUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/Users/alice".into(),
        },
        cold_started: false,
    };
    runner.seed_owned(&adapter, &shell.name, "running");

    assert!(matches!(
        adapter.prepare_shell_mounts(&shell, &grants),
        Err(SbxError::SourceChanged(path)) if path == project
    ));
    assert!(runner.arguments().is_empty());
    drop(grants);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn grant_preparation_never_copies_lifecycle_or_agent_home_into_selected_home() {
    let runner = FakeRunner::with_outputs([ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let lifecycle = test_directory("lifecycle-canary");
    let home = selected_home();
    fs::write(lifecycle.join("credential-file"), b"must-stay-private").unwrap();
    fs::write(lifecycle.join("agent-home-token"), b"must-stay-private").unwrap();

    let mut ready = vm();
    ready.lifecycle_workspace = lifecycle.clone();
    let prepared = adapter
        .prepare_grants(
            &ready,
            "attempt_canary",
            &[grant(home.clone(), "/Users/alice", MountAccess::ReadWrite)],
        )
        .unwrap();

    assert_eq!(prepared.job_mounts().len(), 1);
    assert!(!home.join("credential-file").exists());
    assert!(!home.join("agent-home-token").exists());
    let arguments = runner.arguments();
    assert!(!arguments.iter().flatten().any(|arg| arg == "/usr/bin/tar"));
    assert!(!arguments.iter().flatten().any(|arg| arg == "/home/agent"));
    assert!(
        !arguments
            .iter()
            .flatten()
            .any(|arg| arg == lifecycle.to_str().unwrap())
    );
    fs::remove_dir_all(lifecycle).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn final_grant_release_retains_the_warm_mount_and_reuses_it() {
    let runner = FakeRunner::with_outputs([ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let root = test_directory("grant-retained");
    let grants = [grant(
        root.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    )];
    let prepared = adapter
        .prepare_grants(&vm(), "attempt-one", &grants)
        .unwrap();
    adapter.revoke_grants(&prepared).unwrap();
    // A second attempt on the warm VM reuses the retained mount: no stock call.
    let again = adapter
        .prepare_grants(&vm(), "attempt-two", &grants)
        .unwrap();
    adapter.revoke_grants(&again).unwrap();
    let commands = runner.arguments();
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert_eq!(commands[0].first().map(String::as_str), Some("mount"));
    fs::remove_dir(root).unwrap();
}

#[test]
fn partial_mount_failure_reports_uncertain_rollback_and_preserves_cause() {
    let failed_mount = CommandOutput {
        exit_code: Some(1),
        stdout: Vec::new(),
        stderr: b"second mount rejected".to_vec(),
    };
    let busy = || CommandOutput {
        exit_code: Some(1),
        stdout: Vec::new(),
        stderr: b"first mount remains busy".to_vec(),
    };
    let runner = FakeRunner::with_outputs([ok(), failed_mount, busy()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let first = test_directory("partial-grant-first");
    let second = test_directory("partial-grant-second");
    let error = adapter
        .prepare_grants(
            &vm(),
            "attempt-partial",
            &[
                grant(first.clone(), "/Users/alice/first", MountAccess::ReadWrite),
                grant(
                    second.clone(),
                    "/Users/alice/second",
                    MountAccess::ReadWrite,
                ),
            ],
        )
        .unwrap_err();

    assert_eq!(error.grant_rollback_complete(), Some(false));
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("second mount rejected"));
    assert!(diagnostic.contains("rollback complete: false"));
    assert!(
        runner
            .arguments()
            .iter()
            .all(|command| !command.iter().any(|value| value == "/bin/rm"))
    );
    fs::remove_dir(first).unwrap();
    fs::remove_dir(second).unwrap();
}

#[test]
fn mount_failure_reports_complete_rollback_only_after_all_mounts_are_revoked() {
    let failed_mount = CommandOutput {
        exit_code: Some(1),
        stdout: Vec::new(),
        stderr: b"second mount rejected".to_vec(),
    };
    let runner = FakeRunner::with_outputs([ok(), failed_mount, ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let first = test_directory("complete-grant-first");
    let second = test_directory("complete-grant-second");
    let error = adapter
        .prepare_grants(
            &vm(),
            "attempt-complete",
            &[
                grant(first.clone(), "/Users/alice/first", MountAccess::ReadWrite),
                grant(
                    second.clone(),
                    "/Users/alice/second",
                    MountAccess::ReadWrite,
                ),
            ],
        )
        .unwrap_err();

    assert_eq!(error.grant_rollback_complete(), Some(true));
    assert!(error.to_string().contains("second mount rejected"));
    assert_eq!(
        runner
            .arguments()
            .iter()
            .filter(|command| command.first().is_some_and(|value| value == "umount"))
            .count(),
        1 // the failed mount's own rollback; the first stays retained
    );
    fs::remove_dir(first).unwrap();
    fs::remove_dir(second).unwrap();
}

#[test]
fn overlapping_jobs_keep_shared_grant_until_the_last_release() {
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let root = canonical_temp_dir().join(format!(
        "marsh-grant-transition-{}-{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    let first = adapter
        .prepare_grants(
            &vm(),
            "attempt-first",
            &[grant(
                root.clone(),
                "/Users/alice/project",
                MountAccess::ReadWrite,
            )],
        )
        .unwrap();
    let second = adapter
        .prepare_grants(
            &vm(),
            "attempt-second",
            &[grant(
                root.clone(),
                "/Users/alice/project",
                MountAccess::ReadWrite,
            )],
        )
        .unwrap();
    assert_eq!(first.job_mounts(), second.job_mounts());
    assert_eq!(runner.arguments().len(), 1);

    adapter.revoke_grants(&first).unwrap();
    assert_eq!(runner.arguments().len(), 1);
    assert_eq!(second.job_mounts()[0].source, first.job_mounts()[0].source);

    adapter.revoke_grants(&second).unwrap();
    let commands = runner.arguments();
    // Retained while the VM is warm: no umount on final release.
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0][0], "mount");
    fs::remove_dir(root).unwrap();
}

#[test]
fn session_pin_reuses_mount_across_sequential_jobs_and_revokes_at_detach() {
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let source = test_directory("session-sequential-grant");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    adapter
        .prepare_session_grants(&vm(), "session-one", std::slice::from_ref(&admitted))
        .unwrap();

    let first = adapter
        .prepare_grants(&vm(), "attempt-first", std::slice::from_ref(&admitted))
        .unwrap();
    adapter.revoke_grants(&first).unwrap();
    let second = adapter
        .prepare_grants(&vm(), "attempt-second", std::slice::from_ref(&admitted))
        .unwrap();
    adapter.revoke_grants(&second).unwrap();
    assert_eq!(runner.arguments().len(), 1);

    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    let commands = runner.arguments();
    // Retained while the VM is warm: no umount on final release.
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0][0], "mount");
    fs::remove_dir(source).unwrap();
}

#[test]
fn unrelated_vm_grant_transitions_progress_while_another_mount_blocks() {
    let runner = Arc::new(ConcurrentGrantRunner::default());
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let first_source = test_directory("grant-transition-first-vm");
    let second_source = test_directory("grant-transition-second-vm");
    let first_grant = grant(
        first_source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    let second_grant = grant(
        second_source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    let first_vm = vm();
    let mut second_vm = vm();
    second_vm.name = "marsh-kit-second".into();
    let first_adapter = Arc::clone(&adapter);
    let first = thread::spawn(move || {
        first_adapter.prepare_session_grants(&first_vm, "session-one", &[first_grant])
    });
    let second_adapter = Arc::clone(&adapter);
    let second = thread::spawn(move || {
        second_adapter.prepare_session_grants(&second_vm, "session-two", &[second_grant])
    });

    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    assert!(runner.overlapped.load(Ordering::SeqCst));
    fs::remove_dir(first_source).unwrap();
    fs::remove_dir(second_source).unwrap();
}

#[test]
fn same_vm_stock_mounts_are_serialized_without_serializing_cached_hits() {
    let runner = Arc::new(SerializedStockRunner::default());
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let first_source = test_directory("serialized-stock-first");
    let second_source = test_directory("serialized-stock-second");
    let cached_source = test_directory("serialized-stock-cached");
    let first_grant = grant(
        first_source.clone(),
        "/Users/alice/first",
        MountAccess::ReadWrite,
    );
    let second_grant = grant(
        second_source.clone(),
        "/Users/alice/second",
        MountAccess::ReadWrite,
    );
    let cached_grant = grant(
        cached_source.clone(),
        "/Users/alice/cached",
        MountAccess::ReadWrite,
    );

    // Seed the cached reference without using the blocking mount runner.
    adapter.grant_mount_references.lock().unwrap().insert(
        (vm().name, cached_source.clone()),
        GrantMountReference {
            identity: cached_grant.identity.clone(),
            source_record: source_chain::SourceRecord::new(
                &cached_grant.source,
                &cached_grant.chain,
            ),
            access: cached_grant.access,
            vm_source: shared_grant_path(&cached_source, &cached_grant.identity),
            active_jobs: 0,
            pinned_sessions: BTreeMap::from([(
                "session-pinned".to_owned(),
                Arc::clone(&cached_grant.handle),
            )]),
        },
    );

    let first_adapter = Arc::clone(&adapter);
    let first = thread::spawn(move || {
        first_adapter.prepare_session_grants(&vm(), "session-first", &[first_grant])
    });
    {
        let mut state = runner.state.lock().unwrap();
        while state.0 == 0 {
            state = runner.changed.wait(state).unwrap();
        }
    }

    let second_adapter = Arc::clone(&adapter);
    let second = thread::spawn(move || {
        second_adapter.prepare_session_grants(&vm(), "session-second", &[second_grant])
    });
    assert!(
        runner
            .changed
            .wait_timeout_while(
                runner.state.lock().unwrap(),
                Duration::from_millis(50),
                |state| state.0 < 2,
            )
            .unwrap()
            .1
            .timed_out()
    );

    // A reference hit has no stock effect and remains independent of the
    // first mount's external-operation critical section.
    adapter
        .prepare_session_grants(&vm(), "session-hit", std::slice::from_ref(&cached_grant))
        .unwrap();
    assert_eq!(runner.state.lock().unwrap().0, 1);

    {
        let mut state = runner.state.lock().unwrap();
        state.1 = true;
        runner.changed.notify_all();
    }
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    assert_eq!(runner.max_active.load(Ordering::SeqCst), 1);

    fs::remove_dir(first_source).unwrap();
    fs::remove_dir(second_source).unwrap();
    fs::remove_dir(cached_source).unwrap();
}

#[test]
fn same_grant_transition_serializes_one_mount_and_keeps_both_session_refs() {
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let source = test_directory("grant-transition-same-source");
    let first_grant = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    let second_grant = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    let first_adapter = Arc::clone(&adapter);
    let first = thread::spawn(move || {
        first_adapter.prepare_session_grants(&vm(), "session-one", &[first_grant])
    });
    let second_adapter = Arc::clone(&adapter);
    let second = thread::spawn(move || {
        second_adapter.prepare_session_grants(&vm(), "session-two", &[second_grant])
    });
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    assert_eq!(runner.arguments().len(), 1);

    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    assert_eq!(runner.arguments().len(), 1);
    adapter
        .release_session_grants(&vm().name, "session-two")
        .unwrap();
    assert_eq!(runner.arguments().len(), 1); // retained while warm
    fs::remove_dir(source).unwrap();
}

#[test]
fn session_close_waits_for_inflight_pin_and_rejects_late_lazy_preparation() {
    let runner = Arc::new(BlockedGrantResetRunner {
        mount_state: Mutex::new((false, false)),
        changed: Condvar::new(),
        runs: Mutex::new(Vec::new()),
        marker: Vec::new(),
    });
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let source = test_directory("session-close-pin-race");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );

    let preparing_adapter = Arc::clone(&adapter);
    let preparing = thread::spawn(move || {
        preparing_adapter.prepare_session_grants(&vm(), "session-racing", &[admitted])
    });
    {
        let mut state = runner.mount_state.lock().unwrap();
        while !state.0 {
            state = runner.changed.wait(state).unwrap();
        }
    }

    let (close_started_send, close_started_receive) = mpsc::channel();
    let (close_done_send, close_done_receive) = mpsc::channel();
    let closing_adapter = Arc::clone(&adapter);
    let closing = thread::spawn(move || {
        close_started_send.send(()).unwrap();
        let result = closing_adapter.close_session_grants("session-racing");
        close_done_send.send(()).unwrap();
        result
    });
    close_started_receive
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    assert!(
        close_done_receive
            .recv_timeout(Duration::from_millis(50))
            .is_err(),
        "session close passed an in-flight pin operation"
    );

    {
        let mut state = runner.mount_state.lock().unwrap();
        state.1 = true;
        runner.changed.notify_all();
    }
    preparing.join().unwrap().unwrap();
    closing.join().unwrap().unwrap();
    assert!(adapter.pinned_session_vms("session-racing").is_empty());

    let late_grant = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    assert!(matches!(
        adapter.prepare_session_grants(&vm(), "session-racing", &[late_grant]),
        Err(SbxError::ClosedSession(session)) if session == "session-racing"
    ));
    assert_eq!(runner.runs.lock().unwrap().len(), 1); // retained while warm
    fs::remove_dir(source).unwrap();
}

#[test]
fn completed_grant_transition_keys_do_not_accumulate() {
    let runner = FakeRunner::with_outputs((0..64).map(|_| ok()));
    let adapter = StockSbx::new("sbx", runner);
    for sequence in 0..32 {
        let source = test_directory(&format!("grant-transition-churn-{sequence}"));
        let admitted = grant(
            source.clone(),
            "/Users/alice/project",
            MountAccess::ReadWrite,
        );
        let session = format!("session-{sequence}");
        adapter
            .prepare_session_grants(&vm(), &session, &[admitted])
            .unwrap();
        adapter
            .release_session_grants(&vm().name, &session)
            .unwrap();
        fs::remove_dir(source).unwrap();
    }
    assert!(adapter.grant_transition_locks.lock().unwrap().is_empty());
    assert!(adapter.grant_vm_transition_locks.lock().unwrap().is_empty());
    assert!(
        adapter
            .grant_stock_operation_locks
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn final_session_pin_release_keeps_the_warm_mount_without_stock_effects() {
    let runner = FakeRunner::with_outputs([ok(), failed("unmount failed"), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    runner.seed_owned(&adapter, &vm().name, "running");
    let source = test_directory("session-release-failure");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    adapter
        .prepare_session_grants(&vm(), "session-one", std::slice::from_ref(&admitted))
        .unwrap();
    let effects = runner.arguments().len();
    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    assert!(adapter.reject_quarantined(&vm().name).is_ok());
    // The next session on the warm VM reuses the retained mount.
    adapter
        .prepare_session_grants(&vm(), "session-two", &[admitted])
        .unwrap();
    assert_eq!(runner.arguments().len(), effects);
    fs::remove_dir(source).unwrap();
}

#[test]
fn failed_session_pin_rollback_quarantines_and_stops_exact_vm() {
    let runner = FakeRunner::with_outputs([
        ok(),
        failed("second mount failed"),
        failed("rollback unmount failed"),
        ok(),
    ]);
    let adapter = StockSbx::new("sbx", runner.clone());
    runner.seed_owned(&adapter, &vm().name, "running");
    let first = test_directory("session-rollback-first");
    let second = test_directory("session-rollback-second");
    let error = adapter
        .prepare_session_grants(
            &vm(),
            "session-one",
            &[
                grant(first.clone(), "/Users/alice/first", MountAccess::ReadWrite),
                grant(
                    second.clone(),
                    "/Users/alice/second",
                    MountAccess::ReadWrite,
                ),
            ],
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SbxError::GrantPreparationFailed {
            rollback_complete: false,
            ..
        }
    ));
    assert!(matches!(
        adapter.reject_quarantined(&vm().name),
        Err(SbxError::QuarantinedVm(_))
    ));
    assert_eq!(
        runner.arguments().last().unwrap(),
        &["stop", vm().name.as_str()]
    );
    fs::remove_dir(first).unwrap();
    fs::remove_dir(second).unwrap();
}

#[test]
fn active_jobs_retain_mount_after_session_detach_until_final_job_release() {
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let source = test_directory("session-concurrent-grant");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    adapter
        .prepare_session_grants(&vm(), "session-one", std::slice::from_ref(&admitted))
        .unwrap();
    let first = adapter
        .prepare_grants(&vm(), "attempt-one", std::slice::from_ref(&admitted))
        .unwrap();
    let second = adapter
        .prepare_grants(&vm(), "attempt-two", std::slice::from_ref(&admitted))
        .unwrap();

    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    adapter.revoke_grants(&first).unwrap();
    assert_eq!(runner.arguments().len(), 1);
    adapter.revoke_grants(&second).unwrap();
    assert_eq!(runner.arguments().len(), 1); // retained while warm
    fs::remove_dir(source).unwrap();
}

#[test]
fn different_sessions_hold_independent_pins_on_one_exact_mount() {
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let source = test_directory("different-session-grant");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    adapter
        .prepare_session_grants(&vm(), "session-one", std::slice::from_ref(&admitted))
        .unwrap();
    adapter
        .prepare_session_grants(&vm(), "session-two", std::slice::from_ref(&admitted))
        .unwrap();
    assert_eq!(runner.arguments().len(), 1);

    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    assert_eq!(runner.arguments().len(), 1);
    adapter
        .release_session_grants(&vm().name, "session-two")
        .unwrap();
    assert_eq!(runner.arguments().len(), 1); // retained while warm
    fs::remove_dir(source).unwrap();
}

#[test]
fn pinned_session_rejects_replaced_source_without_stock_effects() {
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let source = test_directory("replaced-session-grant");
    let displaced = source.with_extension("displaced");
    let original = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    adapter
        .prepare_session_grants(&vm(), "session-one", &[original])
        .unwrap();
    fs::rename(&source, &displaced).unwrap();
    fs::create_dir(&source).unwrap();
    let replacement = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    let error = adapter
        .prepare_session_grants(&vm(), "session-two", &[replacement])
        .unwrap_err();
    assert!(error.to_string().contains("source changed"));
    assert_eq!(runner.arguments().len(), 1);

    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    fs::remove_dir(source).unwrap();
    fs::remove_dir(displaced).unwrap();
}

#[test]
fn stale_exact_stock_mount_is_reclaimed_once_after_daemon_restart() {
    let runner = FakeRunner::with_outputs([failed("mount already mounted"), ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let source = test_directory("stale-session-grant");
    let admitted = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadWrite,
    );
    adapter
        .prepare_session_grants(&vm(), "session-one", &[admitted])
        .unwrap();
    let commands = runner.arguments();
    assert_eq!(commands.len(), 3);
    assert_eq!(commands[0][0], "mount");
    assert_eq!(commands[1][0], "umount");
    assert_eq!(commands[2][0], "mount");

    adapter
        .release_session_grants(&vm().name, "session-one")
        .unwrap();
    assert_eq!(runner.arguments().len(), 3); // retained while warm
    fs::remove_dir(source).unwrap();
}

#[test]
fn live_shared_grant_rejects_a_different_access_mode_without_stock_effects() {
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let source = test_directory("shared-grant-access");
    let first = adapter
        .prepare_grants(
            &vm(),
            "attempt-read",
            &[grant(
                source.clone(),
                "/Users/alice/project",
                MountAccess::ReadOnly,
            )],
        )
        .unwrap();
    let error = adapter
        .prepare_grants(
            &vm(),
            "attempt-write",
            &[grant(
                source.clone(),
                "/Users/alice/project",
                MountAccess::ReadWrite,
            )],
        )
        .unwrap_err();
    assert!(matches!(
        error,
        SbxError::GrantPreparationFailed {
            rollback_complete: true,
            ..
        }
    ));
    assert_eq!(runner.arguments().len(), 1);
    adapter.revoke_grants(&first).unwrap();
    assert_eq!(runner.arguments().len(), 1); // retained while warm
    fs::remove_dir(source).unwrap();
}

#[test]
fn prewarm_pulls_exact_digest_and_quarantine_stops_only_one_vm() {
    let image =
        OciImage::parse(format!("example.invalid/probe@sha256:{}", "a".repeat(64))).unwrap();
    let image_id = format!("sha256:{}", "e".repeat(64));
    let runner = FakeRunner::with_outputs([
        ok(),
        CommandOutput {
            exit_code: Some(0),
            stdout: image_id.into_bytes(),
            stderr: Vec::new(),
        },
        ok(),
    ]);
    let adapter = StockSbx::new("sbx", runner.clone());
    runner.seed_owned(&adapter, &vm().name, "running");
    adapter.prewarm_image(&vm(), &image).unwrap();
    adapter.stop_quarantined(&vm()).unwrap();
    let home = selected_home();
    let error = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: vm().name,
            worker_binary: "/tmp/does-not-matter-after-quarantine".into(),
            workload_kit: workload_ref(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap_err();
    assert!(matches!(error, SbxError::QuarantinedVm(_)));
    let commands = runner.arguments();
    assert_eq!(
        commands.len(),
        3,
        "quarantine must reject before any reuse effect"
    );
    assert_eq!(
        commands[0][..6],
        [
            "exec",
            "-u",
            "root",
            "marsh-kit-fixture-abc",
            "docker",
            "pull"
        ]
    );
    assert_eq!(commands[0][6], image.as_str());
    assert_eq!(commands[1][4..7], ["docker", "image", "inspect"]);
    assert_eq!(commands[2], ["stop", vm().name.as_str()]);
    fs::remove_dir(home).unwrap();
}

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn alice_shell() -> ReadyShellVm {
    ReadyShellVm {
        name: "marsh-shell-user".into(),
        user: ShellUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/Users/alice".into(),
        },
        cold_started: false,
    }
}

fn supervisor_starts(runner: &FakeRunner) -> Vec<shell_supervisor::StartSpec> {
    // Plain children (relays) start without awaiting the acknowledgement.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut seen = 0;
    while std::time::Instant::now() < deadline {
        let count = runner.supervisor_frames.lock().unwrap().len();
        if count > 0 && count == seen {
            break;
        }
        seen = count;
        std::thread::sleep(Duration::from_millis(20));
    }
    runner
        .supervisor_frames
        .lock()
        .unwrap()
        .iter()
        .filter_map(|frame| match frame {
            shell_supervisor::Down::Start { spec, .. } => Some(spec.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn shell_attach_preserves_natural_cwd_identity_environment_and_argv() {
    let runner = Arc::new(FakeRunner::default());
    let adapter = StockSbx::new("sbx", runner.clone());
    let shell = alice_shell();
    adapter.start_test_supervisor(&shell.name);
    adapter
        .attach_shell(
            &shell,
            Path::new("/Users/alice/project"),
            true,
            Some(TerminalSize {
                rows: 47,
                columns: 123,
            }),
            &[OsString::from("-c"), OsString::from("printf hello")],
        )
        .unwrap();
    // One supervisor frame, no stock process for the session itself.
    assert_eq!(runner.spawns.lock().unwrap().len(), 1);
    assert!(runner.arguments().is_empty());
    let starts = supervisor_starts(&runner);
    let [spec] = starts.as_slice() else {
        panic!("expected one start: {starts:?}")
    };
    let shell_supervisor::StartKind::Shell {
        record,
        uid,
        terminal,
    } = &spec.kind
    else {
        panic!("shell start expected")
    };
    let record = String::from_utf8(record.clone()).unwrap();
    assert!(record.starts_with("/run/marsh/1000/") && record.ends_with("/shell"));
    assert_eq!((*uid, *terminal), (1000, Some((47, 123))));
    assert_eq!(spec.program, b"/usr/local/bin/marsh");
    assert_eq!(spec.working_directory, b"/Users/alice/project");
    let environment = &spec.environment;
    for (name, value) in [("HOME", "/Users/alice"), ("BASH_ENV", ""), ("ENV", "")] {
        assert!(environment.contains(&(name.as_bytes().to_vec(), value.as_bytes().to_vec())));
    }
    let arguments = spec
        .arguments
        .iter()
        .map(|word| String::from_utf8(word.clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        arguments,
        [
            "--internal-record-session",
            record.as_str(),
            "1000",
            "1000",
            "-c",
            "printf hello"
        ]
    );
}

#[test]
fn shell_attach_exposes_only_relay_paths_and_controls_in_band() {
    let runner = Arc::new(FakeRunner::default());
    let adapter = StockSbx::new("sbx", runner.clone());
    let shell = alice_shell();
    adapter.start_test_supervisor(&shell.name);
    let attachment = adapter
        .attach_shell_with_relay(
            &shell,
            Path::new("/Users/alice/project"),
            false,
            None,
            &[],
            Some((
                Path::new("/run/marsh/1000/session/s"),
                Path::new("/run/marsh/1000/session/t"),
            )),
            &[],
        )
        .unwrap();
    attachment.control.signal(JobSignal::Interrupt).unwrap();
    attachment.control.cleanup_session().unwrap();
    let spec = supervisor_starts(&runner).remove(0);
    assert_eq!(
        spec.kind,
        shell_supervisor::StartKind::Shell {
            record: b"/run/marsh/1000/session/shell".to_vec(),
            uid: 1000,
            terminal: None,
        }
    );
    for (name, value) in [
        ("MARSH_DAEMON_SOCKET", "/run/marsh/1000/session/s"),
        ("MARSH_DAEMON_TOKEN", "/run/marsh/1000/session/t"),
    ] {
        assert!(
            spec.environment
                .contains(&(name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        );
    }
    assert!(!format!("{spec:?}").contains("bearer"));
    // Signal is an attempt-keyed frame; cleanup after exit was reported
    // in-band by the supervisor; neither starts a stock process.
    let frames = runner.supervisor_frames.lock().unwrap().clone();
    assert!(frames.iter().any(|frame| matches!(
        frame,
        shell_supervisor::Down::Control { op: shell_supervisor::ControlOp::Signal(name), .. }
            if name == "INT"
    )));
    assert!(!frames.iter().any(|frame| matches!(
        frame,
        shell_supervisor::Down::Control {
            op: shell_supervisor::ControlOp::Cleanup,
            ..
        }
    )));
    assert!(runner.arguments().is_empty());
}

#[test]
fn lost_supervisor_with_live_session_is_uncertain_and_fences_the_vm() {
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());
    let relay = artifact();
    let mut outputs = cold_shell_outputs();
    outputs.extend([ok(), ok(), ok()]);
    let runner = FakeRunner::with_outputs(outputs);
    let adapter = StockSbx::new("sbx", runner.clone());
    let vm = adapter.ensure_shell_vm(&spec).unwrap();
    let mut attachment = adapter
        .launch_shell_relay(&vm, &relay, "session-1")
        .unwrap()
        .process;
    assert_eq!(attachment.process.try_wait().unwrap(), None);
    // The single retained transport dies with a started attempt.
    runner.exit_spawn(0);
    assert!(matches!(
        adapter.ensure_shell_vm(&spec),
        Err(SbxError::QuarantinedShellVm(name)) if name == spec.name
    ));
    assert!(attachment.process.try_wait().is_err());
    assert!(attachment.control.cleanup_session().is_err());
    // Never replayed or replaced underneath the uncertain session.
    assert_eq!(runner.spawns.lock().unwrap().len(), 1);
    fs::remove_file(shell_binary).unwrap();
    fs::remove_file(relay).unwrap();
}

#[test]
fn shell_mounts_selected_home_at_guest_home_without_exposing_real_home() {
    let runner = FakeRunner::with_outputs([ok(), ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let sequence = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let project = canonical_temp_dir().join(format!(
        "marsh-shell-project-{}-{sequence}",
        std::process::id()
    ));
    let selected = canonical_temp_dir().join(format!(
        "marsh-selected-home-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir(&project).unwrap();
    fs::create_dir(&selected).unwrap();
    let shell = ReadyShellVm {
        name: "marsh-shell-user".into(),
        user: ShellUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/Users/alice".into(),
        },
        cold_started: false,
    };
    runner.seed_owned(&adapter, &shell.name, "running");
    let grants = shell_grants(&project, &selected);
    let mounts = adapter.prepare_shell_mounts(&shell, &grants).unwrap();
    adapter.revoke_shell_mounts(&mounts).unwrap();
    let commands = runner.arguments();
    assert_eq!(commands[0][0], "mount");
    assert!(commands[..2].iter().any(|command| command[2] == "."));
    assert!(
        commands[..2]
            .iter()
            .any(|command| command[2] == ".:/Users/alice:rw")
    );
    let invocations = runner.invocations();
    assert!(invocations[..2].iter().any(|invocation| {
        invocation.working_directory.as_deref() == Some(project.as_path())
            && invocation.arguments[2] == "."
    }));
    assert!(invocations[..2].iter().any(|invocation| {
        invocation.working_directory.as_deref() == Some(selected.as_path())
            && invocation.arguments[2] == ".:/Users/alice:rw"
    }));
    assert_eq!(commands.len(), 2); // retained while the shell VM is warm
    fs::remove_dir(project).unwrap();
    fs::remove_dir(selected).unwrap();
}

#[test]
fn shell_project_with_colon_uses_relative_stock_sbx_mount_and_unmount() {
    let runner = FakeRunner::with_outputs([ok(), ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let project = test_directory("project:colon,equals=value");
    let selected = test_directory("selected-home-colon");
    let shell = ReadyShellVm {
        name: "marsh-shell-punctuation".into(),
        user: ShellUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/Users/alice".into(),
        },
        cold_started: false,
    };
    runner.seed_owned(&adapter, &shell.name, "running");

    let mounts = adapter
        .prepare_shell_mounts(&shell, &shell_grants(&project, &selected))
        .unwrap();
    adapter.revoke_shell_mounts(&mounts).unwrap();

    let invocations = runner.invocations();
    let project_calls = invocations
        .iter()
        .filter(|invocation| invocation.working_directory.as_deref() == Some(project.as_path()))
        .collect::<Vec<_>>();
    assert_eq!(project_calls.len(), 1); // retained while warm
    assert_eq!(
        project_calls[0].arguments,
        ["mount", "marsh-shell-punctuation", "."]
    );

    fs::remove_dir(project).unwrap();
    fs::remove_dir(selected).unwrap();
}

#[test]
fn grant_source_with_colon_uses_relative_stock_sbx_mount_and_unmount() {
    let runner = FakeRunner::with_outputs([ok(), ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let source = test_directory("grant:colon,equals=value");
    let prepared = adapter
        .prepare_grants(
            &vm(),
            "colon-grant",
            &[grant(
                source.clone(),
                "/Users/alice/project:colon,equals=value",
                MountAccess::ReadWrite,
            )],
        )
        .unwrap();
    adapter.revoke_grants(&prepared).unwrap();

    let invocations = runner.invocations();
    let source_calls = invocations
        .iter()
        .filter(|invocation| invocation.working_directory.as_deref() == Some(source.as_path()))
        .collect::<Vec<_>>();
    assert_eq!(source_calls.len(), 1); // retained while warm
    let shared = prepared.job_mounts()[0].source.clone();
    assert_eq!(
        source_calls[0].arguments,
        [
            "mount",
            "marsh-kit-fixture-abc",
            &format!(".:{}:rw", shared.display()),
        ]
    );

    fs::remove_dir(source).unwrap();
}

#[test]
fn local_v3_source_requires_canonical_directory_and_hashes_symlink_targets_without_following() {
    let source = local_kit_source();
    let relative = PathBuf::from("kits/fixture");
    assert!(matches!(
        NativeKitRef::local_v3_source(relative),
        Err(SbxError::Metadata { .. } | SbxError::InvalidLocalKitSource(_))
    ));
    let kit = NativeKitRef::local_v3_source(source.clone()).unwrap();
    assert_eq!(kit.source_dir(), Some(source.as_path()));
    assert!(kit.workload_image().is_none());

    let outside = test_directory("outside-kit-link");
    fs::write(outside.join("first"), "one").unwrap();
    fs::write(outside.join("second"), "two").unwrap();
    let link = source.join("ordinary-link");
    std::os::unix::fs::symlink(outside.join("first"), &link).unwrap();
    let first = local_source_fingerprint(&source).unwrap();

    // The source identity includes the link text, never the linked-to file.
    fs::write(outside.join("first"), "changed outside source").unwrap();
    assert_eq!(local_source_fingerprint(&source).unwrap(), first);
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(outside.join("second"), &link).unwrap();
    assert_ne!(local_source_fingerprint(&source).unwrap(), first);

    fs::remove_dir_all(outside).unwrap();
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn local_v3_source_rejects_special_files() {
    let source = local_kit_source();
    let socket = source.join("local.socket");
    let listener = match std::os::unix::net::UnixListener::bind(&socket) {
        Ok(listener) => listener,
        // Some hermetic macOS test sandboxes deny creation of Unix sockets.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            fs::remove_dir_all(source).unwrap();
            return;
        }
        Err(error) => panic!("create test Unix socket: {error}"),
    };
    assert!(matches!(
        local_source_fingerprint(&source),
        Err(SbxError::InvalidLocalKitSource(path)) if path == socket
    ));
    drop(listener);
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn stale_local_scratch_sweep_is_narrow_and_never_follows_symlinks() {
    let home = selected_home();
    let outside = test_directory("scratch-sweep-outside");
    let victim = outside.join("victim");
    fs::write(&victim, "keep").unwrap();
    let stale_archive = home.join(".marsh-kit-123-4.docker.tar");
    let stale_metadata = home.join(".marsh-kit-123-4.metadata.json");
    let fresh_archive = home.join(".marsh-kit-123-5.docker.tar");
    let current_archive = home.join(format!(".marsh-kit-{}-99.docker.tar", std::process::id()));
    let unrelated = home.join("unrelated.docker.tar");
    let malformed = home.join(".marsh-kit-owner-6.docker.tar");
    let disguised_link = home.join(".marsh-kit-123-7.docker.tar");
    for path in [
        &stale_archive,
        &stale_metadata,
        &fresh_archive,
        &current_archive,
        &unrelated,
        &malformed,
    ] {
        fs::write(path, "scratch").unwrap();
    }
    std::os::unix::fs::symlink(&victim, &disguised_link).unwrap();

    let now = SystemTime::now();
    let old = now - LOCAL_SCRATCH_STALE_AFTER - Duration::from_secs(1);
    for path in [
        &stale_archive,
        &stale_metadata,
        &current_archive,
        &unrelated,
        &malformed,
    ] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
    }

    sweep_stale_local_scratch(&home, now).unwrap();
    assert!(!stale_archive.exists());
    assert!(!stale_metadata.exists());
    assert!(fresh_archive.exists());
    assert!(current_archive.exists());
    assert!(unrelated.exists());
    assert!(malformed.exists());
    assert!(
        fs::symlink_metadata(&disguised_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");

    fs::remove_dir_all(home).unwrap();
    fs::remove_dir_all(outside).unwrap();
}

#[test]
fn local_v3_detection_ignores_unrelated_compose_yaml() {
    let source = local_kit_source();
    fs::write(
        source.join("compose.yaml"),
        "services:\n  app:\n    image: example.invalid/app:latest\n",
    )
    .unwrap();
    assert_eq!(
        local_v3_descriptor(&source).unwrap(),
        source.join("fixture.yaml")
    );
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn local_v3_detection_propagates_candidate_read_errors() {
    let source = local_kit_source();
    let broken = source.join("broken.yaml");
    fs::write(&broken, "schemaVersion: \"3\"\n").unwrap();
    let mut permissions = fs::metadata(&broken).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0);
    fs::set_permissions(&broken, permissions).unwrap();
    assert!(matches!(
        local_v3_descriptor(&source),
        Err(SbxError::Metadata { path, .. }) if path == broken
    ));
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn local_v3_detection_rejects_legacy_spec_coexistence_and_skips_yaml_directories() {
    let source = local_kit_source();
    fs::create_dir(source.join("ignored.yaml")).unwrap();
    assert_eq!(
        local_v3_descriptor(&source).unwrap(),
        source.join("fixture.yaml")
    );
    fs::write(source.join("spec.yaml"), "schemaVersion: \"2\"\n").unwrap();
    assert!(matches!(
        local_v3_descriptor(&source),
        Err(SbxError::InvalidLocalKitSource(path)) if path == source
    ));
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn nested_archive_streaming_drains_both_output_pipes_concurrently() {
    let terminated = Arc::new(AtomicBool::new(false));
    let reaped = Arc::new(AtomicBool::new(false));
    let runner = Arc::new(ArchivePressureRunner {
        fail_write: false,
        terminated: Arc::clone(&terminated),
        reaped: Arc::clone(&reaped),
    });
    let adapter = StockSbx::new("sbx", runner);
    let archive = artifact();
    fs::write(&archive, vec![b'i'; 1024 * 1024]).unwrap();
    adapter
        .load_nested_archive("marsh-kit-local-source", &archive)
        .unwrap();
    assert!(!terminated.load(Ordering::SeqCst));
    assert!(reaped.load(Ordering::SeqCst));
    fs::remove_file(archive).unwrap();
}

#[test]
fn nested_archive_transfer_failure_terminates_and_reaps_child() {
    let terminated = Arc::new(AtomicBool::new(false));
    let reaped = Arc::new(AtomicBool::new(false));
    let runner = Arc::new(ArchivePressureRunner {
        fail_write: true,
        terminated: Arc::clone(&terminated),
        reaped: Arc::clone(&reaped),
    });
    let adapter = StockSbx::new("sbx", runner);
    let archive = artifact();
    fs::write(&archive, vec![b'i'; 1024 * 1024]).unwrap();
    assert!(matches!(
        adapter.load_nested_archive("marsh-kit-local-source", &archive),
        Err(SbxError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe
    ));
    assert!(terminated.load(Ordering::SeqCst));
    assert!(reaped.load(Ordering::SeqCst));
    fs::remove_file(archive).unwrap();
}

#[test]
fn nested_archive_deadline_terminates_reaps_and_joins_all_streams() {
    let terminated = Arc::new(AtomicBool::new(false));
    let reaped = Arc::new(AtomicBool::new(false));
    let runner = Arc::new(BlockingLoadRunner {
        terminated: Arc::clone(&terminated),
        reaped: Arc::clone(&reaped),
    });
    let timeout = Duration::from_millis(40);
    let adapter = StockSbx::new("sbx", runner).with_preparation_timeouts(PreparationTimeouts {
        sbx: timeout,
        build: timeout,
        load: timeout,
        docker: timeout,
    });
    let archive = artifact();
    fs::write(&archive, vec![b'i'; 1024 * 1024]).unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(
        adapter.load_nested_archive("marsh-kit-local-source", &archive),
        Err(SbxError::PreparationTimeout {
            operation: "load local v3 kit image into worker VM",
            ..
        })
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(terminated.load(Ordering::SeqCst));
    assert!(reaped.load(Ordering::SeqCst));
    fs::remove_file(archive).unwrap();
}

#[test]
fn retained_worker_teardown_times_out_when_child_termination_wedges() {
    let timeout = Duration::from_millis(30);
    let adapter = StockSbx::new("sbx", Arc::new(FakeRunner::default()))
        .with_retained_process_teardown_timeout(timeout);
    let terminate_started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    adapter.worker_leases.lock().unwrap().insert(
        "marsh-kit-wedged".into(),
        Arc::new(RetainedWorker {
            generation: 1,
            input: Mutex::new(Box::new(io::sink())),
            process: Mutex::new(Box::new(WedgedTerminationProcess {
                terminate_started: Arc::clone(&terminate_started),
                release: Arc::clone(&release),
            })),
            routes: Mutex::new(BTreeMap::new()),
            ping_serial: Mutex::new(()),
            pending_pong: Mutex::new(None),
            next_ping: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            active: AtomicUsize::new(0),
            loss: WorkerLoss::default(),
        }),
    );

    let started = Instant::now();
    let error = adapter.drop_worker_lease("marsh-kit-wedged").unwrap_err();
    assert!(matches!(
        error,
        SbxError::Io(ref error) if error.kind() == io::ErrorKind::TimedOut
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(terminate_started.load(Ordering::SeqCst));
    assert!(
        adapter
            .worker_leases
            .lock()
            .unwrap()
            .get("marsh-kit-wedged")
            .is_none()
    );
    release.store(true, Ordering::SeqCst);
}

#[test]
fn local_v3_cold_preparation_overlaps_stock_create_and_direct_build() {
    let source = local_kit_source();
    let home = selected_home();
    let manifest = format!("sha256:{}", "f".repeat(64));
    let runner = Arc::new(ConcurrentPreparationRunner {
        state: Mutex::new((false, false)),
        changed: Condvar::new(),
        overlapped: AtomicBool::new(false),
        manifest,
    });
    let adapter = StockSbx::new("sbx", runner.clone());
    let spec = KitVmSpec {
        name: "marsh-kit-local-source".into(),
        worker_binary: "/tmp/not-used-during-preparation".into(),
        workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
        lifecycle_workspace: home.clone(),
    };
    let fingerprint = spec.workload_kit.validate_captured_source(&source).unwrap();
    let lifecycle_grant = StockSbx::open_lifecycle_workspace(&spec).unwrap();
    let (_, build) = adapter
        .create_and_build_local(&spec, &source, &fingerprint, &lifecycle_grant)
        .unwrap();
    assert!(runner.overlapped.load(Ordering::SeqCst));
    StockSbx::cleanup_local_build(&build).unwrap();
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
#[allow(clippy::too_many_lines)] // One scenario asserts the complete public command sequence.
fn local_v3_cold_start_builds_verifies_and_seeds_exact_image() {
    let source = local_kit_source();
    let home = selected_home();
    let worker = artifact();
    let manifest = format!("sha256:{}", "f".repeat(64));
    let image_id = manifest.clone();
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        local_inspect(&manifest),
        stdout(format!("{image_id}\n").as_bytes()),
        ok(),
        ok(),
        ok(),
        ok(),
        stdout(b"0:0:700\n"),
        stdout(format!("{image_id}\n").as_bytes()),
    ]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let ready = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: "marsh-kit-local-source".into(),
            worker_binary: worker.clone(),
            workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap();
    assert_eq!(ready.job_image.as_str(), image_id);
    adapter.prewarm_workload(&ready).unwrap();

    let invocations = runner.invocations();
    let build = invocations
        .iter()
        .find(|invocation| {
            invocation.program == Path::new("docker")
                && invocation
                    .arguments
                    .first()
                    .is_some_and(|value| value == "buildx")
        })
        .unwrap();
    assert_eq!(
        build
            .arguments
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>()[..6],
        [
            "buildx",
            "build",
            "--platform",
            "linux/arm64",
            "--output",
            &build.arguments[5].to_string_lossy()
        ]
    );
    let output_paths = build
        .arguments
        .iter()
        .filter_map(|argument| {
            let argument = argument.to_str()?;
            argument.strip_prefix("type=docker,dest=")
        })
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    assert_eq!(output_paths.len(), 1);
    assert!(!build.arguments.iter().any(|argument| {
        argument
            .to_str()
            .is_some_and(|argument| argument.starts_with("type=oci,"))
    }));
    assert!(
        output_paths
            .iter()
            .all(|path| path.starts_with(&home) && !path.exists())
    );
    let commands = runner.arguments();
    let first_create = commands
        .iter()
        .position(|arguments| arguments.first().is_some_and(|value| value == "create"))
        .unwrap();
    assert!(
        !commands[..first_create]
            .iter()
            .any(|arguments| arguments.first().is_some_and(|value| value == "inspect")),
        "inventory-proven absence needs no inspect before create"
    );
    let create = commands
        .iter()
        .find(|arguments| arguments.first().is_some_and(|value| value == "create"))
        .unwrap();
    assert!(
        create
            .iter()
            .any(|value| value == &source.display().to_string())
    );
    assert!(!commands.iter().any(|arguments| {
        arguments.first().is_some_and(|value| value == "template")
            && arguments.get(1).is_some_and(|value| value == "load")
    }));
    let tag_index = build
        .arguments
        .iter()
        .position(|value| value == "--tag")
        .unwrap();
    assert_eq!(
        build.arguments[tag_index + 1].to_string_lossy(),
        format!(
            "marsh-local-v3:{}",
            &local_source_fingerprint(&source).unwrap()[7..]
        )
    );
    assert!(runner.spawns.lock().unwrap().iter().any(|invocation| {
        invocation.arguments
            == [
                "exec",
                "-i",
                "-u",
                "root",
                "marsh-kit-local-source",
                "sh",
                "-c",
                "docker info --format '{{.ID}}' >/dev/null && exec \"$@\"",
                "marsh-load",
                "docker",
                "image",
                "load",
            ]
    }));
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);

    fs::remove_file(worker).unwrap();
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn captured_local_generation_reuses_its_fingerprint_but_validates_before_source_use() {
    let source = local_kit_source();
    let captured = NativeKitRef::local_v3_source(source.clone())
        .unwrap()
        .capture_generation()
        .unwrap();
    let original = captured.expected_local_fingerprint(&source).unwrap();

    fs::write(
        source.join("fixture.dockerfile"),
        "FROM scratch\n# changed\n",
    )
    .unwrap();

    assert_eq!(
        captured.expected_local_fingerprint(&source).unwrap(),
        original
    );
    assert!(matches!(
        captured.validate_captured_source(&source),
        Err(SbxError::SourceChanged(path)) if path == source
    ));
    fs::remove_dir_all(source).unwrap();
}

#[test]
fn local_v3_source_edit_invalidates_ready_image_without_rebuilding_unchanged_source() {
    let source = local_kit_source();
    let home = selected_home();
    let worker = artifact();
    let manifest = format!("sha256:{}", "f".repeat(64));
    let first_image_id = manifest.clone();
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        local_inspect(&manifest),
        stdout(format!("{first_image_id}\n").as_bytes()),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        ok(),
        stdout(b"0:0:700\n"),
    ]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let spec = KitVmSpec {
        name: "marsh-kit-local-source".into(),
        worker_binary: worker.clone(),
        workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
        lifecycle_workspace: home.clone(),
    };
    let ready = adapter.ensure_kit_vm(&spec).unwrap();
    assert!(adapter.revalidate_cached_kit(&spec, &ready).unwrap());
    let original_fingerprint = local_source_fingerprint(&source).unwrap();
    let builds_before_edit = runner
        .invocations()
        .iter()
        .filter(|invocation| {
            invocation.program == Path::new("docker")
                && invocation
                    .arguments
                    .first()
                    .is_some_and(|value| value == "buildx")
        })
        .count();
    assert_eq!(builds_before_edit, 1);

    fs::write(
        source.join("fixture.dockerfile"),
        "FROM scratch\n# changed\n",
    )
    .unwrap();
    assert!(!adapter.revalidate_cached_kit(&spec, &ready).unwrap());
    let builds_after_edit = runner
        .invocations()
        .iter()
        .filter(|invocation| {
            invocation.program == Path::new("docker")
                && invocation
                    .arguments
                    .first()
                    .is_some_and(|value| value == "buildx")
        })
        .count();
    assert_eq!(
        builds_after_edit, 1,
        "revalidation stays a cheap fingerprint check"
    );
    let changed_fingerprint = local_source_fingerprint(&source).unwrap();
    assert_ne!(changed_fingerprint, original_fingerprint);
    assert_ne!(
        format!("marsh-local-v3:{}", &changed_fingerprint[7..]),
        format!("marsh-local-v3:{}", &original_fingerprint[7..])
    );

    fs::remove_file(worker).unwrap();
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn local_v3_prewarm_fails_when_nested_image_disappears() {
    let source = local_kit_source();
    let home = selected_home();
    let image = format!("sha256:{}", "c".repeat(64));
    let runner = FakeRunner::with_outputs([failed("image missing")]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let ready = local_vm(&source, home.clone(), &image);
    adapter.local_resolutions.lock().unwrap().insert(
        ready.kit_ref.clone(),
        LocalResolution {
            source_fingerprint: local_source_fingerprint(&source).unwrap(),
            image_id: ready.job_image.clone(),
            local_tag: "marsh-local-v3:seed-failure".into(),
        },
    );
    assert!(matches!(
        adapter.prewarm_workload(&ready),
        Err(SbxError::CommandFailed {
            operation: "inspect local v3 kit image",
            ..
        })
    ));
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn local_v3_build_failure_leaves_no_vm_or_scratch() {
    let source = local_kit_source();
    let home = selected_home();
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    runner.fail_next_build("build failed");
    let adapter = StockSbx::new("sbx", runner.clone());
    let worker = artifact();
    let error = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: "marsh-kit-local-source".into(),
            worker_binary: worker.clone(),
            workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap_err();
    assert!(matches!(
        error,
        SbxError::CommandFailed {
            operation: "build local v3 kit job image",
            ..
        }
    ));
    let invocations = runner.invocations();
    assert!(invocations.iter().any(|invocation| {
        invocation
            .arguments
            .first()
            .is_some_and(|argument| argument == "create")
    }));
    assert!(
        invocations.iter().any(|invocation| {
            invocation.arguments == ["rm", "--force", "marsh-kit-local-source"]
        })
    );
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
    fs::remove_file(worker).unwrap();
}

#[test]
fn local_v3_create_failure_joins_build_and_removes_its_scratch() {
    let source = local_kit_source();
    let home = selected_home();
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    runner.fail_next_create("create failed");
    let adapter = StockSbx::new("sbx", runner.clone());
    let worker = artifact();
    let error = adapter
        .ensure_kit_vm(&KitVmSpec {
            name: "marsh-kit-local-source".into(),
            worker_binary: worker.clone(),
            workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
            lifecycle_workspace: home.clone(),
        })
        .unwrap_err();
    assert!(matches!(
        error,
        SbxError::CommandFailed {
            operation: "create local kit VM",
            ..
        }
    ));
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    // A failed create leaves no VM; the adapter refuses further reuse.
    assert!(!runner.present("marsh-kit-local-source"));

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
    fs::remove_file(worker).unwrap();
}

#[test]
fn local_v3_create_timeout_is_typed_cleans_up_and_releases_retry_lock() {
    let source = local_kit_source();
    let home = selected_home();
    let runner = FakeRunner::with_outputs([ok(), ok(), missing(), ok(), ok()]);
    runner.timeout_next_create();
    let timeout = Duration::from_millis(40);
    let adapter =
        StockSbx::new("sbx", runner.clone()).with_preparation_timeouts(PreparationTimeouts {
            sbx: timeout,
            build: timeout,
            load: timeout,
            docker: timeout,
        });
    let worker = artifact();
    let spec = KitVmSpec {
        name: "marsh-kit-local-source".into(),
        worker_binary: worker.clone(),
        workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
        lifecycle_workspace: home.clone(),
    };

    assert!(matches!(
        adapter.ensure_kit_vm(&spec),
        Err(SbxError::PreparationTimeout {
            operation: "create local kit VM",
            ..
        })
    ));
    assert!(adapter.local_resolutions.lock().unwrap().is_empty());
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);

    // An uncertain create quarantines the name: no implicit retry.
    assert!(matches!(
        adapter.ensure_kit_vm(&spec),
        Err(SbxError::QuarantinedVm(_))
    ));
    // Explicit operator retire (`marsh workers reset KIT`) releases it.
    assert!(!adapter.reset_kit_vm(&spec).unwrap());
    runner.fail_next_create("retry reached stock create");
    assert!(matches!(
        adapter.ensure_kit_vm(&spec),
        Err(SbxError::CommandFailed {
            operation: "create local kit VM",
            ..
        })
    ));
    let creates = runner
        .invocations()
        .into_iter()
        .filter(|invocation| {
            invocation
                .arguments
                .first()
                .is_some_and(|argument| argument == "create")
        })
        .count();
    assert_eq!(creates, 2);
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);

    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
    fs::remove_file(worker).unwrap();
}

#[test]
fn shell_create_timeout_is_typed_and_does_not_enter_ready_cache() {
    let runner = FakeRunner::with_outputs([]);
    runner.timeout_next_create();
    let timeout = Duration::from_millis(40);
    let adapter = StockSbx::new("sbx", runner).with_preparation_timeouts(PreparationTimeouts {
        sbx: timeout,
        build: timeout,
        load: timeout,
        docker: timeout,
    });
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());

    assert!(matches!(
        adapter.ensure_shell_vm(&spec),
        Err(SbxError::PreparationTimeout {
            operation: "create shell VM",
            ..
        })
    ));
    assert!(adapter.ready_shells.lock().unwrap().is_empty());
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn published_kit_create_timeout_is_typed() {
    let runner = FakeRunner::with_outputs([]);
    runner.timeout_next_create();
    let timeout = Duration::from_millis(40);
    let adapter = StockSbx::new("sbx", runner).with_preparation_timeouts(PreparationTimeouts {
        sbx: timeout,
        build: timeout,
        load: timeout,
        docker: timeout,
    });
    let home = selected_home();
    let spec = KitVmSpec {
        name: "marsh-kit-published".into(),
        worker_binary: artifact(),
        workload_kit: NativeKitRef::immutable_oci(format!(
            "registry.example/fixture@sha256:{}",
            "a".repeat(64)
        ))
        .unwrap(),
        lifecycle_workspace: home.clone(),
    };

    assert!(matches!(
        adapter.create_published_vm(&spec),
        Err(SbxError::PreparationTimeout {
            operation: "create kit VM",
            ..
        })
    ));
    fs::remove_dir(home).unwrap();
    fs::remove_file(spec.worker_binary).unwrap();
}

#[test]
fn local_v3_rejects_outer_and_direct_build_digest_mismatch() {
    let source = local_kit_source();
    let home = selected_home();
    let worker = artifact();
    let outer_manifest = format!("sha256:{}", "a".repeat(64));
    let runner = FakeRunner::with_outputs([ok(), ok(), local_inspect(&outer_manifest), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    assert!(matches!(
        adapter.ensure_kit_vm(&KitVmSpec {
            name: "marsh-kit-local-source".into(),
            worker_binary: worker.clone(),
            workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
            lifecycle_workspace: home.clone(),
        }),
        Err(SbxError::LocalKitDigestMismatch)
    ));
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    assert!(runner.spawns.lock().unwrap().is_empty());
    assert!(
        runner.invocations().iter().any(|invocation| {
            invocation.arguments == ["rm", "--force", "marsh-kit-local-source"]
        })
    );

    fs::remove_file(worker).unwrap();
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn local_v3_nested_load_failure_removes_vm_and_all_scratch() {
    let source = local_kit_source();
    let home = selected_home();
    let worker = artifact();
    let manifest = format!("sha256:{}", "f".repeat(64));
    let runner = FakeRunner::with_outputs([ok(), ok(), local_inspect(&manifest), ok(), ok()]);
    runner.fail_next_spawn("nested load transport failed");
    let adapter = StockSbx::new("sbx", runner.clone());
    assert!(matches!(
        adapter.ensure_kit_vm(&KitVmSpec {
            name: "marsh-kit-local-source".into(),
            worker_binary: worker.clone(),
            workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
            lifecycle_workspace: home.clone(),
        }),
        Err(SbxError::Io(error)) if error.to_string() == "nested load transport failed"
    ));
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    // A failed create leaves no VM; the adapter refuses further reuse.
    assert!(!runner.present("marsh-kit-local-source"));
    fs::remove_file(worker).unwrap();
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn concurrent_local_v3_existence_checks_do_not_build() {
    let source = local_kit_source();
    let home = selected_home();
    let runner = FakeRunner::with_outputs([missing(), missing()]);
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let spec = KitVmSpec {
        name: "marsh-kit-local-source".into(),
        worker_binary: "/tmp/not-read-during-existence-check".into(),
        workload_kit: NativeKitRef::local_v3_source(source.clone()).unwrap(),
        lifecycle_workspace: home.clone(),
    };
    let threads = (0..2)
        .map(|_| {
            let adapter = Arc::clone(&adapter);
            let spec = spec.clone();
            std::thread::spawn(move || adapter.kit_vm_exists(&spec).unwrap())
        })
        .collect::<Vec<_>>();
    assert!(threads.into_iter().all(|thread| !thread.join().unwrap()));
    assert_eq!(
        runner
            .invocations()
            .iter()
            .filter(|invocation| invocation.program == Path::new("docker")
                && invocation
                    .arguments
                    .first()
                    .is_some_and(|argument| argument == "buildx"))
            .count(),
        0,
        "existence checks must not build before cold-start progress is emitted"
    );
    fs::remove_dir_all(source).unwrap();
    fs::remove_dir(home).unwrap();
}

#[test]
fn concurrent_shells_reference_count_shared_mounts() {
    let runner = FakeRunner::with_outputs([ok(), ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let sequence = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let project = canonical_temp_dir().join(format!(
        "marsh-shared-project-{}-{sequence}",
        std::process::id()
    ));
    let selected = canonical_temp_dir().join(format!(
        "marsh-shared-home-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir(&project).unwrap();
    fs::create_dir(&selected).unwrap();
    let shell = ReadyShellVm {
        name: "marsh-shell-user".into(),
        user: ShellUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/Users/alice".into(),
        },
        cold_started: false,
    };
    runner.seed_owned(&adapter, &shell.name, "running");

    let grants = shell_grants(&project, &selected);
    let first = adapter.prepare_shell_mounts(&shell, &grants).unwrap();
    let second = adapter.prepare_shell_mounts(&shell, &grants).unwrap();
    assert_eq!(runner.arguments().len(), 2);
    adapter.revoke_shell_mounts(&first).unwrap();
    assert_eq!(runner.arguments().len(), 2);
    adapter.revoke_shell_mounts(&second).unwrap();
    assert_eq!(runner.arguments().len(), 2); // retained while warm
    fs::remove_dir(project).unwrap();
    fs::remove_dir(selected).unwrap();
}

#[test]
fn cold_shell_initializes_in_one_root_exec_after_copying_the_shell() {
    let runner = FakeRunner::with_outputs(cold_shell_outputs());
    let adapter = StockSbx::new("sbx", runner.clone());
    let shell_binary = artifact();
    adapter
        .ensure_shell_vm(&shell_spec(shell_binary.clone()))
        .unwrap();
    let commands = runner.arguments();
    let verbs = commands
        .iter()
        .map(|command| command[0].as_str())
        .collect::<Vec<_>>();
    assert_eq!(verbs, ["create", "cp", "exec"]);
    let setup = &commands[2];
    assert!(setup.windows(2).any(|words| words == ["-u", "root"]));
    for variable in [
        "MARSH_SHELL_USER=alice",
        "MARSH_RELAY_UID=501",
        "MARSH_RELAY_GID=20",
        "MARSH_RELAY_RUNTIME=/run/marsh/501",
        "MARSH_SHELL_HOME=/Users/alice",
        "MARSH_RELAY_INSTALL=",
    ] {
        assert!(
            setup.windows(2).any(|words| words == ["-e", variable]),
            "{variable}"
        );
    }
    let script = setup.last().unwrap();
    // The marker is the last effect: a partial setup is never "initialized".
    assert!(script.ends_with("> /var/lib/marsh/worker.json"));
    for step in [
        "/usr/sbin/visudo -cf",
        "install -o root -g root -m 0440",
        "s#http://deb.debian.org/#https://deb.debian.org/#g",
        "usermod -aG docker",
        "test -S /var/run/docker.sock",
        "directory:$MARSH_RELAY_UID:$MARSH_RELAY_GID:700",
    ] {
        assert!(script.contains(step), "{step}");
    }
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn cold_shell_rejects_a_guest_user_with_another_numeric_identity() {
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        CommandOutput {
            exit_code: Some(SHELL_USER_MISMATCH_EXIT),
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    ]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let shell_binary = artifact();
    assert!(matches!(
        adapter.ensure_shell_vm(&shell_spec(shell_binary.clone())),
        Err(SbxError::InvalidShellUser)
    ));
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn cold_shell_co_installs_the_packaged_relay() {
    let runner = FakeRunner::with_outputs([ok(), ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let shell_binary = artifact();
    let relay = shell_binary.with_file_name("marsh-relay-linux-arm64");
    fs::write(&relay, b"relay").unwrap();
    fs::set_permissions(&relay, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let shell = adapter
        .ensure_shell_vm(&shell_spec(shell_binary.clone()))
        .unwrap();
    let copies = runner
        .arguments()
        .into_iter()
        .filter(|command| command[0] == "cp")
        .count();
    assert_eq!(copies, 2);
    assert!(runner.arguments().iter().any(|command| {
        command
            .windows(2)
            .any(|words| words == ["-e", "MARSH_RELAY_INSTALL=/tmp/marsh-relay.install"])
    }));
    let before = runner.arguments().len();
    adapter
        .launch_shell_relay(&shell, &relay, "session-1")
        .unwrap();
    assert_eq!(runner.arguments().len(), before, "relay already installed");
    fs::remove_file(relay).unwrap();
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn warm_shell_reuses_validated_initialization_without_stock_calls() {
    let runner = FakeRunner::with_outputs(cold_shell_outputs());
    let adapter = StockSbx::new("sbx", runner.clone());
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());
    assert!(adapter.ensure_shell_vm(&spec).unwrap().cold_started);
    let initialized_calls = runner.arguments().len();
    assert!(!adapter.ensure_shell_vm(&spec).unwrap().cold_started);
    assert_eq!(runner.arguments().len(), initialized_calls);
    let spawns = runner.spawns.lock().unwrap();
    assert_eq!(spawns.len(), 1);
    assert_eq!(
        spawns[0]
            .arguments
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>(),
        [
            "exec",
            "-i",
            "-u",
            "root",
            "-w",
            "/",
            "marsh-shell-cache",
            "/usr/local/bin/marsh",
            "--internal-supervisor",
        ]
    );
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn shell_vm_is_not_ready_until_supervisor_acknowledges_its_generation() {
    let runner = FakeRunner::with_outputs(cold_shell_outputs());
    runner.reject_next_shell_keepalive_readiness();
    let adapter = StockSbx::new("sbx", runner);
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());

    assert!(matches!(
        adapter.ensure_shell_vm(&spec),
        Err(SbxError::ShellLeaseLost(name)) if name == spec.name
    ));
    assert!(
        adapter
            .ready_shells
            .lock()
            .unwrap()
            .get(&spec.name)
            .is_none()
    );
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn retained_worker_readiness_wait_is_bounded() {
    let (_writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(
        read_worker_ready(Box::new(reader), Duration::from_millis(25)),
        Err(SbxError::WorkerLeaseLost(message)) if message.contains("readiness timed out")
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn retained_response_budget_drops_only_output_after_limit_and_keeps_terminal() {
    let (send, _receive) = mpsc::channel();
    let mut route = AttemptRoute {
        send,
        queued_bytes: 0,
        byte_limit: 3,
        overflowing: false,
    };
    assert!(route.admit(&WorkerResponse::Stdout {
        attempt: "attempt-1".into(),
        bytes: vec![1, 2],
    }));
    assert!(!route.admit(&WorkerResponse::Stderr {
        attempt: "attempt-1".into(),
        bytes: vec![3, 4],
    }));
    assert!(route.overflowing);
    assert_eq!(route.queued_bytes, 2);
    assert!(route.admit(&WorkerResponse::Terminal {
        attempt: "attempt-1".into(),
        report: marsh_worker::WorkerReport {
            container_id: None,
            execution: marsh_worker::ExecutionOutcome::SupervisionFailed,
            delivery: marsh_worker::DeliveryOutcome::Failed,
            control_errors: 1,
            retained_processes: Vec::new(),
            cleanup: marsh_worker::CleanupOutcome::Verified,
            quarantine: false,
        },
    }));
}

#[test]
fn lost_shell_lease_reinstalls_before_returning_cached_vm() {
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());
    let mut outputs = cold_shell_outputs();
    // A dead supervisor revalidates the owned VM marker, then reinstalls.
    outputs.extend(existing_shell_outputs(&spec));
    let runner = FakeRunner::with_outputs(outputs);
    let adapter = StockSbx::new("sbx", runner.clone());
    adapter.ensure_shell_vm(&spec).unwrap();
    runner.exit_spawn(0);

    assert!(!adapter.ensure_shell_vm(&spec).unwrap().cold_started);
    assert_eq!(runner.spawns.lock().unwrap().len(), 2);
    assert_eq!(
        runner
            .arguments()
            .iter()
            .filter(|arguments| arguments.first().is_some_and(|value| value == "cp"))
            .count(),
        2
    );
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn changed_shell_artifact_forces_revalidation_and_reinstall() {
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());
    let mut outputs = cold_shell_outputs();
    outputs.extend(existing_shell_outputs(&spec));
    let runner = FakeRunner::with_outputs(outputs);
    let adapter = StockSbx::new("sbx", runner.clone());
    adapter.ensure_shell_vm(&spec).unwrap();
    fs::write(&shell_binary, b"changed shell artifact").unwrap();
    adapter.ensure_shell_vm(&spec).unwrap();
    let copies = runner
        .arguments()
        .into_iter()
        .filter(|arguments| arguments.first().is_some_and(|value| value == "cp"))
        .count();
    assert_eq!(copies, 2);
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn dead_idle_supervisor_revalidates_the_vm_before_a_new_generation() {
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());
    let mut outputs = cold_shell_outputs();
    outputs.extend(cold_shell_outputs());
    let runner = FakeRunner::with_outputs(outputs);
    let adapter = StockSbx::new("sbx", runner.clone());
    let vm = adapter.ensure_shell_vm(&spec).unwrap();
    let initialized_calls = runner.arguments().len();
    // The VM disappears out of band; its supervisor transport dies with it.
    runner.inventory.lock().unwrap().remove(&spec.name);
    runner.exit_spawn(0);
    let error = adapter
        .attach_shell(&vm, Path::new("/Users/alice/project"), false, None, &[])
        .err()
        .unwrap();
    assert!(matches!(error, SbxError::ShellLeaseLost(_)));
    assert!(adapter.ensure_shell_vm(&spec).unwrap().cold_started);
    assert!(runner.arguments().len() > initialized_calls);
    assert_eq!(runner.spawns.lock().unwrap().len(), 2);
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn warm_relay_install_is_cached_but_session_runtime_stays_per_invocation() {
    let runner = FakeRunner::with_outputs([ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let relay = artifact();
    let shell = ReadyShellVm {
        name: "marsh-shell-user".into(),
        user: ShellUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/Users/alice".into(),
        },
        cold_started: false,
    };
    adapter.start_test_supervisor(&shell.name);
    let first = adapter
        .launch_shell_relay(&shell, &relay, "session-1")
        .unwrap();
    let second = adapter
        .launch_shell_relay(&shell, &relay, "session-2")
        .unwrap();
    assert_ne!(first.socket_path, second.socket_path);
    let commands = runner.arguments();
    assert_eq!(
        commands
            .iter()
            .filter(|arguments| arguments.first().is_some_and(|value| value == "cp"))
            .count(),
        1
    );
    assert_eq!(commands.len(), 3, "warm relay launch adds no stock call");
    // Relays are supervisor children, not stock processes.
    assert_eq!(runner.spawns.lock().unwrap().len(), 1);
    let starts = supervisor_starts(&runner);
    assert_eq!(starts.len(), 2);
    for (spec, session) in starts.iter().zip(["session-1", "session-2"]) {
        assert_eq!(
            spec.kind,
            shell_supervisor::StartKind::Child {
                uid: 1000,
                gid: 1000
            }
        );
        assert_eq!(spec.program, RELAY_PATH.as_bytes());
        assert_eq!(
            spec.arguments[0],
            format!("/run/marsh/1000/{session}/s").into_bytes()
        );
    }
    fs::remove_file(relay).unwrap();
}

#[test]
fn parallel_cold_relays_install_one_complete_artifact_before_launch() {
    let runner = Arc::new(SlowInstallRunner::default());
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let relay = artifact();
    let shell = ReadyShellVm {
        name: "marsh-shell-parallel-relay".into(),
        user: ShellUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/Users/alice".into(),
        },
        cold_started: false,
    };
    adapter.start_test_supervisor(&shell.name);
    let start = Arc::new(std::sync::Barrier::new(8));
    let tasks = (0..8)
        .map(|index| {
            let adapter = Arc::clone(&adapter);
            let relay = relay.clone();
            let shell = shell.clone();
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                adapter
                    .launch_shell_relay(&shell, &relay, &format!("session-{index}"))
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    for task in tasks {
        let _ = task.join().unwrap();
    }
    assert_eq!(runner.copies.load(Ordering::SeqCst), 1);
    fs::remove_file(relay).unwrap();
}

#[test]
fn concurrent_cold_shell_initialization_creates_exactly_once() {
    let image = OciImage::parse(format!("dhi.example/shell@sha256:{}", "f".repeat(64))).unwrap();
    let marker = serde_json::to_vec(&StockSbx::marker(&format!(
        "shell:{}:{}",
        "marsh-shell-concurrent",
        image.as_str()
    )))
    .unwrap();
    let uid = CommandOutput {
        exit_code: Some(0),
        stdout: b"501\n".to_vec(),
        stderr: Vec::new(),
    };
    let gid = CommandOutput {
        exit_code: Some(0),
        stdout: b"20\n".to_vec(),
        stderr: Vec::new(),
    };
    let runner = FakeRunner::with_outputs([
        ok(),
        ok(),
        ok(),
        ok(),
        uid.clone(),
        gid.clone(),
        ok(),
        ok(),
        CommandOutput {
            exit_code: Some(0),
            stdout: marker,
            stderr: Vec::new(),
        },
        ok(),
        ok(),
        ok(),
        uid,
        gid,
        ok(),
    ]);
    let adapter = Arc::new(StockSbx::new("sbx", runner.clone()));
    let shell_binary = artifact();
    let spec = ShellVmSpec {
        name: "marsh-shell-concurrent".into(),
        image,
        shell_binary: shell_binary.clone(),
        user: ShellUser {
            name: "alice".into(),
            uid: 501,
            gid: 20,
            home: "/Users/alice".into(),
        },
    };
    let first_adapter = Arc::clone(&adapter);
    let first_spec = spec.clone();
    let first = std::thread::spawn(move || first_adapter.ensure_shell_vm(&first_spec));
    let second_adapter = Arc::clone(&adapter);
    let second = std::thread::spawn(move || second_adapter.ensure_shell_vm(&spec));

    let first = first.join().unwrap().unwrap();
    let second = second.join().unwrap().unwrap();
    assert_ne!(first.cold_started, second.cold_started);
    assert_eq!(
        runner
            .arguments()
            .iter()
            .filter(|arguments| arguments.first().is_some_and(|value| value == "create"))
            .count(),
        1
    );
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn relay_uses_private_runtime_paths_and_no_secret_argument() {
    let runner = FakeRunner::with_outputs([ok(), ok(), ok(), ok(), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let relay = artifact();
    let shell = ReadyShellVm {
        name: "marsh-shell-user".into(),
        user: ShellUser {
            name: "alice".into(),
            uid: 1000,
            gid: 1000,
            home: "/Users/alice".into(),
        },
        cold_started: false,
    };
    adapter.start_test_supervisor(&shell.name);
    let launch = adapter
        .launch_shell_relay(&shell, &relay, "session-1")
        .unwrap();
    assert_eq!(launch.socket_path, Path::new("/run/marsh/1000/session-1/s"));
    assert_eq!(launch.token_path, Path::new("/run/marsh/1000/session-1/t"));
    let starts = supervisor_starts(&runner);
    let arguments = starts[0]
        .arguments
        .iter()
        .map(|word| String::from_utf8(word.clone()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        arguments,
        ["/run/marsh/1000/session-1/s", "/run/marsh/1000/session-1/t"]
    );
    assert!(starts[0].environment.is_empty());
    fs::remove_file(relay).unwrap();
}

#[test]
fn reset_shell_removes_only_the_exact_owned_shell_vm() {
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());
    let runner = FakeRunner::with_outputs([ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    runner.seed_owned(&adapter, &spec.name, "running");
    runner.seed_present("marsh-shell-other", "running");

    assert!(adapter.reset_shell_vm(&spec).unwrap());
    assert_eq!(runner.arguments(), [["rm", "--force", spec.name.as_str()]]);
    assert!(!runner.present(&spec.name));
    assert!(runner.present("marsh-shell-other"));
    assert_eq!(adapter.ownership.uuid(&spec.name), None);
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn expired_scope_deadline_starts_no_stock_sbx_effect() {
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());
    let runner = FakeRunner::default();
    let adapter = StockSbx::new("sbx", Arc::new(runner));

    assert!(matches!(
        adapter.reset_shell_vm_before(&spec, Instant::now()),
        Err(SbxError::LifecycleDeadline {
            operation: "shell reset admission"
        })
    ));
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn reset_shell_rejects_a_foreign_vm_without_removing_it() {
    let shell_binary = artifact();
    let spec = shell_spec(shell_binary.clone());
    let runner = FakeRunner::with_outputs([]);
    let adapter = StockSbx::new("sbx", runner.clone());
    // Same name, but not a UUID this daemon recorded.
    runner.seed_present(&spec.name, "running");

    assert!(matches!(
        adapter.reset_shell_vm(&spec),
        Err(SbxError::ForeignVm(_))
    ));
    assert!(
        runner.arguments().is_empty(),
        "foreign VMs are never touched"
    );
    assert!(runner.present(&spec.name));
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn scope_reset_removes_every_recorded_kit_vm_by_uuid_and_never_touches_unrecorded() {
    let runner = FakeRunner::with_outputs([ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let stale = adapter.vm_name(VmPurpose::Kit, "old-kit").unwrap();
    assert!(stale.starts_with("marsh-k-") && stale.len() == "marsh-k-".len() + 8);
    runner.seed_present(&stale, "running");
    // An in-process intent (create not yet run) survives inventory.
    let intent = adapter.vm_name(VmPurpose::Kit, "never-created").unwrap();
    runner.seed_present("user-unrelated", "running");
    let shell = adapter.vm_name(VmPurpose::Shell, "shell").unwrap();
    runner.seed_present(&shell, "running");

    // Inventory adopts the present intents (stale kit and shell).
    assert_eq!(adapter.reset_stale_scope_kit_vms().unwrap(), 1);
    let removals = runner
        .arguments()
        .into_iter()
        .filter(|args| {
            args.first()
                .is_some_and(|verb| verb == "rm" || verb == "stop")
        })
        .collect::<Vec<_>>();
    assert_eq!(removals, vec![vec!["rm", "--force", stale.as_str()]]);
    assert!(runner.present("user-unrelated"));
    assert!(
        runner.present(&shell),
        "scope Kit reset leaves the shell VM"
    );
    assert_eq!(adapter.ownership.uuid(&stale), None);
    // A later use of the same Kit gets a fresh random name.
    assert_ne!(adapter.vm_name(VmPurpose::Kit, "old-kit").unwrap(), stale);
    assert_eq!(
        adapter.vm_name(VmPurpose::Kit, "never-created").unwrap(),
        intent
    );
}

/// Host temp root with symlinked ancestry resolved (macOS `/var` ->
/// `/private/var`): grant sources reject symlinks anywhere in their chain.
fn canonical_temp_dir() -> PathBuf {
    fs::canonicalize(std::env::temp_dir()).unwrap()
}

/// Stock SBX v0.46 resolves a locally loaded template only by tag: creation
/// uses the import tag and the created VM must report the pinned digest.
#[test]
fn local_shell_template_creates_by_tag_and_removes_a_digest_mismatch() {
    let digest = format!("sha256:{}", "a".repeat(64));
    let tag = format!("sha256-{}", "c".repeat(64));
    let inspect = serde_json::json!({"image_digest": format!("sha256:{}", "b".repeat(64))});
    let runner =
        FakeRunner::with_outputs([ok(), stdout(&serde_json::to_vec(&inspect).unwrap()), ok()]);
    let adapter = StockSbx::new("sbx", runner.clone());
    let shell_binary = artifact();
    let mut spec = shell_spec(shell_binary.clone());
    spec.image = OciImage::parse(format!(
        "docker.io/library/marsh-shell-local:{tag}@{digest}"
    ))
    .unwrap();
    let error = adapter.ensure_shell_vm(&spec).unwrap_err();
    assert!(error.to_string().contains("did not resolve"), "{error}");
    let commands = runner.arguments();
    let create = commands
        .iter()
        .find(|command| command.first().is_some_and(|verb| verb == "create"))
        .unwrap();
    assert!(create.windows(2).any(|words| {
        words
            == [
                "--template",
                &format!("docker.io/library/marsh-shell-local:{tag}"),
            ]
    }));
    assert!(create.windows(2).any(|words| words == ["--pull", "never"]));
    assert_eq!(
        commands.last().unwrap(),
        &["rm", "--force", spec.name.as_str()]
    );
    assert!(!runner.present(&spec.name));

    // Without its import tag a local template is rejected before any effect.
    let untagged = FakeRunner::with_outputs([]);
    let adapter = StockSbx::new("sbx", untagged.clone());
    spec.image = OciImage::parse(format!("docker.io/library/marsh-shell-local@{digest}")).unwrap();
    assert!(adapter.ensure_shell_vm(&spec).is_err());
    assert!(untagged.arguments().is_empty());
    fs::remove_file(shell_binary).unwrap();
}

#[test]
fn kit_image_cache_pairs_records_with_archives_and_prunes() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("kit-images");
    assert!(private_cache_directory(&cache));
    let build = |fingerprint: &str, manifest: &str| {
        let scratch = temp.path().join(format!("{}.tar", &fingerprint[7..15]));
        fs::write(&scratch, manifest).unwrap();
        LocalBuild {
            source_fingerprint: fingerprint.to_owned(),
            local_tag: String::new(),
            manifest_digest: OciImage::parse(manifest.to_owned()).unwrap(),
            docker_archive: scratch,
            metadata: temp.path().join("absent.json"),
            retained: false,
            _private_scratch: None,
        }
    };
    let fingerprint = format!("sha256:{}", "a".repeat(64));
    let first = format!("sha256:{}", "1".repeat(64));
    let second = format!("sha256:{}", "2".repeat(64));
    assert!(cached_local_build(&cache, &fingerprint).is_none());
    retain_local_build(&cache, &build(&fingerprint, &first)).unwrap();
    let hit = cached_local_build(&cache, &fingerprint).unwrap();
    assert!(hit.retained && hit.manifest_digest.as_str() == first);
    assert_eq!(fs::read_to_string(&hit.docker_archive).unwrap(), first);
    // A second writer of the same fingerprint replaces the pair as a unit.
    retain_local_build(&cache, &build(&fingerprint, &second)).unwrap();
    let hit = cached_local_build(&cache, &fingerprint).unwrap();
    assert_eq!(fs::read_to_string(&hit.docker_archive).unwrap(), second);
    assert!(cached_local_build(&cache, &format!("sha256:{}", "b".repeat(64))).is_none());
    // A group-accessible cache is not trusted.
    fs::set_permissions(&cache, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(cached_local_build(&cache, &fingerprint).is_none());
    assert!(private_cache_directory(&cache));
    assert!(cached_local_build(&cache, &fingerprint).is_some());
    // Eviction drops the record; pruning keeps the newest entries only.
    evict_kit_image_cache(&cache, &hit);
    assert!(cached_local_build(&cache, &fingerprint).is_none());
    for index in 0..KIT_IMAGE_CACHE_ENTRIES + 2 {
        let fingerprint = format!("sha256:{index:064x}");
        retain_local_build(&cache, &build(&fingerprint, &first)).unwrap();
    }
    prune_kit_image_cache(&cache, SystemTime::now() + Duration::from_hours(2));
    let records = fs::read_dir(&cache)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".json")
        })
        .count();
    assert_eq!(records, KIT_IMAGE_CACHE_ENTRIES);
    let archives = fs::read_dir(&cache).unwrap().count() - records;
    assert_eq!(archives, KIT_IMAGE_CACHE_ENTRIES);
}

#[test]
fn published_kit_images_share_the_cache_by_digest() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("kit-images");
    assert!(private_cache_directory(&cache));
    let digest = format!("sha256:{}", "c".repeat(64));
    let reference = OciImage::parse(format!("docker.io/example/kit@{digest}")).unwrap();
    assert_eq!(
        published_cache_tag(&reference).unwrap(),
        format!("docker.io/example/kit:marsh-{}", "c".repeat(64))
    );
    let tagged = OciImage::parse(format!("localhost:5000/kit:v1@{digest}")).unwrap();
    assert_eq!(
        published_cache_tag(&tagged).unwrap(),
        format!("localhost:5000/kit:marsh-{}", "c".repeat(64))
    );
    assert!(published_image_digest(&OciImage::parse(digest.clone()).unwrap()).is_none());
    assert!(cached_published_image(&cache, &reference).is_none());
    // A stand-in `docker image save` whose output becomes the cache entry.
    let saved = temp.path().join("archive");
    fs::write(&saved, "oci-archive").unwrap();
    let runner = FakeRunner::with_outputs([]);
    runner
        .spawn_stdout
        .lock()
        .unwrap()
        .push_back(fs::read(&saved).unwrap());
    store_published_image(
        runner.as_ref(),
        &Invocation {
            program: "sbx".into(),
            arguments: Vec::new(),
            working_directory: None,
        },
        &cache,
        &reference,
        Duration::from_secs(5),
    )
    .unwrap();
    let archive = cached_published_image(&cache, &reference).unwrap();
    assert_eq!(fs::read_to_string(&archive).unwrap(), "oci-archive");
    // The record binds the full reference: another repository with the same
    // digest, or a local-build lookup of that key, never uses it.
    let other = OciImage::parse(format!("docker.io/other/kit@{digest}")).unwrap();
    assert!(cached_published_image(&cache, &other).is_none());
    assert!(cached_local_build(&cache, &digest).is_none());
    // Pruning keeps the pair; a failed use evicts both files.
    prune_kit_image_cache(&cache, SystemTime::now() + Duration::from_hours(2));
    assert!(cached_published_image(&cache, &reference).is_some());
    evict_published_image(&cache, &reference);
    assert!(cached_published_image(&cache, &reference).is_none());
    assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);
}

#[test]
fn worker_loss_reason_keeps_a_printable_bounded_stderr_tail() {
    let loss = WorkerLoss::default();
    assert_eq!(loss.describe(), None);
    loss.record_stderr(&vec![b'x'; 4096]);
    loss.record_stderr(b"\nthread 'main' panicked at worker.rs:1:\n\x1b[31mboom\x1b[0m\n");
    loss.set("worker transport ended: unexpected EOF (carrier exited 101)".into());
    loss.set("a later reason is ignored".into());
    let described = loss.describe().unwrap();
    assert!(described.starts_with(
        "worker transport ended: unexpected EOF (carrier exited 101); worker stderr: "
    ));
    assert!(
        described.ends_with("panicked at worker.rs:1: [31mboom [0m"),
        "{described}"
    );
    assert!(!described.chars().any(char::is_control));
    assert!(described.len() < 700);
}
