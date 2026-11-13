//! How many daemon connections jobs may hold. Every job capability
//! connection (`cap.sock`) takes a slot here before its bridge connects, so
//! jobs together never hold more than [`capability_share`] of the daemon's
//! connection cap, and one job never more than its live `job-children` plus
//! 4. The rest of the cap is the host's: `marsh` commands, `marsh jobs
//! cancel`, relays, shells, and the daemon's own split-branch and child-job
//! connections.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};

/// Of a daemon connection cap, what job capability connections may hold
/// together: three quarters, so a quarter is always left for the host.
#[must_use]
pub const fn capability_share(cap: usize) -> usize {
    cap - cap / 4
}

/// What one job may hold at once with `job_children` live children allowed:
/// one connection per child it waits on, plus 4 (as the worker's per-attempt
/// limit, `marsh_contracts::process::CONNECTION_LIMIT`, at its maximum).
#[must_use]
pub const fn per_job(job_children: usize) -> usize {
    job_children.saturating_add(4)
}

#[derive(Debug, Default)]
struct Held {
    total: usize,
    by_job: BTreeMap<String, usize>,
}

#[derive(Debug)]
pub(crate) struct Budget {
    cap: Mutex<usize>,
    held: Mutex<Held>,
}

impl Budget {
    pub(crate) fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            cap: Mutex::new(cap),
            held: Mutex::default(),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_cap(&self, cap: usize) {
        *self
            .cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = cap;
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
    ) -> Result<CapabilityConnection, String> {
        let share = capability_share(
            *self
                .cap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let limit = per_job(job_children);
        let mut held = self.held();
        let mine = held.by_job.get(job).copied().unwrap_or(0);
        if mine >= limit {
            return Err(format!(
                "{} (at most {limit} at once: job-children + 4)",
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
