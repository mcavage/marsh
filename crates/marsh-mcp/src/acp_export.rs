//! Export-only MCP control for one daemon-owned ACP session.

use marsh_daemon::{AcpSessionStatus, Client};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

const MAX_DECLARATION_BYTES: u64 = 16_384;
const MAX_PROMPT_BYTES: usize = 1_048_576;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcpDeclaration {
    pub schema_version: String,
    pub tool_name: String,
    pub agent_session_id: String,
    pub generation: String,
    pub canonical_workspace: PathBuf,
}

impl AcpDeclaration {
    pub const SCHEMA_VERSION: &'static str = "marsh.published_acp/v1";

    /// Validate the narrow publication fields.
    ///
    /// # Errors
    /// Returns an error for unsupported schemas, names, IDs, or workspace paths.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err("unsupported ACP publication schema".into());
        }
        marsh_daemon::PublishedName::parse(&self.tool_name)?;
        for id in [&self.agent_session_id, &self.generation] {
            if Uuid::parse_str(id).map_or(true, |parsed| parsed.to_string() != *id) {
                return Err("ACP publication IDs must be canonical UUIDs".into());
            }
        }
        if !self.canonical_workspace.is_absolute()
            || self.canonical_workspace.canonicalize().ok().as_deref()
                != Some(&self.canonical_workspace)
        {
            return Err("ACP publication workspace is not canonical".into());
        }
        Ok(())
    }

    /// Read one owner-only declaration without following a symlink.
    ///
    /// # Errors
    /// Returns an error for unsafe paths, excess bytes, or invalid content.
    pub fn load_private(path: &Path) -> Result<Self, String> {
        let parent = path.parent().ok_or("ACP publication has no directory")?;
        let directory = fs::symlink_metadata(parent).map_err(|error| error.to_string())?;
        if !directory.file_type().is_dir()
            || directory.uid() != rustix::process::geteuid().as_raw()
            || directory.mode() & 0o077 != 0
        {
            return Err("ACP publication directory is not owner-only".into());
        }
        let nofollow = i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())
            .map_err(|_| "invalid no-follow flag")?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nofollow)
            .open(path)
            .map_err(|error| error.to_string())?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.file_type().is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.len() > MAX_DECLARATION_BYTES
        {
            return Err("ACP publication must be one bounded owner-only real file".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_DECLARATION_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_DECLARATION_BYTES {
            return Err("ACP publication is too large".into());
        }
        let declaration: Self =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        declaration.validate()?;
        Ok(declaration)
    }
}

#[derive(Clone)]
pub struct AcpExportMcp {
    home: PathBuf,
    declaration_path: PathBuf,
    declaration: AcpDeclaration,
}

impl AcpExportMcp {
    /// Bind the exporter to an exact private declaration and selected home.
    ///
    /// # Errors
    /// Returns an error when the home or declaration is unsafe or changed.
    pub fn new(
        home: PathBuf,
        declaration_path: PathBuf,
        declaration: AcpDeclaration,
    ) -> Result<Self, String> {
        if !home.is_absolute() || home.canonicalize().ok().as_deref() != Some(&home) {
            return Err("ACP publication home is not canonical".into());
        }
        declaration.validate()?;
        if AcpDeclaration::load_private(&declaration_path)? != declaration {
            return Err("ACP publication changed".into());
        }
        Ok(Self {
            home,
            declaration_path,
            declaration,
        })
    }

    fn current(&self) -> bool {
        AcpDeclaration::load_private(&self.declaration_path).as_ref() == Ok(&self.declaration)
    }

