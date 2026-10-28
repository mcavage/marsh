//! Detached runs (`docs/design/cloud-detach.md` s3): the workload outlives
//! the `sbx exec` that started it. `run --detached` starts a supervisor in
//! its own session and returns once the workload has started; the
//! supervisor spools the output in the sandbox, stops every workload
//! process when the leader exits, records the status, prepares the delta,
//! and writes `done`. Later steps only read (`state`, `tail`, `fetch`,
//! `baseline`) or signal; none starts the workload again.

use super::coordinator::{self, Coordinator};
use super::{
    CloudRequest, delta, helper_path, kill_workload, link_dir, prepare_links, privileged,
    record_status, run_user, state_dir, state_path, workload_command,
};
use marsh_contracts::cloud::CloudMode;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Each output stream keeps at most this many bytes (its newest part).
pub(super) const SPOOL_CAP: u64 = 256 << 20;
/// The most one `tail` step returns.
pub(super) const TAIL_CHUNK: u64 = 512 << 10;
/// Set for a detached workload without a coordinator (cloud-delegate off):
/// command links refuse instead of relaying.
pub(super) const DETACHED_VARIABLE: &str = "MARSH_CLOUD_DETACHED";

/// `run --detached`: claim the at-most-once start, start the supervisor in
/// its own session, and report `started` once the workload runs.
pub(super) fn run() -> i32 {
    let claimed = state_dir().and_then(|state| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(state.join("started"))
    });
    if let Err(error) = claimed {
        eprintln!("marsh cloud: the command was already started in this sandbox: {error}");
        return 125;
    }
    let mut command = Command::new(helper_path());
    command.args(["--internal-cloud", "supervise"]);
    // Agent sandboxes are named as the laptop's own (`MARSH_VM_PREFIX`).
    if let Some(prefix) = state_dir()
        .ok()
        .and_then(|state| fs::read(state.join("request")).ok())
        .and_then(|bytes| serde_json::from_slice::<CloudRequest>(&bytes).ok())
        .and_then(|request| request.delegate)
        .map(|plan| plan.vm_prefix)
        .filter(|prefix| marsh_sbx::valid_vm_prefix(prefix))
    {
        command.env("MARSH_VM_PREFIX", prefix);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            // Nothing runs: say so the same way a supervisor would.
            eprintln!("marsh cloud: cannot start the supervisor: {error}");
            let _ = finish_without_workload(&format!("cannot start the supervisor: {error}"));
            println!("failed");
            return 1;
        }
    };
    let mut line = String::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = BufReader::new(stdout).read_line(&mut line);
    }
    // The supervisor runs on in its own session; it is not waited for.
    drop(child);
    match line.trim_end() {
        "started" => {
            println!("started");
            0
        }
        other => {
            eprintln!(
                "marsh cloud: {}",
                other.strip_prefix("failed: ").unwrap_or(other)
            );
            println!("failed");
            1
        }
    }
}

