//! Session-origin cloud placement (`docs/design/cloud-placement.md`): one
//! disposable Docker Cloud sandbox per `cloud { ... }` / `marsh cloud` node.
//!
//! Phases: admit, snapshot the caller's view, provision an owned sandbox,
//! upload the static `marsh-local` helper and the snapshot, run with live
//! streams, confirm the command ended (its process group killed), capture the
//! exact delta, apply it to the same view, then delete the sandbox and observe
//! its absence. Nothing is replayed after a lost transport: an unconfirmed end
//! is never captured, and a failed apply keeps its payload.

use crate::{StockDaemonBackend, backend_error, merge_exported_environment};
use marsh_contracts::OciImage;
use marsh_contracts::cloud::{CloudMode, CloudRequest, LinkRequest, MAX_ARCHIVE, run_frame};
use marsh_daemon::cloud_call::{
    self, CONTROL_TIMEOUT, CallHost, CallReply, Caller, Ctl, LOCAL_ONLY, STEP_TIMEOUT,
    SharedBuffer, Step, WORKSPACE_MIXIN, actionable, capture_delta, confirm_status,
    failed_before_start, hex, os_args, read_bounded, retried_step, run_carrier, signal_control,
    small_step, spawn_run, step, summary, upload_helper,
};
use marsh_daemon::cloud_fs::{self, SNAPSHOT_LIMIT};
use marsh_daemon::process::JobControl;
use marsh_daemon::split_fs::Dir;
use marsh_daemon::{
    AttachmentFrame, CleanupState, DaemonError, DaemonStore, ExecuteSpec, ExecutionOutcome,
    ExitStatus, NewJob, PublicMount, ServerAttachment, TimingReport,
};
use marsh_runtime::Attachment;
use marsh_sbx::cloud::{CloudError, CloudImageSpec, CloudSandboxId, CloudSbx};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

mod detach;

/// The pinned cloud shell image (`docs/plan/cloud-feasibility.md`).
pub const DEFAULT_CLOUD_IMAGE: &str = "docker.io/docker/sandbox-templates:shell-docker@sha256:b2c007bbb5dcdb60a29f3019c7160ebe0c50c968e08854fbba6093834f8f6a1b";
const TTL: Duration = Duration::from_hours(4);
const STDIN_QUEUE: usize = marsh_daemon::SHELL_STDIN_WINDOW + 1;

/// Lazily opened owned cloud boundary and per-destination import locks.
#[derive(Default)]
pub(crate) struct CloudPlacement {
    adapter: Mutex<Option<Arc<CloudSbx>>>,
    imports: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
    collector: detach::Collector,
}

/// The caller's `--image`, else the daemon's `MARSH_CLOUD_IMAGE`, else the
/// pinned default. It must be digest-pinned (checked again at create).
fn image(requested: Option<&str>) -> Result<OciImage, DaemonError> {
    let reference = requested.map_or_else(
        || std::env::var("MARSH_CLOUD_IMAGE").unwrap_or_else(|_| DEFAULT_CLOUD_IMAGE.to_owned()),
        ToOwned::to_owned,
    );
    if !reference.contains("@sha256:") {
        return Err(DaemonError::Refused(
            "cloud: the image must be pinned by digest (REPOSITORY@sha256:...)".into(),
        ));
    }
    OciImage::parse(reference)
        .map_err(|_| DaemonError::Refused("cloud: not a valid image reference".into()))
}

/// A pipe whose write end the snapshot writer fills while the read end
/// feeds the remote unpack: the snapshot is never spooled on the host.
type SnapshotResult = Result<(cloud_fs::SnapshotStats, cloud_fs::GitOutcome), String>;

fn snapshot_pipe(
    root: Dir,
    filtered: bool,
    git: cloud_fs::GitView,
    request: CloudRequest,
) -> io::Result<(io::PipeReader, thread::JoinHandle<SnapshotResult>)> {
    let (reader, writer) = io::pipe()?;
    let handle = thread::spawn(move || {
        let compressed =
            flate2::write::GzEncoder::new(io::BufWriter::new(writer), flate2::Compression::fast());
        let (compressed, stats, git) =
            cloud_fs::write_snapshot(&root, filtered, &git, &request, compressed, SNAPSHOT_LIMIT)?;
        compressed
            .finish()
            .and_then(|mut writer| writer.flush())
            .map_err(|e| e.to_string())?;
        Ok((stats, git))
    });
    Ok((reader, handle))
}

/// Settles the receipt, forwards the final status, and always tries to
/// remove the sandbox before reporting.
struct Settlement<'a> {
    store: &'a DaemonStore,
    attachment: &'a ServerAttachment,
    job_id: String,
    timing: TimingReport,
    started: Instant,
    phase: Instant,
    /// All workload output reached the caller.
    output_complete: bool,
}

impl Settlement<'_> {
    fn phase(&mut self, name: &str) {
        let now = Instant::now();
        let elapsed = u64::try_from(now.duration_since(self.phase).as_millis()).unwrap_or(u64::MAX);
        self.timing.durations_ms.insert(name.into(), elapsed);
        self.phase = now;
    }

    fn note(&self, message: &str) {
        let _ = self.attachment.send(&AttachmentFrame::Stderr {
            bytes: format!("marsh: cloud: {message}\n").into_bytes(),
        });
    }

    fn finish(
        self,
        execution: ExecutionOutcome,
        code: i32,
        cause: String,
        cleanup: CleanupState,
    ) -> Result<(), DaemonError> {
        self.finish_reporting(execution, code, cause, cleanup, None)
    }

    /// `public` overrides the status sent to the caller when the receipt's
    /// settlement code is not the shell status (cancelled before start: 130).
    fn finish_reporting(
        mut self,
        execution: ExecutionOutcome,
        code: i32,
        cause: String,
        cleanup: CleanupState,
        public: Option<i32>,
    ) -> Result<(), DaemonError> {
        self.timing.wall_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.store.unregister_job_control(&self.job_id);
        self.store.finish_job_with_execution(
            &self.job_id,
            execution,
            ExitStatus {
                code: Some(code),
                cause,
            },
            self.output_complete,
            cleanup,
            std::mem::take(&mut self.timing),
        )?;
        let code = public.unwrap_or_else(|| {
            self.store
                .job(&self.job_id)
                .ok()
                .and_then(|job| job.exit.and_then(|exit| exit.code))
                .unwrap_or(code)
        });
        self.attachment.send(&AttachmentFrame::Exited { code })
    }
}

impl CloudPlacement {
    fn adapter(&self, backend: &StockDaemonBackend) -> Result<Arc<CloudSbx>, DaemonError> {
        let mut slot = self
            .adapter
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(adapter) = slot.as_ref() {
            return Ok(Arc::clone(adapter));
        }
        let directory = backend.config.control_home.join("cloud");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let directory = directory.canonicalize()?;
        let adapter = Arc::new(backend.sbx.open_cloud(directory).map_err(backend_error)?);
        // Leftovers of an earlier daemon (including sandboxes kept on purpose
        // after a failed capture) stay quarantined until
        // `marsh workers reset cloud` or their TTL; nothing is deleted here.
        *slot = Some(Arc::clone(&adapter));
        Ok(adapter)
    }

    fn import_lock(&self, root: &Path) -> Arc<Mutex<()>> {
        Arc::clone(
            self.imports
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(root.to_path_buf())
                .or_default(),
        )
    }

