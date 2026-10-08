//! marsh executable entry point.

mod acp_publication;
mod ephemeral_recovery;
mod mcp_load;
mod sbx_shim;

use marsh::{
    cli,
    client::{
        CommandExecutor, DaemonClient, LocalDaemonClient, exec_system_process_command,
        resolve_stock_sbx,
    },
    external_commands::{self, ExternalCommands},
    product,
    registered_commands::{RegisteredCommandExecutor, invocation_from_environment},
    session::{EphemeralHomeGuard, SessionConfig},
};
use marsh_mcp::{StdinBinding, ToolBindings, ToolDeclaration, WorkspaceIdentity};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use sha2::{Digest as _, Sha256};
use std::{
    env,
    ffi::{OsStr, OsString},
    fmt::Write as _,
    io::Write as _,
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    os::unix::process::CommandExt as _,
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

const HELP: &str = r"marsh — marshal your agents: a Bash-compatible shell for Apple Silicon Macs that
opens your project in a Linux VM and runs agents and tools as sandboxed containers

Usage:
  marsh [MARSH_OPTIONS] [BRUSH_ARGS...]  Launch the project shell
  marsh status [--json]
  marsh jobs [--all] [--json]          This session's jobs (host: the last hour)
  marsh jobs show JOB [--json]
  marsh jobs --tree [--all] [--json]   The same jobs, children under parents
  marsh run [--spawn NAME,...|--no-spawn] NAME [ARG...]
                                       Run a registered command as a new job
  marsh context                        Print the context text every job sees
  marsh results [--json]
  marsh results show CURSOR|JOB [--json]
  marsh workers reset KIT[,KIT...]|all
  marsh split [-n] [-b LABEL=STRING]... [::: LABEL CMD [ARG...]]...
  marsh join [--json] [--keep] [-- CMD [ARG...]]
  marsh splits [--json] [ID] | cancel ID | rm ID
  marsh fanout [-n] [-b LABEL=STRING]... [::: LABEL CMD [ARG...]]... | marsh collect [--json] [--timing]
                                       Run branches at once on the same files
  marsh kit install NAME --from REPOSITORY@sha256:DIGEST
  marsh config shell [marsh|bash|zsh]  Show or set this home's default shell
  marsh stop [--json]                  Remove this home's VMs and stop its daemon
  marsh reset [--json]                 Remove this home's VMs; keep the daemon
  marsh recover-home SLOT --discard
  marsh mcp serve [MCP_OPTIONS...]
  marsh mcp export-serve [MCP_OPTIONS...] --declaration PATH
  marsh mcp start
  marsh mcp stop
  marsh mcp install codex|claude|sbx
  marsh mcp install-published codex NAME
  marsh mcp remove-published codex NAME
  marsh mcp load NAME --sandbox SANDBOX
  marsh mcp unpublish NAME
  marsh acp install-published codex NAME
  marsh acp remove-published codex NAME

Inside the project shell:
  ps --marsh [--verbose|--json]  Show scoped shell, Kit job, and ACP activity once
  top --marsh [--verbose] [--once]  Refresh activity; CPU/memory are unavailable
  mcp publish NAME [--description TEXT] [--kit KIT | --sandbox SANDBOX] -- 'PIPELINE'
                           Publish a fixed Brush pipeline for MCP clients
  mcp load NAME --kit KIT | --sandbox SANDBOX
                           Load an existing publication without republishing
  mcp unpublish NAME       Revoke a published pipeline
  acp run AGENT &          Start a registered ACP agent (e.g. claude-session) as a Brush background job
  acp reserve AGENT        Reserve a session id; then acp run --reservation ID AGENT &
  acp list [--mine] [--wait [ID]] [--json]
                           Find sessions, including agents still starting
  acp ask ID TEXT          Send a turn and print the agent's answer
  acp prompt [--key UUID] ID TEXT
                           Send a turn asynchronously; retry with the printed key
  acp status ID [CURSOR] [--json]
                           Inspect updates and the linked Kit receipt
  acp cancel ID            Cancel the active turn
  acp permissions ID [--json]
                           List the agent's pending permission requests
  acp respond ID REQUEST OPTION
                           Answer one permission request
  acp attach ID            Control a session from this shell
  acp release ID           Give up control of a session
  acp publish ID --name NAME [--kit KIT | --sandbox SANDBOX]
                           Publish a session as an MCP tool (acp unpublish NAME)
  acp stop ID              Stop the Kit job

Options:
  --load KIT[,KIT...]|all  Prepare selected configured Kit VMs before launch
  --shell marsh|bash|zsh   Interactive shell for this launch (default: the home's, else marsh)
  --ephemeral-home         Use a blank session home with no persistent writeback
  --dev                    Develop marsh: open a dev shell whose `sbx` is a confined host broker
  -h, --help               Show this host-local help and exit
  -V, --version            Show the marsh version and exit

Environment:
  MARSH_HOME               Absolute scope root; guest home is $MARSH_HOME/home
  MARSH_SHELL=bash|zsh|marsh  Interactive shell when --shell is not given
  MARSH_PLACE=local        Placement for registered commands inside the shell
  MARSH_SPAWN=a,b|none     Narrow the names jobs started from this shell may spawn

All other arguments go to the shell (Brush unless bash or zsh is chosen).
After -c or --, marsh does not interpret arguments.
";
const MCP_HELP: &str = r"marsh mcp - share selected shell work with agents

Usage:
  marsh mcp serve [OPTIONS]
  marsh mcp export-serve [OPTIONS] --declaration PATH
  marsh mcp start
  marsh mcp stop
  marsh mcp install codex|claude|sbx
  marsh mcp install-published codex NAME
  marsh mcp remove-published codex NAME
  marsh mcp load NAME --sandbox SANDBOX
  marsh mcp unpublish NAME

Inside an attached project shell:
  mcp publish NAME [--description TEXT] [--kit KIT | --sandbox SANDBOX] -- 'PIPELINE'

`install` registers this installed marsh MCP server with a host client for the
current canonical workspace under a deterministic workspace-specific name.
Codex and Claude launch it on demand. SBX exposes one shared per-workspace host
broker through thin stdio connections from selected sandboxes. `install sbx`
starts that broker automatically; `start` restores it after a host restart
without changing the SBX registration. `stop` refuses while sandboxes are
attached to the broker.

`publish` registers one named Brush pipeline as an export-only MCP tool from
an attached project shell. Direct host publication is rejected before effects. It accepts a bounded `input` string on stdin and returns
the command's output and exit status. `--sandbox` loads the tool into a running
stock SBX sandbox; connected agents can then run the pipeline and read its
output. `--kit KIT` loads it into that Kit's VM. With neither, the tool is a
default for agent Kits: every Kit VM created from now on (first use, or after
`marsh workers reset KIT`) loads it before its first job. Kit VMs already
running are not changed. No sudo is required.
`load NAME --sandbox SANDBOX` loads an existing publication without changing
its generation or invalidating other clients. Inside an attached project shell,
`mcp load NAME --kit KIT` prepares and loads the exact Kit worker. Every client
and later job in the target sandbox can use it; start a new agent session there
to discover the tool. Loading does not change workspace or network policy.
`unpublish` revokes calls and may also be run from a host terminal in the same
project and selected home. Publishing requires the attached project shell. `--sandbox` can name another same-user stock SBX sandbox; it explicitly
grants this shell narrow host MCP registration/load authority. A loaded client
may still display the tool after its sandbox restarts; calls to the revoked
publication are denied. Remove the stale tool from that client. Project files
read by the pipeline remain mutable.
`install-published codex NAME` makes an existing fixed publication available
to new host Codex tasks. `remove-published` removes only its exact registration.
Run these two commands on the macOS host, from the publishing project with the
same selected marsh home. They do not install the broader development server.

Options passed to the MCP server:
  --workspace PATH  Fix the managed workspace (default: current directory)
  --scope-root PATH Select the private root for isolated development scopes
  --home PATH       Select legacy single-scope MARSH_HOME (disables scope_start)
  --marsh PATH      Select the marsh executable used by managed operations
  --sbx PATH        Select the stock SBX executable used by managed operations
  --allow-full-sbx-control
                    Allow fixed repository qualification gates to execute on host
  -h, --help        Show server-specific help

The server uses stdin and stdout for MCP. Start it as a host process from an
MCP client such as Claude Code or Codex; it is unavailable through the project-shell relay.
";
const VERSION: &str = concat!("marsh ", env!("CARGO_PKG_VERSION"), "\n");

/// `marsh --version`, marked when the install is not a release: a
/// `make dist-local` tarball carries `libexec/marsh/local-build`, a
/// `make dev` install `libexec/marsh/dev-enabled`.
fn version_text() -> String {
    let libexec = env::current_exe()
        .and_then(std::fs::canonicalize)
        .ok()
        .and_then(|exe| {
            exe.parent()?
                .parent()
                .map(|prefix| prefix.join("libexec/marsh"))
        });
    let Some(libexec) = libexec else {
        return VERSION.to_owned();
    };
    if let Ok(note) = std::fs::read_to_string(libexec.join("local-build")) {
        let note = note.lines().next().unwrap_or("").trim();
        return format!(
            "marsh {} (local build{}{note}; not a release)\n",
            env!("CARGO_PKG_VERSION"),
            if note.is_empty() { "" } else { ": " }
        );
    }
    if libexec.join("dev-enabled").is_file() {
        return format!("marsh {} (dev build)\n", env!("CARGO_PKG_VERSION"));
    }
    VERSION.to_owned()
}
const STATUS_HELP: &str = "Usage: marsh status [--json]\n\nShow scoped daemon, host control directory, effective per-job limits, shells, and workers.\nJob limits are a snapshot of the first daemon launch environment, not per-shell flags.\nUse the printed control directory for commands.json.\n";
const JOBS_HELP: &str = "Usage: marsh jobs [--all] [--json]\n       marsh jobs --tree [--all] [--json]\n       marsh jobs show JOB [--json]\n\nList Kit jobs, running first, then newest: short id, start, state, exit,\nRUN time (in the ready Kit VM; `marsh results` WALL adds VM preparation),\nand command. Inside a session only its jobs are listed; from a\nhost terminal, running trees, trees started in the last hour, and the five\nnewest trees. --all lists every recorded job. --tree draws child\njobs under their parent. JOB may be an id prefix. --json prints the full,\nunscoped document.\n";
const RESULTS_HELP: &str = "Usage: marsh results [--json]\n       marsh results show CURSOR|JOB [--json]\n\nList durable Kit results or inspect one result.\n";
const WORKERS_HELP: &str = "Usage: marsh workers reset KIT[,KIT...]|all\n\nStop and recreate selected Kit workers on their next use.\n";
const KIT_HELP: &str = "Usage: marsh kit install NAME --from REPOSITORY@sha256:DIGEST\n\nInstall and prepare an alternate native Kit in this daemon scope.\n";
const STOP_HELP: &str = "Usage: marsh stop [--json]\n\nRemove the shell and Kit VMs owned by the selected MARSH_HOME, then stop its daemon.\nRefuses while shells or jobs are active. Home and results are kept.\nPrints what was removed on stderr; --json also writes the daemon report on stdout.\nExits nonzero if any cleanup could not be verified. Run a script named `stop` as `marsh ./stop`.\n";
const RESET_HELP: &str = "Usage: marsh reset [--json]\n\nRemove the shell and Kit VMs owned by the selected MARSH_HOME; the daemon keeps running.\nRefuses while shells or jobs are active. Home and results are kept.\nPrints what was removed on stderr; --json also writes the daemon report on stdout.\nExits nonzero if any cleanup could not be verified. Run a script named `reset` as `marsh ./reset`.\n";
const RECOVER_HOME_HELP: &str = "Usage: marsh recover-home SLOT --discard\n\nrecover-home explicitly selects one retained private slot (0..15), after all shell/Kit owners and pending work are closed. Use the SAME SDK selection; unknown/live authority remains retained. It never removes VMs or sweeps other slots.\n";
const ACP_HOST_HELP: &str = "Usage: marsh acp install-published codex NAME\n       marsh acp remove-published codex NAME\n\nRegister or remove a published ACP control tool for new host Codex tasks. Inside the project shell, run `acp --help` for session commands.\n";

fn main() -> ExitCode {
    let arguments: Vec<OsString> = env::args_os().collect();
    if sbx_shim::invoked_as_sbx(&arguments) {
        return exit_code(sbx_shim::run(&arguments));
    }
    let context = env::var_os(external_commands::SESSION_VARIABLE).is_some();
    if let Some(name) = external_command_name_from(&arguments, context) {
        if external_commands::SHELL_BUILTINS.contains(&name.as_str()) {
            return exit_code(run_shell_builtin_link(&name, arguments));
        }
        return exit_code(run_external_command(
            &name,
            arguments.into_iter().skip(1).collect(),
        ));
    }
    run(arguments)
}

fn external_command_name_from(arguments: &[OsString], context: bool) -> Option<String> {
    if !context {
        return None;
    }
    let name = Path::new(arguments.first()?).file_name()?.to_str()?;
    if name == "marsh" || name == "msh" {
        return None;
    }
    // Brush deliberately sets argv[0] to the registered name when it forks a
    // process-backed builtin. Those children must keep their existing Brush
    // dispatch path; only a descendant's direct PATH lookup uses this route.
    if arguments
        .get(1)
        .is_some_and(|arg| arg == "--invoke-bundled")
        && arguments.get(2).is_some_and(|arg| arg == name)
        && arguments.get(3).is_some_and(|arg| arg == "--marsh-guest")
    {
        return None;
    }
    Some(name.to_owned())
}

/// A bash/zsh session reaches `acp`, `mcp`, `ps`, and `top` through links;
/// re-enter the bundled dispatch Brush's process shim would have used.
fn run_shell_builtin_link(name: &str, arguments: Vec<OsString>) -> i32 {
    let Some(context) = env::var_os(external_commands::SESSION_VARIABLE) else {
        return 125;
    };
    let arguments =
        match external_commands::bundled_arguments(name, &context, arguments.into_iter().skip(1)) {
            Ok(arguments) => arguments,
            Err(message) => {
                eprintln!("{name}: {message}");
                return 125;
            }
        };
    let executable = match env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            eprintln!("{name}: {error}");
            return 126;
        }
    };
    let error = Command::new(executable).arg0(name).args(arguments).exec();
    eprintln!("{name}: {error}");
    126
}

fn run_external_command(name: &str, args: Vec<OsString>) -> i32 {
    if env::var_os("MARSH_DAEMON_SOCKET").is_none() || env::var_os("MARSH_DAEMON_TOKEN").is_none() {
        eprintln!("{name}: registered command requires an attached marsh session");
        return 125;
    }
    let Some(context) = env::var_os(external_commands::SESSION_VARIABLE) else {
        return 125;
    };
    let Ok(session) = serde_json::from_slice::<SessionConfig>(context.as_encoded_bytes()) else {
        eprintln!("{name}: invalid marsh session context");
        return 125;
    };
    if session.session_id.is_none() {
        eprintln!("{name}: missing marsh session identity");
        return 125;
    }
    let client = match LocalDaemonClient::ensure_running(&session.home_backing) {
        Ok(client) => Arc::new(client),
        Err(error) => {
            eprintln!("{name}: {error}");
            return 125;
        }
    };
    let names = match client.registered_commands() {
        Ok(names) => names,
        Err(error) => {
            eprintln!("{name}: {error}");
            return 125;
        }
    };
    if !external_commands::registered_name(name, &names) {
        eprintln!("{name}: command is not registered in this marsh scope");
        return 127;
    }
    let invocation = match invocation_from_environment(name.to_owned(), args) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("{name}: {error}");
            return 125;
        }
    };
    CommandExecutor::new(client, session).execute(invocation)
}

fn run(arguments: Vec<OsString>) -> ExitCode {
    run_with(arguments, std::io::stdout(), try_run)
}

