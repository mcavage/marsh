//! The cloud crossing shared by the laptop daemon and the remote
//! coordinator of a detached run (`docs/design/cloud-detach.md` s6): stock
//! `sbx --cloud exec` control steps against the `marsh-local` helper, the
//! framed run carrier, and one registered-Kit call in its own agent sandbox
//! (create, copy the caller's files in, relay, confirm, capture, delete,
//! write back).

use crate::cloud_fs::ApplyReport;
use crate::process::JobControl;
use marsh_contracts::cloud::{LinkRequest, MAX_ARCHIVE, run_frame};
use marsh_runtime::Attachment;
use marsh_sbx::cloud::{CloudError, CloudSandboxId, CloudSbx};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// The uploaded helper (sandbox user's private directory); `prepare` installs
/// the root-owned copy every later step runs through `sudo -n`.
pub const UPLOADED: &str = "/tmp/marsh-cloud/marsh-local";
const HELPER: &str = marsh_contracts::cloud::INSTALLED_HELPER;
pub const STEP_TIMEOUT: Duration = Duration::from_mins(10);
pub const CONTROL_TIMEOUT: Duration = Duration::from_mins(1);
/// Exported names that describe the caller's machine, not the sandbox.
pub const LOCAL_ONLY: [&str; 6] = ["SHLVL", "OLDPWD", "PWD", "TMPDIR", "SHELL", "_"];

