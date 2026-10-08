//! Job management

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::Display;

use futures::FutureExt;

use crate::ExecutionResult;
use crate::error;
use crate::processes;
use crate::results::ExecutionExitCode;
use crate::sys;
use crate::trace_categories;
use crate::traps;

pub(crate) type JobJoinHandle = tokio::task::JoinHandle<Result<ExecutionResult, error::Error>>;
pub(crate) type JobResult = (Job, Result<ExecutionResult, error::Error>);

/// Linux `PID_MAX_LIMIT`; synthetic in-process job identities start here.
const SYNTHETIC_PID_BASE: sys::process::ProcessId = 1 << 22;

/// Manages the jobs that are currently managed by the shell.
#[derive(Default)]
pub struct JobManager {
    /// The jobs that are currently managed by the shell.
    pub jobs: Vec<Job>,
    /// Statuses reported at a prompt but not yet collected by wait.
    completed: VecDeque<CompletedJob>,
    /// Bash retains `$!` after the job finishes or another job becomes current.
    last_background_pid: Option<sys::process::ProcessId>,
    /// Exit statuses of PIDs already collected by `wait`, as Bash's `bgpids`.
    reaped: VecDeque<(sys::process::ProcessId, ExecutionExitCode)>,
    /// The parent shell's jobs as seen by `jobs` in a pipeline stage or command
    /// substitution. Bash lists them there, but they are not waitable.
    inherited: Vec<Job>,
}

struct CompletedJob {
    id: usize,
    pid: Option<sys::process::ProcessId>,
    result: ExecutionResult,
}

/// Represents a task that is part of a job.
pub enum JobTask {
    /// An external process.
    External(processes::ChildProcess),
    /// An internal asynchronous task.
    Internal(JobJoinHandle),
}

/// Represents the result of waiting on a job task.
pub enum JobTaskWaitResult {
    /// The task has completed.
    Completed(ExecutionResult),
    /// The task was stopped.
    Stopped,
}

impl JobTask {
    /// Returns whether the task is an external process.
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// Waits for the task to complete. Returns the result of the wait.
    pub async fn wait(&mut self) -> Result<JobTaskWaitResult, error::Error> {
        match self {
            Self::External(process) => {
                let wait_result = process.wait().await?;
                match wait_result {
                    processes::ProcessWaitResult::Completed { output, .. } => {
                        Ok(JobTaskWaitResult::Completed(output.into()))
                    }
                    processes::ProcessWaitResult::Stopped => Ok(JobTaskWaitResult::Stopped),
                }
            }
            Self::Internal(handle) => Ok(JobTaskWaitResult::Completed(handle.await??)),
        }
    }

    /// Polls the task for completion. Returns `Some(result)` if the task has completed,
    /// or `None` if it is still running. The result is the execution result of the task.
    /// Behaves in a best-effort manner; if an internal error occurs during polling,
    /// it will return `None`.
    fn poll(&mut self) -> Option<Result<ExecutionResult, error::Error>> {
        match self {
            Self::External(process) => {
                let check_result = process.poll();
                check_result.map(|polled_result| polled_result.map(|output| output.into()))
            }
            Self::Internal(handle) => {
                let checkable_handle = handle;
                checkable_handle.now_or_never().and_then(|r| r.ok())
            }
        }
    }
}

impl JobManager {
    /// Returns a new job manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns an empty job manager for a subshell that can list, but not wait
    /// for or signal through job state, this manager's jobs.
    #[must_use]
    pub fn subshell_view(&self) -> Self {
        Self {
            inherited: self.listed_jobs().map(Job::view).collect(),
            ..Self::default()
        }
    }

    /// Forgets the parent jobs inherited by [`Self::subshell_view`], as an
    /// explicit `( ... )` subshell does in Bash.
    pub fn clear_inherited(&mut self) {
        self.inherited.clear();
    }

    /// Returns the jobs that `jobs` lists: this shell's own, then any listed
    /// by the parent shell.
    pub fn listed_jobs(&self) -> impl Iterator<Item = &Job> {
        self.jobs.iter().chain(&self.inherited)
    }

