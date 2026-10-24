//! A detached run's coordinator (`docs/design/cloud-detach.md` s6): with
//! `marsh config cloud-delegate on`, the supervisor serves the workload's
//! registered-command links itself, in place of the laptop. It installs the
//! stock `sbx` CLI (a pinned release, checked by SHA-256), signs it in as
//! root with the access token the laptop uploaded into a root-only file,
//! and runs each call through the same crossing the laptop uses
//! (`marsh_daemon::cloud_call`): an agent sandbox from the Kit, the run's
//! current files copied in, streams relayed, the changes written back.
//!
//! The workload (`marsh-run`, no sudo) can only ask: the Kits the laptop
//! resolved for this run, at most `max_children` agent sandboxes, each
//! with what remains of the run's time limit. Every agent sandbox is in
//! the ownership record under the root-owned state directory and is
//! deleted when its call returns; leftovers are deleted before `done`.

use super::{INSTALLED, PATH, TRUSTED_ENV, state_path};
use marsh_contracts::cloud::{CloudRequest, DelegatePlan, LinkRequest, run_frame};
use marsh_daemon::cloud_call::{self, Caller, STEP_TIMEOUT};
use marsh_daemon::process::JobControl;
use marsh_sbx::cloud::{CloudSandboxId, CloudSbx};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

/// The stock CLI the coordinator runs: a pinned Docker Sandboxes release.
const SBX_RELEASE: &str = "https://github.com/docker/sbx-releases/releases/download/v0.49.2/DockerSandboxes-linux-arm64.tar.gz";
const SBX_RELEASE_SHA256: &str = "53c5250c480b8ef671d89413d824af62e08575c3b0051570dc5efa9f48a94eb6";
/// The uploaded Docker ID and access token (root, 0600).
pub(super) const DELEGATE_FILE: &str = "delegate";
/// One JSON line per finished agent call.
pub(super) const CALLS_FILE: &str = "children.jsonl";
/// The coordinator's sandbox ownership record (`cloud-ownership.json`).
pub(super) const CHILDREN_DIR: &str = "children";
const MAX_TOKEN: usize = 4096;
const DISPLAY_ARGS: usize = 8;
const DISPLAY_ARG_CHARS: usize = 64;

/// `--internal-cloud delegate`, as root before the run starts: keep the
/// Docker ID and token from stdin (`ID\nTOKEN\n`) in a root-only file.
/// Nothing is echoed.
pub(super) fn store_delegate() -> io::Result<()> {
    if !super::privileged() {
        return Err(io::Error::other(
            "agent calls from a detached run need the marsh cloud shell (a Kit sandbox has no root to keep the token from the workload)",
        ));
    }
    let state = super::state_dir()?;
    if state.join("started").exists() {
        return Err(io::Error::other("the run has already started"));
    }
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_TOKEN as u64 + 256)
        .read_to_end(&mut input)?;
    let (account, token) = parse_delegate(&input)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid delegate input"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(state.join(DELEGATE_FILE))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(format!("{account}\n{token}\n").as_bytes())?;
    input.fill(0);
    file.sync_all()
}

/// `ID\nTOKEN\n` with a plausible Docker ID and a token of printable,
/// non-space ASCII.
fn parse_delegate(input: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(input).ok()?;
    let mut lines = text.lines();
    let account = lines.next()?.trim();
    let token = lines.next()?.trim();
    let valid_account = !account.is_empty()
        && account.len() <= 64
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    let valid_token = !token.is_empty()
        && token.len() <= MAX_TOKEN
        && token.bytes().all(|b| b.is_ascii_graphic());
    (valid_account && valid_token).then(|| (account.to_owned(), token.to_owned()))
}

/// The trusted network settings the stock CLI needs in the sandbox.
fn trusted_environment() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    TRUSTED_ENV
        .iter()
        .filter_map(|name| Some(((*name).into(), std::env::var_os(name)?)))
        .collect()
}

