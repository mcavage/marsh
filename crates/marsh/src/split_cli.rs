//! `marsh split`, `marsh join`, and `marsh splits` (`docs/design/workspaces.md` s2):
//! thin clients of the daemon's split requests. The same code runs on the
//! host (an attached ephemeral session), in the shell VM (the session relay),
//! and in a Kit container (the capability socket).

use marsh_daemon::split::{BranchSpec, SplitCreateSpec, SplitRelease};
use marsh_daemon::{AttachmentFrame, PublicReply, PublicRequest, read_frame, write_frame};
use std::ffi::OsString;
use std::io::{IsTerminal as _, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;

pub const SPLIT_HELP: &str = "Usage: marsh split [-n] [-b LABEL=STRING]... [::: LABEL CMD [ARG...]]...\n\nRun each branch in a private fork of the current tree. `-b` runs STRING with\nthe session shell in the shell VM (trusted, your own commands); `:::` runs one\nregistered command in a fresh confined container. stdin is spooled once and\nread by every branch (`-n` or a terminal: empty). Waits for every branch,\nprints one handle line, and exits with the first nonzero branch status.\n";
pub const JOIN_HELP: &str = "Usage: marsh join [--json] [--keep] [--timing] [-- CMD [ARG...]]\n\nRead a split handle on stdin (creator only). With `-- CMD`, CMD reads the\nrendering with SPLIT_ID, SPLIT_DIR, SPLIT_MANIFEST, and SPLIT_OBJECTS set;\nthe split is removed when CMD exits 0 and kept otherwise. Without CMD the\nrendering (or manifest with --json) goes to stdout. --timing prints the\nsplit's phase times (snapshot, forks, run, capture, consumer) on stderr.\n";
pub const SPLITS_HELP: &str = "Usage: marsh splits [--json] [ID]\n       marsh splits cancel ID\n       marsh splits rm ID\n\nList this scope's splits, cancel a running one, or remove a kept one.\nWith ID, a joined split is still shown (states and phase times, no files).\n";

const SPOOL_LIMIT: usize = 64 << 20;
const FRAME_CHUNK: usize = 64 << 10;

/// Where requests go: the caller's daemon channel and, for split creation,
/// the session the branches run under.
pub struct Channel {
    pub client: marsh_daemon::Client,
    pub session: Option<marsh_daemon::SessionSpec>,
    pub detach: Option<String>,
}

fn usage(message: &str) -> i32 {
    eprintln!("marsh: {message}");
    2
}

fn text(argument: &OsString) -> Option<&str> {
    argument.to_str()
}

/// Run `split`, `join`, or `splits`; `arguments` excludes the verb.
pub fn run(
    verb: &str,
    arguments: &[OsString],
    channel: impl FnOnce(bool) -> Result<Channel, String>,
) -> i32 {
    match verb {
        "split" => split(arguments, channel),
        "join" => join(arguments, channel),
        _ => splits(arguments, channel),
    }
}

#[must_use]
pub fn help(verb: &str, arguments: &[OsString]) -> Option<&'static str> {
    arguments
        .first()
        .filter(|argument| matches!(argument.to_str(), Some("-h" | "--help")))
        .map(|_| match verb {
            "split" => SPLIT_HELP,
            "join" => JOIN_HELP,
            _ => SPLITS_HELP,
        })
}

fn parse_split(arguments: &[OsString]) -> Result<(Vec<BranchSpec>, bool), String> {
    let mut branches = Vec::new();
    let mut no_input = false;
    let mut index = 0;
    while index < arguments.len() {
        match text(&arguments[index]) {
            Some("-n") => no_input = true,
            Some("-b") => {
                index += 1;
                let value = arguments
                    .get(index)
                    .and_then(text)
                    .ok_or("split: -b needs LABEL=STRING")?;
                let (label, shell) = value
                    .split_once('=')
                    .ok_or("split: -b needs LABEL=STRING")?;
                branches.push(BranchSpec {
                    label: label.into(),
                    shell: Some(shell.into()),
                    argv: None,
                });
            }
            Some(":::") => {
                let label = arguments
                    .get(index + 1)
                    .and_then(text)
                    .ok_or("split: ::: needs LABEL CMD [ARG...]")?;
                let mut argv = Vec::new();
                index += 2;
                while index < arguments.len() && text(&arguments[index]) != Some(":::") {
                    argv.push(arguments[index].as_bytes().to_vec());
                    index += 1;
                }
                if argv.is_empty() {
                    return Err(format!("split: branch {label} has no command"));
                }
                branches.push(BranchSpec {
                    label: label.into(),
                    shell: None,
                    argv: Some(argv),
                });
                continue;
            }
            _ => {
                return Err(format!(
                    "split: unexpected argument {}; see `marsh split --help`",
                    arguments[index].display()
                ));
            }
        }
        index += 1;
    }
    if branches.is_empty() {
        return Err("split: no branches; see `marsh split --help`".into());
    }
    Ok((branches, no_input))
}

