//! Product-facing daemon client contract.

use crate::{
    cli::LoadSelection,
    registered_commands::{Invocation, RegisteredCommandExecutor},
    session::SessionConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt,
    io::{IsTerminal as _, Read as _, Write},
    os::unix::net::UnixStream,
    os::unix::{fs::MetadataExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoadReport {
    /// Kits whose VM actually had to start during this request.
    pub cold_kits: Vec<String>,
    /// Exact sandbox selected for each prepared command.
    pub sandboxes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkerResetReport {
    pub kits: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecuteRequest {
    pub command: String,
    pub args: Vec<Vec<u8>>,
    pub placement: marsh_daemon::Placement,
    pub environment: marsh_contracts::ExportedEnvironment,
    /// Invocation cwd, never the project mount or session identity.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "marsh_contracts::byte_path::optional"
    )]
    pub working_directory: Option<PathBuf>,
    pub session: SessionConfig,
    /// `MARSH_SPAWN` narrowing read at invocation (`docs/design/processes.md` s6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientError(pub String);

impl fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ClientError {}

/// The wire (`snake_case`) name of a serde enum value, for user-facing output.
pub fn serde_label<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(label)) => label,
        Ok(other) => other.to_string(),
        Err(_) => "unknown".to_owned(),
    }
}

/// ACP admission errors retain whether retry-with-key advice is appropriate.
#[derive(Debug)]
pub enum AcpPromptError {
    Rejected(ClientError),
    Uncertain(ClientError),
}

impl AcpPromptError {
    fn into_inner(self) -> ClientError {
        match self {
            Self::Rejected(error) | Self::Uncertain(error) => error,
        }
    }
}

/// Synchronous shell-side contract. Implementations relay inherited streams
/// and terminal state while `execute` is blocked.
#[allow(clippy::missing_errors_doc)]
pub trait DaemonClient: Send + Sync + 'static {
    fn attach_shell(&self, process_id: u32, session: &SessionConfig)
    -> Result<String, ClientError>;
    fn detach_shell(&self, session_id: &str) -> Result<(), ClientError>;
    /// Launch the Linux project shell and relay its terminal until exit.
    /// The server owns post-open cleanup/detach, including asynchronous cleanup
    /// after transport loss. The caller must not clear that retained authority.
    fn open_shell(
        &self,
        session: &SessionConfig,
        brush_args: &[OsString],
    ) -> Result<i32, ClientError>;
    fn registered_commands(&self) -> Result<Vec<String>, ClientError>;
    fn install_kit(&self, _command: &str, _reference: &str) -> Result<String, ClientError> {
        Err(ClientError("Kit installation is unavailable".into()))
    }
    fn registered_kits(&self) -> Result<BTreeMap<String, String>, ClientError> {
        Err(ClientError("Kit lookup is unavailable".into()))
    }
    fn prepare(
        &self,
        selection: &LoadSelection,
        session: &SessionConfig,
    ) -> Result<LoadReport, ClientError>;
    fn reset_workers(&self, selection: &LoadSelection) -> Result<WorkerResetReport, ClientError>;
    fn reset_scope(&self) -> Result<marsh_daemon::ScopeLifecycleReport, ClientError> {
        Err(ClientError("reset is unavailable".into()))
    }
    fn stop_scope(&self) -> Result<marsh_daemon::ScopeLifecycleReport, ClientError> {
        Err(ClientError("stop is unavailable".into()))
    }
    fn status_json(&self) -> Result<Value, ClientError>;
    fn process_view(
        &self,
        _session: &SessionConfig,
    ) -> Result<marsh_daemon::ProcessViewDocument, ClientError> {
        Err(ClientError("process view is unavailable".into()))
    }
    fn jobs_json(&self) -> Result<Value, ClientError>;
    fn job_json(&self, selector: &str) -> Result<Value, ClientError>;
    fn execute(&self, request: ExecuteRequest) -> Result<i32, ClientError>;
    fn mcp_publish(
        &self,
        _session: &SessionConfig,
        _name: &str,
        _description: Option<&str>,
        _sandbox: Option<&str>,
        _pipeline: &str,
        _kit: Option<&str>,
    ) -> Result<String, ClientError> {
        Err(ClientError("MCP publication is unavailable".into()))
    }
    fn mcp_load(
        &self,
        _session: &SessionConfig,
        _name: &str,
        _kit: Option<&str>,
        _sandbox: Option<&str>,
    ) -> Result<String, ClientError> {
        Err(ClientError("MCP load is unavailable".into()))
    }
    fn mcp_unpublish(&self, _session: &SessionConfig, _name: &str) -> Result<String, ClientError> {
        Err(ClientError("MCP publication is unavailable".into()))
    }
    fn acp_start(
        &self,
        _adapter: &str,
        _session: &SessionConfig,
        _reservation_id: Option<&str>,
    ) -> Result<(String, String, UnixStream), ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_reserve(&self, _adapter: &str, _session: &SessionConfig) -> Result<String, ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_prompt_with_key(
        &self,
        _id: &str,
        _session: &SessionConfig,
        _operation_id: &str,
        _text: String,
    ) -> Result<String, ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_prompt_classified(
        &self,
        id: &str,
        session: &SessionConfig,
        key: &str,
        text: String,
    ) -> Result<String, AcpPromptError> {
        self.acp_prompt_with_key(id, session, key, text)
            .map_err(AcpPromptError::Rejected)
    }
    fn acp_cancel(&self, _id: &str, _session: &SessionConfig) -> Result<String, ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_respond(
        &self,
        _id: &str,
        _session: &SessionConfig,
        _request_id: &str,
        _option_id: &str,
    ) -> Result<(), ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_status(
        &self,
        _id: &str,
        _session: &SessionConfig,
        _after: u64,
    ) -> Result<marsh_daemon::AcpSessionStatus, ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_list(
        &self,
        _session: &SessionConfig,
    ) -> Result<Vec<marsh_daemon::AcpSessionSummary>, ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_stop(&self, _id: &str, _session: &SessionConfig) -> Result<(), ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_attach(&self, _id: &str, _session: &SessionConfig) -> Result<(), ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_release(&self, _id: &str, _session: &SessionConfig) -> Result<(), ClientError> {
        Err(ClientError("ACP is unavailable".into()))
    }
    fn acp_publish(
        &self,
        _id: &str,
        _session: &SessionConfig,
        _name: &str,
        _sandbox: Option<&str>,
        _kit: Option<&str>,
    ) -> Result<String, ClientError> {
        Err(ClientError("ACP publication is unavailable".into()))
    }
    fn acp_unpublish(&self, _session: &SessionConfig, _name: &str) -> Result<String, ClientError> {
        Err(ClientError("ACP publication is unavailable".into()))
    }
}

pub struct CommandExecutor {
    client: Arc<dyn DaemonClient>,
    session: SessionConfig,
}

impl CommandExecutor {
    #[must_use]
    pub fn new(client: Arc<dyn DaemonClient>, session: SessionConfig) -> Self {
        Self { client, session }
    }

    /// Reserve a visible agent session before Brush starts a background job.
    ///
    /// # Errors
    /// Returns a daemon or validation error when reservation cannot be made.
    pub fn reserve_acp(&self, adapter: &str) -> Result<String, ClientError> {
        self.client.acp_reserve(adapter, &self.session)
    }
}

impl RegisteredCommandExecutor for CommandExecutor {
    fn execute(&self, invocation: Invocation) -> i32 {
        if invocation.command == "acp" {
            return self.execute_acp(&invocation.args);
        }
        if invocation.command == "mcp" {
            return self.execute_mcp(&invocation.args);
        }
        if matches!(invocation.command.as_str(), "ps" | "top") {
            return self.execute_process_command(&invocation.command, &invocation.args);
        }
        let request = ExecuteRequest {
            command: invocation.command,
            args: invocation
                .args
                .iter()
                .map(|argument| os_bytes(argument).to_vec())
                .collect(),
            placement: invocation.placement,
            environment: invocation.environment,
            working_directory: Some(invocation.working_directory),
            session: self.session.clone(),
            spawn: std::env::var("MARSH_SPAWN")
                .ok()
                .map(|value| marsh_contracts::process::parse_spawn(&value)),
        };
        match self.client.execute(request) {
            Ok(status) => status,
            Err(error) => {
                eprintln!("marsh: {error}");
                125
            }
        }
    }
}

/// Run the real PATH command before shell or daemon setup for ordinary ps/top.
#[must_use]
pub fn exec_system_process_command(name: &str, args: &[OsString]) -> i32 {
    let Some(candidate) = system_process_command_path(name) else {
        eprintln!("{name}: command not found in PATH");
        return 127;
    };
    let error = Command::new(&candidate).args(args).exec();
    eprintln!("{name}: cannot run {}: {error}", candidate.display());
    126
}

