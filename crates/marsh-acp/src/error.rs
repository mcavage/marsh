//! Error types for the marsh ACP v1 client and agent registry.

use std::time::Duration;
use thiserror::Error;

/// Errors produced during ACP v1 protocol interactions.
#[derive(Debug, Error)]
pub enum AcpError {
    /// A prompt turn is already active for this session.
    #[error("session is busy: prompt turn already active")]
    Busy,

    /// A capability-gated operation was requested but not advertised by the agent.
    #[error("capability '{0}' is not supported by the agent")]
    CapabilityNotSupported(&'static str),

    /// A capability required by the registered workload was not negotiated.
    #[error("registered agent did not advertise required capability '{0}'")]
    RequiredCapabilityMissing(String),

    /// An incoming stdio line exceeded the configured maximum frame size.
    #[error("frame size of {size} bytes exceeds maximum allowed {max} bytes")]
    FrameTooLarge { size: usize, max: usize },

    /// An operation timed out.
    #[error("operation '{operation}' timed out after {elapsed:?}")]
    Timeout {
        operation: &'static str,
        elapsed: Duration,
    },

    /// The remote agent returned a JSON-RPC error response.
    #[error("remote agent error (code {code}): {message}")]
    JsonRpc {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },

    /// Protocol violation or unexpected message.
    #[error("ACP protocol error: {0}")]
    Protocol(String),

    /// Transport disconnected or subprocess exited prematurely.
    #[error("ACP transport lost: {0}")]
    TransportLost(String),

    /// The agent attempted an unauthorized host execution RPC (e.g. terminal or fs).
    #[error("unauthorized host execution request rejected: method '{method}'")]
    HostExecutionAttemptRejected { method: String },

    /// A permission request was denied or timed out.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// Cancellation requested when no prompt is currently active.
    #[error("no active prompt to cancel in session '{0}'")]
    NoActivePrompt(String),

    /// Session ID was not recognized.
    #[error("session '{0}' not found")]
    SessionNotFound(String),

    /// JSON serialization or deserialization failure.
    #[error("JSON serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// IO failure.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to agent adapter registry and command resolution.
#[derive(Debug, Error)]
pub enum AgentError {
    /// The specified agent name was not found.
    #[error("agent '{0}' not found in registry")]
    NotFound(String),

    /// A second declaration attempted to replace an existing name.
    #[error("agent '{0}' is registered more than once")]
    DuplicateName(String),

    /// Attempted to use ACP protocol with an ordinary CLI command.
    #[error("unsupported protocol for '{command}': {reason}")]
    UnsupportedProtocol { command: String, reason: String },

    /// Invalid adapter configuration.
    #[error("invalid agent adapter configuration: {0}")]
    InvalidConfiguration(String),

    /// Referenced native Kit command does not exist in command registry.
    #[error("referenced command '{0}' not registered")]
    UnregisteredCommand(String),

    /// Digest mismatch between adapter declaration and workload.
    #[error("workload digest mismatch for '{command}': expected {expected}, got {actual}")]
    DigestMismatch {
        command: String,
        expected: String,
        actual: String,
    },

    /// IO failure loading registry.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON error parsing registry.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
