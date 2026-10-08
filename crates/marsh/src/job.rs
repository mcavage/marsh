//! Job mode of the static artifact (`docs/design/processes.md` s4-s5): the
//! per-name links in `/run/marsh/bin` and the in-container `marsh` CLI. Each
//! starts from `/run/marsh/job.json` alone and reaches the daemon only
//! through the job's `cap.sock`. A job's `bash` and `sh` are the image's own.

use marsh_contracts::process::{
    self as contract, CONTEXT, JOB_JSON, JobDocument, LinkDecision, SOCKET,
};
use marsh_daemon::{DaemonError, ExecuteSpec, PublicReply, PublicRequest, SessionSpec};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};

pub const JOB_HELP: &str = "marsh (in a job) - start child jobs and splits from any process

Usage:
  marsh run [--spawn NAME,...|--no-spawn] NAME [ARG...]   Run NAME as a child job
  marsh split [-n] [::: LABEL CMD [ARG...]]...           Private forks, child jobs
  marsh join [--json] [--keep] [-- CMD [ARG...]]
  marsh fanout [-n] [::: LABEL CMD [ARG...]]... | marsh collect [--json] [--timing]
                                                           Concurrent, same files
  marsh splits [--json] [ID]
  marsh jobs [--json] | jobs --tree [--json] | jobs show JOB [--json]   This job's subtree
  marsh context                                            Print this job's context

Registered names in /run/marsh/bin are links: calling one starts a child job.
See /run/marsh/context.md.
";

pub const RUN_HELP: &str = "Usage: marsh run [--spawn NAME,...|--no-spawn] NAME [ARG...]\n\nRun registered command NAME as a new job (from a job: a child job). stdin,\nstdout, stderr, and the exit status are relayed; a refusal exits 125.\n--spawn and --no-spawn (or MARSH_SPAWN) narrow what NAME may start;\nnothing widens it.\n";

/// The artifact's entry: a shebang link, or (invoked directly) Brush.
#[must_use]
pub fn main() -> i32 {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    if let Some(name) = arguments
        .get(1)
        .and_then(|argument| argument.to_str())
        .and_then(|argument| argument.strip_prefix("--link="))
    {
        // `#!/run/marsh/marsh --link=NAME` gives [artifact, --link=NAME, stub, args...]:
        // the mode comes from the kernel and the stub, never from argv[0].
        let rest = arguments.get(3..).unwrap_or_default().to_vec();
        return match name {
            "marsh" => cli(&rest),
            name => link(name, &rest),
        };
    }
    if let Some(verb) = arguments.get(1).and_then(|verb| verb.to_str())
        && matches!(
            verb,
            "split" | "join" | "splits" | "run" | "jobs" | "context" | "fanout" | "collect"
        )
        && Path::new(JOB_JSON).exists()
    {
        return cli(&arguments[1..]);
    }
    brush_shell::entry::enable_marsh_extensions();
    brush_shell::entry::run();
    0
}

fn document() -> Option<JobDocument> {
    serde_json::from_slice(&std::fs::read(JOB_JSON).ok()?).ok()
}

/// The canonical path of an executable regular file.
fn executable(path: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    std::fs::metadata(&real)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .then_some(real)
}

/// A link for `name` (s5): the entry case and the image-tool case exec in
/// this container; everything else is a child job.
fn link(name: &str, arguments: &[OsString]) -> i32 {
    let document = document().unwrap_or_default();
    let path = std::env::var("PATH").unwrap_or_else(|_| document.path.clone());
    let marker = std::env::var_os("MARSH_ENTRY").is_some_and(|value| value == "1");
    let parent_is_init = nix::unistd::getppid().as_raw() == 1;
    match contract::link_decision(
        name,
        &document.name,
        marker,
        parent_is_init,
        &path,
        executable,
    ) {
        LinkDecision::Entry(binary) => {
            // PATH stays intact so the agent's later registered names still
            // route through their links; only the marker is consumed.
            let error = std::process::Command::new(&binary)
                .arg0(name)
                .args(arguments)
                .env_remove("MARSH_ENTRY")
                .exec();
            eprintln!("marsh: {}: {error}", binary.display());
            127
        }
        LinkDecision::ImageTool(binary) => {
            let error = std::process::Command::new(&binary)
                .arg0(name)
                .args(arguments)
                .exec();
            eprintln!("marsh: {}: {error}", binary.display());
            127
        }
        LinkDecision::Child => child(name, arguments, None, true),
    }
}