    fn invoke(&self, args: &Map<String, Value>) -> Result<Value, String> {
        if !self.current() {
            return Err("ACP publication was removed or changed; reload this tool".into());
        }
        if args.keys().any(|key| {
            !matches!(
                key.as_str(),
                "action" | "text" | "key" | "cursor" | "turn_id" | "request_id" | "option_id"
            )
        }) {
            return Err("unknown ACP tool argument".into());
        }
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .ok_or("action is required")?;
        let client = Client::connect_if_running(&self.home)
            .map_err(|error| error.to_string())?
            .ok_or("ACP daemon is unavailable; the publication is no longer active")?;
        let id = self.declaration.agent_session_id.clone();
        let generation = self.declaration.generation.clone();
        match action {
            "ask" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("ask requires text")?;
                if text.is_empty() || text.len() > MAX_PROMPT_BYTES {
                    return Err("ACP prompt must contain 1 to 1048576 bytes".into());
                }
                let key = args
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or("ask requires a unique canonical UUID key for safe retries")?;
                if Uuid::parse_str(key).map_or(true, |parsed| parsed.to_string() != key) {
                    return Err("key must be a canonical UUID".into());
                }
                let turn_id = client
                    .acp_published_prompt(id.clone(), generation.clone(), key.into(), text.into())
                    .map_err(|error| error.to_string())?;
                // Fetch only metadata, never inline session history in an ask.
                // The immutable receipt is retained with the dedupe ledger, so
                // retries return the original start even after another turn.
                let status = client
                    .acp_published_status(id, generation, u64::MAX)
                    .map_err(|error| error.to_string())?;
                let receipt = status
                    .turns
                    .get(&turn_id)
                    .ok_or("Turn receipt unavailable; check status before retrying the same key")?;
                Ok(
                    json!({"turn_id": turn_id, "start_cursor": receipt.start_cursor, "next_cursor": receipt.start_cursor}),
                )
            }
            "status" => {
                let metadata = client
                    .acp_published_status(id.clone(), generation.clone(), u64::MAX)
                    .map_err(|error| error.to_string())?;
                let turn_id = args
                    .get("turn_id")
                    .map(|_| bounded_id(args, "turn_id"))
                    .transpose()?;
                let selected = turn_id
                    .or(metadata.current_turn_id.as_deref())
                    .or(metadata.last_turn_id.as_deref());
                let receipt = selected
                    .map(|id| {
                        metadata
                            .turns
                            .get(id)
                            .ok_or("Unknown turn_id; use the ID returned by ask")
                    })
                    .transpose()?;
                let cursor = args
                    .get("cursor")
                    .map_or(Some(receipt.map_or(0, |r| r.start_cursor)), Value::as_u64)
                    .ok_or("cursor must be a nonnegative integer")?;
                let status = client
                    .acp_published_status(id, generation, cursor)
                    .map_err(|error| error.to_string())?;
                public_status(&status, selected, cursor)
            }
            "cancel" => {
                let phase = client
                    .acp_published_cancel(id, generation)
                    .map_err(|error| error.to_string())?;
                Ok(json!({"phase": phase}))
            }
            "respond" => {
                let request_id = bounded_id(args, "request_id")?;
                let option_id = bounded_id(args, "option_id")?;
                client
                    .acp_published_respond(id, generation, request_id.into(), option_id.into())
                    .map_err(|error| error.to_string())?;
                Ok(
                    json!({"accepted": true, "phase": "queued_for_agent", "note": "The daemon accepted this choice; cancellation or transport loss may prevent delivery to the agent. Poll this turn's status."}),
                )
            }
            _ => Err("action must be ask, status, cancel, or respond".into()),
        }
    }
}

fn bounded_id<'a>(args: &'a Map<String, Value>, name: &str) -> Result<&'a str, String> {
    let value = args
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} is required"))?;
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(format!("invalid {name}"));
    }
    Ok(value)
}

fn public_status(
    status: &AcpSessionStatus,
    selected: Option<&str>,
    cursor: u64,
) -> Result<Value, String> {
    let receipt = selected
        .map(|id| status.turns.get(id).ok_or("Turn receipt unavailable"))
        .transpose()?;
    let active = selected.is_some() && selected == status.current_turn_id.as_deref();
    let end = receipt
        .and_then(|r| r.end_cursor)
        .unwrap_or(status.latest_cursor);
    let start = receipt.map_or(0, |r| r.start_cursor);
    let retained_after = receipt.map_or(0, |r| r.retained_after);
    if cursor < start || cursor > end {
        return Err(format!(
            "Cursor is outside this turn; resume at start_cursor {start} (latest {end})"
        ));
    }
    let updates: Vec<_> = status
        .updates
        .iter()
        .filter(|u| u.turn_id.as_deref() == selected)
        .collect();
    let next = updates
        .last()
        .map_or(cursor.max(retained_after.min(end)), |u| u.cursor);
    let dropped = if active {
        status.dropped_updates
    } else {
        receipt.map_or(0, |r| r.dropped_updates)
    };
    Ok(json!({
        "turn_id": selected,
        "current_turn_id": status.current_turn_id,
        "last_turn_id": status.last_turn_id,
        "start_cursor": start,
        "turn_active": active,
        "last_stop_reason": receipt.and_then(|r| r.stop_reason),
        "error": receipt.and_then(|r| r.error.as_deref()),
        "turn_error": receipt.is_some_and(|r| r.error.is_some()),
        "updates": updates,
        "next_cursor": next,
        "latest_cursor": end,
        "more_updates": next < end,
        "updates_lost": dropped > 0 || cursor < retained_after.min(end),
        "retained_after": retained_after,
        "dropped_updates": dropped,
        "session_out_of_turn_updates": status.out_of_turn_updates,
        "status_omissions": status.status_omissions,
        "permission_note": if active { status.permission_note.as_deref() } else { receipt.and_then(|r| r.permission_note.as_deref()) },
        "permissions": if active { status.permissions.clone() } else { Vec::new() },
        "cancel_requested": active && status.cancel_requested,
        "stopping": status.stopping,
        "terminal": status.attachment.terminal.is_some(),
    }))
}

