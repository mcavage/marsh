use brush_parser::ast::{self, CommandPrefixOrSuffixItem};
use itertools::Itertools;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arithmetic::{self, ExpandAndEvaluate};
use crate::commands::{self, CommandArg};
use crate::env::{EnvironmentLookup, EnvironmentScope, valid_variable_name};
use crate::openfiles::{OpenFile, OpenFiles};
use crate::results::{
    ExecutionExitCode, ExecutionResult, ExecutionSpawnResult, ExecutionWaitResult,
};
use crate::shell::Shell;
use crate::variables::{
    ArrayLiteral, ShellValue, ShellValueLiteral, ShellValueUnsetType, ShellVariable,
};
use crate::{
    ShellFd, error, expansion, extendedtests, extensions, ioutils, jobs, openfiles, processes, sys,
    timing,
};

/// Encapsulates the context of execution in a command pipeline.
struct PipelineExecutionContext<'a, SE: extensions::ShellExtensions> {
    /// The shell in which the command should be executed.
    shell: commands::ShellForCommand<'a, SE>,
    /// Process group ID for spawned processes.
    process_group_id: Option<i32>,
}

/// Parameters for execution.
#[derive(Clone, Default)]
pub struct ExecutionParameters {
    /// The open files tracked by the current context.
    open_files: openfiles::OpenFiles,
    /// Policy for how to manage spawned external processes.
    pub process_group_policy: ProcessGroupPolicy,
    /// Whether `errexit` (exit on error) behavior should be
    /// suppressed in this execution context. Defaults to `false`.
    pub suppress_errexit: bool,
}

impl ExecutionParameters {
    /// Returns the standard input file; usable with `write!` et al.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stdin(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl std::io::Read + 'static {
        self.try_stdin(shell).unwrap_or_else(|| {
            ioutils::FailingReaderWriter::new("standard input not available").into()
        })
    }

    /// Tries to retrieve the standard input file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stdin(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDIN_FD)
    }

    /// Returns the standard output file; usable with `write!` et al. In the event that
    /// no such file is available, returns a valid implementation of `std::io::Write`
    /// that fails all I/O requests.
    ///
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stdout(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl std::io::Write + 'static {
        self.try_stdout(shell).unwrap_or_else(|| {
            ioutils::FailingReaderWriter::new("standard output not available").into()
        })
    }

    /// Tries to retrieve the standard output file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stdout(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDOUT_FD)
    }

    /// Returns the standard error file; usable with `write!` et al. In the event that
    /// no such file is available, returns a valid implementation of `std::io::Write`
    /// that fails all I/O requests.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn stderr(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl std::io::Write + 'static {
        self.try_stderr(shell).unwrap_or_else(|| {
            ioutils::FailingReaderWriter::new("standard error not available").into()
        })
    }

    /// Tries to retrieve the standard error file. Returns `None` if not set.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn try_stderr(&self, shell: &Shell<impl extensions::ShellExtensions>) -> Option<OpenFile> {
        self.try_fd(shell, openfiles::OpenFiles::STDERR_FD)
    }

    /// Returns the file descriptor with the given number. Returns `None`
    /// if the file descriptor is not open.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    /// * `fd` - The file descriptor number to retrieve.
    pub fn try_fd(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
        fd: ShellFd,
    ) -> Option<openfiles::OpenFile> {
        match self.open_files.fd_entry(fd) {
            openfiles::OpenFileEntry::Open(f) => Some(f.clone()),
            openfiles::OpenFileEntry::NotPresent => None,
            openfiles::OpenFileEntry::NotSpecified => {
                // We didn't have this fd specified one way or the other; we fallback
                // to what's represented in the shell's open files.
                shell.persistent_open_files().try_fd(fd).cloned()
            }
        }
    }

    /// Sets the given file descriptor to the provided open file.
    ///
    /// # Arguments
    ///
    /// * `fd` - The file descriptor number to set.
    /// * `file` - The open file to set.
    pub fn set_fd(&mut self, fd: ShellFd, file: openfiles::OpenFile) {
        self.open_files.set_fd(fd, file);
    }

    /// Iterates over all open file descriptors in this context.
    ///
    /// # Arguments
    ///
    /// * `shell` - The shell context.
    pub fn iter_fds(
        &self,
        shell: &Shell<impl extensions::ShellExtensions>,
    ) -> impl Iterator<Item = (ShellFd, openfiles::OpenFile)> {
        let our_fds = self.open_files.iter_fds();
        let shell_fds = shell
            .persistent_open_files()
            .iter_fds()
            .filter(|(fd, _)| !self.open_files.contains_fd(*fd));

        #[allow(clippy::needless_collect)]
        let all_fds: Vec<_> = our_fds
            .chain(shell_fds)
            .map(|(fd, file)| (fd, file.clone()))
            .collect();

        all_fds.into_iter()
    }
}

#[derive(Clone, Debug, Default)]
/// Policy for how to manage spawned external processes.
pub enum ProcessGroupPolicy {
    /// Place the process in a new process group.
    #[default]
    NewProcessGroup,
    /// Place the process in the same process group as its parent.
    SameProcessGroup,
}

#[async_trait::async_trait]
pub trait Execute {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error>;
}

#[async_trait::async_trait]
trait ExecuteInPipeline<SE: extensions::ShellExtensions> {
    async fn execute_in_pipeline(
        &self,
        context: PipelineExecutionContext<'_, SE>,
        params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error>;
}

#[async_trait::async_trait]
impl Execute for ast::Program {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();

        for command in &self.complete_commands {
            result = execute_complete_command(shell, command, params).await;

            // Check if we should stop executing subsequent commands
            if !result.is_normal_flow() {
                break;
            }
        }

        Ok(result)
    }
}

/// Executes one complete command of a program, reporting any error.
pub(crate) async fn execute_complete_command(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    command: &ast::CompleteCommand,
    params: &ExecutionParameters,
) -> ExecutionResult {
    // Execute the command and handle any errors without immediately propagating them.
    // This allows interactive shells to continue executing subsequent commands even after
    // errors.
    let result = match command.execute(shell, params).await {
        Ok(exec_result) => exec_result,
        Err(err) => {
            // Display the error and convert to an execution result.
            let _ = shell.display_error(&mut params.stderr(shell), &err);
            err.into_result(shell)
        }
    };

    // Update status
    shell.set_last_exit_status(result.exit_code.into());
    result
}

#[async_trait::async_trait]
impl Execute for ast::CompoundList {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();

        for ast::CompoundListItem(ao_list, sep) in &self.0 {
            let run_async = matches!(sep, ast::SeparatorOperator::Async);

            if run_async {
                let job = spawn_async_ao_list(ao_list, shell, params).await?;
                let job_formatted = job.to_pid_style_string();

                if shell.options().interactive && !shell.is_subshell() {
                    writeln!(params.stderr(shell), "{job_formatted}")?;
                }

                result = ExecutionResult::success();
            } else {
                result = ao_list.execute(shell, params).await?;

                // Update status
                shell.set_last_exit_status(result.exit_code.into());
            }

            if !result.is_normal_flow() {
                break;
            }
        }

        Ok(result)
    }
}

async fn spawn_async_ao_list<'a, SE: extensions::ShellExtensions>(
    ao_list: &ast::AndOrList,
    shell: &'a mut Shell<SE>,
    params: &ExecutionParameters,
) -> Result<&'a jobs::Job, error::Error> {
    let prelaunch_environment = if let Some(command) = background_simple_command(ao_list, shell) {
        // A failed embedding reservation must not make `&` abort the rest of
        // the shell list. Let the child run and report its own failure.
        commands::background_prelaunch_environment(&command).unwrap_or_default()
    } else {
        Vec::new()
    };
    if ao_list.additional.is_empty()
        && composition_shape(&ao_list.first).is_none()
        && crate::split_interp::split_shape(&ao_list.first).is_none()
    {
        let pipefail = shell.options().return_last_failure_from_pipeline;
        let mut child_shell = shell.clone();
        apply_background_environment(&mut child_shell, &prelaunch_environment)?;
        child_shell.options_mut().interactive = false;
        let mut child_params = params.clone();
        child_params.process_group_policy = ProcessGroupPolicy::NewProcessGroup;
        if let Ok(null) = openfiles::null() {
            child_params.set_fd(openfiles::OpenFiles::STDIN_FD, null);
        }

        let spawned =
            spawn_pipeline_processes(&ao_list.first, &mut child_shell, &child_params, true).await?;
        let tasks = spawned.into_iter().map(|spawn_result| match spawn_result {
            ExecutionSpawnResult::StartedProcess(process) => jobs::JobTask::External(process),
            ExecutionSpawnResult::StartedTask(task) => jobs::JobTask::Internal(task),
            ExecutionSpawnResult::Completed(result) => {
                jobs::JobTask::Internal(tokio::spawn(async move { Ok(result) }))
            }
        });
        return Ok(shell.jobs_mut().add_as_background(jobs::Job::new(
            tasks,
            ao_list.to_string(),
            jobs::JobState::Running,
            pipefail,
        )));
    }

    // Clone the inputs.
    let mut cloned_shell = shell.clone();
    apply_background_environment(&mut cloned_shell, &prelaunch_environment)?;
    let mut cloned_params = params.clone();
    let cloned_ao_list = ao_list.clone();

    // Mark the child shell as not interactive; we don't want it messing with the terminal too much.
    cloned_shell.options_mut().interactive = false;

    // Redirect stdin to null, per spec.
    if let Ok(null) = openfiles::null() {
        cloned_params.set_fd(openfiles::OpenFiles::STDIN_FD, null);
    }

    // Like Bash, run the list in its own process.
    #[cfg(unix)]
    {
        let mut child_params = cloned_params.clone();
        child_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
        let ignore_interrupts = !shell.options().enable_job_control;
        let list = ao_list.clone();
        let mut child_shell = cloned_shell.clone();
        child_shell.set_depth(cloned_shell.depth());
        if let Some(child) = fork_child(
            child_shell,
            child_params,
            crate::subshell::ChildGroup::New { foreground: false },
            ignore_interrupts,
            Box::new(move |shell, params| {
                Box::pin(async move {
                    match list.execute(shell, &params).await {
                        Ok(result) => result,
                        Err(error) => {
                            let _ = shell.display_error(&mut params.stderr(shell), &error);
                            error.into_result(shell)
                        }
                    }
                })
            }),
        )? {
            let pipefail = shell.options().return_last_failure_from_pipeline;
            return Ok(shell.jobs_mut().add_as_background(jobs::Job::new(
                [jobs::JobTask::External(child)],
                ao_list.to_string(),
                jobs::JobState::Running,
                pipefail,
            )));
        }
    }

    let join_handle = tokio::spawn(async move {
        cloned_ao_list
            .execute(&mut cloned_shell, &cloned_params)
            .await
    });

    Ok(shell.jobs_mut().add_as_background(jobs::Job::new(
        [jobs::JobTask::Internal(join_handle)],
        ao_list.to_string(),
        jobs::JobState::Running,
        false, // The internal task already computes its pipeline status.
    )))
}

fn background_simple_command<SE: extensions::ShellExtensions>(
    ao_list: &ast::AndOrList,
    shell: &Shell<SE>,
) -> Option<String> {
    // The embedding cannot install a reservation into a readonly shell variable.
    if shell
        .env()
        .get("MARSH_ACP_RESERVATION_ID")
        .is_some_and(|(_, variable)| variable.is_readonly())
    {
        return None;
    }
    if !ao_list.additional.is_empty() || ao_list.first.seq.len() != 1 {
        return None;
    }
    let ast::Command::Simple(command) = ao_list.first.seq.first()? else {
        return None;
    };
    if command.prefix.is_some() {
        return None;
    }
    let name = command.word_or_name.as_ref()?.to_string();
    if shell.funcs().get(&name).is_some()
        || (shell.options().expand_aliases && shell.aliases().contains_key(&name))
    {
        return None;
    }
    let mut words = vec![name];
    for item in command.suffix.iter().flat_map(|suffix| &suffix.0) {
        match item {
            ast::CommandPrefixOrSuffixItem::Word(word) => words.push(word.to_string()),
            ast::CommandPrefixOrSuffixItem::IoRedirect(_) => {}
            _ => return None,
        }
    }
    Some(words.join(" "))
}

fn apply_background_environment<SE: extensions::ShellExtensions>(
    shell: &mut Shell<SE>,
    environment: &[(String, String)],
) -> Result<(), error::Error> {
    for (name, value) in environment {
        let mut variable = ShellVariable::new(value.clone());
        variable.export();
        shell.env_mut().set_global(name.clone(), variable)?;
    }
    Ok(())
}