/// What this job offers a child (s6): the variables it received from its
/// parent, plus those it set or changed. The daemon drops an offered
/// variable that still matches its salted starting digest (unchanged image
/// `ENV`) and applies the split filter; Docker-set names are never offered.
pub(crate) fn offered(
    document: &JobDocument,
) -> (
    marsh_contracts::ExportedEnvironment,
    std::collections::BTreeMap<String, String>,
) {
    let variables = std::env::vars_os().filter_map(|(name, value)| {
        let name = name.into_string().ok()?;
        marsh_daemon::split::forwardable(&name).then(|| (name, value.as_bytes().to_vec()))
    });
    let (variables, mut start) = contract::job_offer(document, variables);
    let environment = crate::registered_commands::collect_exported_environment(
        variables.into_iter().map(|(name, value)| {
            (
                OsString::from(name),
                std::ffi::OsStr::from_bytes(&value).to_owned(),
            )
        }),
    )
    .0;
    start.retain(|name, _| environment.contains_key(name));
    (environment, start)
}

/// Start `name` as a child job over `cap.sock` and relay it.
fn child(name: &str, arguments: &[OsString], spawn: Option<Vec<String>>, from_link: bool) -> i32 {
    let document = document().unwrap_or_default();
    let refused = if from_link { 126 } else { 125 };
    if !Path::new(SOCKET).exists() {
        eprintln!("marsh: spawn refused: {name} not in this job's spawn set (MARSH_SPAWN)");
        return refused;
    }
    let Ok(cwd) = std::env::current_dir() else {
        eprintln!("marsh: cannot determine the working directory");
        return 125;
    };
    let spawn = spawn.or_else(|| {
        std::env::var("MARSH_SPAWN")
            .ok()
            .map(|value| contract::parse_spawn(&value))
    });
    let (environment, start_env) = offered(&document);
    let spec = ExecuteSpec {
        command: name.to_owned(),
        arguments: arguments
            .iter()
            .map(|argument| argument.as_bytes().to_vec())
            .collect(),
        placement: marsh_daemon::Placement::Local,
        environment,
        working_directory: Some(cwd),
        // The daemon replaces this with the job's own session; only the
        // terminal flag is read (a TTY child is refused).
        session: placeholder_session(
            std::io::stdin().is_terminal() || std::io::stdout().is_terminal(),
        ),
        process: Some(marsh_daemon::process::ProcessLink {
            spawn,
            start_env,
            ..marsh_daemon::process::ProcessLink::default()
        }),
    };
    let client = marsh_daemon::Client::capability(Path::new(SOCKET));
    // The connection cap is transient: a refused connection read nothing
    // and started nothing, so it is retried briefly.
    let mut result = Err(DaemonError::Refused(String::new()));
    for attempt in 0..30 {
        result = client.run_process_with_io(
            spec.clone(),
            std::io::stdin(),
            std::io::stdout(),
            std::io::stderr(),
        );
        match &result {
            // A refused connection may also surface as a broken pipe: the
            // refusal closed the socket before the request was fully
            // written, so the daemon never read it and nothing started.
            // (Loss after acceptance is `JobUncertain`, never retried.)
            Err(error)
                if attempt < 29
                    && (remote_message(error).contains("too many concurrent daemon requests")
                        || matches!(error, DaemonError::Io(io)
                            if io.kind() == std::io::ErrorKind::BrokenPipe)) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            _ => break,
        }
    }
    match result {
        Ok(code) => code,
        Err(error) => {
            let message = remote_message(&error);
            eprintln!("marsh: {message}");
            if from_link && message.contains("spawn refused") {
                refused
            } else {
                125
            }
        }
    }
}

fn remote_message(error: &DaemonError) -> String {
    let message = match error {
        DaemonError::Remote(message) | DaemonError::Refused(message) => message.clone(),
        other => other.to_string(),
    };
    message
        .strip_prefix("marsh: ")
        .map_or(message.clone(), str::to_owned)
}

fn placeholder_session(terminal: bool) -> SessionSpec {
    SessionSpec {
        session_id: String::new(),
        username: String::new(),
        uid: 0,
        gid: 0,
        launch_directory: PathBuf::from("/"),
        guest_home: PathBuf::from("/"),
        home_backing: PathBuf::from("/"),
        ephemeral_home: false,
        terminal,
        terminal_size: None,
    }
}

