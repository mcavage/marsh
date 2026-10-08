//! Product command orchestration independent from the daemon transport.

use crate::{
    cli::{ParsedCli, ProductCommand},
    client::{ClientError, CommandExecutor, DaemonClient},
    external_commands,
    registered_commands::{RegisteredCommands, install_registered_commands},
    session::SessionConfig,
};
use std::{ffi::OsString, fmt, io::Write, sync::Arc};

const RESULTS_HELP: &str = "\
Usage:
  marsh results [--json]
  marsh results show CURSOR|JOB [--json]

Lists durable structural job receipts newest first. RESULT may be a displayed
cursor, a full job UUID, or a unique UUID prefix. Brush `history` remains
ordinary shell command history; results do not contain prompts or output.
WALL is the whole request, invocation to verified cleanup, including Kit VM
preparation; `marsh jobs` RUN is only the job's own run in the ready VM.
";

const WORKERS_HELP: &str = "\
Usage:
  marsh workers reset KIT[,KIT...]|all

Stops and removes only the selected idle marsh Kit worker VMs. Active jobs are
never cancelled. The project shell, selected home, results, credentials, and
unrelated Docker Sandboxes are preserved. Run marsh --load KIT[,KIT...]|all to
recreate and prewarm the workers.
";

#[derive(Debug)]
pub enum ProductError {
    Operation(String),
    /// Failure of shell preparation, attachment delivery, or cleanup, not a
    /// command's actual exit status and not a CLI usage error.
    ShellDelivery(String),
}