fn private_directory(path: &Path) -> io::Result<PathBuf> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || std::os::unix::fs::MetadataExt::uid(&metadata) != 0 {
        return Err(io::Error::other(format!(
            "{} is not a private directory",
            path.display()
        )));
    }
    path.canonicalize()
}

/// Download the pinned stock CLI, check its digest, and keep only `sbx`.
fn install_sbx(state: &Path) -> io::Result<PathBuf> {
    let directory = private_directory(&state.join("sbx"))?;
    let program = directory.join("sbx");
    if program.exists() {
        return Ok(program);
    }
    let archive = directory.join("release.tar.gz");
    let status = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--retry", "3", "-o"])
        .arg(&archive)
        .arg(SBX_RELEASE)
        .env_clear()
        .env("PATH", PATH)
        .envs(trusted_environment())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(io::Error::other(
            "could not download the Docker Sandboxes CLI",
        ));
    }
    let mut hasher = Sha256::new();
    let mut file = File::open(&archive)?;
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    if marsh_daemon::cloud_call::hex(&hasher.finalize()) != SBX_RELEASE_SHA256 {
        let _ = fs::remove_file(&archive);
        return Err(io::Error::other(
            "the downloaded Docker Sandboxes CLI does not match its pinned SHA-256",
        ));
    }
    let status = Command::new("/usr/bin/tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&directory)
        .args(["--strip-components=1", "docker-sbx/sbx"])
        .env_clear()
        .env("PATH", PATH)
        .status()?;
    let _ = fs::remove_file(&archive);
    if !status.success() {
        return Err(io::Error::other(
            "could not unpack the Docker Sandboxes CLI",
        ));
    }
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;
    Ok(program)
}

/// Sign the stock CLI in as root with the uploaded token (stdin only).
fn sign_in(program: &Path, home: &Path, account: &str, token: &str) -> io::Result<()> {
    let mut child = Command::new(program)
        .args(["login", "--username", account, "--password-stdin"])
        .env_clear()
        .env("PATH", PATH)
        .env("HOME", home)
        .envs(trusted_environment())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(token.as_bytes())?;
        stdin.write_all(b"\n")?;
    }
    let output = child.wait_with_output()?;
    if output.status.success() {
        return Ok(());
    }
    let diagnostic = String::from_utf8_lossy(&output.stderr)
        .lines()
        .filter(|line| !line.starts_with("WARN"))
        .collect::<Vec<_>>()
        .join(" ")
        .replace(token, "[token]");
    Err(io::Error::other(format!(
        "signing in to Docker Cloud as {account} with the cloud-delegate token failed: {}",
        diagnostic.trim()
    )))
}

/// The helper's own bytes, gzip, and their SHA-256 (what agent sandboxes get).
fn helper_payload() -> io::Result<(Vec<u8>, String)> {
    let bytes = fs::read(INSTALLED)?;
    let digest = cloud_call::hex(&Sha256::digest(&bytes));
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&bytes)?;
    Ok((encoder.finish()?, digest))
}

/// One finished agent call, as the laptop records it in the job's receipt.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct CallRecord {
    pub kit: String,
    pub args: Vec<String>,
    pub exit: i32,
    pub summary: String,
    pub cause: String,
}

