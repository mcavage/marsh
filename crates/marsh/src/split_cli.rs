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

pub const SPLIT_HELP: &str =
    "Usage: marsh split [-n] [-d] [--from CHECKPOINT] [-b LABEL=STRING]... [::: LABEL CMD [ARG...]]...
       marsh split --open [--from CHECKPOINT] [--label TEXT] LABEL...
       marsh split close ID [--json]
       marsh split apply ID LABEL [--3way] [--json]

Run each branch in a private fork of the current tree. `-b` runs STRING with
the session shell in the shell VM (trusted, your own commands); `:::` runs one
registered command in a fresh confined container. stdin is spooled once and
read by every branch (`-n`, `-d`, or a terminal: empty). Waits for every branch,
prints one handle line, and exits with the first nonzero branch status.
Interrupting or killing it cancels every branch.

-d  detach: print the handle line at once and exit 0; the branches run on
    without this command (for long splits, e.g. from an agent's tool call).
    `marsh join` waits for them; `marsh splits cancel ID` stops them.

--open makes one lane per LABEL (a fork with no command), prints
{\"marsh_split\":1,\"id\":ID,\"lanes\":{LABEL:FORK,...}} and returns at once.
Registered commands started in a lane's fork (`cd FORK && marsh run NAME`) are
confined to it; only the split's creator may start them. `close` stops
admitting lane jobs, waits up to 30 s for running ones, captures every lane,
keeps the split, and prints its manifest. `apply` applies a captured lane's
patch to the project in the shell VM. --from bases the forks on a checkpoint.
--label TEXT (1 to 128 bytes, no control characters) is recorded on the split:
in its manifest, `marsh splits --json`, and its `marsh events` lines.
";
pub const CHECKPOINT_HELP: &str = "Usage: marsh checkpoint [--label TEXT] [--json]
       marsh checkpoints [--json] [--label TEXT]
       marsh checkpoints rm ID

Snapshot the current project (as a split's base: Git-ignored files, .git, and
.marsh left out, plus HEAD, index, packed-refs, and excludes) into
.marsh/checkpoints/ID. marsh keeps the newest 20 per label and 200 per project,
none older than 7 days. A checkpoint needs 5 GiB and 5% free on the volume
(exit 75 otherwise). `marsh split --from ID` starts forks from it.
";
pub const EVENTS_HELP: &str = "Usage: marsh events --json

Print daemon changes as JSON lines until killed: a hello line with the
current sequence number and features, then job, split, and checkpoint
changes, and a heartbeat every 15 s. A {\"type\":\"resync\"} line, or the end
of the stream, means changes were missed: list state again and resubscribe.
";
pub const STORAGE_HELP: &str = "Usage: marsh storage [--json] [--all]

Checkpoints and kept splits of the current project (every project marsh knows
with --all): counts and logical bytes, and the volume's free space.
";
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
        "split" => match arguments.first().and_then(text) {
            Some("close") => close(&arguments[1..], channel),
            Some("apply") => apply(&arguments[1..], channel),
            _ => split(arguments, channel),
        },
        "join" => join(arguments, channel),
        "checkpoint" => checkpoint(arguments, channel),
        "checkpoints" => checkpoints(arguments, channel),
        "events" => events(arguments, channel),
        "storage" => storage(arguments, channel),
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
            "checkpoint" | "checkpoints" => CHECKPOINT_HELP,
            "events" => EVENTS_HELP,
            "storage" => STORAGE_HELP,
            _ => SPLITS_HELP,
        })
}

struct SplitArguments {
    branches: Vec<BranchSpec>,
    no_input: bool,
    detach: bool,
    open: bool,
    from: Option<String>,
    label: Option<String>,
}

