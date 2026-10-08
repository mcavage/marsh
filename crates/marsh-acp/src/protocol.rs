//! ACP v1 protocol structures and JSON-RPC 2.0 message framing.
//!
//! Based on the official Agent Client Protocol v1 specification:
//! <https://agentclientprotocol.com/protocol/v1/>.

use serde::{Deserialize, Serialize};

/// Fixed protocol version for ACP v1.
pub const ACP_V1_PROTOCOL_VERSION: u32 = 1;

/// JSON-RPC 2.0 standard error codes.
pub const JSONRPC_PARSE_ERROR: i64 = -32700;
pub const JSONRPC_INVALID_REQUEST: i64 = -32600;
pub const JSONRPC_METHOD_NOT_FOUND: i64 = -32601;
pub const JSONRPC_INVALID_PARAMS: i64 = -32602;
pub const JSONRPC_INTERNAL_ERROR: i64 = -32603;
pub const JSONRPC_REQUEST_CANCELLED: i64 = -32800;

/// ACP specific application error codes.
pub const JSONRPC_BUSY: i64 = -32001;
pub const JSONRPC_CAPABILITY_NOT_SUPPORTED: i64 = -32002;
pub const JSONRPC_PERMISSION_DENIED: i64 = -32003;

/// JSON-RPC request identifier.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Integer ID.
    Number(i64),
    /// String ID.
    String(String),
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s}"),
        }
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[must_use]
    pub fn with_data(code: i64, message: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            JSONRPC_METHOD_NOT_FOUND,
            format!("method '{method}' not found or disabled"),
        )
    }

    #[must_use]
    pub fn permission_denied(reason: &str) -> Self {
        Self::new(JSONRPC_PERMISSION_DENIED, reason)
    }

    #[must_use]
    pub fn busy() -> Self {
        Self::new(JSONRPC_BUSY, "session is busy with an active prompt")
    }

    #[must_use]
    pub fn cancelled() -> Self {
        Self::new(JSONRPC_REQUEST_CANCELLED, "request was cancelled")
    }
}

/// Inbound or outbound JSON-RPC 2.0 message.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    /// Request expecting a response.
    Request(JsonRpcRequest),
    /// Response matching a previous request.
    Response(JsonRpcResponse),
    /// Notification expecting no response.
    Notification(JsonRpcNotification),
}

impl<'de> Deserialize<'de> for JsonRpcMessage {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;

        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("JSON-RPC message must be an object"))?;
        if object.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
            return Err(D::Error::custom("JSON-RPC version must be 2.0"));
        }
        if object.contains_key("method") {
            if object.contains_key("result") || object.contains_key("error") {
                return Err(D::Error::custom(
                    "JSON-RPC request cannot contain a result or error",
                ));
            }
            if object
                .get("params")
                .is_some_and(|params| !params.is_object() && !params.is_array())
            {
                return Err(D::Error::custom(
                    "JSON-RPC params must be an object or array",
                ));
            }
            // Select by field presence so an invalid request ID cannot fall
            // through to the notification shape and silently lose its ID.
            if object.contains_key("id") {
                serde_json::from_value(value)
                    .map(Self::Request)
                    .map_err(D::Error::custom)
            } else {
                serde_json::from_value(value)
                    .map(Self::Notification)
                    .map_err(D::Error::custom)
            }
        } else {
            let has_result = object.contains_key("result");
            if has_result == object.contains_key("error") || object.contains_key("params") {
                return Err(D::Error::custom(
                    "JSON-RPC response must contain exactly one result or error",
                ));
            }
            if object.get("error").is_some_and(serde_json::Value::is_null) {
                return Err(D::Error::custom("JSON-RPC error must be an object"));
            }
            let mut response: JsonRpcResponse =
                serde_json::from_value(value).map_err(D::Error::custom)?;
            if has_result && response.result.is_none() {
                // A present null result is a legal JSON-RPC success value.
                response.result = Some(serde_json::Value::Null);
            }
            Ok(Self::Response(response))
        }
    }
}

/// A JSON-RPC 2.0 Request.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcRequest {
    #[must_use]
    pub fn new(
        id: RequestId,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 Response.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    #[must_use]
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn err(id: RequestId, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// A JSON-RPC 2.0 Notification.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl JsonRpcNotification {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<serde_json::Value>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: method.into(),
            params,
        }
    }
}

// ---------------------------------------------------------------------------
// ACP v1 Domain Types
// ---------------------------------------------------------------------------

/// Client implementation name and version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImplementationInfo {
    pub name: String,
    pub version: String,
}

/// Client capabilities declared during `initialize`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientCapabilities {
    /// Filesystem capabilities. None/false ensures no host filesystem access.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsCapabilities>,
    /// Terminal execution capability. None/false ensures no host terminal execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
}

/// Filesystem capabilities advertised to the agent.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_text_file: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_text_file: Option<bool>,
}

/// Session-level capabilities advertised by the agent.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    /// Advertises support for `session/resume`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<serde_json::Value>,
    /// Advertises support for `session/close`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close: Option<serde_json::Value>,
    /// Advertises support for `session/list`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list: Option<serde_json::Value>,
}