/// The body a forked child runs on its own runtime.
#[cfg(unix)]
pub(crate) type ForkedBody<SE> = Box<
    dyn for<'a> FnOnce(
            &'a mut Shell<SE>,
            ExecutionParameters,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = ExecutionResult> + 'a>>
        + Send,
>;

/// Forks `child_shell` into a real process that runs `run`, then exits with
/// its status (after any `EXIT` trap the child itself set). Returns `None`
/// when real-process subshells are unavailable.
#[cfg(unix)]
pub(crate) fn fork_child<SE: extensions::ShellExtensions>(
    mut child_shell: Shell<SE>,
    child_params: ExecutionParameters,
    group: crate::subshell::ChildGroup,
    ignore_interrupts: bool,
    run: ForkedBody<SE>,
) -> Result<Option<processes::ChildProcess>, error::Error> {
    if !crate::subshell::available() {
        return Ok(None);
    }
    let term_trapped = child_shell
        .traps()
        .handles(crate::traps::TrapSignal::Signal(
            sys::signal::Signal::SIGTERM,
        ));
    let signals = crate::subshell::ChildSignals {
        ignore_interrupts,
        default_stop_signals: child_shell.options().enable_job_control,
        ignored: child_shell
            .traps()
            .iter_handlers()
            .filter_map(|(signal, handler)| match signal {
                crate::traps::TrapSignal::Signal(signal) if handler.command.is_empty() => {
                    Some(signal as i32)
                }
                _ => None,
            })
            .collect(),
    };
    let keep = child_params
        .iter_fds(&child_shell)
        .chain(
            child_shell
                .persistent_open_files()
                .iter_fds()
                .map(|(fd, file)| (fd, file.clone())),
        )
        .filter_map(|(_, file)| {
            use std::os::fd::AsRawFd as _;
            file.try_borrow_as_fd().ok().map(|fd| fd.as_raw_fd())
        })
        .collect();
    child_shell.set_owns_process();
    let forked = crate::subshell::fork(group, &signals, keep, move || async move {
        let mut shell = child_shell;
        let generation = shell.traps().generation();
        let result = run(&mut shell, child_params).await;
        shell.set_last_exit_status(result.exit_code.into());
        if shell.traps().generation() != generation
            && shell.traps().handles(crate::traps::TrapSignal::Exit)
        {
            let _ = shell.on_exit().await;
        }
        i32::from(u8::from(result.exit_code))
    })?;
    Ok(forked.map(|forked| processes::ChildProcess::from_forked(forked, term_trapped)))
}

/// Runs a list in the current (forked) shell, reporting errors as Bash would.
#[cfg(unix)]
pub(crate) async fn run_list_reporting_errors<SE: extensions::ShellExtensions>(
    list: &ast::CompoundList,
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
) -> ExecutionResult {
    match list.execute(shell, params).await {
        Ok(result) => result,
        Err(error) => {
            let _ = shell.display_error(&mut params.stderr(shell), &error);
            error.into_result(shell)
        }
    }
}

/// Runs traps for caught signals that no wait delivered; an untrapped one ends
/// a noninteractive shell (after its `EXIT` trap) as its default action would.
#[cfg(unix)]
async fn deliver_pending_signals<SE: extensions::ShellExtensions>(
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
) -> Result<Option<ExecutionResult>, error::Error> {
    for signal in crate::signals::take_pending() {
        let Ok(caught) = sys::signal::Signal::try_from(signal) else {
            continue;
        };
        let trap = crate::traps::TrapSignal::Signal(caught);
        match shell
            .traps()
            .get_handler(trap)
            .map(|h| h.command.is_empty())
        {
            Some(true) => {}
            Some(false) => {
                let result = shell.invoke_trap_handler(trap, params).await?;
                if !result.is_normal_flow() {
                    return Ok(Some(result));
                }
            }
            None if !shell.options().interactive => {
                let _ = shell.on_exit().await;
                crate::subshell::die_by_signal(signal);
            }
            // An interactive shell abandons the command line on an untrapped INT.
            None if signal == libc::SIGINT => {
                // End the echoed `^C` line, as Bash does.
                let _ = writeln!(params.stderr(shell));
                shell.set_last_exit_status(130);
                return Ok(Some(ExecutionResult::interrupted()));
            }
            None => {}
        }
    }
    Ok(None)
}

/// Whether `list` is exactly one simple command, which a forked child may `exec`.
fn is_single_simple_command(list: &ast::CompoundList) -> bool {
    matches!(
        list.0.as_slice(),
        [ast::CompoundListItem(ao, ast::SeparatorOperator::Sequence)]
            if ao.additional.is_empty()
                && !ao.first.bang
                && ao.first.timed.is_none()
                && matches!(ao.first.seq.as_slice(), [ast::Command::Simple(_)])
    )
}

/// Forks a `( ... )` subshell running `list`.
#[cfg(unix)]
fn fork_subshell_list<SE: extensions::ShellExtensions>(
    shell: &Shell<SE>,
    params: &ExecutionParameters,
    list: &ast::CompoundList,
    process_group_id: Option<i32>,
) -> Result<Option<processes::ChildProcess>, error::Error> {
    if !crate::subshell::available() {
        return Ok(None);
    }
    let mut subshell = shell.clone();
    subshell.jobs_mut().clear_inherited();
    subshell.set_exec_in_place(is_single_simple_command(list));
    let stdin_is_terminal = params
        .try_fd(shell, OpenFiles::STDIN_FD)
        .is_some_and(|file| file.is_terminal());
    let group = crate::subshell::ChildGroup::for_policy(
        &params.process_group_policy,
        process_group_id,
        stdin_is_terminal,
    );
    let mut child_params = params.clone();
    child_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
    let list = list.clone();
    fork_child(
        subshell,
        child_params,
        group,
        false,
        Box::new(move |shell, params| {
            Box::pin(async move { run_list_reporting_errors(&list, shell, &params).await })
        }),
    )
}

/// Runs one pipeline stage inside its forked child.
#[cfg(unix)]
async fn run_forked_stage<SE: extensions::ShellExtensions>(
    command: &ast::Command,
    shell: &mut Shell<SE>,
    mut params: ExecutionParameters,
) -> ExecutionResult {
    if let ast::Command::Compound(ast::CompoundCommand::Subshell(subshell), redirects) = command {
        // The stage process is already the subshell; Bash does not fork again.
        shell.set_depth(shell.depth() + 1);
        shell.jobs_mut().clear_inherited();
        shell.set_exec_in_place(is_single_simple_command(&subshell.list));
        if let Some(redirects) = redirects {
            for redirect in &redirects.0 {
                if let Err(error) = setup_redirect(shell, &mut params, redirect).await {
                    let _ = shell.display_error(&mut params.stderr(shell), &error);
                    return error.into_result(shell);
                }
            }
        }
        return run_list_reporting_errors(&subshell.list, shell, &params).await;
    }
    let context = PipelineExecutionContext {
        shell: commands::ShellForCommand::ParentShell(shell),
        process_group_id: None,
    };
    let spawned = command.execute_in_pipeline(context, params.clone()).await;
    let waited = match spawned {
        Ok(spawned) => spawned.wait().await,
        Err(error) => Err(error),
    };
    match waited {
        Ok(ExecutionWaitResult::Completed { result, .. }) => result,
        Ok(ExecutionWaitResult::Stopped(_)) => ExecutionResult::stopped(),
        Err(error) => {
            let _ = shell.display_error(&mut params.stderr(shell), &error);
            error.into_result(shell)
        }
    }
}

#[async_trait::async_trait]
impl Execute for ast::AndOrList {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let has_operators = !self.additional.is_empty();

        // For the first command, suppress errexit if there are more commands after it
        let mut first_params = params.clone();
        if has_operators {
            first_params.suppress_errexit = true;
        }

        let mut result = self.first.execute(shell, &first_params).await?;

        for (index, next_ao) in self.additional.iter().enumerate() {
            // Check for non-normal control flow.
            if !result.is_normal_flow() {
                break;
            }

            let (is_and, pipeline) = match next_ao {
                ast::AndOr::And(p) => (true, p),
                ast::AndOr::Or(p) => (false, p),
            };

            // If we short-circuit, then we don't break out of the whole loop
            // but we skip evaluating the current pipeline. We'll then continue
            // on and possibly evaluate a subsequent one (depending on the
            // operator before it).
            if is_and {
                if !result.is_success() {
                    continue;
                }
            } else if result.is_success() {
                continue;
            }

            // For the last command in the chain, use original params (errexit not suppressed)
            // For earlier commands, suppress errexit
            let mut params = params.clone();

            let is_last = index == self.additional.len() - 1;
            if !is_last {
                params.suppress_errexit = true;
            }

            result = pipeline.execute(shell, &params).await?;
        }

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::Pipeline {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let split = crate::split_interp::split_shape(self);
        if split.is_some() || composition_shape(self).is_some() {
            let stopwatch = self
                .timed
                .is_some()
                .then(timing::start_timing)
                .transpose()?;
            let result = if let Some(split) = split {
                crate::split_interp::execute_split_pipeline(self, split, shell, params).await?
            } else if let Some(shape) = composition_shape(self) {
                execute_composition_pipeline(self, shape, shell, params).await?
            } else {
                unreachable!()
            };
            if let (Some(timed), Some(stopwatch)) = (&self.timed, stopwatch)
                && let Some(mut stderr) = params.try_fd(shell, openfiles::OpenFiles::STDERR_FD)
            {
                let measured = stopwatch.stop()?;
                if timed.is_posix_output() {
                    write!(
                        stderr,
                        "real {}\nuser {}\nsys {}\n",
                        timing::format_duration_posixly(&measured.wall),
                        timing::format_duration_posixly(&measured.user),
                        timing::format_duration_posixly(&measured.system),
                    )?;
                } else {
                    write!(
                        stderr,
                        "\nreal\t{}\nuser\t{}\nsys\t{}\n",
                        timing::format_duration_non_posixly(&measured.wall),
                        timing::format_duration_non_posixly(&measured.user),
                        timing::format_duration_non_posixly(&measured.system),
                    )?;
                }
            }
            return Ok(result);
        }

        // Capture current timing if so requested.
        let stopwatch = self
            .timed
            .is_some()
            .then(timing::start_timing)
            .transpose()?;

        let mut params = params.clone();

        // If this pipeline is negated, suppress errexit for commands within it
        if self.bang {
            params.suppress_errexit = true;
        }

        // Spawn all the processes required for the pipeline, connecting outputs/inputs with pipes
        // as needed.
        let spawn_results = spawn_pipeline_processes(self, shell, &params, false).await?;

        // Wait for the processes. This also has a side effect of updating pipeline status.
        let mut result =
            wait_for_pipeline_processes_and_update_status(self, spawn_results, shell, &params)
                .await?;

        // Invert the exit code if requested.
        if self.bang {
            result.exit_code = ExecutionExitCode::from(if result.is_success() { 1 } else { 0 });
        }

        // Update exit status.
        shell.set_last_exit_status(result.exit_code.into());

        // Fire the ERR trap if the pipeline failed in a non-conditional context.
        // We reuse `suppress_errexit` here because bash suppresses the ERR trap in
        // exactly the same contexts it suppresses errexit (conditionals, `!`-prefixed
        // pipelines, etc.).
        if !result.is_success() && !params.suppress_errexit && !self.bang {
            if shell.traps().handles(crate::traps::TrapSignal::Err) {
                shell
                    .invoke_trap_handler(crate::traps::TrapSignal::Err, &params)
                    .await?;
            }
        }

        // Apply errexit if not suppressed (and not negated)
        if !params.suppress_errexit && !self.bang {
            shell.apply_errexit_if_enabled(&mut result);
        }

        // Deliver signals that arrived while no wait was observing them.
        #[cfg(unix)]
        if let Some(trap_result) = deliver_pending_signals(shell, &params).await? {
            return Ok(trap_result);
        }

        // If requested, report timing.
        if let (Some(timed), Some(stopwatch)) = (&self.timed, &stopwatch)
            && let Some(mut stderr) = params.try_fd(shell, openfiles::OpenFiles::STDERR_FD)
        {
            let timing = stopwatch.stop()?;
            if timed.is_posix_output() {
                std::write!(
                    stderr,
                    "real {}\nuser {}\nsys {}\n",
                    timing::format_duration_posixly(&timing.wall),
                    timing::format_duration_posixly(&timing.user),
                    timing::format_duration_posixly(&timing.system),
                )?;
            } else {
                std::write!(
                    stderr,
                    "\nreal\t{}\nuser\t{}\nsys\t{}\n",
                    timing::format_duration_non_posixly(&timing.wall),
                    timing::format_duration_non_posixly(&timing.user),
                    timing::format_duration_non_posixly(&timing.system),
                )?;
            }
        }

        Ok(result)
    }
}