/// The workload never started: record that as its end (status 127, an
/// empty delta, `done`), so collection reads one outcome either way.
fn finish_without_workload(message: &str) -> io::Result<()> {
    let state = state_dir()?;
    fs::write(state.join("failed-start"), message)?;
    let root = fs::read(state.join("request"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<CloudRequest>(&bytes).ok())
        .map(|request| request_root(&request));
    if run_user().is_ok_and(|user| kill_workload(user.uid.as_raw())) {
        record_status(127);
    }
    finish(&state, root.as_deref());
    Ok(())
}

fn request_root(request: &CloudRequest) -> PathBuf {
    PathBuf::from(std::ffi::OsStr::from_bytes(&request.root))
}

/// After the status: prepare the delta (`result.delta`), or record why not
/// (`failed`), then `done`.
fn finish(state: &Path, root: Option<&Path>) {
    let prepared = match root {
        Some(root) if state.join("status").exists() => {
            let part = state.join("result.delta.part");
            delta(root, &part).and_then(|()| fs::rename(&part, state.join("result.delta")))
        }
        Some(_) => Err(io::Error::other("no status was recorded")),
        None => Err(io::Error::other("the request is unreadable")),
    };
    if let Err(error) = prepared {
        let _ = fs::write(state.join("failed"), error.to_string());
    }
    let _ = fs::write(state.join("done"), b"");
}

/// Why the workload's program could not start, naming it: the bare OS error
/// ("No such file or directory") does not say what was missing.
fn start_failure(request: &CloudRequest, error: &io::Error) -> io::Error {
    let name = match request.mode {
        CloudMode::Argv => request
            .argv
            .first()
            .map(|word| String::from_utf8_lossy(word).into_owned()),
        CloudMode::Shell => None,
    };
    match name {
        Some(name) if error.kind() == io::ErrorKind::NotFound => io::Error::other(format!(
            "`{name}` was not found in the cloud sandbox ({error}). The sandbox is the plain \
             cloud shell image, which has only the base system; a registered command such as \
             claude runs in its own Kit sandbox, which needs a Kit pinned by digest"
        )),
        Some(name) => io::Error::other(format!("cannot run `{name}`: {error}")),
        None => io::Error::other(format!("cannot start the command: {error}")),
    }
}

/// A started workload: its process, group, user, output pumps, and root.
type Supervised = (
    std::process::Child,
    i32,
    u32,
    Vec<std::thread::JoinHandle<()>>,
    PathBuf,
    Option<std::sync::Arc<Coordinator>>,
);

/// The supervisor: in its own session, owned by the control user, outside
/// the exec session that started it. Its stdout is the start report.
#[allow(clippy::too_many_lines)] // One ordered start, wait, stop, status, delta sequence.
pub(super) fn supervise() -> i32 {
    // Not a process group leader (spawned plainly), so this cannot fail;
    // a failure would only leave it in the exec session's group.
    let _ = nix::unistd::setsid();
    let report = |line: &str| {
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{line}").and_then(|()| out.flush());
        // No more reports: the reader in the exec session may be gone.
        if let Ok(null) = File::options().write(true).open("/dev/null") {
            let _ = nix::unistd::dup2_stdout(&null);
        }
    };
    let started = (|| -> io::Result<Supervised> {
        let state = state_dir()?;
        let user = run_user()?;
        fs::write(state.join("supervisor"), std::process::id().to_string())?;
        if !privileged() {
            // As `run`: marks the workload apart from the stock exec
            // sessions of the same user (the supervisor is spared).
            fs::write(state.join("runner"), std::process::id().to_string())?;
            #[cfg(target_os = "linux")]
            nix::sys::prctl::set_no_new_privs().map_err(io::Error::from)?;
        }
        let request: CloudRequest =
            serde_json::from_slice(&fs::read(state.join("request"))?).map_err(io::Error::other)?;
        let root = request_root(&request);
        let stubs = !request.kits.is_empty();
        // With cloud-delegate on, this supervisor serves the command links
        // (before the workload starts: a refused token refuses the run).
        let coordinator = match request.delegate.clone() {
            Some(plan) if stubs => {
                let coordinator = Coordinator::start(&request, plan, user.clone())?;
                coordinator.serve(prepare_links(&request.kits, &user)?);
                Some(coordinator)
            }
            _ => {
                if stubs {
                    prepare_stubs(&request.kits)?;
                }
                None
            }
        };
        let spools = vec![
            Spool::create(&state, 1, SPOOL_CAP)?,
            Spool::create(&state, 2, SPOOL_CAP)?,
        ];
        let mut command = workload_command(&request, &user, false, stubs)?;
        if coordinator.is_none() {
            command.env(DETACHED_VARIABLE, "1");
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| start_failure(&request, &error))?;
        let group = i32::try_from(child.id()).unwrap_or(i32::MAX);
        if let Err(error) = fs::write(state.join("pgid"), group.to_string()) {
            eprintln!("marsh cloud: {error}");
        }
        let mut pumps = Vec::new();
        let outputs: Vec<Option<Box<dyn io::Read + Send>>> = vec![
            child
                .stdout
                .take()
                .map(|out| Box::new(out) as Box<dyn io::Read + Send>),
            child
                .stderr
                .take()
                .map(|err| Box::new(err) as Box<dyn io::Read + Send>),
        ];
        for (output, mut spool) in outputs.into_iter().zip(spools) {
            let Some(mut output) = output else {
                continue;
            };
            pumps.push(std::thread::spawn(move || {
                let mut buffer = vec![0_u8; 64 * 1024];
                loop {
                    match output.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        // A full disk loses output, never the run.
                        Ok(count) => {
                            let _ = spool.write(&buffer[..count]);
                        }
                    }
                }
            }));
        }
        Ok((child, group, user.uid.as_raw(), pumps, root, coordinator))
    })();
    let (mut child, group, uid, pumps, root, coordinator) = match started {
        Ok(started) => started,
        Err(error) => {
            report(&format!("failed: {error}"));
            let _ = finish_without_workload(&error.to_string());
            return 1;
        }
    };
    report("started");
    let status = child.wait();
    if let Some(coordinator) = &coordinator {
        coordinator.seal();
    }
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(group),
        nix::sys::signal::Signal::SIGKILL,
    );
    let code = match status {
        Ok(status) => status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(125),
        Err(_) => 125,
    };
    let state = state_path();
    let stopped = kill_workload(uid);
    // Agent calls end with the run: every agent sandbox is deleted before
    // `done` (one that cannot be is listed by `state` for the laptop).
    if let Some(coordinator) = &coordinator
        && !coordinator.settle()
    {
        let _ = fs::write(
            state.join("children-left"),
            "some agent sandboxes could not be deleted",
        );
    }
    if !stopped {
        let _ = fs::write(state.join("failed"), "workload processes did not stop");
        let _ = fs::write(state.join("done"), b"");
        return 125;
    }
    // Every holder of the output pipes is gone: drain them to the end.
    for pump in pumps {
        let _ = pump.join();
    }
    record_status(code);
    finish(&state, Some(&root));
    0
}