/// Agent capabilities returned in the `initialize` response.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    /// Advertises support for `session/load`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    /// Session capability sub-object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<SessionCapabilities>,
}

impl AgentCapabilities {
    /// Returns true if the agent advertises support for `session/load`.
    #[must_use]
    pub fn can_load_session(&self) -> bool {
        self.load_session.unwrap_or(false)
    }

    /// Returns true if the agent advertises support for `session/resume`.
    #[must_use]
    pub fn can_resume_session(&self) -> bool {
        self.session_capabilities
            .as_ref()
            .and_then(|s| s.resume.as_ref())
            .is_some()
    }
}

/// `initialize` request params (client -> agent).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeRequest {
    pub protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_capabilities: Option<ClientCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_info: Option<ImplementationInfo>,
}

/// `initialize` response result (agent -> client).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_capabilities: Option<AgentCapabilities>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<ImplementationInfo>,
}

/// `session/new` request params (client -> agent).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
}

/// `session/new` response result (agent -> client).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionResponse {
    pub session_id: String,
}

/// Content blocks for prompts and streaming chunks.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    #[serde(rename = "audio")]
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    #[serde(rename = "resource_link")]
    ResourceLink {
        name: String,
        uri: String,
        #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<i64>,
    },
    #[serde(rename = "resource")]
    Resource { resource: serde_json::Value },
}

impl ContentBlock {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    #[must_use]
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image {
            data: data.into(),
            mime_type: mime_type.into(),
            uri: None,
        }
    }

    #[must_use]
    pub fn audio(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Audio {
            data: data.into(),
            mime_type: mime_type.into(),
        }
    }

    #[must_use]
    pub fn resource_link(name: impl Into<String>, uri: impl Into<String>) -> Self {
        Self::ResourceLink {
            name: name.into(),
            uri: uri.into(),
            mime_type: None,
            description: None,
            title: None,
            size: None,
        }
    }

    #[must_use]
    pub fn resource(resource: serde_json::Value) -> Self {
        Self::Resource { resource }
    }
}

/// `session/prompt` request params (client -> agent).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

/// Stop reason for `session/prompt` completion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

/// `session/prompt` response result (agent -> client).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: StopReason,
}

/// `session/cancel` notification params (client -> agent).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelNotification {
    pub session_id: String,
}

/// `session/load` request params (client -> agent).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<serde_json::Value>,
}

/// `session/load` response result.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LoadSessionResponse {}

/// `session/resume` request params (client -> agent).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeSessionRequest {
    pub session_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Vec<serde_json::Value>>,
}

/// `session/resume` response result.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ResumeSessionResponse {}

/// `session/update` notification params (agent -> client).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
}

/// Session update payload variants.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk {
        content: ContentBlock,
    },
    AgentThoughtChunk {
        content: ContentBlock,
    },
    UserMessageChunk {
        content: ContentBlock,
    },
    ToolCall {
        #[serde(flatten)]
        update: ToolCallUpdate,
    },
    ToolCallUpdate {
        #[serde(flatten)]
        update: ToolCallUpdate,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    #[serde(other)]
    Unknown,
}

/// A single entry in the execution plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
}

impl PlanEntry {
    #[must_use]
    pub fn new(
        content: impl Into<String>,
        priority: PlanEntryPriority,
        status: PlanEntryStatus,
    ) -> Self {
        Self {
            content: content.into(),
            priority,
            status,
        }
    }
}

/// Priority levels for plan entries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

/// Execution status of a plan entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

/// Execution plan payload for `session/update`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
}

/// Tool execution status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// Tool categories.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

/// Tool call update payload.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<serde_json::Value>,
}

/// Permission request from agent to client (`session/request_permission`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionRequest {
    pub session_id: String,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
}

/// Offered option for a permission request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: String, // allow_once, allow_always, reject_once, reject_always
}

/// Outcome of a permission request response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
}

/// Decision on a permission request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RequestPermissionOutcome {
    Cancelled,
    #[serde(rename_all = "camelCase")]
    Selected {
        option_id: String,
    },
}

impl RequestPermissionOutcome {
    #[must_use]
    pub fn cancel() -> Self {
        Self::Cancelled
    }