fn parse_split(arguments: &[OsString]) -> Result<SplitArguments, String> {
    let mut branches = Vec::new();
    let mut no_input = false;
    let mut detach = false;
    let (mut open, mut from, mut label) = (false, None, None);
    let mut index = 0;
    while index < arguments.len() {
        match text(&arguments[index]) {
            Some("-n") => no_input = true,
            Some("-d") => detach = true,
            Some("--open") => open = true,
            Some("--label") => {
                index += 1;
                label = Some(
                    arguments
                        .get(index)
                        .and_then(text)
                        .filter(|label| marsh_daemon::valid_label(label))
                        .ok_or("split: --label needs 1 to 128 bytes of text with no control characters")?
                        .to_owned(),
                );
            }
            Some("--from") => {
                index += 1;
                from = Some(
                    arguments
                        .get(index)
                        .and_then(text)
                        .filter(|id| marsh_daemon::split::valid_id(id))
                        .ok_or("split: --from needs a checkpoint ID")?
                        .to_owned(),
                );
            }
            Some(label) if open && !label.starts_with('-') => branches.push(BranchSpec {
                label: label.into(),
                shell: None,
                argv: None,
            }),
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
    if label.is_some() && !open {
        return Err("split: --label is for an --open split".into());
    }
    Ok(SplitArguments {
        branches,
        no_input: no_input || open,
        detach,
        open,
        from,
        label,
    })
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
    let parsed = match parse_split(arguments) {
        Ok(parsed) => parsed,
        Err(message) => return usage(&message),
    };
    let mut spool = Vec::new();
    let detach = parsed.detach;
    if !parsed.no_input && !detach && !std::io::stdin().is_terminal() {
        let mut stdin = std::io::stdin().lock().take(SPOOL_LIMIT as u64 + 1);
        if stdin.read_to_end(&mut spool).is_err() || spool.len() > SPOOL_LIMIT {
            return usage("split: input exceeds the 64 MiB spool");
        }
    }
    let channel = match channel(true) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("split: {message}")),
    };
    let status = create(&channel, parsed, &spool);
    // A detached host split's session ends with the split, in the daemon.
    if !detach && let Some(session) = &channel.detach {
        let _ = channel.client.request(PublicRequest::DetachShell {
            session_id: session.clone(),
        });
    }
    status
}

