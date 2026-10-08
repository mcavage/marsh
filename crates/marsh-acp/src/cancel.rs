//! ACP prompt turn cancellation tracker.
//!
//! Enforces PRD ACP-05 requirement:
//! Cancel requested, cancel dispatched, and cancel confirmed are distinct states.

use std::sync::atomic::{AtomicU8, Ordering};

const PHASE_NOT_CANCELLED: u8 = 0;
const PHASE_REQUESTED: u8 = 1;
const PHASE_DISPATCHED: u8 = 2;
const PHASE_CONFIRMED: u8 = 3;

/// Distinct cancellation phases during a prompt turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelPhase {
    /// Turn is running normally without cancellation.
    NotCancelled,
    /// Client or shell controller has requested cancellation.
    Requested,
    /// Cancellation notification (`session/cancel`) was dispatched over the transport.
    Dispatched,
    /// Agent confirmed cancellation via `StopReason::Cancelled` after cancel was dispatched.
    Confirmed,
}

impl std::fmt::Display for CancelPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotCancelled => write!(f, "not_cancelled"),
            Self::Requested => write!(f, "requested"),
            Self::Dispatched => write!(f, "dispatched"),
            Self::Confirmed => write!(f, "confirmed"),
        }
    }
}

/// Thread-safe tracker for prompt cancellation lifecycle.
#[derive(Debug)]
pub struct CancelTracker {
    phase: AtomicU8,
}

impl Default for CancelTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelTracker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(PHASE_NOT_CANCELLED),
        }
    }

    /// Current cancellation phase.
    #[must_use]
    pub fn phase(&self) -> CancelPhase {
        match self.phase.load(Ordering::Acquire) {
            PHASE_REQUESTED => CancelPhase::Requested,
            PHASE_DISPATCHED => CancelPhase::Dispatched,
            PHASE_CONFIRMED => CancelPhase::Confirmed,
            _ => CancelPhase::NotCancelled,
        }
    }

    /// Returns true if cancellation has been requested, dispatched, or confirmed.
    #[must_use]
    pub fn is_requested(&self) -> bool {
        self.phase.load(Ordering::Acquire) >= PHASE_REQUESTED
    }

    /// Returns true if cancellation notification has been dispatched across stdio.
    #[must_use]
    pub fn is_dispatched(&self) -> bool {
        self.phase.load(Ordering::Acquire) >= PHASE_DISPATCHED
    }

    /// Returns true if agent has confirmed cancellation.
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        self.phase.load(Ordering::Acquire) == PHASE_CONFIRMED
    }

    /// Transitions to `Requested` if currently `NotCancelled`.
    /// Returns true if transitioned.
    pub fn mark_requested(&self) -> bool {
        self.phase
            .compare_exchange(
                PHASE_NOT_CANCELLED,
                PHASE_REQUESTED,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Transitions to `Dispatched` if currently `Requested`.
    /// Returns true if transitioned.
    pub fn mark_dispatched(&self) -> bool {
        self.phase
            .compare_exchange(
                PHASE_REQUESTED,
                PHASE_DISPATCHED,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Transitions to `Confirmed` if currently `Dispatched`.
    /// Returns true if transitioned.
    pub fn mark_confirmed(&self) -> bool {
        self.phase
            .compare_exchange(
                PHASE_DISPATCHED,
                PHASE_CONFIRMED,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Resets tracker for a new turn.
    pub fn reset(&self) {
        self.phase.store(PHASE_NOT_CANCELLED, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cancel_lifecycle() {
        let tracker = CancelTracker::new();
        assert_eq!(tracker.phase(), CancelPhase::NotCancelled);
        assert!(!tracker.is_requested());
        assert!(!tracker.is_dispatched());
        assert!(!tracker.is_confirmed());

        assert!(tracker.mark_requested());
        assert_eq!(tracker.phase(), CancelPhase::Requested);
        assert!(tracker.is_requested());
        assert!(!tracker.is_dispatched());

        // Cannot transition from NotCancelled again
        assert!(!tracker.mark_requested());

        assert!(tracker.mark_dispatched());
        assert_eq!(tracker.phase(), CancelPhase::Dispatched);
        assert!(tracker.is_dispatched());
        assert!(!tracker.is_confirmed());

        assert!(tracker.mark_confirmed());
        assert_eq!(tracker.phase(), CancelPhase::Confirmed);
        assert!(tracker.is_confirmed());

        tracker.reset();
        assert_eq!(tracker.phase(), CancelPhase::NotCancelled);
    }

    #[test]
    fn test_unsolicited_cancelled_cannot_confirm() {
        let tracker = CancelTracker::new();
        assert_eq!(tracker.phase(), CancelPhase::NotCancelled);

        // Unsolicited: agent sends cancelled before cancel was requested or dispatched
        assert!(!tracker.mark_confirmed());
        assert_eq!(tracker.phase(), CancelPhase::NotCancelled);
        assert!(!tracker.is_confirmed());

        // Even if cancel was requested, without dispatch it cannot confirm
        assert!(tracker.mark_requested());
        assert!(!tracker.mark_confirmed());
        assert_eq!(tracker.phase(), CancelPhase::Requested);
        assert!(!tracker.is_confirmed());
    }
}