/// Command links in a detached run without a coordinator: each refuses
/// (there is no laptop relay to serve an agent call).
fn prepare_stubs(kits: &[String]) -> io::Result<()> {
    let dir = &link_dir();
    let bin = dir.join("bin");
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(&bin)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    for name in kits {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            continue;
        }
        let stub = bin.join(name);
        fs::write(
            &stub,
            format!("#!{} --cloud-link={name}\n", helper_path().display()),
        )?;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// One spooled output stream: the newest `cap` bytes at most, in two
/// segments (`out.N.prev`, `out.N`) of up to half the cap each. Offsets
/// are absolute stream positions. `out.N.meta` holds `GEN PREV CUR`: an odd
/// generation while a rotation is under way, then the absolute start of
/// the previous and the current segment.
pub(super) struct Spool {
    current: PathBuf,
    previous: PathBuf,
    meta: PathBuf,
    cap: u64,
    generation: u64,
    previous_start: u64,
    current_start: u64,
    current_len: u64,
    file: File,
}

fn spool_paths(state: &Path, stream: u8) -> (PathBuf, PathBuf, PathBuf) {
    (
        state.join(format!("out.{stream}")),
        state.join(format!("out.{stream}.prev")),
        state.join(format!("out.{stream}.meta")),
    )
}

fn create_private(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)
}

impl Spool {
    pub(super) fn create(state: &Path, stream: u8, cap: u64) -> io::Result<Self> {
        let (current, previous, meta) = spool_paths(state, stream);
        let _ = fs::remove_file(&previous);
        let file = create_private(&current)?;
        let spool = Self {
            current,
            previous,
            meta,
            cap: cap.max(2),
            generation: 0,
            previous_start: 0,
            current_start: 0,
            current_len: 0,
            file,
        };
        spool.write_meta()?;
        Ok(spool)
    }

    fn write_meta(&self) -> io::Result<()> {
        let temporary = self.meta.with_extension("meta.new");
        fs::write(
            &temporary,
            format!(
                "{} {} {}\n",
                self.generation, self.previous_start, self.current_start
            ),
        )?;
        fs::rename(&temporary, &self.meta)
    }

