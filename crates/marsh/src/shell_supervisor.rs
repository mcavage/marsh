//! Guest half of the per-VM shell supervisor (`marsh --internal-supervisor`).
//!
//! Runs as root inside the shell VM over one retained stock `sbx exec -i`.
//! Starts contained shells (through `--internal-record-session`) and plain
//! relay children, relays their bytes as attempt-keyed frames, delivers
//! signals/resizes, reports exit status, and kills and verifies each shell's
//! cgroup in-band. Protocol: `marsh_sbx::shell_supervisor`.

use marsh_sbx::shell_supervisor::{
    CHUNK, ControlOp, Down, STDERR, STDOUT, StartKind, StartSpec, Up, WINDOW, read_frame,
    write_frame,
};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::OwnedFd,
        unix::{
            ffi::OsStrExt,
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

type Writer = Arc<Mutex<io::Stdout>>;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn send(writer: &Writer, frame: &Up, payload: &[u8]) {
    let _ = write_frame(&mut *lock(writer), frame, payload);
}

// openpty descriptors are not close-on-exec until marked. Serialize every
// spawn with that window so no sibling child inherits another session's PTY.
static SPAWN: Mutex<()> = Mutex::new(());

struct Credit {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
}

impl Credit {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((WINDOW, false)),
            changed: Condvar::new(),
        })
    }

    fn acquire(&self) -> bool {
        let mut state = lock(&self.state);
        while state.0 == 0 && !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.1 {
            return false;
        }
        state.0 -= 1;
        true
    }

    fn release(&self) {
        let mut state = lock(&self.state);
        state.0 = (state.0 + 1).min(WINDOW);
        self.changed.notify_all();
    }

    fn close(&self) {
        lock(&self.state).1 = true;
        self.changed.notify_all();
    }
}

enum Kind {
    Shell { record: PathBuf, uid: u32 },
    Child,
}

struct Attempt {
    id: u64,
    pid: i32,
    kind: Kind,
    master: Option<File>,
    stdin: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    credits: [Arc<Credit>; 2],
    exited: Arc<(Mutex<bool>, Condvar)>,
    started: AtomicBool,
    reported: AtomicBool,
    cleaned: Mutex<bool>,
}

impl Attempt {
    fn has_exited(&self) -> bool {
        *lock(&self.exited.0)
    }

    fn wait_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut exited = lock(&self.exited.0);
        while !*exited {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            exited = self
                .exited
                .1
                .wait_timeout(exited, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        true
    }

    fn signal(&self, name: &str) -> io::Result<()> {
        match &self.kind {
            Kind::Shell { record, uid } => {
                crate::session_process::signal_recorded(record, *uid, name)
            }
            Kind::Child => {
                let signal = match name {
                    "INT" => nix::sys::signal::Signal::SIGINT,
                    "TERM" => nix::sys::signal::Signal::SIGTERM,
                    "HUP" => nix::sys::signal::Signal::SIGHUP,
                    "KILL" => nix::sys::signal::Signal::SIGKILL,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "invalid signal",
                        ));
                    }
                };
                if self.has_exited() {
                    return Ok(());
                }
                nix::sys::signal::killpg(nix::unistd::Pid::from_raw(self.pid), signal)
                    .or_else(|error| {
                        if error == nix::errno::Errno::ESRCH {
                            Ok(())
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(io::Error::from)
            }
        }
    }

    /// Kills the whole tree and verifies it is gone. Idempotent.
    fn cleanup(&self) -> io::Result<()> {
        let mut cleaned = lock(&self.cleaned);
        if *cleaned {
            return Ok(());
        }
        match &self.kind {
            Kind::Shell { record, uid } => {
                if self.started.load(Ordering::Acquire) {
                    crate::session_process::signal_recorded(record, *uid, "CLEANUP")?;
                } else {
                    // Never published: the leader may still be pre-enrollment.
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(self.pid),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                    let _ = crate::session_process::signal_recorded(record, *uid, "CLEANUP");
                    if !self.wait_exit(Duration::from_secs(5)) {
                        return Err(io::Error::other("unpublished shell leader did not exit"));
                    }
                }
            }
            Kind::Child => {
                self.signal("KILL")?;
                if !self.wait_exit(Duration::from_secs(5)) {
                    return Err(io::Error::other("relay child did not exit after SIGKILL"));
                }
            }
        }
        *cleaned = true;
        Ok(())
    }

    fn control(&self, op: &ControlOp) -> io::Result<()> {
        match op {
            ControlOp::Signal(name) => self.signal(name),
            ControlOp::Cleanup => self.cleanup(),
            ControlOp::Resize { rows, columns } => {
                let master = self.master.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "pipe attachment has no terminal",
                    )
                })?;
                rustix::termios::tcsetwinsize(
                    master,
                    rustix::termios::Winsize {
                        ws_row: *rows,
                        ws_col: *columns,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    },
                )
                .map_err(io::Error::from)
            }
        }
    }

    fn release(&self) {
        for credit in &self.credits {
            credit.close();
        }
        lock(&self.stdin).take();
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(125)
}