/// Exported variables for branches, minus marsh, SBX, Docker, token, and
/// host-placement variables (`docs/design/workspaces.md` s2).
/// The environment offered to branches; in a job, what a child gets (s6).
fn exported_environment() -> (
    marsh_contracts::ExportedEnvironment,
    std::collections::BTreeMap<String, String>,
) {
    if let Some(document) = std::fs::read(marsh_contracts::process::JOB_JSON)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        return crate::job::offered(&document);
    }
    let (mut environment, _) =
        crate::registered_commands::collect_exported_environment(std::env::vars_os());
    environment.retain(|name, _| marsh_daemon::split::forwardable(name));
    (environment, std::collections::BTreeMap::new())
}

fn split(arguments: &[OsString], channel: impl FnOnce(bool) -> Result<Channel, String>) -> i32 {
    let (branches, no_input) = match parse_split(arguments) {
        Ok(parsed) => parsed,
        Err(message) => return usage(&message),
    };
    let mut spool = Vec::new();
    if !no_input && !std::io::stdin().is_terminal() {
        let mut stdin = std::io::stdin().lock().take(SPOOL_LIMIT as u64 + 1);
        if stdin.read_to_end(&mut spool).is_err() || spool.len() > SPOOL_LIMIT {
            return usage("split: input exceeds the 64 MiB spool");
        }
    }
    let channel = match channel(true) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("split: {message}")),
    };
    let status = create(&channel, branches, &spool);
    if let Some(session) = &channel.detach {
        let _ = channel.client.request(PublicRequest::DetachShell {
            session_id: session.clone(),
        });
    }
    status
}

fn create(channel: &Channel, branches: Vec<BranchSpec>, spool: &[u8]) -> i32 {
    let Some(session) = channel.session.clone() else {
        return usage("split: no session for branches");
    };
    let Ok(cwd) = std::env::current_dir() else {
        return usage("split: cannot determine the working directory");
    };
    let (environment, start_env) = exported_environment();
    let spec = SplitCreateSpec {
        session,
        cwd,
        branches,
        environment,
        start_env,
    };
    let mut stream = match channel
        .client
        .open_request(PublicRequest::SplitCreate(spec))
    {
        Ok(stream) => stream,
        Err(error) => return usage(&format!("split: {error}")),
    };
    // A refusal arrives before the spool is read; it is still the reply.
    let _ = (|| -> Result<(), marsh_daemon::DaemonError> {
        for chunk in spool.chunks(FRAME_CHUNK) {
            write_frame(
                &mut stream,
                &AttachmentFrame::Stdin {
                    bytes: chunk.to_vec(),
                },
            )?;
        }
        write_frame(&mut stream, &AttachmentFrame::StdinEof)
    })();
    match read_frame::<PublicReply>(&mut stream) {
        Ok(PublicReply::SplitStarted { .. }) => {}
        Ok(PublicReply::Error { message, .. }) => return usage(&message),
        Ok(reply) => return usage(&format!("split: unexpected reply {reply:?}")),
        Err(error) => return usage(&format!("split: {error}")),
    }
    let _cancel = CancelOnSignal::start(&stream);
    match read_frame::<PublicReply>(&mut stream) {
        Ok(PublicReply::SplitFinished {
            id,
            status,
            cancelled,
            message,
        }) => {
            if cancelled {
                eprintln!("split {id}: {}", message.as_deref().unwrap_or("cancelled"));
                return 130;
            }
            if let Some(message) = message {
                eprintln!("split {id}: {message}");
            }
            println!("{{\"marsh_split\":1,\"id\":\"{id}\"}}");
            status
        }
        Ok(reply) => usage(&format!("split: unexpected reply {reply:?}")),
        Err(error) => {
            eprintln!(
                "marsh: split: lost the daemon before capture ({error}); the split is uncertain"
            );
            125
        }
    }
}