const COMPOSITION_INPUT_LIMIT: u64 = 64 * 1024 * 1024;
pub(crate) const COMPOSITION_OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
struct CompositionShape {
    fanout: usize,
    collect: Option<usize>,
}

fn composition_shape(pipeline: &ast::Pipeline) -> Option<CompositionShape> {
    let fanout = pipeline
        .seq
        .iter()
        .position(|command| matches!(command, ast::Command::Fanout(_)))?;
    let collect = pipeline
        .seq
        .iter()
        .position(|command| matches!(command, ast::Command::Collect(_)));
    if pipeline.seq.iter().enumerate().any(|(index, command)| {
        matches!(command, ast::Command::Fanout(_) | ast::Command::Collect(_))
            && index != fanout
            && Some(index) != collect
    }) || collect.is_some_and(|index| index != fanout + 1)
    {
        return None;
    }
    Some(CompositionShape { fanout, collect })
}

pub(crate) struct CompositionFile {
    file: std::fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl CompositionFile {
    pub(crate) fn create(kind: &str) -> std::io::Result<Self> {
        for _ in 0..32 {
            let path =
                std::env::temp_dir().join(format!(".marsh-{kind}-{:032x}", rand::random::<u128>()));
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    #[cfg(unix)]
                    std::fs::remove_file(&path)?;
                    return Ok(Self {
                        file,
                        #[cfg(not(unix))]
                        path,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "cannot allocate composition spool",
        ))
    }

    pub(crate) fn read(&self) -> std::io::Result<std::fs::File> {
        let mut file = self.file.try_clone()?;
        file.seek(std::io::SeekFrom::Start(0))?;
        Ok(file)
    }

    pub(crate) fn write(&self) -> std::io::Result<std::fs::File> {
        let mut file = self.file.try_clone()?;
        file.set_len(0)?;
        file.seek(std::io::SeekFrom::Start(0))?;
        Ok(file)
    }
}

impl Drop for CompositionFile {
    fn drop(&mut self) {
        #[cfg(not(unix))]
        let _ = std::fs::remove_file(&self.path);
    }
}

/// One finished `fanout` branch, as `collect` renders it (also used by the
/// `marsh fanout | marsh collect` CLI).
pub struct BranchResult {
    /// The branch label.
    pub label: String,
    /// The branch exit status.
    pub status: u8,
    /// The branch wall time.
    pub elapsed: std::time::Duration,
    /// The branch's captured stdout.
    pub stdout: Vec<u8>,
    /// The branch's captured stderr.
    pub stderr: Vec<u8>,
}

/// Spawns a thread and returns once it is running its closure.
///
/// A thread that is still starting holds the standard library's process-wide
/// thread registry lock (its stack overflow handler's, on Darwin). A `fork`
/// while that lock is held hands the child the lock held, with no thread left
/// to release it, so the child's first thread spawn (the host of its runtime)
/// never returns. Brush forks right after starting these threads, so it waits
/// for each to be past its start first.
fn spawn_running<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> std::thread::JoinHandle<T> {
    let (running, started) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _ = running.send(());
        work()
    });
    let _ = started.recv();
    handle
}

pub(crate) fn capture_bounded(
    mut reader: impl Read,
    remaining: &AtomicU64,
    exceeded: &AtomicBool,
    limit_reached: &tokio::sync::Notify,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok(output);
        }
        let requested = u64::try_from(read).unwrap_or(u64::MAX);
        let granted = remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |available| {
                Some(available.saturating_sub(requested))
            })
            .unwrap_or(0)
            .min(requested);
        output.extend_from_slice(&buffer[..usize::try_from(granted).unwrap_or(0)]);
        if granted != requested && !exceeded.swap(true, Ordering::Relaxed) {
            limit_reached.notify_one();
        }
    }
}

pub(crate) fn join_capture(
    capture: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> std::io::Result<Vec<u8>> {
    capture
        .join()
        .map_err(|_| std::io::Error::other("composition capture thread panicked"))?
}

fn collect_as_simple(command: &ast::CollectCommand) -> ast::SimpleCommand {
    let mut suffix = command
        .arguments
        .iter()
        .cloned()
        .map(ast::CommandPrefixOrSuffixItem::Word)
        .collect::<Vec<_>>();
    suffix.extend(
        command
            .redirects
            .iter()
            .flat_map(|redirects| redirects.0.iter().cloned())
            .map(ast::CommandPrefixOrSuffixItem::IoRedirect),
    );
    ast::SimpleCommand {
        prefix: None,
        word_or_name: Some(ast::Word {
            value: "collect".into(),
            loc: Some(command.loc.clone()),
        }),
        suffix: (!suffix.is_empty()).then_some(ast::CommandSuffix(suffix)),
    }
}

/// Runs the stages before a composition and spools their stdout (or this
/// pipeline's stdin when there are none), bounded by the composition input
/// limit. Returns `None` after reporting an oversized input.
pub(crate) async fn composition_input<SE: extensions::ShellExtensions>(
    prefix: &[ast::Command],
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
    statuses: &mut Vec<u8>,
) -> Result<Option<(u8, Vec<u8>)>, error::Error> {
    if prefix.is_empty() {
        let mut input = Vec::new();
        if !params
            .try_stdin(shell)
            .is_some_and(|stdin| stdin.is_terminal())
        {
            let mut reader = params.stdin(shell);
            reader
                .by_ref()
                .take(COMPOSITION_INPUT_LIMIT + 1)
                .read_to_end(&mut input)?;
        }
        if u64::try_from(input.len()).unwrap_or(u64::MAX) > COMPOSITION_INPUT_LIMIT {
            writeln!(params.stderr(shell), "fanout: input exceeds 64 MiB limit")?;
            return Ok(None);
        }
        return Ok(Some((0, input)));
    }
    let (reader, writer) = std::io::pipe()?;
    let remaining = Arc::new(AtomicU64::new(COMPOSITION_INPUT_LIMIT));
    let exceeded = Arc::new(AtomicBool::new(false));
    let limit_reached = Arc::new(tokio::sync::Notify::new());
    let capture = {
        let remaining = Arc::clone(&remaining);
        let exceeded = Arc::clone(&exceeded);
        let limit_reached = Arc::clone(&limit_reached);
        spawn_running(move || capture_bounded(reader, &remaining, &exceeded, &limit_reached))
    };
    let mut prefix_params = params.clone();
    prefix_params.set_fd(OpenFiles::STDOUT_FD, writer.into());
    let result = execute_composition_segment(prefix, shell, &prefix_params).await?;
    statuses.extend_from_slice(shell.last_pipeline_statuses());
    drop(prefix_params);
    let input = join_capture(capture)?;
    if exceeded.load(Ordering::Relaxed) {
        writeln!(params.stderr(shell), "fanout: input exceeds 64 MiB limit")?;
        return Ok(None);
    }
    Ok(Some((result.exit_code.into(), input)))
}

async fn execute_composition_pipeline<SE: extensions::ShellExtensions>(
    pipeline: &ast::Pipeline,
    shape: CompositionShape,
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
) -> Result<ExecutionResult, error::Error> {
    let ast::Command::Fanout(fanout) = &pipeline.seq[shape.fanout] else {
        unreachable!()
    };
    let collect = shape.collect.and_then(|index| match &pipeline.seq[index] {
        ast::Command::Collect(command) => Some(command),
        _ => None,
    });
    let typed_end = shape.collect.unwrap_or(shape.fanout) + 1;
    let has_suffix = typed_end < pipeline.seq.len();
    let rendered = has_suffix
        .then(|| CompositionFile::create("collect-output"))
        .transpose()?;
    let mut collect_params = params.clone();
    if let Some(rendered) = &rendered {
        collect_params.set_fd(OpenFiles::STDOUT_FD, rendered.write()?.into());
    }
    if let Some(redirects) = collect.and_then(|command| command.redirects.as_ref()) {
        for redirect in &redirects.0 {
            setup_redirect(shell, &mut collect_params, redirect).await?;
        }
    }
    let options = collect_options(collect, shell, &collect_params).await?;
    if options.invalid {
        return Ok(ExecutionResult::new(2));
    }
    let started = std::time::Instant::now();

    let mut statuses = Vec::new();
    let Some((prefix_status, input)) =
        composition_input(&pipeline.seq[..shape.fanout], shell, params, &mut statuses).await?
    else {
        return Ok(ExecutionResult::new(2));
    };

    if prefix_status != 0 {
        statuses.push(prefix_status);
        if shape.collect.is_some() {
            statuses.push(prefix_status);
        }
        *shell.last_pipeline_statuses_mut() = statuses;
        shell.set_last_exit_status(prefix_status);
        return Ok(ExecutionResult::new(prefix_status));
    }

    let input = Arc::new(input);
    let remaining_output = Arc::new(AtomicU64::new(COMPOSITION_OUTPUT_LIMIT));
    let output_exceeded = Arc::new(AtomicBool::new(false));
    let output_limit_reached = Arc::new(tokio::sync::Notify::new());
    let branch_shell = shell.clone();
    let branch_params = params.clone();
    // Jobs started by a branch carry `FANOUT_BRANCH=<id>/<label>` so
    // `marsh jobs --tree` can group them under one fanout node.
    let fanout_id = format!("{:016x}", rand::random::<u64>());
    let futures = fanout.branches.iter().map(|branch| {
        let mut shell = branch_shell.clone();
        let lineage = format!("{fanout_id}/{}", branch.label);
        let params = branch_params.clone();
        let input = Arc::clone(&input);
        let remaining_output = Arc::clone(&remaining_output);
        let output_exceeded = Arc::clone(&output_exceeded);
        let output_limit_reached = Arc::clone(&output_limit_reached);
        async move {
            shell.env_mut().update_or_add(
                "FANOUT_BRANCH",
                ShellValueLiteral::Scalar(lineage),
                |variable| {
                    variable.export();
                    Ok(())
                },
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            )?;
            execute_fanout_branch(
                branch,
                shell,
                params,
                input,
                remaining_output,
                output_exceeded,
                output_limit_reached,
            )
            .await
        }
    });
    enum BranchCompletion {
        Complete(Vec<BranchResult>),
        OutputLimit,
        Interrupted,
        Terminated,
    }
    #[cfg(unix)]
    let term_trapped = shell.traps().handles(crate::traps::TrapSignal::Signal(
        sys::signal::Signal::SIGTERM,
    ));
    #[cfg(not(unix))]
    let term_trapped = false;
    let completion = {
        let all_branches = futures::future::join_all(futures);
        tokio::pin!(all_branches);
        tokio::select! {
            results = &mut all_branches => BranchCompletion::Complete(
                results.into_iter().collect::<Result<Vec<_>, error::Error>>()?
            ),
            () = output_limit_reached.notified() => BranchCompletion::OutputLimit,
            result = sys::signal::await_ctrl_c() => {
                result?;
                BranchCompletion::Interrupted
            },
            result = sys::signal::await_term(), if term_trapped => {
                result?;
                BranchCompletion::Terminated
            },
        }
    };
    let branch_results = match completion {
        BranchCompletion::Complete(results) => results,
        BranchCompletion::OutputLimit => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "fanout combined output exceeds 16 MiB limit",
            )
            .into());
        }
        BranchCompletion::Interrupted => {
            let interrupted = 130;
            statuses.push(interrupted);
            if shape.collect.is_some() {
                statuses.push(interrupted);
            }
            *shell.last_pipeline_statuses_mut() = statuses;
            shell.set_last_exit_status(interrupted);
            return Ok(if shell.options().interactive {
                ExecutionResult::interrupted()
            } else {
                ExecutionResult::new(interrupted)
            });
        }
        BranchCompletion::Terminated => {
            let terminated = 143;
            statuses.push(terminated);
            if shape.collect.is_some() {
                statuses.push(terminated);
            }
            *shell.last_pipeline_statuses_mut() = statuses;
            shell.set_last_exit_status(terminated);
            #[cfg(unix)]
            {
                let signal = crate::traps::TrapSignal::Signal(sys::signal::Signal::SIGTERM);
                let trap_result = shell.invoke_trap_handler(signal, params).await?;
                return Ok(if trap_result.is_normal_flow() {
                    ExecutionResult::new(terminated)
                } else {
                    trap_result
                });
            }
            #[cfg(not(unix))]
            return Ok(ExecutionResult::new(terminated));
        }
    };
    if output_exceeded.load(Ordering::Relaxed) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            "fanout combined output exceeds 16 MiB limit",
        )
        .into());
    }
    let branch_failure = branch_results
        .iter()
        .find(|result| result.status != 0)
        .map_or(0, |result| result.status);
    let mut output: Box<dyn Write + Send> = Box::new(collect_params.stdout(shell));
    render_collected(&mut output, &branch_results, started.elapsed(), options)?;
    drop(output);

    statuses.push(branch_failure);
    if shape.collect.is_some() {
        statuses.push(branch_failure);
    }
    let mut result = if let Some(rendered) = &rendered {
        let mut suffix_params = params.clone();
        suffix_params.set_fd(OpenFiles::STDIN_FD, rendered.read()?.into());
        let mut suffix =
            execute_composition_segment(&pipeline.seq[typed_end..], shell, &suffix_params).await?;
        statuses.extend_from_slice(shell.last_pipeline_statuses());
        if shell.options().return_last_failure_from_pipeline
            && branch_failure != 0
            && suffix.is_success()
        {
            suffix.exit_code = ExecutionExitCode::from(branch_failure);
        }
        suffix
    } else {
        ExecutionResult::new(if branch_failure != 0 {
            branch_failure
        } else {
            prefix_status
        })
    };
    if pipeline.bang && !result.is_return_or_exit() {
        result.exit_code = ExecutionExitCode::from(if result.is_success() { 1 } else { 0 });
    }
    *shell.last_pipeline_statuses_mut() = statuses;
    shell.set_last_exit_status(result.exit_code.into());
    Ok(result)
}

