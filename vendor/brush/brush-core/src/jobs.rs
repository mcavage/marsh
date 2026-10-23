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
use crate::tasks;
use crate::trace_categories;
use crate::traps;

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
    Internal(tasks::TaskHandle<Result<ExecutionResult, error::Error>>),
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
                    processes::ProcessWaitResult::Completed { result, .. } => {
                        Ok(JobTaskWaitResult::Completed(result))
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
            Self::External(process) => process.poll(),
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
            inherited: self
                .listed_jobs()
                .filter(|job| !job.is_notified_dead())
                .map(Job::view)
                .collect(),
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
        self.cleanup_dead_jobs();
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
            name => match find_by_name(&self.jobs, name) {
                JobSpecMatch::Found(index) => self.jobs.get_mut(index),
                JobSpecMatch::NoSuchJob | JobSpecMatch::Ambiguous => None,
            },
        }
    }

    /// Resolves a `disown` operand as Bash does: a number names a PID,
    /// anything else (or nothing: the current job) a job spec. The parent's
    /// jobs, as listed in a pipeline stage or command substitution, count.
    pub fn find_for_disown(&self, operand: Option<&str>) -> JobSpecMatch<JobKey> {
        let tables = [(false, &self.jobs), (true, &self.inherited)];
        let found = |inherited, index| JobSpecMatch::Found(JobKey { inherited, index });
        let Some(operand) = operand else {
            return tables
                .iter()
                .find_map(|(inherited, jobs)| {
                    jobs.iter()
                        .position(Job::is_current)
                        .map(|index| found(*inherited, index))
                })
                .unwrap_or(JobSpecMatch::NoSuchJob);
        };
        if let Some(pid) = parse_pid(operand) {
            return tables
                .iter()
                .find_map(|(inherited, jobs)| {
                    jobs.iter()
                        .position(|job| job.has_pid(pid))
                        .map(|index| found(*inherited, index))
                })
                .unwrap_or(JobSpecMatch::NoSuchJob);
        }
        let spec = operand.strip_prefix('%').unwrap_or(operand);
        for (inherited, jobs) in tables {
            let result = match spec {
                "" if operand.is_empty() => JobSpecMatch::NoSuchJob,
                "" | "%" | "+" => jobs
                    .iter()
                    .position(Job::is_current)
                    .map_or(JobSpecMatch::NoSuchJob, JobSpecMatch::Found),
                "-" => jobs
                    .iter()
                    .position(Job::is_prev)
                    .map_or(JobSpecMatch::NoSuchJob, JobSpecMatch::Found),
                s if s.bytes().all(|b| b.is_ascii_digit()) => s
                    .parse::<usize>()
                    .ok()
                    .and_then(|id| jobs.iter().position(|j| j.id == id))
                    .map_or(JobSpecMatch::NoSuchJob, JobSpecMatch::Found),
                name => find_by_name(jobs, name),
            };
            match result {
                JobSpecMatch::Found(index) => return found(inherited, index),
                JobSpecMatch::Ambiguous => return JobSpecMatch::Ambiguous,
                JobSpecMatch::NoSuchJob => {}
            }
        }
        JobSpecMatch::NoSuchJob
    }

    /// Returns the job a [`JobKey`] names.
    pub fn get(&self, key: JobKey) -> Option<&Job> {
        if key.inherited {
            self.inherited.get(key.index)
        } else {
            self.jobs.get(key.index)
        }
    }

    /// Marks a job so a SIGHUP the shell sends its jobs skips it (`disown -h`).
    pub fn set_nohup(&mut self, key: JobKey) {
        let jobs = if key.inherited {
            &mut self.inherited
        } else {
            &mut self.jobs
        };
        if let Some(job) = jobs.get_mut(key.index) {
            job.nohup = true;
        }
    }

    /// Removes a job from the table without signaling it (`disown`). As in
    /// Bash, a later `wait PID` reports the status known at this point, and
    /// the shell still reaps the process.
    pub fn disown(&mut self, key: JobKey) {
        if key.inherited {
            if key.index < self.inherited.len() {
                self.inherited.remove(key.index);
            }
            return;
        }
        if key.index >= self.jobs.len() {
            return;
        }
        let job = self.jobs.remove(key.index);
        if let Some(pid) = job.representative_pid() {
            self.remember_reaped(pid, &job.known_status());
        }
        self.reannotate();
        reap_in_background(job);
    }

    /// `disown -a` and `disown -r`: removes every job (or every running one),
    /// remembering none of their statuses; `-a` also forgets saved statuses.
    pub fn disown_all(&mut self, running_only: bool) {
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.jobs)
            .into_iter()
            .partition(|job| !running_only || job.is_running());
        self.jobs = kept;
        if !running_only {
            self.inherited.clear();
            self.completed.clear();
            self.reaped.clear();
        } else {
            self.inherited.retain(|job| !job.is_running());
        }
        // Bash leaves no current or previous job, even among those kept.
        for job in &mut self.jobs {
            job.annotation = JobAnnotation::None;
        }
        gone.into_iter().for_each(reap_in_background);
    }

    /// `disown -ah` and `disown -rh`: marks every job (or every running one)
    /// so a SIGHUP the shell sends its jobs skips it.
    pub fn set_nohup_all(&mut self, running_only: bool) {
        for job in self.jobs.iter_mut().chain(&mut self.inherited) {
            if !running_only || job.is_running() {
                job.nohup = true;
            }
        }
    }

    /// Sends SIGHUP (and so SIGCONT to a stopped job) to every job not marked
    /// by `disown -h`, as Bash does when an interactive login shell with
    /// `huponexit` exits.
    pub fn hangup_all(&mut self) {
        for job in self
            .jobs
            .iter_mut()
            .filter(|job| !job.nohup && !job.tasks.is_empty())
        {
            #[cfg(unix)]
            {
                let _ = job.kill(traps::TrapSignal::Signal(sys::signal::Signal::SIGHUP));
            }
        }
    }

    /// Removes the dead jobs an earlier `wait PID` or `wait %N` collected.
    /// Bash keeps such a job (`disown %1` still finds it) until the next
    /// `jobs`, `wait`, or foreground or background process.
    pub fn cleanup_dead_jobs(&mut self) {
        let before = self.jobs.len();
        self.jobs.retain(|job| !job.is_notified_dead());
        if self.jobs.len() != before {
            self.reannotate();
        }
    }

    /// Waits for all managed jobs to complete. With `keep_finished_last` (a
    /// noninteractive shell), the `$!` job is kept, unreported, if it had
    /// already finished when the wait began, as Bash keeps it.
    pub async fn wait_all(
        &mut self,
        terminate_only: bool,
        keep_finished_last: bool,
    ) -> Result<Vec<Job>, error::Error> {
        let finished_last = self
            .last_background_pid
            .filter(|_| keep_finished_last)
            .and_then(|pid| {
                let job = self
                    .jobs
                    .iter_mut()
                    .find(|job| job.representative_pid() == Some(pid))?;
                matches!(job.poll_done(), Ok(Some(_))).then_some(job.id)
            });
        for job in &mut self.jobs {
            // The caller wants the jobs, not their statuses: `wait` with no operands returns 0
            // whatever they were.
            let _ = if terminate_only {
                job.wait_for_termination().await?
            } else {
                job.wait().await?
            };
        }

        self.completed.clear();
        let kept = finished_last.and_then(|id| {
            let index = self.jobs.iter().position(|job| job.id == id)?;
            Some(self.jobs.remove(index))
        });
        let swept = self.sweep_completed_jobs();
        if let Some(job) = kept {
            self.jobs.push(job);
            self.jobs.sort_by_key(|job| job.id);
        }
        Ok(swept)
    }

    /// Removes finished jobs that `jobs` has just reported.
    pub fn forget_reported(&mut self, ids: &[usize]) {
        let before = self.jobs.len();
        self.jobs
            .retain(|job| !(job.tasks.is_empty() && ids.contains(&job.id)));
        if self.jobs.len() != before {
            self.reannotate();
        }
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

    /// Ends a `wait PID` or `wait %N`: removes the jobs an earlier wait
    /// collected and keeps the ones this wait collected until the next
    /// [`Self::cleanup_dead_jobs`].
    pub fn finish_explicit_wait(&mut self) {
        self.cleanup_dead_jobs();
        for job in &mut self.jobs {
            if job.tasks.is_empty() {
                job.notified = true;
            }
        }
    }

    /// Polls all managed jobs for completion.
    pub fn poll(&mut self) -> Result<Vec<JobResult>, error::Error> {
        self.cleanup_dead_jobs();
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
    /// The process group ID the job was launched with, or its first process.
    pgid: Option<sys::process::ProcessId>,
    /// The selected pipeline stage and status, retained across polls and canceled waits.
    wait_result: Option<(usize, ExecutionResult)>,
    /// Whether to select the rightmost failing stage, captured at pipeline launch.
    pipefail: bool,

    /// The annotation of the job (e.g., current, previous).
    annotation: JobAnnotation,

    /// The shell-internal ID of the job.
    pub id: usize,

    /// The command line of the job.
    pub command_line: String,

    /// The current operational state of the job.
    pub state: JobState,

    /// Whether `disown -h` exempted the job from the shell's SIGHUP.
    nohup: bool,

    /// Whether an explicit `wait` collected the job, which stays listed
    /// until the next cleanup (Bash's `J_NOTIFIED`).
    notified: bool,
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
            nohup: false,
            notified: false,
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
            nohup: self.nohup,
            notified: false,
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

    /// Returns whether `disown -h` exempted the job from the shell's SIGHUP.
    pub const fn is_nohup(&self) -> bool {
        self.nohup
    }

    const fn is_running(&self) -> bool {
        matches!(self.state, JobState::Running)
    }

    fn is_notified_dead(&self) -> bool {
        self.notified && self.tasks.is_empty()
    }

    /// Whether `pid` is one of the job's processes or its process group.
    fn has_pid(&self, pid: sys::process::ProcessId) -> bool {
        self.representative_pid == Some(pid)
            || self.pgid == Some(pid)
            || self.tasks.iter().any(|(_, task)| match task {
                JobTask::External(process) => process.pid() == Some(pid),
                JobTask::Internal(_) => false,
            })
    }

    /// The status a `wait` would report now without waiting: the collected
    /// status of a finished job, success for one still running (Bash records
    /// the last process's status as it stands).
    fn known_status(&self) -> ExecutionResult {
        if self.tasks.is_empty() {
            self.wait_result
                .map_or_else(ExecutionResult::success, |(_, result)| result)
        } else {
            ExecutionResult::success()
        }
    }

    /// The commands of the job's pipeline stages, for `%name` job specs.
    fn stage_commands(&self) -> impl Iterator<Item = &str> {
        self.command_line.split('|').map(str::trim_start)
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
        self.continue_job()
    }

    /// Moves the job to execute in the foreground.
    pub fn move_to_foreground(&mut self) -> Result<(), error::Error> {
        // The job gets the terminal before it's continued, so that it doesn't find itself in the
        // background and stop again on reading from the terminal. (Without a terminal, as in a
        // script that enables job control, there's none to give it, and it's continued anyway.)
        if let Some(pgid) = self.own_process_group()
            && let Err(err) = sys::terminal::move_to_foreground(pgid)
        {
            tracing::debug!(target: trace_categories::JOBS, "not giving job {} the terminal: {err}", self.id);
        }

        self.continue_job()
    }

    /// Continues the job, and records it running.
    fn continue_job(&mut self) -> Result<(), error::Error> {
        if matches!(self.state, JobState::Stopped) {
            self.continue_processes()?;
            self.state = JobState::Running;
        } else {
            // As bash does for `fg`, continue it anyway: it may have been stopped without the
            // shell noticing yet (e.g., by reading from the terminal in the background).
            let _ = self.continue_processes();
        }

        Ok(())
    }

    /// Continues the job's stopped processes: all of its process group, if it has one.
    fn continue_processes(&self) -> Result<(), error::Error> {
        if let Some(pgid) = self.own_process_group() {
            return sys::signal::continue_process_group(pgid);
        }
        let mut pids = self.task_pids();
        if pids.is_empty() {
            pids.extend(self.process_group_id());
        }
        if pids.is_empty() {
            return Err(error::ErrorKind::FailedToSendSignal.into());
        }
        let mut result = Ok(());
        for pid in pids {
            if let Err(error) = sys::signal::continue_process(pid) {
                result = Err(error);
            }
        }
        result
    }

    /// Checks whether the job can be signaled.
    pub fn check_signalable(&self) -> Result<(), error::Error> {
        if let Some(pgid) = self.own_process_group() {
            return sys::signal::check_process_group_signalable(pgid);
        }
        let mut pids = self.task_pids();
        if pids.is_empty() {
            pids.extend(self.process_group_id());
        }
        if pids.is_empty() {
            return Err(error::ErrorKind::FailedToSendSignal.into());
        }
        let mut result = Ok(());
        for pid in pids {
            if let Err(error) = sys::signal::check_signalable(pid) {
                result = Err(error);
            }
        }
        result
    }

    /// Kills the job: sends the signal to all of its process group, if it has one. A job sent
    /// SIGTERM or SIGHUP is also continued, so that it acts on it if it's stopped.
    ///
    /// # Arguments
    ///
    /// * `signal` - The signal to send to the job.
    pub fn kill(&mut self, signal: traps::TrapSignal) -> Result<(), error::Error> {
        if let Some(pgid) = self.own_process_group() {
            sys::signal::kill_process_group(pgid, signal)?;
        } else {
            self.kill_each_process(signal)?;
        }

        // As bash does, continue a stopped job sent SIGTERM or SIGHUP, so that it acts on it;
        // and one not known to be stopped, which may have stopped without the shell's noticing.
        if sys::signal::continue_stopped_on(signal) {
            self.continue_job()?;
        }

        Ok(())
    }

    /// Tries to retrieve a "representative" pid for the job.
    pub fn representative_pid(&self) -> Option<sys::process::ProcessId> {
        self.representative_pid
    }

    /// Tries to retrieve the process group ID (PGID) of the job: that of its own process group,
    /// if it has one, or else its representative pid.
    pub fn process_group_id(&self) -> Option<sys::process::ProcessId> {
        // TODO(jobs): Don't assume that the first PID is the PGID.
        self.pgid.or_else(|| {
            self.representative_pid()
                .filter(|pid| *pid < SYNTHETIC_PID_BASE)
        })
    }

    /// Returns the job's own process group, if it has one: a group other than the shell's,
    /// led by the first of the job's processes. Without job control, a job's processes
    /// are in the shell's group, and are signaled one by one.
    fn own_process_group(&self) -> Option<sys::process::ProcessId> {
        self.pgid
            .filter(|pgid| sys::signal::leads_separate_process_group(*pgid))
    }

    /// The process IDs of the job's processes that are still tasks of the job.
    fn task_pids(&self) -> Vec<sys::process::ProcessId> {
        self.tasks
            .iter()
            .filter_map(|(_, task)| match task {
                JobTask::External(process) => process.pid(),
                JobTask::Internal(_) => None,
            })
            .collect()
    }

    /// Sends a signal to each of the job's processes; as Bash does for a job without a process
    /// group of its own, that includes every stage of a pipeline.
    fn kill_each_process(&self, signal: traps::TrapSignal) -> Result<(), error::Error> {
        let pids = self.task_pids();
        if pids.is_empty() {
            return self.process_group_id().map_or_else(
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
}

/// Identifies a job in a [`JobManager`]: one of its own, or one listed from
/// the parent shell.
#[derive(Clone, Copy)]
pub struct JobKey {
    inherited: bool,
    index: usize,
}

/// The outcome of resolving a job specification.
pub enum JobSpecMatch<T = usize> {
    /// The spec names this job.
    Found(T),
    /// No job matches.
    NoSuchJob,
    /// A `%name` or `%?name` spec matches more than one job.
    Ambiguous,
}

/// Bash's `%name` (a stage command starting with `name`) and `%?name` (a
/// stage command containing `name`, ignoring case) job specs, newest first.
/// As in Bash, a job whose match is not its first stage is ambiguous.
fn find_by_name(jobs: &[Job], name: &str) -> JobSpecMatch {
    let matches = |command: &str| {
        if let Some(needle) = name.strip_prefix('?') {
            command
                .to_ascii_lowercase()
                .contains(&needle.to_ascii_lowercase())
        } else {
            command.starts_with(name)
        }
    };
    let mut found = None;
    for (index, job) in jobs.iter().enumerate().rev() {
        let Some(stage) = job.stage_commands().position(matches) else {
            continue;
        };
        if found.is_some() || stage > 0 {
            return JobSpecMatch::Ambiguous;
        }
        found = Some(index);
    }
    found.map_or(JobSpecMatch::NoSuchJob, JobSpecMatch::Found)
}

/// Bash's `legal_number` restricted to a PID: optional surrounding blanks and
/// sign, decimal digits, and a value that fits a process ID.
pub fn parse_pid(text: &str) -> Option<sys::process::ProcessId> {
    let trimmed = text.trim_matches([' ', '\t', '\n']);
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse().ok()
}

/// Reaps a job the shell no longer tracks, as Bash's SIGCHLD handler would.
fn reap_in_background(mut job: Job) {
    if job.tasks.is_empty() || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move { while job.wait().await.is_ok() && !job.tasks.is_empty() {} });
}
