use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionResult, builtins, error, jobs};

/// Manage jobs.
#[derive(Parser)]
pub(crate) struct JobsCommand {
    /// Also show process IDs.
    #[arg(short = 'l')]
    also_show_pids: bool,

    /// List only jobs that have changed status since the last notification.
    #[arg(short = 'n')]
    list_changed_only: bool,

    /// Show only process group IDs.
    #[arg(short = 'p')]
    show_pids_only: bool,

    /// Show only running jobs.
    #[arg(short = 'r')]
    running_jobs_only: bool,

    /// Show only stopped jobs.
    #[arg(short = 's')]
    stopped_jobs_only: bool,

    /// Job specs to list.
    // TODO(jobs): Add -x option
    job_specs: Vec<String>,
}

impl builtins::Command for JobsCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if self.list_changed_only {
            return error::unimp("jobs -n");
        }

        context.shell.jobs_mut().cleanup_dead_jobs();
        if self.job_specs.is_empty() {
            let mut reported = vec![];
            for job in context.shell.jobs().listed_jobs() {
                if self.display_job(&context, job)? && matches!(job.state, jobs::JobState::Done) {
                    reported.push(job.id);
                }
            }
            context.shell.jobs_mut().forget_reported(&reported);
        } else {
            return error::unimp("jobs with job specs");
        }

        Ok(ExecutionResult::success())
    }
}

impl JobsCommand {
    fn display_job(
        &self,
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        job: &jobs::Job,
    ) -> Result<bool, brush_core::Error> {
        if self.running_jobs_only && !matches!(job.state, jobs::JobState::Running) {
            return Ok(false);
        }
        if self.stopped_jobs_only && !matches!(job.state, jobs::JobState::Stopped) {
            return Ok(false);
        }

        if self.show_pids_only {
            if let Some(pid) = job.process_group_id().or_else(|| job.representative_pid()) {
                writeln!(context.stdout(), "{pid}")?;
            }
        } else if self.also_show_pids {
            writeln!(context.stdout(), "{}", job.to_long_string())?;
        } else {
            writeln!(context.stdout(), "{job}")?;
        }

        Ok(true)
    }
}