/// `marsh` inside a job.
fn cli(arguments: &[OsString]) -> i32 {
    let verb = arguments
        .first()
        .and_then(|verb| verb.to_str())
        .unwrap_or("--help");
    let rest = arguments.get(1..).unwrap_or_default();
    match verb {
        "-h" | "--help" | "help" => {
            print!("{JOB_HELP}");
            0
        }
        "context" => {
            print!(
                "{}",
                std::fs::read_to_string(Path::new(contract::CAPABILITY_DIR).join("context.md"))
                    .unwrap_or_else(|_| CONTEXT.to_owned())
            );
            0
        }
        "run" => match parse_run(rest) {
            Ok(None) => {
                print!("{RUN_HELP}");
                0
            }
            Ok(Some((spawn, name, arguments))) => child(&name, &arguments, spawn, false),
            Err(message) => {
                eprintln!("marsh: {message}");
                2
            }
        },
        "split" | "join" | "splits" => {
            if let Some(help) = crate::split_cli::help(verb, rest) {
                print!("{help}");
                return 0;
            }
            crate::split_cli::run(verb, rest, |_| {
                Ok(crate::split_cli::Channel {
                    client: marsh_daemon::Client::capability(Path::new(SOCKET)),
                    session: Some(placeholder_session(false)),
                    detach: None,
                })
            })
        }
        "jobs" => jobs(&marsh_daemon::Client::capability(Path::new(SOCKET)), rest),
        "fanout" => crate::fanout_cli::fanout(rest, crate::fanout_cli::Place::Job),
        "collect" => crate::fanout_cli::collect(rest),
        other => {
            eprintln!("marsh: `{other}` is not available inside a job; see `marsh --help`");
            2
        }
    }
}

/// `run [--spawn a,b | --no-spawn] NAME ARGS`; `None` for `--help`.
///
/// # Errors
/// Returns a usage message.
#[allow(clippy::type_complexity)]
pub fn parse_run(
    arguments: &[OsString],
) -> Result<Option<(Option<Vec<String>>, String, Vec<OsString>)>, String> {
    let mut spawn = None;
    let mut index = 0;
    while let Some(argument) = arguments.get(index).and_then(|argument| argument.to_str()) {
        match argument {
            "-h" | "--help" => return Ok(None),
            "--no-spawn" => spawn = Some(Vec::new()),
            "--spawn" => {
                index += 1;
                let value = arguments
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or("run: --spawn needs NAME[,NAME...]")?;
                spawn = Some(contract::parse_spawn(value));
            }
            value if value.starts_with("--spawn=") => {
                spawn = Some(contract::parse_spawn(&value["--spawn=".len()..]));
            }
            _ => break,
        }
        index += 1;
    }
    let name = arguments
        .get(index)
        .and_then(|name| name.to_str())
        .filter(|name| !name.starts_with('-'))
        .ok_or("run: usage: marsh run [--spawn NAME,...|--no-spawn] NAME [ARG...]")?;
    Ok(Some((
        spawn,
        name.to_owned(),
        arguments[index + 1..].to_vec(),
    )))
}

/// Which job trees `marsh jobs` text shows (`docs/design/processes.md` s9).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobScope<'a> {
    /// Inside a job: the daemon already sent only this job's subtree.
    Subtree,
    /// Inside a session: trees whose root this session started.
    Session(&'a str),
    /// From a host terminal: running trees, trees started in the last hour,
    /// and always the five newest trees.
    Recent,
    /// `--all`: every recorded tree.
    All,
}

/// Whether `marsh jobs ARGS` is a text listing this module renders (the
/// JSON forms and `jobs show` keep their documents).
#[must_use]
pub fn is_listing(arguments: &[OsString]) -> bool {
    let words = || arguments.iter().map(|argument| argument.to_str());
    words().any(|word| word == Some("--tree")) || words().all(|word| word == Some("--all"))
}

/// `jobs [--all]`, `jobs --json`, `jobs --tree [--all] [--json]`,
/// `jobs show JOB [--json]`.
#[must_use]
pub fn jobs(client: &marsh_daemon::Client, arguments: &[OsString]) -> i32 {
    jobs_in(client, arguments, JobScope::Subtree)
}

/// [`jobs`] with the text listings limited to `scope` unless `--all`.
#[must_use]
pub fn jobs_in(client: &marsh_daemon::Client, arguments: &[OsString], scope: JobScope<'_>) -> i32 {
    let words = arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let json = words.iter().any(|word| word == "--json");
    let tree = words.iter().any(|word| word == "--tree");
    let scope = if words.iter().any(|word| word == "--all") && scope != JobScope::Subtree {
        JobScope::All
    } else {
        scope
    };
    let result = if tree || (!json && words.iter().all(|word| word == "--all")) {
        client
            .request(PublicRequest::ProcessShow)
            .and_then(|reply| match reply {
                PublicReply::ProcessTree { document } => Ok(if json {
                    format!("{document}\n")
                } else {
                    render_jobs(&document, scope, tree, now_unix_ms())
                }),
                other => Err(unexpected(&other)),
            })
    } else if words.first().is_some_and(|word| word == "show") {
        let Some(job) = words.get(1).filter(|word| *word != "--json") else {
            eprintln!("marsh: usage: marsh jobs show JOB [--json]");
            return 2;
        };
        client.job(job.clone()).map(|receipt| {
            let value = serde_json::to_value(&receipt).unwrap_or_default();
            if json {
                format!("{value}\n")
            } else {
                serde_json::to_string_pretty(&value).unwrap_or_default() + "\n"
            }
        })
    } else if json {
        client
            .jobs()
            .map(|document| serde_json::to_string(&document).unwrap_or_default() + "\n")
    } else {
        eprintln!("marsh: usage: {JOBS_USAGE}");
        return 2;
    };
    match result {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(error) => {
            eprintln!("marsh: {}", remote_message(&error));
            1
        }
    }
}

