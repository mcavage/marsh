//! marsh-acp: Independent ACP v1 protocol and session foundation.
//!
//! Provides:
//! - Real stdio JSON-RPC ACP v1 client (`AcpClient`)
//! - Initialization, `session/new`, sequential prompt execution, Busy error enforcement
//! - Phased cancellation: requested -> dispatched -> confirmed
//! - Bounded message frame sizes and operation timeouts
//! - Capability-gated `session/load` and `session/resume`
//! - Hard enforcement of NO host execution from agent RPCs (`terminal/*`, `fs/*`)
//! - Versioned native Kit adapter declarations and agent registry (`AgentRegistry`)
//! - Explicit rejection of fake claims that CLI tools (e.g. plain `claude`) support ACP

pub mod adapter;
pub mod cancel;
pub mod client;
pub mod error;
pub mod protocol;
pub mod transport;

pub use adapter::{AgentAdapterDeclaration, AgentProtocol, AgentRegistry};
pub use cancel::{CancelPhase, CancelTracker};
pub use client::{AcpClient, AcpClientConfig, DefaultDenyPermissionHandler, PermissionHandler};
pub use error::{AcpError, AgentError};
pub use protocol::{
    ACP_V1_PROTOCOL_VERSION, AgentCapabilities, ClientCapabilities, ContentBlock,
    ImplementationInfo, InitializeRequest, InitializeResponse, JsonRpcError, JsonRpcMessage,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PermissionOption, PromptRequest, PromptResponse,
    RequestId, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionNotification, SessionUpdate, StopReason,
    ToolCallStatus, ToolCallUpdate, ToolKind,
};
pub use transport::{StdioTransport, read_bounded_line};

#[cfg(feature = "test-support")]
pub use transport::SubprocessHandle;