fn system_process_command_path(name: &str) -> Option<PathBuf> {
    let current = std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::metadata(path).ok());
    let search = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
    for directory in std::env::split_paths(&search) {
        let candidate = directory.join(name);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() || metadata.mode() & 0o111 == 0 {
            continue;
        }
        if current.as_ref().is_some_and(|self_file| {
            self_file.dev() == metadata.dev() && self_file.ino() == metadata.ino()
        }) {
            continue;
        }
        return Some(candidate);
    }
    None
}

#[allow(clippy::too_many_lines)] // Keep one compact logical activity table and its optional detail lines together.
fn write_process_view(
    output: &mut impl Write,
    view: &marsh_daemon::ProcessViewDocument,
    refreshing: bool,
    verbose: bool,
) -> std::io::Result<()> {
    writeln!(
        output,
        "marsh activity (CPU and memory unavailable){}",
        if view.truncated {
            " · more activity omitted"
        } else {
            ""
        }
    )?;
    if refreshing {
        writeln!(output, "Updates every 2s · Ctrl-C exits")?;
    }
    writeln!(
        output,
        "{:<36} {:<19} {:<11} {:<7} {:>8}",
        "ID", "NAME", "STATE", "PLACE", "ELAPSED"
    )?;
    for shell in &view.shells {
        writeln!(
            output,
            "{:<36} {:<19} {:<11} {:<7} {:>8}",
            shell.session_id, "shell", "attached", "local", "—"
        )?;
        if verbose {
            writeln!(
                output,
                "          session={} host-pid={}",
                shell.session_id, shell.host_attachment_pid
            )?;
        }
    }
    for start in &view.starting_kits {
        writeln!(
            output,
            "{:<36} {:<19} {:<11} {:<7} {:>8}",
            start.invocation_id,
            start.command,
            "starting",
            placement_name(start.placement),
            elapsed(view.observed_unix_ms, start.started_unix_ms)
        )?;
        if verbose {
            writeln!(
                output,
                "          invocation={} shell={}",
                start.invocation_id, start.shell_session_id
            )?;
        }
    }
    for job in &view.jobs {
        // One ACP session is one workload. Its underlying Kit job is detail,
        // not a second row that asks the user to correlate two UUIDs.
        if view
            .acp_sessions
            .iter()
            .any(|acp| acp.job_id.as_deref() == Some(&job.job_id))
        {
            continue;
        }
        writeln!(
            output,
            "{:<36} {:<19} {:<11} {:<7} {:>8}",
            job.job_id,
            job.command,
            serde_label(&job.state),
            placement_name(job.placement),
            elapsed(view.observed_unix_ms, job.created_unix_ms)
        )?;
        if verbose {
            writeln!(
                output,
                "          job={} shell={} vm={} container={} cleanup={}",
                job.job_id,
                job.shell_session_id,
                job.vm_id.as_deref().unwrap_or("pending"),
                job.container_id.as_deref().unwrap_or("pending"),
                serde_label(&job.cleanup)
            )?;
        }
    }
    for acp in &view.acp_sessions {
        let job = view
            .jobs
            .iter()
            .find(|job| acp.job_id.as_deref() == Some(&job.job_id));
        let state =
            acp.state
                .as_deref()
                .unwrap_or(if acp.turn_active { "working" } else { "ready" });
        writeln!(
            output,
            "{:<36} {:<19} {:<11} {:<7} {:>8}",
            acp.agent_session_id,
            acp.adapter,
            state,
            job.map_or("local", |job| placement_name(job.placement)),
            job.map_or("—".into(), |job| elapsed(
                view.observed_unix_ms,
                job.created_unix_ms
            ))
        )?;
        if verbose {
            writeln!(
                output,
                "          session={} job={}",
                acp.agent_session_id,
                acp.job_id.as_deref().unwrap_or("pending")
            )?;
        }
    }
    if view.starting_kits.is_empty() && view.jobs.is_empty() && view.acp_sessions.is_empty() {
        writeln!(output, "No active Kit commands or ACP agents.")?;
    }
    Ok(())
}

fn placement_name(place: marsh_daemon::Placement) -> &'static str {
    match place {
        marsh_daemon::Placement::Local => "local",
    }
}

fn elapsed(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then) / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m{}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{}m", seconds / 3_600, seconds / 60 % 60)
    }
}

