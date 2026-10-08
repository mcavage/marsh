use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionExitCode, ExecutionResult, builtins, env, traps::TrapSignal, variables};

/// Wait for jobs to terminate.
#[derive(Parser)]
pub(crate) struct WaitCommand {
    /// Wait for specified job to terminate (instead of change status).
    #[arg(short = 'f')]
    wait_for_terminate: bool,

    /// Wait for a single job to change status; if jobs are specified, waits for
    /// the first to change status, and otherwise waits for the next change.
    #[arg(short = 'n')]
    wait_for_first_or_next: bool,

    /// Name of variable to receive the job ID of the job whose status is indicated.
    #[arg(short = 'p', value_name = "VAR_NAME")]
    variable_to_receive_id: Option<String>,

    /// Process IDs or job specs to wait for.
    ids: Vec<String>,
}

impl builtins::Command for WaitCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        mut context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if let Some(name) = &self.variable_to_receive_id {
            if !env::valid_variable_name(name) {
                writeln!(
                    context.stderr(),
                    "{}: `{name}`: not a valid identifier",
                    context.command_name
                )?;
                return Ok(ExecutionResult::general_error());
            }
            context.shell.env_mut().unset(name)?;
        }

        if self.wait_for_first_or_next {
            let eligible_ids = if self.ids.is_empty() {
                context.shell.jobs_mut().waitable_ids()
            } else {
                let mut ids = Vec::new();
                for id in &self.ids {
                    let job = if id.starts_with('%') {
                        context.shell.jobs_mut().resolve_job_spec(id)
                    } else {
                        id.parse()
                            .ok()
                            .and_then(|pid| context.shell.jobs_mut().job_for_pid_mut(pid))
                    };
                    if let Some(job) = job {
                        ids.push(job.id);
                    } else if let Ok(pid) = id.parse() {
                        if let Some(job_id) = context.shell.jobs_mut().waitable_id_for_pid(pid) {
                            ids.push(job_id);
                        }
                    }
                }
                ids
            };
            #[cfg(unix)]
            let waited = {
                let trapped = TrappedSignals::of(context.shell);
                tokio::select! {
                    result = context.shell.jobs_mut().wait_first(&eligible_ids, self.wait_for_terminate) => result?,
                    signal = trapped.recv() => return run_trap(&mut context, signal?).await,
                }
            };
            #[cfg(not(unix))]
            let waited = context
                .shell
                .jobs_mut()
                .wait_first(&eligible_ids, self.wait_for_terminate)
                .await?;
            let Some((job_id, pid, result)) = waited else {
                return Ok(ExecutionResult::new(127));
            };
            if let Some(name) = &self.variable_to_receive_id {
                let value = pid.map_or_else(|| format!("%{job_id}"), |pid| pid.to_string());
                context.shell.env_mut().update_or_add(
                    name,
                    variables::ShellValueLiteral::Scalar(value),
                    |_| Ok(()),
                    env::EnvironmentLookup::Anywhere,
                    env::EnvironmentScope::Global,
                )?;
            }
            return Ok(result);
        }
        #[cfg(unix)]
        {
            let trapped = TrappedSignals::of(context.shell);
            let signal = tokio::select! {
                result = self.wait_without_next(&mut context) => return result,
                signal = trapped.recv() => signal?,
            };
            return run_trap(&mut context, signal).await;
        }

        #[cfg(not(unix))]
        self.wait_without_next(&mut context).await
    }
}

