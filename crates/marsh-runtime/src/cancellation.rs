//! Cooperative cancellation for owned, interruptible I/O. Cancellation never
//! acquires the writer's lock; adapters must check it between nonblocking polls.
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default)]
pub struct Cancellation(pub(crate) Arc<AtomicBool>, pub(crate) Option<Instant>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) || self.1.is_some_and(|deadline| Instant::now() >= deadline)
    }
    /// Adds a local deadline without extending the caller's deadline. Manual
    /// cancellation remains shared; expiry does not mutate the parent token.
    #[must_use]
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        Self(
            self.0.clone(),
            Some(self.1.map_or(deadline, |existing| existing.min(deadline))),
        )
    }
    /// # Errors
    /// Returns `ConnectionAborted` (not retryable `Interrupted`) after cancellation.
    pub fn check(&self) -> io::Result<()> {
        if self.1.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "owned operation deadline exceeded",
            ));
        }
        if self.0.load(Ordering::Acquire) {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "owned I/O cancelled",
            ))
        } else {
            Ok(())
        }
    }
}