impl CommandExecutor {
    fn execute_process_command(&self, name: &str, args: &[OsString]) -> i32 {
        if args.first().is_none_or(|arg| arg != "--marsh") {
            return exec_system_process_command(name, args);
        }
        let tail = &args[1..];
        if tail == ["--help"] {
            print_process_help(name, false);
            return 0;
        }
        let json = name == "ps" && tail == ["--json"];
        let verbose = tail.contains(&OsString::from("--verbose"));
        let once = name == "top" && tail.contains(&OsString::from("--once"));
        let valid = if name == "ps" {
            tail.is_empty() || json || tail == ["--verbose"]
        } else {
            tail.len() <= 2
                && tail.iter().all(|arg| arg == "--verbose" || arg == "--once")
                && !(tail.len() == 2 && tail[0] == tail[1])
        };
        if !valid {
            print_process_help(name, true);
            return 2;
        }
        let refresh = name == "top" && !once && std::io::stdout().is_terminal();
        loop {
            let view = match self.client.process_view(&self.session) {
                Ok(view) => view,
                Err(error) => {
                    eprintln!("{name}: {error}");
                    return 1;
                }
            };
            let mut output = std::io::stdout().lock();
            if refresh && write!(output, "\x1b[H\x1b[J").is_err() {
                return 1;
            }
            let result = if json {
                serde_json::to_writer(&mut output, &view)
                    .map_err(std::io::Error::other)
                    .and_then(|()| writeln!(output))
            } else {
                write_process_view(&mut output, &view, refresh, verbose)
            };
            if result.and_then(|()| output.flush()).is_err() {
                return 1;
            }
            if !refresh {
                return 0;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn execute_mcp(&self, args: &[OsString]) -> i32 {
        let Some(args) = args
            .iter()
            .map(|arg| arg.to_str())
            .collect::<Option<Vec<_>>>()
        else {
            eprintln!("mcp: arguments must be UTF-8");
            return 2;
        };
        let result = match args.as_slice() {
            [] | ["--help" | "help"] => {
                print_mcp_help(None, false);
                return 0;
            }
            [command, "--help"] | ["help", command] => {
                print_mcp_help(Some(command), false);
                return 0;
            }
            ["unpublish", name] => self.client.mcp_unpublish(&self.session, name),
            ["load", name, "--kit", kit] => {
                self.client.mcp_load(&self.session, name, Some(kit), None)
            }
            ["load", name, "--sandbox", sandbox] => {
                self.client
                    .mcp_load(&self.session, name, None, Some(sandbox))
            }
            ["publish", name, tail @ ..] => {
                let mut description = None;
                let mut sandbox = None;
                let mut kit = None;
                let mut index = 0;
                while index + 1 < tail.len() && tail[index] != "--" {
                    match tail[index] {
                        "--description" if description.is_none() => {
                            description = Some(tail[index + 1]);
                        }
                        "--sandbox" if sandbox.is_none() => sandbox = Some(tail[index + 1]),
                        "--kit" if kit.is_none() => kit = Some(tail[index + 1]),
                        _ => return mcp_usage(),
                    }
                    index += 2;
                }
                if tail.get(index) != Some(&"--")
                    || tail.len() != index + 2
                    || (kit.is_some() && sandbox.is_some())
                {
                    return mcp_usage();
                }
                self.client.mcp_publish(
                    &self.session,
                    name,
                    description,
                    sandbox,
                    tail[index + 1],
                    kit,
                )
            }
            _ => return mcp_usage(),
        };
        match result {
            Ok(message) => {
                println!("{message}");
                0
            }
            Err(error) => {
                eprintln!("mcp: {error}");
                125
            }
        }
    }

    fn execute_acp(&self, args: &[OsString]) -> i32 {
        let Some(args) = args
            .iter()
            .map(|arg| arg.to_str())
            .collect::<Option<Vec<_>>>()
        else {
            eprintln!("acp: arguments must be UTF-8");
            return 2;
        };
        let result = match args.as_slice() {
            [] | ["--help" | "help"] => {
                print_acp_help(None, false);
                return 0;
            }
            [command, "--help"] | ["help", command] => {
                print_acp_help(Some(command), false);
                return 0;
            }
            ["run", adapter] => return self.run_acp(adapter),
            ["reserve", adapter] => self
                .client
                .acp_reserve(adapter, &self.session)
                .map(|id| println!("{id}")),
            ["run", "--reservation", id, adapter] => {
                return self.run_acp_reserved(adapter, Some(id));
            }
            ["list", tail @ ..] => return self.list_acp_options(tail),
            ["status", id] => self.status_acp(id, None, false),
            ["status", id, "--json"] => self.status_acp(id, None, true),
            ["status", id, after, "--json"] => after
                .parse::<u64>()
                .map_err(|_| ClientError("ACP cursor must be a nonnegative integer".into()))
                .and_then(|cursor| self.status_acp(id, Some(cursor), true)),
            ["status", id, after] => after
                .parse::<u64>()
                .map_err(|_| ClientError("ACP cursor must be a nonnegative integer".into()))
                .and_then(|cursor| self.status_acp(id, Some(cursor), false)),
            ["ask", id, text @ ..] if !text.is_empty() => self.ask_acp(id, text),
            ["prompt", "--key", key, id, text @ ..] if !text.is_empty() => {
                self.prompt_acp(id, Some(key), text)
            }
            ["prompt", id, text @ ..] if !text.is_empty() => self.prompt_acp(id, None, text),
            ["cancel", id] => self.client.acp_cancel(id, &self.session).map(|phase| {
                println!("{phase}");
            }),
            ["permissions", id] => self.permissions_acp(id, false),
            ["permissions", id, "--json"] => self.permissions_acp(id, true),
            ["respond", id, request_id, option_id] => {
                self.client
                    .acp_respond(id, &self.session, request_id, option_id)
            }
            ["stop", id] => self
                .client
                .acp_stop(id, &self.session)
                .map(|()| println!("Stopped {id}")),
            ["attach", id] => self
                .client
                .acp_attach(id, &self.session)
                .map(|()| println!("Attached {id}")),
            ["release", id] => self
                .client
                .acp_release(id, &self.session)
                .map(|()| println!("Released {id}")),
            ["publish", id, tail @ ..] => return self.publish_acp_options(id, tail),
            ["unpublish", name] => self
                .client
                .acp_unpublish(&self.session, name)
                .map(|message| println!("{message}")),
            _ => {
                print_acp_help(args.first().copied(), true);
                return 2;
            }
        };
        acp_command_result(result)
    }

    fn list_acp_options(&self, tail: &[&str]) -> i32 {
        let (mut json, mut wait, mut mine, mut target) = (false, false, false, None);
        for arg in tail {
            match *arg {
                "--json" if !json => json = true,
                "--wait" if !wait => wait = true,
                "--mine" if !mine => mine = true,
                id if !id.starts_with('-') && target.is_none() => target = Some(id),
                _ => {
                    print_acp_help(Some("list"), true);
                    return 2;
                }
            }
        }
        if target.is_some() && !wait {
            print_acp_help(Some("list"), true);
            return 2;
        }
        acp_command_result(self.list_acp(json, wait, target, mine))
    }

    fn publish_acp_options(&self, id: &str, tail: &[&str]) -> i32 {
        let (mut name, mut sandbox, mut kit) = (None, None, None);
        for pair in tail.chunks(2) {
            match pair {
                ["--name", value] if name.is_none() => name = Some(*value),
                ["--sandbox", value] if sandbox.is_none() => sandbox = Some(*value),
                ["--kit", value] if kit.is_none() => kit = Some(*value),
                _ => {
                    print_acp_help(Some("publish"), true);
                    return 2;
                }
            }
        }
        let Some(name) = name.filter(|_| kit.is_none() || sandbox.is_none()) else {
            print_acp_help(Some("publish"), true);
            return 2;
        };
        acp_command_result(
            self.client
                .acp_publish(id, &self.session, name, sandbox, kit)
                .map(|message| println!("{message}")),
        )
    }

    fn permissions_acp(&self, id: &str, json: bool) -> Result<(), ClientError> {
        let status = self.client.acp_status(id, &self.session, 0)?;
        if json {
            return print_json(&status.permissions);
        }
        if let Some(note) = status.permission_note {
            println!("{note}");
        }
        if status.permissions.is_empty() {
            println!("No pending one-time permissions.");
        }
        for request in status.permissions {
            println!(
                "{}: {}",
                request.request_id,
                request
                    .title
                    .unwrap_or_else(|| request.tool_call_id.clone())
                    .escape_debug()
            );
            if let Some(input) = request.input_preview {
                println!("  Input: {}", input.escape_debug());
            }
            for option in request.options {
                println!(
                    "  {} ({}) -- acp respond {id} {} '{}'",
                    option.name.escape_debug(),
                    option.kind,
                    request.request_id,
                    option.option_id.replace('\'', "'\\''")
                );
            }
        }
        Ok(())
    }

    fn list_acp(
        &self,
        json: bool,
        wait: bool,
        target: Option<&str>,
        mine: bool,
    ) -> Result<(), ClientError> {
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_mins(4);
        let mut target = target.map(str::to_owned);
        let mut missing_since = None;
        loop {
            let sessions: Vec<_> = self
                .client
                .acp_list(&self.session)?
                .into_iter()
                .filter(|s| {
                    !mine || Some(&s.owner_shell_session_id) == self.session.session_id.as_ref()
                })
                .collect();
            if wait && target.is_none() {
                let mut live = sessions.iter().filter(|s| {
                    !s.terminal && !matches!(s.state.as_deref(), Some("failed" | "stopping"))
                });
                let first = live.next();
                if live.next().is_some() {
                    return Err(ClientError("Multiple live ACP sessions; use `acp list --wait ID` with an exact reservation/session ID. For race-free startup: id=$(acp reserve AGENT); acp run --reservation $id AGENT & acp list --wait $id".into()));
                }
                target = first.map(|s| s.agent_session_id.clone());
            }
            let selected = target.as_ref().and_then(|id| {
                sessions
                    .iter()
                    .find(|session| &session.agent_session_id == id)
            });
            if !wait || selected.is_some_and(|session| session.state.as_deref() != Some("starting"))
            {
                let sessions = if let Some(session) = selected {
                    vec![session.clone()]
                } else {
                    sessions
                };
                let failed = wait
                    && sessions.first().is_some_and(|session| {
                        session.terminal
                            || matches!(session.state.as_deref(), Some("failed" | "stopping"))
                    });
                if json {
                    print_json(&sessions)?;
                } else if sessions.is_empty() {
                    println!("No ACP sessions. Start one with `acp run AGENT &`.");
                } else {
                    for session in sessions {
                        let state = if let Some(state) = session.state.as_deref() {
                            state
                        } else if session.terminal {
                            "exited"
                        } else if session.turn_active {
                            "working"
                        } else {
                            "ready"
                        };
                        println!(
                            "{state:<8} {:<18} {}{}",
                            session.adapter,
                            session.agent_session_id,
                            session
                                .published_name
                                .as_ref()
                                .map_or_else(String::new, |name| format!("  published as {name}"))
                        );
                    }
                }
                if failed {
                    return Err(ClientError(
                        "ACP session failed or ended before becoming ready; inspect the background `acp run` job".into(),
                    ));
                }
                return Ok(());
            }
            if target.is_some() && selected.is_none() {
                let missing = missing_since.get_or_insert_with(std::time::Instant::now);
                if missing.elapsed() >= std::time::Duration::from_secs(1) {
                    return Err(ClientError(
                        "ACP session ID is not visible in this project".into(),
                    ));
                }
            } else {
                missing_since = None;
            }
            if std::time::Instant::now() >= deadline {
                return Err(ClientError("no ACP session became ready within four minutes; inspect the background `acp run` job for startup errors".into()));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    fn status_acp(&self, id: &str, after: Option<u64>, json: bool) -> Result<(), ClientError> {
        let mut status = self
            .client
            .acp_status(id, &self.session, after.unwrap_or(0))?;
        if !json && after.is_none() && status.turn_start_cursor > 0 {
            status = self
                .client
                .acp_status(id, &self.session, status.turn_start_cursor)?;
        }
        if json {
            return print_json(&status);
        }
        let state = if status.attachment.terminal.is_some() {
            "exited"
        } else if status.stopping {
            "stopping"
        } else if status.turn_active {
            "working"
        } else {
            "ready"
        };
        println!("{}  {}  {state}", status.adapter, status.agent_session_id);
        if let Some(name) = &status.published_name {
            println!("Controller: published as {name}");
        }
        if let Some(receipt) = &status.receipt {
            println!(
                "Kit job: {}  {}  cleanup {}",
                receipt.job_id,
                serde_label(&receipt.state),
                serde_label(&receipt.cleanup)
            );
        } else if let Some(job_id) = &status.attachment.job_id {
            println!("Kit job: {job_id}");
        }
        if let Some(reason) = status.last_stop_reason {
            println!("Last turn: {}", serde_label(&reason));
        }
        if let Some(error) = status.last_error {
            println!("Error: {error}");
        }
        if let Some(note) = &status.permission_note {
            println!("{note}");
        }
        if !status.permissions.is_empty() {
            println!(
                "{} permission request(s) pending; run `acp permissions {id}`",
                status.permissions.len()
            );
        }
        if status.updates_lost {
            println!("Some agent updates were lost.");
        }
        if !status.status_omissions.is_empty() {
            println!(
                "Status frame budget omitted optional fields: {}. Permission choice IDs and turn receipts are retained; continue paging updates.",
                status.status_omissions.join(", ")
            );
        }
        if status.out_of_turn_updates > 0 {
            println!(
                "{} out-of-turn updates/invalid frames omitted; they are not part of this turn.",
                status.out_of_turn_updates
            );
        }
        for update in &status.updates {
            if let Some(text) = acp_message_text(&update.update) {
                print!("{text}");
            }
        }
        if status.more_updates {
            println!("\nMore updates: acp status {id} {}", status.next_cursor);
        } else if !status.updates.is_empty() {
            println!();
        }
        Ok(())
    }

    fn ask_acp(&self, id: &str, text: &[&str]) -> Result<(), ClientError> {
        let prompt = acp_prompt_text(text)?;
        let key = Uuid::new_v4().to_string();
        let turn_id = self.client.acp_prompt_classified(id, &self.session, &key, prompt).map_err(|error| {
            if matches!(error, AcpPromptError::Uncertain(_)) {
                eprintln!("acp: prompt outcome may be uncertain. Retry with `acp prompt --key {key} {id} TEXT` using the same text (or - with the same stdin).");
            }
            error.into_inner()
        })?;
        let metadata = self.client.acp_status(id, &self.session, u64::MAX)?;
        let mut cursor = metadata
            .turns
            .get(&turn_id)
            .ok_or_else(|| {
                ClientError("Turn receipt unavailable; check status before retrying".into())
            })?
            .start_cursor;
        let mut lost_updates = false;
        loop {
            let status = self.client.acp_status(id, &self.session, cursor).inspect_err(|_error| {
                eprintln!("acp: answer outcome may be uncertain. Prompt key: {key}. Check `acp status {id}` before retrying with the same key and text.");
            })?;
            for update in &status.updates {
                if update.turn_id.as_deref() == Some(&turn_id)
                    && let Some(text) = acp_message_text(&update.update)
                {
                    print!("{text}");
                }
            }
            std::io::stdout()
                .flush()
                .map_err(|error| ClientError(error.to_string()))?;
            let receipt = status
                .turns
                .get(&turn_id)
                .ok_or_else(|| ClientError("Turn receipt unavailable".into()))?;
            if cursor
                < receipt
                    .retained_after
                    .min(receipt.end_cursor.unwrap_or(status.latest_cursor))
                || receipt.dropped_updates > 0
                || (status.current_turn_id.as_deref() == Some(&turn_id)
                    && status.dropped_updates > 0)
            {
                lost_updates = true;
            }
            cursor = status.next_cursor;
            if status.current_turn_id.as_deref() == Some(&turn_id) && !status.permissions.is_empty()
            {
                return Err(ClientError(format!(
                    "permission needed; run `acp permissions {id}` and `acp respond {id} REQUEST OPTION`"
                )));
            }
            if receipt.end_cursor.is_some_and(|end| cursor >= end) {
                if lost_updates {
                    return Err(ClientError(
                        "agent updates were lost; answer may be incomplete".into(),
                    ));
                }
                if let Some(error) = &receipt.error {
                    return Err(ClientError(error.clone()));
                }
                if let Some(note) = &receipt.permission_note {
                    eprintln!("acp: {note}");
                }
                println!();
                return if receipt.stop_reason == Some(marsh_acp::StopReason::EndTurn) {
                    Ok(())
                } else {
                    Err(ClientError(format!(
                        "agent turn ended: {}",
                        receipt
                            .stop_reason
                            .as_ref()
                            .map_or_else(|| "without a stop reason".to_owned(), serde_label)
                    )))
                };
            }
            if !status.more_updates {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    fn prompt_acp(
        &self,
        id: &str,
        supplied_key: Option<&str>,
        text: &[&str],
    ) -> Result<(), ClientError> {
        let prompt = acp_prompt_text(text)?;
        let key = supplied_key.map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);
        if Uuid::parse_str(&key).map_or(true, |parsed| parsed.to_string() != key) {
            return Err(ClientError(
                "ACP prompt key must be a canonical UUID".into(),
            ));
        }
        eprintln!("acp prompt key: {key}");
        let turn_id = self
            .client
            .acp_prompt_with_key(id, &self.session, &key, prompt)?;
        println!("{turn_id}");
        Ok(())
    }

    fn run_acp(&self, adapter: &str) -> i32 {
        let reservation_id = std::env::var("MARSH_ACP_RESERVATION_ID").ok();
        self.run_acp_reserved(adapter, reservation_id.as_deref())
    }

    fn run_acp_reserved(&self, adapter: &str, reservation_id: Option<&str>) -> i32 {
        let (id, job_id, mut lease) =
            match self
                .client
                .acp_start(adapter, &self.session, reservation_id)
            {
                Ok(started) => started,
                Err(error) => {
                    eprintln!("acp: {error}");
                    return 125;
                }
            };
        eprintln!("acp: session {id} ready; Kit job {job_id}");
        let mut cursor = 0;
        loop {
            match self.client.acp_status(&id, &self.session, cursor) {
                Ok(status) => {
                    cursor = status.updates.last().map_or(cursor, |update| update.cursor);
                    if let Some(receipt) = status.receipt
                        && !matches!(
                            receipt.state,
                            marsh_daemon::JobState::Queued | marsh_daemon::JobState::Running
                        )
                    {
                        return if receipt.cleanup == marsh_daemon::CleanupState::Uncertain {
                            125
                        } else {
                            receipt.exit.and_then(|exit| exit.code).unwrap_or(125)
                        };
                    }
                }
                Err(error) => {
                    eprintln!("acp: {error}");
                    if let Err(stop_error) = self.client.acp_stop(&id, &self.session) {
                        eprintln!("acp: stop after status failure: {stop_error}");
                    }
                    let mut terminal = [0u8; 1];
                    let confirmed = lease
                        .set_read_timeout(Some(std::time::Duration::from_secs(7)))
                        .and_then(|()| lease.read_exact(&mut terminal))
                        .is_ok_and(|()| terminal == *b"T");
                    if !confirmed {
                        eprintln!("acp: Kit cleanup is unconfirmed");
                    }
                    return 125;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

fn print_process_help(name: &str, error: bool) {
    let help = if name == "ps" {
        "Usage: ps --marsh [--verbose|--json]\n\nShow one snapshot of this project's shell, Kit commands, and ACP agents. IDs are shortened for display; --verbose shows full IDs and worker details, and --json emits the complete machine-readable snapshot. Ordinary `ps` runs the system command."
    } else {
        "Usage: top --marsh [--verbose] [--once]\n\nRefresh this project's active shell, Kit commands, and ACP agents every two seconds. Ctrl-C returns to the prompt. --once prints one snapshot; --verbose shows full IDs and worker details. CPU and memory use are unavailable. Ordinary `top` runs the system command."
    };
    if error {
        eprintln!("{help}");
    } else {
        println!("{help}");
    }
}

fn acp_command_result(result: Result<(), ClientError>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("acp: {error}");
            125
        }
    }
}

fn acp_prompt_text(text: &[&str]) -> Result<String, ClientError> {
    const LIMIT: usize = 1_048_576;
    let bytes = if text == ["-"] {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| ClientError(format!("cannot read prompt stdin: {error}")))?;
        bytes
    } else {
        text.join(" ").into_bytes()
    };
    if bytes.len() > LIMIT {
        return Err(ClientError("ACP prompt exceeds 1 MiB UTF-8; shorten it or split it across turns. This prompt was not admitted.".into()));
    }
    String::from_utf8(bytes).map_err(|_| {
        ClientError("ACP prompt stdin must be UTF-8; bytes were not modified or dispatched".into())
    })
}

fn print_acp_help(command: Option<&str>, error: bool) {
    let help = match command {
        None => {
            "ACP agent sessions\n\nStart an agent as a shell background job, then use its session ID to talk to it.\n\n  acp run AGENT &                  Start a registered ACP agent\n  acp list [--mine] [--wait [ID]] [--json] Find sessions; use an exact ID when multiple exist\n  acp ask ID TEXT                 Send a turn and print its answer\n  acp status ID [CURSOR]           Show session state and updates\n  acp stop ID                      End a session\n\nExample:\n  id=$(acp reserve claude-session)\n  acp run --reservation \"$id\" claude-session &\n  acp list --mine --wait \"$id\"\n  acp ask \"$id\" 'Say hi'\n\nOther commands: prompt, permissions, respond, cancel, attach, release, publish, unpublish.\nRun `acp COMMAND --help` for details."
        }
        Some("run") => {
            "Usage: acp run [--reservation ID] AGENT &\n\nStart an ACP agent as a Brush background job. Prints session and Kit job IDs to stderr when ready. For race-free composition, including quoted agent names: id=$(acp reserve AGENT); acp run --reservation \"$id\" AGENT & acp list --mine --wait \"$id\". Reservations expire after 15 seconds if not started."
        }
        Some("list") => {
            "Usage: acp list [--mine] [--wait [ID]] [--json]\n\nShow this project's sessions, newest first. --mine filters to sessions started by this shell. --wait selects the only live session, or requires an exact ID when several are live; terminal history does not make a live session ambiguous. For new background work use `acp reserve AGENT` then `acp run --reservation ID AGENT &` and wait for that ID; a prior ready session is not evidence of new startup. --json includes owner and Kit job identity."
        }
        Some("reserve") => {
            "Usage: acp reserve AGENT\n\nPrint a session reservation ID owned by this shell. Start within 15 seconds using `acp run --reservation ID AGENT &`, then `acp list --mine --wait ID`."
        }
        Some("ask") => {
            "Usage: acp ask ID TEXT | acp ask ID -\n\nSend one prompt and print its answer. A sole - reads exact UTF-8 stdin, including trailing newlines. Limit: 1 MiB UTF-8; both encoded ACP and daemon control frames must fit 1 MiB, including JSON escaping/session envelopes. Uncertain errors include a retry key; check status before retrying. Other words are joined with single spaces."
        }
        Some("status") => {
            "Usage: acp status ID [CURSOR] [--json]\n\nShow state, recent agent updates, and pending permissions. CURSOR is exclusive: pass next_cursor back unchanged. Idle polls do not advance it. Without a cursor, human output shows the latest turn; --json starts at session history cursor 0 and includes raw JSON, turn IDs and bounded receipts."
        }
        Some("prompt") => {
            "Usage: acp prompt [--key UUID] ID TEXT | acp prompt [--key UUID] ID -\n\nSubmit a prompt without waiting. A sole - reads exact UTF-8 stdin up to 1 MiB; both encoded ACP and daemon control frames must also fit 1 MiB including escaping/session envelopes. Reuse --key with identical text/bytes after uncertain submission. Read updates with `acp status ID`."
        }
        Some("permissions") => {
            "Usage: acp permissions ID [--json]\n\nShow requests awaiting one-time approval. Respond with `acp respond ID REQUEST OPTION` using an offered option."
        }
        Some("respond") => {
            "Usage: acp respond ID REQUEST OPTION\n\nAnswer one pending permission request using an option shown by `acp permissions ID`."
        }
        Some("cancel") => {
            "Usage: acp cancel ID\n\nAsk the agent to cancel its current turn. The session remains available."
        }
        Some("stop") => "Usage: acp stop ID\n\nStop the agent session and its Kit job.",
        Some("attach") => {
            "Usage: acp attach ID\n\nTake control of an unowned ACP session from this shell."
        }
        Some("release") => {
            "Usage: acp release ID\n\nRelease this shell's control of an ACP session."
        }
        Some("publish") => {
            "Usage: acp publish ID --name NAME [--kit KIT | --sandbox SANDBOX]\n\nExpose an idle ACP session as an MCP control tool. Options may appear in any order. Loaded clients can prompt/cancel and answer one-time permissions. --kit prepares the exact registered Kit VM only after publication admission and loads there; --sandbox selects an existing sandbox. Start a new client agent session after loading. Unpublish before parent steering."
        }
        Some("unpublish") => "Usage: acp unpublish NAME\n\nRevoke a published ACP control tool.",
        Some(other) => {
            eprintln!("acp: unknown command `{other}`\nRun `acp --help` to see commands.");
            return;
        }
    };
    if error {
        eprintln!("{help}");
    } else {
        println!("{help}");
    }
}

fn mcp_usage() -> i32 {
    print_mcp_help(None, true);
    2
}

fn print_mcp_help(command: Option<&str>, error: bool) {
    let help = match command {
        None => {
            "Publish a fixed shell pipeline as an MCP tool.\n\n  mcp publish NAME [--description TEXT] [--kit KIT | --sandbox SANDBOX] -- 'PIPELINE'\n  mcp load NAME --kit KIT | --sandbox SANDBOX\n  mcp unpublish NAME\n\nLoad reuses an existing publication without changing its generation. Publishing lets clients that load the tool run the pipeline with this project shell's privileges, including sudo and its private Docker Engine, and read its output. `--kit` prepares and loads a registered Kit VM; `--sandbox` loads another named sandbox. Run `mcp COMMAND --help` for details."
        }
        Some("publish") => {
            "Usage: mcp publish NAME [--description TEXT] [--kit KIT | --sandbox SANDBOX] -- 'PIPELINE'\n\nRegister a fixed Brush pipeline as an MCP tool from an attached project shell. Direct host publication is rejected before effects. Loaded clients can run it with this project's shell privileges, including private VM Docker access, and read the output. --kit resolves a registered command such as codex and loads its ready Kit VM; later jobs sharing that VM in this selected-home scope can also use it. --sandbox names an explicit running sandbox. Without either target, it becomes a default: every Kit VM marsh creates from now on (first use, or after `marsh workers reset KIT`) loads it before its first job; Kit VMs already running are not changed (use `mcp load NAME --kit KIT` to load one now). Republishing with a target ends the default; `mcp unpublish NAME` revokes it everywhere. Start a new agent session after loading; an existing session may keep its previous tool list."
        }
        Some("load") => {
            "Usage: mcp load NAME --kit KIT | --sandbox SANDBOX\n\nLoad this project's existing publication without republishing or changing its generation. Existing clients remain valid. --kit prepares the exact registered Kit VM; --sandbox selects a running same-user stock sandbox. Every client and later job there can run the pipeline in its publishing project with its shell privileges and read the output. Start a new agent session in the target sandbox to discover it. Workspace and network authority are unchanged."
        }
        Some("unpublish") => {
            "Usage: mcp unpublish NAME\n\nRevoke this project's published MCP tool. Existing clients lose the grant; unload it from a sandbox's catalog separately if needed."
        }
        Some(other) => {
            eprintln!("mcp: unknown command `{other}`\nRun `mcp --help` to see commands.");
            return;
        }
    };
    if error {
        eprintln!("{help}");
    } else {
        println!("{help}");
    }
}

fn print_json(value: &impl Serialize) -> Result<(), ClientError> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, value)
        .map_err(|error| ClientError(format!("cannot encode ACP reply: {error}")))?;
    output
        .write_all(b"\n")
        .map_err(|error| ClientError(format!("cannot write ACP reply: {error}")))
}

fn acp_message_text(update: &Value) -> Option<&str> {
    (update.get("sessionUpdate")?.as_str()? == "agent_message_chunk")
        .then(|| update.get("content")?.get("text")?.as_str())
        .flatten()
}

#[cfg(unix)]
fn os_bytes(value: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(value: &std::ffi::OsStr) -> &[u8] {
    value.to_str().unwrap_or_default().as_bytes()
}

/// Concrete authenticated client for the same-user daemon.
pub struct LocalDaemonClient(
    marsh_daemon::Client,
    Option<(u64, u64)>,
    Option<marsh_sbx::EphemeralHomeToken>,
);

impl LocalDaemonClient {
    /// Pin host attachment to the publication's directory identity.
    #[must_use]
    pub fn with_expected_project_identity(mut self, identity: Option<(u64, u64)>) -> Self {
        self.1 = identity;
        self
    }

    /// Carry an opaque allocation only on the authenticated host attach route.
    /// It is never copied into guest `SessionSpec` or workload environment.
    #[must_use]
    pub fn with_ephemeral_home_token(
        mut self,
        token: Option<marsh_sbx::EphemeralHomeToken>,
    ) -> Self {
        self.2 = token;
        self
    }

    /// The underlying authenticated daemon client.
    #[must_use]
    pub fn into_daemon(self) -> marsh_daemon::Client {
        self.0
    }

    /// Connect only to an already-running scoped daemon. This never starts one.
    ///
    /// # Errors
    /// Returns an actionable lifecycle or authentication error for unsafe or
    /// inconsistent endpoint state.
    pub fn connect_existing(home: &std::path::Path) -> Result<Option<Self>, ClientError> {
        marsh_daemon::Client::connect_if_running(home)
            .map(|client| client.map(|client| Self(client, None, None)))
            .map_err(map_daemon_error)
    }

    /// Connect to the scoped daemon, starting the installed sibling `marshd`
    /// when no lifecycle artifacts exist.
    ///
    /// # Errors
    /// Returns an actionable lifecycle or authentication error when the
    /// scoped daemon cannot be reached safely.
    pub fn ensure_running(home: &std::path::Path) -> Result<Self, ClientError> {
        let relay_socket = std::env::var_os("MARSH_DAEMON_SOCKET");
        let relay_token = std::env::var_os("MARSH_DAEMON_TOKEN");
        match (relay_socket, relay_token) {
            (Some(socket), Some(token)) => {
                return marsh_daemon::Client::connect_relay(
                    std::path::Path::new(&socket),
                    std::path::Path::new(&token),
                )
                .map(|client| Self(client, None, None))
                .map_err(map_daemon_error);
            }
            (None, None) => {}
            _ => {
                return Err(ClientError(
                    "guest daemon relay requires both MARSH_DAEMON_SOCKET and MARSH_DAEMON_TOKEN"
                        .into(),
                ));
            }
        }
        // Resolve symlinks so a linked `marsh` (Homebrew's bin/) finds the
        // marshd of its own release tree.
        let executable = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(|error| ClientError(format!("cannot locate marsh: {error}")))?;
        let daemon = executable
            .parent()
            .ok_or_else(|| ClientError("marsh has no executable directory".into()))?
            .join("marshd");
        let stock_sbx = resolve_stock_sbx()?;
        marsh_daemon::Client::ensure_running(home, &daemon, &stock_sbx)
            .map(|client| Self(client, None, None))
            .map_err(map_daemon_error)
    }
}

/// The one install instruction for stock SBX. The Homebrew formula's caveats
/// (`packaging/homebrew/marsh.rb.in`) and docs/install.md print the same text.
pub const SBX_INSTALL_HINT: &str =
    "marsh needs Docker Sandboxes: brew install docker/tap/sbx; then `sbx login`";

/// Resolve the configured stock SBX executable and pin its canonical path.
///
/// # Errors
/// Returns an error when `MARSH_SBX` is invalid or `sbx` cannot be found on
/// `PATH`.
pub fn resolve_stock_sbx() -> Result<PathBuf, ClientError> {
    resolve_stock_sbx_from(
        std::env::var_os("MARSH_SBX").as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

fn resolve_stock_sbx_from(
    configured: Option<&OsStr>,
    path: Option<&OsStr>,
) -> Result<PathBuf, ClientError> {
    if let Some(configured) = configured {
        return marsh_daemon::canonical_executable(Path::new(configured)).map_err(|error| {
            let reason = match &error {
                marsh_daemon::DaemonError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                    "not found".to_owned()
                }
                marsh_daemon::DaemonError::Io(io) => io.to_string(),
                marsh_daemon::DaemonError::EndpointInconsistent(_) => {
                    "is not a regular executable file".to_owned()
                }
                other => other.to_string(),
            };
            ClientError(format!(
                "stock sbx {reason} at {} (MARSH_SBX); fix the path, or unset MARSH_SBX. {SBX_INSTALL_HINT}",
                Path::new(configured).display()
            ))
        });
    }
    let path = path
        .ok_or_else(|| ClientError("PATH is required to locate stock sbx; set MARSH_SBX".into()))?;
    let candidate = std::env::split_paths(path)
        .map(|directory| directory.join("sbx"))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            ClientError(format!(
                "stock sbx not found on PATH. {SBX_INSTALL_HINT} (or set MARSH_SBX to its path)"
            ))
        })?;
    marsh_daemon::canonical_executable(&candidate).map_err(map_daemon_error)
}

impl DaemonClient for LocalDaemonClient {
    fn attach_shell(
        &self,
        process_id: u32,
        session: &SessionConfig,
    ) -> Result<String, ClientError> {
        let authority = marsh_daemon::SessionAuthority {
            username: session.username.clone(),
            uid: session.uid,
            gid: session.gid,
            launch_directory: session.launch_directory.clone(),
            guest_home: session.guest_home.clone(),
            home_backing: session.home_backing.clone(),
            ephemeral_home: session.ephemeral_home,
        };
        let request = if let Some(token) = &self.2 {
            marsh_daemon::PublicRequest::AttachEphemeralShell {
                pid: process_id,
                session: authority,
                expected_project_identity: self.1,
                token: token.clone(),
            }
        } else {
            if session.ephemeral_home {
                return Err(ClientError(
                    "ephemeral attachment requires the host-allocated private HOME token".into(),
                ));
            }
            match self.1 {
                Some(expected_project_identity) => marsh_daemon::PublicRequest::AttachPinnedShell {
                    pid: process_id,
                    session: authority,
                    expected_project_identity,
                },
                None => marsh_daemon::PublicRequest::AttachShell {
                    pid: process_id,
                    session: authority,
                },
            }
        };
        match self.0.request(request).map_err(map_daemon_error)? {
            marsh_daemon::PublicReply::ShellAttached { session_id } => Ok(session_id),
            marsh_daemon::PublicReply::Error { message, .. } => Err(ClientError(message)),
            reply => Err(ClientError(format!(
                "daemon returned an unexpected attach response: {reply:?}"
            ))),
        }
    }

    fn detach_shell(&self, session_id: &str) -> Result<(), ClientError> {
        match self
            .0
            .request(marsh_daemon::PublicRequest::DetachShell {
                session_id: session_id.into(),
            })
            .map_err(map_daemon_error)?
        {
            marsh_daemon::PublicReply::Detached => Ok(()),
            marsh_daemon::PublicReply::Error { message, .. } => Err(ClientError(message)),
            reply => Err(ClientError(format!(
                "daemon returned an unexpected detach response: {reply:?}"
            ))),
        }
    }

    fn open_shell(
        &self,
        session: &SessionConfig,
        brush_args: &[OsString],
    ) -> Result<i32, ClientError> {
        self.0
            .open_shell(marsh_daemon::ShellSpec {
                arguments: brush_args
                    .iter()
                    .skip(1)
                    .map(|argument| argument.as_encoded_bytes().to_vec())
                    .collect(),
                session: daemon_session(session)?,
                dev: crate::cli::DEV_SESSION.load(std::sync::atomic::Ordering::Relaxed),
            })
            .map_err(map_daemon_error)
    }

    fn registered_commands(&self) -> Result<Vec<String>, ClientError> {
        self.0.registered_commands().map_err(map_daemon_error)
    }

    fn install_kit(&self, command: &str, reference: &str) -> Result<String, ClientError> {
        self.0
            .install_kit(command, reference)
            .map_err(map_daemon_error)
    }

    fn registered_kits(&self) -> Result<BTreeMap<String, String>, ClientError> {
        self.0.registered_kits().map_err(map_daemon_error)
    }

    fn prepare(
        &self,
        selection: &LoadSelection,
        session: &SessionConfig,
    ) -> Result<LoadReport, ClientError> {
        let selection = match selection {
            LoadSelection::All => marsh_daemon::LoadSelection::All,
            LoadSelection::Kits(kits) => marsh_daemon::LoadSelection::Kits(kits.clone()),
        };
        self.0
            .prepare_with_progress(
                selection,
                daemon_session(session)?,
                crate::cli::DEV_SESSION.load(std::sync::atomic::Ordering::Relaxed),
                std::io::stderr(),
            )
            .map(|result| LoadReport {
                cold_kits: result.cold_kits,
                sandboxes: result.sandboxes,
            })
            .map_err(map_daemon_error)
    }

    fn reset_workers(&self, selection: &LoadSelection) -> Result<WorkerResetReport, ClientError> {
        let selection = match selection {
            LoadSelection::All => marsh_daemon::LoadSelection::All,
            LoadSelection::Kits(kits) => marsh_daemon::LoadSelection::Kits(kits.clone()),
        };
        self.0
            .reset_workers(selection)
            .map(|kits| WorkerResetReport { kits })
            .map_err(map_daemon_error)
    }

    fn reset_scope(&self) -> Result<marsh_daemon::ScopeLifecycleReport, ClientError> {
        self.0.reset_scope().map_err(map_daemon_error)
    }

    fn stop_scope(&self) -> Result<marsh_daemon::ScopeLifecycleReport, ClientError> {
        self.0.stop_scope().map_err(map_daemon_error)
    }

    fn status_json(&self) -> Result<Value, ClientError> {
        serde_json::to_value(self.0.status(None).map_err(map_daemon_error)?)
            .map_err(|error| ClientError(format!("cannot encode daemon status: {error}")))
    }

    fn process_view(
        &self,
        session: &SessionConfig,
    ) -> Result<marsh_daemon::ProcessViewDocument, ClientError> {
        self.0
            .process_view(daemon_session(session)?)
            .map_err(map_daemon_error)
    }

    fn jobs_json(&self) -> Result<Value, ClientError> {
        serde_json::to_value(self.0.jobs().map_err(map_daemon_error)?)
            .map_err(|error| ClientError(format!("cannot encode daemon jobs: {error}")))
    }

    fn job_json(&self, selector: &str) -> Result<Value, ClientError> {
        serde_json::to_value(self.0.job(selector.into()).map_err(map_daemon_error)?)
            .map_err(|error| ClientError(format!("cannot encode daemon job: {error}")))
    }

    fn execute(&self, request: ExecuteRequest) -> Result<i32, ClientError> {
        self.0
            .execute(marsh_daemon::ExecuteSpec {
                command: request.command,
                arguments: request.args,
                placement: request.placement,
                environment: request.environment,
                working_directory: request.working_directory,
                session: daemon_session(&request.session)?,
                process: request
                    .spawn
                    .map(|spawn| marsh_daemon::process::ProcessLink {
                        parent_job: None,
                        spawn: Some(spawn),
                        branch: false,
                        start_env: std::collections::BTreeMap::new(),
                    }),
            })
            .map_err(map_daemon_error)
    }

    fn mcp_publish(
        &self,
        session: &SessionConfig,
        name: &str,
        description: Option<&str>,
        sandbox: Option<&str>,
        pipeline: &str,
        kit: Option<&str>,
    ) -> Result<String, ClientError> {
        self.0
            .mcp_publish_target(
                daemon_session(session)?,
                name.to_owned(),
                description.map(str::to_owned),
                sandbox.map(str::to_owned),
                pipeline.to_owned(),
                kit.map(str::to_owned),
            )
            .map_err(map_daemon_error)
    }

    fn mcp_load(
        &self,
        session: &SessionConfig,
        name: &str,
        kit: Option<&str>,
        sandbox: Option<&str>,
    ) -> Result<String, ClientError> {
        self.0
            .mcp_load(
                daemon_session(session)?,
                name.into(),
                kit.map(str::to_owned),
                sandbox.map(str::to_owned),
            )
            .map_err(map_daemon_error)
    }

    fn mcp_unpublish(&self, session: &SessionConfig, name: &str) -> Result<String, ClientError> {
        self.0
            .mcp_unpublish(daemon_session(session)?, name.to_owned())
            .map_err(map_daemon_error)
    }

    fn acp_start(
        &self,
        adapter: &str,
        session: &SessionConfig,
        reservation_id: Option<&str>,
    ) -> Result<(String, String, UnixStream), ClientError> {
        self.0
            .acp_start_reserved(
                adapter.into(),
                daemon_session(session)?,
                reservation_id.map(str::to_owned),
            )
            .map_err(map_daemon_error)
    }

    fn acp_reserve(&self, adapter: &str, session: &SessionConfig) -> Result<String, ClientError> {
        self.0
            .acp_reserve(adapter.into(), daemon_session(session)?)
            .map_err(map_daemon_error)
    }

    fn acp_prompt_with_key(
        &self,
        id: &str,
        session: &SessionConfig,
        operation_id: &str,
        text: String,
    ) -> Result<String, ClientError> {
        self.acp_prompt_classified(id, session, operation_id, text)
            .map_err(AcpPromptError::into_inner)
    }

    fn acp_prompt_classified(
        &self,
        id: &str,
        session: &SessionConfig,
        key: &str,
        text: String,
    ) -> Result<String, AcpPromptError> {
        let session = daemon_session(session).map_err(AcpPromptError::Rejected)?;
        self.0.acp_prompt_classified(id.into(), session, key.into(), text).map_err(|error| {
            use marsh_daemon::{DaemonError, PromptAdmissionError};
            match error {
                PromptAdmissionError::Rejected(DaemonError::FrameTooLarge(_)) => AcpPromptError::Rejected(ClientError("Encoded prompt plus control/session envelope exceeds 1 MiB; shorten the prompt or split it across turns. This prompt was not admitted.".into())),
                PromptAdmissionError::Rejected(error) => AcpPromptError::Rejected(map_daemon_error(error)),
                PromptAdmissionError::Uncertain(error) => AcpPromptError::Uncertain(map_daemon_error(error)),
            }
        })
    }

    fn acp_cancel(&self, id: &str, session: &SessionConfig) -> Result<String, ClientError> {
        self.0
            .acp_cancel(id.into(), daemon_session(session)?)
            .map_err(map_daemon_error)
    }

    fn acp_respond(
        &self,
        id: &str,
        session: &SessionConfig,
        request_id: &str,
        option_id: &str,
    ) -> Result<(), ClientError> {
        self.0
            .acp_respond(
                id.into(),
                daemon_session(session)?,
                request_id.into(),
                option_id.into(),
            )
            .map_err(map_daemon_error)
    }

    fn acp_status(
        &self,
        id: &str,
        session: &SessionConfig,
        after: u64,
    ) -> Result<marsh_daemon::AcpSessionStatus, ClientError> {
        self.0
            .acp_status(id.into(), daemon_session(session)?, after)
            .map_err(map_daemon_error)
    }

    fn acp_list(
        &self,
        session: &SessionConfig,
    ) -> Result<Vec<marsh_daemon::AcpSessionSummary>, ClientError> {
        self.0
            .acp_list(daemon_session(session)?)
            .map_err(map_daemon_error)
    }

    fn acp_stop(&self, id: &str, session: &SessionConfig) -> Result<(), ClientError> {
        self.0
            .acp_stop(id.into(), daemon_session(session)?)
            .map_err(map_daemon_error)
    }

    fn acp_attach(&self, id: &str, session: &SessionConfig) -> Result<(), ClientError> {
        self.0
            .acp_attach(id.into(), daemon_session(session)?)
            .map_err(map_daemon_error)
    }

    fn acp_release(&self, id: &str, session: &SessionConfig) -> Result<(), ClientError> {
        self.0
            .acp_release(id.into(), daemon_session(session)?)
            .map_err(map_daemon_error)
    }

    fn acp_publish(
        &self,
        id: &str,
        session: &SessionConfig,
        name: &str,
        sandbox: Option<&str>,
        kit: Option<&str>,
    ) -> Result<String, ClientError> {
        self.0
            .acp_publish_target(
                id.into(),
                daemon_session(session)?,
                name.into(),
                sandbox.map(str::to_owned),
                kit.map(str::to_owned),
            )
            .map_err(map_daemon_error)
    }

    fn acp_unpublish(&self, session: &SessionConfig, name: &str) -> Result<String, ClientError> {
        self.0
            .acp_unpublish(daemon_session(session)?, name.into())
            .map_err(map_daemon_error)
    }
}

/// The wire session for an attached `SessionConfig`.
///
/// # Errors
/// Returns an error when the session has not been attached.
pub fn daemon_session(session: &SessionConfig) -> Result<marsh_daemon::SessionSpec, ClientError> {
    use std::io::IsTerminal;
    Ok(marsh_daemon::SessionSpec {
        session_id: session
            .session_id
            .clone()
            .ok_or_else(|| ClientError("shell session has not been attached".into()))?,
        username: session.username.clone(),
        uid: session.uid,
        gid: session.gid,
        launch_directory: session.launch_directory.clone(),
        guest_home: session.guest_home.clone(),
        home_backing: session.home_backing.clone(),
        ephemeral_home: session.ephemeral_home,
        // A pipeline stage can inherit a terminal on stdin while its stdout
        // feeds the next stage. Only a bidirectional terminal gets a PTY.
        // A redirected stderr must remain a distinct stream, including -c
        // invoked from a terminal. A PTY would silently merge it into stdout.
        terminal: std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal()
            && shared_terminal_device(),
        terminal_size: None,
    })
}

#[allow(clippy::needless_pass_by_value)]
// A redirect to a different TTY is still a redirect. Do not merge stderr
// merely because every descriptor passes isatty. A failed observation selects
// separate pipes, not an invented common terminal.
fn shared_terminal_device() -> bool {
    let input = nix::sys::stat::fstat(std::io::stdin());
    let output = nix::sys::stat::fstat(std::io::stdout());
    let error = nix::sys::stat::fstat(std::io::stderr());
    match (input, output, error) {
        (Ok(input), Ok(output), Ok(error)) => {
            let identity =
                |stat: &nix::sys::stat::FileStat| (stat.st_dev, stat.st_ino, stat.st_rdev);
            identity(&input) == identity(&output) && identity(&input) == identity(&error)
        }
        _ => false,
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "Result::map_err transfers the owned transport error to this conversion"
)]
fn map_daemon_error(error: marsh_daemon::DaemonError) -> ClientError {
    ClientError(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    #[derive(Default)]
    struct KitPublishClient {
        published_sandbox: Mutex<Option<String>>,
    }

    impl DaemonClient for KitPublishClient {
        fn attach_shell(&self, _: u32, _: &SessionConfig) -> Result<String, ClientError> {
            unreachable!()
        }
        fn detach_shell(&self, _: &str) -> Result<(), ClientError> {
            unreachable!()
        }
        fn open_shell(&self, _: &SessionConfig, _: &[OsString]) -> Result<i32, ClientError> {
            unreachable!()
        }
        fn registered_commands(&self) -> Result<Vec<String>, ClientError> {
            unreachable!()
        }
        fn registered_kits(&self) -> Result<BTreeMap<String, String>, ClientError> {
            Ok(BTreeMap::from([("codex".into(), "/kits/codex".into())]))
        }
        fn prepare(&self, _: &LoadSelection, _: &SessionConfig) -> Result<LoadReport, ClientError> {
            panic!("publication must not prepare on the guest before authoritative admission")
        }
        fn reset_workers(&self, _: &LoadSelection) -> Result<WorkerResetReport, ClientError> {
            unreachable!()
        }
        fn status_json(&self) -> Result<Value, ClientError> {
            Ok(json!({"workers": [
                {"kit_profile": "/kits/codex@sha256:old", "health": "ready", "warm": true, "vm_id": "old-vm"},
                {"kit_profile": "/kits/codex@sha256:new", "health": "ready", "warm": true, "vm_id": "new-vm"}
            ]}))
        }
        fn jobs_json(&self) -> Result<Value, ClientError> {
            unreachable!()
        }
        fn job_json(&self, _: &str) -> Result<Value, ClientError> {
            unreachable!()
        }
        fn execute(&self, _: ExecuteRequest) -> Result<i32, ClientError> {
            unreachable!()
        }
        fn mcp_publish(
            &self,
            _: &SessionConfig,
            _: &str,
            _: Option<&str>,
            sandbox: Option<&str>,
            _: &str,
            kit: Option<&str>,
        ) -> Result<String, ClientError> {
            assert!(sandbox.is_none());
            *self.published_sandbox.lock().unwrap() = kit.map(str::to_string);
            Ok("published".into())
        }
    }

    #[test]
    fn mcp_publish_kit_defers_preparation_until_daemon_admission() {
        let home = tempfile::tempdir().unwrap();
        let client = Arc::new(KitPublishClient::default());
        let executor = CommandExecutor::new(
            client.clone(),
            SessionConfig {
                session_id: Some("shell-id".into()),
                username: "test".into(),
                uid: 1,
                gid: 1,
                launch_directory: home.path().into(),
                guest_home: home.path().into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        assert_eq!(
            executor.execute_mcp(&[
                "publish".into(),
                "tool".into(),
                "--kit".into(),
                "codex".into(),
                "--".into(),
                "cat".into()
            ]),
            0
        );
        assert_eq!(
            *client.published_sandbox.lock().unwrap(),
            Some("codex".into())
        );
    }

    #[derive(Default)]
    struct AcpStatusFailureClient {
        stopped: AtomicBool,
        peer: Mutex<Option<UnixStream>>,
        response: Mutex<Option<(String, String, String)>>,
    }

    impl DaemonClient for AcpStatusFailureClient {
        fn attach_shell(&self, _: u32, _: &SessionConfig) -> Result<String, ClientError> {
            unreachable!()
        }
        fn detach_shell(&self, _: &str) -> Result<(), ClientError> {
            unreachable!()
        }
        fn open_shell(&self, _: &SessionConfig, _: &[OsString]) -> Result<i32, ClientError> {
            unreachable!()
        }
        fn registered_commands(&self) -> Result<Vec<String>, ClientError> {
            unreachable!()
        }
        fn prepare(&self, _: &LoadSelection, _: &SessionConfig) -> Result<LoadReport, ClientError> {
            unreachable!()
        }
        fn reset_workers(&self, _: &LoadSelection) -> Result<WorkerResetReport, ClientError> {
            unreachable!()
        }
        fn status_json(&self) -> Result<Value, ClientError> {
            unreachable!()
        }
        fn jobs_json(&self) -> Result<Value, ClientError> {
            unreachable!()
        }
        fn job_json(&self, _: &str) -> Result<Value, ClientError> {
            unreachable!()
        }
        fn execute(&self, _: ExecuteRequest) -> Result<i32, ClientError> {
            unreachable!()
        }
        fn acp_start(
            &self,
            _: &str,
            _: &SessionConfig,
            _: Option<&str>,
        ) -> Result<(String, String, UnixStream), ClientError> {
            let (lease, peer) = UnixStream::pair().unwrap();
            *self.peer.lock().unwrap() = Some(peer);
            Ok(("agent-id".into(), "kit-job-id".into(), lease))
        }
        fn acp_status(
            &self,
            _: &str,
            _: &SessionConfig,
            _: u64,
        ) -> Result<marsh_daemon::AcpSessionStatus, ClientError> {
            Err(ClientError("status transport timed out".into()))
        }
        fn acp_stop(&self, _: &str, _: &SessionConfig) -> Result<(), ClientError> {
            self.stopped.store(true, Ordering::Release);
            let mut peer = self.peer.lock().unwrap().take().unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                peer.write_all(b"T").unwrap();
            });
            Ok(())
        }
        fn acp_respond(
            &self,
            id: &str,
            _: &SessionConfig,
            request_id: &str,
            option_id: &str,
        ) -> Result<(), ClientError> {
            *self.response.lock().unwrap() = Some((id.into(), request_id.into(), option_id.into()));
            Ok(())
        }
    }

    #[test]
    fn acp_status_transport_failure_stops_and_waits_for_terminal() {
        let home = tempfile::tempdir().unwrap();
        let client = Arc::new(AcpStatusFailureClient::default());
        let executor = CommandExecutor::new(
            client.clone(),
            SessionConfig {
                session_id: Some("shell-id".into()),
                username: "test".into(),
                uid: 1,
                gid: 1,
                launch_directory: home.path().into(),
                guest_home: home.path().into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        let started = std::time::Instant::now();
        assert_eq!(executor.run_acp("agent-kit"), 125);
        assert!(client.stopped.load(Ordering::Acquire));
        assert!(started.elapsed() >= std::time::Duration::from_millis(50));
    }

    #[test]
    fn acp_respond_shell_command_passes_exact_user_choice() {
        let home = tempfile::tempdir().unwrap();
        let client = Arc::new(AcpStatusFailureClient::default());
        let executor = CommandExecutor::new(
            client.clone(),
            SessionConfig {
                session_id: Some("shell-id".into()),
                username: "test".into(),
                uid: 1,
                gid: 1,
                launch_directory: home.path().into(),
                guest_home: home.path().into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        assert_eq!(
            executor.execute_acp(&[
                "respond".into(),
                "agent-id".into(),
                "request-id".into(),
                "once".into(),
            ]),
            0
        );
        assert_eq!(
            *client.response.lock().unwrap(),
            Some(("agent-id".into(), "request-id".into(), "once".into()))
        );
    }

    fn file(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "test").unwrap();
    }

    #[test]
    fn explicit_sbx_override_wins_without_path() {
        let installation = tempfile::tempdir().unwrap();
        let configured = installation.path().join("configured/sbx");
        file(&configured);
        assert_eq!(
            resolve_stock_sbx_from(Some(configured.as_os_str()), None).unwrap(),
            configured.canonicalize().unwrap()
        );
    }

    #[test]
    fn stock_sbx_resolves_and_canonicalizes_path_entry() {
        let checkout = tempfile::tempdir().unwrap();
        let path_sbx = checkout.path().join("tools/sbx");
        file(&path_sbx);
        assert_eq!(
            resolve_stock_sbx_from(None, Some(checkout.path().join("tools").as_os_str())).unwrap(),
            path_sbx.canonicalize().unwrap()
        );
    }

    #[test]
    fn missing_path_reports_actionable_error() {
        let error = resolve_stock_sbx_from(None, None).unwrap_err();
        assert!(error.0.contains("PATH is required"));
    }
}