    /// Adds a job to the job manager and marks it as the current job;
    /// returns an immutable reference to the job.
    ///
    /// # Arguments
    ///
    /// * `job` - The job to add.
    #[allow(
        clippy::missing_panics_doc,
        reason = "push() guarantees the vector length is >= 1"
    )]
    pub fn add_as_current(&mut self, mut job: Job) -> &Job {
        for j in &mut self.jobs {
            j.annotation = match j.annotation {
                JobAnnotation::Current => JobAnnotation::Previous,
                JobAnnotation::Previous | JobAnnotation::None => JobAnnotation::None,
            };
        }

        // Bash numbers a new job one past the highest job still in the table,
        // so numbers are reused once earlier jobs are reported done.
        let id = self.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        // A reported job with this number keeps its `wait PID` status only.
        while let Some(index) = self.completed.iter().position(|c| c.id == id) {
            if let Some(stale) = self.completed.remove(index) {
                if let Some(pid) = stale.pid {
                    self.remember_reaped(pid, &stale.result);
                }
            }
        }
        job.id = id;
        job.annotation = JobAnnotation::Current;
        self.jobs.push(job);

        #[allow(clippy::unwrap_used, reason = "we just pushed an element")]
        self.jobs.last().unwrap()
    }

    /// Adds a background job and remembers its last pipeline process for `$!`.
    pub fn add_as_background(&mut self, mut job: Job) -> &Job {
        if job.representative_pid.is_none() {
            // An in-process background list has no OS process. Give it a
            // waitable identity above every platform PID limit so `$!`,
            // `jobs -p`, and `wait PID` work; it is never signaled as a PID.
            static NEXT_SYNTHETIC_PID: std::sync::atomic::AtomicI32 =
                std::sync::atomic::AtomicI32::new(SYNTHETIC_PID_BASE);
            job.representative_pid =
                Some(NEXT_SYNTHETIC_PID.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        }
        self.last_background_pid = job.representative_pid();
        self.add_as_current(job)
    }

    /// Returns the most recently launched background process, even after reap.
    pub fn last_background_pid(&self) -> Option<sys::process::ProcessId> {
        self.last_background_pid
    }

    /// Returns the current job, if there is one.
    pub fn current_job(&self) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| matches!(j.annotation, JobAnnotation::Current))
    }

    /// Returns a mutable reference to the current job, if there is one.
    pub fn current_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| matches!(j.annotation, JobAnnotation::Current))
    }

    /// Returns the previous job, if there is one.
    pub fn prev_job(&self) -> Option<&Job> {
        self.jobs
            .iter()
            .find(|j| matches!(j.annotation, JobAnnotation::Previous))
    }

    /// Returns a mutable reference to the previous job, if there is one.
    pub fn prev_job_mut(&mut self) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|j| matches!(j.annotation, JobAnnotation::Previous))
    }

    /// Tries to resolve the given job specification to a job.
    ///
    /// # Arguments
    ///
    /// * `job_spec` - The job specification to resolve.
    pub fn resolve_job_spec(&mut self, job_spec: &str) -> Option<&mut Job> {
        let remainder = job_spec.strip_prefix('%')?;

        match remainder {
            "%" | "+" => self.current_job_mut(),
            "-" => self.prev_job_mut(),
            s if s.chars().all(char::is_numeric) => {
                let id = s.parse::<usize>().ok()?;
                self.jobs.iter_mut().find(|j| j.id == id)
            }
            _ => {
                tracing::warn!(target: trace_categories::UNIMPLEMENTED, "unimplemented: job spec naming command: '{job_spec}'");
                None
            }
        }
    }

    /// Waits for all managed jobs to complete.
    pub async fn wait_all(&mut self, terminate_only: bool) -> Result<Vec<Job>, error::Error> {
        for job in &mut self.jobs {
            if terminate_only {
                job.wait_for_termination().await?;
            } else {
                job.wait().await?;
            }
        }

        self.completed.clear();
        Ok(self.sweep_completed_jobs())
    }

    /// Waits for the first eligible job to complete, preserving every other
    /// job so a later wait can still collect its result.
    pub async fn wait_first(
        &mut self,
        eligible_ids: &[usize],
        terminate_only: bool,
    ) -> Result<Option<(usize, Option<sys::process::ProcessId>, ExecutionResult)>, error::Error>
    {
        if let Some(index) = self
            .completed
            .iter()
            .position(|job| eligible_ids.contains(&job.id))
        {
            if let Some(job) = self.completed.remove(index) {
                return Ok(Some((job.id, job.pid, job.result)));
            }
        }
        let waits = self
            .jobs
            .iter_mut()
            .filter(|job| eligible_ids.contains(&job.id) && !job.tasks.is_empty())
            .map(|job| {
                let id = job.id;
                let pid = job.representative_pid();
                Box::pin(async move {
                    let result = if terminate_only {
                        job.wait_for_termination().await
                    } else {
                        job.wait().await
                    };
                    (id, pid, result)
                })
            })
            .collect::<Vec<_>>();
        if waits.is_empty() {
            return Ok(None);
        }
        let ((id, pid, result), _, _) = futures::future::select_all(waits).await;
        let result = result?;
        self.sweep_waited_jobs();
        Ok(Some((id, pid, result)))
    }

    /// Resolves a child process ID to its managed job.
    pub fn job_for_pid_mut(&mut self, pid: sys::process::ProcessId) -> Option<&mut Job> {
        self.jobs
            .iter_mut()
            .find(|job| job.representative_pid() == Some(pid))
    }

    /// Remembers the status of a PID collected by `wait`.
    pub fn remember_reaped(&mut self, pid: sys::process::ProcessId, result: &ExecutionResult) {
        const MAX_REAPED: usize = 1024;
        self.reaped.retain(|(reaped, _)| *reaped != pid);
        if self.reaped.len() == MAX_REAPED {
            self.reaped.pop_front();
        }
        self.reaped.push_back((pid, result.exit_code));
    }

    /// Returns the status of a PID previously collected by `wait`.
    pub fn reaped_status(&self, pid: sys::process::ProcessId) -> Option<ExecutionResult> {
        self.reaped
            .iter()
            .rev()
            .find(|(reaped, _)| *reaped == pid)
            .map(|(_, code)| ExecutionResult::from(*code))
    }

    /// Takes an already reported child status by PID.
    pub fn take_completed_for_pid(
        &mut self,
        pid: sys::process::ProcessId,
    ) -> Option<ExecutionResult> {
        let index = self.completed.iter().position(|job| job.pid == Some(pid))?;
        self.completed.remove(index).map(|job| job.result)
    }

    /// Takes an already reported child status by job ID.
    pub fn take_completed_for_id(
        &mut self,
        id: usize,
    ) -> Option<(Option<sys::process::ProcessId>, ExecutionResult)> {
        let index = self.completed.iter().position(|job| job.id == id)?;
        self.completed
            .remove(index)
            .map(|job| (job.pid, job.result))
    }

    /// IDs that remain eligible for wait.
    pub fn waitable_ids(&self) -> Vec<usize> {
        self.jobs
            .iter()
            .map(|job| job.id)
            .chain(self.completed.iter().map(|job| job.id))
            .collect()
    }

    /// Resolves a PID from an active or already reported job.
    pub fn waitable_id_for_pid(&self, pid: sys::process::ProcessId) -> Option<usize> {
        self.jobs
            .iter()
            .find(|job| job.representative_pid() == Some(pid))
            .map(|job| job.id)
            .or_else(|| {
                self.completed
                    .iter()
                    .find(|job| job.pid == Some(pid))
                    .map(|job| job.id)
            })
    }

    /// Removes jobs whose tasks were consumed by an explicit wait.
    pub fn sweep_waited_jobs(&mut self) {
        let _ = self.sweep_completed_jobs();
    }

    /// Polls all managed jobs for completion.
    pub fn poll(&mut self) -> Result<Vec<JobResult>, error::Error> {
        let mut results = Vec::with_capacity(self.jobs.len());

        let mut i = 0;
        while i != self.jobs.len() {
            let pid = self.jobs[i].representative_pid();
            if let Some(result) = self.jobs[i].poll_done()? {
                let job = self.jobs.remove(i);
                if let Ok(completed) = &result {
                    self.completed.push_back(CompletedJob {
                        id: job.id,
                        pid,
                        result: *completed,
                    });
                }
                results.push((job, result));
            } else if matches!(self.jobs[i].state, JobState::Done) {
                // TODO(jobs): This is a workaround to remove jobs that are done but for which we
                // don't know what happened.
                results.push((self.jobs.remove(i), Ok(ExecutionResult::success())));
            } else {
                i += 1;
            }
        }

        if !results.is_empty() {
            self.reannotate();
        }
        Ok(results)
    }

    /// After jobs leave the table, keep one current (`+`) and one previous
    /// (`-`) job as Bash does: the previous job is promoted, then the newest.
    fn reannotate(&mut self) {
        if !self
            .jobs
            .iter()
            .any(|j| matches!(j.annotation, JobAnnotation::Current))
        {
            if let Some(previous) = self.prev_job_mut() {
                previous.annotation = JobAnnotation::Current;
            } else if let Some(newest) = self.jobs.iter_mut().max_by_key(|j| j.id) {
                newest.annotation = JobAnnotation::Current;
            }
        }
        if !self
            .jobs
            .iter()
            .any(|j| matches!(j.annotation, JobAnnotation::Previous))
        {
            if let Some(next) = self
                .jobs
                .iter_mut()
                .filter(|j| matches!(j.annotation, JobAnnotation::None))
                .max_by_key(|j| j.id)
            {
                next.annotation = JobAnnotation::Previous;
            }
        }
    }

    fn sweep_completed_jobs(&mut self) -> Vec<Job> {
        let mut completed_jobs = vec![];

        let mut i = 0;
        while i != self.jobs.len() {
            if self.jobs[i].tasks.is_empty() {
                completed_jobs.push(self.jobs.remove(i));
            } else {
                i += 1;
            }
        }

        if !completed_jobs.is_empty() {
            self.reannotate();
        }
        completed_jobs
    }
}