/// The finished calls (`children.jsonl`).
pub(super) fn call_records(state: &Path) -> Vec<CallRecord> {
    fs::read_to_string(state.join(CALLS_FILE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Agent sandboxes the coordinator still owns: `{name, id, image, agent}`.
pub(super) fn owned_sandboxes(state: &Path) -> Vec<serde_json::Value> {
    let Ok(bytes) = fs::read(state.join(CHILDREN_DIR).join("cloud-ownership.json")) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    document["entries"]
        .as_object()
        .map(|entries| {
            entries
                .values()
                .map(|entry| {
                    serde_json::json!({
                        "name": entry["name"],
                        "id": entry["id"],
                        "image": entry["image"],
                        "agent": entry["agent"],
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The coordinator's guardrails, decided on the laptop (the plan): which
/// Kits, how many agent sandboxes, and how long each may run.
struct Guard {
    plan: DelegatePlan,
    started_at: Instant,
    /// Agent sandboxes admitted so far (refused calls do not count).
    started: Mutex<u32>,
}

impl Guard {
    fn new(plan: DelegatePlan) -> Self {
        Self {
            plan,
            started_at: Instant::now(),
            started: Mutex::new(0),
        }
    }

    /// Admit one call: an allowed Kit (its pinned reference) within the cap.
    fn admit(&self, name: &str) -> Result<String, (i32, String)> {
        let Some(kit) = self.plan.kits.get(name) else {
            return Err((
                127,
                format!("marsh: {name}: not available in this detached cloud run\n"),
            ));
        };
        let mut started = self
            .started
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *started >= self.plan.max_children {
            return Err((
                125,
                format!(
                    "marsh: {name}: refused: this detached run has started {} agent sandboxes, its limit (MARSH_CLOUD_CHILDREN)\n",
                    self.plan.max_children
                ),
            ));
        }
        *started += 1;
        Ok(kit.clone())
    }

    /// What remains of the run's time limit after `elapsed` (at least the
    /// stock minimum of one minute).
    fn ttl_after(&self, elapsed: Duration) -> Duration {
        Duration::from_secs(self.plan.ttl_seconds)
            .saturating_sub(elapsed)
            .max(Duration::from_mins(1))
            .min(Duration::from_hours(24))
    }

    fn ttl(&self) -> Duration {
        self.ttl_after(self.started_at.elapsed())
    }
}

pub(super) struct Coordinator {
    adapter: Arc<CloudSbx>,
    guard: Guard,
    root: String,
    user: nix::unistd::User,
    helper: (Vec<u8>, String),
    mixin: PathBuf,
    results: PathBuf,
    state: PathBuf,
    pending: Mutex<usize>,
    idle: Condvar,
    controls: Mutex<BTreeMap<u32, JobControl>>,
    next: AtomicU32,
    /// The run is ending: no new calls.
    closing: AtomicBool,
    /// Held while a call writes back; `true` once the run's end is being
    /// recorded: no write-back after that.
    sealed: Mutex<bool>,
}

impl Coordinator {
    /// Install and sign in the stock CLI and open the ownership record.
    /// Fails before the workload starts, so a bad token refuses the run.
    pub(super) fn start(
        request: &CloudRequest,
        plan: DelegatePlan,
        user: nix::unistd::User,
    ) -> io::Result<Arc<Self>> {
        let state = state_path();
        let delegate = fs::read_to_string(state.join(DELEGATE_FILE)).map_err(|_| {
            io::Error::other("cloud-delegate: no access token was uploaded for this run")
        })?;
        let (account, token) = parse_delegate(delegate.as_bytes())
            .ok_or_else(|| io::Error::other("cloud-delegate: the uploaded token is unreadable"))?;
        let program = install_sbx(&state)?;
        let home = private_directory(&state.join("sbx-home"))?;
        sign_in(&program, &home, &account, &token)?;
        drop(token);
        let children = private_directory(&state.join(CHILDREN_DIR))?;
        let runner = Arc::new(
            marsh_runtime::SystemCommandRunner::new(&home)
                .with_stock_environment(trusted_environment()),
        );
        let adapter = Arc::new(
            CloudSbx::open(program, runner, children.clone())
                .map_err(|error| io::Error::other(error.to_string()))?,
        );
        // Proves the sign-in reaches the Sandboxes API before the run starts.
        adapter.inventory().map_err(|error| {
            io::Error::other(format!(
                "cloud-delegate: Docker Cloud refused the token: {}",
                cloud_call::actionable(&error)
            ))
        })?;
        let mixin = private_directory(&state.join("workspace-mixin"))?;
        fs::write(mixin.join("kit.yaml"), cloud_call::WORKSPACE_MIXIN)?;
        let results = private_directory(&children.join("results"))?;
        Ok(Arc::new(Self {
            adapter,
            guard: Guard::new(plan),
            root: String::from_utf8(request.root.clone())
                .map_err(|_| io::Error::other("the workspace path is not UTF-8"))?,
            user,
            helper: helper_payload()?,
            mixin,
            results,
            state,
            pending: Mutex::new(0),
            idle: Condvar::new(),
            controls: Mutex::new(BTreeMap::new()),
            next: AtomicU32::new(1),
            closing: AtomicBool::new(false),
            sealed: Mutex::new(false),
        }))
    }

    /// Serve the command links' socket until the run ends.
    pub(super) fn serve(self: &Arc<Self>, listener: UnixListener) {
        let coordinator = Arc::clone(self);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let coordinator = Arc::clone(&coordinator);
                if coordinator.closing.load(Ordering::Acquire) {
                    refuse(
                        &stream,
                        125,
                        "marsh: agent call refused: the run is ending\n",
                    );
                    continue;
                }
                *coordinator
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                std::thread::spawn(move || {
                    coordinator.call(stream);
                    let mut pending = coordinator
                        .pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    *pending = pending.saturating_sub(1);
                    coordinator.idle.notify_all();
                });
            }
        });
    }

    /// The workload's leader ended: no write-back from here on (one in
    /// progress finishes first).
    pub(super) fn seal(&self) {
        self.closing.store(true, Ordering::Release);
        *self
            .sealed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }

    /// Stop calls still running, wait for them, then delete every agent
    /// sandbox still owned. True when none is left.
    pub(super) fn settle(&self) -> bool {
        self.closing.store(true, Ordering::Release);
        let controls = self
            .controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for control in &controls {
            (control.0)("KILL");
        }
        let deadline = Instant::now() + STEP_TIMEOUT;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *pending > 0 && Instant::now() < deadline {
            pending = self
                .idle
                .wait_timeout(pending, Duration::from_secs(1))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        drop(pending);
        self.delete_leftovers()
    }

    fn delete_leftovers(&self) -> bool {
        let Ok(records) = self.adapter.reconcile() else {
            return false;
        };
        let mut clean = true;
        for record in records {
            match record.id {
                Some(id) => clean &= self.adapter.retire(&id).is_ok(),
                None => clean = false,
            }
        }
        clean
    }

    /// Admit one call: refused once the run is ending, else as the guard says.
    fn admit(&self, name: &str) -> Result<String, (i32, String)> {
        if self.closing.load(Ordering::Acquire) {
            return Err((125, format!("marsh: {name}: refused: the run is ending\n")));
        }
        self.guard.admit(name)
    }

    #[allow(clippy::needless_pass_by_value)] // The call owns its connection until it ends.
    fn call(&self, stream: UnixStream) {
        let Ok(Some((run_frame::DATA, body))) = run_frame::read(&mut &stream) else {
            return;
        };
        let Ok(request) = serde_json::from_slice::<LinkRequest>(&body) else {
            refuse(
                &stream,
                125,
                "marsh: agent call refused: unreadable request\n",
            );
            return;
        };
        let name = request.command.clone();
        let kit = match self.admit(&name) {
            Ok(kit) => kit,
            Err((code, message)) => {
                refuse(&stream, code, &message);
                return;
            }
        };
        let args = request
            .argv
            .iter()
            .take(DISPLAY_ARGS)
            .map(|arg| {
                String::from_utf8_lossy(arg)
                    .chars()
                    .take(DISPLAY_ARG_CHARS)
                    .collect()
            })
            .collect::<Vec<String>>();
        let caller: Arc<dyn Caller> = match stream.try_clone() {
            Ok(out) => Arc::new(LinkCaller(Mutex::new(out))),
            Err(_) => return,
        };
        let input = link_input(&stream);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let outcome = self.run(id, &name, &kit, request, input, Arc::clone(&caller));
        self.controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        let record = CallRecord {
            kit: name,
            args,
            exit: outcome.code,
            summary: outcome.summary,
            cause: outcome.cause,
        };
        if let Ok(line) = serde_json::to_string(&record) {
            let _ = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(self.state.join(CALLS_FILE))
                .and_then(|mut file| writeln!(file, "{line}"));
        }
        let _ = run_frame::write(&mut &stream, run_frame::EXIT, &outcome.code.to_be_bytes());
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    fn run(
        &self,
        id: u32,
        name: &str,
        kit: &str,
        request: LinkRequest,
        input: mpsc::Receiver<Option<Vec<u8>>>,
        caller: Arc<dyn Caller>,
    ) -> cloud_call::ChildOutcome {
        let failed = |message: String| {
            caller.diagnostic(format!("marsh: {name}: {message}\n").into_bytes());
            cloud_call::ChildOutcome {
                execution: marsh_daemon::ExecutionOutcome::NotStarted,
                code: 125,
                cause: format!("cloud_setup_failed: {message}"),
                cleanup: marsh_daemon::CleanupState::NotRequired,
                summary: String::new(),
            }
        };
        let Ok(kit) = marsh_contracts::OciImage::parse(kit.to_owned()) else {
            return failed("the run's Kit reference is invalid".into());
        };
        let manifest = match self.adapter.kit_manifest(&kit) {
            Ok(manifest) => manifest,
            Err(error) => return failed(cloud_call::actionable(&error)),
        };
        let call = cloud_call::ChildCall {
            adapter: &self.adapter,
            kit: &kit,
            manifest,
            mixin: &self.mixin,
            root: &self.root,
            cpus: self.guard.plan.cpus,
            ttl: self.guard.ttl(),
            stop_on_timeout: true,
            helper: &self.helper,
            payload: self.results.join(format!("{id}.delta")),
        };
        let parent = LocalParent {
            coordinator: self,
            id,
        };
        cloud_call::run_child(&call, &parent, name, request, input, caller)
    }

    /// A helper step as the workload user, in a clean environment.
    fn as_workload(&self, op: &str) -> Command {
        let mut command = Command::new(INSTALLED);
        command
            .args(["--internal-cloud", op, &self.root])
            .env_clear()
            .env("PATH", PATH)
            .env("HOME", &self.user.dir)
            .current_dir("/")
            .uid(self.user.uid.as_raw())
            .gid(self.user.gid.as_raw());
        command
    }
}

/// The coordinator's side of one call: the run's own workspace, here.
struct LocalParent<'a> {
    coordinator: &'a Coordinator,
    id: u32,
}

impl cloud_call::ChildParent for LocalParent<'_> {
    fn archive(&self, request: Vec<u8>) -> Result<cloud_call::Archive, String> {
        let mut child = self
            .coordinator
            .as_workload("archive")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&request).map_err(|e| e.to_string())?;
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "archive has no output".to_owned())?;
        let mut stderr = child.stderr.take();
        let errors = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(stderr) = stderr.as_mut() {
                let _ = stderr.take(64 * 1024).read_to_end(&mut bytes);
            }
            bytes
        });
        Ok(cloud_call::Archive {
            stdout: Box::new(stdout),
            finish: Box::new(move || {
                let status = child.wait().map_err(|e| e.to_string())?;
                let diagnostic =
                    String::from_utf8_lossy(&errors.join().unwrap_or_default()).into_owned();
                if status.success() {
                    Ok(())
                } else {
                    Err(diagnostic.trim().to_owned())
                }
            }),
        })
    }

    fn apply(&self, payload: &Path) -> Result<String, String> {
        let sealed = self
            .coordinator
            .sealed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *sealed {
            return Err("the detached run ended before the call returned".into());
        }
        let file = File::open(payload).map_err(|e| e.to_string())?;
        let output = self
            .coordinator
            .as_workload("apply")
            .stdin(Stdio::from(file))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| e.to_string())?;
        drop(sealed);
        cloud_call::apply_report(
            &output.stdout,
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).trim(),
        )
    }

    fn cancelled(&self) -> bool {
        self.coordinator.closing.load(Ordering::Acquire)
    }

    fn running(&self, _id: &CloudSandboxId, control: JobControl) -> Result<(), String> {
        if self.cancelled() {
            return Err("the run is ending".into());
        }
        self.coordinator
            .controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.id, control);
        Ok(())
    }
}