fn create(channel: &Channel, parsed: SplitArguments, spool: &[u8]) -> i32 {
    let detach = parsed.detach;
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
        branches: parsed.branches,
        environment,
        start_env,
        detach,
        open: parsed.open,
        from: parsed.from,
        label: parsed.label,
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
        Ok(PublicReply::SplitStarted { id }) if detach => {
            // The split runs on in the daemon; `marsh join` waits for it.
            println!("{{\"marsh_split\":1,\"id\":\"{id}\"}}");
            return 0;
        }
        Ok(PublicReply::SplitStarted { .. }) => {}
        Ok(PublicReply::SplitOpened { id, lanes }) => {
            println!(
                "{}",
                serde_json::json!({"marsh_split": 1, "id": id, "lanes": lanes})
            );
            return 0;
        }
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
            if let Some(signal) = signals.forever().next() {
                let name = match signal {
                    SIGTERM => "TERM",
                    SIGHUP => "HUP",
                    _ => "INT",
                };
                let _ = write_frame(
                    &mut writer,
                    &AttachmentFrame::Signal {
                        signal: name.into(),
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
            open: false,
            from: None,
            label: None,
            detach: false,
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
                id,
                cancelled: true,
                message,
                ..
            } => {
                // Ctrl-C needs no words; any other cause does.
                if let Some(message) = message
                    && !message.starts_with("cancelled: interrupted")
                {
                    eprintln!("marsh: split {id}: {message}");
                }
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

/// Long daemon operations: a checkpoint of a large tree, or a close that
/// waits 30 s for lane jobs and then captures every lane.
const LONG_REPLY: std::time::Duration = std::time::Duration::from_mins(30);

/// The project a host command acts on: the working directory (the daemon
/// refuses one inside `.marsh`). In the shell VM the daemon uses the session's.
fn project() -> Option<std::path::PathBuf> {
    std::env::current_dir().ok()
}

fn failed(verb: &str, reply: Result<PublicReply, marsh_daemon::DaemonError>) -> i32 {
    match reply {
        Ok(PublicReply::Error { code, message }) => {
            eprintln!("marsh: {message}");
            match code {
                marsh_daemon::ErrorCode::NoSpace => 75,
                marsh_daemon::ErrorCode::NotFound => 1,
                _ => 2,
            }
        }
        Ok(reply) => usage(&format!("{verb}: unexpected reply {reply:?}")),
        Err(error) => usage(&format!("{verb}: {error}")),
    }
}

fn close(arguments: &[OsString], channel: impl FnOnce(bool) -> Result<Channel, String>) -> i32 {
    let words = arguments
        .iter()
        .map(|a| text(a).unwrap_or(""))
        .collect::<Vec<_>>();
    let (id, json) = match words.as_slice() {
        [id] => (*id, false),
        [id, "--json"] | ["--json", id] => (*id, true),
        _ => return usage("usage: marsh split close ID [--json]"),
    };
    let channel = match channel(false) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("split close: {message}")),
    };
    match channel
        .client
        .request_with_timeout(PublicRequest::SplitClose { split: id.into() }, LONG_REPLY)
    {
        Ok(PublicReply::Document { document }) if json => {
            println!("{document}");
            i32::from(document["status"].as_i64() != Some(0))
        }
        Ok(PublicReply::Document { document }) => {
            for lane in document["branches"].as_array().into_iter().flatten() {
                let files = lane["files"].as_u64().and_then(|n| usize::try_from(n).ok());
                println!(
                    "{}  {}  {}",
                    lane["label"].as_str().unwrap_or(""),
                    lane["status"].as_str().unwrap_or(""),
                    marsh_daemon::split::plural(
                        files.unwrap_or(0),
                        "file changed",
                        "files changed"
                    )
                );
            }
            println!("{}", document["out"].as_str().unwrap_or(""));
            i32::from(document["status"].as_i64() != Some(0))
        }
        reply => failed("split close", reply),
    }
}

/// The script `split apply --json` runs in the shell VM.
const APPLY_SCRIPT: &str = include_str!("split_apply.sh");

/// What the apply script found in the tree and the index.
#[derive(Debug, Eq, PartialEq)]
struct ApplyOutcome {
    result: String,
    message: String,
    conflicts: Vec<String>,
}

/// Parse the script's NUL-separated reply (`R` RESULT `M` MESSAGE, then `U`
/// and the unmerged paths).
fn parse_apply_reply(bytes: &[u8]) -> Option<ApplyOutcome> {
    let mut words = bytes
        .split(|byte| *byte == 0)
        .map(|word| String::from_utf8_lossy(word).into_owned());
    let mut outcome = ApplyOutcome {
        result: String::new(),
        message: String::new(),
        conflicts: Vec::new(),
    };
    while let Some(tag) = words.next() {
        match tag.as_str() {
            "R" => outcome.result = words.next()?,
            "M" => outcome.message = words.next()?,
            "U" => {
                outcome.conflicts = words.filter(|path| !path.is_empty()).collect();
                break;
            }
            "" => {}
            _ => return None,
        }
    }
    matches!(
        outcome.result.as_str(),
        "clean" | "conflicts" | "failed" | "unknown"
    )
    .then_some(outcome)
}

/// The paths a patch changes, from `git apply --numstat -z`: records are
/// `ADDED TAB DELETED TAB PATH NUL`, or, for a rename or copy,
/// `ADDED TAB DELETED TAB NUL SOURCE NUL DESTINATION NUL`.
fn parse_numstat(bytes: &[u8]) -> Vec<String> {
    let mut fields = bytes.split(|byte| *byte == 0);
    let mut paths: Vec<String> = Vec::new();
    let mut add = |bytes: &[u8]| {
        let path = String::from_utf8_lossy(bytes).into_owned();
        if !path.is_empty() && !paths.contains(&path) {
            paths.push(path);
        }
    };
    while let Some(record) = fields.next() {
        let mut parts = record.splitn(3, |byte| *byte == b'\t');
        let (Some(_), Some(_), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if path.is_empty() {
            for _ in 0..2 {
                if let Some(path) = fields.next() {
                    add(path);
                }
            }
        } else {
            add(path);
        }
    }
    paths
}

/// `split apply --json` prints this one object.
fn apply_document(result: &str, files: &[String], conflicts: &[String], message: &str) -> String {
    serde_json::json!({
        "result": result,
        "files": files,
        "conflicts": conflicts,
        "message": message,
    })
    .to_string()
}

/// `split apply ID LABEL [--3way] [--json]`: `git apply` the lane's patch to
/// the project inside the shell VM, with the split's objects for 3-way
/// merges. With `--json` a patch that does not apply cleanly is retried with
/// `--3way`, and the outcome is read from the tree and the index.
#[allow(clippy::too_many_lines)] // One join: open, apply, release.
fn apply(arguments: &[OsString], channel: impl FnOnce(bool) -> Result<Channel, String>) -> i32 {
    let words = arguments
        .iter()
        .map(|a| text(a).unwrap_or(""))
        .collect::<Vec<_>>();
    let three_way = words.contains(&"--3way");
    let json = words.contains(&"--json");
    let (id, label) = match words
        .iter()
        .filter(|w| !matches!(**w, "--3way" | "--json"))
        .collect::<Vec<_>>()[..]
    {
        [id, label] => (*id, *label),
        _ => return usage("usage: marsh split apply ID LABEL [--3way] [--json]"),
    };
    // Every refusal is also a `failed` document for `--json`.
    let refuse = |message: &str, status: i32| {
        if json {
            println!("{}", apply_document("failed", &[], &[], message));
        } else {
            eprintln!("marsh: split apply: {message}");
        }
        status
    };
    let channel = match channel(false) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("split apply: {message}")),
    };
    // Applying is a join that keeps the split: while it runs the split is
    // `joined`, so neither `splits rm` nor the kept-split collector removes
    // the patch under it; the release keeps it (with fresh retention).
    let mut stream = match channel
        .client
        .open_request(PublicRequest::SplitJoin { split: id.into() })
    {
        Ok(stream) => stream,
        Err(error) => return refuse(&error.to_string(), 1),
    };
    let (manifest, out, objects) = match read_frame::<PublicReply>(&mut stream) {
        Ok(PublicReply::SplitJoined {
            manifest,
            dir,
            objects,
            ..
        }) => (manifest, dir, objects),
        Ok(PublicReply::Error { message, .. }) => return refuse(&message, 1),
        Ok(reply) => return refuse(&format!("unexpected reply {reply:?}"), 1),
        Err(error) => return refuse(&error.to_string(), 1),
    };
    let release = |stream: &mut std::os::unix::net::UnixStream, status: i32| {
        let _ = write_frame(
            stream,
            &SplitRelease {
                status,
                keep: true,
                consumer: Some("split apply".into()),
            },
        );
        let _ = read_frame::<PublicReply>(stream);
        status
    };
    let split = serde_json::from_str::<serde_json::Value>(&manifest).unwrap_or_default();
    let lane = split["branches"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|branch| branch["label"] == label);
    if lane.is_none_or(|lane| lane["state"] != "captured") || split["root"].as_str().is_none() {
        let status = refuse(&format!("{id} has no captured lane {label}"), 2);
        return release(&mut stream, status);
    }
    let root = Path::new(split["root"].as_str().unwrap_or_default());
    let patch = out.join(label).join("diff.patch");
    if !patch.is_file() {
        if json {
            println!(
                "{}",
                apply_document("clean", &[], &[], &format!("lane {label} changed nothing"))
            );
        } else {
            eprintln!("marsh: split apply: lane {label} changed nothing");
        }
        return release(&mut stream, 0);
    }
    let objects = objects
        .as_deref()
        .map(Path::as_os_str)
        .unwrap_or_default()
        .to_owned();
    // In the shell VM, the session shell is already there; from the host,
    // `marsh -c` runs the same script in it.
    let vm = |script: &str, mode: Option<&str>| {
        let mut command = if std::env::var_os("MARSH_DAEMON_SOCKET").is_some() {
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(script);
            Some(command)
        } else {
            std::env::current_exe().ok().map(|marsh| {
                let mut command = std::process::Command::new(marsh);
                command.arg("-c").arg(script);
                command
            })
        }?;
        command
            .arg("apply")
            .arg(root)
            .arg(&objects)
            .arg(&patch)
            .current_dir(root)
            .stdin(std::process::Stdio::null());
        if let Some(mode) = mode {
            command.arg(mode);
        }
        Some(command)
    };
    if !json {
        let (script, mode) = if three_way {
            (APPLY_SCRIPT, Some("raw3way"))
        } else {
            (
                "cd -- \"$1\" && GIT_ALTERNATE_OBJECT_DIRECTORIES=\"$2\" git apply -- \"$3\"",
                None,
            )
        };
        let Some(mut command) = vm(script, mode) else {
            return usage("split apply: cannot find the marsh executable");
        };
        let status = match command.status() {
            Ok(status) => status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            Err(error) => usage(&format!("split apply: {error}")),
        };
        return release(&mut stream, status);
    }
    // What the patch changes (read-only), then the apply, which git alone
    // does. A reply that cannot be read after the apply began is `unknown`.
    let files = vm(APPLY_SCRIPT, Some("numstat"))
        .and_then(|mut command| command.stderr(std::process::Stdio::inherit()).output().ok())
        .filter(|output| output.status.success())
        .map(|output| parse_numstat(&output.stdout));
    let Some(files) = files else {
        println!(
            "{}",
            apply_document(
                "failed",
                &[],
                &[],
                "the patch could not be read in the shell VM; nothing was applied"
            )
        );
        return release(&mut stream, 1);
    };
    let outcome = vm(APPLY_SCRIPT, Some("json"))
        .and_then(|mut command| command.stderr(std::process::Stdio::inherit()).output().ok())
        .and_then(|output| parse_apply_reply(&output.stdout));
    let status = if let Some(outcome) = outcome {
        // `files` is what the patch changes (for `unknown`, what it may have
        // changed); `failed` means git wrote nothing.
        let files = if outcome.result == "failed" {
            Vec::new()
        } else {
            files
        };
        println!(
            "{}",
            apply_document(
                &outcome.result,
                &files,
                &outcome.conflicts,
                &outcome.message
            )
        );
        i32::from(outcome.result != "clean")
    } else {
        println!(
            "{}",
            apply_document(
                "unknown",
                &files,
                &[],
                "the shell VM did not report the outcome; check the project with git status"
            )
        );
        1
    };
    release(&mut stream, status)
}

fn checkpoint(
    arguments: &[OsString],
    channel: impl FnOnce(bool) -> Result<Channel, String>,
) -> i32 {
    let (mut json, mut label, mut index) = (false, None, 0);
    while index < arguments.len() {
        match text(&arguments[index]) {
            Some("--json") => json = true,
            Some("--label") if index + 1 < arguments.len() => {
                index += 1;
                label = Some(arguments[index].to_string_lossy().into_owned());
            }
            _ => return usage("usage: marsh checkpoint [--label TEXT] [--json]"),
        }
        index += 1;
    }
    let channel = match channel(false) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("checkpoint: {message}")),
    };
    match channel.client.request_with_timeout(
        PublicRequest::Checkpoint {
            root: project(),
            action: marsh_daemon::CheckpointAction::Create { label },
        },
        LONG_REPLY,
    ) {
        Ok(PublicReply::Document { document }) if json => {
            println!("{document}");
            0
        }
        Ok(PublicReply::Document { document }) => {
            println!("{}", document["checkpoint"].as_str().unwrap_or(""));
            0
        }
        reply => failed("checkpoint", reply),
    }
}