/// Represents the current execution state of a job.
#[derive(Clone)]
pub enum JobState {
    /// Unknown state.
    Unknown,
    /// The job is running.
    Running,
    /// The job is stopped.
    Stopped,
    /// The job has completed.
    Done,
}

impl Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "Unknown"),
            Self::Running => write!(f, "Running"),
            Self::Stopped => write!(f, "Stopped"),
            Self::Done => write!(f, "Done"),
        }
    }
}

/// Represents an annotation for a job.
#[derive(Clone)]
pub enum JobAnnotation {
    /// No annotation.
    None,
    /// The job is the current job.
    Current,
    /// The job is the previous job.
    Previous,
}

impl JobAnnotation {
    /// The one-character listing marker: `+`, `-`, or a space.
    const fn marker(&self) -> char {
        match self {
            Self::None => ' ',
            Self::Current => '+',
            Self::Previous => '-',
        }
    }
}

impl Display for JobAnnotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, ""),
            Self::Current => write!(f, "+"),
            Self::Previous => write!(f, "-"),
        }
    }
}

/// Encapsulates a set of processes managed by the shell as a single unit.
pub struct Job {
    /// The tasks that make up the job, indexed in pipeline order.
    tasks: VecDeque<(usize, JobTask)>,
    /// The original process ID used by `$!` and `wait`, even after polling
    /// removes a completed stage from a pipeline.
    representative_pid: Option<sys::process::ProcessId>,
    /// The selected pipeline stage and status, retained across polls and canceled waits.
    wait_result: Option<(usize, ExecutionResult)>,
    /// Whether to select the rightmost failing stage, captured at pipeline launch.
    pipefail: bool,