/// The calling link's streams: output frames back over its socket.
struct LinkCaller(Mutex<UnixStream>);

impl LinkCaller {
    fn write(&self, tag: u8, bytes: &[u8]) {
        let mut stream = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for chunk in bytes.chunks(run_frame::MAX_PAYLOAD) {
            if run_frame::write(&mut *stream, tag, chunk).is_err() {
                return;
            }
        }
    }
}

impl Caller for LinkCaller {
    fn deliver(&self, tag: u8, bytes: Vec<u8>) {
        let tag = if tag == run_frame::STDOUT {
            run_frame::STDOUT
        } else {
            run_frame::STDERR
        };
        self.write(tag, &bytes);
    }
    fn forwarded(&self, _len: u64) {}
    fn input_closed(&self) {}
    fn diagnostic(&self, bytes: Vec<u8>) {
        self.write(run_frame::STDERR, &bytes);
    }
}

/// The calling link's stdin frames, as the crossing's input queue.
fn link_input(stream: &UnixStream) -> mpsc::Receiver<Option<Vec<u8>>> {
    let (sender, input) = mpsc::channel();
    if let Ok(mut reader) = stream.try_clone() {
        std::thread::spawn(move || {
            loop {
                match run_frame::read(&mut reader) {
                    Ok(Some((run_frame::DATA, bytes))) => {
                        if sender.send(Some(bytes)).is_err() {
                            return;
                        }
                    }
                    Ok(Some((run_frame::EOF, _)) | None) | Err(_) => break,
                    Ok(Some(_)) => {}
                }
            }
            let _ = sender.send(None);
        });
    }
    input
}

