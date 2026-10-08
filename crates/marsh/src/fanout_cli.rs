//! `marsh fanout ... | marsh collect` (`docs/design/processes.md` s1): the CLI form
//! of Brush's `fanout { } | collect`. Branches run concurrently, here, on the
//! same files (no fork), each with the same spooled stdin; `collect` renders
//! them in declaration order with Brush's own renderer. Limits match the
//! sugar: 64 MiB of input, 16 MiB of combined branch output.
//!
//! - In a job, branches are argv only (`::: LABEL CMD [ARG...]`), as for
//!   `split`; a registered name reaches its link, so it is a child job.
//! - In the shell VM, `-b LABEL=STRING` runs STRING with the session shell
//!   (a child guest marsh attached to the same session).
//! - On the host, `main.rs` runs the whole fanout in a session shell.
//!
//! Between the two commands flows one JSON line (`marsh.fanout/v1`, branch
//! bytes in base64); `fanout` with a terminal on stdout renders it itself.

use base64::Engine as _;
use std::ffi::OsString;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub const FANOUT_HELP: &str = "Usage: marsh fanout [-n] [-b LABEL=STRING]... [::: LABEL CMD [ARG...]]... | marsh collect [--json] [--timing] [--stderr]\n\nRun every branch at once, here, on the same files (no fork; `split` forks).\n`::: LABEL CMD [ARG...]` runs one command (a registered name starts a job);\n`-b LABEL=STRING` runs STRING with the session shell (not from a job). stdin\nis spooled once and read by every branch (`-n` or a terminal: empty). Waits\nfor every branch and exits with the first nonzero branch status. `collect`\nprints each branch's stdout in order, then a failed branch's stderr under\n`== LABEL stderr ==` (as `join` does; `--stderr` shows every branch's);\nwith a terminal on stdout `fanout` prints that rendering itself.\n\nLimits: 16 branches, 64 MiB input, 16 MiB combined branch output.\n";
pub const COLLECT_HELP: &str = "Usage: marsh fanout ... | marsh collect [--json] [--timing] [--stderr]\n\nPer branch, in order: `== LABEL (state) ==`, its stdout, then a failed\nbranch's stderr under `== LABEL stderr ==`.\n\n  --timing  append per-branch and total wall time\n  --json    emit one JSON document (branch stdout must be UTF-8)\n  --stderr  also show stderr of successful branches\n\nExits with the first nonzero branch status.\n";

const SCHEMA: &str = "marsh.fanout/v1";
const INPUT_LIMIT: u64 = 64 << 20;
const OUTPUT_LIMIT: u64 = 16 << 20;
const MAX_BRANCHES: usize = 16;
const GRACE: Duration = Duration::from_secs(10);

/// Where `fanout` runs, which decides what a `-b` branch may be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Place {
    /// Inside a Kit job: argv branches only.
    Job,
    /// In the shell VM, attached to a session.
    Session,
}

/// How `fanout` writes its result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    /// Decide by stdout: a terminal gets the rendering, anything else the document.
    Auto,
    Document,
    Text,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Branch {
    Argv { label: String, argv: Vec<OsString> },
    Shell { label: String, source: String },
}