    /// If available, the process group ID of the job's processes.
    pgid: Option<sys::process::ProcessId>,

    /// The annotation of the job (e.g., current, previous).
    annotation: JobAnnotation,

    /// The shell-internal ID of the job.
    pub id: usize,

    /// The command line of the job.
    pub command_line: String,

    /// The current operational state of the job.
    pub state: JobState,
}

impl Display for Job {
    /// Bash's standard listing: `[1]+  Running                 sleep 10 &`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.to_string();
        let background = if matches!(self.state, JobState::Running) {
            " &"
        } else {
            ""
        };
        write!(
            f,
            "[{}]{}  {state:<27}{}{background}",
            self.id,
            self.annotation.marker(),
            self.command_line
        )
    }
}

impl Job {
    /// Returns a new job object.
    ///
    /// # Arguments
    ///
    /// * `children` - The job's known child processes.
    /// * `command_line` - The command line of the job.
    /// * `state` - The current operational state of the job.
    /// * `pipefail` - Whether to select the rightmost failing pipeline stage.
    pub(crate) fn new<I>(tasks: I, command_line: String, state: JobState, pipefail: bool) -> Self
    where
        I: IntoIterator<Item = JobTask>,
    {
        let tasks: VecDeque<_> = tasks.into_iter().enumerate().collect();
        let representative_pid = tasks.iter().rev().find_map(|(_, task)| match task {
            JobTask::External(process) => process.pid(),
            JobTask::Internal(_) => None,
        });
        let pgid = tasks.iter().find_map(|(_, task)| match task {
            JobTask::External(process) => process.pgid().or_else(|| process.pid()),
            JobTask::Internal(_) => None,
        });
        Self {
            id: 0,
            tasks,
            representative_pid,
            wait_result: None,
            pipefail,
            pgid,
            annotation: JobAnnotation::None,
            command_line,
            state,
        }
    }