fn refuse(stream: &UnixStream, code: i32, message: &str) {
    let _ = run_frame::write(&mut &*stream, run_frame::STDERR, message.as_bytes());
    let _ = run_frame::write(&mut &*stream, run_frame::EXIT, &code.to_be_bytes());
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(max_children: u32) -> DelegatePlan {
        DelegatePlan {
            kits: BTreeMap::from([(
                "codex".to_owned(),
                format!("docker.io/docker/codex@sha256:{}", "c".repeat(64)),
            )]),
            max_children,
            ttl_seconds: 7200,
            cpus: 2,
            account: "someone".into(),
            vm_prefix: "marsh-".into(),
        }
    }

    #[test]
    fn only_the_runs_kits_start_and_never_past_the_cap() {
        let guard = Guard::new(plan(8));
        assert!(matches!(guard.admit("claude"), Err((127, _))));
        for _ in 0..8 {
            assert_eq!(
                guard.admit("codex").unwrap(),
                format!("docker.io/docker/codex@sha256:{}", "c".repeat(64))
            );
        }
        let (code, message) = guard.admit("codex").unwrap_err();
        assert_eq!(code, 125);
        assert!(
            message.contains("limit (MARSH_CLOUD_CHILDREN)"),
            "{message}"
        );
        // A refused name never used a slot.
        let guard = Guard::new(plan(1));
        assert!(guard.admit("pi").is_err());
        assert!(guard.admit("codex").is_ok());
    }

    #[test]
    fn a_child_gets_what_remains_of_the_runs_time_limit() {
        let guard = Guard::new(plan(8));
        assert_eq!(guard.ttl_after(Duration::ZERO), Duration::from_hours(2));
        assert_eq!(
            guard.ttl_after(Duration::from_mins(90)),
            Duration::from_mins(30)
        );
        // Never below the stock minimum, even past the limit.
        assert_eq!(
            guard.ttl_after(Duration::from_hours(3)),
            Duration::from_mins(1)
        );
    }

    #[test]
    fn the_delegate_input_is_an_id_and_one_token() {
        assert_eq!(
            parse_delegate(b"markc741\ndckr_pat_abc-123\n"),
            Some(("markc741".into(), "dckr_pat_abc-123".into()))
        );
        for bad in [
            &b""[..],
            b"id\n",
            b"\ntoken\n",
            b"id with space\ntoken\n",
            b"id\ntok en\n",
            b"id\ntok\x01en\n",
        ] {
            assert_eq!(parse_delegate(bad), None, "{bad:?}");
        }
    }
}