/// SIGINT, SIGTERM, or SIGHUP at the client sends a cancel on the stream.
struct CancelOnSignal(signal_hook::iterator::Handle);

impl CancelOnSignal {
    fn start(stream: &UnixStream) -> Option<Self> {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
        let mut writer = stream.try_clone().ok()?;
        let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM, SIGHUP]).ok()?;
        let handle = signals.handle();
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                let _ = write_frame(
                    &mut writer,
                    &AttachmentFrame::Signal {
                        signal: "INT".into(),
                    },
                );
            }
        });
        Some(Self(handle))
    }
}

impl Drop for CancelOnSignal {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn read_handle() -> Result<String, String> {
    let mut input = Vec::new();
    std::io::stdin()
        .lock()
        .take(SPOOL_LIMIT as u64)
        .read_to_end(&mut input)
        .map_err(|error| error.to_string())?;
    String::from_utf8_lossy(&input)
        .lines()
        .rev()
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            (value.get("marsh_split")? == 1)
                .then(|| value.get("id")?.as_str().map(str::to_owned))
                .flatten()
        })
        .filter(|id| marsh_daemon::split::valid_id(id))
        .ok_or_else(|| "join: no split handle on stdin".to_owned())
}

fn join(arguments: &[OsString], channel: impl FnOnce(bool) -> Result<Channel, String>) -> i32 {
    let (mut json, mut keep, mut timing) = (false, false, false);
    let mut command = None;
    for (index, argument) in arguments.iter().enumerate() {
        match text(argument) {
            Some("--json") => json = true,
            Some("--keep") => keep = true,
            Some("--timing") => timing = true,
            Some("--") if index + 1 < arguments.len() => {
                command = Some(&arguments[index + 1..]);
                break;
            }
            _ => {
                return usage(
                    "join: usage: marsh join [--json] [--keep] [--timing] [-- CMD [ARG...]]",
                );
            }
        }
    }
    let id = match read_handle() {
        Ok(id) => id,
        Err(message) => return usage(&message),
    };
    let channel = match channel(false) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("join: {message}")),
    };
    let mut stream = match channel
        .client
        .open_request(PublicRequest::SplitJoin { split: id })
    {
        Ok(stream) => stream,
        Err(error) => return usage(&format!("join: {error}")),
    };
    let (id, manifest, rendering, dir, objects) = match read_frame::<PublicReply>(&mut stream) {
        Ok(PublicReply::SplitJoined {
            id,
            manifest,
            rendering,
            dir,
            objects,
        }) => (id, manifest, rendering, dir, objects),
        Ok(PublicReply::Error { message, .. }) => {
            eprintln!("marsh: join: {message}");
            return 1;
        }
        Ok(reply) => return usage(&format!("join: unexpected reply {reply:?}")),
        Err(error) => return usage(&format!("join: {error}")),
    };
    let input = if json {
        format!("{manifest}\n").into_bytes()
    } else {
        rendering
    };
    let branch_status = serde_json::from_str::<serde_json::Value>(&manifest)
        .ok()
        .and_then(|value| value.get("status").and_then(serde_json::Value::as_i64))
        .and_then(|status| i32::try_from(status).ok())
        .unwrap_or(0);
    let consuming = std::time::Instant::now();
    let status = if let Some(command) = command {
        consume(command, &input, &id, &dir, objects.as_deref())
    } else {
        let mut stdout = std::io::stdout().lock();
        // Branch bytes are untrusted: never send their control sequences to
        // a terminal.
        let input = if stdout.is_terminal() {
            printable(&input)
        } else {
            input
        };
        let _ = stdout.write_all(&input);
        let _ = stdout.flush();
        branch_status
    };
    let consumed = status;
    if timing {
        print_timing(&id, &manifest, consuming.elapsed());
    }
    let _ = write_frame(
        &mut stream,
        &SplitRelease {
            status: consumed,
            keep,
            consumer: Some(
                if command.is_some() {
                    "join command"
                } else {
                    "a branch"
                }
                .into(),
            ),
        },
    );
    if let Ok(PublicReply::SplitDone { message }) = read_frame::<PublicReply>(&mut stream)
        && message.starts_with("kept")
    {
        eprintln!("marsh: join: {message}");
    }
    status
}