    /// Returns a pid-style string for the job.
    pub fn to_pid_style_string(&self) -> String {
        let display_pid = self
            .representative_pid()
            .map_or(Cow::Borrowed("<pid unknown>"), |pid| {
                Cow::Owned(pid.to_string())
            });
        // Bash's launch line: `[1] 467`.
        std::format!("[{}] {}", self.id, display_pid)
    }

    /// Bash's completion notification: `[1]+  Done                    sleep 1`,
    /// or `Exit N` for a nonzero status.
    pub fn to_completion_string(&self, result: &ExecutionResult) -> String {
        let code = u8::from(result.exit_code);
        let state = if code == 0 {
            "Done".to_owned()
        } else {
            std::format!("Exit {code}")
        };
        std::format!(
            "[{}]{}  {state:<27}{}",
            self.id,
            self.annotation.marker(),
            self.command_line
        )
    }

    /// Returns the long `jobs -l` representation.
    pub fn to_long_string(&self) -> String {
        let display_pid = self
            .representative_pid()
            .map_or(Cow::Borrowed("<pid unknown>"), |pid| {
                Cow::Owned(pid.to_string())
            });
        std::format!(
            "[{}]{:3} {}\t{}\t{}",
            self.id,
            self.annotation,
            display_pid,
            self.state,
            self.command_line
        )
    }

    /// A copy of this job's metadata without its processes or tasks.
    fn view(&self) -> Self {
        Self {
            tasks: VecDeque::new(),
            representative_pid: self.representative_pid,
            wait_result: self.wait_result,
            pipefail: self.pipefail,
            pgid: self.pgid,
            annotation: self.annotation.clone(),
            id: self.id,
            command_line: self.command_line.clone(),
            state: self.state.clone(),
        }
    }

    /// Returns the annotation of the job.
    pub fn annotation(&self) -> JobAnnotation {
        self.annotation.clone()
    }

    /// Returns the command name of the job.
    pub fn command_name(&self) -> &str {
        self.command_line
            .split_ascii_whitespace()
            .next()
            .unwrap_or_default()
    }

    /// Returns whether the job is the current job.
    pub const fn is_current(&self) -> bool {
        matches!(self.annotation, JobAnnotation::Current)
    }

    /// Returns whether the job is the previous job.
    pub const fn is_prev(&self) -> bool {
        matches!(self.annotation, JobAnnotation::Previous)
    }

    // Polling consumes stages from the front; waits consume them from the back and may be
    // canceled. Select by original stage index, never by completion or observation order.
    fn record_task_result(&mut self, index: usize, result: ExecutionResult) {
        // A background list's `exit`/`return` ends only that list, never the waiter.
        let result = ExecutionResult::from(result.exit_code);
        let replace = self.wait_result.is_none_or(|(previous_index, previous)| {
            if self.pipefail && previous.is_success() != result.is_success() {
                !result.is_success()
            } else {
                index > previous_index
            }
        });
        if replace {
            self.wait_result = Some((index, result));
        }
    }