fn run_with(
    arguments: Vec<OsString>,
    mut output: impl std::io::Write,
    run_product: impl FnOnce(Vec<OsString>) -> Result<i32, RunError>,
) -> ExitCode {
    if let Some(command) = host_command(&arguments) {
        let text = match command {
            HostCommand::Help => HELP.to_owned(),
            HostCommand::Version => version_text(),
            HostCommand::McpHelp => MCP_HELP.to_owned(),
            HostCommand::Section(help) => help.to_owned(),
        };
        if let Err(error) = output.write_all(text.as_bytes()) {
            eprintln!("marsh: cannot write output: {error}");
            return exit_code(1);
        }
        return ExitCode::SUCCESS;
    }
    match run_product(arguments) {
        Ok(status) => exit_code(status),
        Err(error) => {
            eprintln!("marsh: {}", error.message);
            exit_code(error.status)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostCommand {
    Help,
    Version,
    McpHelp,
    Section(&'static str),
}

fn host_command(arguments: &[OsString]) -> Option<HostCommand> {
    let words = arguments
        .get(1..)?
        .iter()
        .map(|arg| arg.to_str())
        .collect::<Option<Vec<_>>>()?;
    match words.as_slice() {
        [argument] if matches!(*argument, "-h" | "--help" | "help") => Some(HostCommand::Help),
        [argument] if matches!(*argument, "-V" | "--version") => Some(HostCommand::Version),
        [mcp, help] if *mcp == "mcp" && matches!(*help, "-h" | "--help") => {
            Some(HostCommand::McpHelp)
        }
        [command, help] if matches!(*help, "-h" | "--help" | "help") => {
            section_help(command).map(HostCommand::Section)
        }
        [help, command] if *help == "help" => section_help(command).map(HostCommand::Section),
        [command, subcommand, help]
            if matches!(*help, "-h" | "--help")
                && recognized_help_subcommand(command, subcommand) =>
        {
            section_help(command).map(HostCommand::Section)
        }
        [command, help, subcommand]
            if *help == "help" && recognized_help_subcommand(command, subcommand) =>
        {
            section_help(command).map(HostCommand::Section)
        }
        _ => None,
    }
}

fn recognized_help_subcommand(command: &str, subcommand: &str) -> bool {
    matches!(
        (command, subcommand),
        ("jobs" | "results", "show")
            | ("workers", "reset")
            | ("kit", "install")
            | ("acp", "install-published" | "remove-published")
            | (
                "mcp",
                "serve"
                    | "export-serve"
                    | "start"
                    | "stop"
                    | "install"
                    | "install-published"
                    | "remove-published"
                    | "publish"
                    | "load"
                    | "unpublish"
            )
    )
}

fn section_help(command: &str) -> Option<&'static str> {
    match command {
        "status" => Some(STATUS_HELP),
        "jobs" => Some(JOBS_HELP),
        "results" => Some(RESULTS_HELP),
        "workers" => Some(WORKERS_HELP),
        "kit" => Some(KIT_HELP),
        "stop" => Some(STOP_HELP),
        "reset" => Some(RESET_HELP),
        "recover-home" => Some(RECOVER_HOME_HELP),
        "acp" => Some(ACP_HOST_HELP),
        "mcp" => Some(MCP_HELP),
        "config" => Some(marsh::shell_choice::CONFIG_HELP),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)] // Keep host session authority and daemon attachment in one sequence.
fn try_run(mut arguments: Vec<OsString>) -> Result<i32, RunError> {
    if arguments
        .get(1)
        .is_some_and(|value| value == "--internal-own-tty")
    {
        return own_tty_and_exec(&arguments);
    }
    if arguments.get(1).is_some_and(|value| value == "acp") {
        return acp_publication::run(&protocol_arguments(&arguments[2..])?);
    }
    if arguments.get(1).is_some_and(|value| value == "mcp") {
        // argv[0] is not protocol data. Do not decode a raw executable path
        // merely to call the existing text-only MCP argument parser.
        let mut protocol = vec!["marsh".into(), "mcp".into()];
        protocol.extend(protocol_arguments(&arguments[2..])?);
        if let Some(action) = mcp_action(&protocol)? {
            return run_mcp_action(action);
        }
    }
    if let Some(verb) = arguments
        .get(1)
        .and_then(|value| value.to_str())
        .filter(|verb| matches!(*verb, "split" | "join" | "splits"))
    {
        let verb = verb.to_owned();
        if let Some(help) = marsh::split_cli::help(&verb, &arguments[2..]) {
            print!("{help}");
            return Ok(0);
        }
        return Ok(marsh::split_cli::run(&verb, &arguments[2..], split_channel));
    }
    match arguments.get(1).and_then(|value| value.to_str()) {
        Some("collect") => return Ok(marsh::fanout_cli::collect(&arguments[2..])),
        Some("fanout") => {
            if matches!(
                arguments.get(2).and_then(|value| value.to_str()),
                Some("-h" | "--help")
            ) {
                print!("{}", marsh::fanout_cli::FANOUT_HELP);
                return Ok(0);
            }
            if env::var_os("MARSH_DAEMON_SOCKET").is_some()
                && env::var_os(external_commands::SESSION_VARIABLE).is_some()
            {
                return Ok(marsh::fanout_cli::fanout(
                    &arguments[2..],
                    marsh::fanout_cli::Place::Session,
                ));
            }
            // Host: run the same fanout in a session shell in the shell VM.
            let script = marsh::fanout_cli::host_script(&arguments[2..])
                .map_err(|message| RunError::new(message, 2))?;
            arguments = vec![arguments[0].clone(), "-c".into(), script.into()];
        }
        Some("context") if arguments.len() == 2 => {
            print!("{}", marsh_contracts::process::CONTEXT);
            return Ok(0);
        }
        Some("run") => return Ok(run_command(&arguments[2..])),
        Some("jobs") if marsh::job::is_listing(&arguments[2..]) => {
            return match split_channel(false) {
                Ok(channel) => {
                    let scope = channel
                        .session
                        .as_ref()
                        .map_or(marsh::job::JobScope::Recent, |session| {
                            marsh::job::JobScope::Session(&session.session_id)
                        });
                    Ok(marsh::job::jobs_in(&channel.client, &arguments[2..], scope))
                }
                Err(message) => Err(RunError::new(message, 1)),
            };
        }
        _ => {}
    }
    if arguments
        .get(1)
        .is_some_and(|value| value == "--internal-supervisor")
    {
        if arguments.len() != 2 {
            return Err(RunError::new("invalid internal supervisor", 2));
        }
        marsh::shell_supervisor::run()
            .map_err(|error| RunError::new(format!("shell supervisor failed: {error}"), 1))?;
        return Ok(0);
    }
    if arguments
        .get(1)
        .is_some_and(|value| value == "--internal-record-session")
    {
        if arguments.len() < 5 {
            return Err(RunError::new("invalid internal session registration", 2));
        }
        let record = std::path::Path::new(&arguments[2]);
        let uid = internal_id(&arguments[3], "invalid internal session UID")?;
        let gid = internal_id(&arguments[4], "invalid internal session GID")?;
        marsh::session_process::record_current(record, uid, gid).map_err(|error| {
            RunError::new(format!("cannot contain shell session: {error}"), 125)
        })?;
        arguments.drain(1..5);
    }
    if arguments.get(1).is_some_and(|arg| arg == "recover-home") {
        return ephemeral_recovery::run(&arguments[2..]);
    }
    if arguments.get(1).is_some_and(|arg| arg == "config") {
        let scope = SessionConfig::from_environment(false)
            .map_err(|error| RunError::new(error.to_string(), 1))?;
        let scope = scope
            .home_backing
            .parent()
            .ok_or_else(|| RunError::new("selected home has no scope root", 1))?
            .to_path_buf();
        return marsh::shell_choice::config_command(&arguments[2..], &scope)
            .map_err(|(message, status)| RunError::new(message, status));
    }
    let mut parsed =
        cli::parse_os(arguments).map_err(|error| RunError::new(error.to_string(), 2))?;
    // Pure input validation precedes session/home creation and daemon startup.
    if let cli::ProductCommand::KitInstall { command, reference } = &parsed.command {
        marsh_contracts::command_registry::CommandName::parse(command.clone())
            .map_err(|error| RunError::new(error.to_string(), 2))?;
        marsh_contracts::OciImage::parse(reference.clone()).map_err(|_| {
            RunError::new(
                "Kit install requires an immutable REPOSITORY@sha256:DIGEST reference",
                2,
            )
        })?;
    }
    if let [_, dispatch, name, args @ ..] = parsed.brush_args.as_slice()
        && dispatch == "--invoke-bundled"
        && matches!(name.to_str(), Some("ps" | "top"))
        && args.first().is_none_or(|arg| arg != "--marsh")
    {
        let name = if name == "ps" { "ps" } else { "top" };
        return Ok(exec_system_process_command(name, args));
    }
    let mut session = SessionConfig::from_environment(parsed.ephemeral_home)
        .map_err(|error| RunError::new(error.to_string(), 1))?;
    if parsed.guest
        && parsed
            .brush_args
            .get(1)
            .is_some_and(|argument| argument == "--invoke-bundled")
        && let Some(session_id) = parsed.session_id.as_deref()
    {
        let context = env::var_os(external_commands::SESSION_VARIABLE).ok_or_else(|| {
            RunError::new("registered command is missing attached shell context", 125)
        })?;
        inherit_guest_project_root(&mut session, session_id, &context)?;
    }
    let expected_project_identity = if parsed.guest {
        None
    } else {
        validate_pinned_guest_project(&session.launch_directory)?
    };
    // Daemon scope follows the selected persistent MARSH_HOME even when this
    // session receives a disposable home backing. This preserves one resident
    // daemon and shared warm workers across persistent and ephemeral shells.
    let daemon_home = session
        .home_backing
        .parent()
        .ok_or_else(|| RunError::new("selected home has no scope root", 1))?
        .to_path_buf();
    if !parsed.guest && matches!(parsed.command, cli::ProductCommand::Shell) {
        let variable = env::var(marsh::shell_choice::SHELL_VARIABLE).ok();
        parsed.shell = Some(
            marsh::shell_choice::resolve(parsed.shell, variable.as_deref(), &daemon_home)
                .map_err(|message| RunError::new(message, 2))?,
        );
    }
    if matches!(
        parsed.command,
        cli::ProductCommand::Reset { .. } | cli::ProductCommand::Stop { .. }
    ) {
        match std::fs::symlink_metadata(&daemon_home) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // No scope directory exists here. Do not create
                // it or launch a daemon merely to report the absent scope.
                return write_absent_scope_report(&parsed.command, &daemon_home);
            }
            Err(error) => {
                return Err(RunError::new(
                    format!("cannot inspect scope {}: {error}", daemon_home.display()),
                    1,
                ));
            }
            Ok(_) => {}
        }
    }
    if parsed.guest && parsed.ephemeral_home && parsed.ephemeral_home_backing.is_none() {
        return Err(RunError::new(
            "ephemeral guest shell is missing its host backing",
            2,
        ));
    }
    if let Some(backing) = parsed.ephemeral_home_backing.clone() {
        if !parsed.guest {
            return Err(RunError::new(
                "--marsh-home-backing is reserved for the guest shell",
                2,
            ));
        }
        session
            .apply_ephemeral_backing(backing)
            .map_err(|error| RunError::new(error.to_string(), 2))?;
    }
    let activate_ephemeral_home = parsed.ephemeral_home && !parsed.guest;
    with_session_home(
        session,
        activate_ephemeral_home,
        |mut session, ephemeral_token| {
            let lifecycle_only = matches!(
                &parsed.command,
                cli::ProductCommand::Reset { .. } | cli::ProductCommand::Stop { .. }
            );
            let client = if lifecycle_only {
                match LocalDaemonClient::connect_existing(&daemon_home).map_err(|error| {
                    RunError::new(format!("cannot connect to the local daemon: {error}"), 1)
                })? {
                    Some(client) => client,
                    None => return write_absent_scope_report(&parsed.command, &daemon_home),
                }
            } else {
                let shell_delivery =
                    !parsed.guest && matches!(parsed.command, cli::ProductCommand::Shell);
                let fail = |message: String| {
                    let error = if shell_delivery {
                        product::ProductError::ShellDelivery(message)
                    } else {
                        product::ProductError::Operation(message)
                    };
                    RunError::new(error.to_string(), error.exit_code())
                };
                // A missing or wrong stock sbx is the user's setup, not a
                // daemon connection failure: say so plainly.
                // A relayed (guest) client reaches the daemon without sbx.
                if std::env::var_os("MARSH_DAEMON_SOCKET").is_none()
                    && std::env::var_os("MARSH_DAEMON_TOKEN").is_none()
                {
                    resolve_stock_sbx().map_err(|error| fail(error.to_string()))?;
                }
                LocalDaemonClient::ensure_running(&daemon_home)
                    .map_err(|error| fail(format!("cannot connect to the local daemon: {error}")))?
            };
            let client = Arc::new(
                client
                    .with_expected_project_identity(expected_project_identity)
                    .with_ephemeral_home_token(ephemeral_token),
            );
            let relay_inspection = env::var_os("MARSH_DAEMON_SOCKET").is_some()
                && env::var_os("MARSH_DAEMON_TOKEN").is_some()
                && matches!(
                    &parsed.command,
                    cli::ProductCommand::Status { .. }
                        | cli::ProductCommand::Results { .. }
                        | cli::ProductCommand::Result { .. }
                        | cli::ProductCommand::KitInstall { .. }
                );
            if !parsed.guest && !relay_inspection {
                session
                    .ensure_persistent_home()
                    .map_err(|error| RunError::new(error.to_string(), 1))?;
            }
            let external_session = session.clone();
            let guest = parsed.guest;
            let daemon: Arc<dyn DaemonClient> = client.clone();
            let prepared = product::prepare(
                parsed,
                daemon,
                session,
                std::io::stdout(),
                std::io::stderr(),
            )
            .map_err(|error| RunError::new(error.to_string(), error.exit_code()))?;
            let prepared = match prepared {
                product::ProductOutcome::Complete(status) => return Ok(status),
                product::ProductOutcome::GuestShell(prepared) => prepared,
            };
            let links = if guest {
                let names = client
                    .registered_commands()
                    .map_err(|error| RunError::new(error.to_string(), 1))?;
                let links = ExternalCommands::install(&names).map_err(|error| {
                    RunError::new(format!("cannot install descendant commands: {error}"), 1)
                })?;
                let path = links
                    .path()
                    .to_str()
                    .ok_or_else(|| RunError::new("command directory is not UTF-8", 1))?;
                let mut context = external_session;
                context.session_id = Some(prepared.session_id.clone());
                let context = external_commands::session_context(&context).map_err(|error| {
                    RunError::new(format!("cannot encode session context: {error}"), 1)
                })?;
                let user_shell = prepared.shell != marsh::shell_choice::ShellChoice::Marsh
                    && prepared
                        .brush_args
                        .get(1)
                        .is_none_or(|arg| arg != "--invoke-bundled");
                if user_shell {
                    links.install_shell_builtins().map_err(|error| {
                        RunError::new(format!("cannot install shell commands: {error}"), 1)
                    })?;
                    let status = marsh::shell_choice::run_user_shell(
                        prepared.shell,
                        &prepared.brush_args[1..],
                        links.path(),
                        &context,
                    )
                    .map_err(|(message, status)| RunError::new(message, status));
                    drop(links);
                    return status;
                }
                brush_shell::entry::install_marsh_external_commands(path.to_owned(), context);
                Some(links)
            } else {
                None
            };
            brush_shell::entry::enable_marsh_extensions();
            let brush_args = prepared
                .brush_args
                .into_iter()
                .map(|argument| {
                    argument
                        .into_string()
                        .map_err(|_| RunError::new("shell arguments must be valid UTF-8", 2))
                })
                .collect::<Result<Vec<String>, _>>()?;
            let status = brush_shell::entry::run_from(brush_args);
            drop(links);
            Ok(status)
        },
    )
}

/// `marsh run` from the host or the shell VM: a session-root job through
/// `Execute` (`docs/design/processes.md` s3), narrowed by `--spawn`/`MARSH_SPAWN`.
fn run_command(arguments: &[OsString]) -> i32 {
    let (spawn, name, rest) = match marsh::job::parse_run(arguments) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            print!("{}", marsh::job::RUN_HELP);
            return 0;
        }
        Err(message) => {
            eprintln!("marsh: {message}");
            return 2;
        }
    };
    let spawn = spawn.or_else(|| {
        env::var("MARSH_SPAWN")
            .ok()
            .map(|value| marsh_contracts::process::parse_spawn(&value))
    });
    let channel = match split_channel(true) {
        Ok(channel) => channel,
        Err(message) => {
            eprintln!("marsh: run: {message}");
            return 125;
        }
    };
    let Some(session) = channel.session.clone() else {
        eprintln!("marsh: run: no session");
        return 125;
    };
    let (environment, _) = marsh::registered_commands::collect_exported_environment(env::vars_os());
    let spec = marsh_daemon::ExecuteSpec {
        command: name,
        arguments: rest
            .iter()
            .map(|argument| argument.as_encoded_bytes().to_vec())
            .collect(),
        placement: marsh_daemon::Placement::Local,
        environment,
        working_directory: env::current_dir().ok(),
        session,
        process: spawn.map(|spawn| marsh_daemon::process::ProcessLink {
            parent_job: None,
            spawn: Some(spawn),
            branch: false,
            start_env: std::collections::BTreeMap::new(),
        }),
    };
    let status = match channel.client.execute_with_io(
        spec,
        std::io::stdin(),
        std::io::stdout(),
        std::io::stderr(),
    ) {
        Ok(code) => code,
        Err(error) => {
            let message = match error {
                marsh_daemon::DaemonError::Remote(message)
                | marsh_daemon::DaemonError::Refused(message) => message,
                other => other.to_string(),
            };
            eprintln!("marsh: {message}");
            125
        }
    };
    if let Some(session) = &channel.detach {
        let _ = channel
            .client
            .request(marsh_daemon::PublicRequest::DetachShell {
                session_id: session.clone(),
            });
    }
    status
}