fn checkpoints(
    arguments: &[OsString],
    channel: impl FnOnce(bool) -> Result<Channel, String>,
) -> i32 {
    let words = arguments
        .iter()
        .map(|a| text(a).unwrap_or(""))
        .collect::<Vec<_>>();
    let (action, json) = if let ["rm", id] = words.as_slice() {
        (
            marsh_daemon::CheckpointAction::Remove { id: (*id).into() },
            false,
        )
    } else {
        let (mut json, mut label, mut rest) = (false, None, words.iter());
        while let Some(word) = rest.next() {
            match (*word, rest.clone().next()) {
                ("--json", _) => json = true,
                ("--label", Some(value)) => {
                    label = Some((*value).to_owned());
                    rest.next();
                }
                _ => return usage("usage: marsh checkpoints [--json] [--label TEXT] | rm ID"),
            }
        }
        (marsh_daemon::CheckpointAction::List { label }, json)
    };
    let channel = match channel(false) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("checkpoints: {message}")),
    };
    match channel.client.request(PublicRequest::Checkpoint {
        root: project(),
        action,
    }) {
        Ok(PublicReply::Document { document }) if json => {
            println!("{document}");
            0
        }
        Ok(PublicReply::Document { document }) => {
            for checkpoint in document["checkpoints"].as_array().into_iter().flatten() {
                println!(
                    "{}  {}  {}",
                    checkpoint["checkpoint"].as_str().unwrap_or(""),
                    checkpoint["created_unix_ms"],
                    checkpoint["label"].as_str().unwrap_or("-"),
                );
            }
            0
        }
        Ok(PublicReply::SplitDone { message }) => {
            eprintln!("marsh: {message}");
            0
        }
        reply => failed("checkpoints", reply),
    }
}