impl Branch {
    fn label(&self) -> &str {
        match self {
            Self::Argv { label, .. } | Self::Shell { label, .. } => label,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Plan {
    pub branches: Vec<Branch>,
    pub no_input: bool,
    pub format: Format,
}

fn valid_label(label: &str) -> bool {
    let mut bytes = label.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && label.len() <= 64
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Parse `fanout` arguments (without the verb).
///
/// # Errors
/// Returns a usage message.
pub fn parse(arguments: &[OsString]) -> Result<Plan, String> {
    let mut plan = Plan {
        branches: Vec::new(),
        no_input: false,
        format: Format::Auto,
    };
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("-n") => plan.no_input = true,
            Some("--format=document") => plan.format = Format::Document,
            Some("--format=text") => plan.format = Format::Text,
            Some("-b") => {
                index += 1;
                let (label, source) = arguments
                    .get(index)
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.split_once('='))
                    .ok_or("fanout: -b needs LABEL=STRING")?;
                plan.branches.push(Branch::Shell {
                    label: label.into(),
                    source: source.into(),
                });
            }
            Some(":::") => {
                let label = arguments
                    .get(index + 1)
                    .and_then(|label| label.to_str())
                    .ok_or("fanout: ::: needs LABEL CMD [ARG...]")?
                    .to_owned();
                index += 2;
                let mut argv = Vec::new();
                while index < arguments.len() && arguments[index] != ":::" {
                    argv.push(arguments[index].clone());
                    index += 1;
                }
                if argv.is_empty() {
                    return Err(format!("fanout: branch {label} has no command"));
                }
                plan.branches.push(Branch::Argv { label, argv });
                continue;
            }
            _ => {
                return Err(format!(
                    "fanout: unexpected argument {}; see `marsh fanout --help`",
                    arguments[index].display()
                ));
            }
        }
        index += 1;
    }
    if plan.branches.is_empty() {
        return Err("fanout: no branches; see `marsh fanout --help`".into());
    }
    if plan.branches.len() > MAX_BRANCHES {
        return Err(format!("fanout: at most {MAX_BRANCHES} branches"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for branch in &plan.branches {
        let label = branch.label();
        if !valid_label(label) {
            return Err(format!(
                "fanout: invalid label {label:?} (letters, digits, _ - .; starts with a letter or _)"
            ));
        }
        if !seen.insert(label.to_ascii_lowercase()) {
            return Err(format!("fanout: duplicate label {label}"));
        }
    }
    Ok(plan)
}

fn usage(message: &str) -> i32 {
    eprintln!("marsh: {message}");
    2
}

/// The session shell for a `-b` branch: this guest binary attached to the
/// caller's session, as Brush runs a script without a shebang.
fn session_shell(source: &str) -> Result<Command, String> {
    let context = std::env::var_os(crate::external_commands::SESSION_VARIABLE)
        .ok_or("shell branches need an attached marsh session")?;
    let session: crate::session::SessionConfig = serde_json::from_slice(context.as_encoded_bytes())
        .map_err(|_| "invalid marsh session context")?;
    let id = session.session_id.ok_or("missing marsh session identity")?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut command = Command::new(executable);
    command.arg0("marsh");
    command.args(["--marsh-guest", "--marsh-session", &id]);
    if session.ephemeral_home {
        command.arg("--ephemeral-home").arg("--marsh-home-backing");
        command.arg(&session.home_backing);
    }
    command.args(["-c", source]);
    Ok(command)
}

fn install_signal_handlers() -> Arc<AtomicUsize> {
    let received = Arc::new(AtomicUsize::new(0));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        let _ = signal_hook::flag::register_usize(
            signal,
            Arc::clone(&received),
            usize::try_from(signal).unwrap_or(0),
        );
    }
    received
}

struct Running {
    label: String,
    child: Child,
    started: Instant,
    finished: Option<(u8, Duration)>,
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
}

fn capture(
    mut reader: impl std::io::Read,
    remaining: &AtomicU64,
    exceeded: &AtomicBool,
) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    while let Ok(read) = reader.read(&mut buffer) {
        if read == 0 {
            break;
        }
        let requested = read as u64;
        let granted = remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |available| {
                Some(available.saturating_sub(requested))
            })
            .unwrap_or(0)
            .min(requested);
        output.extend_from_slice(&buffer[..usize::try_from(granted).unwrap_or(0)]);
        if granted != requested {
            exceeded.store(true, Ordering::Relaxed);
        }
    }
    output
}

fn status_byte(status: std::process::ExitStatus) -> u8 {
    status
        .code()
        .map(|code| u8::try_from(code & 0xff).unwrap_or(1))
        .or_else(|| {
            status
                .signal()
                .map(|signal| u8::try_from(128 + signal).unwrap_or(255))
        })
        .unwrap_or(1)
}

fn signal_group(child: &Child, signal: nix::sys::signal::Signal) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
    }
}