/// The daemon channel for `split`/`join`/`splits`: the session relay inside
/// the shell VM, or the scoped daemon from a host terminal. A host `split`
/// attaches an ephemeral session so shell branches have a shell VM.
fn split_channel(create: bool) -> Result<marsh::split_cli::Channel, String> {
    if let (Some(socket), Some(token)) = (
        env::var_os("MARSH_DAEMON_SOCKET"),
        env::var_os("MARSH_DAEMON_TOKEN"),
    ) {
        let client = marsh_daemon::Client::connect_relay(Path::new(&socket), Path::new(&token))
            .map_err(|error| error.to_string())?;
        let session = env::var_os(external_commands::SESSION_VARIABLE)
            .and_then(|context| {
                serde_json::from_slice::<SessionConfig>(context.as_encoded_bytes()).ok()
            })
            .ok_or("missing attached session context")?;
        let session = marsh::client::daemon_session(&session).map_err(|error| error.to_string())?;
        return Ok(marsh::split_cli::Channel {
            client,
            session: Some(session),
            detach: None,
        });
    }
    let mut session = SessionConfig::from_environment(false).map_err(|error| error.to_string())?;
    let daemon_home = session
        .home_backing
        .parent()
        .ok_or("selected home has no scope root")?
        .to_path_buf();
    let client =
        LocalDaemonClient::ensure_running(&daemon_home).map_err(|error| error.to_string())?;
    if !create {
        return Ok(marsh::split_cli::Channel {
            client: client.into_daemon(),
            session: None,
            detach: None,
        });
    }
    session
        .ensure_persistent_home()
        .map_err(|error| error.to_string())?;
    let session_id = client
        .attach_shell(std::process::id(), &session)
        .map_err(|error| error.to_string())?;
    session.session_id = Some(session_id.clone());
    let spec = marsh::client::daemon_session(&session).map_err(|error| error.to_string())?;
    Ok(marsh::split_cli::Channel {
        client: client.into_daemon(),
        session: Some(terminal_off(spec)),
        detach: Some(session_id),
    })
}

fn terminal_off(mut spec: marsh_daemon::SessionSpec) -> marsh_daemon::SessionSpec {
    spec.terminal = false;
    spec.terminal_size = None;
    spec
}

/// A spawned bundled command has its own physical cwd, but its session metadata
/// must keep the parent's project root. This environment is NOT authority: the
/// daemon still replaces all `SessionSpec` authority fields from its own store.
fn inherit_guest_project_root(
    session: &mut SessionConfig,
    session_id: &str,
    context: &OsStr,
) -> Result<(), RunError> {
    let inherited: SessionConfig = serde_json::from_slice(context.as_encoded_bytes())
        .map_err(|_| RunError::new("invalid attached shell context", 125))?;
    if inherited.session_id.as_deref() != Some(session_id)
        || !inherited.launch_directory.is_absolute()
    {
        return Err(RunError::new(
            "attached shell context does not match the command session",
            125,
        ));
    }
    session.launch_directory = inherited.launch_directory;
    Ok(())
}

/// Checked text conversion is reserved for recognized product protocols, never
/// ordinary Brush source, paths, positional arguments, or bundled Kit operands.
fn protocol_arguments(arguments: &[OsString]) -> Result<Vec<String>, RunError> {
    arguments
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| RunError::new("product command arguments must be valid UTF-8", 2))
        })
        .collect()
}

fn internal_id(value: &OsStr, message: &'static str) -> Result<u32, RunError> {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| RunError::new(message, 2))
}