impl ServerHandler for AcpExportMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("marsh-acp-export", env!("CARGO_PKG_VERSION")))
            .with_instructions("This tool controls one published ACP agent session. `ask` returns turn_id and start_cursor without history. Call `status` with that turn_id and cursor=start_cursor, then pass back each next_cursor unchanged (exclusive cursor; idle polls do not advance it). Continue until turn_active is false AND more_updates is false. Updates and errors belong to the selected turn. Example ask: {\"action\":\"ask\",\"text\":\"Say hi\",\"key\":\"38f58490-9e41-431c-bc09-639ab80dd4cf\"}. Use a new UUID for each new turn and reuse the same key for a retry. `respond` can select only an offered one-time allow/reject permission choice. Its accepted reply means queued at the daemon, not applied by the agent; cancellation or transport loss can still win. Poll the selected turn for the observed outcome.")
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        if !self.current() {
            return Ok(ListToolsResult::with_all_items(Vec::new()));
        }
        let schema = json!({
            "type":"object",
            "properties": {
                "action":{"type":"string","enum":["ask","status","cancel","respond"]},
                "text":{"type":"string","description":"Prompt for asynchronous ask: text and encoded ACP/control frames each max 1 MiB including escaping/session envelopes; poll status afterward"},
                "key":{"type":"string","description":"Unique canonical UUID for this ask; reuse on retry"},
                "turn_id":{"type":"string","description":"Turn ID returned by ask; selects that turn even after another caller starts one"},
                "cursor":{"type":"integer","minimum":0,"description":"Exclusive cursor: pass start_cursor from ask, then next_cursor unchanged"},
                "request_id":{"type":"string","description":"Pending permission request ID for respond"},
                "option_id":{"type":"string","description":"Offered allow_once or reject_once option ID"}
            },
            "required":["action"],
            "additionalProperties":false
        });
        let Value::Object(schema) = schema else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            self.declaration.tool_name.clone(),
            "Control this published ACP agent: ask, status, cancel, or respond to an offered one-time permission request".to_owned(),
            Arc::new(schema),
        )]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name != self.declaration.tool_name {
            return Ok(tool_error("unknown ACP tool"));
        }
        let args = request.arguments.unwrap_or_default();
        let server = self.clone();
        let result = tokio::task::spawn_blocking(move || server.invoke(&args))
            .await
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(match result {
            Ok(value) => CallToolResult::structured(json!({"ok":true,"result":value})).into(),
            Err(error) => tool_error(&error),
        })
    }
}

fn tool_error(error: &str) -> CallToolResponse {
    // Never expose arbitrary daemon/IO diagnostics or agent-controlled content.
    let safe = if error.contains("frame is too large") || error.contains("encoded ACP prompt") {
        "Encoded prompt plus control/session envelope exceeds 1 MiB; shorten the text or split it across turns. This prompt was not admitted."
    } else if error.contains("key is bound") {
        "Prompt key belongs to another text/controller; retry identical text with its original key, or use a new UUID."
    } else if error.contains("ledger is full") {
        "Prompt-key capacity reached; ask the publisher to start a new session."
    } else if error.contains("already active") {
        "A turn is active; poll status or cancel it before asking again."
    } else if error.contains("revoked") || error.contains("changed") || error.contains("removed") {
        "Publication was revoked or changed; ask the publisher to republish and reload this tool."
    } else if error.contains("offered one-time") {
        "Select an offered allow_once or reject_once option from the current status."
    } else if [
        "ask requires",
        "ACP prompt must",
        "key must",
        "cursor must",
        "action",
        "unknown ACP",
        "invalid request_id",
        "invalid option_id",
        "request_id is",
        "option_id is",
        "turn_id is",
        "invalid turn_id",
        "Unknown turn_id",
        "Cursor is outside",
        "Turn receipt",
        "ACP daemon is",
    ]
    .iter()
    .any(|prefix| error.starts_with(prefix))
    {
        error
    } else {
        "ACP operation failed; check status and the publication's controller. If the daemon/session ended, ask the publisher to start and publish a new session."
    };
    let safe: String = safe.chars().filter(|c| !c.is_control()).take(512).collect();
    let mut result = CallToolResult::structured(json!({"ok":false,"error":safe}));
    result.is_error = Some(true);
    result.into()
}
