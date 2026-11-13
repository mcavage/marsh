//! How many daemon connections jobs may hold. A job's own `cap.sock`
//! connections take a slot here before their bridge connects, and so do
//! the daemon's connections to itself on a job's behalf (a `ProcessRun`
//! child's start, the argv branches of a job's split). So jobs together
//! never hold more than [`capability_share`] of the daemon's connection cap,
//! and one job never more than twice its live `job-children` plus 4. The
//! rest of the cap is left to the host: `marsh` commands, relays, shells,
//! and host and session splits with their branches. What the host itself
//! opens can still fill that rest.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};

/// The daemon's connection cap at the smallest job pools.
pub const BASE_CAP: usize = 1024;

/// The connection cap a job pool of `daemon_jobs` needs: at least
/// [`BASE_CAP`], and enough that the jobs' share holds two connections
/// for every job of the pool (each child's request and the daemon's
/// connection that starts it), so a full pool of nested children fits.
/// `daemon-jobs` 64 keeps 1024; 512 needs 1371.
#[must_use]
pub const fn connection_cap_for(daemon_jobs: usize) -> usize {
    let jobs_side = daemon_jobs.saturating_mul(2).saturating_add(4);
    let cap = jobs_side.saturating_mul(4).div_ceil(3);
    if cap > BASE_CAP { cap } else { BASE_CAP }
}

/// Of a daemon connection cap, what job capability connections may hold
/// together: three quarters, so a quarter is always left for the host.
#[must_use]
pub const fn capability_share(cap: usize) -> usize {
    cap - cap / 4
}

/// What one job may hold at once with `job_children` live children allowed:
/// two connections per child (the job's request, and the daemon's own
/// connection that starts the child), plus 4.
#[must_use]
pub const fn per_job(job_children: usize) -> usize {
    job_children.saturating_mul(2).saturating_add(4)
}

#[derive(Debug, Default)]
struct Held {
    total: usize,
    by_job: BTreeMap<String, usize>,
}

#[derive(Debug)]
pub(crate) struct Budget {
    /// The most connections the daemon may serve whatever its job pool
    /// (tests lower it).
    limit: Mutex<usize>,
    held: Mutex<Held>,
}

impl Budget {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            limit: Mutex::new(usize::MAX),
            held: Mutex::default(),
        })
    }

    pub(crate) fn set_limit(&self, limit: usize) {
        *self
            .limit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = limit;
    }

    #[cfg(test)]
    pub(crate) fn limit(&self) -> usize {
        *self
            .limit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The connection cap now, for a job pool of `daemon_jobs`.
    pub(crate) fn cap(&self, daemon_jobs: usize) -> usize {
        let limit = *self
            .limit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        connection_cap_for(daemon_jobs).min(limit)
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A slot for one of `job`'s capability connections, or why there is none.
    pub(crate) fn acquire(
        self: &Arc<Self>,
        job: &str,
        job_children: usize,
        daemon_jobs: usize,
    ) -> Result<CapabilityConnection, String> {
        let share = capability_share(self.cap(daemon_jobs));
        let limit = per_job(job_children);
        let mut held = self.held();
        let mine = held.by_job.get(job).copied().unwrap_or(0);
        if mine >= limit {
            return Err(format!(
                "{} (at most {limit} at once: twice job-children, plus 4)",
                marsh_contracts::process::CONNECTION_REFUSAL
            ));
        }
        if held.total >= share {
            return Err(format!(
                "marsh: jobs hold {share} daemon connections, the most they may; this request was refused so `marsh` commands on the host still get through"
            ));
        }
        held.total += 1;
        held.by_job.insert(job.to_owned(), mine + 1);
        Ok(CapabilityConnection {
            budget: Arc::downgrade(self),
            job: job.to_owned(),
        })
    }

    #[cfg(test)]
    pub(crate) fn held_total(&self) -> usize {
        self.held().total
    }
}

/// One job capability connection's slot, released when dropped.
#[derive(Debug)]
pub struct CapabilityConnection {
    budget: Weak<Budget>,
    job: String,
}

impl Drop for CapabilityConnection {
    fn drop(&mut self) {
        let Some(budget) = self.budget.upgrade() else {
            return;
        };
        let mut held = budget.held();
        held.total = held.total.saturating_sub(1);
        if let Some(count) = held.by_job.get_mut(&self.job) {
            *count -= 1;
            if *count == 0 {
                held.by_job.remove(&self.job);
            }
        }
    }
}