fn own_tty_and_exec(arguments: &[OsString]) -> Result<i32, RunError> {
    if arguments.len() < 5 || nix::unistd::Uid::effective().as_raw() != 0 {
        return Err(RunError::new("invalid internal tty setup", 2));
    }
    let uid = internal_id(&arguments[2], "invalid internal tty UID")?;
    let gid = internal_id(&arguments[3], "invalid internal tty GID")?;
    // Only the attached PTY slave is changed; no /dev/pts pathname is opened.
    let stdin = std::io::stdin();
    if !nix::unistd::isatty(&stdin).unwrap_or(false) {
        return Err(RunError::new("internal tty setup requires a PTY", 1));
    }
    nix::unistd::fchown(
        &stdin,
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
    )
    .map_err(|error| RunError::new(format!("cannot own attached PTY: {error}"), 1))?;
    // Keep the PTY private to the shell user after setpriv drops root.
    nix::sys::stat::fchmod(
        &stdin,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .map_err(|error| RunError::new(format!("cannot secure attached PTY: {error}"), 1))?;
    let error = Command::new("/usr/bin/setpriv")
        .args(&arguments[4..])
        .exec();
    Err(RunError::new(
        format!("cannot drop shell privileges: {error}"),
        1,
    ))
}

fn run_mcp_action(action: McpAction) -> Result<i32, RunError> {
    match action {
        McpAction::Serve(server_arguments) => run_mcp_server(&server_arguments),
        McpAction::Start => start_mcp_broker(),
        McpAction::Stop => stop_mcp_broker(),
        McpAction::Install(client) => install_mcp(client),
        McpAction::InstallPublished(name) => codex_published_mcp(&name, false),
        McpAction::RemovePublished(name) => codex_published_mcp(&name, true),
        McpAction::Publish {
            name,
            description,
            sandbox,
            pipeline,
        } => {
            let mut transaction = mcp_load::HostPublication::open(
                marsh_daemon::PublicationKind::Mcp,
                marsh_daemon::PublicationOperation::Publish,
                &name,
            )?;
            let result = publish_mcp(
                &name,
                description.as_deref(),
                sandbox.as_deref(),
                &pipeline,
                &mut transaction,
            );
            transaction.finish(result)
        }
        McpAction::Load { name, kit, sandbox } => {
            let mut transaction = mcp_load::HostPublication::open(
                marsh_daemon::PublicationKind::Mcp,
                marsh_daemon::PublicationOperation::Load,
                &name,
            )?;
            let result =
                mcp_load::load(&name, kit.as_deref(), sandbox.as_deref(), &mut transaction);
            transaction.finish(result)
        }
        McpAction::Unpublish(name) => {
            let mut transaction = mcp_load::HostPublication::open(
                marsh_daemon::PublicationKind::Mcp,
                marsh_daemon::PublicationOperation::Unpublish,
                &name,
            )?;
            let result = unpublish_mcp(&name, &mut transaction);
            transaction.finish(result)
        }
    }
}

/// No daemon answers for this home. Only the home's VM ownership map can say
/// whether anything is still running; never launch a daemon to find out.
fn absent_scope_report(
    command: &cli::ProductCommand,
    recorded: std::io::Result<Vec<String>>,
) -> Result<marsh_daemon::ScopeLifecycleReport, RunError> {
    let (action, word) = match command {
        cli::ProductCommand::Reset { .. } => (marsh_daemon::ScopeLifecycleAction::Reset, "reset"),
        cli::ProductCommand::Stop { .. } => (marsh_daemon::ScopeLifecycleAction::Stop, "stop"),
        _ => return Err(RunError::new("internal lifecycle command mismatch", 1)),
    };
    let uncertain =
        |label: &str, vm: Option<String>, detail: String| marsh_daemon::ScopeCleanupComponent {
            kind: "scope".into(),
            label: label.into(),
            state: marsh_daemon::ScopeCleanupState::CleanupUncertain,
            vm,
            detail: Some(detail),
        };
    let components = match recorded {
        Ok(names) => names
            .into_iter()
            .map(|name| {
                uncertain(
                    "recorded",
                    Some(name),
                    format!(
                        "daemon is not running, so this VM was not removed; run `marsh status` to start the daemon, then `marsh {word}`"
                    ),
                )
            })
            .collect(),
        Err(error) => vec![uncertain(
            "ownership map",
            None,
            format!("daemon is not running and its VM ownership map is unreadable ({error}); VM cleanup is unverified"),
        )],
    };
    Ok(marsh_daemon::ScopeLifecycleReport {
        action,
        cleanup_complete: components.is_empty(),
        components,
    })
}

fn write_absent_scope_report(
    command: &cli::ProductCommand,
    daemon_home: &Path,
) -> Result<i32, RunError> {
    let account_home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let control = env::var_os("MARSH_CONTROL_HOME").map(PathBuf::from);
    let recorded = marsh_daemon::host_state_directory::recorded_scope_vms(
        &account_home,
        control.as_deref(),
        daemon_home,
    );
    let report = absent_scope_report(command, recorded)?;
    let json = matches!(
        command,
        cli::ProductCommand::Reset { json: true } | cli::ProductCommand::Stop { json: true }
    );
    if report.cleanup_complete {
        eprintln!("marsh: nothing running for {}", daemon_home.display());
        if json {
            let mut encoded = serde_json::to_vec(&report).map_err(|error| {
                RunError::new(format!("cannot encode lifecycle report: {error}"), 1)
            })?;
            encoded.push(b'\n');
            std::io::stdout().write_all(&encoded).map_err(|error| {
                RunError::new(format!("cannot write lifecycle report: {error}"), 1)
            })?;
        }
        return Ok(0);
    }
    let outcome = product::write_lifecycle_report(
        &report,
        daemon_home,
        json,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
    .map_err(|error| RunError::new(error.to_string(), error.exit_code()))?;
    match outcome {
        product::ProductOutcome::Complete(status) => Ok(status),
        product::ProductOutcome::GuestShell(_) => {
            Err(RunError::new("internal lifecycle mismatch", 1))
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum McpAction {
    Serve(Vec<String>),
    Start,
    Stop,
    Install(McpClient),
    InstallPublished(String),
    RemovePublished(String),
    Publish {
        name: String,
        description: Option<String>,
        sandbox: Option<String>,
        pipeline: String,
    },
    Load {
        name: String,
        kit: Option<String>,
        sandbox: Option<String>,
    },
    Unpublish(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum McpClient {
    Codex,
    Claude,
    Sbx,
}

impl McpClient {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "sbx" => Some(Self::Sbx),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Sbx => "sbx",
        }
    }
}

fn mcp_action(arguments: &[String]) -> Result<Option<McpAction>, RunError> {
    let Some(first) = arguments.get(1) else {
        return Ok(None);
    };
    if first != "mcp" {
        return Ok(None);
    }
    match arguments.get(2).map(String::as_str) {
        Some("serve" | "export" | "export-serve" | "serve-export") => {
            Ok(Some(McpAction::Serve(arguments[2..].to_vec())))
        }
        Some("start") if arguments.len() == 3 => Ok(Some(McpAction::Start)),
        Some("stop") if arguments.len() == 3 => Ok(Some(McpAction::Stop)),
        Some("install") if arguments.len() == 4 => McpClient::parse(&arguments[3])
            .map(|client| Some(McpAction::Install(client)))
            .ok_or_else(mcp_usage_error),
        Some("install-published") if arguments.len() == 5 && arguments[3] == "codex" => {
            Ok(Some(McpAction::InstallPublished(arguments[4].clone())))
        }
        Some("remove-published") if arguments.len() == 5 && arguments[3] == "codex" => {
            Ok(Some(McpAction::RemovePublished(arguments[4].clone())))
        }
        Some("publish") => {
            let name = arguments.get(3).ok_or_else(mcp_usage_error)?.clone();
            let mut description = None;
            let mut sandbox = None;
            let mut index = 4;
            while arguments.get(index).is_some_and(|value| value != "--") {
                let value = arguments.get(index + 1).ok_or_else(mcp_usage_error)?;
                match arguments[index].as_str() {
                    "--description" if description.replace(value.clone()).is_none() => {}
                    "--sandbox" if sandbox.replace(value.clone()).is_none() => {}
                    _ => return Err(mcp_usage_error()),
                }
                index += 2;
            }
            if arguments.get(index).is_none_or(|value| value != "--")
                || arguments.len() != index + 2
                || arguments[index + 1].is_empty()
            {
                return Err(mcp_usage_error());
            }
            Ok(Some(McpAction::Publish {
                name,
                description,
                sandbox,
                pipeline: arguments[index + 1].clone(),
            }))
        }
        Some("load") if arguments.len() == 6 => {
            let (kit, sandbox) = match arguments[4].as_str() {
                "--kit" => (Some(arguments[5].clone()), None),
                "--sandbox" => (None, Some(arguments[5].clone())),
                _ => return Err(mcp_usage_error()),
            };
            Ok(Some(McpAction::Load {
                name: arguments[3].clone(),
                kit,
                sandbox,
            }))
        }
        Some("unpublish") if arguments.len() == 4 => {
            Ok(Some(McpAction::Unpublish(arguments[3].clone())))
        }
        _ => Err(mcp_usage_error()),
    }
}

fn mcp_usage_error() -> RunError {
    RunError::new(
        "publish from an attached shell: mcp publish NAME [--description TEXT] [--kit KIT | --sandbox SANDBOX] -- 'PIPELINE'; host commands: marsh mcp load NAME --sandbox SANDBOX | marsh mcp unpublish NAME | marsh mcp install-published codex NAME | marsh mcp remove-published codex NAME | marsh mcp --help",
        2,
    )
}

#[derive(Debug, Eq, PartialEq)]
struct McpRegistration {
    server_name: String,
    workspace: PathBuf,
    scope_root: PathBuf,
    marsh_mcp: PathBuf,
    marsh: PathBuf,
    sbx: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
struct McpClientCommand {
    program: std::ffi::OsString,
    arguments: Vec<std::ffi::OsString>,
    current_dir: PathBuf,
}

impl McpRegistration {
    fn server_arguments_for(&self, mode: &str) -> Vec<std::ffi::OsString> {
        vec![
            self.marsh_mcp.clone().into_os_string(),
            std::ffi::OsString::from(mode),
            std::ffi::OsString::from("--workspace"),
            self.workspace.clone().into_os_string(),
            std::ffi::OsString::from("--scope-root"),
            self.scope_root.clone().into_os_string(),
            std::ffi::OsString::from("--marsh"),
            self.marsh.clone().into_os_string(),
            std::ffi::OsString::from("--sbx"),
            self.sbx.clone().into_os_string(),
        ]
    }

    fn server_arguments(&self) -> Vec<std::ffi::OsString> {
        self.server_arguments_for("serve")
    }

    fn broker_start_command(&self) -> McpClientCommand {
        McpClientCommand {
            program: self.marsh_mcp.clone().into_os_string(),
            arguments: self.server_arguments_for("broker-start")[1..].to_vec(),
            current_dir: self.workspace.clone(),
        }
    }

    fn broker_stop_command(&self) -> McpClientCommand {
        McpClientCommand {
            program: self.marsh_mcp.clone().into_os_string(),
            arguments: self.server_arguments_for("broker-stop")[1..].to_vec(),
            current_dir: self.workspace.clone(),
        }
    }

    fn client_command(&self, client: McpClient) -> Result<McpClientCommand, RunError> {
        let mut arguments = vec![
            std::ffi::OsString::from("mcp"),
            std::ffi::OsString::from("add"),
        ];
        let program = match client {
            McpClient::Codex => {
                arguments.push(std::ffi::OsString::from(&self.server_name));
                arguments.push(std::ffi::OsString::from("--"));
                arguments.extend(self.server_arguments());
                std::ffi::OsString::from("codex")
            }
            McpClient::Claude => {
                arguments.extend([
                    std::ffi::OsString::from("--transport"),
                    std::ffi::OsString::from("stdio"),
                    std::ffi::OsString::from("--scope"),
                    std::ffi::OsString::from("local"),
                    std::ffi::OsString::from(&self.server_name),
                    std::ffi::OsString::from("--"),
                ]);
                arguments.extend(self.server_arguments());
                std::ffi::OsString::from("claude")
            }
            McpClient::Sbx => {
                arguments.push(std::ffi::OsString::from(&self.server_name));
                arguments.push(std::ffi::OsString::from("--command"));
                arguments.push(self.marsh_mcp.clone().into_os_string());
                arguments.push(std::ffi::OsString::from("--args"));
                arguments.push(std::ffi::OsString::from(self.sbx_gateway_arguments()?));
                arguments.push(std::ffi::OsString::from("--dir"));
                arguments.push(self.workspace.clone().into_os_string());
                self.sbx.clone().into_os_string()
            }
        };
        Ok(McpClientCommand {
            program,
            arguments,
            current_dir: self.workspace.clone(),
        })
    }

    fn sbx_gateway_arguments(&self) -> Result<String, RunError> {
        let values = [
            std::ffi::OsStr::new("connect"),
            std::ffi::OsStr::new("--workspace"),
            self.workspace.as_os_str(),
            std::ffi::OsStr::new("--scope-root"),
            self.scope_root.as_os_str(),
            std::ffi::OsStr::new("--marsh"),
            self.marsh.as_os_str(),
            std::ffi::OsStr::new("--sbx"),
            self.sbx.as_os_str(),
        ];
        let mut encoded = Vec::with_capacity(values.len());
        for value in values {
            let value = value
                .to_str()
                .ok_or_else(|| RunError::new("SBX MCP gateway arguments must be valid UTF-8", 1))?;
            if value.contains(',') {
                return Err(RunError::new(
                    format!("SBX MCP gateway cannot encode a comma in argument {value:?}"),
                    1,
                ));
            }
            encoded.push(value);
        }
        Ok(encoded.join(","))
    }
}

fn install_mcp(client: McpClient) -> Result<i32, RunError> {
    require_host_mcp_install(
        client,
        env::var_os("MARSH_DAEMON_SOCKET").is_some(),
        env::var_os("MARSH_DAEMON_TOKEN").is_some(),
    )?;
    let executable = env::current_exe()
        .map_err(|error| RunError::new(format!("cannot locate installed marsh: {error}"), 1))?;
    let workspace = env::current_dir()
        .map_err(|error| RunError::new(format!("cannot read current directory: {error}"), 1))?;
    let registration = mcp_registration(client, &executable, &workspace)?;
    if client == McpClient::Sbx {
        ensure_mcp_broker(&registration)?;
    }
    apply_mcp_registration(client, &registration)?;
    println!(
        "Registered {} with {} for {}.",
        registration.server_name,
        client.name(),
        registration.workspace.display()
    );
    if client == McpClient::Sbx {
        println!(
            "Active sandboxes keep their loaded instance; {} applies to future loads.",
            registration.server_name
        );
    }
    Ok(0)
}

/// Register only the already-published fixed export server with host Codex.
/// Codex configuration is account-wide, so this action is never available
/// through the guest relay.
fn codex_published_mcp(name: &str, remove: bool) -> Result<i32, RunError> {
    codex_published(name, remove, false)
}

fn codex_published_acp(name: &str, remove: bool) -> Result<i32, RunError> {
    codex_published(name, remove, true)
}

#[allow(clippy::too_many_lines)] // Exact Codex registration inspection and matching publication validation share one path.
fn codex_published(name: &str, remove: bool, acp: bool) -> Result<i32, RunError> {
    if relay_environment_present(
        env::var_os("MARSH_DAEMON_SOCKET").is_some(),
        env::var_os("MARSH_DAEMON_TOKEN").is_some(),
    ) {
        return Err(RunError::new(
            "published Codex registration is host-only; use a Mac terminal in the publishing project",
            2,
        ));
    }
    let (registration, home, declaration_path) = if acp {
        published_acp_context(name)?
    } else {
        published_mcp_context(name)?
    };
    let _lock = lock_publication(&declaration_path)?;
    let generation = if remove {
        None
    } else if acp {
        Some(
            marsh_mcp::AcpDeclaration::load_private(&declaration_path)
                .map_err(|error| RunError::new(format!("invalid ACP publication: {error}"), 1))?
                .generation,
        )
    } else {
        ToolDeclaration::load_from_path(&declaration_path, Some(name))
            .map_err(|error| RunError::new(format!("invalid publication: {error}"), 1))?
            .publication_generation
    };
    let sbx_add = if acp {
        published_acp_sbx_add_command(
            &registration,
            &home,
            &declaration_path,
            generation.as_deref(),
        )?
    } else {
        published_sbx_add_command_with_generation(
            &registration,
            &home,
            &declaration_path,
            generation.as_deref(),
        )?
    };
    let encoded = sbx_add
        .arguments
        .windows(2)
        .find(|pair| pair[0] == "--args")
        .and_then(|pair| pair[1].to_str())
        .ok_or_else(|| RunError::new("published export command has no arguments", 1))?;
    let export_args = encoded.split(',').collect::<Vec<_>>();
    let existing = inspect_published_codex_registration(&registration, &export_args, remove)?;
    if remove {
        if existing {
            run_checked_mcp_command(
                &McpClientCommand {
                    program: "codex".into(),
                    arguments: ["mcp", "remove", &registration.server_name]
                        .into_iter()
                        .map(std::ffi::OsString::from)
                        .collect(),
                    current_dir: registration.workspace.clone(),
                },
                "remove published Codex tool",
            )?;
        }
        println!("Removed {} from host Codex.", registration.server_name);
        return Ok(0);
    }
    let metadata = std::fs::symlink_metadata(&declaration_path)
        .map_err(|error| RunError::new(format!("publication is missing: {error}"), 1))?;
    if !metadata.file_type().is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(RunError::new(
            "publication must be an owner-only real file",
            1,
        ));
    }
    if acp {
        let declaration = marsh_mcp::AcpDeclaration::load_private(&declaration_path)
            .map_err(|error| RunError::new(format!("invalid ACP publication: {error}"), 1))?;
        if declaration.tool_name != name
            || declaration.canonical_workspace != registration.workspace
        {
            return Err(RunError::new(
                "ACP publication does not match this project",
                1,
            ));
        }
    } else {
        let declaration: ToolDeclaration = serde_json::from_slice(
            &std::fs::read(&declaration_path)
                .map_err(|error| RunError::new(format!("cannot read publication: {error}"), 1))?,
        )
        .map_err(|error| RunError::new(format!("invalid publication: {error}"), 1))?;
        declaration
            .validate()
            .map_err(|error| RunError::new(format!("invalid publication: {error}"), 1))?;
        if declaration.tool_name != name
            || declaration.canonical_workspace.as_deref() != Some(&registration.workspace)
            || declaration.workspace_identity
                != Some(
                    WorkspaceIdentity::for_directory(&registration.workspace)
                        .map_err(|error| RunError::new(error, 1))?,
                )
        {
            return Err(RunError::new("publication does not match this project", 1));
        }
    }
    if !inspect_published_sbx_registration(&registration, &sbx_add)? {
        return Err(RunError::new(
            "publication is not registered with stock SBX",
            1,
        ));
    }
    if !existing {
        let mut arguments = vec![
            "mcp".into(),
            "add".into(),
            registration.server_name.clone().into(),
            "--".into(),
            registration.marsh_mcp.clone().into_os_string(),
        ];
        arguments.extend(export_args.iter().map(std::ffi::OsString::from));
        run_checked_mcp_command(
            &McpClientCommand {
                program: "codex".into(),
                arguments,
                current_dir: registration.workspace.clone(),
            },
            "install published Codex tool",
        )?;
        if !inspect_published_codex_registration(&registration, &export_args, false)? {
            return Err(RunError::new("Codex did not retain the published tool", 1));
        }
    }
    println!(
        "Registered {} with host Codex. Start a new Codex task and verify tool discovery.",
        registration.server_name
    );
    println!(
        "Grant: Codex tasks using this account can {} in {}.",
        if acp {
            "prompt, cancel, and approve offered one-time permissions for the published ACP agent, including actions with its Kit's project write access"
        } else {
            "run the fixed pipeline with the project shell's privileges, including sudo, and read its output"
        },
        registration.workspace.display()
    );
    Ok(0)
}

fn inspect_published_codex_registration(
    registration: &McpRegistration,
    export_args: &[&str],
    allow_prior_generation: bool,
) -> Result<bool, RunError> {
    let command = McpClientCommand {
        program: "codex".into(),
        arguments: ["mcp", "get", &registration.server_name, "--json"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect(),
        current_dir: registration.workspace.clone(),
    };
    let output = run_mcp_client_command_output(&command).map_err(|error| {
        RunError::new(format!("cannot inspect Codex MCP registration: {error}"), 1)
    })?;
    if output.status.success() {
        let inspected: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| RunError::new("Codex MCP inspect returned invalid JSON", 1))?;
        let mut expected = serde_json::json!({
            "name": registration.server_name,
            "enabled": true,
            "disabled_reason": null,
            "transport": {
                "type": "stdio", "command": registration.marsh_mcp,
                "args": export_args, "env": null, "env_vars": [], "cwd": null,
            },
            "enabled_tools": null, "disabled_tools": null,
            "startup_timeout_sec": null, "tool_timeout_sec": null,
        });
        if allow_prior_generation && inspected != expected {
            let observed_args = inspected
                .pointer("/transport/args")
                .and_then(serde_json::Value::as_array);
            if let Some(args) = observed_args
                && args.len() == export_args.len() + 2
                && args
                    .iter()
                    .take(export_args.len())
                    .zip(export_args.iter())
                    .all(|(observed, expected)| observed.as_str() == Some(*expected))
                && args[export_args.len()] == "--expected-generation"
                && args
                    .last()
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|value| {
                        uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
                    })
            {
                expected["transport"]["args"] = serde_json::Value::Array(args.clone());
            }
        }
        if inspected != expected {
            return Err(RunError::new(
                "Codex MCP name is occupied by a different or modified registration; refusing to replace or remove it",
                1,
            ));
        }
        return Ok(true);
    }
    let missing = format!(
        "Error: No MCP server named '{}' found.\n",
        registration.server_name
    );
    if output.stdout.is_empty() && output.stderr == missing.as_bytes() {
        return Ok(false);
    }
    Err(RunError::new(
        format!(
            "cannot inspect Codex MCP registration: {}",
            mcp_client_diagnostic(&output)
        ),
        output.status.code().unwrap_or(1),
    ))
}

fn published_mcp_context(name: &str) -> Result<(McpRegistration, PathBuf, PathBuf), RunError> {
    published_context(name, false)
}

fn published_acp_context(name: &str) -> Result<(McpRegistration, PathBuf, PathBuf), RunError> {
    published_context(name, true)
}

fn reject_cross_protocol_name(name: &str, acp: bool) -> Result<(), RunError> {
    let (mut other, _, path) = published_context(name, acp)?;
    let name = marsh_daemon::PublishedName::parse(name).map_err(|error| RunError::new(error, 2))?;
    let (scope, owner) = marsh_daemon::PublicationScope::from_declaration_path(&path)
        .map_err(|error| RunError::new(error, 1))?;
    let opposite = scope.other_protocol();
    let other_path = opposite.declaration_in(&owner, &name);
    other.server_name = opposite.server_name(&name);
    match std::fs::symlink_metadata(other_path) {
        Ok(_) => Err(RunError::new(
            "this MCP tool name is already published by the other protocol in this scope",
            1,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let output = run_mcp_client_command_output(&McpClientCommand {
                program: other.sbx.into_os_string(),
                arguments: ["mcp", "inspect", &other.server_name, "--json"]
                    .into_iter()
                    .map(OsString::from)
                    .collect(),
                current_dir: other.workspace,
            })
            .map_err(|error| {
                RunError::new(
                    format!("cannot inspect other protocol registration: {error}"),
                    1,
                )
            })?;
            if output.status.success() {
                return Err(RunError::new(
                    "this name has an existing registration from the other protocol; unpublish it before reusing the name",
                    1,
                ));
            }
            let missing = format!(
                "error: mcp server \"{}\" not found: mcp server not found\n  try: sbx mcp ls\n",
                other.server_name
            );
            if output.stdout.is_empty() && output.stderr == missing.as_bytes() {
                Ok(())
            } else {
                Err(RunError::new(
                    format!(
                        "cannot inspect other protocol registration: {}",
                        mcp_client_diagnostic(&output)
                    ),
                    1,
                ))
            }
        }
        Err(error) => Err(RunError::new(
            format!("cannot inspect the other MCP publication: {error}"),
            1,
        )),
    }
}

fn published_context(
    name: &str,
    acp: bool,
) -> Result<(McpRegistration, PathBuf, PathBuf), RunError> {
    let published_name =
        marsh_daemon::PublishedName::parse(name).map_err(|error| RunError::new(error, 2))?;
    require_host_mcp_install(
        McpClient::Sbx,
        env::var_os("MARSH_DAEMON_SOCKET").is_some(),
        env::var_os("MARSH_DAEMON_TOKEN").is_some(),
    )?;
    let executable = env::current_exe()
        .map_err(|error| RunError::new(format!("cannot locate marsh: {error}"), 1))?;
    let workspace = env::current_dir()
        .map_err(|error| RunError::new(format!("cannot read current directory: {error}"), 1))?;
    validate_pinned_guest_project(&workspace)?;
    let mut registration = mcp_registration(McpClient::Sbx, &executable, &workspace)?;
    let mut selection = SessionConfig::from_environment(false)
        .map_err(|error| RunError::new(error.to_string(), 1))?;
    let home = selection
        .home_backing
        .parent()
        .ok_or_else(|| RunError::new("selected home has no scope root", 1))?
        .to_path_buf();
    private_directory(&home)?;
    selection
        .ensure_persistent_home()
        .map_err(|error| RunError::new(error.to_string(), 1))?;
    let guest_home = selection.home_backing;
    let home = home
        .canonicalize()
        .map_err(|error| RunError::new(format!("cannot resolve marsh home: {error}"), 1))?;
    if home.starts_with(&registration.workspace) {
        return Err(RunError::new(
            "published MCP home cannot be inside the project workspace",
            1,
        ));
    }
    let host_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| RunError::new("HOME is required for host-only MCP publications", 1))?
        .canonicalize()
        .map_err(|error| RunError::new(format!("cannot resolve host home: {error}"), 1))?;
    if host_home.starts_with(&registration.workspace) {
        return Err(RunError::new(
            "host home cannot be inside the project workspace",
            1,
        ));
    }
    let owner_root = marsh_daemon::PublicationScope::owner_root(&host_home);
    if let Some(parent) = owner_root.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            RunError::new(format!("cannot create {}: {error}", parent.display()), 1)
        })?;
    }
    private_directory(&owner_root)?;
    let resolved_owner = owner_root.canonicalize().map_err(|error| {
        RunError::new(
            format!("cannot resolve {}: {error}", owner_root.display()),
            1,
        )
    })?;
    if resolved_owner.starts_with(&registration.workspace) || resolved_owner.starts_with(&home) {
        return Err(RunError::new(
            "published MCP declarations must stay outside the workspace and marsh home",
            1,
        ));
    }
    let scope = marsh_daemon::PublicationScope::new(
        if acp {
            marsh_daemon::PublicationKind::Acp
        } else {
            marsh_daemon::PublicationKind::Mcp
        },
        &registration.workspace,
        &guest_home,
    );
    let project = scope.directory(&host_home);
    private_directory(project.parent().expect("publication protocol directory"))?;
    private_directory(&project)?;
    registration.server_name = scope.server_name(&published_name);
    Ok((
        registration,
        home,
        scope.declaration_path(&host_home, &published_name),
    ))
}

fn validate_pinned_guest_project(workspace: &Path) -> Result<Option<(u64, u64)>, RunError> {
    let path = env::var_os("MARSH_MCP_EXPECTED_PROJECT_PATH");
    let dev = env::var_os("MARSH_MCP_EXPECTED_PROJECT_DEV");
    let ino = env::var_os("MARSH_MCP_EXPECTED_PROJECT_INO");
    if path.is_none() && dev.is_none() && ino.is_none() {
        return Ok(None);
    }
    let (Some(path), Some(dev), Some(ino)) = (path, dev, ino) else {
        return Err(RunError::new("incomplete attached project identity", 1));
    };
    let expected = PathBuf::from(path);
    let expected_dev = dev
        .to_string_lossy()
        .parse::<u64>()
        .map_err(|_| RunError::new("invalid attached project device", 1))?;
    let expected_ino = ino
        .to_string_lossy()
        .parse::<u64>()
        .map_err(|_| RunError::new("invalid attached project inode", 1))?;
    validate_project_identity(workspace, &expected, expected_dev, expected_ino)?;
    Ok(Some((expected_dev, expected_ino)))
}

fn validate_project_identity(
    workspace: &Path,
    expected: &Path,
    expected_dev: u64,
    expected_ino: u64,
) -> Result<(), RunError> {
    let metadata = std::fs::symlink_metadata(workspace)
        .map_err(|error| RunError::new(format!("cannot inspect attached project: {error}"), 1))?;
    let canonical = workspace
        .canonicalize()
        .map_err(|error| RunError::new(format!("cannot resolve attached project: {error}"), 1))?;
    // Read the actual process cwd as well as its logical pathname. A child
    // can still hold the original directory after that pathname is replaced.
    let cwd = std::fs::metadata(".")
        .map_err(|error| RunError::new(format!("cannot inspect process project: {error}"), 1))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || workspace != expected
        || canonical != expected
        || metadata.dev() != expected_dev
        || metadata.ino() != expected_ino
        || cwd.dev() != expected_dev
        || cwd.ino() != expected_ino
    {
        return Err(RunError::new(
            "attached project identity changed; MCP operation denied",
            1,
        ));
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), RunError> {
    use std::os::unix::fs::DirBuilderExt as _;
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(RunError::new(
                format!(
                    "cannot create private MCP directory {}: {error}",
                    path.display()
                ),
                1,
            ));
        }
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        RunError::new(
            format!(
                "cannot inspect private MCP directory {}: {error}",
                path.display()
            ),
            1,
        )
    })?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(RunError::new(
            format!(
                "MCP directory must be an owner-only real directory: {}",
                path.display()
            ),
            1,
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep the host publication transaction under one lock.
fn publish_mcp(
    name: &str,
    description: Option<&str>,
    sandbox: Option<&str>,
    pipeline: &str,
    transaction: &mut mcp_load::HostPublication,
) -> Result<i32, RunError> {
    let kit = transaction
        .context()
        .and_then(|context| context.kit.clone());
    marsh_daemon::validate_publication_options(description, kit.as_deref(), sandbox, false)
        .map_err(|error| RunError::new(error, 2))?;
    transaction.bind_arguments(sandbox, None)?;
    let workspace = env::current_dir().map_err(|error| RunError::new(error.to_string(), 1))?;
    let declaration = published_tool_declaration(name, description, pipeline, &workspace)?;
    let encoded = serde_json::to_vec(&declaration)
        .map_err(|error| RunError::new(format!("cannot encode MCP publication: {error}"), 1))?;
    let (registration, home, declaration_path) = published_mcp_context(name)?;
    marsh_mcp::HostConfig::new(
        &registration.workspace,
        &home,
        &registration.marsh,
        &registration.sbx,
        false,
    )
    .map_err(|error| RunError::new(format!("cannot prepare MCP runtime: {error}"), 1))?;
    let _publication_lock = transaction.lock_scope(&declaration_path)?;
    mcp_load::check_revocation(&declaration_path)?;
    reject_cross_protocol_name(name, false)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&declaration_path)
        && (!metadata.file_type().is_file()
            || metadata.uid() != nix::unistd::Uid::effective().as_raw()
            || metadata.mode() & 0o077 != 0)
    {
        return Err(RunError::new(
            "existing MCP publication must be an owner-only real file",
            1,
        ));
    }
    let previous = match std::fs::read(&declaration_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(RunError::new(
                format!("cannot read existing MCP publication: {error}"),
                1,
            ));
        }
    };
    let previous_generation = previous
        .as_deref()
        .map(|bytes| {
            let text = std::str::from_utf8(bytes).map_err(|error| {
                RunError::new(format!("invalid existing MCP publication: {error}"), 1)
            })?;
            let previous = ToolDeclaration::load_from_str(text, Some(name)).map_err(|error| {
                RunError::new(format!("invalid existing MCP publication: {error}"), 1)
            })?;
            Ok::<_, RunError>(previous.publication_generation)
        })
        .transpose()?
        .flatten();
    let old_add = published_sbx_add_command_with_generation(
        &registration,
        &home,
        &declaration_path,
        previous_generation.as_deref(),
    )?;
    let add_command = published_sbx_add_command_with_generation(
        &registration,
        &home,
        &declaration_path,
        declaration.publication_generation.as_deref(),
    )?;
    let registration_exists =
        inspect_published_sbx_registration_with_prior_generation(&registration, &old_add, true)?;
    if registration_exists && previous.is_none() {
        return Err(RunError::new(
            "MCP registration exists without its owner declaration; refusing to replace it",
            1,
        ));
    }
    validate_publication_target(&registration, sandbox)?;
    // Admission and exact prior-registration validation precede cold preparation.
    // The host scope lock remains held through the private daemon handshake.
    let prepared = kit
        .as_deref()
        .map(|kit| transaction.prepare_kit(kit))
        .transpose()?;
    let sandbox = prepared.as_deref().or(sandbox);
    if prepared.is_some() {
        validate_publication_target(&registration, sandbox)?;
    }
    transaction.verify_project(&registration.workspace)?;
    mcp_load::check_revocation(&declaration_path)?;
    if std::fs::read(&declaration_path).ok() != previous {
        return Err(RunError::new(
            "MCP declaration changed during preparation; not published",
            1,
        ));
    }
    if prepared.is_some()
        && inspect_published_sbx_registration_with_prior_generation(&registration, &old_add, true)?
            != registration_exists
    {
        return Err(RunError::new(
            "MCP registration changed during preparation; not published",
            1,
        ));
    }
    transaction.begin_commit()?;
    write_publication(&declaration_path, &encoded)?;
    if registration_exists && let Err(error) = remove_sbx_registration(&registration) {
        rollback_replaced_publication(
            &registration,
            &declaration_path,
            previous.as_deref(),
            &old_add,
            None,
            true,
            transaction,
        )
        .map_err(|rollback| {
            RunError::new(
                format!("{}; rollback failed: {}", error.message, rollback.message),
                error.status,
            )
        })?;
        return Err(error);
    }
    if let Err(error) = run_checked_mcp_command(&add_command, "register published MCP tool") {
        rollback_replaced_publication(
            &registration,
            &declaration_path,
            previous.as_deref(),
            &old_add,
            Some(&add_command),
            registration_exists,
            transaction,
        )
        .map_err(|rollback| {
            RunError::new(
                format!("{}; rollback failed: {}", error.message, rollback.message),
                error.status,
            )
        })?;
        return Err(error);
    }
    if let Some(sandbox) = sandbox {
        let command = published_load_command(&registration, sandbox);
        if let Err(error) = run_checked_mcp_command(&command, "load published MCP tool") {
            rollback_replaced_publication(
                &registration,
                &declaration_path,
                previous.as_deref(),
                &old_add,
                Some(&add_command),
                registration_exists,
                transaction,
            )
            .map_err(|rollback| {
                RunError::new(
                    format!("{}; rollback failed: {}", error.message, rollback.message),
                    error.status,
                )
            })?;
            return Err(error);
        }
    }
    transaction.message(format!("Published MCP tool: {name}"));
    transaction.message(format!("Server: {}", registration.server_name));
    if let Some(sandbox) = sandbox {
        transaction.message(format!("Loaded into sandbox: {sandbox}"));
        transaction.message("Every client in that sandbox, including later jobs loaded there, can use it. Start a new agent session to discover it.");
    } else if transaction.context().is_some() {
        transaction.message("Available to agent Kits: every Kit VM created from now on loads it before its first job.");
        transaction.message(format!(
            "Kit VMs already running are not changed. Load one now with: mcp load {name} --kit KIT, or recreate it with: marsh workers reset KIT"
        ));
    } else {
        transaction.message("Registered for this shell scope. Load it into a sandbox to use it.");
    }
    transaction.message("Access: loaded clients can run this fixed pipeline with the project shell's privileges, including sudo and its private Docker Engine, and read its output.");
    if previous.is_some() {
        transaction.message("Existing loaded clients must reload this MCP tool; until then calls fail. The export server does not send tools/list_changed notifications.");
    }
    if sandbox.is_none() {
        transaction.message(format!(
            "Load it into a running sandbox with: {}mcp load {name} --sandbox SANDBOX",
            transaction.prefix()
        ));
    }
    transaction.message(format!(
        "Revoke: {}mcp unpublish {name}",
        transaction.prefix()
    ));
    Ok(0)
}

