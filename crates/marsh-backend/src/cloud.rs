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
use marsh_daemon::cloud_fs::{self, ApplyReport, SNAPSHOT_LIMIT};
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
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// The pinned cloud shell image (`docs/plan/cloud-feasibility.md`).
pub const DEFAULT_CLOUD_IMAGE: &str = "docker.io/docker/sandbox-templates:shell-docker@sha256:b2c007bbb5dcdb60a29f3019c7160ebe0c50c968e08854fbba6093834f8f6a1b";
/// The uploaded helper (sandbox user's private directory); `prepare` installs
/// the root-owned copy every later step runs through `sudo -n`.
const UPLOADED: &str = "/tmp/marsh-cloud/marsh-local";
const HELPER: &str = marsh_contracts::cloud::INSTALLED_HELPER;
const TTL: Duration = Duration::from_hours(4);
const STEP_TIMEOUT: Duration = Duration::from_mins(10);
const CONTROL_TIMEOUT: Duration = Duration::from_mins(1);
const STDIN_QUEUE: usize = marsh_daemon::SHELL_STDIN_WINDOW + 1;
/// Exported names that describe the caller's machine, not the sandbox.
const LOCAL_ONLY: [&str; 6] = ["SHLVL", "OLDPWD", "PWD", "TMPDIR", "SHELL", "_"];

/// Lazily opened owned cloud boundary and per-destination import locks.
#[derive(Default)]
pub(crate) struct CloudPlacement {
    adapter: Mutex<Option<Arc<CloudSbx>>>,
    imports: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
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

/// Make stock CLI failures actionable: sign-in and entitlement come first.
fn actionable(error: &CloudError) -> String {
    let text = error.to_string();
    let lower = text.to_ascii_lowercase();
    if lower.contains("login")
        || lower.contains("sign in")
        || lower.contains("unauthorized")
        || lower.contains("not authenticated")
    {
        format!(
            "{text}\nmarsh: cloud: sign in with `sbx login` using an account with Docker Cloud sandbox access"
        )
    } else if lower.contains("subscription") || lower.contains("entitle") || lower.contains("quota")
    {
        format!(
            "{text}\nmarsh: cloud: this Docker account has no Docker Cloud sandbox capacity (subscription or quota)"
        )
    } else {
        text
    }
}

/// How control steps reach the helper: the session cloud shell runs them as
/// root through `sudo` (installed helper, `marsh-run` workload); a Kit
/// sandbox has no root, so the uploaded helper runs as the Kit's own user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ctl {
    pub kit: bool,
}

impl Ctl {
    /// A control step: `(program, words)`.
    fn step(self, args: &[&str]) -> (String, Vec<String>) {
        let mut words = Vec::new();
        let program = if self.kit {
            UPLOADED.to_owned()
        } else {
            words.extend(["-n".to_owned(), HELPER.to_owned()]);
            "/usr/bin/sudo".to_owned()
        };
        words.extend(args.iter().map(|arg| (*arg).to_owned()));
        (program, words)
    }

    /// A step that touches the workspace as the workload user.
    fn as_workload(self, args: &[&str]) -> (String, Vec<String>) {
        if self.kit {
            return self.step(args);
        }
        let mut words = ["-n", "-u", "marsh-run", HELPER]
            .map(str::to_owned)
            .to_vec();
        words.extend(args.iter().map(|arg| (*arg).to_owned()));
        ("/usr/bin/sudo".to_owned(), words)
    }

    /// The first step, from the uploaded helper (it installs itself).
    fn prepare(self, root: &str) -> (String, Vec<String>) {
        let mut words = Vec::new();
        let program = if self.kit {
            UPLOADED.to_owned()
        } else {
            words.extend(["-n".to_owned(), UPLOADED.to_owned()]);
            "/usr/bin/sudo".to_owned()
        };
        words.extend(["--internal-cloud", "prepare", root].map(str::to_owned));
        (program, words)
    }

    /// `sh -c` script that unpacks a gzip snapshot from stdin into `$1`.
    fn unpack_script(self) -> String {
        if self.kit {
            format!("/usr/bin/gzip -dc | {UPLOADED} --internal-cloud unpack - \"$1\"")
        } else {
            format!(
                "/usr/bin/gzip -dc | /usr/bin/sudo -n {HELPER} --internal-cloud unpack - \"$1\""
            )
        }
    }
}

fn os_args(args: &[String]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// One stock exec step's result.
struct Step {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Step {
    fn ok(&self) -> bool {
        self.code == Some(0)
    }

    fn diagnostic(&self) -> String {
        String::from_utf8_lossy(&self.stderr)
            .chars()
            .take(1024)
            .collect::<String>()
            .trim()
            .to_owned()
    }
}

fn read_bounded(mut reader: impl Read, mut sink: impl Write, limit: u64) -> io::Result<u64> {
    let copied = io::copy(&mut (&mut reader).take(limit + 1), &mut sink)?;
    if copied > limit {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "stream exceeds its byte limit",
        ));
    }
    sink.flush()?;
    // Drain without keeping anything, so the remote side is never blocked.
    io::copy(&mut reader, &mut io::sink())?;
    Ok(copied)
}