fn events(arguments: &[OsString], channel: impl FnOnce(bool) -> Result<Channel, String>) -> i32 {
    if arguments.iter().any(|a| text(a) != Some("--json")) {
        return usage("usage: marsh events --json");
    }
    let channel = match channel(false) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("events: {message}")),
    };
    let mut stream = match channel.client.open_request(PublicRequest::Events) {
        Ok(stream) => stream,
        Err(error) => return usage(&format!("events: {error}")),
    };
    let mut stdout = std::io::stdout().lock();
    loop {
        match read_frame::<PublicReply>(&mut stream) {
            Ok(PublicReply::Event { line }) => {
                if writeln!(stdout, "{line}")
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    return 0;
                }
            }
            Ok(PublicReply::Error { message, .. }) => return usage(&format!("events: {message}")),
            Ok(reply) => return usage(&format!("events: unexpected reply {reply:?}")),
            Err(_) => {
                eprintln!("marsh: events: the daemon ended the stream");
                return 1;
            }
        }
    }
}

fn storage(arguments: &[OsString], channel: impl FnOnce(bool) -> Result<Channel, String>) -> i32 {
    let words = arguments
        .iter()
        .map(|a| text(a).unwrap_or(""))
        .collect::<Vec<_>>();
    if words
        .iter()
        .any(|word| !matches!(*word, "--json" | "--all"))
    {
        return usage("usage: marsh storage [--json] [--all]");
    }
    let channel = match channel(false) {
        Ok(channel) => channel,
        Err(message) => return usage(&format!("storage: {message}")),
    };
    let root = if words.contains(&"--all") {
        None
    } else {
        project()
    };
    match channel
        .client
        .request_with_timeout(PublicRequest::Storage { root }, LONG_REPLY)
    {
        Ok(PublicReply::Document { document }) if words.contains(&"--json") => {
            println!("{document}");
            0
        }
        Ok(PublicReply::Document { document }) => {
            let mib = |value: &serde_json::Value| value.as_u64().unwrap_or(0) >> 20;
            for project in document["projects"].as_array().into_iter().flatten() {
                println!(
                    "{}: {} checkpoints ({} MiB), {} kept splits ({} MiB); {} MiB free",
                    project["root"].as_str().unwrap_or(""),
                    project["checkpoints"]["count"],
                    mib(&project["checkpoints"]["bytes"]),
                    project["kept_splits"]["count"],
                    mib(&project["kept_splits"]["bytes"]),
                    mib(&project["volume"]["free_bytes"]),
                );
            }
            0
        }
        reply => failed("storage", reply),
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;

    #[test]
    fn numstat_records_give_every_path_raw() {
        let bytes = b"1\t1\ta.txt\0-\t-\tbin\0 0\t0\t\0old name\0new\nname\0".to_vec();
        assert_eq!(
            parse_numstat(&bytes),
            ["a.txt", "bin", "old name", "new\nname"]
        );
    }

    #[test]
    fn a_reply_is_a_result_a_message_and_the_unmerged_paths() {
        let reply = b"R\0conflicts\0M\0merged\0U\0a b\0c\nd\0";
        assert_eq!(
            parse_apply_reply(reply),
            Some(ApplyOutcome {
                result: "conflicts".into(),
                message: "merged".into(),
                conflicts: vec!["a b".into(), "c\nd".into()],
            })
        );
        assert_eq!(parse_apply_reply(b"R\0done\0"), None);
        assert_eq!(parse_apply_reply(b""), None);
    }
}