fn validate_mcp_publish_options(
    description: Option<&str>,
    sandbox: Option<&str>,
) -> Result<(), RunError> {
    marsh_daemon::validate_publication_options(description, None, sandbox, false)
        .map_err(|error| RunError::new(error, 2))
}

fn published_tool_declaration(
    name: &str,
    description: Option<&str>,
    pipeline: &str,
    workspace: &Path,
) -> Result<ToolDeclaration, RunError> {
    let declaration = ToolDeclaration {
        schema_version: "marsh.published_tool/v1".into(),
        tool_name: name.into(),
        description: description.map_or_else(
            || format!("Run the {name} shell pipeline in its publishing marsh project"),
            str::to_owned,
        ),
        publication_generation: Some(uuid::Uuid::new_v4().to_string()),
        command: String::new(),
        pipeline: Some(pipeline.into()),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "additionalProperties": false
        }),
        bindings: ToolBindings {
            argv: Vec::new(),
            stdin: Some(StdinBinding {
                field: "input".into(),
                max_bytes: 65_536,
            }),
        },
        max_output_bytes: 262_144,
        timeout_ms: 120_000,
        kit_identity: None,
        canonical_workspace: Some(workspace.to_path_buf()),
        workspace_identity: Some(
            WorkspaceIdentity::for_directory(workspace).map_err(|error| RunError::new(error, 1))?,
        ),
    };
    declaration
        .validate()
        .map_err(|error| RunError::new(format!("invalid MCP publication: {error}"), 2))?;
    Ok(declaration)
}

fn write_publication(path: &Path, encoded: &[u8]) -> Result<(), RunError> {
    let parent = path
        .parent()
        .ok_or_else(|| RunError::new("invalid publication path", 1))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| RunError::new(format!("cannot create MCP declaration: {error}"), 1))?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .and_then(|()| temporary.write_all(encoded))
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| RunError::new(format!("cannot write MCP declaration: {error}"), 1))?;
    temporary.persist(path).map_err(|error| {
        RunError::new(
            format!("cannot publish MCP declaration: {}", error.error),
            1,
        )
    })?;
    Ok(())
}

fn lock_publication(path: &Path) -> Result<std::fs::File, RunError> {
    // One admission lock per project/home, shared by ACP and MCP. Fixed order:
    // daemon name lock (when attached), then this scope lock. Never unlink a
    // lock file: unlinking would allow two holders of different lock inodes.
    let (scope, owner) = marsh_daemon::PublicationScope::from_declaration_path(path)
        .map_err(|error| RunError::new(error, 1))?;
    let lock_path = scope.lock_path(&owner);
    private_directory(lock_path.parent().expect("publication lock directory"))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| RunError::new(format!("cannot open MCP publication lock: {error}"), 1))?;
    let metadata = file.metadata().map_err(|error| {
        RunError::new(format!("cannot inspect MCP publication lock: {error}"), 1)
    })?;
    if !metadata.file_type().is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(RunError::new(
            "MCP publication lock must be an owner-only real file",
            1,
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(RunError::new("MCP publication is busy; retry", 1));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(RunError::new(
                    format!("cannot lock MCP publication: {error}"),
                    1,
                ));
            }
        }
    }
}

fn rollback_publication(
    registration: &McpRegistration,
    path: &Path,
    previous: Option<&[u8]>,
    registration_added: bool,
) -> Result<(), RunError> {
    if let Some(previous) = previous {
        write_publication(path, previous)?;
    } else if path.exists() {
        std::fs::remove_file(path).map_err(|error| {
            RunError::new(format!("cannot revoke failed MCP publication: {error}"), 1)
        })?;
    }
    if registration_added {
        remove_sbx_registration(registration)?;
    }
    Ok(())
}

fn rollback_replaced_publication(
    registration: &McpRegistration,
    path: &Path,
    previous: Option<&[u8]>,
    old_add: &McpClientCommand,
    new_add: Option<&McpClientCommand>,
    old_registered: bool,
    transaction: &mut mcp_load::HostPublication,
) -> Result<(), RunError> {
    transaction.begin_rollback()?;
    rollback_publication(registration, path, previous, false)?;
    if let Some(new_add) = new_add
        && inspect_published_sbx_registration(registration, new_add)?
    {
        remove_sbx_registration(registration)?;
    }
    if old_registered && !inspect_published_sbx_registration(registration, old_add)? {
        run_checked_mcp_command(old_add, "restore previous MCP registration")?;
    }
    Ok(())
}

fn inspect_published_sbx_registration(
    registration: &McpRegistration,
    add_command: &McpClientCommand,
) -> Result<bool, RunError> {
    inspect_published_sbx_registration_with_prior_generation(registration, add_command, false)
}

fn inspect_published_sbx_registration_with_prior_generation(
    registration: &McpRegistration,
    add_command: &McpClientCommand,
    allow_prior_generation: bool,
) -> Result<bool, RunError> {
    let command = McpClientCommand {
        program: registration.sbx.clone().into_os_string(),
        arguments: ["mcp", "inspect", &registration.server_name, "--json"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect(),
        current_dir: registration.workspace.clone(),
    };
    let output = run_mcp_client_command_output(&command)
        .map_err(|error| RunError::new(format!("cannot inspect MCP registration: {error}"), 1))?;
    if output.status.success() {
        let inspected: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|_| RunError::new("MCP registration inspect returned invalid JSON", 1))?;
        let executable = registration.marsh_mcp.to_str().ok_or_else(|| {
            RunError::new("non-UTF-8 marsh-mcp executable cannot be inspected", 1)
        })?;
        let encoded_args = add_command
            .arguments
            .iter()
            .position(|argument| argument.to_str() == Some("--args"))
            .and_then(|index| add_command.arguments.get(index + 1))
            .and_then(|argument| argument.to_str())
            .ok_or_else(|| RunError::new("published MCP registration has no args", 1))?;
        let expected_command: Vec<&str> = std::iter::once(executable)
            .chain(encoded_args.split(','))
            .collect();
        let stable_prefix = if expected_command.len() >= 2
            && expected_command[expected_command.len() - 2] == "--expected-generation"
        {
            &expected_command[..expected_command.len() - 2]
        } else {
            &expected_command[..]
        };
        let command_matches = inspected.get("command")
            == Some(&serde_json::json!(expected_command))
            || (allow_prior_generation
                && inspected
                    .get("command")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|args| {
                        args.len() >= stable_prefix.len()
                            && args
                                .iter()
                                .take(stable_prefix.len())
                                .zip(stable_prefix)
                                .all(|(observed, expected)| observed.as_str() == Some(*expected))
                            && (args.len() == stable_prefix.len()
                                || (args.len() == stable_prefix.len() + 2
                                    && args[stable_prefix.len()] == "--expected-generation"
                                    && args
                                        .last()
                                        .and_then(serde_json::Value::as_str)
                                        .is_some_and(|value| {
                                            uuid::Uuid::parse_str(value)
                                                .is_ok_and(|parsed| parsed.to_string() == value)
                                        })))
                    }));
        if inspected.get("name").and_then(serde_json::Value::as_str)
            != Some(registration.server_name.as_str())
            || inspected.get("type").and_then(serde_json::Value::as_str) != Some("local")
            || inspected
                .get("resolved_command")
                .and_then(serde_json::Value::as_str)
                != Some(executable)
            || !command_matches
        {
            return Err(RunError::new(
                "existing MCP registration does not match this publication's executable and arguments; registration is stale or owned by another publisher. Republish with `mcp publish NAME -- 'PIPELINE'`; if still refused, inspect the conflicting stock registration before removing it",
                1,
            ));
        }
        return Ok(true);
    }
    let missing = format!(
        "error: mcp server \"{}\" not found: mcp server not found\n  try: sbx mcp ls\n",
        registration.server_name
    );
    if output.stdout.is_empty() && output.stderr == missing.as_bytes() {
        return Ok(false);
    }
    Err(RunError::new(
        format!(
            "cannot inspect MCP registration: {}",
            mcp_client_diagnostic(&output)
        ),
        output.status.code().unwrap_or(1),
    ))
}

fn published_sbx_add_command_with_generation(
    registration: &McpRegistration,
    home: &Path,
    declaration_path: &Path,
    generation: Option<&str>,
) -> Result<McpClientCommand, RunError> {
    published_sbx_add_command_with_mode(
        registration,
        home,
        declaration_path,
        "export-serve",
        generation,
    )
}

fn published_acp_sbx_add_command(
    registration: &McpRegistration,
    home: &Path,
    declaration_path: &Path,
    generation: Option<&str>,
) -> Result<McpClientCommand, RunError> {
    published_sbx_add_command_with_mode(
        registration,
        home,
        declaration_path,
        "acp-export-serve",
        generation,
    )
}