/// Make stock CLI failures actionable: sign-in and entitlement come first.
#[must_use]
pub fn actionable(error: &CloudError) -> String {
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
pub struct Ctl {
    pub kit: bool,
}

impl Ctl {
    /// A control step: `(program, words)`.
    #[must_use]
    pub fn step(self, args: &[&str]) -> (String, Vec<String>) {
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
    pub fn as_workload(self, args: &[&str]) -> (String, Vec<String>) {
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
    pub fn prepare(self, root: &str) -> (String, Vec<String>) {
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
    #[must_use]
    pub fn unpack_script(self) -> String {
        if self.kit {
            format!("/usr/bin/gzip -dc | {UPLOADED} --internal-cloud unpack - \"$1\"")
        } else {
            format!(
                "/usr/bin/gzip -dc | /usr/bin/sudo -n {HELPER} --internal-cloud unpack - \"$1\""
            )
        }
    }
}

pub fn os_args(args: &[String]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// One stock exec step's result.
pub struct Step {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Step {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    #[must_use]
    pub fn diagnostic(&self) -> String {
        String::from_utf8_lossy(&self.stderr)
            .chars()
            .take(1024)
            .collect::<String>()
            .trim()
            .to_owned()
    }
}

pub fn read_bounded(mut reader: impl Read, mut sink: impl Write, limit: u64) -> io::Result<u64> {
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
pub fn step(
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
pub fn small_step<S: AsRef<str>>(
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
#[must_use]
pub fn failed_before_start(step: &Step) -> bool {
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
pub fn spawn_run(adapter: &CloudSbx, id: &CloudSandboxId, ctl: Ctl) -> Result<Attachment, String> {
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
pub struct Exited(Option<i32>);

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
pub fn retried_step<S: AsRef<str>>(
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

pub struct SharedBuffer(pub Arc<Mutex<Vec<u8>>>);

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

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

#[must_use]
pub fn summary(report: &ApplyReport) -> String {
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
    if report.ignored > 0 {
        let _ = write!(
            text,
            "; {} ignored file{} left in the cloud",
            report.ignored,
            plural(report.ignored)
        );
    }
    text
}

/// Relay queued stdin to the carrier and its output back to the caller.
/// Returns the carrier's exit code (`None` if it could not be observed).
/// Unacknowledged stdin bytes in flight to the remote workload.
pub struct Window {
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
    #[must_use]
    pub fn with_limit(limit: u64) -> Self {
        Self {
            state: Mutex::new((0, false)),
            changed: std::sync::Condvar::new(),
            limit,
        }
    }

    /// Wait until `len` more bytes fit; false once input is closed.
    pub fn reserve(&self, len: u64) -> bool {
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

    pub fn acknowledge(&self, len: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = state.0.saturating_sub(len);
        self.changed.notify_all();
    }

    pub fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = true;
        self.changed.notify_all();
    }
}

/// The carrier's stdin, shared by the stdin writer and output acknowledgements.
pub type CarrierInput = Arc<Mutex<Box<dyn Write + Send>>>;

pub fn carrier_frame(input: &CarrierInput, tag: u8, payload: &[u8]) -> io::Result<()> {
    let mut input = input
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    run_frame::write(&mut **input, tag, payload)
}

/// What the run carrier delivered.
pub struct Carried {
    /// The carrier's own exit code, if observed.
    pub code: Option<i32>,
    /// The helper's end-of-output marker arrived: no output was lost.
    pub output_complete: bool,
}

/// Who a run carrier serves: the session caller, or an agent call relayed
/// from a parent cloud workload.
pub trait Caller: Send + Sync + 'static {
    /// Workload stdout or stderr (`run_frame::STDOUT`/`STDERR`); may block.
    fn deliver(&self, tag: u8, bytes: Vec<u8>);
    /// One input chunk of `len` bytes was forwarded to the carrier.
    fn forwarded(&self, len: u64);
    /// The workload stopped reading its input.
    fn input_closed(&self);
    /// Helper diagnostics (unframed carrier stderr).
    fn diagnostic(&self, bytes: Vec<u8>);
}

/// Replies to one agent call over its parent's carrier, within the parent's
/// acknowledged window.
#[derive(Clone)]
pub struct CallReply {
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

    pub fn stderr(&self, text: &str) {
        self.send(run_frame::KIT_ERR, text.as_bytes());
    }

    pub fn exit(&self, code: i32) {
        let _ = carrier_frame(
            &self.input,
            run_frame::KIT_EXIT,
            &self.tagged(&code.to_be_bytes()),
        );
    }

    /// The call's stdin bytes were taken: let the parent helper send more.
    pub fn taken(&self, len: u64) {
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
pub trait CallHost: Send + Sync + 'static {
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
pub fn run_carrier(
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

/// The marsh workspace mixin: creates the caller's natural workspace path,
/// owned by the Kit's user, at sandbox startup (a Kit sandbox has no root).
pub const WORKSPACE_MIXIN: &str = r#"# syntax=docker/sandbox-kit:3@sha256:11bb68806aef6a68d45c10c0c3b3dc86c2f794a296b5a933661d9dadf760b753
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

/// Upload the gzip helper to a fresh sandbox and check its digest.
pub fn upload_helper(
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
pub fn signal_control(adapter: &Arc<CloudSbx>, id: &CloudSandboxId, ctl: Ctl) -> JobControl {
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
pub fn confirm_status(adapter: &CloudSbx, id: &CloudSandboxId, ctl: Ctl) -> Option<i32> {
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
pub fn capture_delta(
    adapter: &CloudSbx,
    id: &CloudSandboxId,
    ctl: Ctl,
    root: &str,
    results: &Path,
    payload: &Path,
) -> Result<u64, String> {
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
        Ok(marsh_contracts::cloud::ignored_count(&step.stderr))
    } else {
        Err(step.diagnostic())
    }
}

/// The caller's current files, streamed from the parent's `archive` step.
pub struct Archive {
    pub stdout: Box<dyn Read + Send>,
    /// Waits for the archive step; its diagnostic on failure.
    pub finish: Box<dyn FnOnce() -> Result<(), String> + Send>,
}

/// The parent side of one agent call: where its files come from and go
/// back to, and the call's bookkeeping. The laptop daemon reaches the
/// parent sandbox over stock exec; a detached run's coordinator is in it.
pub trait ChildParent: Sync {
    /// Stream the parent's current files (`--internal-cloud archive ROOT`,
    /// fed `request`, a serialized [`CloudRequest`]).
    ///
    /// # Errors
    /// The archive step could not start.
    fn archive(&self, request: Vec<u8>) -> Result<Archive, String>;
    /// Apply the call's returned delta to the parent's live view: the
    /// summary, empty for no changes.
    ///
    /// # Errors
    /// The write-back did not complete (or the parent no longer accepts it).
    fn apply(&self, payload: &Path) -> Result<String, String>;
    /// The call was cancelled.
    fn cancelled(&self) -> bool;
    /// The agent sandbox exists (before anything runs in it).
    fn created(&self, _id: &CloudSandboxId) {}
    /// The workload is about to start; `control` signals it.
    ///
    /// # Errors
    /// The call may not start now.
    fn running(&self, id: &CloudSandboxId, control: JobControl) -> Result<(), String>;
}

/// One registered-Kit call's agent sandbox.
pub struct ChildCall<'a> {
    pub adapter: &'a Arc<CloudSbx>,
    /// The Kit's digest-pinned reference, its declared name and entrypoint.
    pub kit: &'a marsh_contracts::OciImage,
    pub manifest: marsh_sbx::cloud::KitManifest,
    /// The workspace mixin directory (`WORKSPACE_MIXIN`).
    pub mixin: &'a Path,
    /// The parent's workspace root (the natural path).
    pub root: &'a str,
    pub cpus: u32,
    pub ttl: Duration,
    pub stop_on_timeout: bool,
    /// The gzip helper and its SHA-256.
    pub helper: &'a (Vec<u8>, String),
    /// Where the delta is captured (its directory is made owner-only).
    pub payload: std::path::PathBuf,
}

/// How a call ended.
#[derive(Debug)]
pub struct ChildOutcome {
    pub execution: crate::ExecutionOutcome,
    pub code: i32,
    pub cause: String,
    pub cleanup: crate::CleanupState,
    /// What came back (`wrote back ...`), empty for no changes.
    pub summary: String,
}

fn note(caller: &dyn Caller, name: &str, message: &str) {
    caller.diagnostic(format!("marsh: {name}: {message}\n").into_bytes());
}

/// Run one call in its own agent sandbox: create it from the Kit, copy the
/// parent's current files in, relay the call's streams, confirm its end,
/// capture its delta, delete the sandbox, then write the changes back into
/// the parent before returning. Never replays: an unconfirmed end captures
/// nothing.
#[allow(clippy::too_many_lines)] // One ordered crossing: provision, copy, run, return.
#[allow(clippy::needless_pass_by_value)] // The call's request and streams are consumed.
pub fn run_child(
    call: &ChildCall<'_>,
    parent: &dyn ChildParent,
    name: &str,
    request: LinkRequest,
    input: mpsc::Receiver<Option<Vec<u8>>>,
    caller: Arc<dyn Caller>,
) -> ChildOutcome {
    use crate::{CleanupState, ExecutionOutcome};
    use std::os::unix::ffi::OsStrExt as _;
    let not_started = |message: String, cleanup: CleanupState| {
        note(&*caller, name, &message);
        ChildOutcome {
            execution: ExecutionOutcome::NotStarted,
            code: 125,
            cause: format!("cloud_setup_failed: {message}"),
            cleanup,
            summary: String::new(),
        }
    };
    if parent.cancelled() {
        return not_started("cancelled".into(), CleanupState::NotRequired);
    }
    let adapter = call.adapter;
    let spec = marsh_sbx::cloud::CloudImageSpec {
        image: call.kit.clone(),
        kit: Some(marsh_sbx::cloud::CloudKit {
            name: call.manifest.name.clone(),
            mixins: vec![call.mixin.to_path_buf()],
            env: vec![("MARSH_CLOUD_ROOT".into(), call.root.to_owned())],
        }),
        cpus: call.cpus,
        memory_mib: call.cpus * 2048,
        ttl: call.ttl,
        stop_on_timeout: call.stop_on_timeout,
    };
    caller.diagnostic(format!("[starting a Docker Cloud sandbox: Kit {name}…]\n").into_bytes());
    let record = match adapter.create(&spec) {
        Ok(record) => record,
        Err(CloudError::CreateUncertain {
            name: created,
            diagnostic,
        }) => {
            // The sandbox may exist; ownership keeps it for cleanup.
            let cleanup = match adapter
                .reconcile()
                .ok()
                .and_then(|records| records.into_iter().find(|r| r.name == created))
                .and_then(|record| record.id)
                .map(|id| adapter.retire(&id))
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
    parent.created(&id);
    let retire = || match adapter.retire(&id) {
        Ok(()) => CleanupState::Verified,
        Err(_) => CleanupState::Uncertain,
    };
    let setup_failed = |message: String| {
        let _ = adapter.quarantine(&id);
        not_started(message, retire())
    };
    let child = Ctl { kit: true };
    if let Err(message) = upload_helper(adapter, &id, call.helper) {
        return setup_failed(message);
    }
    let (program, words) = child.prepare(call.root);
    match retried_step(adapter, &id, &program, &words, None, CONTROL_TIMEOUT) {
        Ok(step) if step.ok() => {}
        Ok(step) => return setup_failed(format!("workspace path: {}", step.diagnostic())),
        Err(message) => return setup_failed(format!("workspace path: {message}")),
    }
    let requested = Path::new(std::ffi::OsStr::from_bytes(&request.cwd));
    let cwd = if requested.starts_with(call.root)
        && requested
            .components()
            .all(|component| !matches!(component, std::path::Component::ParentDir))
    {
        request.cwd.clone()
    } else {
        call.root.as_bytes().to_vec()
    };
    let mut argv = call
        .manifest
        .entrypoint
        .iter()
        .map(|word| word.as_bytes().to_vec())
        .collect::<Vec<_>>();
    argv.extend(request.argv.iter().cloned());
    let child_request = marsh_contracts::cloud::CloudRequest {
        cwd,
        root: call.root.as_bytes().to_vec(),
        mode: marsh_contracts::cloud::CloudMode::Argv,
        argv,
        env: request
            .env
            .into_iter()
            .filter(|(name, _)| {
                crate::split::forwardable(name)
                    && !marsh_contracts::reserved_exported_environment_name(name)
                    && !marsh_contracts::placement_bound_environment_name(name)
                    && !LOCAL_ONLY.contains(&name.as_str())
            })
            .collect(),
        terminal: None,
        // An agent call's own sandbox has no command links: no further calls.
        kits: Vec::new(),
        stdin_terminal: false,
        detached: false,
        delegate: None,
        ignore: None,
    };
    if let Err(message) = copy_view(adapter, &id, call.root, parent, &child_request) {
        return setup_failed(format!("copying the block's files: {message}"));
    }
    if parent.cancelled() {
        return setup_failed("cancelled".into());
    }
    if let Err(message) = parent.running(&id, signal_control(adapter, &id, child)) {
        return setup_failed(message);
    }
    let carrier = match spawn_run(adapter, &id, child) {
        Ok(carrier) => carrier,
        Err(message) => return setup_failed(message),
    };
    let delivered = run_carrier(carrier, Arc::clone(&caller), Some(input), None, None);
    let Some(exit_code) = confirm_status(adapter, &id, child) else {
        note(
            &*caller,
            name,
            "lost the call before it was confirmed ended; nothing was written back",
        );
        let _ = adapter.quarantine(&id);
        return ChildOutcome {
            execution: ExecutionOutcome::Unknown,
            code: 125,
            cause: format!("transport_lost: carrier exit {:?}", delivered.code),
            cleanup: retire(),
            summary: String::new(),
        };
    };
    // Capture, retire, then return the changes to the parent's live view.
    let results = call.payload.parent().unwrap_or(Path::new("/"));
    if let Err(message) = capture_delta(adapter, &id, child, call.root, results, &call.payload) {
        let _ = fs::remove_file(&call.payload);
        let _ = adapter.quarantine(&id);
        note(
            &*caller,
            name,
            &format!(
                "could not download its file changes: {message}; the sandbox {} is kept",
                id.as_str()
            ),
        );
        return ChildOutcome {
            execution: ExecutionOutcome::Exited { code: exit_code },
            code: 125,
            cause: format!("capture_failed: {message}; sandbox retained"),
            cleanup: CleanupState::Uncertain,
            summary: String::new(),
        };
    }
    let cleanup = retire();
    let (code, cause, summary) = match parent.apply(&call.payload) {
        Ok(summary) => {
            let _ = fs::remove_file(&call.payload);
            if !summary.is_empty() {
                note(&*caller, name, &summary);
            }
            if delivered.output_complete {
                (exit_code, "exited".to_owned(), summary)
            } else {
                note(&*caller, name, "its output did not all arrive");
                (125, "output_incomplete".to_owned(), summary)
            }
        }
        Err(message) => {
            note(
                &*caller,
                name,
                &format!(
                    "write-back stopped: {message}; the full delta is kept at {}",
                    call.payload.display()
                ),
            );
            (
                125,
                format!("write_back_failed: {message}"),
                format!("write-back stopped: {message}"),
            )
        }
    };
    ChildOutcome {
        execution: ExecutionOutcome::Exited { code: exit_code },
        code,
        cause,
        cleanup,
        summary,
    }
}

/// Stream the parent's current files (`archive`) into the agent sandbox
/// (`unpack`), never through intermediate storage.
fn copy_view(
    adapter: &CloudSbx,
    child: &CloudSandboxId,
    root: &str,
    parent: &dyn ChildParent,
    request: &marsh_contracts::cloud::CloudRequest,
) -> Result<(), String> {
    let request = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    let archive = parent.archive(request)?;
    let script = Ctl { kit: true }.unpack_script();
    let unpack = adapter
        .spawn(
            child,
            "/bin/sh",
            &["-c".into(), script.into(), "sh".into(), root.into()],
        )
        .map_err(|e| actionable(&e))?;
    let unpacked = step(
        unpack,
        Some(archive.stdout),
        Box::new(io::sink()),
        0,
        STEP_TIMEOUT,
    );
    let archived = (archive.finish)();
    match (unpacked, archived) {
        (Ok(step), Ok(())) if step.ok() => Ok(()),
        (Ok(step), archived) => Err(format!(
            "{} {}",
            archived.err().unwrap_or_default(),
            step.diagnostic()
        )
        .trim()
        .to_owned()),
        (Err(message), _) => Err(message),
    }
}

/// The helper's `apply` report (JSON on stdout): the summary, empty for no
/// changes.
///
/// # Errors
/// The apply stopped (its message and what was already written).
pub fn apply_report(stdout: &[u8], ok: bool, diagnostic: &str) -> Result<String, String> {
    let report: serde_json::Value =
        serde_json::from_slice(stdout).unwrap_or(serde_json::Value::Null);
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
        ignored: 0,
        listings: std::collections::BTreeSet::new(),
    };
    match report["error"].as_str() {
        Some(error) => Err(format!("{error}; {} already written", summary(&applied))),
        None if !ok => Err(diagnostic.to_owned()),
        None if applied.total() == 0 && applied.kept.is_empty() => Ok(String::new()),
        None => Ok(summary(&applied)),
    }
}