/// `marsh fanout ARGS` in a job or the shell VM.
#[must_use]
pub fn fanout(arguments: &[OsString], place: Place) -> i32 {
    if matches!(
        arguments.first().and_then(|argument| argument.to_str()),
        Some("-h" | "--help")
    ) {
        print!("{FANOUT_HELP}");
        return 0;
    }
    let plan = match parse(arguments) {
        Ok(plan) => plan,
        Err(message) => return usage(&message),
    };
    if place == Place::Job
        && let Some(branch) = plan
            .branches
            .iter()
            .find(|branch| matches!(branch, Branch::Shell { .. }))
    {
        return usage(&format!(
            "fanout: {}: shell branches (-b) run only from the session shell; from a job \
             use argv branches: ::: LABEL CMD [ARG...]",
            branch.label()
        ));
    }
    let mut input = Vec::new();
    if !plan.no_input && !std::io::stdin().is_terminal() {
        let mut stdin = std::io::stdin().lock().take(INPUT_LIMIT + 1);
        if stdin.read_to_end(&mut input).is_err() || input.len() as u64 > INPUT_LIMIT {
            return usage("fanout: input exceeds 64 MiB limit");
        }
    }
    let received = install_signal_handlers();
    let input = Arc::new(input);
    let remaining = Arc::new(AtomicU64::new(OUTPUT_LIMIT));
    let exceeded = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let mut running = match spawn_all(&plan.branches, &input, &remaining, &exceeded) {
        Ok(running) => running,
        Err(status) => return status,
    };
    let stop = wait_all(&mut running, &received, &exceeded);
    let total = started.elapsed();
    let results = running
        .into_iter()
        .map(|branch| {
            let (status, elapsed) = branch.finished.unwrap_or((1, Duration::ZERO));
            brush_core::BranchResult {
                label: branch.label,
                status,
                elapsed,
                stdout: branch.stdout.join().unwrap_or_default(),
                stderr: branch.stderr.join().unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    if let Some((signal, _)) = stop.filter(|(signal, _)| *signal != 0) {
        return 128 + signal;
    }
    if exceeded.load(Ordering::Relaxed) {
        eprintln!("marsh: fanout: combined output exceeds 16 MiB limit");
        return 1;
    }
    let status = first_failure(&results);
    let text = match plan.format {
        Format::Auto => std::io::stdout().is_terminal(),
        Format::Document => false,
        Format::Text => true,
    };
    let written = if text {
        render(&results, total, brush_core::CollectOptions::default())
    } else {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&document(&results, total))
            .and_then(|()| stdout.flush())
            .map_err(|error| error.to_string())
    };
    match written {
        Ok(()) => i32::from(status),
        Err(message) => {
            eprintln!("marsh: fanout: {message}");
            1
        }
    }
}

/// A random fanout id (no cryptographic need; display grouping only).
fn rand_u64() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |now| now.as_nanos()),
    );
    hasher.write_u32(std::process::id());
    hasher.finish()
}

/// Start every branch in its own process group with piped stdio; on a
/// failure the started ones are killed and the exit status is returned.
fn spawn_all(
    branches: &[Branch],
    input: &Arc<Vec<u8>>,
    remaining: &Arc<AtomicU64>,
    exceeded: &Arc<AtomicBool>,
) -> Result<Vec<Running>, i32> {
    let mut running = Vec::new();
    // Jobs a branch starts carry `FANOUT_BRANCH=<id>/<label>` so `marsh jobs
    // --tree` groups them under one fanout node (as the Brush sugar does).
    let fanout_id = format!("{:016x}", rand_u64());
    for branch in branches {
        let mut command = match branch {
            Branch::Argv { argv, .. } => {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                command
            }
            Branch::Shell { source, .. } => match session_shell(source) {
                Ok(command) => command,
                Err(message) => {
                    kill_all(&mut running);
                    return Err(usage(&format!("fanout: {}: {message}", branch.label())));
                }
            },
        };
        // Its own process group, so a signal or the output limit reaches the
        // branch's descendants too.
        command
            .env("FANOUT_BRANCH", format!("{fanout_id}/{}", branch.label()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                kill_all(&mut running);
                let name = match branch {
                    Branch::Argv { argv, .. } => argv[0].to_string_lossy().into_owned(),
                    Branch::Shell { .. } => "marsh".into(),
                };
                eprintln!("marsh: fanout: {}: {name}: {error}", branch.label());
                return Err(if error.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                });
            }
        };
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            unreachable!("piped stdio");
        };
        let bytes = Arc::clone(input);
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
        let (left, over) = (Arc::clone(remaining), Arc::clone(exceeded));
        let stdout = std::thread::spawn(move || capture(stdout, &left, &over));
        let (left, over) = (Arc::clone(remaining), Arc::clone(exceeded));
        let stderr = std::thread::spawn(move || capture(stderr, &left, &over));
        running.push(Running {
            label: branch.label().to_owned(),
            child,
            started: Instant::now(),
            finished: None,
            stdout,
            stderr,
        });
    }
    Ok(running)
}