    #[must_use]
    pub fn select(option_id: impl Into<String>) -> Self {
        Self::Selected {
            option_id: option_id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonrpc_request_response_serialization() {
        let req = JsonRpcRequest::new(
            RequestId::Number(1),
            "session/prompt",
            Some(serde_json::json!({"sessionId": "s1"})),
        );
        let serialized = serde_json::to_string(&req).unwrap();
        assert!(serialized.contains(r#""jsonrpc":"2.0""#));
        assert!(serialized.contains(r#""id":1"#));
        assert!(serialized.contains(r#""method":"session/prompt""#));

        let resp = JsonRpcResponse::ok(RequestId::Number(1), serde_json::json!({"status": "ok"}));
        let resp_ser = serde_json::to_string(&resp).unwrap();
        assert!(resp_ser.contains(r#""result":{"status":"ok"}"#));

        let err_resp = JsonRpcResponse::err(RequestId::Number(1), JsonRpcError::busy());
        let err_ser = serde_json::to_string(&err_resp).unwrap();
        assert!(err_ser.contains(r#""code":-32001"#));
    }

    #[test]
    fn test_capabilities_query() {
        let mut caps = AgentCapabilities::default();
        assert!(!caps.can_load_session());
        assert!(!caps.can_resume_session());

        caps.load_session = Some(true);
        assert!(caps.can_load_session());

        caps.session_capabilities = Some(SessionCapabilities {
            resume: Some(serde_json::json!({})),
            close: None,
            list: None,
        });
        assert!(caps.can_resume_session());
    }

    #[test]
    fn test_session_update_deserialization() {
        let json_chunk = r#"{
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {
                    "type": "text",
                    "text": "hello world"
                }
            }
        }"#;

        let notif: SessionNotification = serde_json::from_str(json_chunk).unwrap();
        assert_eq!(notif.session_id, "s1");
        match notif.update {
            SessionUpdate::AgentMessageChunk { content } => {
                assert_eq!(content, ContentBlock::text("hello world"));
            }
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn test_mcp_servers_required_even_empty() {
        let req = NewSessionRequest {
            cwd: "/tmp".into(),
            mcp_servers: vec![],
        };
        let ser = serde_json::to_string(&req).unwrap();
        assert!(
            ser.contains(r#""mcpServers":[]"#),
            "expected mcpServers to be serialized even when empty: {ser}"
        );

        let load_req = LoadSessionRequest {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            mcp_servers: vec![],
        };
        let load_ser = serde_json::to_string(&load_req).unwrap();
        assert!(
            load_ser.contains(r#""mcpServers":[]"#),
            "expected mcpServers to be serialized even when empty in load: {load_ser}"
        );
    }

    #[test]
    fn test_content_block_audio_image_resource_link_wire_fields() {
        let img = ContentBlock::image("data123", "image/png");
        let ser = serde_json::to_value(&img).unwrap();
        assert_eq!(ser["type"], "image");
        assert_eq!(ser["mimeType"], "image/png");
        assert_eq!(ser["data"], "data123");

        let audio = ContentBlock::audio("audiodata", "audio/wav");
        let ser_audio = serde_json::to_value(&audio).unwrap();
        assert_eq!(ser_audio["type"], "audio");
        assert_eq!(ser_audio["mimeType"], "audio/wav");

        let link = ContentBlock::resource_link("doc", "file:///doc.txt");
        let ser_link = serde_json::to_value(&link).unwrap();
        assert_eq!(ser_link["type"], "resource_link");
        assert_eq!(ser_link["name"], "doc");
        assert_eq!(ser_link["uri"], "file:///doc.txt");
    }

    #[test]
    fn test_plan_entries_update_wire_conformance() {
        let update = SessionUpdate::Plan {
            entries: vec![
                PlanEntry::new(
                    "Task 1",
                    PlanEntryPriority::High,
                    PlanEntryStatus::Completed,
                ),
                PlanEntry::new(
                    "Task 2",
                    PlanEntryPriority::Medium,
                    PlanEntryStatus::InProgress,
                ),
            ],
        };
        let ser = serde_json::to_value(&update).unwrap();
        assert_eq!(ser["sessionUpdate"], "plan");
        assert_eq!(ser["entries"][0]["content"], "Task 1");
        assert_eq!(ser["entries"][0]["priority"], "high");
        assert_eq!(ser["entries"][0]["status"], "completed");
        assert_eq!(ser["entries"][1]["content"], "Task 2");
        assert_eq!(ser["entries"][1]["priority"], "medium");
        assert_eq!(ser["entries"][1]["status"], "in_progress");

        let deserialized: SessionUpdate = serde_json::from_value(ser).unwrap();
        match deserialized {
            SessionUpdate::Plan { entries } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].priority, PlanEntryPriority::High);
            }
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn test_permission_outcome_wire_conformance() {
        let opt = PermissionOption {
            option_id: "allow-once".into(),
            name: "Allow this once".into(),
            kind: "allow_once".into(),
        };
        let opt_ser = serde_json::to_value(&opt).unwrap();
        assert_eq!(opt_ser["optionId"], "allow-once");

        let resp_selected = RequestPermissionResponse {
            outcome: RequestPermissionOutcome::select("allow-once"),
        };
        let sel_val = serde_json::to_value(&resp_selected).unwrap();
        assert_eq!(sel_val["outcome"]["outcome"], "selected");
        assert_eq!(sel_val["outcome"]["optionId"], "allow-once");

        let resp_cancelled = RequestPermissionResponse {
            outcome: RequestPermissionOutcome::cancel(),
        };
        let can_val = serde_json::to_value(&resp_cancelled).unwrap();
        assert_eq!(can_val["outcome"]["outcome"], "cancelled");
        assert!(can_val["outcome"].get("optionId").is_none());
    }
}