fn published_sbx_add_command_with_mode(
    registration: &McpRegistration,
    home: &Path,
    declaration_path: &Path,
    mode: &str,
    generation: Option<&str>,
) -> Result<McpClientCommand, RunError> {
    let mut arguments = vec![
        mode,
        "--workspace",
        registration
            .workspace
            .to_str()
            .ok_or_else(|| RunError::new("non-UTF-8 workspace", 1))?,
        "--home",
        home.to_str()
            .ok_or_else(|| RunError::new("non-UTF-8 MCP home", 1))?,
        "--marsh",
        registration
            .marsh
            .to_str()
            .ok_or_else(|| RunError::new("non-UTF-8 marsh path", 1))?,
        "--sbx",
        registration
            .sbx
            .to_str()
            .ok_or_else(|| RunError::new("non-UTF-8 sbx path", 1))?,
        "--declaration",
        declaration_path
            .to_str()
            .ok_or_else(|| RunError::new("non-UTF-8 declaration path", 1))?,
    ];
    if let Some(generation) = generation {
        arguments.extend(["--expected-generation", generation]);
    }
    if arguments.iter().any(|value| value.contains(',')) {
        return Err(RunError::new(
            "SBX cannot encode a comma in MCP server arguments",
            2,
        ));
    }
    Ok(McpClientCommand {
        program: registration.sbx.clone().into_os_string(),
        arguments: vec![
            "mcp".into(),
            "add".into(),
            registration.server_name.clone().into(),
            "--command".into(),
            registration.marsh_mcp.clone().into_os_string(),
            "--args".into(),
            arguments.join(",").into(),
            "--dir".into(),
            registration.workspace.clone().into_os_string(),
        ],
        current_dir: registration.workspace.clone(),
    })
}

fn published_load_command(registration: &McpRegistration, sandbox: &str) -> McpClientCommand {
    McpClientCommand {
        program: registration.sbx.clone().into_os_string(),
        arguments: marsh_daemon::publication_load_arguments(&registration.server_name, sandbox)
            .into_iter()
            .map(OsString::from)
            .collect(),
        current_dir: registration.workspace.clone(),
    }
}

fn validate_publication_target(
    registration: &McpRegistration,
    sandbox: Option<&str>,
) -> Result<(), RunError> {
    if let Some(sandbox) = sandbox {
        let output = run_mcp_client_command_output(&McpClientCommand {
            program: registration.sbx.clone().into_os_string(),
            arguments: ["inspect", "--json", sandbox]
                .into_iter()
                .map(OsString::from)
                .collect(),
            current_dir: registration.workspace.clone(),
        })
        .map_err(|error| RunError::new(format!("cannot inspect target sandbox: {error}"), 1))?;
        marsh_daemon::validate_publication_sandbox_output(
            sandbox,
            &marsh_daemon::PublicationStockOutput {
                exit_code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            },
        )
        .map_err(|error| RunError::new(error, 2))?;
    }
    Ok(())
}

fn run_checked_mcp_command(command: &McpClientCommand, action: &str) -> Result<(), RunError> {
    let output = run_mcp_client_command_output(command)
        .map_err(|error| RunError::new(format!("cannot {action}: {error}"), 1))?;
    if !output.status.success() {
        return Err(RunError::new(
            format!("cannot {action}: {}", mcp_client_diagnostic(&output)),
            output.status.code().unwrap_or(1),
        ));
    }
    Ok(())
}

fn unpublish_mcp(name: &str, transaction: &mut mcp_load::HostPublication) -> Result<i32, RunError> {
    transaction.bind_arguments(None, None)?;
    let (registration, home, declaration_path) = published_mcp_context(name)?;
    transaction.begin_commit()?;
    marsh_daemon::mark_mcp_revocation_pending(&declaration_path)
        .map_err(|error| RunError::new(error, 1))?;
    let _publication_lock = transaction.lock_scope(&declaration_path)?;
    let generation = match ToolDeclaration::load_from_path(&declaration_path, Some(name)) {
        Ok(declaration) => declaration.publication_generation,
        Err(_error) if !declaration_path.exists() => None,
        Err(error) => {
            return Err(RunError::new(
                format!("invalid MCP publication: {error}"),
                1,
            ));
        }
    };
    let add_command = published_sbx_add_command_with_generation(
        &registration,
        &home,
        &declaration_path,
        generation.as_deref(),
    )?;
    let registered = inspect_published_sbx_registration_with_prior_generation(
        &registration,
        &add_command,
        true,
    )?;
    // A retry after a partial revocation still has the pending marker or the
    // host registration; with neither, this name was never published.
    if !registered
        && std::fs::symlink_metadata(&declaration_path).is_err()
        && std::fs::symlink_metadata(declaration_path.with_extension("revoke")).is_err()
    {
        return Err(RunError::new(format!("{name} is not published"), 1));
    }
    match std::fs::remove_file(&declaration_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RunError::new(
                format!("cannot revoke published MCP tool {name}: {error}"),
                1,
            ));
        }
    }
    if registered {
        remove_sbx_registration(&registration)?;
    }
    match std::fs::remove_file(declaration_path.with_extension("revoke")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RunError::new(
                format!("revoked, but pending marker cleanup failed: {error}; retry unpublish"),
                1,
            ));
        }
    }
    transaction.message(format!("Revoked {name}. Loaded clients may still show the tool, but its calls are denied; remove the stale tool from those clients."));
    Ok(0)
}

fn start_mcp_broker() -> Result<i32, RunError> {
    require_host_mcp_install(
        McpClient::Sbx,
        env::var_os("MARSH_DAEMON_SOCKET").is_some(),
        env::var_os("MARSH_DAEMON_TOKEN").is_some(),
    )?;
    let executable = env::current_exe()
        .map_err(|error| RunError::new(format!("cannot locate installed marsh: {error}"), 1))?;
    let workspace = env::current_dir()
        .map_err(|error| RunError::new(format!("cannot read current directory: {error}"), 1))?;
    let registration = mcp_registration(McpClient::Sbx, &executable, &workspace)?;
    ensure_mcp_broker(&registration)?;
    println!(
        "Shared sbx MCP broker is ready for {}.",
        registration.workspace.display()
    );
    Ok(0)
}

fn stop_mcp_broker() -> Result<i32, RunError> {
    require_host_mcp_install(
        McpClient::Sbx,
        env::var_os("MARSH_DAEMON_SOCKET").is_some(),
        env::var_os("MARSH_DAEMON_TOKEN").is_some(),
    )?;
    let executable = env::current_exe()
        .map_err(|error| RunError::new(format!("cannot locate installed marsh: {error}"), 1))?;
    let workspace = env::current_dir()
        .map_err(|error| RunError::new(format!("cannot read current directory: {error}"), 1))?;
    let registration = mcp_registration(McpClient::Sbx, &executable, &workspace)?;
    let command = registration.broker_stop_command();
    let output = run_mcp_client_command_output(&command).map_err(|error| {
        RunError::new(format!("cannot stop the shared sbx MCP broker: {error}"), 1)
    })?;
    if !output.status.success() {
        return Err(RunError::new(
            format!(
                "failed to stop the shared sbx MCP broker {}: {}",
                registration.server_name,
                mcp_client_diagnostic(&output)
            ),
            output.status.code().unwrap_or(1),
        ));
    }
    println!(
        "Shared sbx MCP broker is stopped for {}.",
        registration.workspace.display()
    );
    Ok(0)
}

fn ensure_mcp_broker(registration: &McpRegistration) -> Result<(), RunError> {
    let command = registration.broker_start_command();
    let output = run_mcp_client_command_output(&command).map_err(|error| {
        RunError::new(
            format!(
                "cannot start the shared sbx MCP broker {}: {error}",
                registration.server_name
            ),
            1,
        )
    })?;
    if !output.status.success() {
        return Err(RunError::new(
            format!(
                "failed to start the shared sbx MCP broker {}: {}",
                registration.server_name,
                mcp_client_diagnostic(&output)
            ),
            output.status.code().unwrap_or(1),
        ));
    }
    Ok(())
}

fn apply_mcp_registration(
    client: McpClient,
    registration: &McpRegistration,
) -> Result<(), RunError> {
    if client == McpClient::Sbx {
        remove_sbx_registration(registration)?;
    }

    let add = registration.client_command(client)?;
    let status = run_mcp_client_command(&add).map_err(|error| {
        RunError::new(
            format!(
                "cannot add {} MCP registration {}: {error}",
                client.name(),
                registration.server_name
            ),
            1,
        )
    })?;
    if !status.success() {
        return Err(RunError::new(
            format!(
                "failed to add {} MCP registration {}",
                client.name(),
                registration.server_name
            ),
            status.code().unwrap_or(1),
        ));
    }
    Ok(())
}

fn remove_sbx_registration(registration: &McpRegistration) -> Result<(), RunError> {
    let remove = McpClientCommand {
        program: registration.sbx.clone().into_os_string(),
        arguments: ["mcp", "rm", "--force", &registration.server_name]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect(),
        current_dir: registration.workspace.clone(),
    };
    let output = run_mcp_client_command_output(&remove).map_err(|error| {
        RunError::new(
            format!(
                "cannot remove sbx MCP registration {}: {error}",
                registration.server_name
            ),
            1,
        )
    })?;
    if !output.status.success()
        && !exact_missing_sbx_registration(&output, &registration.server_name)
    {
        return Err(RunError::new(
            format!(
                "failed to remove sbx MCP registration {}: {}",
                registration.server_name,
                mcp_client_diagnostic(&output)
            ),
            output.status.code().unwrap_or(1),
        ));
    }
    Ok(())
}

fn run_mcp_client_command_output(
    command: &McpClientCommand,
) -> std::io::Result<std::process::Output> {
    const TIMEOUT: Duration = Duration::from_mins(1);
    const LIMIT: usize = 1024 * 1024;
    if let Some(result) = mcp_load::stock_command(command) {
        return result;
    }
    let mut child = marsh_runtime::with_host_descriptor_creation_excluded(|| {
        Command::new(&command.program)
            .args(&command.arguments)
            .current_dir(&command.current_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
    })?;
    let (out_tx, out_rx) = mpsc::sync_channel(1);
    let (err_tx, err_rx) = mpsc::sync_channel(1);
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    thread::spawn(move || {
        let _ = out_tx.send(read_mcp_stream(stdout, LIMIT));
    });
    thread::spawn(move || {
        let _ = err_tx.send(read_mcp_stream(stderr, LIMIT));
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                terminate_mcp_client(&mut child);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "stock MCP command timed out; registration state is uncertain",
                ));
            }
            Err(error) => {
                terminate_mcp_client(&mut child);
                return Err(error);
            }
        }
    };
    let stdout = out_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
        terminate_mcp_client(&mut child);
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "stock MCP stdout did not close; registration state is uncertain",
        )
    })??;
    let stderr = err_rx.recv_timeout(Duration::from_secs(2)).map_err(|_| {
        terminate_mcp_client(&mut child);
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "stock MCP stderr did not close; registration state is uncertain",
        )
    })??;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn read_mcp_stream(mut stream: impl std::io::Read, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let size = stream.read(&mut buffer)?;
        if size == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(size) > limit {
            return Err(std::io::Error::other(
                "stock MCP output exceeded 1 MiB; registration state is uncertain",
            ));
        }
        output.extend_from_slice(&buffer[..size]);
    }
}

fn terminate_mcp_client(child: &mut std::process::Child) {
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    if let Ok(id) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(id), Signal::SIGTERM);
        thread::sleep(Duration::from_millis(250));
        let _ = killpg(Pid::from_raw(id), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn exact_missing_sbx_registration(output: &std::process::Output, name: &str) -> bool {
    if output.status.success() || !output.stdout.is_empty() {
        return false;
    }
    let expected = format!("error: MCP server \"{name}\" not found");
    output.stderr == expected.as_bytes() || output.stderr == format!("{expected}\n").as_bytes()
}

fn mcp_client_diagnostic(output: &std::process::Output) -> String {
    let stderr = compact_mcp_client_stream(&output.stderr);
    if !stderr.is_empty() {
        return format!("stderr: {stderr}");
    }
    let stdout = compact_mcp_client_stream(&output.stdout);
    if !stdout.is_empty() {
        return format!("stdout: {stdout}");
    }
    "no diagnostic".into()
}

fn compact_mcp_client_stream(bytes: &[u8]) -> String {
    const LIMIT: usize = 2 * 1024;
    let retained = &bytes[..bytes.len().min(LIMIT)];
    let mut rendered = String::from_utf8_lossy(retained).trim().to_owned();
    if bytes.len() > retained.len() {
        rendered.push_str(" [truncated]");
    }
    rendered
}

fn run_mcp_client_command(command: &McpClientCommand) -> std::io::Result<std::process::ExitStatus> {
    let output = run_mcp_client_command_output(command)?;
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;
    Ok(output.status)
}

fn require_host_mcp_install(
    client: McpClient,
    relay_socket: bool,
    relay_token: bool,
) -> Result<(), RunError> {
    if relay_environment_present(relay_socket, relay_token) {
        return Err(RunError::new(
            format!(
                "{} MCP registration is host-only; run `marsh mcp install {}` from macOS",
                client.name(),
                client.name()
            ),
            2,
        ));
    }
    Ok(())
}

fn mcp_registration(
    client: McpClient,
    marsh_executable: &Path,
    workspace: &Path,
) -> Result<McpRegistration, RunError> {
    mcp_registration_with(client, marsh_executable, workspace, || {
        resolve_stock_sbx().map_err(|error| RunError::new(error.to_string(), 1))
    })
}

fn mcp_registration_with(
    client: McpClient,
    marsh_executable: &Path,
    workspace: &Path,
    resolve_sbx: impl FnOnce() -> Result<PathBuf, RunError>,
) -> Result<McpRegistration, RunError> {
    let marsh = require_installed_executable(marsh_executable, "marsh")?;
    let workspace = workspace.canonicalize().map_err(|error| {
        RunError::new(
            format!("cannot resolve workspace {}: {error}", workspace.display()),
            1,
        )
    })?;
    let bin = marsh.parent().ok_or_else(|| {
        RunError::new(
            format!("installed marsh has no parent: {}", marsh.display()),
            1,
        )
    })?;
    let marsh_mcp = require_installed_executable(&bin.join("marsh-mcp"), "marsh-mcp")?;
    let sbx = require_installed_executable(&resolve_sbx()?, "sbx")?;
    let key = workspace_key(&workspace);
    let scope_root = client_scope_root(client, &workspace)?;
    Ok(McpRegistration {
        server_name: format!("marsh-dev-{}", &key[..12]),
        workspace,
        scope_root,
        marsh_mcp,
        marsh,
        sbx,
    })
}

fn require_installed_executable(path: &Path, label: &str) -> Result<PathBuf, RunError> {
    let canonical = path.canonicalize().map_err(|error| {
        RunError::new(
            format!(
                "cannot locate {label} at {}; install it before continuing: {error}",
                path.display()
            ),
            1,
        )
    })?;
    let metadata = std::fs::symlink_metadata(&canonical).map_err(|error| {
        RunError::new(
            format!("cannot inspect {label} at {}: {error}", canonical.display()),
            1,
        )
    })?;
    if !metadata.file_type().is_file() || !metadata_is_executable(&metadata) {
        return Err(RunError::new(
            format!(
                "{label} is not an executable regular file: {}",
                canonical.display()
            ),
            1,
        ));
    }
    Ok(canonical)
}

#[cfg(unix)]
fn metadata_is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn metadata_is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

fn workspace_key(workspace: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(workspace.as_os_str().as_encoded_bytes())
    )
}

fn client_scope_root(client: McpClient, workspace: &Path) -> Result<PathBuf, RunError> {
    let host_home = env::var_os("HOME").map(PathBuf::from);
    let codex_home = env::var_os("CODEX_HOME").map(PathBuf::from);
    let claude_config = env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let xdg_data = env::var_os("XDG_DATA_HOME").map(PathBuf::from);
    client_scope_root_with(
        client,
        workspace,
        host_home.as_deref(),
        codex_home.as_deref(),
        claude_config.as_deref(),
        xdg_data.as_deref(),
    )
}

fn client_scope_root_with(
    client: McpClient,
    workspace: &Path,
    host_home: Option<&Path>,
    codex_home: Option<&Path>,
    claude_config: Option<&Path>,
    xdg_data: Option<&Path>,
) -> Result<PathBuf, RunError> {
    let required_home =
        || host_home.ok_or_else(|| RunError::new("HOME is required to register marsh MCP", 1));
    let (base, label) = match client {
        McpClient::Codex => {
            if let Some(home) = codex_home {
                (home.to_owned(), "CODEX_HOME")
            } else {
                (required_home()?.join(".codex"), "HOME")
            }
        }
        McpClient::Claude => {
            if let Some(home) = claude_config {
                (home.to_owned(), "CLAUDE_CONFIG_DIR")
            } else {
                (required_home()?.join(".claude"), "HOME")
            }
        }
        McpClient::Sbx => {
            let home = required_home()?;
            if !home.is_absolute() {
                return Err(RunError::new(
                    "HOME must be absolute to register marsh MCP",
                    1,
                ));
            }
            let data_root = if cfg!(target_os = "macos") {
                marsh_mcp::mcp_scope_base(home)
            } else if let Some(xdg) = xdg_data {
                if !xdg.is_absolute() {
                    return Err(RunError::new(
                        "XDG_DATA_HOME must be absolute to register marsh MCP",
                        1,
                    ));
                }
                xdg.join(marsh_mcp::MCP_SCOPES_DIR)
            } else {
                home.join(".local/share").join(marsh_mcp::MCP_SCOPES_DIR)
            };
            let key = workspace_key(workspace);
            return Ok(data_root.join("clients").join(client.name()).join(key));
        }
    };
    if !base.is_absolute() {
        return Err(RunError::new(
            format!("{label} must be absolute to register marsh MCP"),
            1,
        ));
    }
    let key = workspace_key(workspace);
    Ok(base
        .join("marsh")
        .join("dev-mcp")
        .join(client.name())
        .join(key))
}