impl ProductError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Operation(_) => 1,
            Self::ShellDelivery(_) => 125,
        }
    }

    fn for_shell(self) -> Self {
        Self::ShellDelivery(self.to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedShell {
    pub brush_args: Vec<OsString>,
    pub session_id: String,
    /// The interactive shell to run; Brush unless the host chose bash or zsh.
    pub shell: crate::shell_choice::ShellChoice,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductOutcome {
    Complete(i32),
    GuestShell(PreparedShell),
}

impl fmt::Display for ProductError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Operation(message) | Self::ShellDelivery(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ProductError {}

impl From<ClientError> for ProductError {
    fn from(error: ClientError) -> Self {
        Self::Operation(error.0)
    }
}

impl From<std::io::Error> for ProductError {
    fn from(error: std::io::Error) -> Self {
        Self::Operation(format!("cannot write daemon response: {error}"))
    }
}

/// Run a public inspection command, or prepare a shell and return its Brush
/// argument vector. JSON is written only to stdout; progress remains stderr.
///
/// # Errors
/// Returns daemon, registry, or output failures without entering Brush.
pub fn prepare(
    parsed: ParsedCli,
    client: Arc<dyn DaemonClient>,
    mut session: SessionConfig,
    mut stdout: impl Write,
    mut stderr: impl Write,
) -> Result<ProductOutcome, ProductError> {
    if let Some(session_id) = &parsed.session_id {
        session.session_id = Some(session_id.clone());
    }
    match parsed.command {
        ProductCommand::KitInstall { command, reference } => {
            let installed = client.install_kit(&command, &reference)?;
            if let Err(error) = external_commands::install_live_command(&command) {
                writeln!(
                    &mut stderr,
                    "Kit installed; open a new shell to invoke {command}: {error}"
                )?;
            }
            writeln!(&mut stdout, "installed {command} from {installed}")?;
            return Ok(ProductOutcome::Complete(0));
        }
        ProductCommand::Status { json } => {
            let value = client.status_json()?;
            if json {
                write_json(&mut stdout, &value)?;
            } else {
                write_status(&mut stdout, value)?;
            }
            return Ok(ProductOutcome::Complete(0));
        }
        ProductCommand::Results { json } => {
            let value = client.jobs_json()?;
            if json {
                write_json(&mut stdout, &value)?;
            } else {
                write_results(&mut stdout, value)?;
            }
            return Ok(ProductOutcome::Complete(0));
        }
        ProductCommand::Result { selector, json } => {
            let value = client.job_json(&selector)?;
            if json {
                write_json(&mut stdout, &value)?;
            } else {
                write_result(&mut stdout, value)?;
            }
            return Ok(ProductOutcome::Complete(0));
        }
        ProductCommand::ResultsHelp => return write_help(&mut stdout, RESULTS_HELP),
        ProductCommand::WorkersHelp => return write_help(&mut stdout, WORKERS_HELP),
        ProductCommand::WorkersReset { selection } => {
            return reset_workers(client.as_ref(), &selection, &mut stdout);
        }
        ProductCommand::Reset { json } | ProductCommand::Stop { json } => {
            let reset = matches!(parsed.command, ProductCommand::Reset { .. });
            let home = session
                .home_backing
                .parent()
                .unwrap_or(&session.home_backing);
            return scope_lifecycle(client.as_ref(), reset, json, home, &mut stdout, &mut stderr);
        }
        ProductCommand::Shell => {}
    }

    prepare_shell_session(client.as_ref(), &mut session, &parsed).map_err(|error| {
        if parsed.guest {
            error
        } else {
            error.for_shell()
        }
    })?;

    let session_id = session.session_id.clone().ok_or_else(|| {
        ProductError::ShellDelivery("daemon did not assign a shell session".into())
    })?;
    if !parsed.guest {
        let mut guest_args = vec![
            parsed.brush_args[0].clone(),
            "--marsh-guest".into(),
            "--marsh-session".into(),
            session_id.clone().into(),
        ];
        append_ephemeral_handoff(&session, &mut guest_args);
        if let Some(shell) = parsed
            .shell
            .filter(|shell| *shell != crate::shell_choice::ShellChoice::Marsh)
        {
            guest_args.extend(["--marsh-shell".into(), shell.name().into()]);
        }
        guest_args.extend(parsed.brush_args.iter().skip(1).cloned());
        // OpenShell transfers cleanup ownership to the server. In particular,
        // a lost delivery may return here BEFORE its cleanup has completed.
        // A client-side detach must not clear the server's retained authority,
        // or replace an already received terminal with a redundant RPC failure.
        let status = client
            .open_shell(&session, &guest_args)
            .map_err(|error| ProductError::ShellDelivery(error.to_string()))?;
        return Ok(ProductOutcome::Complete(status));
    }
    prepare_guest_shell(parsed, client, session, session_id)
}

fn prepare_guest_shell(
    parsed: ParsedCli,
    client: Arc<dyn DaemonClient>,
    session: SessionConfig,
    session_id: String,
) -> Result<ProductOutcome, ProductError> {
    if parsed.session_id.is_none() {
        return Err(ProductError::Operation(
            "guest shell launch is missing its daemon session identity".into(),
        ));
    }
    let mut shim_args = vec![
        "--marsh-guest".into(),
        "--marsh-session".into(),
        session_id.clone().into(),
    ];
    append_ephemeral_handoff(&session, &mut shim_args);
    // Like Bash, run executable files without a shebang (ENOEXEC) with this
    // shell: a child guest marsh attached to the same session, not /bin/sh.
    if let Ok(executable) = std::env::current_exe() {
        brush_shell::entry::install_script_interpreter(&executable, &shim_args);
    }
    let shim_args = shim_args
        .into_iter()
        .map(|argument| {
            argument.into_string().map_err(|_| {
                ProductError::Operation("process shim argument is not valid UTF-8".into())
            })
        })
        .collect::<Result<Vec<String>, _>>()?;
    brush_shell::bundled::install_process_shim_args(shim_args);

    let reservation_client = Arc::clone(&client);
    let reservation_session = session.clone();
    brush_shell::bundled::install_background_prelaunch_hook(Arc::new(move |command| {
        let Some(adapter) = background_acp_adapter(command.as_bytes()) else {
            return Ok(Vec::new());
        };
        reservation_client
            .acp_reserve(adapter, &reservation_session)
            .map(|id| vec![("MARSH_ACP_RESERVATION_ID".into(), id)])
            .map_err(|error| format!("acp: {error}"))
    }));

    // `split { ... } | join`: the daemon owns the workspaces; Brush is a client.
    if let Ok(spec) = crate::client::daemon_session(&session) {
        brush_shell::bundled::install_split_workspace_hook(Arc::new(
            crate::split_cli::SessionSplit {
                session: marsh_daemon::SessionSpec {
                    terminal: false,
                    terminal_size: None,
                    ..spec
                },
            },
        ));
    }

    let commands = shell_registered_commands(client.as_ref())?;
    install_registered_commands(commands, Arc::new(CommandExecutor::new(client, session)))
        .map_err(|error| ProductError::Operation(error.to_string()))?;
    Ok(ProductOutcome::GuestShell(PreparedShell {
        brush_args: parsed.brush_args,
        session_id,
        shell: parsed.shell.unwrap_or_default(),
    }))
}

/// Recognize only the existing literal reservation form. Never evaluate source
/// or expansions in the parent, and never decode unrelated command operands.
fn background_acp_adapter(command: &[u8]) -> Option<&str> {
    let mut words = command
        .split(u8::is_ascii_whitespace)
        .filter(|word| !word.is_empty());
    if words.next()? != b"acp" || words.next()? != b"run" {
        return None;
    }
    let adapter = words.next()?;
    if words.next().is_some()
        || !adapter
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    // The actual protocol identity is ASCII; the source as a whole need not be.
    std::str::from_utf8(adapter).ok()
}

fn prepare_shell_session(
    client: &dyn DaemonClient,
    session: &mut SessionConfig,
    parsed: &ParsedCli,
) -> Result<(), ProductError> {
    ensure_session(client, session)?;
    let result: Result<(), ClientError> = (|| {
        if let Some(selection) = &parsed.load {
            client.prepare(selection, session)?;
        }
        Ok(())
    })();
    if result.is_err()
        && !parsed.guest
        && let Some(id) = &session.session_id
    {
        let _ = client.detach_shell(id);
    }
    result.map_err(Into::into)
}

fn shell_registered_commands(
    client: &dyn DaemonClient,
) -> Result<RegisteredCommands, ProductError> {
    let mut names = client.registered_commands()?;
    if names
        .iter()
        .any(|name| matches!(name.as_str(), "acp" | "mcp" | "ps" | "top"))
    {
        return Err(ProductError::Operation(
            "acp, mcp, ps, and top are reserved shell commands".into(),
        ));
    }
    names.extend(["acp".into(), "mcp".into(), "ps".into(), "top".into()]);
    RegisteredCommands::new(names).map_err(|error| ProductError::Operation(error.to_string()))
}

fn write_help(stdout: &mut impl Write, help: &str) -> Result<ProductOutcome, ProductError> {
    stdout.write_all(help.as_bytes())?;
    Ok(ProductOutcome::Complete(0))
}

fn scope_lifecycle(
    client: &dyn DaemonClient,
    reset: bool,
    json: bool,
    home: &std::path::Path,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<ProductOutcome, ProductError> {
    let report = if reset {
        client.reset_scope()?
    } else {
        client.stop_scope()?
    };
    write_lifecycle_report(&report, home, json, stdout, stderr)
}

/// Print a lifecycle report: the facts on stderr, the report on stdout with
/// `--json`. Exit status 1 unless the daemon verified every cleanup.
///
/// # Errors
/// Returns output failures.
pub fn write_lifecycle_report(
    report: &marsh_daemon::ScopeLifecycleReport,
    home: &std::path::Path,
    json: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<ProductOutcome, ProductError> {
    stderr.write_all(lifecycle_summary(report, home).as_bytes())?;
    if json {
        let value = serde_json::to_value(report).map_err(|error| {
            ProductError::Operation(format!("cannot encode lifecycle report: {error}"))
        })?;
        write_json(stdout, &value)?;
    }
    Ok(ProductOutcome::Complete(i32::from(
        !report.cleanup_complete,
    )))
}

/// One line for what happened, one per unverified component, and the next
/// step. Only VMs the daemon reports as removed are named as removed.
#[must_use]
pub fn lifecycle_summary(
    report: &marsh_daemon::ScopeLifecycleReport,
    home: &std::path::Path,
) -> String {
    use marsh_daemon::{ScopeCleanupState, ScopeLifecycleAction};
    use std::fmt::Write as _;
    let (verb, word) = match report.action {
        ScopeLifecycleAction::Stop => ("stopped", "stop"),
        ScopeLifecycleAction::Reset => ("reset", "reset"),
    };
    let home = home.display();
    let removed = report
        .components
        .iter()
        .filter(|component| component.state == ScopeCleanupState::Removed)
        .filter_map(|component| component.vm.as_deref())
        .collect::<Vec<_>>();
    let uncertain = report
        .components
        .iter()
        .filter(|component| component.state == ScopeCleanupState::CleanupUncertain)
        .collect::<Vec<_>>();
    let mut facts = Vec::new();
    match removed.len() {
        0 => {}
        1 => facts.push(format!("1 VM removed ({})", removed[0])),
        count => facts.push(format!("{count} VMs removed ({})", removed.join(", "))),
    }
    if report.components.iter().any(|component| {
        component.kind == "dev-grant" && component.state == ScopeCleanupState::Removed
    }) {
        facts.push("development grants revoked".into());
    }
    if removed.is_empty() && report.cleanup_complete {
        facts.insert(0, "no VMs were running".into());
    }
    if report.cleanup_complete && report.action == ScopeLifecycleAction::Stop {
        facts.push("daemon stopped".into());
    }
    let mut text = if report.cleanup_complete {
        format!("marsh: {verb} {home}: {}\n", facts.join(", "))
    } else if facts.is_empty() {
        format!("marsh: {word} incomplete for {home}\n")
    } else {
        format!(
            "marsh: {word} incomplete for {home}: {}\n",
            facts.join(", ")
        )
    };
    if report.cleanup_complete && report.action == ScopeLifecycleAction::Reset {
        text.push_str("marsh: daemon still running; the next shell starts fresh VMs\n");
    }
    for component in &uncertain {
        let subject = match (component.kind.as_str(), component.vm.as_deref()) {
            ("kit", Some(vm)) => format!("Kit {} VM {vm}", component.label),
            ("shell", Some(vm)) => format!("shell VM {vm}"),
            (kind, Some(vm)) => format!("{kind} {} VM {vm}", component.label),
            (kind, None) => format!("{kind} {}", component.label),
        };
        let detail = component
            .detail
            .as_deref()
            .unwrap_or("cleanup not verified");
        let _ = writeln!(text, "marsh: left {subject}: {detail}");
    }
    if !report.cleanup_complete {
        let _ = writeln!(
            text,
            "marsh: {word} exited 1; anything left is still recorded for this home. Fix the cause above, then rerun `marsh {word}`"
        );
    }
    text
}

fn reset_workers(
    client: &dyn DaemonClient,
    selection: &crate::cli::LoadSelection,
    stdout: &mut impl Write,
) -> Result<ProductOutcome, ProductError> {
    let report = client.reset_workers(selection)?;
    writeln!(stdout, "Reset worker VMs for: {}", report.kits.join(", "))?;
    writeln!(stdout, "Run marsh --load all to recreate and prewarm them.")?;
    Ok(ProductOutcome::Complete(0))
}

fn append_ephemeral_handoff(session: &SessionConfig, arguments: &mut Vec<OsString>) {
    if session.ephemeral_home {
        arguments.extend([
            "--ephemeral-home".into(),
            "--marsh-home-backing".into(),
            session.home_backing.as_os_str().to_owned(),
        ]);
    }
}

fn ensure_session(
    client: &dyn DaemonClient,
    session: &mut SessionConfig,
) -> Result<(), ProductError> {
    if session.session_id.is_none() {
        session.session_id = Some(client.attach_shell(std::process::id(), session)?);
    }
    Ok(())
}

fn write_json(writer: &mut impl Write, value: &serde_json::Value) -> Result<(), ProductError> {
    serde_json::to_writer(&mut *writer, value).map_err(|error| {
        ProductError::Operation(format!("cannot encode daemon response: {error}"))
    })?;
    writer
        .write_all(b"\n")
        .map_err(|error| ProductError::Operation(format!("cannot write daemon response: {error}")))
}

fn write_status(writer: &mut impl Write, value: serde_json::Value) -> Result<(), ProductError> {
    let status: marsh_daemon::StatusDocument = serde_json::from_value(value).map_err(|error| {
        ProductError::Operation(format!("cannot decode daemon status: {error}"))
    })?;
    writeln!(
        writer,
        "Daemon {} (pid {})",
        status.daemon_id, status.endpoint_owner.pid
    )?;
    writeln!(writer, "  scope:        {}", status.scope_id)?;
    writeln!(writer, "  control_home: {}", status.control_home.display())?;
    writeln!(
        writer,
        "  commands:     {}",
        status.control_home.join("commands.json").display()
    )?;
    // Detached records are history, not shells; only live or unrecovered
    // ones are counted.
    let count = |state| {
        status
            .shells
            .iter()
            .filter(|shell| shell.state == state)
            .count()
    };
    let uncertain = count(marsh_daemon::ShellState::CleanupUncertain);
    let uncertain = if uncertain == 0 {
        String::new()
    } else {
        format!(" ({uncertain} cleanup-uncertain; `marsh reset` recovers)")
    };
    writeln!(
        writer,
        "  shells:       {} attached{uncertain}",
        count(marsh_daemon::ShellState::Attached)
    )?;
    let shell_vms = recorded_shell_vms(&status.control_home);
    writeln!(
        writer,
        "  shell VM:     {}",
        if shell_vms.is_empty() {
            "none".to_owned()
        } else {
            shell_vms.join(", ")
        }
    )?;
    writeln!(
        writer,
        "  splits:       {} active, {} awaiting, {} kept",
        status.splits.active, status.splits.awaiting, status.splits.kept
    )?;
    let held = if status.processes.held == 0 {
        String::new()
    } else {
        " (held by cleanup-uncertain jobs; `marsh workers reset KIT` releases them)".into()
    };
    writeln!(
        writer,
        "  processes:    {} running, {} refused, {} held{held}",
        status.processes.running, status.processes.refused, status.processes.held
    )?;
    let job_defaults = status.job_defaults.clone();
    writeln!(writer, "  workers:      {}", status.workers.len())?;
    for worker in status.workers {
        let kit = if worker.kits.is_empty() {
            worker.kit_ref.clone()
        } else {
            worker.kits.join(",")
        };
        writeln!(
            writer,
            "    {kit:<16} VM {}  {}",
            worker.vm_id,
            crate::client::serde_label(&worker.health)
        )?;
    }
    write_job_defaults(writer, job_defaults.as_ref())?;
    Ok(())
}

/// Shell VM names recorded (with a created UUID) in this scope's host-only
/// ownership map. Read-only; a missing or unreadable map lists nothing.
fn recorded_shell_vms(control_home: &std::path::Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(control_home.join("vm-ownership.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    value
        .get("vms")
        .and_then(serde_json::Value::as_object)
        .map(|vms| {
            vms.iter()
                .filter(|(_, entry)| {
                    entry.get("purpose").and_then(serde_json::Value::as_str) == Some("shell")
                        && entry.get("uuid").is_some_and(|uuid| !uuid.is_null())
                })
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn write_job_defaults(
    writer: &mut impl Write,
    defaults: Option<&marsh_daemon::JobDefaults>,
) -> Result<(), ProductError> {
    if let Some(defaults) = defaults {
        let resources = defaults.resources;
        writeln!(writer, "Per-job defaults (daemon first-launch snapshot):")?;
        for (name, variable, value, unit) in [
            (
                "CPU",
                "MARSH_JOB_CPU_MILLIS",
                u64::from(resources.cpu_millis),
                "millicpus",
            ),
            (
                "memory",
                "MARSH_JOB_MEMORY_BYTES",
                resources.memory_bytes,
                "bytes",
            ),
            (
                "PIDs",
                "MARSH_JOB_PIDS",
                u64::from(resources.pids),
                "processes",
            ),
            (
                "writable",
                "MARSH_JOB_WRITABLE_BYTES",
                resources.writable_bytes,
                "bytes",
            ),
            (
                "output",
                "MARSH_JOB_OUTPUT_BYTES",
                resources.output_bytes,
                "bytes (stdout + stderr)",
            ),
            (
                "wall",
                "MARSH_JOB_WALL_SECONDS",
                resources.wall_seconds,
                "seconds (a child: no later than its parent)",
            ),
            (
                "Kit VMs",
                "MARSH_TREE_KIT_VMS",
                defaults.tree_kit_vms as u64,
                "distinct per job tree",
            ),
        ] {
            let origin = if defaults
                .environment_overrides
                .iter()
                .any(|name| name == variable)
            {
                format!("daemon launch environment: {variable}")
            } else {
                "built-in default".into()
            };
            writeln!(writer, "  {name}: {value} {unit} [{origin}]")?;
        }
        writeln!(
            writer,
            "Later shells cannot override this snapshot. Close work and stop the scope before changing it, or use a separate MARSH_HOME."
        )?;
    } else {
        writeln!(
            writer,
            "Per-job defaults: unavailable (inspection-only daemon)"
        )?;
    }
    Ok(())
}

fn write_results(writer: &mut impl Write, value: serde_json::Value) -> Result<(), ProductError> {
    let document: marsh_daemon::JobsDocument = serde_json::from_value(value).map_err(|error| {
        ProductError::Operation(format!("cannot decode daemon results: {error}"))
    })?;
    writeln!(
        writer,
        "CURSOR  JOB                                   COMMAND                 PLACEMENT  STATUS       CLEANUP           WALL  RESULT  PARENT"
    )
    .map_err(ProductError::from)?;
    for job in document.jobs {
        let duration = format_duration(job.wall_ms);
        let result = job
            .exit_code
            .map_or_else(|| "-".into(), |code| code.to_string());
        // A child job names its parent job (short id); a root, `-`.
        let parent = job
            .parent
            .as_deref()
            .and_then(|parent| parent.strip_prefix("job:"))
            .map_or_else(|| "-".to_owned(), |id| id.chars().take(8).collect());
        writeln!(
            writer,
            "{:<7} {:<36}  {:<23} {:<10} {:<12} {:<13} {:>8}  {:<6}  {}",
            job.cursor,
            job.job_id,
            truncate(&job.command, 23),
            placement_name(job.placement),
            state_name(job.state),
            cleanup_name(job.cleanup),
            duration,
            result,
            parent
        )
        .map_err(ProductError::from)?;
    }
    Ok(())
}

fn write_result(writer: &mut impl Write, value: serde_json::Value) -> Result<(), ProductError> {
    let children = value
        .get("children")
        .and_then(serde_json::Value::as_array)
        .map(|children| {
            children
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let receipt: marsh_daemon::JobReceipt = serde_json::from_value(value).map_err(|error| {
        ProductError::Operation(format!("cannot decode daemon result: {error}"))
    })?;
    writeln!(writer, "Result {}", receipt.cursor).map_err(ProductError::from)?;
    writeln!(writer, "  job:        {}", receipt.job_id).map_err(ProductError::from)?;
    writeln!(writer, "  command:    {}", receipt.command).map_err(ProductError::from)?;
    writeln!(
        writer,
        "  placement:  {}",
        placement_name(receipt.placement)
    )?;
    writeln!(writer, "  execution:  {}", typed_value(&receipt.execution)?)?;
    writeln!(writer, "  status:     {}", state_name(receipt.state)).map_err(ProductError::from)?;
    writeln!(
        writer,
        "  duration:   {}",
        format_duration(receipt.timing.wall_ms)
    )
    .map_err(ProductError::from)?;
    if let Some(exit) = receipt.exit {
        writeln!(
            writer,
            "  exit:       {} ({})",
            exit.code
                .map_or_else(|| "unknown".into(), |code| code.to_string()),
            exit.cause
        )
        .map_err(ProductError::from)?;
    }
    writeln!(writer, "  cleanup:    {}", cleanup_name(receipt.cleanup))
        .map_err(ProductError::from)?;
    writeln!(
        writer,
        "  output:     {}",
        if receipt.output_complete {
            "complete"
        } else {
            "incomplete"
        }
    )
    .map_err(ProductError::from)?;
    if let Some(worker) = receipt.worker_id {
        writeln!(writer, "  worker:     {worker}").map_err(ProductError::from)?;
    }
    if let Some(vm) = receipt.vm_id {
        writeln!(writer, "  VM:         {vm}")?;
    }
    if let Some(container) = receipt.container_id {
        writeln!(writer, "  container:  {container}").map_err(ProductError::from)?;
    }
    if let Some(lineage) = &receipt.lineage {
        writeln!(writer, "  parent:     {}", lineage.parent)?;
        if !lineage.root.is_empty() {
            writeln!(writer, "  root:       {}", lineage.root)?;
        }
        writeln!(writer, "  depth:      {}", lineage.depth)?;
        writeln!(writer, "  spawn:      {}", lineage.spawn.join(","))?;
        if let Some(split) = &lineage.split {
            let label = lineage.label.as_deref().unwrap_or("");
            writeln!(writer, "  split:      {split} {label}")?;
        }
    }
    if !children.is_empty() {
        writeln!(writer, "  children:   {children}")?;
    }
    Ok(())
}

fn typed_value(value: &impl serde::Serialize) -> Result<String, ProductError> {
    serde_json::to_value(value)
        .map(|value| match value {
            serde_json::Value::String(name) => name,
            other => other.to_string(),
        })
        .map_err(|error| ProductError::Operation(format!("cannot render result field: {error}")))
}

fn placement_name(placement: marsh_daemon::Placement) -> &'static str {
    match placement {
        marsh_daemon::Placement::Local => "local",
    }
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.into();
    }
    value
        .chars()
        .take(width.saturating_sub(1))
        .chain(['…'])
        .collect()
}

fn state_name(state: marsh_daemon::JobState) -> &'static str {
    match state {
        marsh_daemon::JobState::Queued => "queued",
        marsh_daemon::JobState::Running => "running",
        marsh_daemon::JobState::Finished => "finished",
        marsh_daemon::JobState::Failed => "failed",
        marsh_daemon::JobState::Cancelled => "cancelled",
        marsh_daemon::JobState::Unknown => "unknown",
    }
}

fn cleanup_name(state: marsh_daemon::CleanupState) -> &'static str {
    match state {
        marsh_daemon::CleanupState::Pending => "pending",
        marsh_daemon::CleanupState::Verified => "verified",
        marsh_daemon::CleanupState::Uncertain => "uncertain",
        marsh_daemon::CleanupState::NotRequired => "not_required",
    }
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds < 1_000 {
        format!("{milliseconds}ms")
    } else {
        format!(
            "{}.{:01}s",
            milliseconds / 1_000,
            (milliseconds % 1_000) / 100
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cli::LoadSelection, client::ExecuteRequest};
    use serde_json::json;
    use std::{
        path::PathBuf,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    #[derive(Default)]
    struct FakeClient {
        fail_prepare: bool,
        detached: AtomicBool,
        prepared: AtomicBool,
        opened: Mutex<Option<(SessionConfig, Vec<OsString>)>>,
    }

    impl DaemonClient for FakeClient {
        fn attach_shell(&self, _: u32, _: &SessionConfig) -> Result<String, ClientError> {
            Ok("01234567-89ab-cdef-0123-456789abcdef".into())
        }

        fn detach_shell(&self, _: &str) -> Result<(), ClientError> {
            self.detached.store(true, Ordering::Relaxed);
            Ok(())
        }

        fn open_shell(
            &self,
            session: &SessionConfig,
            arguments: &[OsString],
        ) -> Result<i32, ClientError> {
            *self.opened.lock().unwrap() = Some((session.clone(), arguments.to_vec()));
            Ok(0)
        }

        fn registered_commands(&self) -> Result<Vec<String>, ClientError> {
            Ok(vec!["fixture".into()])
        }

        fn prepare(
            &self,
            _selection: &LoadSelection,
            _session: &SessionConfig,
        ) -> Result<crate::client::LoadReport, ClientError> {
            self.prepared.store(true, Ordering::Relaxed);
            if self.fail_prepare {
                return Err(ClientError("prepare failed".into()));
            }
            Ok(crate::client::LoadReport {
                cold_kits: vec!["fixture".into()],
                sandboxes: std::collections::BTreeMap::default(),
            })
        }

        fn reset_workers(
            &self,
            selection: &LoadSelection,
        ) -> Result<crate::client::WorkerResetReport, ClientError> {
            let kits = match selection {
                LoadSelection::All => vec!["fixture".into()],
                LoadSelection::Kits(kits) => kits.clone(),
            };
            Ok(crate::client::WorkerResetReport { kits })
        }

        fn status_json(&self) -> Result<serde_json::Value, ClientError> {
            Ok(json!({"schema": "marsh.status/v1"}))
        }

        fn jobs_json(&self) -> Result<serde_json::Value, ClientError> {
            Ok(json!({
                "schema": "marsh.jobs/v1",
                "jobs": [{
                    "cursor": 7,
                    "job_id": "01234567-89ab-cdef-0123-456789abcdef",
                    "command": "fixture",
                    "state": "finished",
                    "placement": "local",
                    "cleanup": "verified",
                    "exit_code": 0,
                    "wall_ms": 1420
                }]
            }))
        }

        fn job_json(&self, _: &str) -> Result<serde_json::Value, ClientError> {
            Ok(json!({
                "schema": "marsh.job/v1",
                "cursor": 7,
                "job_id": "01234567-89ab-cdef-0123-456789abcdef",
                "attempt_id": "11111111-2222-3333-4444-555555555555",
                "session_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                "command": "fixture",
                "state": "finished",
                "kit_profile": "fixture",
                "image": "fixture@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "mounts": [],
                "worker_id": "worker-1",
                "vm_id": "vm-1",
                "container_id": null,
                "exit": {"code": 0, "cause": "exited"},
                "output_complete": true,
                "cleanup": "verified",
                "timing": {
                    "durations_ms": {},
                    "milestones_unix_ms": {},
                    "wall_ms": 1420,
                    "orchestration_ms": 20
                },
                "created_unix_ms": 1000,
                "finished_unix_ms": 2420
            }))
        }

        fn execute(&self, _: ExecuteRequest) -> Result<i32, ClientError> {
            Ok(0)
        }
    }

    fn session() -> SessionConfig {
        SessionConfig {
            session_id: None,
            username: "user".into(),
            uid: 501,
            gid: 20,
            launch_directory: PathBuf::from("/Users/user/project"),
            guest_home: PathBuf::from("/Users/user"),
            home_backing: PathBuf::from("/tmp/home"),
            ephemeral_home: false,
        }
    }

    fn component(
        kind: &str,
        label: &str,
        state: marsh_daemon::ScopeCleanupState,
        vm: Option<&str>,
        detail: Option<&str>,
    ) -> marsh_daemon::ScopeCleanupComponent {
        marsh_daemon::ScopeCleanupComponent {
            kind: kind.into(),
            label: label.into(),
            state,
            vm: vm.map(Into::into),
            detail: detail.map(Into::into),
        }
    }

    #[test]
    fn lifecycle_report_names_only_removed_vms_and_fails_on_uncertainty() {
        use marsh_daemon::{ScopeCleanupState as S, ScopeLifecycleAction, ScopeLifecycleReport};
        let home = std::path::Path::new("/Users/x/.marsh");
        let stopped = ScopeLifecycleReport {
            action: ScopeLifecycleAction::Stop,
            cleanup_complete: true,
            components: vec![
                component("kit", "fixture", S::Removed, Some("marsh-k-ab12cd34"), None),
                component("kit", "other", S::Absent, Some("marsh-k-zz000000"), None),
                component("kit", "stale-generations", S::Absent, None, None),
                component(
                    "shell",
                    "marsh-s-9ntzlqaj",
                    S::Removed,
                    Some("marsh-s-9ntzlqaj"),
                    None,
                ),
            ],
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ProductOutcome::Complete(status) =
            write_lifecycle_report(&stopped, home, false, &mut out, &mut err).unwrap()
        else {
            panic!("lifecycle report must complete");
        };
        assert_eq!(status, 0);
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "marsh: stopped /Users/x/.marsh: 2 VMs removed (marsh-k-ab12cd34, marsh-s-9ntzlqaj), daemon stopped\n"
        );

        let idle = ScopeLifecycleReport {
            action: ScopeLifecycleAction::Reset,
            cleanup_complete: true,
            components: vec![component("kit", "stale-generations", S::Absent, None, None)],
        };
        assert_eq!(
            lifecycle_summary(&idle, home),
            "marsh: reset /Users/x/.marsh: no VMs were running\nmarsh: daemon still running; the next shell starts fresh VMs\n"
        );

        let partial = ScopeLifecycleReport {
            action: ScopeLifecycleAction::Reset,
            cleanup_complete: false,
            components: vec![
                component(
                    "shell",
                    "marsh-s-9ntzlqaj",
                    S::Removed,
                    Some("marsh-s-9ntzlqaj"),
                    None,
                ),
                component(
                    "kit",
                    "fixture",
                    S::CleanupUncertain,
                    Some("marsh-k-ab12cd34"),
                    Some("Kit VM cleanup could not be verified"),
                ),
            ],
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ProductOutcome::Complete(status) =
            write_lifecycle_report(&partial, home, true, &mut out, &mut err).unwrap()
        else {
            panic!("lifecycle report must complete");
        };
        assert_eq!(status, 1);
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(json["components"][1]["vm"], "marsh-k-ab12cd34");
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "marsh: reset incomplete for /Users/x/.marsh: 1 VM removed (marsh-s-9ntzlqaj)\n\
             marsh: left Kit fixture VM marsh-k-ab12cd34: Kit VM cleanup could not be verified\n\
             marsh: reset exited 1; anything left is still recorded for this home. Fix the cause above, then rerun `marsh reset`\n"
        );
    }

    #[test]
    fn inspection_json_never_leaks_to_stderr() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = prepare(
            ParsedCli {
                command: ProductCommand::Status { json: true },
                brush_args: vec!["marsh".into()],
                load: None,
                ephemeral_home: false,
                session_id: None,
                ephemeral_home_backing: None,
                guest: false,
                shell: None,
            },
            Arc::new(FakeClient::default()),
            session(),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        assert_eq!(result, ProductOutcome::Complete(0));
        assert_eq!(stdout, b"{\"schema\":\"marsh.status/v1\"}\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn worker_reset_is_a_host_command_and_explains_the_next_step() {
        let mut stdout = Vec::new();
        let outcome = prepare(
            ParsedCli {
                command: ProductCommand::WorkersReset {
                    selection: LoadSelection::Kits(vec!["fixture".into()]),
                },
                brush_args: vec!["marsh".into()],
                load: None,
                ephemeral_home: false,
                session_id: None,
                ephemeral_home_backing: None,
                guest: false,
                shell: None,
            },
            Arc::new(FakeClient::default()),
            session(),
            &mut stdout,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(outcome, ProductOutcome::Complete(0));
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Reset worker VMs for: fixture"));
        assert!(output.contains("marsh --load all"));
    }

    #[test]
    fn failed_host_prewarm_detaches_the_session() {
        let client = Arc::new(FakeClient {
            fail_prepare: true,
            ..FakeClient::default()
        });
        let error = prepare(
            ParsedCli {
                command: ProductCommand::Shell,
                brush_args: vec!["marsh".into(), "-c".into(), "true".into()],
                load: Some(LoadSelection::Kits(vec!["fixture".into()])),
                ephemeral_home: false,
                session_id: None,
                ephemeral_home_backing: None,
                guest: false,
                shell: None,
            },
            client.clone(),
            session(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "prepare failed");
        assert!(client.detached.load(Ordering::Relaxed));
    }

    #[test]
    fn ephemeral_backing_is_handed_to_the_guest_shell() {
        let client = Arc::new(FakeClient::default());
        let mut session = session();
        session.ephemeral_home = true;
        session.home_backing = PathBuf::from("/private/tmp/ephemeral-session");
        let result = prepare(
            ParsedCli {
                command: ProductCommand::Shell,
                brush_args: vec!["marsh".into(), "-c".into(), "true".into()],
                load: None,
                ephemeral_home: true,
                session_id: None,
                ephemeral_home_backing: None,
                guest: false,
                shell: None,
            },
            client.clone(),
            session.clone(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(result, ProductOutcome::Complete(0));

        let opened = client.opened.lock().unwrap();
        let (opened_session, arguments) = opened.as_ref().unwrap();
        assert_eq!(opened_session.home_backing, session.home_backing);
        assert!(opened_session.ephemeral_home);
        assert_eq!(
            arguments,
            &[
                "marsh",
                "--marsh-guest",
                "--marsh-session",
                "01234567-89ab-cdef-0123-456789abcdef",
                "--ephemeral-home",
                "--marsh-home-backing",
                "/private/tmp/ephemeral-session",
                "-c",
                "true",
            ]
        );
        assert!(
            !client.detached.load(Ordering::Relaxed),
            "server owns post-open cleanup"
        );
    }

    #[test]
    fn background_reservation_never_treats_raw_operands_or_shell_syntax_as_identity() {
        assert_eq!(
            background_acp_adapter(b"acp run agent-1_2"),
            Some("agent-1_2")
        );
        for source in [
            b"printf '%s' 'acp run agent'".as_slice(),
            b"acp run agent\xff",
            b"acp run agent \xff",
            b"acp run $(touch marker)",
            b"acp run agent; touch marker",
            b"acp run \"agent\"",
            b"acp run",
        ] {
            assert_eq!(background_acp_adapter(source), None);
        }
    }

    #[test]
    fn guest_handoff_preserves_raw_source_operands_and_ephemeral_backing() {
        use std::os::unix::ffi::OsStringExt as _;
        let client = Arc::new(FakeClient::default());
        let arguments = vec![
            OsString::from_vec(b"marsh-\xff".to_vec()),
            "-c".into(),
            OsString::from_vec(b"printf '%s' '\xfe'".to_vec()),
            OsString::from_vec(b"arg0-\xfe".to_vec()),
            OsString::from_vec(vec![0xff, 0xfe]),
        ];
        let mut session = session();
        session.ephemeral_home = true;
        session.home_backing = OsString::from_vec(b"/tmp/home-\xfe".to_vec()).into();
        let mut parsed = crate::cli::parse_os(arguments.clone()).unwrap();
        parsed.ephemeral_home = true;
        let result = prepare(
            parsed,
            client.clone(),
            session.clone(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(result, ProductOutcome::Complete(0));
        let opened = client.opened.lock().unwrap();
        let (_, received) = opened.as_ref().unwrap();
        assert_eq!(received[0], arguments[0]);
        assert_eq!(received[6], session.home_backing.as_os_str());
        assert_eq!(received[7..], arguments[1..]);
    }
}