    pub(super) fn write(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let segment = self.cap / 2;
        while !bytes.is_empty() {
            if self.current_len >= segment {
                self.rotate()?;
            }
            let room = usize::try_from(segment - self.current_len).unwrap_or(usize::MAX);
            let take = room.min(bytes.len());
            self.file.write_all(&bytes[..take])?;
            self.current_len += take as u64;
            bytes = &bytes[take..];
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.generation += 1;
        self.write_meta()?;
        fs::rename(&self.current, &self.previous)?;
        self.previous_start = self.current_start;
        self.current_start += self.current_len;
        self.current_len = 0;
        self.file = create_private(&self.current)?;
        self.generation += 1;
        self.write_meta()
    }
}

/// A consistent view of one spool: the open segments and their starts.
struct SpoolView {
    previous: Option<(u64, File)>,
    current: (u64, File),
}

impl SpoolView {
    fn open(state: &Path, stream: u8) -> io::Result<Self> {
        let (current, previous, meta) = spool_paths(state, stream);
        let read_meta = || -> io::Result<(u64, u64, u64)> {
            let text = fs::read_to_string(&meta)?;
            let mut words = text.split_whitespace().map(str::parse::<u64>);
            match (words.next(), words.next(), words.next()) {
                (Some(Ok(generation)), Some(Ok(previous)), Some(Ok(current))) => {
                    Ok((generation, previous, current))
                }
                _ => Err(io::Error::other("invalid output spool")),
            }
        };
        for _ in 0..50 {
            let before = read_meta()?;
            if before.0 % 2 == 1 {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            let current_file = File::open(&current)?;
            let previous_file = (before.1 < before.2)
                .then(|| File::open(&previous))
                .transpose()?;
            if read_meta()? == before {
                return Ok(Self {
                    previous: previous_file.map(|file| (before.1, file)),
                    current: (before.2, current_file),
                });
            }
        }
        Err(io::Error::other("output spool kept rotating"))
    }

    /// The first and one-past-last retained offsets.
    fn range(&self) -> io::Result<(u64, u64)> {
        let start = self
            .previous
            .as_ref()
            .map_or(self.current.0, |(start, _)| *start);
        Ok((start, self.current.0 + self.current.1.metadata()?.len()))
    }

    /// Up to `max` bytes from `offset` (clamped to the retained start):
    /// the actual start and the bytes.
    fn read(&mut self, offset: u64, max: u64) -> io::Result<(u64, Vec<u8>)> {
        let (start, _) = self.range()?;
        let mut at = offset.max(start);
        let mut out = Vec::new();
        let first = at;
        if let Some((previous_start, file)) = &mut self.previous
            && at < self.current.0
        {
            file.seek(SeekFrom::Start(at - *previous_start))?;
            file.take(max).read_to_end(&mut out)?;
            at += out.len() as u64;
        }
        let left = max.saturating_sub(out.len() as u64);
        if left > 0 && at >= self.current.0 {
            self.current.1.seek(SeekFrom::Start(at - self.current.0))?;
            (&mut self.current.1).take(left).read_to_end(&mut out)?;
        }
        Ok((first, out))
    }
}

fn process_alive(pid_file: &Path) -> bool {
    fs::read_to_string(pid_file)
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok())
        .and_then(|pid| fs::read(format!("/proc/{pid}/cmdline")).ok())
        .is_some_and(|cmdline| {
            cmdline
                .split(|byte| *byte == 0)
                .any(|word| word == b"supervise")
        })
}

/// The run's state, as one JSON line (read-only).
pub(super) fn state() {
    let state = state_path();
    let exists = |name: &str| state.join(name).exists();
    let streams = [1_u8, 2]
        .into_iter()
        .map(|stream| {
            SpoolView::open(&state, stream)
                .and_then(|view| view.range())
                .map_or(
                    serde_json::Value::Null,
                    |(start, end)| serde_json::json!({ "start": start, "end": end }),
                )
        })
        .collect::<Vec<_>>();
    let text = |name: &str| fs::read_to_string(state.join(name)).ok();
    let document = serde_json::json!({
        "started": exists("started"),
        "supervisor_alive": process_alive(&state.join("supervisor")),
        "status": text("status").and_then(|code| code.trim().parse::<i32>().ok()),
        "done": exists("done"),
        "failed": text("failed"),
        "failed_start": text("failed-start"),
        "delta": fs::metadata(state.join("result.delta")).ok().map(|metadata| metadata.len()),
        "stdout": streams[0],
        "stderr": streams[1],
        "delegate": exists(coordinator::DELEGATE_FILE),
        "children": coordinator::call_records(&state),
        "sandboxes": coordinator::owned_sandboxes(&state),
    });
    println!("{document}");
}

/// `tail STREAM OFFSET`: a line with the start of the returned bytes (the
/// offset, or the oldest retained byte after it), then at most
/// [`TAIL_CHUNK`] bytes.
pub(super) fn tail(stream: &std::ffi::OsStr, offset: &std::ffi::OsStr) -> io::Result<()> {
    let stream = match stream.to_str() {
        Some("1") => 1,
        Some("2") => 2,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown stream",
            ));
        }
    };
    let offset = offset
        .to_str()
        .and_then(|offset| offset.parse::<u64>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid offset"))?;
    let mut view = SpoolView::open(&state_path(), stream)?;
    let (start, bytes) = view.read(offset, TAIL_CHUNK)?;
    let mut out = io::stdout().lock();
    writeln!(out, "{start}")?;
    out.write_all(&bytes)?;
    out.flush()
}