fn run_mcp_server(arguments: &[String]) -> Result<i32, RunError> {
    if relay_environment_present(
        std::env::var_os("MARSH_DAEMON_SOCKET").is_some(),
        std::env::var_os("MARSH_DAEMON_TOKEN").is_some(),
    ) {
        return Err(RunError::new(
            "the MCP server is host-only; start `marsh mcp serve` from a macOS host process, not through the project-shell relay",
            2,
        ));
    }
    let executable = std::env::current_exe()
        .map_err(|error| RunError::new(format!("cannot locate marsh: {error}"), 1))?;
    run_mcp_sibling(&executable, arguments)
}

fn relay_environment_present(socket: bool, token: bool) -> bool {
    socket || token
}

fn run_mcp_sibling(marsh_executable: &Path, arguments: &[String]) -> Result<i32, RunError> {
    let directory = marsh_executable.parent().ok_or_else(|| {
        RunError::new(
            format!(
                "cannot locate the MCP server beside {}",
                marsh_executable.display()
            ),
            1,
        )
    })?;
    let server = directory.join("marsh-mcp");
    run_mcp_command(&server, arguments)
}

#[cfg(unix)]
fn run_mcp_command(server: &Path, arguments: &[String]) -> Result<i32, RunError> {
    use std::os::unix::process::CommandExt as _;
    let error = Command::new(server).args(arguments).exec();
    Err(mcp_start_error(server, &error))
}

#[cfg(not(unix))]
fn run_mcp_command(server: &Path, arguments: &[String]) -> Result<i32, RunError> {
    let status = Command::new(server)
        .args(arguments)
        .status()
        .map_err(|error| mcp_start_error(server, &error))?;
    Ok(status.code().unwrap_or(1))
}

fn mcp_start_error(server: &Path, error: &std::io::Error) -> RunError {
    RunError::new(
        format!(
            "cannot start the installed MCP server at {}: {error}",
            server.display()
        ),
        1,
    )
}

fn with_session_home<T>(
    session: SessionConfig,
    activate_ephemeral_home: bool,
    operation: impl FnOnce(SessionConfig, Option<marsh_sbx::EphemeralHomeToken>) -> Result<T, RunError>,
) -> Result<T, RunError> {
    with_session_home_allocator(
        session,
        activate_ephemeral_home,
        SessionConfig::activate_ephemeral_home,
        operation,
    )
}

fn with_session_home_allocator<T>(
    mut session: SessionConfig,
    activate_ephemeral_home: bool,
    allocate: impl FnOnce(
        &mut SessionConfig,
    ) -> Result<EphemeralHomeGuard, Box<dyn std::error::Error + Send + Sync>>,
    operation: impl FnOnce(SessionConfig, Option<marsh_sbx::EphemeralHomeToken>) -> Result<T, RunError>,
) -> Result<T, RunError> {
    let ephemeral_home = if activate_ephemeral_home {
        Some(allocate(&mut session).map_err(|error| RunError::new(error.to_string(), 125))?)
    } else {
        None
    };
    let token = ephemeral_home.as_ref().map(EphemeralHomeGuard::token);
    match operation(session, token) {
        Ok(value) => {
            if let Some(home) = ephemeral_home {
                home.release().map_err(|error| RunError::new(format!("ephemeral HOME retained at {}: {error}; use explicit retained-slot recovery after all shell/Kit UUID owners are removed", home.path().display()), 125))?;
            }
            Ok(value)
        }
        Err(mut error) => {
            // A failed shell teardown may leave a saved SBX mount. Keep its
            // source present so stock SBX can still boot for safe revocation.
            if let Some(home) = ephemeral_home {
                if let Err(close) = home.close_admission() {
                    let _ = write!(
                        error.message,
                        "; ephemeral admission revocation remains uncertain: {close}"
                    );
                }
                let path = home.keep();
                let _ = write!(
                    error.message,
                    "; ephemeral HOME retained at {} because teardown was not confirmed",
                    path.display()
                );
            }
            Err(error)
        }
    }
}

fn exit_code(status: i32) -> ExitCode {
    ExitCode::from(status.to_le_bytes()[0])
}

#[derive(Debug)]
struct RunError {
    message: String,
    status: i32,
}

impl RunError {
    fn new(message: impl Into<String>, status: i32) -> Self {
        Self {
            message: message.into(),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brush_process_shim_is_not_mistaken_for_a_path_command() {
        let words = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            external_command_name_from(&words(&["/tmp/fixture", "identity"]), true),
            Some("fixture".into())
        );
        assert_eq!(
            external_command_name_from(
                &words(&[
                    "/usr/local/bin/fixture",
                    "--invoke-bundled",
                    "fixture",
                    "--marsh-guest",
                    "--marsh-session",
                    "session-id",
                    "identity",
                ]),
                true,
            ),
            None
        );
    }

    use std::{cell::RefCell, path::PathBuf};

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    fn os_strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn host_help_and_version_exit_without_running_the_product() {
        for (argument, expected) in [
            ("--help", "Usage:\n"),
            ("-h", "Usage:\n"),
            ("help", "Usage:\n"),
            ("--version", VERSION),
            ("-V", VERSION),
        ] {
            let product_called = std::cell::Cell::new(false);
            let mut output = Vec::new();
            let status = run_with(os_strings(&["marsh", argument]), &mut output, |_| {
                product_called.set(true);
                Ok(0)
            });
            assert_eq!(status, ExitCode::SUCCESS);
            assert!(!product_called.get(), "{argument} reached daemon startup");
            assert!(String::from_utf8(output).unwrap().contains(expected));
        }
    }

    #[test]
    fn mcp_help_exits_without_running_the_product() {
        let product_called = std::cell::Cell::new(false);
        let mut output = Vec::new();
        let status = run_with(os_strings(&["marsh", "mcp", "--help"]), &mut output, |_| {
            product_called.set(true);
            Ok(0)
        });
        assert_eq!(status, ExitCode::SUCCESS);
        assert!(!product_called.get());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("marsh mcp serve [OPTIONS]"));
        assert!(output.contains("marsh mcp start"));
        assert!(output.contains("marsh mcp stop"));
        assert!(output.contains("marsh mcp install codex|claude|sbx"));
        assert!(output.contains("--workspace PATH"));
        assert!(output.contains("--home PATH"));
        assert!(output.contains("--marsh PATH"));
        assert!(output.contains("--sbx PATH"));
        assert!(output.contains("--allow-full-sbx-control"));
    }

    #[test]
    fn mcp_serve_preserves_server_arguments() {
        assert_eq!(
            mcp_action(&strings(&[
                "marsh",
                "mcp",
                "serve",
                "--workspace",
                "/Users/example/project",
                "--home",
                "/Users/example/.marsh",
                "--marsh",
                "/opt/marsh/bin/marsh",
            ]))
            .unwrap(),
            Some(McpAction::Serve(strings(&[
                "serve",
                "--workspace",
                "/Users/example/project",
                "--home",
                "/Users/example/.marsh",
                "--marsh",
                "/opt/marsh/bin/marsh",
            ])))
        );
    }

    #[test]
    fn mcp_publish_parses_one_literal_pipeline_and_explicit_sandbox() {
        assert!(
            mcp_action(&strings(&[
                "marsh",
                "mcp",
                "publish",
                "my_tool",
                "--sandbox",
                "work",
                "--",
                "cat | sort",
            ]))
            .is_ok()
        );
        for args in [
            strings(&["marsh", "mcp", "publish", "my_tool", "cat | sort"]),
            strings(&["marsh", "mcp", "publish", "my_tool", "--"]),
            strings(&["marsh", "mcp", "publish", "my_tool", "--", "a", "b"]),
        ] {
            assert!(mcp_action(&args).is_err());
        }
    }

    #[test]
    fn mcp_publish_accepts_a_tool_description() {
        assert!(
            mcp_action(&strings(&[
                "marsh",
                "mcp",
                "publish",
                "sorter",
                "--description",
                "Sorts input lines",
                "--",
                "sort",
            ]))
            .is_ok()
        );
    }

    #[test]
    fn mcp_publication_rejects_path_syntax_before_touching_a_home() {
        let error = published_mcp_context("../escape").unwrap_err();
        assert_eq!(error.status, 2);
        assert!(error.message.contains("published tool name"));
    }

    #[test]
    fn mcp_install_clients_are_exact_host_forms() {
        assert_eq!(
            mcp_action(&strings(&["marsh", "mcp", "start"])).unwrap(),
            Some(McpAction::Start)
        );
        assert_eq!(
            mcp_action(&strings(&["marsh", "mcp", "stop"])).unwrap(),
            Some(McpAction::Stop)
        );
        for (name, client) in [
            ("codex", McpClient::Codex),
            ("claude", McpClient::Claude),
            ("sbx", McpClient::Sbx),
        ] {
            assert_eq!(
                mcp_action(&strings(&["marsh", "mcp", "install", name])).unwrap(),
                Some(McpAction::Install(client))
            );
        }
        for arguments in [
            strings(&["marsh", "mcp", "install"]),
            strings(&["marsh", "mcp", "install", "other"]),
            strings(&["marsh", "mcp", "install", "codex", "extra"]),
        ] {
            let error = mcp_action(&arguments).unwrap_err();
            assert_eq!(error.status, 2);
        }
    }

    #[test]
    fn malformed_mcp_commands_are_host_usage_errors() {
        for arguments in [
            strings(&["marsh", "mcp"]),
            strings(&["marsh", "mcp", "start", "extra"]),
            strings(&["marsh", "mcp", "stop", "extra"]),
        ] {
            let error = mcp_action(&arguments).unwrap_err();
            assert_eq!(error.status, 2);
            assert!(error.message.contains("marsh mcp"));
        }
        assert_eq!(
            mcp_action(&strings(&["marsh", "-c", "mcp serve"])).unwrap(),
            None
        );
    }