pub(crate) async fn execute_composition_segment<SE: extensions::ShellExtensions>(
    commands: &[ast::Command],
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
) -> Result<ExecutionResult, error::Error> {
    if commands.is_empty() {
        return Ok(ExecutionResult::success());
    }
    let segment = ast::Pipeline {
        timed: None,
        bang: false,
        seq: commands.to_vec(),
    };
    let spawned = spawn_pipeline_processes(&segment, shell, params, false).await?;
    wait_for_pipeline_processes_and_update_status(&segment, spawned, shell, params).await
}

async fn execute_fanout_branch<SE: extensions::ShellExtensions>(
    branch: &ast::FanoutBranch,
    mut shell: Shell<SE>,
    mut params: ExecutionParameters,
    input: Arc<Vec<u8>>,
    remaining_output: Arc<AtomicU64>,
    output_exceeded: Arc<AtomicBool>,
    output_limit_reached: Arc<tokio::sync::Notify>,
) -> Result<BranchResult, error::Error> {
    let (input_reader, mut input_writer) = std::io::pipe()?;
    let (stdout_reader, stdout_writer) = std::io::pipe()?;
    let (stderr_reader, stderr_writer) = std::io::pipe()?;
    let input_pump = spawn_running(move || match input_writer.write_all(&input) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error),
    });
    let stdout_capture = {
        let remaining = Arc::clone(&remaining_output);
        let exceeded = Arc::clone(&output_exceeded);
        let limit_reached = Arc::clone(&output_limit_reached);
        spawn_running(move || capture_bounded(stdout_reader, &remaining, &exceeded, &limit_reached))
    };
    let stderr_capture = spawn_running(move || {
        capture_bounded(
            stderr_reader,
            &remaining_output,
            &output_exceeded,
            &output_limit_reached,
        )
    });
    params.set_fd(OpenFiles::STDIN_FD, input_reader.into());
    params.set_fd(OpenFiles::STDOUT_FD, stdout_writer.into());
    params.set_fd(OpenFiles::STDERR_FD, stderr_writer.into());
    shell.options_mut().interactive = false;
    shell.options_mut().kill_external_commands_on_drop = true;
    let started = std::time::Instant::now();
    let result = branch.body.execute(&mut shell, &params).await?;
    drop(params);
    drop(shell);
    input_pump
        .join()
        .map_err(|_| std::io::Error::other("composition input thread panicked"))??;
    let stdout = join_capture(stdout_capture)?;
    let stderr = join_capture(stderr_capture)?;
    Ok(BranchResult {
        label: branch.label.clone(),
        status: result.exit_code.into(),
        elapsed: started.elapsed(),
        stdout,
        stderr,
    })
}

/// `collect` options.
#[derive(Clone, Copy, Default)]
pub struct CollectOptions {
    /// `--timing`: append per-branch and total wall time.
    pub timing: bool,
    /// `--json`: emit one JSON document.
    pub json: bool,
    /// `--stderr`: show stderr of successful branches too (failed branches'
    /// stderr is always shown, as `join` does).
    pub stderr: bool,
    /// `--help`.
    pub help: bool,
    /// An unknown option was given.
    pub invalid: bool,
}

async fn collect_options<SE: extensions::ShellExtensions>(
    collect: Option<&ast::CollectCommand>,
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
) -> Result<CollectOptions, error::Error> {
    let mut options = CollectOptions::default();
    for argument in collect.into_iter().flat_map(|command| &command.arguments) {
        match expansion::basic_expand_word(shell, params, argument.to_string())
            .await?
            .as_str()
        {
            "--timing" => options.timing = true,
            "--json" => options.json = true,
            "--stderr" => options.stderr = true,
            "-h" | "--help" => options.help = true,
            unknown => {
                writeln!(params.stderr(shell), "collect: unknown option: {unknown}")?;
                options.invalid = true;
            }
        }
    }
    Ok(options)
}

const COLLECT_HELP: &str = "collect - render fanout results\n\nUsage: fanout { ... } | collect [--timing] [--json] [--stderr]\n\nPer branch, in order: `== LABEL (state) ==`, its stdout, then (failed\nbranches, or every branch with --stderr) its stderr under `== LABEL stderr ==`.\n\n  --timing  append per-branch and total wall time\n  --json    emit one JSON document\n  --stderr  also show stderr of successful branches\n\nLimits:\n  input     64 MiB completed stdin\n  output    16 MiB combined branch stdout and stderr\n";

/// Retained for embedders: writes each branch's nonempty stderr under a
/// `== LABEL stderr ==` line. `collect` itself renders stderr inline in
/// [`render_collected`].
pub fn render_collected_stderr(
    output: &mut dyn Write,
    branches: &[BranchResult],
) -> std::io::Result<()> {
    for branch in branches {
        write_branch_stderr(output, branch)?;
    }
    Ok(())
}

fn write_branch_stderr(output: &mut dyn Write, branch: &BranchResult) -> std::io::Result<()> {
    if !branch.stderr.is_empty() {
        writeln!(output, "== {} stderr ==", branch.label)?;
        output.write_all(&branch.stderr)?;
        if !branch.stderr.ends_with(b"\n") {
            writeln!(output)?;
        }
    }
    Ok(())
}

/// Renders finished branches as `collect` prints them on stdout.
pub fn render_collected(
    output: &mut dyn Write,
    branches: &[BranchResult],
    total: std::time::Duration,
    options: CollectOptions,
) -> std::io::Result<()> {
    if options.help {
        return output.write_all(COLLECT_HELP.as_bytes());
    }
    for branch in branches {
        if options.json && std::str::from_utf8(&branch.stdout).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "collect --json requires UTF-8 branch stdout",
            ));
        }
    }
    if options.json {
        write!(output, "{{\"branches\":[")?;
        for (index, branch) in branches.iter().enumerate() {
            if index != 0 {
                output.write_all(b",")?;
            }
            write!(
                output,
                "{{\"label\":\"{}\",\"status\":{},\"duration_ms\":{},\"stdout\":\"{}\"}}",
                json_escape(&branch.label),
                branch.status,
                branch.elapsed.as_millis(),
                json_escape(std::str::from_utf8(&branch.stdout).expect("validated UTF-8")),
            )?;
        }
        writeln!(output, "],\"total_ms\":{}}}", total.as_millis())?;
        return Ok(());
    }
    for branch in branches {
        let state = if branch.status == 0 {
            "complete".to_owned()
        } else {
            format!("failed: {}", branch.status)
        };
        writeln!(output, "== {} ({state}) ==", branch.label)?;
        output.write_all(&branch.stdout)?;
        if !branch.stdout.ends_with(b"\n") {
            writeln!(output)?;
        }
        if branch.status != 0 || options.stderr {
            write_branch_stderr(output, branch)?;
        }
        writeln!(output)?;
    }
    if options.timing {
        writeln!(output, "Timing:")?;
        for branch in branches {
            writeln!(
                output,
                "  {:<24} {:>8} ms",
                branch.label,
                branch.elapsed.as_millis()
            )?;
        }
        writeln!(output, "  {:<24} {:>8} ms", "total", total.as_millis())?;
    }
    Ok(())
}

pub(crate) fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", u32::from(character));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

async fn spawn_pipeline_processes<SE: extensions::ShellExtensions>(
    pipeline: &ast::Pipeline,
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
    background: bool,
) -> Result<VecDeque<ExecutionSpawnResult>, error::Error> {
    let pipeline_len = pipeline.seq.len();
    let mut pipe_readers = vec![];
    let mut pipe_writers = vec![];
    let mut spawn_results = VecDeque::new();
    let mut process_group_id: Option<i32> = None;

    // Create pipes to use between commands, but only bother doing so if there's more than one
    // command.
    if pipeline_len > 1 {
        pipe_readers.reserve_exact(pipeline_len - 1);
        pipe_writers.reserve_exact(pipeline_len - 1);

        for _ in 0..(pipeline_len - 1) {
            let (reader, writer) = std::io::pipe()?;
            pipe_readers.push(Some(reader.into()));
            pipe_writers.push(Some(writer.into()));
        }
        // Push `None` to the readers; it will be popped off by the *first* command, which will
        // mean that command gets its stdin from the execution parameters' current stdin.
        pipe_readers.push(None);
    }

    for (current_pipeline_index, command) in pipeline.seq.iter().enumerate() {
        //
        // We run a command directly in the current shell if either of the following is true:
        //     * There's only one command in the pipeline.
        //     * This is the *last* command in the pipeline, the lastpipe option is enabled, and job
        //       monitoring is disabled.
        // Otherwise, we spawn a separate subshell for each command in the pipeline.
        //

        let run_in_current_shell = pipeline_len == 1
            || (current_pipeline_index == pipeline_len - 1
                && shell.options().run_last_pipeline_cmd_in_current_shell
                && !shell.options().enable_job_control);

        // Set up parameters appropriate for this command.
        let mut cmd_params = params.clone();

        // Install pipes.
        if let Some(Some(reader)) = pipe_readers.pop() {
            cmd_params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
        }
        if let Some(Some(writer)) = pipe_writers.pop() {
            cmd_params.open_files.set_fd(OpenFiles::STDOUT_FD, writer);
        }

        // Like Bash, run each stage of a multi-command (or background) pipeline in its own
        // process. The stage keeps its parent's `$BASH_SUBSHELL`.
        #[cfg(unix)]
        if (!run_in_current_shell || background)
            && !matches!(
                command,
                ast::Command::Fanout(_) | ast::Command::Split(_) | ast::Command::Collect(_)
            )
        {
            if !run_in_current_shell && current_pipeline_index > 0 {
                cmd_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
            }
            let mut stage_shell = shell.clone();
            stage_shell.set_depth(shell.depth());
            stage_shell.set_exec_in_place(matches!(command, ast::Command::Simple(_)));
            let stdin_is_terminal = !background
                && cmd_params
                    .try_fd(shell, OpenFiles::STDIN_FD)
                    .is_some_and(|file| file.is_terminal());
            let group = crate::subshell::ChildGroup::for_policy(
                &cmd_params.process_group_policy,
                process_group_id,
                stdin_is_terminal,
            );
            let mut child_params = cmd_params.clone();
            child_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
            let stage = command.clone();
            let ignore_interrupts = background && !shell.options().enable_job_control;
            if let Some(child) = fork_child(
                stage_shell,
                child_params,
                group,
                ignore_interrupts,
                Box::new(move |shell, params| {
                    Box::pin(async move { run_forked_stage(&stage, shell, params).await })
                }),
            )? {
                if process_group_id.is_none() {
                    process_group_id = child.pgid();
                }
                spawn_results.push_back(ExecutionSpawnResult::StartedProcess(child));
                continue;
            }
        }

        let pipeline_context = if !run_in_current_shell {
            // Make sure that all commands in the pipeline are in the same process group.
            if current_pipeline_index > 0 {
                cmd_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
            }

            PipelineExecutionContext {
                shell: commands::ShellForCommand::OwnedShell {
                    target: Box::new(shell.clone()),
                    parent: shell,
                },
                process_group_id,
            }
        } else {
            PipelineExecutionContext {
                shell: commands::ShellForCommand::ParentShell(shell),
                process_group_id,
            }
        };

        let spawn_result = command
            .execute_in_pipeline(pipeline_context, cmd_params)
            .await?;

        // Update the process group ID if something was spawned.
        if let ExecutionSpawnResult::StartedProcess(child) = &spawn_result {
            if process_group_id.is_none() {
                process_group_id = child.pgid();
            }
        }

        spawn_results.push_back(spawn_result);
    }

    Ok(spawn_results)
}