impl WaitCommand {
    async fn wait_without_next<SE: brush_core::ShellExtensions>(
        &self,
        context: &mut brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, brush_core::Error> {
        let mut result = ExecutionResult::success();
        let mut waited_for_pid = false;
        let mut last_waited_id = None;

        if !self.ids.is_empty() {
            for id in &self.ids {
                if id.starts_with('%') {
                    // It's a job spec.
                    if let Some(job) = context.shell.jobs_mut().resolve_job_spec(id) {
                        let pid = job.representative_pid();
                        let job_id = job.id;
                        result = if self.wait_for_terminate {
                            job.wait_for_termination().await?
                        } else {
                            job.wait().await?
                        };
                        last_waited_id =
                            Some(pid.map_or_else(|| format!("%{job_id}"), |pid| pid.to_string()));
                    } else if let Some(job_id) = id.strip_prefix('%').and_then(|id| id.parse().ok())
                    {
                        if let Some((pid, completed)) =
                            context.shell.jobs_mut().take_completed_for_id(job_id)
                        {
                            result = completed;
                            last_waited_id = Some(
                                pid.map_or_else(|| format!("%{job_id}"), |pid| pid.to_string()),
                            );
                        } else {
                            writeln!(
                                context.stderr(),
                                "{}: {id}: no such job",
                                context.command_name
                            )?;
                            result = ExecutionResult::new(127);
                        }
                    } else {
                        writeln!(
                            context.stderr(),
                            "{}: no such job: {}",
                            context.command_name,
                            id
                        )?;

                        result = ExecutionResult::new(127);
                    }
                } else {
                    // It's a process ID.
                    let Ok(pid) = id.parse() else {
                        writeln!(
                            context.stderr(),
                            "{}: `{id}`: not a pid or valid job spec",
                            context.command_name
                        )?;
                        result = ExecutionExitCode::GeneralError.into();
                        continue;
                    };
                    if let Some(job) = context.shell.jobs_mut().job_for_pid_mut(pid) {
                        result = if self.wait_for_terminate {
                            job.wait_for_termination().await?
                        } else {
                            job.wait().await?
                        };
                        waited_for_pid = true;
                        last_waited_id = Some(pid.to_string());
                        context.shell.jobs_mut().remember_reaped(pid, &result);
                    } else if let Some(completed) =
                        context.shell.jobs_mut().take_completed_for_pid(pid)
                    {
                        result = completed;
                        last_waited_id = Some(pid.to_string());
                        context.shell.jobs_mut().remember_reaped(pid, &result);
                    } else if let Some(saved) = context.shell.jobs().reaped_status(pid) {
                        // Bash keeps reaped background statuses for later `wait PID`.
                        result = saved;
                        last_waited_id = Some(pid.to_string());
                    } else {
                        writeln!(
                            context.stderr(),
                            "{}: pid {pid} is not a child of this shell",
                            context.command_name
                        )?;
                        result = ExecutionResult::new(127);
                    }
                }
            }
            if waited_for_pid {
                context.shell.jobs_mut().sweep_waited_jobs();
            }
        } else {
            // Wait for all jobs.
            let jobs = context
                .shell
                .jobs_mut()
                .wait_all(self.wait_for_terminate)
                .await?;

            if context.shell.options().enable_job_control {
                for job in jobs {
                    writeln!(context.stdout(), "{job}")?;
                }
            }
        }

        if let (Some(name), Some(value)) = (&self.variable_to_receive_id, last_waited_id) {
            context.shell.env_mut().update_or_add(
                name,
                variables::ShellValueLiteral::Scalar(value),
                |_| Ok(()),
                env::EnvironmentLookup::Anywhere,
                env::EnvironmentScope::Global,
            )?;
        }

        Ok(result)
    }
}

/// Trapped signals that interrupt `wait`, as in Bash: the wait returns
/// 128+signal and the trap runs, while the awaited children keep running.
#[cfg(unix)]
struct TrappedSignals {
    term: Option<TrapSignal>,
    int: Option<TrapSignal>,
}

#[cfg(unix)]
impl TrappedSignals {
    fn of<SE: brush_core::ShellExtensions>(shell: &brush_core::Shell<SE>) -> Self {
        let trapped = |name: &str| {
            name.parse()
                .ok()
                .filter(|signal| shell.traps().handles(*signal))
        };
        Self {
            term: trapped("TERM"),
            int: trapped("INT"),
        }
    }

    /// Resolves with the trapped signal and its 128+N status.
    async fn recv(&self) -> Result<(TrapSignal, u8), brush_core::Error> {
        tokio::select! {
            result = brush_core::traps::await_term(), if self.term.is_some() => {
                result.map(|()| (self.term.unwrap_or(TrapSignal::Exit), 143))
            }
            result = brush_core::traps::await_int(), if self.int.is_some() => {
                result.map(|()| (self.int.unwrap_or(TrapSignal::Exit), 130))
            }
            else => std::future::pending().await,
        }
    }
}

#[cfg(unix)]
async fn run_trap<SE: brush_core::ShellExtensions>(
    context: &mut brush_core::ExecutionContext<'_, SE>,
    (signal, status): (TrapSignal, u8),
) -> Result<ExecutionResult, brush_core::Error> {
    context.shell.set_last_exit_status(status);
    let trap_result = context
        .shell
        .invoke_trap_handler(signal, &context.params)
        .await?;
    Ok(if trap_result.is_normal_flow() {
        ExecutionResult::new(status)
    } else {
        trap_result
    })
}