    #[test]
    fn registrations_use_installed_siblings_and_client_specific_scope_roots() {
        let installation = tempfile::tempdir().unwrap();
        let prefix = installation.path();
        let bin = prefix.join("bin");
        let stock_bin = prefix.join("stock/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&stock_bin).unwrap();
        for path in [
            bin.join("marsh"),
            bin.join("marsh-mcp"),
            stock_bin.join("sbx"),
        ] {
            std::fs::write(&path, "test").unwrap();
            make_executable(&path);
        }
        let workspace = tempfile::tempdir().unwrap();
        let workspace_path = workspace.path().canonicalize().unwrap();
        let expected_sbx = stock_bin.join("sbx").canonicalize().unwrap();
        let key = format!(
            "{:x}",
            Sha256::digest(workspace_path.as_os_str().as_encoded_bytes())
        );
        for client in [McpClient::Codex, McpClient::Claude, McpClient::Sbx] {
            let registration =
                mcp_registration_with(client, &bin.join("marsh"), &workspace_path, || {
                    Ok(expected_sbx.clone())
                })
                .unwrap();
            assert_eq!(
                registration.marsh,
                bin.join("marsh").canonicalize().unwrap()
            );
            assert_eq!(
                registration.marsh_mcp,
                bin.join("marsh-mcp").canonicalize().unwrap()
            );
            assert_eq!(registration.sbx, expected_sbx);
            let suffix = match client {
                McpClient::Codex | McpClient::Claude => Path::new("marsh")
                    .join("dev-mcp")
                    .join(client.name())
                    .join(&key),
                McpClient::Sbx => Path::new("clients").join(client.name()).join(&key),
            };
            assert!(registration.scope_root.ends_with(suffix));
            let command = registration.client_command(client).unwrap();
            assert_eq!(command.current_dir, workspace_path);
            assert_eq!(command.arguments[0], "mcp");
            assert_eq!(command.arguments[1], "add");
            assert!(
                command
                    .arguments
                    .iter()
                    .any(|argument| argument == format!("marsh-dev-{}", &key[..12]).as_str())
            );
            assert_client_command(client, &registration, &command, &key, &expected_sbx);
            if client == McpClient::Sbx {
                let broker = registration.broker_start_command();
                assert_eq!(broker.program, registration.marsh_mcp);
                assert_eq!(broker.arguments[0], "broker-start");
                assert_eq!(broker.arguments[1], "--workspace");
                assert_eq!(broker.current_dir, workspace_path);
                let stop = registration.broker_stop_command();
                assert_eq!(stop.program, registration.marsh_mcp);
                assert_eq!(stop.arguments[0], "broker-stop");
                assert_eq!(stop.arguments[1], "--workspace");
                assert_eq!(stop.current_dir, workspace_path);
            }
        }
    }

    #[test]
    fn client_scope_roots_use_each_clients_writable_config_home() {
        let workspace = Path::new("/Users/example/project");
        let host_home = Path::new("/Users/example");
        let codex_home = Path::new("/private/codex-home");
        let claude_config = Path::new("/private/claude-config");
        let key = workspace_key(workspace);

        assert_eq!(
            client_scope_root_with(
                McpClient::Codex,
                workspace,
                None,
                Some(codex_home),
                None,
                None,
            )
            .unwrap(),
            codex_home.join("marsh/dev-mcp/codex").join(&key)
        );
        assert_eq!(
            client_scope_root_with(
                McpClient::Claude,
                workspace,
                None,
                None,
                Some(claude_config),
                None,
            )
            .unwrap(),
            claude_config.join("marsh/dev-mcp/claude").join(&key)
        );
        assert_eq!(
            client_scope_root_with(
                McpClient::Codex,
                workspace,
                Some(host_home),
                None,
                None,
                None,
            )
            .unwrap(),
            host_home.join(".codex/marsh/dev-mcp/codex").join(&key)
        );
        assert_eq!(
            client_scope_root_with(
                McpClient::Claude,
                workspace,
                Some(host_home),
                None,
                None,
                None,
            )
            .unwrap(),
            host_home.join(".claude/marsh/dev-mcp/claude").join(&key)
        );
    }

    #[test]
    fn client_scope_roots_reject_relative_config_homes() {
        let workspace = Path::new("/Users/example/project");
        let relative = Path::new("relative");
        let error = client_scope_root_with(
            McpClient::Codex,
            workspace,
            None,
            Some(relative),
            None,
            None,
        )
        .unwrap_err();
        assert!(error.message.contains("CODEX_HOME must be absolute"));
        let error = client_scope_root_with(
            McpClient::Claude,
            workspace,
            None,
            None,
            Some(relative),
            None,
        )
        .unwrap_err();
        assert!(error.message.contains("CLAUDE_CONFIG_DIR must be absolute"));
    }

    #[test]
    fn client_runner_executes_only_the_captured_program_and_argv() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace with spaces");
        std::fs::create_dir(&workspace).unwrap();
        let fake_client = fixture.path().join("fake-client");
        std::fs::write(
            &fake_client,
            "#!/bin/sh\noutput=$0.capture\npwd >\"$output\"\nfor argument do printf '%s\\n' \"$argument\" >>\"$output\"; done\n",
        )
        .unwrap();
        make_executable(&fake_client);
        let command = McpClientCommand {
            program: fake_client.clone().into_os_string(),
            arguments: ["mcp", "add", "marsh-dev-test", "--", "argument with spaces"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect(),
            current_dir: workspace.canonicalize().unwrap(),
        };
        let status = run_mcp_client_command(&command).unwrap();
        assert!(status.success());
        let captured =
            std::fs::read_to_string(format!("{}.capture", fake_client.display())).unwrap();
        let lines = captured.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], command.current_dir.to_str().unwrap());
        assert_eq!(
            &lines[1..],
            ["mcp", "add", "marsh-dev-test", "--", "argument with spaces"]
        );
    }

    #[test]
    fn sbx_registration_replaces_stale_state_and_first_install_is_idempotent() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake_sbx = fixture.path().join("sbx");
        std::fs::write(
            &fake_sbx,
            "#!/bin/sh\nset -eu\nregistry=$0.registry\nlog=$0.log\nprintf '%s\\n' \"$*\" >>\"$log\"\nif [ \"$1\" = mcp ] && [ \"$2\" = rm ] && [ \"$3\" = --force ]; then if [ -f \"$registry\" ]; then rm \"$registry\"; exit 0; fi; printf 'error: MCP server \\\"%s\\\" not found\\n' \"$4\" >&2; exit 1; fi\nif [ \"$1\" = mcp ] && [ \"$2\" = add ]; then printf '%s\\n' \"$@\" >\"$registry\"; exit 0; fi\nexit 64\n",
        )
        .unwrap();
        make_executable(&fake_sbx);
        let registration = McpRegistration {
            server_name: "marsh-dev-test".into(),
            workspace: workspace.canonicalize().unwrap(),
            scope_root: fixture.path().join("scope"),
            marsh_mcp: fixture.path().join("marsh-mcp"),
            marsh: fixture.path().join("marsh"),
            sbx: fake_sbx.clone(),
        };

        apply_mcp_registration(McpClient::Sbx, &registration).unwrap();
        let registry = PathBuf::from(format!("{}.registry", fake_sbx.display()));
        let first = std::fs::read_to_string(&registry).unwrap();
        assert!(first.starts_with("mcp\nadd\nmarsh-dev-test\n"));

        std::fs::write(&registry, "stale registration\n").unwrap();
        apply_mcp_registration(McpClient::Sbx, &registration).unwrap();
        let replaced = std::fs::read_to_string(&registry).unwrap();
        assert_eq!(replaced, first);
        let log = std::fs::read_to_string(format!("{}.log", fake_sbx.display())).unwrap();
        let lines = log.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "mcp rm --force marsh-dev-test");
        assert_eq!(lines[2], "mcp rm --force marsh-dev-test");
        assert_eq!(lines[1], lines[3]);
        assert!(lines[1].starts_with("mcp add marsh-dev-test --command "));
        assert!(lines[1].contains(" --args connect,--workspace,"));
        assert!(lines[1].contains(" --dir "));
    }

    #[test]
    fn sbx_registration_rejects_lookalike_missing_diagnostic() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake_sbx = fixture.path().join("sbx");
        std::fs::write(
            &fake_sbx,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$0.log\"\nprintf 'error: MCP server \\\"marsh-dev-test-old\\\" not found\\n' >&2\nexit 9\n",
        )
        .unwrap();
        make_executable(&fake_sbx);
        let registration = McpRegistration {
            server_name: "marsh-dev-test".into(),
            workspace: workspace.canonicalize().unwrap(),
            scope_root: fixture.path().join("scope"),
            marsh_mcp: fixture.path().join("marsh-mcp"),
            marsh: fixture.path().join("marsh"),
            sbx: fake_sbx.clone(),
        };
        let error = apply_mcp_registration(McpClient::Sbx, &registration).unwrap_err();
        assert_eq!(error.status, 9);
        assert!(
            error
                .message
                .contains("failed to remove sbx MCP registration")
        );
        assert!(error.message.contains("marsh-dev-test-old"));
        let log = std::fs::read_to_string(format!("{}.log", fake_sbx.display())).unwrap();
        assert_eq!(log, "mcp rm --force marsh-dev-test\n");

        std::fs::write(
            &fake_sbx,
            "#!/bin/sh\nprintf 'error: unauthorized\n' >&2\nexit 8\n",
        )
        .unwrap();
        make_executable(&fake_sbx);
        let error = apply_mcp_registration(McpClient::Sbx, &registration).unwrap_err();
        assert_eq!(error.status, 8);
        assert!(error.message.contains("stderr: error: unauthorized"));
    }

    #[test]
    fn published_registration_inspect_and_rollback_keep_existing_registration() {
        let fixture = tempfile::tempdir().unwrap();
        let workspace = fixture.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let fake_sbx = fixture.path().join("sbx");
        std::fs::write(
            &fake_sbx,
            r#"#!/bin/sh
set -eu
registry=$0.registry
printf '%s\n' "$*" >>"$0.log"
if [ "$1" = mcp ] && [ "$2" = inspect ]; then
  if [ -f "$registry" ]; then cat "$registry"; exit 0; fi
  printf 'error: mcp server "%s" not found: mcp server not found\n  try: sbx mcp ls\n' "$3" >&2
  exit 1
fi
if [ "$1" = mcp ] && [ "$2" = add ]; then : >"$registry"; exit 0; fi
if [ "$1" = mcp ] && [ "$2" = rm ]; then rm -f "$registry"; exit 0; fi
exit 64
"#,
        )
        .unwrap();
        make_executable(&fake_sbx);
        let registration = McpRegistration {
            server_name: "marsh-pub-test".into(),
            workspace: workspace.canonicalize().unwrap(),
            scope_root: fixture.path().join("scope"),
            marsh_mcp: fixture.path().join("marsh-mcp"),
            marsh: fixture.path().join("marsh"),
            sbx: fake_sbx.clone(),
        };
        let home = fixture.path().join("home");
        let declaration = fixture.path().join("tool.json");
        let add =
            published_sbx_add_command_with_generation(&registration, &home, &declaration, None)
                .unwrap();
        assert!(!inspect_published_sbx_registration(&registration, &add).unwrap());
        run_checked_mcp_command(&add, "register published MCP tool").unwrap();
        let expected_command = vec![
            registration.marsh_mcp.display().to_string(),
            "export-serve".into(),
            "--workspace".into(),
            registration.workspace.display().to_string(),
            "--home".into(),
            home.display().to_string(),
            "--marsh".into(),
            registration.marsh.display().to_string(),
            "--sbx".into(),
            fake_sbx.display().to_string(),
            "--declaration".into(),
            declaration.display().to_string(),
        ];
        let inspected = serde_json::json!({
            "name": registration.server_name,
            "type": "local",
            "command": expected_command,
            "resolved_command": registration.marsh_mcp,
        });
        let registry = PathBuf::from(format!("{}.registry", fake_sbx.display()));
        std::fs::write(&registry, serde_json::to_vec(&inspected).unwrap()).unwrap();
        assert!(inspect_published_sbx_registration(&registration, &add).unwrap());

        write_publication(&declaration, b"new").unwrap();
        rollback_publication(&registration, &declaration, Some(b"previous"), false).unwrap();
        assert_eq!(std::fs::read(&declaration).unwrap(), b"previous");
        assert!(inspect_published_sbx_registration(&registration, &add).unwrap());
        let mut mismatched = inspected.clone();
        mismatched["command"][1] = serde_json::json!("serve");
        std::fs::write(&registry, serde_json::to_vec(&mismatched).unwrap()).unwrap();
        assert!(
            inspect_published_sbx_registration(&registration, &add)
                .unwrap_err()
                .message
                .contains("does not match")
        );
        std::fs::write(&registry, serde_json::to_vec(&inspected).unwrap()).unwrap();
        let log = std::fs::read_to_string(format!("{}.log", fake_sbx.display())).unwrap();
        assert!(!log.lines().any(|line| line.starts_with("mcp rm ")));

        write_publication(&declaration, b"replacement").unwrap();
        rollback_publication(&registration, &declaration, Some(b"previous"), true).unwrap();
        assert_eq!(std::fs::read(&declaration).unwrap(), b"previous");
        assert!(!inspect_published_sbx_registration(&registration, &add).unwrap());

        std::fs::write(&registry, serde_json::to_vec(&inspected).unwrap()).unwrap();
        rollback_publication(&registration, &declaration, None, true).unwrap();
        assert!(!declaration.exists());
        assert!(!inspect_published_sbx_registration(&registration, &add).unwrap());

        std::fs::write(
            &fake_sbx,
            "#!/bin/sh\nprintf 'error: unauthorized\\n' >&2\nexit 8\n",
        )
        .unwrap();
        make_executable(&fake_sbx);
        let error = inspect_published_sbx_registration(&registration, &add).unwrap_err();
        assert_eq!(error.status, 8);
        assert!(error.message.contains("unauthorized"));
    }

    fn assert_client_command(
        client: McpClient,
        registration: &McpRegistration,
        command: &McpClientCommand,
        key: &str,
        expected_sbx: &Path,
    ) {
        match client {
            McpClient::Codex => {
                assert_eq!(command.program, "codex");
                assert_eq!(command.arguments[3], "--");
                assert_eq!(command.arguments[4], registration.marsh_mcp);
            }
            McpClient::Claude => {
                assert_eq!(command.program, "claude");
                assert_eq!(
                    &command.arguments[2..8],
                    [
                        "--transport",
                        "stdio",
                        "--scope",
                        "local",
                        format!("marsh-dev-{}", &key[..12]).as_str(),
                        "--",
                    ]
                );
                assert_eq!(command.arguments[8], registration.marsh_mcp);
            }
            McpClient::Sbx => {
                assert_eq!(command.program, expected_sbx);
                assert_eq!(command.arguments[3], "--command");
                assert_eq!(command.arguments[4], registration.marsh_mcp);
                assert_eq!(command.arguments[5], "--args");
                let encoded = command.arguments[6].to_str().unwrap();
                assert!(encoded.contains("connect,--workspace,"));
                assert!(encoded.contains(",--scope-root,"));
                assert!(encoded.contains("/clients/sbx/"));
                assert!(encoded.contains(",--sbx,"));
                assert!(encoded.ends_with(expected_sbx.to_str().unwrap()));
                assert_eq!(command.arguments[7], "--dir");
                assert_eq!(command.arguments[8], registration.workspace);
            }
        }
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}

    #[test]
    fn registration_rejects_a_non_executable_mcp_sibling() {
        let installation = tempfile::tempdir().unwrap();
        let bin = installation.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let marsh = bin.join("marsh");
        std::fs::write(&marsh, "test").unwrap();
        make_executable(&marsh);
        std::fs::write(bin.join("marsh-mcp"), "test").unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let error = mcp_registration(McpClient::Codex, &marsh, workspace.path()).unwrap_err();
        assert!(error.message.contains("not an executable regular file"));
        assert!(error.message.contains("marsh-mcp"));
    }

    #[test]
    fn either_relay_variable_makes_mcp_host_ineligible() {
        assert!(!relay_environment_present(false, false));
        assert!(relay_environment_present(true, false));
        assert!(relay_environment_present(false, true));
        assert!(relay_environment_present(true, true));
        for client in [McpClient::Codex, McpClient::Claude, McpClient::Sbx] {
            assert!(require_host_mcp_install(client, false, false).is_ok());
            for (socket, token) in [(true, false), (false, true), (true, true)] {
                let error = require_host_mcp_install(client, socket, token).unwrap_err();
                assert_eq!(error.status, 2);
                assert!(error.message.contains("host-only"));
                assert!(error.message.contains(client.name()));
            }
        }
    }

    #[test]
    fn missing_mcp_sibling_is_actionable() {
        let directory = tempfile::tempdir().unwrap();
        let error = run_mcp_sibling(&directory.path().join("marsh"), &[]).unwrap_err();
        assert_eq!(error.status, 1);
        assert!(
            error
                .message
                .contains("cannot start the installed MCP server")
        );
        assert!(error.message.contains("marsh-mcp"));
    }

    #[test]
    fn help_tokens_outside_the_exact_host_form_continue_to_the_product() {
        for arguments in [
            os_strings(&["marsh", "-c", "printf --help"]),
            os_strings(&["marsh", "--", "--help"]),
            os_strings(&["marsh", "--marsh-guest", "--help"]),
            os_strings(&["marsh", "--invoke-bundled", "fixture", "--help"]),
        ] {
            let expected = arguments.clone();
            let mut output = Vec::new();
            let status = run_with(arguments, &mut output, |received| {
                assert_eq!(received, expected);
                Ok(0)
            });
            assert_eq!(status, ExitCode::SUCCESS);
            assert!(output.is_empty());
        }
    }

    #[test]
    fn raw_shell_input_bypasses_text_protocol_and_help_parsing() {
        use std::os::unix::ffi::OsStringExt as _;
        for tail in [
            vec!["-c".into(), OsString::from_vec(b"printf '\xff'".to_vec())],
            vec![OsString::from_vec(b"script-\xfe".to_vec()), "--help".into()],
            vec![
                "--invoke-bundled".into(),
                "fixture".into(),
                OsString::from_vec(vec![0xff]),
            ],
        ] {
            let mut arguments = vec![OsString::from_vec(b"marsh-\xfe".to_vec())];
            arguments.extend(tail);
            let expected = arguments.clone();
            let mut output = Vec::new();
            let status = run_with(arguments, &mut output, |received| {
                assert_eq!(received, expected);
                Ok(0)
            });
            assert_eq!(status, ExitCode::SUCCESS);
            assert!(output.is_empty());
        }
    }

    #[test]
    fn bundled_cwd_does_not_replace_session_project_metadata() {
        use std::os::unix::ffi::OsStringExt as _;
        let mut parent = session(Path::new("/fixture/home"));
        parent.session_id = Some("attached-session".into());
        let context = external_commands::session_context(&parent).unwrap();
        let mut child = parent.clone();
        child
            .launch_directory
            .push(OsString::from_vec(b"child-\xff".to_vec()));
        inherit_guest_project_root(&mut child, "attached-session", OsStr::new(&context)).unwrap();
        assert_eq!(child.launch_directory, parent.launch_directory);
        let error = inherit_guest_project_root(&mut child, "another-session", OsStr::new(&context))
            .unwrap_err();
        assert_eq!(error.status, 125);
        assert_eq!(child.launch_directory, parent.launch_directory);
    }

    #[test]
    fn recognized_protocols_and_internal_numbers_reject_non_utf8_without_echoing_data() {
        use std::os::unix::ffi::OsStringExt as _;
        let raw = OsString::from_vec(b"private-\xff".to_vec());
        for command in ["acp", "mcp"] {
            let error = try_run(vec!["marsh".into(), command.into(), raw.clone()]).unwrap_err();
            assert_eq!(error.status, 2);
            assert_eq!(
                error.message,
                "product command arguments must be valid UTF-8"
            );
        }
        assert_eq!(internal_id(&raw, "invalid UID").unwrap_err().status, 2);
        assert_eq!(
            internal_id(OsStr::new("1000"), "invalid UID").unwrap(),
            1000
        );
    }

    #[test]
    fn absent_daemon_claims_nothing_running_only_when_no_vm_is_recorded() {
        let stop = cli::ProductCommand::Stop { json: false };
        let reset = cli::ProductCommand::Reset { json: true };
        let empty = absent_scope_report(&stop, Ok(Vec::new())).unwrap();
        assert!(empty.cleanup_complete && empty.components.is_empty());

        let left = absent_scope_report(&reset, Ok(vec!["marsh-k-ab12cd34".into()])).unwrap();
        assert_eq!(left.action, marsh_daemon::ScopeLifecycleAction::Reset);
        assert!(!left.cleanup_complete);
        let text = product::lifecycle_summary(&left, Path::new("/h"));
        assert!(text.contains("marsh: reset incomplete for /h\n"), "{text}");
        assert!(
            text.contains("left scope recorded VM marsh-k-ab12cd34: daemon is not running"),
            "{text}"
        );
        assert!(text.contains("then `marsh reset`"), "{text}");

        let unreadable = absent_scope_report(&stop, Err(std::io::Error::other("bad map"))).unwrap();
        assert!(!unreadable.cleanup_complete);
        assert!(product::lifecycle_summary(&unreadable, Path::new("/h")).contains("bad map"));
    }

    fn session(home: &std::path::Path) -> SessionConfig {
        SessionConfig {
            session_id: None,
            username: "example".into(),
            uid: 1000,
            gid: 1000,
            launch_directory: PathBuf::from("/Users/example/project"),
            guest_home: PathBuf::from("/Users/example"),
            home_backing: home.to_owned(),
            ephemeral_home: true,
        }
    }

    #[test]
    fn session_run_removes_ephemeral_home_after_success_and_retains_on_error() {
        for fails in [false, true] {
            let persistent = tempfile::tempdir().unwrap();
            let ephemeral_path = RefCell::new(None);
            let registry = persistent.path().canonicalize().unwrap().join("registry");
            let adapter = marsh_sbx::StockSbx::new(
                "/unused",
                Arc::new(marsh_runtime::SystemCommandRunner::new("/nonexistent")),
            )
            .with_ephemeral_root_for_test(registry.clone());
            let result = with_session_home_allocator(
                session(persistent.path()),
                true,
                |session| session.activate_ephemeral_home_with(adapter),
                |session, token| {
                    assert!(token.is_some());
                    ephemeral_path.replace(Some(session.home_backing.clone()));
                    if fails {
                        Err(RunError::new("expected failure", 1))
                    } else {
                        Ok(0)
                    }
                },
            );
            assert_eq!(result.is_err(), fails);
            let path = ephemeral_path.into_inner().unwrap();
            assert_eq!(path.exists(), fails);
            if fails {
                let adapter = marsh_sbx::StockSbx::new(
                    "/unused",
                    Arc::new(marsh_runtime::SystemCommandRunner::new("/nonexistent")),
                )
                .with_ephemeral_root_for_test(registry.clone());
                assert_eq!(adapter.recover_ephemeral_home(0).unwrap(), Some(path));
            }
        }
    }
}