/// Run one control step to completion within `timeout`, feeding `input` and
/// copying stdout to `stdout` (at most `limit` bytes). On timeout the local
/// carrier is terminated and the step fails; that is not proof the remote
/// command stopped, so callers quarantine on failure.
fn step(
    attachment: Attachment,
    input: Option<Box<dyn Read + Send>>,
    stdout: Box<dyn Write + Send>,
    limit: u64,
    timeout: Duration,
) -> Result<Step, String> {
    let Attachment {
        mut stdin,
        stdout: out,
        stderr: err,
        mut process,
        ..
    } = attachment;
    let feeder = thread::spawn(move || -> io::Result<()> {
        if let Some(mut input) = input {
            io::copy(&mut input, &mut stdin)?;
        }
        drop(stdin);
        Ok(())
    });
    let output = thread::spawn(move || read_bounded(out, stdout, limit));
    let errors = thread::spawn(move || {
        let mut bytes = Vec::new();
        read_bounded(err, &mut bytes, 64 * 1024).map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let code = loop {
        match process.try_wait() {
            Ok(Some(code)) => break Some(code),
            Ok(None) if Instant::now() >= deadline => {
                let _ = process.terminate();
                break None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let fed = feeder.join().map_err(|_| "input thread panicked")?;
    let copied = output.join().map_err(|_| "output thread panicked")?;
    let stderr = errors
        .join()
        .map_err(|_| "error thread panicked")?
        .unwrap_or_default();
    let Some(code) = code else {
        return Err("cloud step timed out".into());
    };
    if code == 0 {
        fed.map_err(|e| format!("cloud upload failed: {e}"))?;
        copied.map_err(|e| e.to_string())?;
    }
    Ok(Step {
        code: Some(code),
        stdout: Vec::new(),
        stderr,
    })
}

/// [`step`] keeping a small stdout in memory.
fn small_step<S: AsRef<str>>(
    adapter: &CloudSbx,
    id: &CloudSandboxId,
    program: &str,
    args: &[S],
    input: Option<Box<dyn Read + Send>>,
    timeout: Duration,
) -> Result<Step, String> {
    let args = args
        .iter()
        .map(|arg| OsString::from(arg.as_ref()))
        .collect::<Vec<_>>();
    let attachment = adapter
        .spawn(id, program, &args)
        .map_err(|e| actionable(&e))?;
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let mut result = step(
        attachment,
        input,
        Box::new(SharedBuffer(Arc::clone(&buffer))),
        64 * 1024,
        timeout,
    )?;
    result.stdout = std::mem::take(
        &mut *buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    Ok(result)
}

/// The stock CLI failed before reaching the sandbox (sign-in token refresh,
/// often contended with other stock processes, or a TLS timeout while
/// attaching the exec): the command did not run. Only repeatable steps, and
/// the at-most-once `run`, are started again on this.
fn failed_before_start(step: &Step) -> bool {
    let text = String::from_utf8_lossy(&step.stderr);
    !step.ok()
        && (text.contains("init cloud dispatcher")
            || text.contains("access token source")
            || text.contains("refresh lock held")
            || (text.contains("attach exec in cloud sandbox")
                && text.contains("TLS handshake timeout")))
}

/// Start the helper's `run` and wait for its start signal (an empty ACK).
/// When the stock CLI failed before reaching the sandbox, the run is
/// started again: the helper runs at most once per sandbox (an exclusive
/// marker), so a repeat can never run the command twice.
fn spawn_run(adapter: &CloudSbx, id: &CloudSandboxId, ctl: Ctl) -> Result<Attachment, String> {
    let (program, words) = ctl.step(&["--internal-cloud", "run"]);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let Attachment {
            stdin,
            mut stdout,
            mut stderr,
            mut process,
            control,
        } = adapter
            .spawn(id, &program, &os_args(&words))
            .map_err(|error| actionable(&error))?;
        match run_frame::read(&mut stdout) {
            Ok(Some((run_frame::ACK, payload))) if payload == 0_u64.to_be_bytes() => {
                return Ok(Attachment {
                    stdin,
                    stdout,
                    stderr,
                    process,
                    control,
                });
            }
            Ok(None) => {
                let mut diagnostic = Vec::new();
                let _ = read_bounded(&mut stderr, &mut diagnostic, 64 * 1024);
                let code = process.wait().ok();
                let step = Step {
                    code,
                    stdout: Vec::new(),
                    stderr: diagnostic,
                };
                if attempt < 4 && failed_before_start(&step) {
                    thread::sleep(Duration::from_secs(2 * attempt));
                    continue;
                }
                // Not started, or started and lost: the caller confirms.
                return Ok(Attachment {
                    stdin,
                    stdout: Box::new(io::empty()),
                    stderr: Box::new(io::Cursor::new(step.stderr)),
                    process: Box::new(Exited(code)),
                    control,
                });
            }
            Ok(Some(_)) | Err(_) => {
                return Ok(Attachment {
                    stdin,
                    stdout: Box::new(io::empty()),
                    stderr,
                    process,
                    control,
                });
            }
        }
    }
}

/// A stock exec that already ended, handed on with its exit code.
struct Exited(Option<i32>);

impl marsh_runtime::AttachedProcess for Exited {
    fn wait(&mut self) -> io::Result<i32> {
        self.0
            .ok_or_else(|| io::Error::other("the stock exec ended without a status"))
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        self.wait().map(Some)
    }
    fn terminate(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// [`small_step`] for steps that are safe to repeat when the stock CLI
/// failed before starting them; `input` is replayed whole.
fn retried_step<S: AsRef<str>>(
    adapter: &CloudSbx,
    id: &CloudSandboxId,
    program: &str,
    args: &[S],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Step, String> {
    let mut attempt = 0;
    loop {
        let feed =
            input.map(|bytes| Box::new(io::Cursor::new(bytes.to_vec())) as Box<dyn Read + Send>);
        let step = small_step(adapter, id, program, args, feed, timeout)?;
        attempt += 1;
        if attempt >= 4 || !failed_before_start(&step) {
            return Ok(step);
        }
        thread::sleep(Duration::from_secs(2 * attempt));
    }
}

struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
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

    /// `marsh workers reset cloud`: delete every owned cloud sandbox and
    /// observe its absence.
    /// Delete every owned cloud sandbox; the names deleted.
    pub(crate) fn reset(&self, backend: &StockDaemonBackend) -> Result<Vec<String>, DaemonError> {
        let adapter = self.adapter(backend)?;
        let mut failures = Vec::new();
        let mut removed = Vec::new();
        for record in adapter.reconcile().map_err(backend_error)? {
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
                    "expected [--cpus N] [--ttl DURATION] [--image REF] -c SCRIPT, -- PROGRAM, or -i"
                        .into(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            terminal: request.session.terminal.then(|| {
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
            stdin_terminal: options.stdin_terminal,
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
            },
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

fn summary(report: &ApplyReport) -> String {
    use std::fmt::Write as _;
    let plural = |count: u64| if count == 1 { "" } else { "s" };
    let mut text = format!(
        "wrote back {} change{} ({} created, {} modified, {} deleted)",
        report.total(),
        plural(report.total()),
        report.created,
        report.modified,
        report.deleted
    );
    if !report.kept.is_empty() {
        let shown = report
            .kept
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = report.kept.len().saturating_sub(5);
        let _ = write!(
            text,
            "; kept {} directories deleted remotely that still hold local files: {shown}{}",
            report.kept.len(),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        );
    }
    text
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

/// Relay queued stdin to the carrier and its output back to the caller.
/// Returns the carrier's exit code (`None` if it could not be observed).
/// Unacknowledged stdin bytes in flight to the remote workload.
struct Window {
    state: Mutex<(u64, bool)>,
    changed: std::sync::Condvar,
    limit: u64,
}

impl Default for Window {
    fn default() -> Self {
        Self::with_limit(run_frame::STDIN_WINDOW)
    }
}

impl Window {
    fn with_limit(limit: u64) -> Self {
        Self {
            state: Mutex::new((0, false)),
            changed: std::sync::Condvar::new(),
            limit,
        }
    }

    /// Wait until `len` more bytes fit; false once input is closed.
    fn reserve(&self, len: u64) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.1 && state.0 > 0 && state.0 + len > self.limit {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.1 {
            return false;
        }
        state.0 += len;
        true
    }

    fn acknowledge(&self, len: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = state.0.saturating_sub(len);
        self.changed.notify_all();
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = true;
        self.changed.notify_all();
    }
}

/// The carrier's stdin, shared by the stdin writer and output acknowledgements.
type CarrierInput = Arc<Mutex<Box<dyn Write + Send>>>;

fn carrier_frame(input: &CarrierInput, tag: u8, payload: &[u8]) -> io::Result<()> {
    let mut input = input
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    run_frame::write(&mut **input, tag, payload)
}

/// What the run carrier delivered.
struct Carried {
    /// The carrier's own exit code, if observed.
    code: Option<i32>,
    /// The helper's end-of-output marker arrived: no output was lost.
    output_complete: bool,
}

/// Who a run carrier serves: the session caller, or an agent call relayed
/// from a parent cloud workload.
pub(crate) trait Caller: Send + Sync + 'static {
    /// Workload stdout or stderr (`run_frame::STDOUT`/`STDERR`); may block.
    fn deliver(&self, tag: u8, bytes: Vec<u8>);
    /// One input chunk of `len` bytes was forwarded to the carrier.
    fn forwarded(&self, len: u64);
    /// The workload stopped reading its input.
    fn input_closed(&self);
    /// Helper diagnostics (unframed carrier stderr).
    fn diagnostic(&self, bytes: Vec<u8>);
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

/// Replies to one agent call over its parent's carrier, within the parent's
/// acknowledged window.
#[derive(Clone)]
pub(crate) struct CallReply {
    id: u32,
    input: CarrierInput,
    window: Arc<Window>,
}

impl CallReply {
    fn tagged(&self, body: &[u8]) -> Vec<u8> {
        let mut payload = self.id.to_be_bytes().to_vec();
        payload.extend_from_slice(body);
        payload
    }

    fn send(&self, tag: u8, bytes: &[u8]) {
        for chunk in bytes.chunks(run_frame::MAX_PAYLOAD - 4) {
            let len = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
            if !self.window.reserve(len) {
                return;
            }
            let _ = carrier_frame(&self.input, tag, &self.tagged(chunk));
        }
    }

    pub(crate) fn stderr(&self, text: &str) {
        self.send(run_frame::KIT_ERR, text.as_bytes());
    }

    pub(crate) fn exit(&self, code: i32) {
        let _ = carrier_frame(
            &self.input,
            run_frame::KIT_EXIT,
            &self.tagged(&code.to_be_bytes()),
        );
    }

    /// The call's stdin bytes were taken: let the parent helper send more.
    fn taken(&self, len: u64) {
        let _ = carrier_frame(&self.input, run_frame::OUTPUT_ACK, &len.to_be_bytes());
    }
}

impl Caller for CallReply {
    fn deliver(&self, tag: u8, bytes: Vec<u8>) {
        let tag = if tag == run_frame::STDOUT {
            run_frame::KIT_OUT
        } else {
            run_frame::KIT_ERR
        };
        self.send(tag, &bytes);
    }
    fn forwarded(&self, len: u64) {
        self.taken(len);
    }
    fn input_closed(&self) {}
    fn diagnostic(&self, bytes: Vec<u8>) {
        self.send(run_frame::KIT_ERR, &bytes);
    }
}

/// Serves agent calls made by a cloud workload (`KIT_OPEN`).
pub(crate) trait CallHost: Send + Sync + 'static {
    /// Count a call before its thread starts (so settlement waits for it);
    /// false once the parent is settling: refuse it.
    fn begin(&self) -> bool;
    fn call(&self, request: LinkRequest, input: mpsc::Receiver<Option<Vec<u8>>>, reply: CallReply);
}

/// Relay queued stdin to the carrier within the acknowledged window and the
/// helper's framed output back to the caller, acknowledging each delivered
/// output frame (the stock channel drops output a slow reader leaves behind).
/// Agent calls from the workload go to `calls`.
#[allow(clippy::too_many_lines)] // Cooperating stream threads and one join.
#[allow(clippy::needless_pass_by_value)] // Cloned into each stream thread.
fn run_carrier(
    carrier: Attachment,
    caller: Arc<dyn Caller>,
    queue: Option<mpsc::Receiver<Option<Vec<u8>>>>,
    resize: Option<mpsc::Receiver<(u16, u16)>>,
    calls: Option<Arc<dyn CallHost>>,
) -> Carried {
    let Attachment {
        stdin,
        stdout,
        stderr,
        mut process,
        ..
    } = carrier;
    let input: CarrierInput = Arc::new(Mutex::new(stdin));
    if let Some(resize) = resize {
        let input = Arc::clone(&input);
        thread::spawn(move || {
            while let Ok((rows, columns)) = resize.recv() {
                let mut size = rows.to_be_bytes().to_vec();
                size.extend_from_slice(&columns.to_be_bytes());
                if carrier_frame(&input, run_frame::RESIZE, &size).is_err() {
                    break;
                }
            }
        });
    }
    let window = Arc::new(Window::default());
    {
        let (caller, window, input) =
            (Arc::clone(&caller), Arc::clone(&window), Arc::clone(&input));
        thread::spawn(move || {
            let Some(queue) = queue else {
                let _ = carrier_frame(&input, run_frame::EOF, &[]);
                return;
            };
            while let Ok(Some(bytes)) = queue.recv() {
                let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if !window.reserve(len) || carrier_frame(&input, run_frame::DATA, &bytes).is_err() {
                    // The remote command stopped reading: end the input.
                    caller.input_closed();
                    return;
                }
                caller.forwarded(len);
            }
            let _ = carrier_frame(&input, run_frame::EOF, &[]);
        });
    }
    // Delivery to the caller may be slow; the carrier must still be read
    // promptly (the stock channel drops output left unread). Output waits
    // here, bounded by the helper's acknowledged window.
    let (deliver, deliveries) = mpsc::channel::<(u8, Vec<u8>)>();
    let delivery = {
        let (caller, input) = (Arc::clone(&caller), Arc::clone(&input));
        thread::spawn(move || {
            while let Ok((tag, payload)) = deliveries.recv() {
                let len = u64::try_from(payload.len()).unwrap_or(u64::MAX);
                caller.deliver(tag, payload);
                let _ = carrier_frame(&input, run_frame::OUTPUT_ACK, &len.to_be_bytes());
            }
        })
    };
    let frames = {
        let (window, input) = (Arc::clone(&window), Arc::clone(&input));
        let mut stdout = stdout;
        thread::spawn(move || {
            let mut complete = false;
            let mut inboxes =
                std::collections::HashMap::<u32, mpsc::Sender<Option<Vec<u8>>>>::new();
            // Each call's replies have their own window, so a caller that
            // stops reading stalls only its own call.
            let mut windows = std::collections::HashMap::<u32, Arc<Window>>::new();
            while let Ok(Some((tag, payload))) = run_frame::read(&mut stdout) {
                match tag {
                    run_frame::STDOUT | run_frame::STDERR => {
                        let _ = deliver.send((tag, payload));
                    }
                    run_frame::ACK => {
                        window.acknowledge(payload.try_into().map_or(u64::MAX, u64::from_be_bytes));
                    }
                    run_frame::END => complete = true,
                    run_frame::KIT_ACK if payload.len() == 12 => {
                        let id =
                            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                        if let Some(window) = windows.get(&id) {
                            window.acknowledge(
                                payload[4..].try_into().map_or(u64::MAX, u64::from_be_bytes),
                            );
                        }
                    }
                    run_frame::KIT_OPEN | run_frame::KIT_IN | run_frame::KIT_IN_EOF
                        if payload.len() >= 4 =>
                    {
                        let id =
                            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                        let body = payload[4..].to_vec();
                        let call_window = Arc::clone(windows.entry(id).or_insert_with(|| {
                            Arc::new(Window::with_limit(run_frame::CALL_WINDOW))
                        }));
                        let reply = CallReply {
                            id,
                            input: Arc::clone(&input),
                            window: call_window,
                        };
                        match tag {
                            run_frame::KIT_OPEN => {
                                reply.taken(u64::try_from(body.len()).unwrap_or(u64::MAX));
                                let (sender, inbox) = mpsc::channel();
                                inboxes.insert(id, sender);
                                if let (Ok(request), Some(calls)) =
                                    (serde_json::from_slice::<LinkRequest>(&body), &calls)
                                    && calls.begin()
                                {
                                    let calls = Arc::clone(calls);
                                    thread::spawn(move || calls.call(request, inbox, reply));
                                } else {
                                    reply.stderr("marsh: agent call refused: not available here, or the cloud block is ending\n");
                                    reply.exit(127);
                                }
                            }
                            run_frame::KIT_IN => {
                                let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
                                let delivered = inboxes
                                    .get(&id)
                                    .is_some_and(|inbox| inbox.send(Some(body)).is_ok());
                                if !delivered {
                                    reply.taken(len);
                                }
                            }
                            _ => {
                                if let Some(inbox) = inboxes.remove(&id) {
                                    let _ = inbox.send(None);
                                }
                            }
                        }
                    }
                    _ => window.close(),
                }
            }
            window.close();
            for call in windows.values() {
                call.close();
            }
            // Drain anything after a malformed frame so the carrier can end.
            let _ = io::copy(&mut stdout, &mut io::sink());
            complete
        })
    };
    // Helper diagnostics (not workload output) arrive unframed on stderr.
    let diagnostics = {
        let caller = Arc::clone(&caller);
        let mut stderr = stderr;
        thread::spawn(move || {
            let mut buffer = vec![0_u8; 64 * 1024];
            while let Ok(count) = stderr.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                caller.diagnostic(buffer[..count].to_vec());
            }
        })
    };
    let code = process.wait().ok();
    let output_complete = frames.join().unwrap_or(false);
    let _ = delivery.join();
    let _ = diagnostics.join();
    window.close();
    // Release the carrier's stdin so nothing keeps the exec session open.
    if let Ok(mut input) = input.lock() {
        *input = Box::new(io::sink());
    }
    Carried {
        code,
        output_complete,
    }
}

fn run_attached(
    carrier: Attachment,
    attachment: &ServerAttachment,
    relay: &Relay,
    calls: Option<Arc<dyn CallHost>>,
) -> Carried {
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

/// The marsh workspace mixin: creates the caller's natural workspace path,
/// owned by the Kit's user, at sandbox startup (a Kit sandbox has no root).
const WORKSPACE_MIXIN: &str = r#"# syntax=docker/sandbox-kit:3@sha256:11bb68806aef6a68d45c10c0c3b3dc86c2f794a296b5a933661d9dadf760b753
schemaVersion: "3"
displayName: marsh cloud workspace
description: Creates the caller's natural workspace path for a marsh cloud placement
kind: mixin
capabilities:
  - type: com.docker.sandbox/lifecycle@1
    config:
      startup:
        - command: [sh, -c, 'case "$MARSH_CLOUD_ROOT" in /?*) [ ! -e "$MARSH_CLOUD_ROOT" ] && mkdir -p "$MARSH_CLOUD_ROOT" && chown agent:agent "$MARSH_CLOUD_ROOT";; esac']
          user: "0"
          env: [MARSH_CLOUD_ROOT]
          description: Create the natural workspace path
"#;

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

    #[allow(clippy::too_many_lines)] // One ordered crossing: admit, provision, copy, run, return.
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
        let not_started = |message: String, cleanup: CleanupState| {
            reply.stderr(&format!("marsh: {name}: {message}\n"));
            self.finish(
                &job,
                ExecutionOutcome::NotStarted,
                125,
                format!("cloud_setup_failed: {message}"),
                cleanup,
                reply,
            );
        };
        if self.store.cancel_requested(&job) {
            return not_started("cancelled".into(), CleanupState::NotRequired);
        }
        let manifest = match self.adapter.kit_manifest(&binding.kit) {
            Ok(manifest) => manifest,
            Err(error) => return not_started(actionable(&error), CleanupState::NotRequired),
        };
        let entry = manifest.entrypoint;
        let spec = CloudImageSpec {
            image: binding.kit.clone(),
            kit: Some(marsh_sbx::cloud::CloudKit {
                name: manifest.name,
                mixins: vec![self.mixin.clone()],
                env: vec![("MARSH_CLOUD_ROOT".into(), self.root.clone())],
            }),
            cpus: self.shape.0,
            memory_mib: self.shape.0 * 2048,
            ttl: self.shape.1,
        };
        reply.stderr(&format!("[starting a Docker Cloud sandbox: Kit {name}…]\n"));
        let record = match self.adapter.create(&spec) {
            Ok(record) => record,
            Err(CloudError::CreateUncertain {
                name: created,
                diagnostic,
            }) => {
                // The sandbox may exist; ownership keeps it for cleanup.
                let cleanup = match self
                    .adapter
                    .reconcile()
                    .ok()
                    .and_then(|records| records.into_iter().find(|r| r.name == created))
                    .and_then(|record| record.id)
                    .map(|id| self.adapter.retire(&id))
                {
                    Some(Ok(())) => CleanupState::Verified,
                    _ => CleanupState::Uncertain,
                };
                let error = CloudError::CreateUncertain {
                    name: created,
                    diagnostic,
                };
                return not_started(actionable(&error), cleanup);
            }
            Err(error) => return not_started(actionable(&error), CleanupState::NotRequired),
        };
        let Some(id) = record.id.clone() else {
            return not_started("created sandbox has no ID".into(), CleanupState::Uncertain);
        };
        let _ = self.store.record_cloud_sandbox(&job, id.as_str());
        let retire = || match self.adapter.retire(&id) {
            Ok(()) => CleanupState::Verified,
            Err(_) => CleanupState::Uncertain,
        };
        let setup_failed = |message: String| {
            let _ = self.adapter.quarantine(&id);
            not_started(message, retire());
        };
        let child = Ctl { kit: true };
        if let Err(message) = upload_helper(&self.adapter, &id, &self.helper) {
            return setup_failed(message);
        }
        let (program, words) = child.prepare(&self.root);
        match retried_step(&self.adapter, &id, &program, &words, None, CONTROL_TIMEOUT) {
            Ok(step) if step.ok() => {}
            Ok(step) => return setup_failed(format!("workspace path: {}", step.diagnostic())),
            Err(message) => return setup_failed(format!("workspace path: {message}")),
        }
        let requested = Path::new(std::ffi::OsStr::from_bytes(&request.cwd));
        let cwd = if requested.starts_with(&self.root)
            && requested
                .components()
                .all(|component| !matches!(component, std::path::Component::ParentDir))
        {
            request.cwd.clone()
        } else {
            self.root.as_bytes().to_vec()
        };
        let mut argv = entry
            .iter()
            .map(|word| word.as_bytes().to_vec())
            .collect::<Vec<_>>();
        argv.extend(request.argv.iter().cloned());
        let child_request = CloudRequest {
            cwd,
            root: self.root.as_bytes().to_vec(),
            mode: CloudMode::Argv,
            argv,
            env: request
                .env
                .into_iter()
                .filter(|(name, _)| {
                    marsh_daemon::split::forwardable(name)
                        && !marsh_contracts::reserved_exported_environment_name(name)
                        && !marsh_contracts::placement_bound_environment_name(name)
                        && !LOCAL_ONLY.contains(&name.as_str())
                })
                .collect(),
            terminal: None,
            kits: Vec::new(),
            stdin_terminal: false,
        };
        if let Err(message) = self.copy_view(&id, &child_request) {
            return setup_failed(format!("copying the block's files: {message}"));
        }
        if self.store.cancel_requested(&job) {
            return setup_failed("cancelled".into());
        }
        if let Err(error) = self.store.mark_cloud_running(&job, id.as_str()) {
            return setup_failed(error.to_string());
        }
        self.store
            .register_job_control(&job, signal_control(&self.adapter, &id, child));
        let carrier = match spawn_run(&self.adapter, &id, child) {
            Ok(carrier) => carrier,
            Err(message) => return setup_failed(message),
        };
        let delivered = run_carrier(carrier, Arc::new(reply.clone()), Some(input), None, None);
        let Some(exit_code) = confirm_status(&self.adapter, &id, child) else {
            reply.stderr(&format!(
                "marsh: {name}: lost the call before it was confirmed ended; nothing was written back\n"
            ));
            let _ = self.adapter.quarantine(&id);
            let cleanup = retire();
            return self.finish(
                &job,
                ExecutionOutcome::Unknown,
                125,
                format!("transport_lost: carrier exit {:?}", delivered.code),
                cleanup,
                reply,
            );
        };
        // Capture, retire, then return the changes to the block's live view.
        let payload = self.results.join(format!("{job}.delta"));
        if let Err(message) = capture_delta(
            &self.adapter,
            &id,
            child,
            &self.root,
            &self.results,
            &payload,
        ) {
            let _ = fs::remove_file(&payload);
            let _ = self.adapter.quarantine(&id);
            reply.stderr(&format!(
                "marsh: {name}: could not download its file changes: {message}; the sandbox {} is kept\n",
                id.as_str()
            ));
            return self.finish(
                &job,
                ExecutionOutcome::Exited { code: exit_code },
                125,
                format!("capture_failed: {message}; sandbox retained"),
                CleanupState::Uncertain,
                reply,
            );
        }
        let cleanup = retire();
        let sealed = self
            .sealed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let applied = if *sealed {
            Err("the cloud block ended before the call returned".to_owned())
        } else {
            self.apply(&payload)
        };
        drop(sealed);
        let (code, cause) = match applied {
            Ok(summary) => {
                let _ = fs::remove_file(&payload);
                if !summary.is_empty() {
                    reply.stderr(&format!("marsh: {name}: {summary}\n"));
                }
                if delivered.output_complete {
                    (exit_code, "exited".to_owned())
                } else {
                    reply.stderr(&format!("marsh: {name}: its output did not all arrive\n"));
                    (125, "output_incomplete".to_owned())
                }
            }
            Err(message) => {
                reply.stderr(&format!(
                    "marsh: {name}: write-back stopped: {message}; the full delta is kept at {}\n",
                    payload.display()
                ));
                (125, format!("write_back_failed: {message}"))
            }
        };
        self.finish(
            &job,
            ExecutionOutcome::Exited { code: exit_code },
            code,
            cause,
            cleanup,
            reply,
        );
    }

    /// Stream the parent's current files (`archive`) into the agent sandbox
    /// (`unpack`), never through host storage.
    fn copy_view(&self, child: &CloudSandboxId, request: &CloudRequest) -> Result<(), String> {
        let request = serde_json::to_vec(request).map_err(|e| e.to_string())?;
        let (program, args) =
            self.parent_ctl
                .as_workload(&["--internal-cloud", "archive", &self.root]);
        let parent = self
            .adapter
            .spawn(&self.parent, &program, &os_args(&args))
            .map_err(|e| actionable(&e))?;
        let Attachment {
            mut stdin,
            stdout,
            stderr,
            mut process,
            ..
        } = parent;
        stdin.write_all(&request).map_err(|e| e.to_string())?;
        drop(stdin);
        let errors = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = read_bounded(stderr, &mut bytes, 64 * 1024);
            bytes
        });
        let script = Ctl { kit: true }.unpack_script();
        let unpack = self
            .adapter
            .spawn(
                child,
                "/bin/sh",
                &[
                    "-c".into(),
                    script.into(),
                    "sh".into(),
                    self.root.clone().into(),
                ],
            )
            .map_err(|e| actionable(&e))?;
        let unpacked = step(unpack, Some(stdout), Box::new(io::sink()), 0, STEP_TIMEOUT);
        let archived = process.wait().map_err(|e| e.to_string())?;
        let diagnostic = String::from_utf8_lossy(&errors.join().unwrap_or_default()).into_owned();
        match unpacked {
            Ok(step) if step.ok() && archived == 0 => Ok(()),
            Ok(step) => Err(format!("{} {}", diagnostic.trim(), step.diagnostic())
                .trim()
                .to_owned()),
            Err(message) => Err(message),
        }
    }

    /// Apply a returned delta in the parent as its workload user; the summary.
    fn apply(&self, payload: &Path) -> Result<String, String> {
        let file = fs::File::open(payload).map_err(|e| e.to_string())?;
        let (program, args) =
            self.parent_ctl
                .as_workload(&["--internal-cloud", "apply", &self.root]);
        let step = small_step(
            &self.adapter,
            &self.parent,
            &program,
            &args,
            Some(Box::new(file)),
            STEP_TIMEOUT,
        )?;
        let report: serde_json::Value =
            serde_json::from_slice(&step.stdout).unwrap_or(serde_json::Value::Null);
        let count = |key: &str| report[key].as_u64().unwrap_or(0);
        let applied = ApplyReport {
            created: count("created"),
            modified: count("modified"),
            deleted: count("deleted"),
            kept: report["kept"]
                .as_array()
                .map(|kept| {
                    kept.iter()
                        .filter_map(|k| k.as_str().map(ToOwned::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            listings: std::collections::BTreeSet::new(),
        };
        match report["error"].as_str() {
            Some(error) => Err(format!("{error}; {} already written", summary(&applied))),
            None if !step.ok() => Err(step.diagnostic()),
            None if applied.total() == 0 && applied.kept.is_empty() => Ok(String::new()),
            None => Ok(summary(&applied)),
        }
    }
}

/// Upload the gzip helper to a fresh sandbox and check its digest.
fn upload_helper(
    adapter: &CloudSbx,
    id: &CloudSandboxId,
    helper: &(Vec<u8>, String),
) -> Result<(), String> {
    let (bytes, digest) = helper;
    match retried_step(
        adapter,
        id,
        "/bin/sh",
        &[
            "-c",
            "PATH=/usr/sbin:/usr/bin:/sbin:/bin; export PATH; umask 077 && mkdir -p /tmp/marsh-cloud && gzip -dc > /tmp/marsh-cloud/marsh-local && chmod 700 /tmp/marsh-cloud/marsh-local && sha256sum /tmp/marsh-cloud/marsh-local",
        ],
        Some(bytes),
        STEP_TIMEOUT,
    ) {
        Ok(step)
            if step.ok() && String::from_utf8_lossy(&step.stdout).starts_with(digest.as_str()) =>
        {
            Ok(())
        }
        Ok(step) => Err(format!("helper upload failed: {}", step.diagnostic())),
        Err(message) => Err(format!("helper upload failed: {message}")),
    }
}

/// The job control that signals a node's workload (INT, TERM, KILL).
fn signal_control(adapter: &Arc<CloudSbx>, id: &CloudSandboxId, ctl: Ctl) -> JobControl {
    let (adapter, id) = (Arc::clone(adapter), id.clone());
    JobControl(Arc::new(move |signal: &str| {
        let signal = match signal {
            "KILL" => "KILL",
            "TERM" | "HUP" => "TERM",
            _ => "INT",
        };
        let (adapter, id) = (Arc::clone(&adapter), id.clone());
        thread::spawn(move || {
            let (program, args) = ctl.step(&["--internal-cloud", "signal", signal]);
            let _ = retried_step(&adapter, &id, &program, &args, None, CONTROL_TIMEOUT);
        });
    }))
}

/// The recorded status: present only once every workload writer is gone.
fn confirm_status(adapter: &CloudSbx, id: &CloudSandboxId, ctl: Ctl) -> Option<i32> {
    let (program, args) = ctl.step(&["--internal-cloud", "status"]);
    retried_step(adapter, id, &program, &args, None, CONTROL_TIMEOUT)
        .ok()
        .filter(Step::ok)
        .and_then(|step| {
            String::from_utf8_lossy(&step.stdout)
                .trim()
                .parse::<i32>()
                .ok()
        })
}

/// Download the node's delta into owner-only retained storage.
fn capture_delta(
    adapter: &CloudSbx,
    id: &CloudSandboxId,
    ctl: Ctl,
    root: &str,
    results: &Path,
    payload: &Path,
) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(results)
        .map_err(|e| e.to_string())?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(payload)
        .map_err(|e| e.to_string())?;
    let (program, args) = ctl.step(&["--internal-cloud", "delta", root, "-"]);
    let attachment = adapter
        .spawn(id, &program, &os_args(&args))
        .map_err(|e| actionable(&e))?;
    let step = step(
        attachment,
        None,
        Box::new(io::BufWriter::new(file)),
        MAX_ARCHIVE,
        STEP_TIMEOUT,
    )?;
    if step.ok() {
        Ok(())
    } else {
        Err(step.diagnostic())
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