/// The `marsh jobs` grammar, for usage errors.
pub const JOBS_USAGE: &str = "marsh jobs [--all] [--json] | marsh jobs --tree [--all] [--json] | marsh jobs show JOB [--json]";

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |now| i64::try_from(now.as_millis()).unwrap_or(i64::MAX))
}

fn unexpected(reply: &PublicReply) -> DaemonError {
    match reply {
        PublicReply::Error { message, .. } => DaemonError::Remote(message.clone()),
        other => DaemonError::InvalidState(format!("unexpected daemon reply: {other:?}")),
    }
}

/// Characters of `command args...` a listing line shows before `…`.
const COMMAND_WIDTH: usize = 64;
/// How far back a host-terminal listing looks (`JobScope::Recent`).
const RECENT_MS: i64 = 60 * 60 * 1000;
/// The newest trees a host-terminal listing always shows.
const RECENT_FALLBACK: usize = 5;

/// `jobs` / `jobs --tree` text (`docs/design/processes.md` s9): a header, then one
/// line per job: short id, start (relative), state, exit, duration, and the
/// command with its arguments. Trees with a running job come first, then
/// newest first; `--tree` draws children under their parent in the command
/// column, the flat form adds a `PARENT` column instead.
#[must_use]
pub fn render_jobs(
    document: &serde_json::Value,
    scope: JobScope<'_>,
    tree: bool,
    now: i64,
) -> String {
    use serde_json::Value;
    let roots = document
        .get("jobs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut roots = roots.iter().collect::<Vec<_>>();
    // Active trees first, then newest first.
    roots.sort_by_key(|root| {
        (
            !active(root),
            std::cmp::Reverse(number(root, "created_unix_ms")),
        )
    });
    let total = roots.len();
    match scope {
        JobScope::Subtree | JobScope::All => {}
        // A daemon older than `session_id` in tree nodes: nothing to scope by.
        JobScope::Session(id) => {
            roots.retain(|root| root.get("session_id").is_none() || text(root, "session_id") == id);
        }
        JobScope::Recent => {
            let mut newest = roots
                .iter()
                .filter_map(|root| number(root, "created_unix_ms"))
                .collect::<Vec<_>>();
            newest.sort_unstable_by(|a, b| b.cmp(a));
            let floor = newest.get(RECENT_FALLBACK - 1).copied().unwrap_or(i64::MIN);
            roots.retain(|root| {
                let created = number(root, "created_unix_ms");
                active(root) || created.is_some_and(|at| now - at <= RECENT_MS || at >= floor)
            });
        }
    }
    let hidden = total - roots.len();
    let mut out = String::new();
    if roots.is_empty() {
        let _ = writeln!(
            out,
            "no jobs {}",
            match scope {
                JobScope::Session(_) => "in this session",
                _ => "recorded",
            }
        );
    } else {
        let mut rows = Vec::new();
        for root in &roots {
            collect_rows(root, tree, "", "", None, &mut rows);
        }
        if !tree {
            rows.sort_by_key(|row| (!row.active, std::cmp::Reverse(row.created)));
        }
        let _ = writeln!(
            out,
            "{:<8}  {:<8}  {:<9}  {:>4}  {:>7}  {}COMMAND",
            "ID",
            "STARTED",
            "STATE",
            "EXIT",
            "RUN",
            if tree { "" } else { "PARENT    " }
        );
        for row in rows {
            let _ = writeln!(out, "{}", row_line(&row, tree, now));
        }
    }
    if hidden > 0 {
        let what = if hidden == 1 { "tree" } else { "trees" };
        let _ = writeln!(
            out,
            "({hidden} older job {what} not shown; marsh jobs{} --all lists every job)",
            if tree { " --tree" } else { "" }
        );
    }
    out
}

/// One listing line (`render_jobs`): a job, a split, or a split branch.
fn row_line(row: &Row<'_>, tree: bool, now: i64) -> String {
    let job = row.node;
    let created = number(job, "created_unix_ms");
    let started = created.map_or_else(|| "-".to_owned(), |at| ago(now - at));
    let exit = number(job, "exit_code")
        .or_else(|| number(job, "status"))
        .map_or_else(|| "-".to_owned(), |code| code.to_string());
    let elapsed = created.map_or_else(
        || "-".to_owned(),
        |at| duration(number(job, "finished_unix_ms").unwrap_or(now) - at),
    );
    let parent = if tree {
        String::new()
    } else {
        format!("{:<8}  ", row.parent.map_or("-", short))
    };
    let (id, state, command) = match text(job, "node") {
        "fanout" => (
            text(job, "fanout_id"),
            text(job, "state"),
            group_line("fanout", job),
        ),
        "split" => (
            text(job, "split_id"),
            split_state(text(job, "state")),
            split_line(job),
        ),
        "branch" => match row.merged {
            // An argv branch is its job: one line, `label: command`.
            Some(merged) => (
                text(merged, "job_id"),
                text(merged, "state"),
                labelled(text(job, "label"), &command_line(merged)),
            ),
            None => (
                "-",
                branch_state(text(job, "state")),
                labelled(text(job, "label"), &printable(text(job, "command"))),
            ),
        },
        _ => (text(job, "job_id"), text(job, "state"), command_line(job)),
    };
    let row_node = row.merged.unwrap_or(job);
    let (exit, elapsed, started) = match row.merged {
        Some(merged) => {
            let created = number(merged, "created_unix_ms");
            (
                number(merged, "exit_code").map_or_else(|| "-".to_owned(), |code| code.to_string()),
                created.map_or_else(
                    || "-".to_owned(),
                    |at| duration(number(merged, "finished_unix_ms").unwrap_or(now) - at),
                ),
                created.map_or_else(|| "-".to_owned(), |at| ago(now - at)),
            )
        }
        None => (exit, elapsed, started),
    };
    let mut line = format!(
        "{:<8}  {started:<8}  {state:<9}  {exit:>4}  {elapsed:>7}  {parent}{}{command}",
        short(id),
        row.prefix,
    );
    if let Some(split) = row_node
        .get("lineage")
        .and_then(|lineage| lineage.get("consumes"))
        .and_then(serde_json::Value::as_str)
    {
        let _ = write!(line, "  (consumes split {})", short(split));
    }
    if text(row_node, "cleanup") == "uncertain" {
        let _ = write!(
            line,
            "  cleanup uncertain (run marsh workers reset {})",
            text(row_node, "command")
        );
    }
    line.trim_end().to_owned()
}

/// `split (fix, review)`: a split row's command column.
fn split_line(node: &serde_json::Value) -> String {
    group_line("split", node)
}

/// `KIND (label, label)`: a split or fanout row's command column.
fn group_line(kind: &str, node: &serde_json::Value) -> String {
    let labels = children(node)
        .iter()
        .map(|branch| text(branch, "label"))
        .collect::<Vec<_>>();
    cut(format!("{kind} ({})", labels.join(", ")))
}

/// `label: command`, cut like a command.
fn labelled(label: &str, command: &str) -> String {
    cut(format!("{label}: {command}"))
}

fn cut(line: String) -> String {
    if line.chars().count() > COMMAND_WIDTH {
        line.chars().take(COMMAND_WIDTH - 1).collect::<String>() + "…"
    } else {
        line
    }
}

/// A branch's shell string on one line that still parses: the multi-line
/// source (Brush's rendering of the branch body) is joined with `; ` where
/// a line ended a command and with a space after an opener or operator, so
/// `{\n  wc -l;\n  echo done\n}< f` shows as `{ wc -l; echo done; }< f`.
/// Remaining control characters show as spaces.
fn printable(command: &str) -> String {
    let mut line = String::new();
    let mut case_depth = 0usize;
    let mut previous: Option<String> = None;
    for raw in command.lines() {
        let next = raw.trim();
        if next.is_empty() {
            continue;
        }
        if let Some(prev) = &previous {
            let opener = ["{", "(", ";", "&", "|"]
                .iter()
                .any(|end| prev.ends_with(end))
                || [" do", " then", " else", " in"]
                    .iter()
                    .any(|end| prev.ends_with(end))
                || matches!(prev.as_str(), "do" | "then" | "else");
            let case_pattern = case_depth > 0 && prev.ends_with(')');
            line.push_str(if opener || case_pattern || next.starts_with(';') {
                " "
            } else {
                "; "
            });
        }
        if next.starts_with("case ") && next.ends_with(" in") {
            case_depth += 1;
        } else if next == "esac" || next.starts_with("esac ") || next.starts_with("esac;") {
            case_depth = case_depth.saturating_sub(1);
        }
        line.push_str(next);
        previous = Some(next.to_owned());
    }
    line.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// A split's state in job words (`docs/design/processes.md` s9).
fn split_state(state: &str) -> &str {
    match state {
        "run" => "running",
        "await" => "awaiting",
        "" => "-",
        other => other,
    }
}

/// A branch's state in job words.
fn branch_state(state: &str) -> &str {
    match state {
        "pending" => "queued",
        "captured" => "finished",
        "uncertain" => "unknown",
        "" => "-",
        other => other,
    }
}

struct Row<'a> {
    node: &'a serde_json::Value,
    /// An argv branch's own job, drawn on the branch's line.
    merged: Option<&'a serde_json::Value>,
    prefix: String,
    parent: Option<&'a str>,
    active: bool,
    created: Option<i64>,
}

fn children(node: &serde_json::Value) -> &[serde_json::Value] {
    node.get("children")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn collect_rows<'a>(
    node: &'a serde_json::Value,
    tree: bool,
    prefix: &str,
    continuation: &str,
    parent: Option<&'a str>,
    rows: &mut Vec<Row<'a>>,
) {
    let kind = text(node, "node");
    // An argv branch with its job: the job's children hang under the branch.
    let merged = (kind == "branch")
        .then(|| {
            let job = text(node, "job_id");
            children(node)
                .iter()
                .find(|child| !job.is_empty() && text(child, "job_id") == job)
        })
        .flatten();
    // The flat form lists jobs only; split and branch rows are tree rows.
    if tree || kind == "job" || kind.is_empty() || merged.is_some() {
        rows.push(Row {
            node,
            merged,
            prefix: if tree {
                prefix.to_owned()
            } else {
                String::new()
            },
            parent,
            active: active(node),
            created: number(merged.unwrap_or(node), "created_unix_ms"),
        });
    }
    let mut kids = children(node).iter().collect::<Vec<_>>();
    if let Some(merged) = merged {
        kids.retain(|child| !std::ptr::eq(*child, merged));
        kids.splice(0..0, children(merged).iter());
    }
    let own = match kind {
        "fanout" => node.get("fanout_id").and_then(serde_json::Value::as_str),
        "split" | "branch" => node
            .get("split_id")
            .or_else(|| node.get("fanout_id"))
            .and_then(serde_json::Value::as_str),
        _ => node.get("job_id").and_then(serde_json::Value::as_str),
    };
    let own = merged
        .and_then(|merged| merged.get("job_id").and_then(serde_json::Value::as_str))
        .or(own);
    for (index, child) in kids.iter().enumerate() {
        let last = index + 1 == kids.len();
        collect_rows(
            child,
            tree,
            &format!("{continuation}{}", if last { "└─ " } else { "├─ " }),
            &format!("{continuation}{}", if last { "   " } else { "│  " }),
            Some(own.unwrap_or("")),
            rows,
        );
    }
}

fn text<'a>(node: &'a serde_json::Value, name: &str) -> &'a str {
    node.get(name)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