async fn wait_for_pipeline_processes_and_update_status(
    pipeline: &ast::Pipeline,
    mut process_spawn_results: VecDeque<ExecutionSpawnResult>,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<ExecutionResult, error::Error> {
    let mut result = ExecutionResult::success();
    let mut stopped_children = vec![];
    let mut last_failure_exit_code: Option<ExecutionExitCode> = None;
    let mut interrupted = false;
    let mut terminated = false;
    let mut killed_by_int = false;

    // Clear our the pipeline status so we can start filling it out.
    shell.last_pipeline_statuses_mut().clear();

    while let Some(child) = process_spawn_results.pop_front() {
        let wait_result = if !stopped_children.is_empty() {
            child.poll().await?
        } else {
            child.wait().await?
        };

        match wait_result {
            ExecutionWaitResult::Completed {
                result: current_result,
                interrupted: child_interrupted,
                terminated: child_terminated,
                killed_by_int: child_killed_by_int,
            } => {
                interrupted |= child_interrupted;
                terminated |= child_terminated;
                killed_by_int |= child_killed_by_int;
                result = current_result;
                shell.set_last_exit_status(result.exit_code.into());
                shell
                    .last_pipeline_statuses_mut()
                    .push(result.exit_code.into());

                // Track the last failure for pipefail option
                if !result.is_success() {
                    last_failure_exit_code = Some(result.exit_code);
                }
            }
            ExecutionWaitResult::Stopped(child) => {
                result = ExecutionResult::stopped();
                shell.set_last_exit_status(result.exit_code.into());
                shell
                    .last_pipeline_statuses_mut()
                    .push(result.exit_code.into());

                stopped_children.push(jobs::JobTask::External(child));
            }
        }
    }

    // Like Bash, an untrapped INT during a wait matters only if the job died of it.
    #[cfg(unix)]
    if interrupted
        && !shell.traps().handles(crate::traps::TrapSignal::Signal(
            sys::signal::Signal::SIGINT,
        ))
        && u8::from(result.exit_code) != 130
    {
        crate::signals::discard(libc::SIGINT);
    }

    #[cfg(unix)]
    if interrupted {
        let signal = crate::traps::TrapSignal::Signal(sys::signal::Signal::SIGINT);
        if shell.traps().handles(signal) {
            let trap_result = shell.invoke_trap_handler(signal, params).await?;
            if !trap_result.is_normal_flow() {
                return Ok(trap_result);
            }
        }
    }

    #[cfg(unix)]
    if terminated {
        let signal = crate::traps::TrapSignal::Signal(sys::signal::Signal::SIGTERM);
        if shell.traps().handles(signal) {
            let trap_result = shell.invoke_trap_handler(signal, params).await?;
            if !trap_result.is_normal_flow() {
                return Ok(trap_result);
            }
        }
    }

    // Apply pipefail semantics if enabled
    if shell.options().return_last_failure_from_pipeline {
        if let Some(failure_exit_code) = last_failure_exit_code {
            result.exit_code = failure_exit_code;
        }
    }

    // Like Bash, an interactive shell whose foreground job (in its own process
    // group, so the shell itself saw no SIGINT) died of an untrapped SIGINT
    // abandons the rest of the command line: later list items, loop
    // iterations, and callers do not run.
    // Bash also ends the echoed `^C` line so the prompt starts a fresh line.
    #[cfg(unix)]
    if killed_by_int && shell.options().interactive && shell.options().enable_job_control {
        let _ = writeln!(params.stderr(shell));
        if !shell.traps().handles(crate::traps::TrapSignal::Signal(
            sys::signal::Signal::SIGINT,
        )) {
            result.next_control_flow = crate::results::ExecutionControlFlow::Interrupted;
        }
    }

    if shell.options().interactive {
        sys::terminal::move_self_to_foreground()?;
    }

    // If there were stopped jobs, then encapsulate the pipeline as a managed job and hand it
    // off to the job manager.
    if !stopped_children.is_empty() {
        let pipefail = shell.options().return_last_failure_from_pipeline;
        let job = shell.jobs_mut().add_as_current(jobs::Job::new(
            stopped_children,
            pipeline.to_string(),
            jobs::JobState::Stopped,
            pipefail,
        ));

        let formatted = job.to_string();

        // N.B. We use the '\r' to overwrite any ^Z output.
        writeln!(params.stderr(shell), "\r{formatted}")?;
    }

    Ok(result)
}

#[async_trait::async_trait]
impl<SE: extensions::ShellExtensions> ExecuteInPipeline<SE> for ast::Command {
    async fn execute_in_pipeline(
        &self,
        mut pipeline_context: PipelineExecutionContext<'_, SE>,
        mut params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        if pipeline_context.shell.options().do_not_execute_commands {
            return Ok(ExecutionSpawnResult::Completed(ExecutionResult::success()));
        }

        // Updates the shell with information about the currently executing command.
        pipeline_context.shell.set_current_cmd(self);

        match self {
            Self::Fanout(_) => {
                writeln!(
                    params.stderr(&pipeline_context.shell),
                    "fanout: use fanout as a pipeline stage"
                )?;
                Ok(ExecutionResult::new(2).into())
            }
            Self::Split(_) => {
                writeln!(
                    params.stderr(&pipeline_context.shell),
                    "split: use split as a pipeline stage"
                )?;
                Ok(ExecutionResult::new(2).into())
            }
            Self::Collect(command) => {
                collect_as_simple(command)
                    .execute_in_pipeline(pipeline_context, params)
                    .await
            }
            Self::Simple(simple) => simple.execute_in_pipeline(pipeline_context, params).await,
            Self::Compound(compound, redirects) => {
                // Set up any additional redirects.
                if let Some(redirects) = redirects {
                    for redirect in &redirects.0 {
                        setup_redirect(&mut pipeline_context.shell, &mut params, redirect).await?;
                    }
                }

                // A `( ... )` subshell is a process the pipeline waits for like any other.
                #[cfg(unix)]
                if let ast::CompoundCommand::Subshell(subshell) = compound
                    && let Some(child) = fork_subshell_list(
                        &pipeline_context.shell,
                        &params,
                        &subshell.list,
                        pipeline_context.process_group_id,
                    )?
                {
                    return Ok(ExecutionSpawnResult::StartedProcess(child));
                }

                Ok(compound
                    .execute(&mut pipeline_context.shell, &params)
                    .await?
                    .into())
            }
            Self::Function(func) => Ok(func
                .execute(&mut pipeline_context.shell, &params)
                .await?
                .into()),
        }
    }
}

enum WhileOrUntil {
    While,
    Until,
}

#[async_trait::async_trait]
impl Execute for ast::CompoundCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        match self {
            Self::BraceGroup(ast::BraceGroupCommand { list, .. }) => {
                list.execute(shell, params).await
            }
            Self::Subshell(ast::SubshellCommand { list, .. }) => {
                // Like Bash, run the subshell in its own process.
                #[cfg(unix)]
                if let Some(mut child) = fork_subshell_list(shell, params, list, None)? {
                    let result = match child.wait().await? {
                        processes::ProcessWaitResult::Completed { output, .. } => {
                            ExecutionResult::from(output)
                        }
                        processes::ProcessWaitResult::Stopped => ExecutionResult::stopped(),
                    };
                    if shell.options().interactive {
                        sys::terminal::move_self_to_foreground()?;
                    }
                    return Ok(ExecutionResult::from(result.exit_code));
                }

                // Clone off a new subshell, and run the body of the subshell there.
                // TODO(source-info): Do we need to reset the line number?
                let mut subshell = shell.clone();
                subshell.jobs_mut().clear_inherited();

                // Handle errors within the subshell context to prevent fatal errors
                // from propagating to the parent shell.
                let subshell_result = match list.execute(&mut subshell, params).await {
                    Ok(result) => result,
                    Err(error) => {
                        // Display the error to stderr, but prevent fatal error propagation
                        let mut stderr = params.stderr(shell);
                        let _ = shell.display_error(&mut stderr, &error);

                        // Convert error to result in subshell context
                        error.into_result(&subshell)
                    }
                };

                // Preserve the subshell's exit code, but don't honor any of its requests to exit
                // the shell, break out of loops, etc.
                Ok(ExecutionResult::from(subshell_result.exit_code))
            }
            Self::ForClause(f) => f.execute(shell, params).await,
            Self::CaseClause(c) => c.execute(shell, params).await,
            Self::IfClause(i) => i.execute(shell, params).await,
            Self::WhileClause(w) => (WhileOrUntil::While, w).execute(shell, params).await,
            Self::UntilClause(u) => (WhileOrUntil::Until, u).execute(shell, params).await,
            Self::Arithmetic(a) => a.execute(shell, params).await,
            Self::ArithmeticForClause(a) => a.execute(shell, params).await,
            Self::Coprocess(c) => c.execute(shell, params).await,
            Self::ExtendedTest(e) => {
                let result =
                    if extendedtests::eval_extended_test_expr(&e.expr, shell, params).await? {
                        0
                    } else {
                        1
                    };
                Ok(ExecutionResult::new(result))
            }
        }
    }
}

#[async_trait::async_trait]
impl Execute for ast::CoprocessCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        if shell.options().do_not_execute_commands {
            return Ok(ExecutionResult::success());
        }

        // Resolve the name of the variable that will receive the coprocess's file descriptors.
        let name = self
            .name
            .as_ref()
            .map_or(Cow::Borrowed("COPROC"), |w| Cow::Owned(w.to_string()));

        if !valid_variable_name(&name) {
            writeln!(
                params.stderr(shell),
                "coproc {name}: not a valid identifier"
            )?;
            return Ok(ExecutionExitCode::GeneralError.into());
        }

        // Set up the pipes that we'll use to communicate with the coprocess.
        let (stdin_reader, stdin_writer) = std::io::pipe()?;
        let (stdout_reader, stdout_writer) = std::io::pipe()?;
        let (stdin_reader, stdout_writer): (OpenFile, OpenFile) =
            (stdin_reader.into(), stdout_writer.into());

        // Like Bash, run the coprocess in its own process. Its shell is cloned
        // before the parent's ends are installed, so the child closes them.
        #[cfg(unix)]
        if crate::subshell::available() {
            let mut child_shell = shell.clone();
            child_shell.options_mut().interactive = false;
            let mut child_params = params.clone();
            child_params
                .open_files
                .set_fd(OpenFiles::STDIN_FD, stdin_reader.clone());
            child_params
                .open_files
                .set_fd(OpenFiles::STDOUT_FD, stdout_writer.clone());
            child_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
            let job_control = shell.options().enable_job_control;
            let group = if job_control {
                crate::subshell::ChildGroup::New { foreground: false }
            } else {
                crate::subshell::ChildGroup::Inherit
            };
            let body = self.body.clone();
            let child = fork_child(
                child_shell,
                child_params,
                group,
                !job_control,
                Box::new(move |shell, params| {
                    Box::pin(async move { run_forked_stage(&body, shell, params).await })
                }),
            )?;
            if let Some(child) = child {
                let pid = child.pid().unwrap_or_default();
                let stdout_fd = shell.open_files_mut().add(stdout_reader.into())?;
                let stdin_fd = shell.open_files_mut().add(stdin_writer.into())?;
                shell.jobs_mut().add_as_current(jobs::Job::new(
                    [jobs::JobTask::External(child)],
                    format!("coproc {name}"),
                    jobs::JobState::Running,
                    false,
                ));
                let arr_value = ShellValue::from(vec![stdout_fd.to_string(), stdin_fd.to_string()]);
                shell
                    .env_mut()
                    .set_global(name.clone(), ShellVariable::new(arr_value))?;
                shell
                    .env_mut()
                    .set_global(format!("{name}_PID"), ShellVariable::new(pid.to_string()))?;
                return Ok(ExecutionResult::success());
            }
        }

        // Allocate new fds in the (parent) shell for the read end of the coprocess's stdout
        // and the write end of the coprocess's stdin.
        let stdout_fd = shell.open_files_mut().add(stdout_reader.into())?;
        let stdin_fd = shell.open_files_mut().add(stdin_writer.into())?;

        // Crete a subshell that the coprocess will own and run in.
        let mut child_shell = shell.clone();
        child_shell.options_mut().interactive = false;

        // Setup redirection for the coprocess's shell's stdin/stdout.
        let mut child_params = params.clone();
        child_params
            .open_files
            .set_fd(OpenFiles::STDIN_FD, stdin_reader);
        child_params
            .open_files
            .set_fd(OpenFiles::STDOUT_FD, stdout_writer);

        let body = self.body.clone();
        let join_handle = tokio::spawn(async move {
            let pipeline_context = PipelineExecutionContext {
                shell: commands::ShellForCommand::ParentShell(&mut child_shell),
                process_group_id: None,
            };
            let spawn_result = body
                .execute_in_pipeline(pipeline_context, child_params)
                .await?;
            match spawn_result.wait().await? {
                ExecutionWaitResult::Completed { result, .. } => Ok(result),
                ExecutionWaitResult::Stopped(_) => Ok(ExecutionResult::stopped()),
            }
        });

        let job = shell.jobs_mut().add_as_current(jobs::Job::new(
            [jobs::JobTask::Internal(join_handle)],
            format!("coproc {name}"),
            jobs::JobState::Running,
            false, // The internal task already computes its pipeline status.
        ));
        let job_id = job.id;

        // Fill out the fd variable.
        let arr_value = ShellValue::from(vec![stdout_fd.to_string(), stdin_fd.to_string()]);
        shell
            .env_mut()
            .set_global(name.clone(), ShellVariable::new(arr_value))?;

        // Set the job ID for the coprocess in a separate variable with the _PID suffix.
        let pid_name = format!("{name}_PID");
        shell
            .env_mut()
            .set_global(pid_name, ShellVariable::new(job_id.to_string()))?;

        Ok(ExecutionResult::success())
    }
}