fn spawn_output(writer: &Writer, attempt: u64, stream: u8, mut source: File, credit: Arc<Credit>) {
    let writer = Arc::clone(writer);
    thread::spawn(move || {
        let mut buffer = vec![0_u8; CHUNK];
        loop {
            if !credit.acquire() {
                return;
            }
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => send(&writer, &Up::Output { attempt, stream }, &buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => credit.release(),
                // EIO: every PTY slave descriptor closed.
                Err(_) => break,
            }
        }
        send(&writer, &Up::OutputEof { attempt, stream }, &[]);
    });
}

fn spawn_input(writer: &Writer, attempt: u64, mut sink: File) -> mpsc::Sender<Vec<u8>> {
    let (input, chunks) = mpsc::channel::<Vec<u8>>();
    let writer = Arc::clone(writer);
    thread::spawn(move || {
        let mut open = true;
        while let Ok(chunk) = chunks.recv() {
            if !open {
                continue;
            }
            if sink.write_all(&chunk).is_err() {
                open = false;
                send(&writer, &Up::InputClosed { attempt }, &[]);
            } else {
                send(&writer, &Up::InputCredit { attempt }, &[]);
            }
        }
    });
    input
}

/// Spawns one attempt's process. A shell gets a fresh PTY (or pipes); the
/// child (`--internal-record-session`) makes itself the session leader and
/// takes the PTY as its controlling terminal before containment publication.
/// Child-side stdio pipes owned by `uid`, so the unprivileged process can
/// reopen them (`/dev/stdin`, `/dev/stderr`, `/proc/self/fd/N` on Linux
/// reopen the pipe inode, which a root-owned pipe refuses with EACCES).
/// Returns the supervisor's ends: stdin writer, stdout reader, stderr reader.
fn user_pipes(command: &mut Command, uid: u32) -> io::Result<[File; 3]> {
    let pipe = || -> io::Result<(OwnedFd, OwnedFd)> {
        // Called under `SPAWN`: no concurrent spawn can inherit these
        // before close-on-exec is set.
        let (read, write) = nix::unistd::pipe().map_err(io::Error::from)?;
        for end in [&read, &write] {
            nix::fcntl::fcntl(
                end,
                nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
            )
            .map_err(io::Error::from)?;
        }
        nix::unistd::fchown(&read, Some(nix::unistd::Uid::from_raw(uid)), None)
            .map_err(io::Error::from)?;
        Ok((read, write))
    };
    let (stdin_read, stdin_write) = pipe()?;
    let (stdout_read, stdout_write) = pipe()?;
    let (stderr_read, stderr_write) = pipe()?;
    command
        .stdin(Stdio::from(stdin_read))
        .stdout(Stdio::from(stdout_write))
        .stderr(Stdio::from(stderr_write));
    Ok([
        File::from(stdin_write),
        File::from(stdout_read),
        File::from(stderr_read),
    ])
}

type Spawned = (Kind, Option<File>, Option<[File; 3]>, std::process::Child);