    /// Polls whether the job has completed.
    pub fn poll_done(
        &mut self,
    ) -> Result<Option<Result<ExecutionResult, error::Error>>, error::Error> {
        let mut result: Option<Result<ExecutionResult, error::Error>> = None;

        tracing::debug!(target: trace_categories::JOBS, "Polling job {} for completion...", self.id);

        while let Some((index, task)) = self.tasks.front_mut() {
            let index = *index;
            match task.poll() {
                Some(r) => {
                    self.tasks.pop_front();
                    result = Some(r.map(|completed| {
                        self.record_task_result(index, completed);
                        self.wait_result.map_or(completed, |(_, selected)| selected)
                    }));
                }
                None => {
                    return Ok(None);
                }
            }
        }

        tracing::debug!(target: trace_categories::JOBS, "Job {} has completed.", self.id);

        self.state = JobState::Done;

        Ok(result)
    }

    /// Waits for the job to complete.
    pub async fn wait(&mut self) -> Result<ExecutionResult, error::Error> {
        // Reap every stage, preserving the launch-time status policy even if a concurrent
        // wait is canceled or prompt polling has already consumed part of the pipeline.
        while let Some((index, task)) = self.tasks.back_mut() {
            let index = *index;
            match task.wait().await? {
                JobTaskWaitResult::Completed(execution_result) => {
                    self.record_task_result(index, execution_result);
                    self.tasks.pop_back();
                }
                JobTaskWaitResult::Stopped => {
                    self.state = JobState::Stopped;
                    return Ok(ExecutionResult::stopped());
                }
            }
        }

        self.state = JobState::Done;

        Ok(self
            .wait_result
            .map_or_else(ExecutionResult::success, |(_, result)| result))
    }

    /// Waits until this job terminates, ignoring intermediate stopped states.
    pub async fn wait_for_termination(&mut self) -> Result<ExecutionResult, error::Error> {
        loop {
            let result = self.wait().await?;
            if !matches!(self.state, JobState::Stopped) {
                return Ok(result);
            }
        }
    }

    /// Moves the job to execute in the background.
    pub fn move_to_background(&mut self) -> Result<(), error::Error> {
        if matches!(self.state, JobState::Stopped) {
            if let Some(pgid) = self.process_group_id() {
                sys::signal::continue_process(pgid)?;
                self.state = JobState::Running;
                Ok(())
            } else {
                Err(error::ErrorKind::FailedToSendSignal.into())
            }
        } else {
            error::unimp("move job to background")
        }
    }

    /// Moves the job to execute in the foreground.
    pub fn move_to_foreground(&mut self) -> Result<(), error::Error> {
        if matches!(self.state, JobState::Stopped) {
            if let Some(pgid) = self.process_group_id() {
                sys::signal::continue_process(pgid)?;
                self.state = JobState::Running;
            } else {
                return Err(error::ErrorKind::FailedToSendSignal.into());
            }
        }

        if let Some(pgid) = self.process_group_id() {
            sys::terminal::move_to_foreground(pgid)?;
        }

        Ok(())
    }

    /// Kills the job.
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal to send to the job.
    pub fn kill(&self, signal: traps::TrapSignal) -> Result<(), error::Error> {
        let pgid = self.process_group_id();
        // A job in its own process group is signaled as a group. Otherwise (no
        // job control), Bash signals each of the job's processes.
        if let Some(pgid) = pgid.filter(|pgid| sys::signal::leads_separate_process_group(*pgid)) {
            return sys::signal::kill_process(-pgid, signal);
        }
        let pids = self
            .tasks
            .iter()
            .filter_map(|(_, task)| match task {
                JobTask::External(process) => process.pid(),
                JobTask::Internal(_) => None,
            })
            .collect::<Vec<_>>();
        if pids.is_empty() {
            return pgid.map_or_else(
                || Err(error::ErrorKind::FailedToSendSignal.into()),
                |pid| sys::signal::kill_process(pid, signal),
            );
        }
        let mut result = Ok(());
        for pid in pids {
            if let Err(error) = sys::signal::kill_process(pid, signal) {
                result = Err(error);
            }
        }
        result
    }

    /// Tries to retrieve a "representative" pid for the job.
    pub fn representative_pid(&self) -> Option<sys::process::ProcessId> {
        self.representative_pid
    }

    /// Tries to retrieve the process group ID (PGID) of the job.
    pub fn process_group_id(&self) -> Option<sys::process::ProcessId> {
        // TODO(jobs): Don't assume that the first PID is the PGID.
        self.pgid.or_else(|| {
            self.representative_pid()
                .filter(|pid| *pid < SYNTHETIC_PID_BASE)
        })
    }
}