/// Wait for every branch; a signal (forwarded) or the output limit (KILL)
/// stops them, with KILL after the grace. Returns the stop, if any.
fn wait_all(
    running: &mut [Running],
    received: &AtomicUsize,
    exceeded: &AtomicBool,
) -> Option<(i32, Instant)> {
    let mut stop: Option<(i32, Instant)> = None;
    loop {
        for branch in running.iter_mut() {
            if branch.finished.is_none()
                && let Ok(Some(status)) = branch.child.try_wait()
            {
                branch.finished = Some((status_byte(status), branch.started.elapsed()));
            }
        }
        if running.iter().all(|branch| branch.finished.is_some()) {
            break;
        }
        let signal = i32::try_from(received.load(Ordering::Relaxed)).unwrap_or(0);
        if stop.is_none() && (signal != 0 || exceeded.load(Ordering::Relaxed)) {
            let forwarded = nix::sys::signal::Signal::try_from(signal)
                .unwrap_or(nix::sys::signal::Signal::SIGKILL);
            for branch in running.iter().filter(|branch| branch.finished.is_none()) {
                signal_group(&branch.child, forwarded);
            }
            stop = Some((signal, Instant::now()));
        }
        if let Some((_, since)) = stop
            && since.elapsed() > GRACE
        {
            kill_all(running);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    stop
}

fn kill_all(running: &mut [Running]) {
    for branch in running
        .iter_mut()
        .filter(|branch| branch.finished.is_none())
    {
        signal_group(&branch.child, nix::sys::signal::Signal::SIGKILL);
        let _ = branch.child.kill();
        if let Ok(status) = branch.child.wait() {
            branch.finished = Some((status_byte(status), branch.started.elapsed()));
        }
    }
}

fn first_failure(results: &[brush_core::BranchResult]) -> u8 {
    results
        .iter()
        .find(|result| result.status != 0)
        .map_or(0, |result| result.status)
}

/// The `fanout` → `collect` document: one JSON line.
#[must_use]
pub fn document(results: &[brush_core::BranchResult], total: Duration) -> Vec<u8> {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let value = serde_json::json!({
        "schema": SCHEMA,
        "total_ms": u64::try_from(total.as_millis()).unwrap_or(u64::MAX),
        "branches": results.iter().map(|branch| serde_json::json!({
            "label": branch.label,
            "status": branch.status,
            "duration_ms": u64::try_from(branch.elapsed.as_millis()).unwrap_or(u64::MAX),
            "stdout": encode(&branch.stdout),
            "stderr": encode(&branch.stderr),
        })).collect::<Vec<_>>(),
    });
    let mut bytes = value.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

/// Read a `fanout` document.
///
/// # Errors
/// Returns why the bytes are not one.
pub fn read_document(bytes: &[u8]) -> Result<(Vec<brush_core::BranchResult>, Duration), String> {
    let not = || "collect: stdin is not `marsh fanout` output".to_owned();
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| not())?;
    if value.get("schema").and_then(serde_json::Value::as_str) != Some(SCHEMA) {
        return Err(not());
    }
    let decode = |value: &serde_json::Value| {
        base64::engine::general_purpose::STANDARD
            .decode(value.as_str().unwrap_or_default())
            .map_err(|_| not())
    };
    let millis = |value: Option<&serde_json::Value>| {
        Duration::from_millis(value.and_then(serde_json::Value::as_u64).unwrap_or(0))
    };
    let branches = value
        .get("branches")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(not)?
        .iter()
        .map(|branch| {
            Ok(brush_core::BranchResult {
                label: branch
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(not)?
                    .to_owned(),
                status: branch
                    .get("status")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|status| u8::try_from(status).ok())
                    .ok_or_else(not)?,
                elapsed: millis(branch.get("duration_ms")),
                stdout: decode(&branch["stdout"])?,
                stderr: decode(&branch["stderr"])?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((branches, millis(value.get("total_ms"))))
}

fn render(
    results: &[brush_core::BranchResult],
    total: Duration,
    options: brush_core::CollectOptions,
) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    brush_core::render_collected(&mut stdout, results, total, options)
        .and_then(|()| stdout.flush())
        .map_err(|error| error.to_string())
}

/// `marsh collect ARGS`: render a `fanout` document from stdin.
#[must_use]
pub fn collect(arguments: &[OsString]) -> i32 {
    let mut options = brush_core::CollectOptions::default();
    for argument in arguments {
        match argument.to_str() {
            Some("--json") => options.json = true,
            Some("--timing") => options.timing = true,
            Some("--stderr") => options.stderr = true,
            Some("-h" | "--help") => {
                print!("{COLLECT_HELP}");
                return 0;
            }
            _ => {
                return usage(&format!(
                    "collect: unknown option: {}",
                    argument.as_bytes().escape_ascii()
                ));
            }
        }
    }
    let mut input = Vec::new();
    if std::io::stdin()
        .lock()
        .take(2 * (INPUT_LIMIT + OUTPUT_LIMIT))
        .read_to_end(&mut input)
        .is_err()
    {
        return usage("collect: cannot read stdin");
    }
    let (results, total) = match read_document(&input) {
        Ok(read) => read,
        Err(message) => return usage(&message),
    };
    match render(&results, total, options) {
        Ok(()) => i32::from(first_failure(&results)),
        Err(message) => {
            eprintln!("marsh: collect: {message}");
            1
        }
    }
}

fn single_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The host form: the same fanout run by a session shell in the shell VM,
/// so branches see the project there. The host decides the output form
/// (the session may give the VM side a terminal) and, with a terminal on
/// stdin, empty input.
///
/// # Errors
/// Returns a usage message.
pub fn host_script(arguments: &[OsString]) -> Result<String, String> {
    parse(arguments)?;
    let mut script = String::from("exec marsh fanout");
    script.push_str(if std::io::stdout().is_terminal() {
        " --format=text"
    } else {
        " --format=document"
    });
    if std::io::stdin().is_terminal() {
        script.push_str(" -n");
    }
    for argument in arguments {
        let word = argument
            .to_str()
            .ok_or("fanout: arguments must be UTF-8 on the host")?;
        if word.starts_with("--format=") {
            continue;
        }
        script.push(' ');
        script.push_str(&single_quote(word));
    }
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn labels_are_checked_like_split() {
        assert!(parse(&words(&["::: a", "x"])).is_err());
        assert!(parse(&words(&[":::", "a", "true", ":::", "A", "true"])).is_err());
        assert!(parse(&words(&[":::", "a"])).is_err());
        let plan = parse(&words(&["-n", "-b", "s=echo :::", ":::", "a", "echo", "x"])).unwrap();
        assert!(plan.no_input);
        assert_eq!(
            plan.branches[0],
            Branch::Shell {
                label: "s".into(),
                source: "echo :::".into()
            }
        );
    }

    #[test]
    fn host_words_are_quoted_exactly() {
        assert_eq!(single_quote("it's $x"), "'it'\\''s $x'");
    }

    #[test]
    fn the_document_round_trips_bytes() {
        let results = vec![brush_core::BranchResult {
            label: "a".into(),
            status: 3,
            elapsed: Duration::from_millis(12),
            stdout: vec![0, 255, b'\n'],
            stderr: b"e".to_vec(),
        }];
        let (read, total) = read_document(&document(&results, Duration::from_millis(40))).unwrap();
        assert_eq!(total, Duration::from_millis(40));
        assert_eq!(read[0].stdout, results[0].stdout);
        assert_eq!(read[0].status, 3);
        assert!(read_document(b"split-1234\n").is_err());
    }
}