fn spawn_child(spec: StartSpec) -> io::Result<Spawned> {
    let mut command = Command::new(OsStr::from_bytes(&spec.program));
    command
        .args(spec.arguments.iter().map(|word| OsStr::from_bytes(word)))
        .envs(
            spec.environment
                .iter()
                .map(|(name, value)| (OsStr::from_bytes(name), OsStr::from_bytes(value))),
        )
        .current_dir(OsStr::from_bytes(&spec.working_directory));
    let _spawning = lock(&SPAWN);
    match spec.kind {
        StartKind::Shell {
            record,
            uid,
            terminal,
        } => {
            let master = if let Some((rows, columns)) = terminal {
                let size = nix::pty::Winsize {
                    ws_row: rows,
                    ws_col: columns,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                let pty = nix::pty::openpty(Some(&size), None).map_err(io::Error::from)?;
                nix::fcntl::fcntl(
                    &pty.master,
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
                )
                .map_err(io::Error::from)?;
                let slave = pty.slave;
                command
                    .stdin(Stdio::from(slave.try_clone()?))
                    .stdout(Stdio::from(slave.try_clone()?))
                    .stderr(Stdio::from(slave));
                if std::env::var_os("TERM").is_none()
                    && !spec.environment.iter().any(|(name, _)| name == b"TERM")
                {
                    command.env("TERM", "xterm");
                }
                Some(File::from(pty.master))
            } else {
                None
            };
            let pipes = master
                .is_none()
                .then(|| user_pipes(&mut command, uid))
                .transpose()?;
            let record = PathBuf::from(OsStr::from_bytes(&record));
            Ok((Kind::Shell { record, uid }, master, pipes, command.spawn()?))
        }
        StartKind::Child { uid, gid } => {
            let pipes = user_pipes(&mut command, uid)?;
            command.uid(uid).gid(gid).process_group(0);
            Ok((Kind::Child, None, Some(pipes), command.spawn()?))
        }
    }
}

fn start(writer: &Writer, attempt: u64, spec: StartSpec) -> io::Result<Arc<Attempt>> {
    let (kind, master_file, pipes, mut child) = spawn_child(spec)?;
    let pid = i32::try_from(child.id()).map_err(io::Error::other)?;
    let credits = [Credit::new(), Credit::new()];
    let stdin_sender;
    if let Some(master) = &master_file {
        spawn_output(
            writer,
            attempt,
            STDOUT,
            master.try_clone()?,
            Arc::clone(&credits[0]),
        );
        send(
            writer,
            &Up::OutputEof {
                attempt,
                stream: STDERR,
            },
            &[],
        );
        stdin_sender = spawn_input(writer, attempt, master.try_clone()?);
    } else {
        let [stdin, stdout, stderr] = pipes.ok_or_else(missing)?;
        spawn_output(writer, attempt, STDOUT, stdout, Arc::clone(&credits[0]));
        spawn_output(writer, attempt, STDERR, stderr, Arc::clone(&credits[1]));
        stdin_sender = spawn_input(writer, attempt, stdin);
    }
    let exited = Arc::new((Mutex::new(false), Condvar::new()));
    let state = Arc::new(Attempt {
        id: attempt,
        pid,
        kind,
        master: master_file,
        stdin: Mutex::new(Some(stdin_sender)),
        credits,
        exited: Arc::clone(&exited),
        started: AtomicBool::new(false),
        reported: AtomicBool::new(false),
        cleaned: Mutex::new(false),
    });
    let wait_writer = Arc::clone(writer);
    let waited = Arc::clone(&state);
    thread::spawn(move || {
        let code = child.wait().map_or(125, exit_code);
        *lock(&exited.0) = true;
        exited.1.notify_all();
        send(&wait_writer, &Up::Exited { attempt, code }, &[]);
        auto_clean(&wait_writer, &waited);
    });
    match &state.kind {
        Kind::Child => {
            state.started.store(true, Ordering::Release);
            send(writer, &Up::Started { attempt }, &[]);
        }
        Kind::Shell { .. } => {
            let ready = Arc::clone(&state);
            let writer = Arc::clone(writer);
            thread::spawn(move || await_publication(&writer, &ready));
        }
    }
    Ok(state)
}

/// A contained shell ends with its leader: once it is both published and
/// exited, kill and verify the whole cgroup and report it in-band exactly once.
fn auto_clean(writer: &Writer, attempt: &Attempt) {
    if matches!(attempt.kind, Kind::Shell { .. })
        && attempt.started.load(Ordering::Acquire)
        && attempt.has_exited()
        && !attempt.reported.swap(true, Ordering::AcqRel)
    {
        let error = attempt.cleanup().err().map(|error| error.to_string());
        send(
            writer,
            &Up::Cleaned {
                attempt: attempt.id,
                error,
            },
            &[],
        );
    }
}

fn missing() -> io::Error {
    io::Error::other("child stdio unavailable")
}

/// `Started` only after root-owned containment publication, before user code.
fn await_publication(writer: &Writer, attempt: &Arc<Attempt>) {
    let Kind::Shell { record, uid } = &attempt.kind else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let failure = loop {
        let exited = attempt.has_exited();
        match crate::session_process::signal_recorded(record, *uid, "READY") {
            Ok(()) => {
                attempt.started.store(true, Ordering::Release);
                send(
                    writer,
                    &Up::Started {
                        attempt: attempt.id,
                    },
                    &[],
                );
                auto_clean(writer, attempt);
                return;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if exited {
                    break "shell exited before containment publication".to_owned();
                }
                if Instant::now() >= deadline {
                    break "containment publication timed out".to_owned();
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => break error.to_string(),
        }
    };
    let _ = attempt.cleanup();
    send(
        writer,
        &Up::StartFailed {
            attempt: attempt.id,
            message: failure,
        },
        &[],
    );
}

type Attempts = Arc<Mutex<BTreeMap<u64, Arc<Attempt>>>>;

/// Handles one attempt-keyed frame without blocking the transport reader.
fn dispatch(writer: &Writer, attempts: &Attempts, frame: Down, payload: Vec<u8>) -> io::Result<()> {
    match frame {
        Down::Start { attempt, spec } => {
            if lock(attempts).contains_key(&attempt) {
                return Err(io::Error::other("attempt replayed"));
            }
            match start(writer, attempt, spec) {
                Ok(state) => {
                    lock(attempts).insert(attempt, state);
                }
                Err(error) => send(
                    writer,
                    &Up::StartFailed {
                        attempt,
                        message: error.to_string(),
                    },
                    &[],
                ),
            }
        }
        Down::Input { attempt } => {
            if let Some(state) = lock(attempts).get(&attempt)
                && let Some(stdin) = lock(&state.stdin).as_ref()
            {
                let _ = stdin.send(payload);
            }
        }
        Down::InputEof { attempt } => {
            if let Some(state) = lock(attempts).get(&attempt) {
                lock(&state.stdin).take();
            }
        }
        Down::OutputCredit { attempt, stream } => {
            if let Some(state) = lock(attempts).get(&attempt)
                && let Some(credit) = state.credits.get(usize::from(stream).wrapping_sub(1))
            {
                credit.release();
            }
        }
        Down::Control { attempt, seq, op } => {
            let state = lock(attempts).get(&attempt).cloned();
            let writer = Arc::clone(writer);
            thread::spawn(move || {
                let error = match state {
                    Some(state) => state.control(&op).err().map(|error| error.to_string()),
                    None => Some("unknown attempt".into()),
                };
                send(
                    &writer,
                    &Up::Done {
                        attempt,
                        seq,
                        error,
                    },
                    &[],
                );
            });
        }
        Down::Release { attempt } => {
            if let Some(state) = lock(attempts).remove(&attempt) {
                thread::spawn(move || {
                    let _ = state.cleanup();
                    state.release();
                });
            }
        }
        Down::Hello { .. } | Down::Ping { .. } => {}
    }
    Ok(())
}

/// Serves one supervisor transport until host EOF, then cleans every attempt.
///
/// # Errors
/// Returns transport errors other than orderly EOF.
pub fn run() -> io::Result<()> {
    let writer: Writer = Arc::new(Mutex::new(io::stdout()));
    let mut input = io::BufReader::with_capacity(2 * CHUNK, io::stdin().lock());
    let (Down::Hello { generation }, _) = read_frame::<Down>(&mut input)? else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "supervisor expected hello",
        ));
    };
    send(&writer, &Up::Ready { generation }, &[]);
    let attempts: Attempts = Arc::default();
    let outcome = loop {
        let (frame, payload) = match read_frame::<Down>(&mut input) {
            Ok(frame) => frame,
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
            Err(error) => break Err(error),
        };
        match frame {
            Down::Hello { .. } => {
                break Err(io::Error::new(io::ErrorKind::InvalidData, "repeated hello"));
            }
            Down::Ping {
                generation: requested,
                nonce,
            } => {
                if requested != generation {
                    break Err(io::Error::other("supervisor generation mismatch"));
                }
                send(&writer, &Up::Pong { generation, nonce }, &[]);
            }
            frame => {
                if let Err(error) = dispatch(&writer, &attempts, frame, payload) {
                    break Err(error);
                }
            }
        }
    };
    // Transport loss: the host already treats these attempts as uncertain.
    // Still remove every process tree this supervisor started.
    let remaining = std::mem::take(&mut *lock(&attempts));
    let cleaners = remaining
        .into_values()
        .map(|state| {
            thread::spawn(move || {
                let _ = state.cleanup();
                state.release();
            })
        })
        .collect::<Vec<_>>();
    for cleaner in cleaners {
        let _ = cleaner.join();
    }
    outcome
}