/// `fetch`: the prepared delta on stdout, once the run is done.
pub(super) fn fetch() -> io::Result<()> {
    let state = state_path();
    if !state.join("done").exists() {
        return Err(io::Error::other("the command has not finished"));
    }
    let mut file = File::open(state.join("result.delta"))?;
    let mut out = io::stdout().lock();
    io::copy(&mut file, &mut out)?;
    out.flush()
}

/// `baseline`: the hashed record of every uploaded entry, on stdout.
pub(super) fn baseline() -> io::Result<()> {
    let mut file = File::open(state_path().join("baseline"))?;
    let mut out = io::stdout().lock();
    io::copy(&mut file, &mut out)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(state: &Path, stream: u8, offset: u64) -> (u64, Vec<u8>) {
        let mut view = SpoolView::open(state, stream).unwrap();
        view.read(offset, u64::MAX).unwrap()
    }

    #[test]
    fn spool_keeps_the_newest_bytes_with_absolute_offsets() {
        let state = tempfile::tempdir().unwrap();
        let mut spool = Spool::create(state.path(), 1, 8).unwrap();
        spool.write(b"0123").unwrap();
        assert_eq!(read_all(state.path(), 1, 0), (0, b"0123".to_vec()));
        spool.write(b"4567").unwrap();
        assert_eq!(read_all(state.path(), 1, 2), (2, b"234567".to_vec()));
        spool.write(b"89abcd").unwrap();
        // 14 bytes written, cap 8: the oldest whole segment is gone.
        let view = SpoolView::open(state.path(), 1).unwrap();
        assert_eq!(view.range().unwrap(), (8, 14));
        assert_eq!(read_all(state.path(), 1, 0), (8, b"89abcd".to_vec()));
        assert_eq!(read_all(state.path(), 1, 11), (11, b"bcd".to_vec()));
        assert_eq!(read_all(state.path(), 1, 14), (14, Vec::new()));
        // Bounded reads cross the segment boundary in order.
        spool.write(b"ef").unwrap();
        let mut view = SpoolView::open(state.path(), 1).unwrap();
        assert_eq!(view.read(10, 4).unwrap(), (10, b"abcd".to_vec()));
        assert_eq!(view.read(11, 4).unwrap(), (11, b"bcde".to_vec()));
    }

    #[test]
    fn spool_files_are_private() {
        let state = tempfile::tempdir().unwrap();
        let mut spool = Spool::create(state.path(), 2, 4).unwrap();
        spool.write(b"abcdef").unwrap();
        for name in ["out.2", "out.2.prev"] {
            let mode = fs::metadata(state.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "{name}");
        }
    }
}
