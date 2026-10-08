//! Job management for shell instances.

use std::io::Write;

use crate::{error, extensions};

impl<SE: extensions::ShellExtensions> crate::Shell<SE> {
    /// Checks for completed jobs in the shell, reporting any changes found.
    pub fn check_for_completed_jobs(&mut self) -> Result<(), error::Error> {
        let results = self.jobs.poll()?;

        if self.options.enable_job_control {
            for (job, result) in results {
                let line = match result {
                    Ok(result) => job.to_completion_string(&result),
                    Err(_) => job.to_string(),
                };
                writeln!(self.stderr(), "{line}")?;
            }
        }

        Ok(())
    }
}