/// `join --timing`: the manifest's phase times plus the consumer's, on stderr.
fn print_timing(id: &str, manifest: &str, consumer: std::time::Duration) {
    let mut phases = serde_json::from_str::<serde_json::Value>(manifest)
        .ok()
        .and_then(|value| value.get("timing_ms").cloned())
        .unwrap_or_default();
    phases["consumer"] = u64::try_from(consumer.as_millis())
        .unwrap_or(u64::MAX)
        .into();
    eprintln!("split {id} {}", timing_line(&phases));
}

/// Run `-- CMD` with the rendering on stdin and `SPLIT_*` set.
fn consume(
    command: &[OsString],
    input: &[u8],
    id: &str,
    dir: &Path,
    objects: Option<&Path>,
) -> i32 {
    let mut process = std::process::Command::new(&command[0]);
    process
        .args(&command[1..])
        .stdin(std::process::Stdio::piped())
        .env("SPLIT_ID", id)
        .env("SPLIT_DIR", dir)
        .env("SPLIT_MANIFEST", dir.join("manifest.json"));
    if let Some(objects) = objects {
        process.env("SPLIT_OBJECTS", objects);
    }
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("marsh: join: {}: {error}", command[0].to_string_lossy());
            return 127;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        // CMD need not read its input (`join -- true`).
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    match child.wait() {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(_) => 125,
    }
}

fn splits(arguments: &[OsString], channel: impl FnOnce(bool) -> Result<Channel, String>) -> i32 {
    let words = arguments
        .iter()
        .map(|a| a.to_str().unwrap_or(""))
        .collect::<Vec<_>>();
    let request = match words.as_slice() {
        ["cancel", id] => PublicRequest::SplitCancel {
            split: (*id).into(),
        },
        ["rm", id] => PublicRequest::SplitRemove {
            split: (*id).into(),
        },
        ["--json"] | [] => PublicRequest::SplitShow { split: None },
        ["--json", id] | [id] | [id, "--json"] => PublicRequest::SplitShow {
            split: Some((*id).into()),
        },
        _ => return usage("usage: marsh splits [--json] [ID] | cancel ID | rm ID"),
    };
    let json = words.contains(&"--json");
    let channel = match channel(false) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("splits: {message}")),
    };
    match channel.client.request(request) {
        Ok(PublicReply::Splits { document }) if json => {
            println!("{document}");
            0
        }
        Ok(PublicReply::Splits { document }) => {
            for split in document["splits"].as_array().into_iter().flatten() {
                let parent = split["parent"]
                    .as_object()
                    .map(|parent| {
                        format!(
                            "  (child of {}/{})",
                            parent["split"].as_str().unwrap_or(""),
                            parent["label"].as_str().unwrap_or("")
                        )
                    })
                    .unwrap_or_default();
                println!(
                    "{}  {}  {}{parent}",
                    split["id"].as_str().unwrap_or(""),
                    split["state"].as_str().unwrap_or(""),
                    split["dir"].as_str().unwrap_or("")
                );
                if let Some(reason) = split["reason"].as_str() {
                    println!("  ({reason})");
                }
                if split["removed"].as_bool() == Some(true) {
                    println!("  (joined and removed; its files are gone)");
                }
                if split["timing_ms"]
                    .as_object()
                    .is_some_and(|t| !t.is_empty())
                {
                    println!("  {}", timing_line(&split["timing_ms"]));
                }
                for branch in split["branches"].as_array().into_iter().flatten() {
                    let files = branch["files"]
                        .as_u64()
                        .and_then(|files| usize::try_from(files).ok())
                        .unwrap_or(0);
                    println!(
                        "  {}  {}  {}  job {}  {}",
                        branch["label"].as_str().unwrap_or(""),
                        branch["placement"]
                            .as_str()
                            .or_else(|| branch["kind"].as_str())
                            .unwrap_or(""),
                        branch["status"]
                            .as_str()
                            .unwrap_or(branch["state"].as_str().unwrap_or("")),
                        branch["job_id"].as_str().unwrap_or("-"),
                        marsh_daemon::split::plural(files, "file changed", "files changed")
                    );
                }
            }
            0
        }
        Ok(PublicReply::SplitDone { message }) => {
            eprintln!("marsh: {message}");
            0
        }
        Ok(PublicReply::Error { message, .. }) => {
            eprintln!("marsh: splits: {message}");
            1
        }
        Ok(reply) => usage(&format!("splits: unexpected reply {reply:?}")),
        Err(error) => usage(&format!("splits: {error}")),
    }
}