fn number(node: &serde_json::Value, name: &str) -> Option<i64> {
    node.get(name).and_then(serde_json::Value::as_i64)
}

/// A job, split, or branch, or anything under it, is still running.
fn active(node: &serde_json::Value) -> bool {
    matches!(
        text(node, "state"),
        "queued" | "running" | "run" | "pending"
    ) || node
        .get("children")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|children| children.iter().any(active))
}

/// The first eight characters of a job id (`jobs show` accepts the prefix).
fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// `12s ago`, `4m ago`, `3h ago`, `2d ago`.
fn ago(ms: i64) -> String {
    let seconds = ms.max(0) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// `0.4s`, `59.9s`, `2m13s`, `1h05m`.
fn duration(ms: i64) -> String {
    let ms = ms.max(0);
    let seconds = ms / 1000;
    match seconds {
        0..60 => {
            #[allow(clippy::cast_precision_loss)] // A display duration.
            let seconds = ms as f64 / 1000.0;
            format!("{seconds:.1}s")
        }
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60),
    }
}

/// `command arg...`, each argument quoted as a shell would need, cut to
/// `COMMAND_WIDTH` characters with `…`.
fn command_line(node: &serde_json::Value) -> String {
    let mut line = text(node, "command").to_owned();
    for argument in node
        .get("args")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
    {
        line.push(' ');
        line.push_str(&quote(argument));
    }
    if line.chars().count() > COMMAND_WIDTH {
        line = line.chars().take(COMMAND_WIDTH - 1).collect::<String>() + "…";
    }
    line
}