#[async_trait::async_trait]
impl Execute for ast::ForClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();

        // If we were given explicit words to iterate over, then expand them all, with splitting
        // enabled.
        let expanded_values = if let Some(unexpanded_values) = &self.values {
            expand_words(shell, params, unexpanded_values).await?
        } else {
            // Otherwise, we use the current positional parameters.
            shell.current_shell_args().to_vec()
        };

        for value in expanded_values {
            if shell.options().print_commands_and_arguments {
                if let Some(unexpanded_values) = &self.values {
                    shell
                        .trace_command(
                            params,
                            std::format!(
                                "for {} in {}",
                                self.variable_name,
                                unexpanded_values.iter().join(" ")
                            ),
                        )
                        .await;
                } else {
                    shell
                        .trace_command(params, std::format!("for {}", self.variable_name))
                        .await;
                }
            }

            // Update the variable.
            shell.env_mut().update_or_add(
                &self.variable_name,
                ShellValueLiteral::Scalar(value),
                |_| Ok(()),
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            )?;

            result = self.body.list.execute(shell, params).await?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::CaseClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // N.B. One would think it makes sense to trace the expanded value being switched
        // on, but that's not it.
        if shell.options().print_commands_and_arguments {
            shell
                .trace_command(params, std::format!("case {} in", self.value))
                .await;
        }

        let expanded_value = expansion::basic_expand_word(shell, params, &self.value).await?;
        let mut result: ExecutionResult = ExecutionResult::success();
        let mut force_execute_next_case = false;

        for case in &self.cases {
            if force_execute_next_case {
                force_execute_next_case = false;
            } else {
                let mut matches = false;
                for pattern in &case.patterns {
                    let expanded_pattern = expansion::basic_expand_pattern(shell, params, pattern)
                        .await?
                        .set_extended_globbing(shell.options().extended_globbing)
                        .set_case_insensitive(shell.options().case_insensitive_conditionals);

                    if expanded_pattern.exactly_matches(expanded_value.as_str())? {
                        matches = true;
                        break;
                    }
                }

                if !matches {
                    continue;
                }
            }

            result = if let Some(case_cmd) = &case.cmd {
                case_cmd.execute(shell, params).await?
            } else {
                ExecutionResult::success()
            };

            // Check for early return (return/exit) or loop control flow (break/continue)
            if !result.is_normal_flow() {
                break;
            }

            match case.post_action {
                ast::CaseItemPostAction::ExitCase => break,
                ast::CaseItemPostAction::UnconditionallyExecuteNextCaseItem => {
                    force_execute_next_case = true;
                }
                ast::CaseItemPostAction::ContinueEvaluatingCases => (),
            }
        }

        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::IfClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // Execute condition with errexit suppressed
        let mut condition_params = params.clone();
        condition_params.suppress_errexit = true;
        let condition = self.condition.execute(shell, &condition_params).await?;

        // Check if the condition itself resulted in non-normal control flow.
        if !condition.is_normal_flow() {
            return Ok(condition);
        }

        if condition.is_success() {
            return self.then.execute(shell, params).await;
        }

        if let Some(elses) = &self.elses {
            for else_clause in elses {
                match &else_clause.condition {
                    Some(else_condition) => {
                        let else_condition_result =
                            else_condition.execute(shell, &condition_params).await?;

                        // Check if the elif condition caused non-normal control flow.
                        if !else_condition_result.is_normal_flow() {
                            return Ok(else_condition_result);
                        }

                        if else_condition_result.is_success() {
                            return else_clause.body.execute(shell, params).await;
                        }
                    }
                    None => {
                        return else_clause.body.execute(shell, params).await;
                    }
                }
            }
        }

        // If we got down here, then no branch was taken; we make sure to
        // reset the last exit status to success and then return success.
        let result = ExecutionResult::success();
        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for (WhileOrUntil, &ast::WhileOrUntilClauseCommand) {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let is_while = match self.0 {
            WhileOrUntil::While => true,
            WhileOrUntil::Until => false,
        };
        let test_condition = &self.1.0;
        let body = &self.1.1;

        let mut result = ExecutionResult::success();

        // Execute loop condition with errexit suppressed
        let mut condition_params = params.clone();
        condition_params.suppress_errexit = true;

        loop {
            let condition_result = test_condition.execute(shell, &condition_params).await?;

            // Update status for condition
            shell.set_last_exit_status(condition_result.exit_code.into());

            if !condition_result.is_normal_flow() {
                result = condition_result;

                // If the condition has break/continue, the while/until loop itself
                // consumes one level. We need to decrement the level before returning.
                result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();
                break;
            }

            if condition_result.is_success() != is_while {
                break;
            }

            result = body.list.execute(shell, params).await?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::ArithmeticCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let value = self.expr.eval(shell, params, true).await?;
        let result = if value != 0 {
            ExecutionResult::success()
        } else {
            ExecutionResult::general_error()
        };

        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::ArithmeticForClauseCommand {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let mut result = ExecutionResult::success();
        if let Some(initializer) = &self.initializer {
            initializer.eval(shell, params, true).await?;
        }

        loop {
            if let Some(condition) = &self.condition {
                // An empty condition (e.g., `for (( ; ; ))`) means "always true".
                if !condition.value.is_empty() && condition.eval(shell, params, true).await? == 0 {
                    break;
                }
            }

            result = self.body.list.execute(shell, params).await?;
            if result.is_return_or_exit() {
                break;
            }

            let is_break = result.is_break();

            result.next_control_flow = result.next_control_flow.try_decrement_loop_levels();

            if is_break || result.is_continue() {
                break;
            }

            if let Some(updater) = &self.updater {
                updater.eval(shell, params, true).await?;
            }
        }

        shell.set_last_exit_status(result.exit_code.into());
        Ok(result)
    }
}

#[async_trait::async_trait]
impl Execute for ast::FunctionDefinition {
    async fn execute(
        &self,
        shell: &mut Shell<impl extensions::ShellExtensions>,
        _params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let func_name = self.fname.value.clone();

        // In POSIX mode, function names can't shadow special builtins.
        if shell.options().posix_mode
            && shell
                .builtins()
                .get(&func_name)
                .is_some_and(|r| r.special_builtin)
        {
            return Err(
                error::Error::from(error::ErrorKind::FunctionNameShadowsSpecialBuiltin {
                    name: func_name,
                })
                .into_fatal(),
            );
        }

        // The function definition's source context should be the same as the current frame
        // so we directly pass that through.
        let source_info = shell
            .call_stack()
            .current_frame()
            .map_or_else(crate::SourceInfo::default, |frame| {
                frame.adjusted_source_info()
            });
        shell.define_func(func_name, self.clone(), &source_info);

        let result = ExecutionResult::success();
        shell.set_last_exit_status(result.exit_code.into());

        Ok(result)
    }
}

#[async_trait::async_trait]
#[allow(clippy::too_many_lines)]
impl<SE: extensions::ShellExtensions> ExecuteInPipeline<SE> for ast::SimpleCommand {
    async fn execute_in_pipeline(
        &self,
        mut context: PipelineExecutionContext<'_, SE>,
        mut params: ExecutionParameters,
    ) -> Result<ExecutionSpawnResult, error::Error> {
        // Only this command (not one it runs) may replace a forked child's process.
        let exec_in_place = context.shell.take_exec_in_place();
        let prefix_iter = self.prefix.as_ref().map(|s| s.0.iter()).unwrap_or_default();
        let suffix_iter = self.suffix.as_ref().map(|s| s.0.iter()).unwrap_or_default();
        let cmd_name_items = self
            .word_or_name
            .as_ref()
            .map(|won| CommandPrefixOrSuffixItem::Word(won.clone()));

        let mut assignments = vec![];
        let mut args: Vec<CommandArg> = vec![];
        let mut command_takes_assignments = false;

        // Capture the status change count before expansion, so we can detect
        // if expansion (e.g., command substitution) set an exit status.
        let status_change_count_before_expansion = context.shell.last_exit_status_change_count();

        for item in prefix_iter.chain(cmd_name_items.iter()).chain(suffix_iter) {
            match item {
                CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                    if let Err(e) = setup_redirect(&mut context.shell, &mut params, redirect).await
                    {
                        writeln!(params.stderr(&context.shell), "error: {e}")?;
                        return Ok(ExecutionResult::general_error().into());
                    }
                }
                CommandPrefixOrSuffixItem::ProcessSubstitution(kind, subshell_command) => {
                    let (installed_fd_num, substitution_file) = setup_process_substitution(
                        &context.shell,
                        &params,
                        kind,
                        subshell_command,
                    )?;

                    params
                        .open_files
                        .set_fd(installed_fd_num, substitution_file);

                    args.push(CommandArg::String(std::format!(
                        "/dev/fd/{installed_fd_num}"
                    )));
                }
                CommandPrefixOrSuffixItem::AssignmentWord(assignment, word) => {
                    if args.is_empty() {
                        // If we haven't yet seen any arguments, then this must be a proper
                        // scoped assignment. Add it to the list we're accumulating.
                        assignments.push(assignment);
                    } else {
                        if command_takes_assignments {
                            // This looks like an assignment, and the command being invoked is a
                            // well-known builtin that takes arguments that need to function like
                            // assignments (but which are processed by the builtin).
                            let expanded =
                                expand_assignment(&mut context.shell, &params, assignment).await?;
                            args.push(CommandArg::Assignment(expanded));
                        } else {
                            // This *looks* like an assignment, but it's really a string we should
                            // fully treat as a regular looking
                            // argument.
                            let mut next_args = expansion::full_expand_and_split_word(
                                &mut context.shell,
                                &params,
                                word,
                            )
                            .await?
                            .into_iter()
                            .map(CommandArg::String)
                            .collect();
                            args.append(&mut next_args);
                        }
                    }
                }
                CommandPrefixOrSuffixItem::Word(arg) => {
                    let next_args =
                        expansion::full_expand_and_split_word(&mut context.shell, &params, arg)
                            .await?;

                    // Check if we're going to be invoking a special declaration builtin.
                    // That will change how we parse and process args. (Aliases were
                    // already expanded when the command was read.)
                    if args.is_empty()
                        && let Some(first_arg) = next_args.first()
                        && context
                            .shell
                            .builtins()
                            .get(first_arg.as_str())
                            .is_some_and(|r| !r.disabled && r.declaration_builtin)
                    {
                        command_takes_assignments = true;
                    }

                    let mut next_args = next_args.into_iter().map(CommandArg::String).collect();
                    args.append(&mut next_args);
                }
            }
        }

        // If we have a command, then execute it.
        if let Some(CommandArg::String(cmd_name)) = args.first() {
            let mut stderr = params.stderr(&context.shell);

            let (owned_shell, parent_shell) = match context.shell {
                commands::ShellForCommand::ParentShell(shell) => (None, shell),
                commands::ShellForCommand::OwnedShell { target, parent } => (Some(target), parent),
            };

            let shell = if let Some(owned_shell) = owned_shell {
                commands::ShellForCommand::OwnedShell {
                    target: owned_shell,
                    parent: parent_shell,
                }
            } else {
                commands::ShellForCommand::ParentShell(parent_shell)
            };

            let context = PipelineExecutionContext {
                shell,
                process_group_id: context.process_group_id,
            };

            match execute_command(
                context,
                params,
                cmd_name,
                &assignments,
                &args,
                exec_in_place,
            )
            .await
            {
                Ok(result) => Ok(result),
                Err(err) => {
                    let _ = parent_shell.display_error(&mut stderr, &err);

                    let result = err.into_result(parent_shell);
                    Ok(result.into())
                }
            }
        } else {
            // No command to run; assignments must be applied to this shell.
            for assignment in assignments {
                // Apply the assignment. Don't mark as fatal - let errors be handled
                // at the program level so multiple complete_commands can execute independently.
                apply_assignment(
                    assignment,
                    &mut context.shell,
                    &params,
                    false,
                    None,
                    EnvironmentScope::Global,
                )
                .await?;
            }

            // Assignment-only statements clear $_ (set to empty string).
            // This matches bash behavior where assignments don't have a "last
            // argument".
            context.shell.update_last_arg_variable(None);

            // We need to set the last exit status to indicate assignment success,
            // but only if there was no status set during expansion. We use the
            // status count captured before expansion to detect if command
            // substitution (or other expansion) set an exit status.
            if status_change_count_before_expansion == context.shell.last_exit_status_change_count()
            {
                context.shell.set_last_exit_status(0);
            }

            // Return the last exit status we have; in some cases, an expansion
            // might result in a non-zero exit status stored in the shell.
            Ok(ExecutionResult::new(context.shell.last_exit_status()).into())
        }
    }
}

async fn execute_command<T: Into<String>>(
    mut context: PipelineExecutionContext<'_, impl extensions::ShellExtensions>,
    params: ExecutionParameters,
    cmd_name: T,
    assignments: &[&ast::Assignment],
    args: &[CommandArg],
    exec_in_place: bool,
) -> Result<ExecutionSpawnResult, error::Error> {
    // Push a new ephemeral environment scope for the duration of the command. We'll
    // set command-scoped variable assignments after doing so, and revert them before
    // returning.
    let mut guard = crate::env::ScopeGuard::new(&mut context.shell, EnvironmentScope::Command);

    for assignment in assignments {
        // Ensure it's tagged as exported and created in the command scope.
        apply_assignment(
            assignment,
            guard.shell(),
            &params,
            true,
            Some(EnvironmentScope::Command),
            EnvironmentScope::Command,
        )
        .await?;
    }

    if guard.shell().options().print_commands_and_arguments {
        guard
            .shell()
            .trace_command(
                &params,
                args.iter().map(|arg| arg.quote_for_tracing()).join(" "),
            )
            .await;
    }

    guard.detach();
    drop(guard);

    // Construct the command struct.
    let mut cmd =
        commands::SimpleCommand::new(context.shell, params, cmd_name.into(), args.iter().cloned());
    cmd.process_group_id = context.process_group_id;
    cmd.exec_in_place = exec_in_place;

    // Arrange to pop off that ephemeral environment scope.
    cmd.post_execute = Some(|shell| shell.env_mut().pop_scope(EnvironmentScope::Command));

    // Run through any pre-execution hooks as best effort.
    let _ = commands::on_preexecute(&mut cmd).await;

    // Execute
    // TODO(jobs): do we need to move self back to foreground on error here?
    cmd.execute().await
}

/// Expands the given words, with splitting enabled, yielding the fields they expand to.
async fn expand_words(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    words: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<Vec<String>, error::Error> {
    // N.B. Expansion needs `&mut shell`, so the words have to be expanded in sequence.
    let mut fields = vec![];
    for word in words {
        fields.extend(expansion::full_expand_and_split_word(shell, params, word).await?);
    }
    Ok(fields)
}

async fn expand_assignment(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    assignment: &ast::Assignment,
) -> Result<ast::Assignment, error::Error> {
    let value = expand_assignment_value(shell, params, &assignment.value).await?;
    Ok(ast::Assignment {
        name: basic_expand_assignment_name(shell, params, &assignment.name).await?,
        value,
        append: assignment.append,
        loc: assignment.loc.clone(),
    })
}

async fn basic_expand_assignment_name(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    name: &ast::AssignmentName,
) -> Result<ast::AssignmentName, error::Error> {
    match name {
        ast::AssignmentName::VariableName(name) => {
            let expanded = expansion::basic_expand_word(shell, params, name).await?;
            Ok(ast::AssignmentName::VariableName(expanded))
        }
        ast::AssignmentName::ArrayElementName(name, index) => {
            let expanded_name = expansion::basic_expand_word(shell, params, name).await?;
            let expanded_index = expansion::basic_expand_word(shell, params, index).await?;
            Ok(ast::AssignmentName::ArrayElementName(
                expanded_name,
                expanded_index,
            ))
        }
    }
}

async fn expand_assignment_value(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    value: &ast::AssignmentValue,
) -> Result<ast::AssignmentValue, error::Error> {
    let expanded = match value {
        ast::AssignmentValue::Scalar(s) => {
            let expanded_word = expansion::basic_expand_assignment_word(shell, params, s).await?;
            ast::AssignmentValue::Scalar(ast::Word::from(expanded_word))
        }
        ast::AssignmentValue::Array(arr) => {
            let mut expanded_values = vec![];
            for (key, value) in arr {
                if let Some(k) = key {
                    let expanded_key = expansion::basic_expand_assignment_word(shell, params, k)
                        .await?
                        .into();
                    let expanded_value =
                        expansion::basic_expand_assignment_word(shell, params, value)
                            .await?
                            .into();
                    expanded_values.push((Some(expanded_key), expanded_value));
                } else {
                    // Array elements are treated as regular words, not assignments
                    let split_expanded_value =
                        expansion::full_expand_and_split_word(shell, params, value).await?;
                    for expanded_value in split_expanded_value {
                        expanded_values.push((None, expanded_value.into()));
                    }
                }
            }

            ast::AssignmentValue::Array(expanded_values)
        }
    };

    Ok(expanded)
}

#[expect(clippy::too_many_lines)]
async fn apply_assignment(
    assignment: &ast::Assignment,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    mut export: bool,
    required_scope: Option<EnvironmentScope>,
    creation_scope: EnvironmentScope,
) -> Result<(), error::Error> {
    // Figure out if we are trying to assign to a variable or assign to an element of an existing
    // array.
    let mut array_index;
    let variable_name = match &assignment.name {
        ast::AssignmentName::VariableName(name) => {
            array_index = None;
            name
        }
        ast::AssignmentName::ArrayElementName(name, index) => {
            let expanded = expansion::basic_expand_word(shell, params, index).await?;
            array_index = Some(expanded);
            name
        }
    };

    // Expand the values.
    let new_value = match &assignment.value {
        ast::AssignmentValue::Scalar(unexpanded_value) => {
            let value =
                expansion::basic_expand_assignment_word(shell, params, unexpanded_value).await?;
            ShellValueLiteral::Scalar(value)
        }
        ast::AssignmentValue::Array(unexpanded_values) => {
            let mut elements = vec![];
            for (unexpanded_key, unexpanded_value) in unexpanded_values {
                let key = match unexpanded_key {
                    Some(unexpanded_key) => Some(
                        expansion::basic_expand_assignment_word(shell, params, unexpanded_key)
                            .await?,
                    ),
                    None => None,
                };

                if key.is_some() {
                    let value =
                        expansion::basic_expand_assignment_word(shell, params, unexpanded_value)
                            .await?;
                    elements.push((key, value));
                } else {
                    // Array elements are treated as regular words, not assignments
                    let values =
                        expansion::full_expand_and_split_word(shell, params, unexpanded_value)
                            .await?;
                    for value in values {
                        elements.push((None, value));
                    }
                }
            }
            ShellValueLiteral::Array(ArrayLiteral(elements))
        }
    };

    if shell.options().print_commands_and_arguments {
        let op = if assignment.append { "+=" } else { "=" };
        shell
            .trace_command(params, std::format!("{}{op}{new_value}", assignment.name))
            .await;
    }

    // See if we need to eval an array index.
    if let Some(idx) = &array_index {
        // An array subscript is arithmetically evaluated unless the target is an
        // associative array (in which case the subscript is used as a literal key).
        // A scalar or unset/untyped variable becomes an indexed array, so its
        // subscript still needs to be evaluated.
        let will_be_indexed_array =
            if let Some((_, existing_value)) = shell.env().get(variable_name) {
                !matches!(
                    existing_value.value(),
                    ShellValue::AssociativeArray(_)
                        | ShellValue::Unset(ShellValueUnsetType::AssociativeArray)
                )
            } else {
                true
            };

        if will_be_indexed_array {
            array_index = Some(
                arithmetic::expand_and_eval(shell, params, idx.as_str(), false)
                    .await?
                    .to_string(),
            );
        }
    }

    // Read option before taking mutable borrow on env.
    let export_variables_on_modification = shell.options().export_variables_on_modification;

    // Assign through a name reference (`declare -n`) to the variable it names.
    let variable_name = shell
        .env()
        .resolve_nameref(variable_name.as_str())
        .into_owned();

    // See if we can find an existing value associated with the variable.
    if let Some((existing_value_scope, existing_value)) =
        shell.env_mut().get_mut(variable_name.as_str())
    {
        if required_scope.is_none() || Some(existing_value_scope) == required_scope {
            if let Some(array_index) = array_index {
                match new_value {
                    ShellValueLiteral::Scalar(s) => {
                        existing_value.assign_at_index(array_index, s, assignment.append)?;
                    }
                    ShellValueLiteral::Array(_) => {
                        return error::unimp("replacing an array item with an array");
                    }
                }
            } else {
                if !export
                    && export_variables_on_modification
                    && !matches!(new_value, ShellValueLiteral::Array(_))
                {
                    export = true;
                }

                existing_value.assign(new_value, assignment.append)?;
            }

            if export {
                existing_value.export();
            }

            // That's it!
            return Ok(());
        }
    }

    // If we fell down here, then we need to add it.
    let new_value = if let Some(array_index) = array_index {
        match new_value {
            ShellValueLiteral::Scalar(s) => {
                ShellValue::indexed_array_from_literals(ArrayLiteral(vec![(Some(array_index), s)]))
            }
            ShellValueLiteral::Array(_) => {
                return error::unimp("cannot assign list to array member");
            }
        }
    } else {
        match new_value {
            ShellValueLiteral::Scalar(s) => {
                export = export || shell.options().export_variables_on_modification;
                ShellValue::String(s)
            }
            ShellValueLiteral::Array(values) => ShellValue::indexed_array_from_literals(values),
        }
    };

    let mut new_var = ShellVariable::new(new_value);

    if export {
        new_var.export();
    }

    shell.env_mut().add(variable_name, new_var, creation_scope)
}

#[expect(clippy::too_many_lines)]
pub(crate) async fn setup_redirect(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &'_ mut ExecutionParameters,
    redirect: &ast::IoRedirect,
) -> Result<(), error::Error> {
    match redirect {
        ast::IoRedirect::NamedFd(name, inner) => {
            let ast::IoRedirect::File(_, kind, target) = inner.as_ref() else {
                return Err(error::ErrorKind::InvalidRedirection.into());
            };
            let closing =
                matches!(target, ast::IoFileRedirectTarget::Duplicate(word) if word.value == "-");
            // Like Bash, a close uses the descriptor the variable holds; anything
            // else gets the lowest free descriptor from 10 and stores it there.
            let fd = if closing {
                shell
                    .env()
                    .get_str(name.as_str(), shell)
                    .and_then(|value| value.parse::<ShellFd>().ok())
                    .ok_or(error::ErrorKind::InvalidRedirection)?
            } else {
                let mut fd: ShellFd = 10;
                while params.open_files.contains_fd(fd)
                    || shell.persistent_open_files().contains_fd(fd)
                {
                    fd += 1;
                }
                fd
            };
            let numbered = ast::IoRedirect::File(Some(fd), kind.clone(), target.clone());
            Box::pin(setup_redirect(shell, params, &numbered)).await?;
            if !closing {
                shell.env_mut().update_or_add(
                    name.as_str(),
                    ShellValueLiteral::Scalar(fd.to_string()),
                    |_| Ok(()),
                    EnvironmentLookup::Anywhere,
                    EnvironmentScope::Global,
                )?;
            }
        }

        ast::IoRedirect::OutputAndError(f, append) => {
            let mut expanded_fields =
                expansion::full_expand_and_split_word(shell, params, f).await?;
            if expanded_fields.len() != 1 {
                return Err(error::ErrorKind::InvalidRedirection.into());
            }

            let expanded_file_path = expanded_fields.remove(0);
            setup_redirect_output_and_error_to(shell, params, &expanded_file_path, *append)?;
        }

        ast::IoRedirect::File(specified_fd_num, kind, target) => {
            match target {
                ast::IoFileRedirectTarget::Filename(f) => {
                    let mut options = std::fs::File::options();

                    let mut expanded_fields =
                        expansion::full_expand_and_split_word(shell, params, f).await?;

                    if expanded_fields.len() != 1 {
                        return Err(error::ErrorKind::InvalidRedirection.into());
                    }

                    let expanded_file_path: PathBuf =
                        shell.absolute_path(Path::new(expanded_fields.remove(0).as_str()));

                    let default_fd_if_unspecified = get_default_fd_for_redirect_kind(kind);
                    match kind {
                        ast::IoFileRedirectKind::Read => {
                            options.read(true);
                        }
                        ast::IoFileRedirectKind::Write => {
                            if shell
                                .options()
                                .disallow_overwriting_regular_files_via_output_redirection
                            {
                                // First check to see if the path points to an existing regular
                                // file.
                                if !expanded_file_path.is_file() {
                                    options.create(true);
                                } else {
                                    options.create_new(true);
                                }
                                options.write(true);
                            } else {
                                options.create(true);
                                options.write(true);
                                options.truncate(true);
                            }
                        }
                        ast::IoFileRedirectKind::Append => {
                            options.create(true);
                            options.append(true);
                        }
                        ast::IoFileRedirectKind::ReadAndWrite => {
                            options.create(true);
                            options.read(true);
                            options.write(true);
                        }
                        ast::IoFileRedirectKind::Clobber => {
                            options.create(true);
                            options.write(true);
                            options.truncate(true);
                        }
                        ast::IoFileRedirectKind::DuplicateInput => {
                            options.read(true);
                        }
                        ast::IoFileRedirectKind::DuplicateOutput => {
                            options.create(true);
                            options.write(true);
                        }
                    }

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    let opened_file = shell
                        .open_file(&options, &expanded_file_path, params)
                        .map_err(|err| {
                            error::ErrorKind::RedirectionFailure(
                                expanded_file_path.to_string_lossy().to_string(),
                                err.to_string(),
                            )
                        })?;

                    params.open_files.set_fd(fd_num, opened_file);
                }

                ast::IoFileRedirectTarget::Fd(fd) => {
                    let default_fd_if_unspecified = match kind {
                        ast::IoFileRedirectKind::DuplicateInput => 0,
                        ast::IoFileRedirectKind::DuplicateOutput => 1,
                        _ => {
                            return error::unimp("unexpected redirect kind");
                        }
                    };

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    if let Some(target_file) = params.try_fd(shell, *fd) {
                        params.open_files.set_fd(fd_num, target_file);
                    } else {
                        return Err(error::ErrorKind::BadFileDescriptor(*fd).into());
                    }
                }

                ast::IoFileRedirectTarget::Duplicate(word) => {
                    let default_fd_if_unspecified = match kind {
                        ast::IoFileRedirectKind::DuplicateInput => 0,
                        ast::IoFileRedirectKind::DuplicateOutput => 1,
                        _ => {
                            return error::unimp("unexpected redirect kind");
                        }
                    };

                    let fd_num = specified_fd_num.unwrap_or(default_fd_if_unspecified);

                    let mut expanded_fields =
                        expansion::full_expand_and_split_word(shell, params, word).await?;

                    if expanded_fields.len() != 1 {
                        return Err(error::ErrorKind::InvalidRedirection.into());
                    }

                    let mut expanded = expanded_fields.remove(0);

                    let dash = if expanded.ends_with('-') {
                        expanded.pop();
                        true
                    } else {
                        false
                    };

                    if expanded.is_empty() {
                        // Nothing to do
                    } else if expanded.chars().all(|c: char| c.is_ascii_digit()) {
                        let source_fd_num = expanded
                            .parse::<ShellFd>()
                            .map_err(|_| error::ErrorKind::InvalidRedirection)?;

                        // Reference the same open file as the source fd (shared handle; no OS-level duplication).
                        let Some(target_file) = params.try_fd(shell, source_fd_num) else {
                            return Err(error::ErrorKind::BadFileDescriptor(source_fd_num).into());
                        };

                        params.open_files.set_fd(fd_num, target_file);
                    } else if fd_num == 1 && !dash {
                        // Special case for compatibility: redirect stdout and stderr to the file
                        // given by `expanded`.
                        setup_redirect_output_and_error_to(
                            shell, params, &expanded, false, /* append? */
                        )?;
                    } else {
                        return Err(error::ErrorKind::InvalidRedirection.into());
                    }

                    if dash {
                        // Close the specified fd. Ignore it if it's not valid.
                        params.open_files.remove_fd(fd_num);
                    }
                }

                ast::IoFileRedirectTarget::ProcessSubstitution(substitution_kind, subshell_cmd) => {
                    match kind {
                        ast::IoFileRedirectKind::Read
                        | ast::IoFileRedirectKind::Write
                        | ast::IoFileRedirectKind::Append
                        | ast::IoFileRedirectKind::ReadAndWrite
                        | ast::IoFileRedirectKind::Clobber => {
                            let (substitution_fd, substitution_file) = setup_process_substitution(
                                shell,
                                params,
                                substitution_kind,
                                subshell_cmd,
                            )?;

                            let target_file = substitution_file.clone();
                            params.open_files.set_fd(substitution_fd, substitution_file);

                            let fd_num = specified_fd_num
                                .unwrap_or_else(|| get_default_fd_for_redirect_kind(kind));

                            params.open_files.set_fd(fd_num, target_file);
                        }
                        _ => return error::unimp("invalid process substitution"),
                    }
                }
            }
        }

        ast::IoRedirect::HereDocument(fd_num, io_here) => {
            // If not specified, default to stdin (fd 0).
            let fd_num = fd_num.unwrap_or(0);

            // Expand if required.
            let io_here_doc = if io_here.requires_expansion {
                expansion::basic_expand_heredoc_word(shell, params, &io_here.doc).await?
            } else {
                io_here.doc.flatten()
            };

            let f = setup_open_file_with_contents(io_here_doc.as_str())?;

            params.open_files.set_fd(fd_num, f);
        }

        ast::IoRedirect::HereString(fd_num, word) => {
            // If not specified, default to stdin (fd 0).
            let fd_num = fd_num.unwrap_or(0);

            let mut expanded_word = expansion::basic_expand_word(shell, params, word).await?;
            expanded_word.push('\n');

            let f = setup_open_file_with_contents(expanded_word.as_str())?;

            params.open_files.set_fd(fd_num, f);
        }
    }

    Ok(())
}

/// Sets up redirection of both stdout and stderr to the same file, given by `file_path`.
///
/// # Arguments
///
/// * `shell` - The shell instance.
/// * `params` - The execution parameters to modify.
/// * `file_path` - The path to the file to redirect output and error to.
/// * `append` - Whether to append. If `false`, the file will be truncated.
fn setup_redirect_output_and_error_to(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &mut ExecutionParameters,
    file_path: &str,
    append: bool,
) -> Result<(), error::Error> {
    let abs_file_path: PathBuf = shell.absolute_path(Path::new(file_path));

    let mut file_options = std::fs::File::options();
    file_options
        .create(true)
        .write(true)
        .truncate(!append)
        .append(append);

    let stdout_file = shell
        .open_file(&file_options, &abs_file_path, params)
        .map_err(|err| {
            error::ErrorKind::RedirectionFailure(
                abs_file_path.to_string_lossy().to_string(),
                err.to_string(),
            )
        })?;

    let stderr_file = stdout_file.clone();

    params.open_files.set_fd(OpenFiles::STDOUT_FD, stdout_file);
    params.open_files.set_fd(OpenFiles::STDERR_FD, stderr_file);

    Ok(())
}

const fn get_default_fd_for_redirect_kind(kind: &ast::IoFileRedirectKind) -> ShellFd {
    match kind {
        ast::IoFileRedirectKind::Read => 0,
        ast::IoFileRedirectKind::Write => 1,
        ast::IoFileRedirectKind::Append => 1,
        ast::IoFileRedirectKind::ReadAndWrite => 0,
        ast::IoFileRedirectKind::Clobber => 1,
        ast::IoFileRedirectKind::DuplicateInput => 0,
        ast::IoFileRedirectKind::DuplicateOutput => 1,
    }
}

fn setup_process_substitution(
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    kind: &ast::ProcessSubstitutionKind,
    subshell_cmd: &ast::SubshellCommand,
) -> Result<(ShellFd, OpenFile), error::Error> {
    // TODO(execute): Don't execute synchronously!
    // Execute in a subshell.
    #[allow(unused_mut, reason = "rebound mutably on unix")]
    let mut subshell = shell.clone();

    // Set up execution parameters for the child execution.
    let mut child_params = params.clone();
    child_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;

    // Set up pipe so we can connect to the command.
    let (reader, writer) = std::io::pipe()?;
    let (reader, writer) = (reader.into(), writer.into());

    let target_file = match kind {
        ast::ProcessSubstitutionKind::Read => {
            child_params.open_files.set_fd(OpenFiles::STDOUT_FD, writer);
            reader
        }
        ast::ProcessSubstitutionKind::Write => {
            child_params.open_files.set_fd(OpenFiles::STDIN_FD, reader);
            writer
        }
    };

    // Like Bash, run the substitution in its own process, reaped in the background.
    #[cfg(unix)]
    let (mut subshell, child_params) = {
        let list = subshell_cmd.list.clone();
        let mut forked_params = child_params.clone();
        forked_params.process_group_policy = ProcessGroupPolicy::SameProcessGroup;
        let mut child_shell = subshell.clone();
        child_shell.set_depth(subshell.depth());
        match fork_child(
            child_shell,
            forked_params,
            crate::subshell::ChildGroup::Inherit,
            false,
            Box::new(move |shell, params| {
                Box::pin(async move { run_list_reporting_errors(&list, shell, &params).await })
            }),
        )? {
            Some(child) => {
                if let Some(pid) = child.pid() {
                    tokio::spawn(crate::subshell::wait_for_pid(pid));
                }
                drop(child);
                drop(child_params);
                drop(subshell);
                return Ok((free_substitution_fd(params)?, target_file));
            }
            None => (subshell, child_params),
        }
    };

    // Asynchronously spawn off the subshell; we intentionally don't block on its
    // completion.
    let subshell_cmd = subshell_cmd.to_owned();
    tokio::spawn(async move {
        // Intentionally ignore the result of the subshell command.
        let _ = subshell_cmd
            .list
            .execute(&mut subshell, &child_params)
            .await;
    });

    Ok((free_substitution_fd(params)?, target_file))
}

/// Starting at 63 (a.k.a. 64-1)--and decrementing--looks for an available fd.
fn free_substitution_fd(params: &ExecutionParameters) -> Result<ShellFd, error::Error> {
    let mut candidate_fd_num = 63;
    while params.open_files.contains_fd(candidate_fd_num) {
        candidate_fd_num -= 1;
        if candidate_fd_num == 0 {
            return error::unimp("no available file descriptors");
        }
    }
    Ok(candidate_fd_num)
}

fn setup_open_file_with_contents(contents: &str) -> Result<OpenFile, error::Error> {
    let (reader, mut writer) = std::io::pipe()?;

    let bytes = contents.as_bytes();

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsFd as _;

        let len = i32::try_from(bytes.len())
            .map_err(|_err| error::Error::from(error::ErrorKind::TooMuchData))?;
        nix::fcntl::fcntl(reader.as_fd(), nix::fcntl::FcntlArg::F_SETPIPE_SZ(len))?;
    }

    writer.write_all(bytes)?;
    drop(writer);

    Ok(reader.into())
}