/// `timing: snapshot 12ms; forks 3ms; run 4.1s; capture fix 20ms, review
/// 5ms; consumer 46.3s`: a split's phase wall times (`timing_ms`).
#[must_use]
pub fn timing_line(timing: &serde_json::Value) -> String {
    let ms = |name: &str| timing.get(name).and_then(serde_json::Value::as_u64);
    let show = |ms: u64| {
        if ms < 1000 {
            format!("{ms}ms")
        } else {
            #[allow(clippy::cast_precision_loss)] // A display duration.
            let seconds = ms as f64 / 1000.0;
            format!("{seconds:.1}s")
        }
    };
    let mut parts = Vec::new();
    for phase in ["snapshot", "forks", "run"] {
        if let Some(value) = ms(phase) {
            parts.push(format!("{phase} {}", show(value)));
        }
    }
    let captures = timing
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            Some(format!(
                "{} {}",
                name.strip_prefix("capture:")?,
                show(value.as_u64()?)
            ))
        })
        .collect::<Vec<_>>();
    if !captures.is_empty() {
        parts.push(format!("capture {}", captures.join(", ")));
    }
    if let Some(value) = ms("consumer") {
        parts.push(format!("consumer {}", show(value)));
    }
    format!("timing: {}", parts.join("; "))
}

/// Brush `split { ... } | join` sugar: shell branches through the session's
/// relay, then a join whose release follows the last stage.
pub struct SessionSplit {
    pub session: marsh_daemon::SessionSpec,
}

impl SessionSplit {
    #[allow(clippy::unused_self)]
    fn client(&self) -> Result<marsh_daemon::Client, String> {
        relay()
    }
}

/// Escape control bytes (other than newline and tab) for a terminal.
fn printable(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            b'\n' | b'\t' => out.push(*byte),
            0..0x20 | 0x7f => out.extend_from_slice(format!("\\x{byte:02x}").as_bytes()),
            _ => out.push(*byte),
        }
    }
    out
}

fn relay() -> Result<marsh_daemon::Client, String> {
    let socket = std::env::var_os("MARSH_DAEMON_SOCKET").ok_or("no daemon relay")?;
    let token = std::env::var_os("MARSH_DAEMON_TOKEN").ok_or("no daemon relay")?;
    marsh_daemon::Client::connect_relay(Path::new(&socket), Path::new(&token))
        .map_err(|error| error.to_string())
}

fn reply(stream: &mut UnixStream) -> Result<PublicReply, String> {
    read_frame::<PublicReply>(stream).map_err(|e| e.to_string())
}