fn quote(argument: &str) -> String {
    let argument = argument
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect::<String>();
    let plain = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !argument.is_empty() && argument.chars().all(plain) {
        argument
    } else if !argument.contains(['"', '\\', '$', '`', '!']) {
        format!("\"{argument}\"")
    } else {
        format!("'{}'", argument.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::{JobScope, render_jobs};

    const NOW: i64 = 10_000_000_000;

    fn node(
        id: &str,
        session: &str,
        command: &str,
        args: &[&str],
        state: &str,
        ago_ms: i64,
        children: &[serde_json::Value],
    ) -> serde_json::Value {
        let finished = (state != "running").then_some(NOW - ago_ms + 1500);
        serde_json::json!({
            "job_id": format!("{id}-0000-4000-8000-000000000000"), "session_id": session,
            "command": command, "args": args, "state": state, "cleanup": "verified",
            "exit_code": finished.map(|_| 0), "created_unix_ms": NOW - ago_ms,
            "finished_unix_ms": finished, "children": children,
        })
    }

    #[test]
    fn listing_is_scoped_running_first_and_draws_children() {
        let prompt = "use your Bash tool to run: codex exec 'say hi'; then report";
        let document = serde_json::json!({"jobs": [
            node("aaaaaaaa", "s1", "claude", &["-p", prompt], "finished", 120_000, &[
                node("bbbbbbbb", "s1", "codex", &["exec", "say hi"], "finished", 60_000, &[]),
                node("cccccccc", "s1", "shell", &[], "finished", 30_000, &[]),
            ]),
            node("dddddddd", "s1", "fixture", &["hold", "600"], "running", 3_000, &[]),
            node("eeeeeeee", "s0", "codex", &[], "finished", 7_200_000, &[]),
        ]});
        let tree = render_jobs(&document, JobScope::Session("s1"), true, NOW);
        let lines = tree.lines().collect::<Vec<_>>();
        assert!(lines[0].starts_with("ID        STARTED   STATE"), "{tree}");
        assert!(
            lines[1].starts_with("dddddddd  3s ago    running"),
            "{tree}"
        );
        assert!(
            lines[2].starts_with(
                "aaaaaaaa  2m ago    finished      0     1.5s  claude -p \"use your Bash tool"
            ),
            "{tree}"
        );
        assert!(
            lines[2].ends_with('…') && lines[2].chars().count() <= 110,
            "{tree}"
        );
        assert!(
            lines[3].ends_with("├─ codex exec \"say hi\"")
                && lines[3].starts_with("bbbbbbbb  1m ago"),
            "{tree}"
        );
        assert!(lines[4].ends_with("└─ shell"), "{tree}");
        assert!(
            lines[5].starts_with("(1 older job tree not shown; marsh jobs --tree --all"),
            "{tree}"
        );
        assert!(!tree.contains("eeeeeeee"), "{tree}");
        // `--all`, and the flat form with a PARENT column.
        let flat = render_jobs(&document, JobScope::All, false, NOW);
        assert!(
            flat.contains("eeeeeeee") && !flat.contains("not shown"),
            "{flat}"
        );
        assert!(flat.lines().any(|line| line.starts_with("bbbbbbbb") && line.contains("  aaaaaaaa  codex exec")), "{flat}");
        // A host terminal: running trees, the last hour, and the five newest.
        let day = 86_400_000;
        let mut roots = (1..=7)
            .map(|n| {
                node(
                    &format!("{n}0000000"),
                    "s0",
                    "shell",
                    &[],
                    "finished",
                    n * day,
                    &[],
                )
            })
            .collect::<Vec<_>>();
        roots.push(node(
            "ffffffff",
            "s0",
            "fixture",
            &[],
            "running",
            9 * day,
            &[],
        ));
        roots.push(node(
            "99999999",
            "s0",
            "fixture",
            &[],
            "finished",
            1_000,
            &[],
        ));
        let recent = render_jobs(
            &serde_json::json!({ "jobs": roots }),
            JobScope::Recent,
            false,
            NOW,
        );
        let ids = recent
            .lines()
            .skip(1)
            .map(|line| &line[..8])
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "ffffffff", "99999999", "10000000", "20000000", "30000000", "40000000", "(3 older"
            ],
            "{recent}"
        );
        assert_eq!(
            render_jobs(&document, JobScope::Session("s9"), true, NOW)
                .lines()
                .next(),
            Some("no jobs in this session")
        );
    }

    #[test]
    fn compound_branch_bodies_render_on_one_parseable_line() {
        use super::printable;
        assert_eq!(
            printable("{ \n    wc -l;\n    echo done\n}< notes.txt"),
            "{ wc -l; echo done; }< notes.txt"
        );
        assert_eq!(
            printable("if true; then\n    echo a\nelse\n    echo b\nfi"),
            "if true; then echo a; else echo b; fi"
        );
        assert_eq!(
            printable("case $x in\na)\n    echo a\n;;\nesac"),
            "case $x in a) echo a ;; esac"
        );
        assert_eq!(printable("a &&\n  b |\n  c"), "a && b | c");
        assert_eq!(printable("tr a-z A-Z < f"), "tr a-z A-Z < f");
    }

    #[test]
    fn fanout_jobs_hang_under_one_fanout_node() {
        let job = node(
            "aaaaaaaa",
            "s1",
            "fixture",
            &["identity"],
            "finished",
            5_000,
            &[],
        );
        let document = serde_json::json!({"jobs": [{
            "node": "fanout", "fanout_id": "0123456789abcdef", "session_id": "s1",
            "state": "finished", "status": 0, "created_unix_ms": NOW - 5_000,
            "finished_unix_ms": NOW - 3_500,
            "children": [{"node": "branch", "fanout_id": "0123456789abcdef", "label": "id",
                          "kind": "argv", "job_id": job["job_id"], "state": "finished",
                          "children": [job]}],
        }]});
        let tree = render_jobs(&document, JobScope::Session("s1"), true, NOW);
        let lines = tree.lines().collect::<Vec<_>>();
        assert!(
            lines[1].starts_with("01234567  5s ago    finished      0     1.5s  fanout (id)"),
            "{tree}"
        );
        assert!(
            lines[2].starts_with("aaaaaaaa") && lines[2].ends_with("└─ id: fixture identity"),
            "{tree}"
        );
        assert_eq!(lines.len(), 3, "{tree}");
    }

    #[test]
    #[allow(clippy::too_many_lines)] // One forest, its text and flat forms.
    fn splits_draw_their_branches_and_everything_under_them() {
        let codex = node(
            "cccccccc",
            "branch-s",
            "codex",
            &["exec", "fix"],
            "finished",
            50_000,
            &[],
        );
        let fix_job = node(
            "aaaaaaaa",
            "branch-s",
            "claude",
            &["-p", "find and fix"],
            "finished",
            60_000,
            &[codex],
        );
        let nested = node(
            "dddddddd",
            "s1",
            "claude",
            &["-p", "review"],
            "finished",
            40_000,
            &[],
        );
        let review_job = node(
            "bbbbbbbb",
            "s1",
            "codex",
            &["exec", "suggest"],
            "finished",
            60_000,
            &[nested],
        );
        let mut consumer = node(
            "eeeeeeee",
            "s1",
            "claude",
            &["-p", "apply"],
            "finished",
            10_000,
            &[],
        );
        consumer["lineage"] =
            serde_json::json!({"parent": "session:s1", "consumes": "1a2b3c4d5e6f"});
        let split = serde_json::json!({
            "node": "split", "split_id": "1a2b3c4d5e6f", "session_id": "s1", "state": "joined",
            "status": 0, "created_unix_ms": NOW - 70_000, "finished_unix_ms": NOW - 9_000,
            "children": [
                {"node": "branch", "split_id": "1a2b3c4d5e6f", "label": "fix", "kind": "shell",
                 "command": "claude -p 'find and fix'", "state": "captured", "exit_code": 0,
                 "created_unix_ms": NOW - 61_000, "finished_unix_ms": NOW - 10_000,
                 "children": [fix_job]},
                {"node": "branch", "split_id": "1a2b3c4d5e6f", "label": "review", "kind": "argv",
                 "command": "codex exec suggest", "state": "captured", "exit_code": 0,
                 "job_id": review_job["job_id"], "children": [review_job]},
            ],
        });
        let document = serde_json::json!({"jobs": [consumer, split]});
        let tree = render_jobs(&document, JobScope::Session("s1"), true, NOW);
        let lines = tree.lines().collect::<Vec<_>>();
        assert!(
            lines[1].starts_with("eeeeeeee") && lines[1].ends_with("(consumes split 1a2b3c4d)"),
            "{tree}"
        );
        assert!(
            lines[2]
                .starts_with("1a2b3c4d  1m ago    joined        0    1m01s  split (fix, review)"),
            "{tree}"
        );
        assert!(
            lines[3].starts_with(
                "-         1m ago    finished      0    51.0s  ├─ fix: claude -p 'find and fix'"
            ),
            "{tree}"
        );
        assert!(
            lines[4].starts_with("aaaaaaaa")
                && lines[4].ends_with("│  └─ claude -p \"find and fix\""),
            "{tree}"
        );
        assert!(
            lines[5].starts_with("cccccccc") && lines[5].ends_with("│     └─ codex exec fix"),
            "{tree}"
        );
        assert!(
            lines[6].starts_with("bbbbbbbb") && lines[6].ends_with("└─ review: codex exec suggest"),
            "{tree}"
        );
        assert!(
            lines[7].starts_with("dddddddd") && lines[7].ends_with("   └─ claude -p review"),
            "{tree}"
        );
        assert_eq!(lines.len(), 8, "{tree}");
        // The flat form lists the jobs; a branch's job names its split.
        let flat = render_jobs(&document, JobScope::Session("s1"), false, NOW);
        assert!(
            flat.lines()
                .any(|line| line.starts_with("aaaaaaaa") && line.contains("  1a2b3c4d  claude")),
            "{flat}"
        );
        assert!(!flat.contains("split ("), "{flat}");
    }
}