    /// `marsh workers reset cloud`, `stop`, `reset`: delete every owned
    /// cloud sandbox of an attached run (or an unresolved create) and
    /// observe its absence; the names deleted. A detached run's sandbox is
    /// kept (`--detached` discards those first, through collection).
    pub(crate) fn reset(&self, backend: &StockDaemonBackend) -> Result<Vec<String>, DaemonError> {
        let adapter = self.adapter(backend)?;
        let mut failures = Vec::new();
        let mut removed = Vec::new();
        for record in adapter.reconcile().map_err(backend_error)? {
            if record.detached {
                continue;
            }
            match record.id {
                Some(id) => match adapter.retire(&id) {
                    Ok(()) => removed.push(record.name),
                    Err(error) => failures.push(error.to_string()),
                },
                None => failures.push(format!(
                    "{}: create outcome still unresolved; retry later",
                    record.name
                )),
            }
        }
        if failures.is_empty() {
            Ok(removed)
        } else {
            Err(DaemonError::InvalidState(failures.join("; ")))
        }
    }
}

/// What a detached run's coordinator may do: exactly the Kits this node
/// resolved (their pinned cloud forms), the cap, and the time limit. The
/// token is not part of it.
fn delegate_plan(
    token: &marsh_daemon::cloud_delegate::DelegateToken,
    bindings: &BTreeMap<String, Binding>,
    ttl: Duration,
    cpus: u32,
) -> marsh_contracts::cloud::DelegatePlan {
    marsh_contracts::cloud::DelegatePlan {
        kits: bindings
            .iter()
            .map(|(name, binding)| {
                (
                    name.clone(),
                    marsh_contracts::cloud::DelegateKit {
                        kit: binding.kit.as_str().to_owned(),
                        ..Default::default()
                    },
                )
            })
            .collect(),
        max_children: marsh_daemon::cloud_delegate::max_children(),
        ttl_seconds: ttl.as_secs(),
        cpus,
        account: token.account.clone(),
        vm_prefix: marsh_sbx::vm_prefix().to_owned(),
    }
}

/// The caller's words: the arguments without the client's internal
/// `--stdin-terminal` and `--direct N WORD...` (always ahead of the command).
fn caller_words(arguments: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut rest = arguments;
    let mut kept = Vec::new();
    loop {
        match rest {
            [flag, tail @ ..] if flag == b"--stdin-terminal" => rest = tail,
            [flag, count, tail @ ..] if flag == b"--direct" => {
                let count = std::str::from_utf8(count)
                    .ok()
                    .and_then(|count| count.parse::<usize>().ok())
                    .unwrap_or(0)
                    .min(tail.len());
                rest = &tail[count..];
            }
            [flag, value, tail @ ..]
                if matches!(flag.as_slice(), b"--cpus" | b"--ttl" | b"--image") =>
            {
                kept.extend([flag.clone(), value.clone()]);
                rest = tail;
            }
            [flag, tail @ ..] if flag == b"--detach" => {
                kept.push(flag.clone());
                rest = tail;
            }
            _ => break,
        }
    }
    kept.extend(rest.iter().cloned());
    kept
}

/// Sandbox options chosen by the caller (`--cpus`, `--ttl`, `--image`).
#[derive(Debug, Eq, PartialEq)]
struct Options {
    cpus: u32,
    ttl: Duration,
    image: Option<String>,
    /// Internal (the client adds it): the caller's stdin is a terminal.
    stdin_terminal: bool,
    /// Internal (the client adds it): the block is exactly this one call.
    direct: Option<Vec<Vec<u8>>>,
    /// `-d`/`--detach`: a detached run (`docs/design/cloud-detach.md`).
    detach: bool,
    ttl_given: bool,
}

/// A duration such as `90s`, `30m`, `2h`, or bare seconds.
fn duration(text: &str) -> Option<Duration> {
    let (number, unit) = match text.char_indices().last()? {
        (index, 's') => (&text[..index], 1),
        (index, 'm') => (&text[..index], 60),
        (index, 'h') => (&text[..index], 3600),
        _ => (text, 1),
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(unit))
        .map(Duration::from_secs)
}