impl brush_shell::bundled::split::SplitWorkspace for SessionSplit {
    fn start(
        &self,
        cwd: &Path,
        branches: &[brush_shell::bundled::split::SplitBranchRequest],
        environment: Vec<(String, String)>,
        input: Vec<u8>,
        json: bool,
    ) -> Result<Box<dyn brush_shell::bundled::split::SplitHandle>, String> {
        let spec = SplitCreateSpec {
            session: self.session.clone(),
            cwd: cwd.to_path_buf(),
            branches: branches
                .iter()
                .map(|branch| BranchSpec {
                    label: branch.label.clone(),
                    shell: Some(branch.source.clone()),
                    argv: None,
                })
                .collect(),
            environment: environment
                .into_iter()
                .filter(|(name, _)| marsh_daemon::split::forwardable(name))
                .map(|(name, value)| (name, value.into_bytes()))
                .collect(),
            start_env: std::collections::BTreeMap::new(),
        };
        let client = self.client()?;
        let mut stream = client
            .open_request(PublicRequest::SplitCreate(spec))
            .map_err(|error| error.to_string())?;
        let _ = send_spool(&mut stream, &input);
        match reply(&mut stream)? {
            PublicReply::SplitStarted { .. } => Ok(Box::new(SugarSplit {
                stream,
                json,
                client,
            })),
            PublicReply::Error { message, .. } => Err(message),
            other => Err(format!("unexpected reply {other:?}")),
        }
    }
}

fn send_spool(stream: &mut UnixStream, input: &[u8]) -> Result<(), marsh_daemon::DaemonError> {
    for chunk in input.chunks(FRAME_CHUNK) {
        write_frame(
            stream,
            &AttachmentFrame::Stdin {
                bytes: chunk.to_vec(),
            },
        )?;
    }
    write_frame(stream, &AttachmentFrame::StdinEof)
}

/// A running sugar split: its create stream (a signal frame cancels it).
struct SugarSplit {
    stream: UnixStream,
    json: bool,
    client: marsh_daemon::Client,
}

impl brush_shell::bundled::split::SplitHandle for SugarSplit {
    fn canceller(&self) -> Box<dyn Fn() + Send + Sync> {
        let writer = self.stream.try_clone().ok().map(std::sync::Mutex::new);
        Box::new(move || {
            if let Some(writer) = &writer {
                let mut writer = writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = write_frame(
                    &mut *writer,
                    &AttachmentFrame::Signal {
                        signal: "INT".into(),
                    },
                );
            }
        })
    }

    fn wait(mut self: Box<Self>) -> Result<brush_shell::bundled::split::SplitRun, String> {
        let (id, status) = match reply(&mut self.stream)? {
            PublicReply::SplitFinished {
                cancelled: true, ..
            } => {
                return Ok(brush_shell::bundled::split::SplitRun {
                    status: 130,
                    cancelled: true,
                    input: Vec::new(),
                    environment: Vec::new(),
                    release: Box::new(|_, _| Vec::new()),
                });
            }
            PublicReply::SplitFinished { id, status, .. } => (id, status),
            other => return Err(format!("unexpected reply {other:?}")),
        };
        let mut join = self
            .client
            .open_request(PublicRequest::SplitJoin { split: id })
            .map_err(|error| error.to_string())?;
        let (id, manifest, rendering, dir, objects) = match reply(&mut join)? {
            PublicReply::SplitJoined {
                id,
                manifest,
                rendering,
                dir,
                objects,
            } => (id, manifest, rendering, dir, objects),
            PublicReply::Error { message, .. } => return Err(message),
            other => return Err(format!("unexpected reply {other:?}")),
        };
        drop(self.stream);
        let mut environment = vec![
            ("SPLIT_ID".to_owned(), id),
            ("SPLIT_DIR".to_owned(), dir.display().to_string()),
            (
                "SPLIT_MANIFEST".to_owned(),
                dir.join("manifest.json").display().to_string(),
            ),
        ];
        if let Some(objects) = objects {
            environment.push(("SPLIT_OBJECTS".to_owned(), objects.display().to_string()));
        }
        Ok(brush_shell::bundled::split::SplitRun {
            status: u8::try_from(status).unwrap_or(1),
            cancelled: false,
            input: if self.json {
                format!("{manifest}\n").into_bytes()
            } else {
                rendering
            },
            environment,
            release: Box::new(move |last, keep| {
                let _ = write_frame(
                    &mut join,
                    &SplitRelease {
                        status: i32::from(last),
                        keep,
                        consumer: Some("last stage".into()),
                    },
                );
                match read_frame::<PublicReply>(&mut join) {
                    Ok(PublicReply::SplitDone { message }) if message.starts_with("kept") => {
                        vec![message]
                    }
                    _ => Vec::new(),
                }
            }),
        })
    }
}
