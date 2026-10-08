//! Observed workload execution, independent of delivery, data finalization and cleanup.
//!
//! Keep these wire forms identical on the worker transport and in LOCAL/Cloud
//! receipts. Legacy receipts without an observation default to `Unknown`, never
//! to an exit reconstructed from a diagnostic or a public shell status.

use serde::{Deserialize, Serialize};

/// Execution status, independent from container cleanup.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExecutionOutcome {
    /// The workload has not been submitted, or was conclusively rejected before start.
    NotStarted,
    /// Execution may have occurred, but no authoritative terminal observation exists.
    #[default]
    Unknown,
    Exited {
        code: i32,
    },
    LimitExceeded {
        resource: ResourceLimit,
    },
    SetupFailed {
        stage: SetupStage,
    },
    SupervisionFailed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLimit {
    Memory,
    Pids,
    Output,
    Writable,
    Wall,
}

/// Worker container setup stages. Cloud provisioning, capture and publication
/// failures are separate facts, not container setup stages.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupStage {
    Validate,
    Grant,
    Create,
    Attach,
    Start,
}

/// Maximum concurrent containers on one retained worker transport.
///
/// Host admission and worker enforcement use the same bound. This is independent
/// of the shell's fanout branch count. Eight also leaves descriptor headroom on
/// hosts with a 256-descriptor soft limit for relay, worker and control pipes.
pub const WORKER_CONTAINER_CAPACITY: u16 = 8;