/// The parsed placement request: `[OPTION...] -c SCRIPT [NAME ARG...]` or
/// `[OPTION...] -- PROGRAM ARG...`.
fn parse(arguments: &[Vec<u8>]) -> Result<(Options, CloudMode, Vec<Vec<u8>>), DaemonError> {
    let refuse = |message: String| DaemonError::Refused(format!("cloud: {message}"));
    let mut options = Options {
        cpus: 2,
        ttl: TTL,
        image: None,
        stdin_terminal: false,
        direct: None,
        detach: false,
        ttl_given: false,
    };
    let mut rest = arguments;
    loop {
        match rest {
            [flag, value, tail @ ..] if flag == b"--cpus" => {
                options.cpus = std::str::from_utf8(value)
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .filter(|cpus| matches!(cpus, 1 | 2 | 4 | 8 | 16))
                    .ok_or_else(|| refuse("--cpus must be 1, 2, 4, 8, or 16".into()))?;
                rest = tail;
            }
            [flag, value, tail @ ..] if flag == b"--ttl" => {
                options.ttl = std::str::from_utf8(value)
                    .ok()
                    .and_then(duration)
                    .filter(|ttl| (60..=86_400).contains(&ttl.as_secs()))
                    .ok_or_else(|| {
                        refuse("--ttl must be between 1m and 24h (for example 30m or 2h)".into())
                    })?;
                options.ttl_given = true;
                rest = tail;
            }
            [flag, tail @ ..] if flag == b"--detach" => {
                options.detach = true;
                rest = tail;
            }
            [flag, count, tail @ ..] if flag == b"--direct" => {
                let count = std::str::from_utf8(count)
                    .ok()
                    .and_then(|count| count.parse::<usize>().ok())
                    .filter(|count| (1..=tail.len()).contains(count))
                    .ok_or_else(|| refuse("invalid --direct".into()))?;
                options.direct = Some(tail[..count].to_vec());
                rest = &tail[count..];
            }
            [flag, tail @ ..] if flag == b"--stdin-terminal" => {
                options.stdin_terminal = true;
                rest = tail;
            }
            [flag, value, tail @ ..] if flag == b"--image" => {
                options.image = Some(
                    String::from_utf8(value.clone())
                        .map_err(|_| refuse("--image must be UTF-8".into()))?,
                );
                rest = tail;
            }
            [flag, _, ..] if flag == b"-c" => {
                return Ok((options, CloudMode::Shell, rest.to_vec()));
            }
            // An interactive cloud shell (the client requires a terminal).
            [flag] if flag == b"-i" => return Ok((options, CloudMode::Shell, Vec::new())),
            [flag, program, tail @ ..] if flag == b"--" => {
                let mut argv = vec![program.clone()];
                argv.extend(tail.iter().cloned());
                return Ok((options, CloudMode::Argv, argv));
            }
            _ => {
                return Err(refuse(
                    "expected [--cpus N] [--ttl DURATION] [--image REF] [--detach] -c SCRIPT, -- PROGRAM, or -i"
                        .into(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token reaches the sandbox only on one step's stdin: no
    /// serialized form marsh writes (the request in the snapshot, the
    /// detached record, the receipt) or prints carries it.
    #[test]
    fn the_delegate_token_is_never_serialized() {
        const SECRET: &str = "dckr_pat_NEVER_SERIALIZED_42";
        let token =
            marsh_daemon::cloud_delegate::DelegateToken::new("someone", SECRET.as_bytes().to_vec())
                .unwrap();
        let kit =
            OciImage::parse(format!("docker.io/docker/codex@sha256:{}", "c".repeat(64))).unwrap();
        let bindings = BTreeMap::from([(
            "codex".to_owned(),
            Binding {
                kit: kit.clone(),
                identity: "codex-identity".into(),
            },
        )]);
        let plan = delegate_plan(&token, &bindings, Duration::from_hours(2), 2);
        assert_eq!(plan.kits["codex"].kit, kit.as_str());
        assert_eq!((plan.ttl_seconds, plan.account.as_str()), (7200, "someone"));
        let request = CloudRequest {
            cwd: b"/p".to_vec(),
            root: b"/p".to_vec(),
            mode: CloudMode::Shell,
            argv: vec![b"-c".to_vec(), b"codex exec x".to_vec()],
            env: BTreeMap::new(),
            terminal: None,
            kits: vec!["codex".into()],
            stdin_terminal: false,
            detached: true,
            delegate: Some(plan.clone()),
        };
        let mut receipt_calls = vec![marsh_daemon::CloudChildCall {
            kit: "codex".into(),
            args: vec!["exec".into(), "x".into()],
            exit: 0,
            summary: "wrote back 1 change".into(),
        }];
        receipt_calls.push(marsh_daemon::CloudChildCall::default());
        let forms = [
            serde_json::to_string(&request).unwrap(),
            serde_json::to_string(&plan).unwrap(),
            serde_json::to_string(&receipt_calls).unwrap(),
            format!("{request:?}"),
            format!("{token:?}"),
        ];
        for form in &forms {
            assert!(!form.contains(SECRET), "{form}");
        }
        // The upload is the only place it appears.
        assert!(
            String::from_utf8(token.upload_bytes())
                .unwrap()
                .contains(SECRET)
        );
    }

    #[test]
    fn options_precede_the_command_and_are_bounded() {
        let words = |list: &[&str]| {
            list.iter()
                .map(|w| w.as_bytes().to_vec())
                .collect::<Vec<_>>()
        };
        let (options, mode, argv) = parse(&words(&[
            "--cpus", "4", "--ttl", "30m", "-c", "make", "name",
        ]))
        .unwrap();
        assert_eq!(
            (options.cpus, options.ttl.as_secs(), mode),
            (4, 1800, CloudMode::Shell)
        );
        assert_eq!(argv, words(&["-c", "make", "name"]));
        let (_, mode, argv) = parse(&words(&["--", "prog", "--cpus", "9"])).unwrap();
        assert_eq!(
            (mode, argv),
            (CloudMode::Argv, words(&["prog", "--cpus", "9"]))
        );
        for bad in [
            &["--cpus", "3", "-c", "x"][..],
            &["--ttl", "25h", "-c", "x"],
            &["-c"],
            &["--"],
        ] {
            assert!(parse(&words(bad)).is_err(), "{bad:?}");
        }
    }
}

impl StockDaemonBackend {
    /// The cloud part of `marsh workers reset`: `cloud` names this daemon's
    /// owned sandboxes (not a Kit), and `all` includes them once cloud has
    /// been used. Uncertain cloud slots are released only after verified
    /// deletion. `Some` when the whole selection was handled here.
    pub(crate) fn reset_cloud_selection(
        &self,
        selection: &marsh_daemon::LoadSelection,
        store: &DaemonStore,
    ) -> Result<Option<Vec<String>>, DaemonError> {
        use marsh_daemon::{DaemonBackend as _, LoadSelection};
        match selection {
            LoadSelection::All => {
                if self.config.control_home.join("cloud").exists() {
                    self.cloud.reset(self)?;
                }
                Ok(None)
            }
            LoadSelection::Kits(commands) if commands.iter().any(|name| name == "cloud") => {
                let kits = commands
                    .iter()
                    .filter(|name| *name != "cloud")
                    .cloned()
                    .collect::<Vec<_>>();
                self.cloud.reset(self)?;
                let mut done = if kits.is_empty() {
                    Vec::new()
                } else {
                    self.reset_workers(&LoadSelection::Kits(kits), store.clone())?
                };
                done.push("cloud".into());
                Ok(Some(done))
            }
            LoadSelection::Kits(_) => Ok(None),
        }
    }

    /// Run one cloud placement node to settlement: from the session in the
    /// marsh shell image, or from a job in that job's own Kit (its cloud form).
    #[allow(clippy::too_many_lines)] // One ordered phase sequence with one settlement.
    #[allow(clippy::needless_pass_by_value)] // Same ownership as `execute`, which hands off here.
    pub(crate) fn execute_cloud(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError> {
        let (mut options, mode, argv) = parse(&request.arguments)?;
        if options.detach {
            if mode == CloudMode::Shell && argv.is_empty() {
                return Err(DaemonError::Refused(
                    "cloud: a detached run has no terminal; give it a command (-c SCRIPT or -- PROGRAM)"
                        .into(),
                ));
            }
            if request
                .process
                .as_ref()
                .is_some_and(|link| link.parent_job.is_some())
            {
                return Err(DaemonError::Refused(
                    "cloud: a job cannot start a detached cloud run; start it from your shell or a host terminal"
                        .into(),
                ));
            }
            if !options.ttl_given {
                options.ttl = detach::DEFAULT_TTL;
            }
        }
        let parent_job = request
            .process
            .as_ref()
            .and_then(|link| link.parent_job.clone());
        // From a job: the parent Kit's own cloud form and the parent's view.
        let origin = match request
            .process
            .as_ref()
            .and_then(|link| link.parent_job.clone())
        {
            None => None,
            Some(parent) => {
                let job = store.job(&parent)?;
                if job.placement == marsh_daemon::Placement::Cloud {
                    return Err(DaemonError::Refused(
                        "cloud: a cloud job cannot place further work in the cloud".into(),
                    ));
                }
                if options.image.is_some() {
                    return Err(DaemonError::Refused(
                        "cloud: a job's cloud placement runs in its own Kit; --image is not allowed"
                            .into(),
                    ));
                }
                let binding = self.cloud_bindings().remove(&job.command).ok_or_else(|| {
                    DaemonError::Refused(format!(
                        "cloud: {} has no cloud form: its Kit is not pinned by digest \
                         (register it from a published Kit, or name one in cloud-kits.json)",
                        job.command
                    ))
                })?;
                // The parent's own Kit, not whatever the name means now.
                if binding.identity != job.kit_ref {
                    return Err(DaemonError::Refused(format!(
                        "cloud: {} was registered again after this job started; its cloud form is not this job's Kit",
                        job.command
                    )));
                }
                Some((binding, job.mounts, job.command))
            }
        };
        // From the session, a block that is exactly one call to a registered
        // command with a cloud form (and allowed by the spawn set) runs
        // straight in that command's Kit: no cloud shell in between.
        let direct = match (&origin, &options.direct, &options.image) {
            (None, Some(call), None) => call.split_first().and_then(|(name, args)| {
                let name = std::str::from_utf8(name).ok()?.to_owned();
                let allowed = request
                    .process
                    .as_ref()
                    .and_then(|link| link.spawn.as_ref())
                    .is_none_or(|spawn| spawn.contains(&name));
                let binding = self.cloud_bindings().remove(&name).filter(|_| allowed)?;
                Some((binding, name, args.to_vec()))
            }),
            _ => None,
        };
        // A detached block has no laptop relay. With cloud-delegate on, its
        // supervisor starts agent sandboxes itself, signed in with the token
        // from the Keychain (read now: a missing token refuses the run, and
        // every block that can reach a Kit gets it, since the word check
        // below is best effort). Off, an agent call in the block is refused
        // now (its command links also refuse, if one is reached otherwise).
        let delegate = if options.detach && direct.is_none() {
            let bound = self.cloud_bindings();
            if !marsh_daemon::cloud_delegate::configured(
                &self.config.daemon_home.join("config.json"),
            ) {
                if let Some(name) = detach::nested_call(mode, &argv, &bound) {
                    return Err(DaemonError::Refused(
                        marsh_daemon::cloud_delegate::off_message(&name),
                    ));
                }
                None
            } else if bound.is_empty() {
                None
            } else {
                Some(
                    marsh_daemon::cloud_delegate::read_keychain()
                        .map_err(|message| DaemonError::Refused(format!("cloud: {message}")))?,
                )
            }
        } else {
            None
        };
        // The Kit this node runs in, if not the cloud shell: `(binding, name)`.
        let kit_form = origin
            .as_ref()
            .map(|(binding, _, command)| (binding.clone(), command.clone()))
            .or_else(|| {
                direct
                    .as_ref()
                    .map(|(binding, name, _)| (binding.clone(), name.clone()))
            });
        let ctl = Ctl {
            kit: kit_form.is_some(),
        };
        let confinement = request
            .split_confinement()
            .map_err(|reason| DaemonError::Refused(format!("cloud: {reason}")))?;
        // The immediate caller's view: its split fork, else the project.
        let (root, filtered) = match &confinement {
            Some(confinement) => (confinement.workspace.clone(), false),
            None => (request.session.launch_directory.clone(), true),
        };
        let cwd = request.effective_working_directory().to_path_buf();
        if !cwd.starts_with(&root) {
            return Err(DaemonError::Refused(format!(
                "cloud: {} is outside the project; run cloud from inside {}",
                cwd.display(),
                root.display()
            )));
        }
        let (Some(root_text), Some(_)) = (root.to_str(), cwd.to_str()) else {
            return Err(DaemonError::Refused(
                "cloud: the project path must be valid UTF-8".into(),
            ));
        };
        let root_text = root_text.to_owned();
        if let Some((_, mounts, _)) = &origin
            && !mounts
                .iter()
                .any(|mount| mount.target == root && mount.access == "read_write")
        {
            return Err(DaemonError::Refused(format!(
                "cloud: this job cannot write {}; a cloud placement writes its changes back",
                root.display()
            )));
        }
        let helper = self
            .config
            .worker_binary
            .with_file_name("marsh-local-linux-arm64");
        let image = match &kit_form {
            Some((binding, _)) => binding.kit.clone(),
            None => image(options.image.as_deref())?,
        };
        let (environment, omitted) =
            merge_exported_environment(&request.environment, &self.config.env)?;
        let environment = environment
            .into_iter()
            .filter(|(name, _)| {
                marsh_daemon::split::forwardable(name) && !LOCAL_ONLY.contains(&name.as_str())
            })
            .collect::<BTreeMap<_, _>>();
        let registered = self.command_names();
        let link = request.process.clone().unwrap_or_default();
        let (job_id, _attempt, lineage) = store.begin_cloud_job(
            NewJob {
                session_id: request.session.session_id.clone(),
                command: direct
                    .as_ref()
                    .map_or_else(|| "cloud".into(), |(_, name, _)| name.clone()),
                kit_ref: kit_form.as_ref().map_or_else(
                    || "cloud-shell".into(),
                    |(binding, _)| binding.identity.clone(),
                ),
                workload_image: image.as_str().into(),
                mounts: vec![PublicMount {
                    target: root.clone(),
                    access: "read_write".into(),
                }],
            },
            &link,
            &registered,
        )?;
        // The receipt keeps what the caller wrote, not the client's flags: a
        // direct call records its own arguments, as a local job does.
        match &direct {
            Some((_, _, words)) => store.set_job_args(&job_id, words),
            None => store.set_job_args(&job_id, &caller_words(&request.arguments)),
        }
        if origin.is_some() {
            // The parent's remaining wall time bounds the sandbox's lifetime.
            let (left, _) = store.set_job_deadline(&job_id, options.ttl.as_secs());
            options.ttl = Duration::from_secs(left.max(60));
        }
        if let Some(fanout) = request.environment.get("FANOUT_BRANCH") {
            store.set_job_fanout(&job_id, &String::from_utf8_lossy(fanout));
        }
        if let Some(split) = request.environment.get("SPLIT_ID") {
            store.set_job_consumes(&job_id, &String::from_utf8_lossy(split));
        }
        let started = Instant::now();
        let mut settle = Settlement {
            store: &store,
            attachment: &attachment,
            job_id: job_id.clone(),
            timing: TimingReport::default(),
            started,
            phase: started,
            output_complete: true,
        };
        let _ = attachment.send(&AttachmentFrame::JobStarted {
            job_id: job_id.clone(),
        });
        let relay = Relay::start(&attachment, &store, &job_id);
        for name in omitted {
            settle.note(&format!("omitted exported variable {name}"));
        }
        let not_started = |settle: Settlement<'_>, message: String| {
            settle.note(&message);
            settle.finish(
                ExecutionOutcome::NotStarted,
                125,
                format!("cloud_setup_failed: {message}"),
                CleanupState::NotRequired,
            )
        };

        if relay.cancelled() {
            settle.note("cancelled before the command started");
            return settle.finish_reporting(
                ExecutionOutcome::NotStarted,
                130,
                "cancelled".into(),
                CleanupState::NotRequired,
                Some(130),
            );
        }
        // Snapshot source: pinned by descriptor and identity now.
        let root_dir = match Dir::open_root(&root) {
            Ok(dir) => dir,
            Err(error) => return not_started(settle, format!("{}: {error}", root.display())),
        };
        let identity = match root_dir.fstat() {
            Ok(stat) => stat.identity,
            Err(error) => return not_started(settle, error.to_string()),
        };
        let interactive = mode == CloudMode::Shell && argv.is_empty();
        if interactive && !request.session.terminal {
            return Err(DaemonError::Refused(
                "cloud: an interactive cloud shell needs a terminal".into(),
            ));
        }
        // Only bindings inside this node's spawn set (MARSH_SPAWN narrows it).
        // (A direct call is the Kit itself: it calls no further agents.)
        let bindings = self
            .cloud_bindings()
            .into_iter()
            .filter(|(name, _)| lineage.spawn.contains(name) && direct.is_none())
            .collect::<BTreeMap<_, _>>();
        let mut cloud_request = CloudRequest {
            terminal: (request.session.terminal && !options.detach).then(|| {
                request
                    .session
                    .terminal_size
                    .map_or((24, 80), |size| (size.rows, size.columns))
            }),
            cwd: cwd.as_os_str().as_bytes().to_vec(),
            root: root.as_os_str().as_bytes().to_vec(),
            mode,
            argv,
            env: environment,
            kits: bindings.keys().cloned().collect(),
            stdin_terminal: options.stdin_terminal && !options.detach,
            detached: options.detach,
            delegate: delegate
                .as_ref()
                .filter(|_| !bindings.is_empty())
                .map(|token| delegate_plan(token, &bindings, options.ttl, options.cpus)),
        };

        // Provision.
        let _ = attachment.send(&AttachmentFrame::ColdBoot {
            kit: format!(
                "cloud:{}",
                match (&kit_form, &options.image) {
                    (Some((_, name)), _) => format!("Kit {name}"),
                    (None, Some(image)) => format!("image {image}"),
                    (None, None) => "marsh cloud shell".into(),
                }
            ),
            download: false,
        });
        let adapter = match self.cloud.adapter(self) {
            Ok(adapter) => adapter,
            Err(error) => return not_started(settle, error.to_string()),
        };
        // The coordinator cannot inspect Kits in the sandbox (stock `kit
        // inspect` needs a local daemon): resolve each one's name and
        // entrypoint here, from the same pinned reference.
        if let Some(plan) = cloud_request.delegate.as_mut() {
            for (name, kit) in &mut plan.kits {
                let manifest = OciImage::parse(kit.kit.clone())
                    .map_err(|_| "invalid Kit reference".to_owned())
                    .and_then(|image| {
                        adapter
                            .kit_manifest(&image)
                            .map_err(|error| actionable(&error))
                    });
                match manifest {
                    Ok(manifest) => {
                        kit.name = manifest.name;
                        kit.entrypoint = manifest.entrypoint;
                    }
                    Err(message) => {
                        return not_started(
                            settle,
                            format!("cloud-delegate: resolving the Kit of {name}: {message}"),
                        );
                    }
                }
            }
        }
        if options.detach
            && adapter
                .detached_count()
                .is_ok_and(|count| count >= marsh_sbx::cloud::MAX_DETACHED)
        {
            return not_started(
                settle,
                format!(
                    "{} detached cloud runs are outstanding; wait for one to be collected, or drop one with `marsh cloud discard JOB`",
                    marsh_sbx::cloud::MAX_DETACHED
                ),
            );
        }
        let spec = match &kit_form {
            Some((binding, _)) => match adapter
                .kit_manifest(&binding.kit)
                .map_err(|error| actionable(&error))
                .and_then(|manifest| {
                    // A direct call runs the Kit's entrypoint with its words,
                    // exactly as the call would inside a block.
                    if let Some((_, _, words)) = &direct {
                        cloud_request.mode = CloudMode::Argv;
                        cloud_request.argv = manifest
                            .entrypoint
                            .iter()
                            .map(|word| word.as_bytes().to_vec())
                            .chain(words.iter().cloned())
                            .collect();
                    }
                    self.cloud_kit_spec(
                        binding,
                        &manifest.name,
                        &root_text,
                        options.cpus,
                        options.ttl,
                    )
                    .map_err(|error| error.to_string())
                }) {
                Ok(spec) => spec,
                Err(message) => return not_started(settle, message),
            },
            None => CloudImageSpec {
                image,
                kit: None,
                cpus: options.cpus,
                memory_mib: options.cpus * 2048,
                ttl: options.ttl,
                stop_on_timeout: options.detach,
            },
        };
        let spec = CloudImageSpec {
            stop_on_timeout: options.detach,
            ..spec
        };
        let record = match adapter.create(&spec) {
            Ok(record) => record,
            Err(CloudError::CreateUncertain { name, diagnostic }) => {
                // The sandbox may exist; ownership keeps it for cleanup.
                let id = adapter
                    .reconcile()
                    .ok()
                    .and_then(|records| records.into_iter().find(|r| r.name == name))
                    .and_then(|record| record.id);
                let cleanup = match id.map(|id| adapter.retire(&id)) {
                    Some(Ok(())) => CleanupState::Verified,
                    _ => CleanupState::Uncertain,
                };
                settle.note(&actionable(&CloudError::CreateUncertain {
                    name: name.clone(),
                    diagnostic: diagnostic.clone(),
                }));
                return settle.finish(
                    ExecutionOutcome::NotStarted,
                    125,
                    format!("cloud_create_failed: {name}"),
                    cleanup,
                );
            }
            Err(error) => return not_started(settle, actionable(&error)),
        };
        let Some(id) = record.id.clone() else {
            return not_started(settle, "created sandbox has no ID".into());
        };
        settle.phase("provision");
        let _ = store.record_cloud_sandbox(&job_id, id.as_str());
        let retire = |adapter: &CloudSbx| match adapter.retire(&id) {
            Ok(()) => CleanupState::Verified,
            Err(_) => CleanupState::Uncertain,
        };
        let cancelled_setup = |settle: Settlement<'_>| {
            settle.note(
                "cancelled before the command started; nothing ran and nothing was written back",
            );
            let _ = adapter.quarantine(&id);
            let cleanup = retire(&adapter);
            let public = (cleanup == CleanupState::Verified).then_some(130);
            settle.finish_reporting(
                ExecutionOutcome::NotStarted,
                130,
                "cancelled".into(),
                cleanup,
                public,
            )
        };
        let setup_failed = |settle: Settlement<'_>, message: String| {
            settle.note(&message);
            let _ = adapter.quarantine(&id);
            let cleanup = retire(&adapter);
            settle.finish(
                ExecutionOutcome::NotStarted,
                125,
                format!("cloud_setup_failed: {message}"),
                cleanup,
            )
        };

        if relay.cancelled() {
            return cancelled_setup(settle);
        }
        // Upload the helper, then create the natural path and unpack into it.
        let helper_bytes = match fs::read(&helper) {
            Ok(bytes) => bytes,
            Err(error) => {
                return setup_failed(settle, format!("{}: {error}", helper.display()));
            }
        };
        let digest = hex(&Sha256::digest(&helper_bytes));
        let helper_bytes = {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            match encoder
                .write_all(&helper_bytes)
                .and_then(|()| encoder.finish())
            {
                Ok(bytes) => bytes,
                Err(error) => return setup_failed(settle, error.to_string()),
            }
        };
        let helper_payload = Arc::new((helper_bytes, digest.clone()));
        if let Err(message) = upload_helper(&adapter, &id, &helper_payload) {
            return setup_failed(settle, message);
        }
        let (program, words) = ctl.prepare(&root_text);
        match retried_step(&adapter, &id, &program, &words, None, CONTROL_TIMEOUT) {
            Ok(step) if step.ok() => {}
            Ok(step) => {
                return setup_failed(settle, format!("workspace path: {}", step.diagnostic()));
            }
            Err(message) => return setup_failed(settle, format!("workspace path: {message}")),
        }
        let git_view = match &confinement {
            Some(confinement) => cloud_fs::GitView::Fork {
                project: request.session.launch_directory.clone(),
                split: confinement.split.clone(),
                label: confinement.label.clone(),
                fork: confinement.workspace.clone(),
            },
            None => cloud_fs::GitView::Project,
        };
        let delegated = cloud_request.delegate.clone();
        let (pipe, writer) = match snapshot_pipe(root_dir, filtered, git_view, cloud_request) {
            Ok(pipe) => pipe,
            Err(error) => return setup_failed(settle, error.to_string()),
        };
        let unpack_script = ctl.unpack_script();
        let unpack = small_step(
            &adapter,
            &id,
            "/bin/sh",
            &["-c", &unpack_script, "sh", &root_text],
            Some(Box::new(pipe)),
            STEP_TIMEOUT,
        );
        let stats = writer
            .join()
            .unwrap_or_else(|_| Err("snapshot writer panicked".into()));
        let stats = match (stats, unpack) {
            (Ok(stats), Ok(step)) if step.ok() => stats,
            (Err(message), _) => return setup_failed(settle, format!("snapshot: {message}")),
            (_, Ok(step)) => {
                return setup_failed(settle, format!("snapshot upload: {}", step.diagnostic()));
            }
            (_, Err(message)) => {
                return setup_failed(settle, format!("snapshot upload: {message}"));
            }
        };
        settle.phase("upload");
        if let (_, cloud_fs::GitOutcome::Skipped(reason)) = &stats {
            settle.note(&format!(
                "Git metadata was not uploaded ({reason}); `git` in the block sees no repository"
            ));
        }

        if relay.cancelled() {
            return cancelled_setup(settle);
        }
        if let (Some(plan), Some(token)) = (&delegated, &delegate) {
            // The token goes only on this root-only step's stdin, into a
            // root-only file the workload cannot read.
            let mut bytes = token.upload_bytes();
            let (program, words) = ctl.step(&["--internal-cloud", "delegate"]);
            let stored = retried_step(
                &adapter,
                &id,
                &program,
                &words,
                Some(&bytes),
                CONTROL_TIMEOUT,
            );
            bytes.fill(0);
            match stored {
                Ok(step) if step.ok() => {}
                Ok(step) => {
                    return setup_failed(settle, format!("cloud-delegate: {}", step.diagnostic()));
                }
                Err(message) => return setup_failed(settle, format!("cloud-delegate: {message}")),
            }
            let _ = attachment.send(&AttachmentFrame::Stderr {
                bytes: format!(
                    "marsh: this run can start agent sandboxes in Docker Cloud as {} (cloud-delegate on)\n",
                    plan.account
                )
                .into_bytes(),
            });
        }
        drop(delegate);
        if options.detach {
            let display = match &direct {
                Some((_, _, words)) => words.clone(),
                None => caller_words(&request.arguments),
            }
            .iter()
            .map(|word| String::from_utf8_lossy(word).into_owned())
            .collect();
            return self.start_detached(
                settle,
                &store,
                &adapter,
                &id,
                &record.name,
                ctl,
                display,
                cwd,
                root,
                identity,
                confinement.is_some(),
                options.ttl,
                delegated.map(|plan| plan.account),
            );
        }
        // Execute with live streams.
        if let Err(error) = store.mark_cloud_running(&job_id, id.as_str()) {
            return setup_failed(settle, error.to_string());
        }
        let mixin = if bindings.is_empty() {
            None
        } else {
            match self.workspace_mixin() {
                Ok(mixin) => Some(mixin),
                Err(error) => return setup_failed(settle, format!("agent calls: {error}")),
            }
        };
        store.register_job_control(&job_id, signal_control(&adapter, &id, ctl));
        let carrier = match spawn_run(&adapter, &id, ctl) {
            Ok(carrier) => carrier,
            Err(message) => return setup_failed(settle, message),
        };
        if relay.cancelled() {
            // Cancelled between the last setup check and control registration.
            store.cancel_tree(&job_id, true);
        }
        let calls = mixin.map(|mixin| {
            Arc::new(AgentCalls {
                adapter: Arc::clone(&adapter),
                store: store.clone(),
                parent_job: job_id.clone(),
                parent: id.clone(),
                root: root_text.clone(),
                session_id: request.session.session_id.clone(),
                mounts: vec![PublicMount {
                    target: root.clone(),
                    access: "read_write".into(),
                }],
                registered: registered.clone(),
                bindings: bindings.clone(),
                helper: Arc::clone(&helper_payload),
                results: self.config.control_home.join("cloud").join("results"),
                parent_ctl: ctl,
                mixin,
                shape: (options.cpus, options.ttl),
                active: Mutex::new(BTreeSet::new()),
                idle: std::sync::Condvar::new(),
                pending: Mutex::new(0),
                closing: AtomicBool::new(false),
                sealed: Mutex::new(false),
            })
        });
        let delivered = run_attached(
            carrier,
            &attachment,
            &relay,
            calls.clone().map(|calls| calls as Arc<dyn CallHost>),
        );
        // Children first: cancel and settle calls still running, so their
        // changes land before this block is captured.
        if let Some(calls) = &calls {
            calls.settle();
        }
        let carrier_code = delivered.code;
        if !delivered.output_complete {
            settle.output_complete = false;
            settle.note("the command's output did not all arrive; it is incomplete");
        }
        settle.phase("execute");

        // Confirm the end before any capture: the helper records the status
        // only after killing the command's whole process group.
        let confirmed = confirm_status(&adapter, &id, ctl);
        let Some(exit_code) = confirmed else {
            settle.note(
                "lost the cloud command before it was confirmed ended; no files were written back",
            );
            let _ = adapter.quarantine(&id);
            let cleanup = retire(&adapter);
            return settle.finish(
                ExecutionOutcome::Unknown,
                125,
                format!(
                    "transport_lost: carrier exit {}",
                    carrier_code.map_or_else(|| "unknown".into(), |code| code.to_string())
                ),
                cleanup,
            );
        };

        // Capture into owner-only retained storage, then apply.
        let results = self.config.control_home.join("cloud").join("results");
        let payload = results.join(format!("{job_id}.delta"));
        let captured = capture_delta(&adapter, &id, ctl, &root_text, &results, &payload);
        settle.phase("capture");
        if let Err(message) = captured {
            // The sandbox holds the only copy of the results: keep it
            // (quarantined, owned) instead of deleting it.
            let _ = fs::remove_file(&payload);
            let _ = adapter.quarantine(&id);
            settle.note(&format!(
                "could not download the file changes: {message}; nothing was written back. \
                 The sandbox {} is kept with the files (inspect it with `sbx --cloud exec {} sh`); \
                 `marsh workers reset cloud` deletes it, and it expires within 4 hours",
                id.as_str(),
                id.as_str()
            ));
            return settle.finish(
                ExecutionOutcome::Exited { code: exit_code },
                125,
                format!("capture_failed: {message}; sandbox retained"),
                CleanupState::Uncertain,
            );
        }
        // The payload is retained on the host; the sandbox can go now.
        let cleanup = retire(&adapter);
        settle.phase("cleanup");
        if relay.lost() && confinement.is_some() {
            // The caller (a split branch) is gone; its fork may already be
            // captured or removed. Keep the result instead of writing into it.
            settle.note(&format!(
                "the caller ended before write-back; the changes are kept at {}",
                payload.display()
            ));
            return settle.finish(
                ExecutionOutcome::Exited { code: exit_code },
                125,
                "write_back_skipped: caller gone".into(),
                cleanup,
            );
        }
        let applied = {
            let lock = self.cloud.import_lock(&root);
            let _held = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // From a job: write only while that job still runs (its view
            // may be a fork already captured once it ended).
            let parent_live = parent_job.as_ref().is_none_or(|parent| {
                store.job(parent).is_ok_and(|job| {
                    matches!(
                        job.state,
                        marsh_daemon::JobState::Running | marsh_daemon::JobState::Queued
                    )
                })
            });
            parent_live.then(|| cloud_fs::apply_delta(&root, identity, &payload, MAX_ARCHIVE))
        };
        settle.phase("apply");
        let Some(applied) = applied else {
            settle.note(&format!(
                "the calling job ended before write-back; the changes are kept at {}",
                payload.display()
            ));
            return settle.finish(
                ExecutionOutcome::Exited { code: exit_code },
                125,
                "write_back_skipped: caller gone".into(),
                cleanup,
            );
        };
        match applied {
            Ok(report) => {
                let _ = fs::remove_file(&payload);
                if !report.listings.is_empty() {
                    // Before the exit status: the caller's next command
                    // (in a VM with a cached file share) lists the changes.
                    let _ = attachment.send(&AttachmentFrame::Refresh {
                        directories: report
                            .listings
                            .iter()
                            .map(|relative| {
                                root.join(std::ffi::OsStr::from_bytes(relative))
                                    .to_string_lossy()
                                    .into_owned()
                            })
                            .collect(),
                    });
                }
                if report.total() > 0 || !report.kept.is_empty() {
                    settle.note(&summary(&report));
                }
                let cause = if store.cancel_requested(&job_id) {
                    "cancelled"
                } else {
                    "exited"
                };
                settle.finish(
                    ExecutionOutcome::Exited { code: exit_code },
                    exit_code,
                    cause.into(),
                    cleanup,
                )
            }
            Err(error) => {
                settle.note(&format!(
                    "write-back stopped: {}; {} already written; the full delta is kept at {}",
                    error.message,
                    summary(&error.applied),
                    payload.display()
                ));
                settle.finish(
                    ExecutionOutcome::Exited { code: exit_code },
                    125,
                    format!("write_back_failed: {}", error.message),
                    cleanup,
                )
            }
        }
    }
}

/// The caller's side of a placement, read from admission on: stdin is queued
/// (bounded) for the remote command, and Ctrl-C or a lost caller cancels the
/// node at any phase. Before execution that stops setup; during execution it
/// reaches the remote process group through the registered job control.
struct Relay {
    input: Mutex<Option<mpsc::Receiver<Option<Vec<u8>>>>>,
    resize: Mutex<Option<mpsc::Receiver<(u16, u16)>>>,
    cancelled: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl Relay {
    fn start(attachment: &ServerAttachment, store: &DaemonStore, job_id: &str) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        // The client sends at most SHELL_STDIN_WINDOW chunks before a credit,
        // so this queue never fills and this reader never waits on stdin:
        // signals and loss are always observed.
        let (sender, input) = mpsc::sync_channel::<Option<Vec<u8>>>(STDIN_QUEUE);
        let (resizer, resize) = mpsc::channel::<(u16, u16)>();
        let attachment = attachment.clone();
        let store = store.clone();
        let job_id = job_id.to_owned();
        let (thread_cancelled, thread_lost, thread_done) =
            (Arc::clone(&cancelled), Arc::clone(&lost), Arc::clone(&done));
        thread::spawn(move || {
            let mut input = Some(sender);
            loop {
                let frame = attachment.receive();
                if thread_done.load(Ordering::Acquire) {
                    break;
                }
                let cancel = || {
                    thread_cancelled.store(true, Ordering::Release);
                    store.cancel_tree(&job_id, true);
                };
                match frame {
                    Ok(AttachmentFrame::Stdin { bytes }) => {
                        // A full queue means the client ignored its credit
                        // window: stop its input rather than block.
                        if let Some(sender) = &input
                            && sender.try_send(Some(bytes)).is_err()
                        {
                            input = None;
                            let _ = attachment.send(&AttachmentFrame::StdinClosed);
                        }
                    }
                    Ok(AttachmentFrame::StdinEof) => {
                        if let Some(sender) = input.take() {
                            let _ = sender.send(None);
                        }
                    }
                    Ok(AttachmentFrame::Signal { .. }) => cancel(),
                    Ok(AttachmentFrame::Resize { rows, columns }) => {
                        let _ = resizer.send((rows, columns));
                    }
                    Ok(_) => {}
                    Err(_) => {
                        // The caller is gone: cancel, never leave it running.
                        thread_lost.store(true, Ordering::Release);
                        cancel();
                        break;
                    }
                }
            }
        });
        Self {
            input: Mutex::new(Some(input)),
            resize: Mutex::new(Some(resize)),
            cancelled,
            lost,
            done,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
    }
}

struct AttachmentCaller(ServerAttachment);

impl Caller for AttachmentCaller {
    fn deliver(&self, tag: u8, bytes: Vec<u8>) {
        let frame = if tag == run_frame::STDOUT {
            AttachmentFrame::Stdout { bytes }
        } else {
            AttachmentFrame::Stderr { bytes }
        };
        let _ = self.0.send(&frame);
    }
    fn forwarded(&self, _len: u64) {
        // One credit per forwarded chunk (`SHELL_STDIN_WINDOW`).
        let _ = self.0.send(&AttachmentFrame::StdinCredit);
    }
    fn input_closed(&self) {
        let _ = self.0.send(&AttachmentFrame::StdinClosed);
    }
    fn diagnostic(&self, bytes: Vec<u8>) {
        let _ = self.0.send(&AttachmentFrame::Stderr { bytes });
    }
}

fn run_attached(
    carrier: Attachment,
    attachment: &ServerAttachment,
    relay: &Relay,
    calls: Option<Arc<dyn CallHost>>,
) -> cloud_call::Carried {
    fn take<T>(slot: &Mutex<Option<T>>) -> Option<T> {
        slot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
    let queue = take(&relay.input);
    let resize = take(&relay.resize);
    run_carrier(
        carrier,
        Arc::new(AttachmentCaller(attachment.clone())),
        queue,
        resize,
        calls,
    )
}

/// A registered command's Kit in Docker Cloud: the same digest-pinned v3
/// Kit the command runs locally (its image, entrypoint, network policy, and
/// credentials), or an explicit override from `cloud-kits.json` for a Kit
/// registered from source. Never a translation of the Kit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Binding {
    pub kit: OciImage,
    /// The registered Kit's identity: same-Kit chains count across places.
    pub identity: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingOverride {
    kit: String,
}

impl StockDaemonBackend {
    /// Cloud Kits of registered commands: each command's own pinned Kit, or
    /// `cloud-kits.json` in the control home (`{"NAME": {"kit": "REF@sha256:..."}}`).
    /// Commands registered from a source directory have none without one.
    pub(crate) fn cloud_bindings(&self) -> BTreeMap<String, Binding> {
        let overrides: BTreeMap<String, BindingOverride> =
            fs::read(self.config.control_home.join("cloud-kits.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
        self.command_names()
            .into_iter()
            .filter_map(|name| {
                let kit = self.registered_kit(&name).ok()?;
                let identity = kit.workload.identity().to_owned();
                let pinned = match overrides.get(&name) {
                    Some(binding) => OciImage::parse(binding.kit.clone()).ok()?,
                    None => kit.workload.workload_image()?.clone(),
                };
                pinned.as_str().contains("@sha256:").then_some((
                    name,
                    Binding {
                        kit: pinned,
                        identity,
                    },
                ))
            })
            .collect()
    }

    /// The mixin directory passed with `--kit`, written once.
    fn workspace_mixin(&self) -> Result<PathBuf, DaemonError> {
        let dir = self
            .config
            .control_home
            .join("cloud")
            .join("workspace-mixin");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let file = dir.join("kit.yaml");
        if fs::read_to_string(&file).ok().as_deref() != Some(WORKSPACE_MIXIN) {
            fs::write(&file, WORKSPACE_MIXIN)?;
        }
        Ok(dir.canonicalize()?)
    }

    /// The sandbox spec for a Kit node with its natural workspace path.
    fn cloud_kit_spec(
        &self,
        binding: &Binding,
        name: &str,
        root: &str,
        cpus: u32,
        ttl: Duration,
    ) -> Result<CloudImageSpec, DaemonError> {
        Ok(CloudImageSpec {
            image: binding.kit.clone(),
            kit: Some(marsh_sbx::cloud::CloudKit {
                name: name.to_owned(),
                mixins: vec![self.workspace_mixin()?],
                env: vec![("MARSH_CLOUD_ROOT".into(), root.to_owned())],
            }),
            cpus,
            memory_mib: cpus * 2048,
            ttl,
            stop_on_timeout: false,
        })
    }
}

/// Serves agent calls from one cloud workload: each call runs in its own
/// agent sandbox with the block's current files, and its changes return to
/// the block's live view before the call returns.
pub(crate) struct AgentCalls {
    pub adapter: Arc<CloudSbx>,
    pub store: DaemonStore,
    pub parent_job: String,
    pub parent: CloudSandboxId,
    pub root: String,
    pub session_id: String,
    pub mounts: Vec<PublicMount>,
    pub registered: Vec<String>,
    pub bindings: BTreeMap<String, Binding>,
    pub helper: Arc<(Vec<u8>, String)>,
    pub results: PathBuf,
    /// How to reach the helper in the parent sandbox.
    pub parent_ctl: Ctl,
    /// The workspace mixin directory for agent Kit sandboxes.
    pub mixin: PathBuf,
    pub shape: (u32, Duration),
    pub active: Mutex<BTreeSet<String>>,
    pub idle: std::sync::Condvar,
    /// Calls counted by `begin` and not yet finished.
    pub pending: Mutex<usize>,
    /// The parent is settling: no new calls.
    pub closing: AtomicBool,
    /// Held while a call applies into the parent; `true` once the parent's
    /// capture may start: no apply after that (a late result is kept).
    pub sealed: Mutex<bool>,
}

impl AgentCalls {
    /// The parent ended: cancel calls still running and wait until each has
    /// settled (its changes applied or kept), before the parent is captured.
    pub(crate) fn settle(&self) {
        self.closing.store(true, Ordering::Release);
        let running = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for job in &running {
            self.store.cancel_tree(job, true);
        }
        let deadline = Instant::now() + STEP_TIMEOUT;
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *pending > 0 && Instant::now() < deadline {
            // Calls admitted meanwhile are cancelled too.
            let running = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            for job in &running {
                if !self.store.cancel_requested(job) {
                    self.store.cancel_tree(job, true);
                }
            }
            pending = self
                .idle
                .wait_timeout(pending, Duration::from_secs(1))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        drop(pending);
        // From here the parent is captured: a call still running keeps its
        // result instead of writing into it.
        *self
            .sealed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    }

    fn done(&self, job: &str) {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(job);
        self.idle.notify_all();
    }

    fn finish(
        &self,
        job: &str,
        execution: ExecutionOutcome,
        code: i32,
        cause: String,
        cleanup: CleanupState,
        reply: &CallReply,
    ) {
        self.store.unregister_job_control(job);
        let _ = self.store.finish_job_with_execution(
            job,
            execution,
            ExitStatus {
                code: Some(code),
                cause,
            },
            true,
            cleanup,
            TimingReport::default(),
        );
        let code = self
            .store
            .job(job)
            .ok()
            .and_then(|receipt| receipt.exit.and_then(|exit| exit.code))
            .unwrap_or(code);
        reply.exit(code);
        self.done(job);
    }

    fn run_call(
        &self,
        request: LinkRequest,
        input: mpsc::Receiver<Option<Vec<u8>>>,
        reply: &CallReply,
    ) {
        let name = request.command.clone();
        let Some(binding) = self.bindings.get(&name).cloned() else {
            reply.stderr(&format!("marsh: {name}: not available in a cloud block\n"));
            reply.exit(127);
            return;
        };
        let link = marsh_daemon::process::ProcessLink {
            parent_job: Some(self.parent_job.clone()),
            ..Default::default()
        };
        let job = match self.store.begin_cloud_job(
            NewJob {
                session_id: self.session_id.clone(),
                command: name.clone(),
                kit_ref: binding.identity.clone(),
                workload_image: binding.kit.as_str().to_owned(),
                mounts: self.mounts.clone(),
            },
            &link,
            &self.registered,
        ) {
            Ok((job, _, _)) => job,
            Err(error) => {
                let message = match error {
                    DaemonError::Refused(message) => message,
                    other => other.to_string(),
                };
                reply.stderr(&format!("marsh: {name}: {message}\n"));
                reply.exit(125);
                return;
            }
        };
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(job.clone());
        self.store.set_job_args(&job, &request.argv);
        if self.store.cancel_requested(&job) {
            reply.stderr(&format!("marsh: {name}: cancelled\n"));
            return self.finish(
                &job,
                ExecutionOutcome::NotStarted,
                125,
                "cloud_setup_failed: cancelled".into(),
                CleanupState::NotRequired,
                reply,
            );
        }
        let manifest = match self.adapter.kit_manifest(&binding.kit) {
            Ok(manifest) => manifest,
            Err(error) => {
                let message = actionable(&error);
                reply.stderr(&format!("marsh: {name}: {message}\n"));
                return self.finish(
                    &job,
                    ExecutionOutcome::NotStarted,
                    125,
                    format!("cloud_setup_failed: {message}"),
                    CleanupState::NotRequired,
                    reply,
                );
            }
        };
        let call = cloud_call::ChildCall {
            adapter: &self.adapter,
            kit: &binding.kit,
            manifest,
            mixin: &self.mixin,
            root: &self.root,
            cpus: self.shape.0,
            ttl: self.shape.1,
            stop_on_timeout: false,
            helper: &self.helper,
            payload: self.results.join(format!("{job}.delta")),
        };
        let parent = CallParent {
            calls: self,
            job: &job,
        };
        let outcome = cloud_call::run_child(
            &call,
            &parent,
            &name,
            request,
            input,
            Arc::new(reply.clone()),
        );
        self.finish(
            &job,
            outcome.execution,
            outcome.code,
            outcome.cause,
            outcome.cleanup,
            reply,
        );
    }
}

/// The laptop's side of one call: the parent sandbox over stock exec, and
/// the call's job receipt.
struct CallParent<'a> {
    calls: &'a AgentCalls,
    job: &'a str,
}

impl cloud_call::ChildParent for CallParent<'_> {
    fn archive(&self, request: Vec<u8>) -> Result<cloud_call::Archive, String> {
        let calls = self.calls;
        let (program, args) =
            calls
                .parent_ctl
                .as_workload(&["--internal-cloud", "archive", &calls.root]);
        let Attachment {
            mut stdin,
            stdout,
            stderr,
            mut process,
            ..
        } = calls
            .adapter
            .spawn(&calls.parent, &program, &os_args(&args))
            .map_err(|e| actionable(&e))?;
        stdin.write_all(&request).map_err(|e| e.to_string())?;
        drop(stdin);
        let errors = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = read_bounded(stderr, &mut bytes, 64 * 1024);
            bytes
        });
        Ok(cloud_call::Archive {
            stdout,
            finish: Box::new(move || {
                let code = process.wait().map_err(|e| e.to_string())?;
                let diagnostic =
                    String::from_utf8_lossy(&errors.join().unwrap_or_default()).into_owned();
                if code == 0 {
                    Ok(())
                } else {
                    Err(diagnostic.trim().to_owned())
                }
            }),
        })
    }

    /// Apply in the parent as its workload user, unless the parent's capture
    /// has begun (a late result is kept instead).
    fn apply(&self, payload: &Path) -> Result<String, String> {
        let calls = self.calls;
        let sealed = calls
            .sealed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *sealed {
            return Err("the cloud block ended before the call returned".to_owned());
        }
        let file = fs::File::open(payload).map_err(|e| e.to_string())?;
        let (program, args) =
            calls
                .parent_ctl
                .as_workload(&["--internal-cloud", "apply", &calls.root]);
        let step = small_step(
            &calls.adapter,
            &calls.parent,
            &program,
            &args,
            Some(Box::new(file)),
            STEP_TIMEOUT,
        )?;
        drop(sealed);
        cloud_call::apply_report(&step.stdout, step.ok(), &step.diagnostic())
    }

    fn cancelled(&self) -> bool {
        self.calls.store.cancel_requested(self.job)
    }

    fn created(&self, id: &CloudSandboxId) {
        let _ = self.calls.store.record_cloud_sandbox(self.job, id.as_str());
    }

    fn running(&self, id: &CloudSandboxId, control: JobControl) -> Result<(), String> {
        self.calls
            .store
            .mark_cloud_running(self.job, id.as_str())
            .map_err(|error| error.to_string())?;
        self.calls.store.register_job_control(self.job, control);
        Ok(())
    }
}

impl CallHost for AgentCalls {
    fn begin(&self) -> bool {
        if self.closing.load(Ordering::Acquire) {
            return false;
        }
        *self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        true
    }

    fn call(&self, request: LinkRequest, input: mpsc::Receiver<Option<Vec<u8>>>, reply: CallReply) {
        self.run_call(request, input, &reply);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *pending = pending.saturating_sub(1);
        self.idle.notify_all();
    }
}
