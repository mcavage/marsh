//! Export-only MCP server mode for one owner-declared Kit command or Brush pipeline.
//!
//! Exposes exactly one typed tool defined by a versioned declaration file.
//! Arguments are validated against the declared JSON Schema. Kit command arguments
//! bind to literal argv/options; pipeline input binds only to bounded stdin. A
//! pipeline runs through the marsh project shell and Brush, not host Bash.

use crate::HostConfig;
use base64::Engine as _;
use marsh_daemon::{
    AttachmentFrame, Client, DaemonError, ExecuteSpec, PublicReply, PublicRequest,
    SessionAuthority, SessionSpec,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex as TokioMutex;
use tokio::{io::AsyncWriteExt, process::Command};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 262_144; // 256 KiB
pub const MAX_OUTPUT_BYTES_LIMIT: usize = 262_144; // 256 KiB (fits within 1 MiB MCP frame with stdout+stderr+metadata)
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000; // 120s
pub const MAX_TIMEOUT_MS: u64 = 900_000; // 15 min
pub const DEFAULT_MAX_STDIN_BYTES: usize = 65_536; // 64 KiB
pub const MAX_STDIN_BYTES_LIMIT: usize = 1_048_576; // 1 MiB
pub const MAX_ARG_STRING_BYTES: usize = 65_536; // 64 KiB
pub const SUPPORTED_SCHEMA_VERSION: &str = "marsh.published_tool/v1";

fn default_max_output_bytes() -> usize {
    DEFAULT_MAX_OUTPUT_BYTES
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn default_max_stdin_bytes() -> usize {
    DEFAULT_MAX_STDIN_BYTES
}

/// A published tool declaration describing schema, bindings, and command.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolDeclaration {
    pub schema_version: String,
    pub tool_name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_generation: Option<String>,
    #[serde(default)]
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    pub input_schema: Value,
    pub bindings: ToolBindings,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub kit_identity: Option<String>,
    #[serde(default)]
    pub canonical_workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_identity: Option<WorkspaceIdentity>,
}

/// Filesystem identity of the project that published a tool.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceIdentity {
    pub device: u64,
    pub inode: u64,
}

impl WorkspaceIdentity {
    /// Read the identity without following a replacement symlink.
    ///
    /// # Errors
    /// Returns an error if the path is missing, is a symlink, or is not a directory.
    pub fn for_directory(path: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect published workspace: {error}"))?;
        if !metadata.file_type().is_dir() {
            return Err("published workspace is no longer a real directory".into());
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

/// Bindings from schema properties to argv arguments and standard input.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolBindings {
    #[serde(default)]
    pub argv: Vec<ArgBinding>,
    #[serde(default)]
    pub stdin: Option<StdinBinding>,
}

/// Binding for an argv element.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ArgBinding {
    Literal { value: String },
    Positional { field: String },
    NamedOption { option: String, field: String },
    Flag { option: String, field: String },
}

/// Binding for standard input.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StdinBinding {
    pub field: String,
    #[serde(default = "default_max_stdin_bytes")]
    pub max_bytes: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum DeclarationFileFormat {
    Single(Box<ToolDeclaration>),
    Envelope {
        #[serde(default)]
        _version: Option<String>,
        tools: Vec<ToolDeclaration>,
    },
}

impl ToolDeclaration {
    /// Load and validate a declaration from a file path.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read, JSON is invalid, or the declaration
    /// fails validation rules.
    pub fn load_from_path(path: &Path, selected_tool: Option<&str>) -> Result<Self, String> {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("cannot read declaration file {}: {e}", path.display()))?;
        Self::load_from_str(&content, selected_tool)
    }

    /// Load and validate a declaration from a JSON string.
    ///
    /// # Errors
    /// Returns an error if JSON is invalid or fails validation rules.
    pub fn load_from_str(content: &str, selected_tool: Option<&str>) -> Result<Self, String> {
        let parsed: DeclarationFileFormat = serde_json::from_str(content)
            .map_err(|e| format!("invalid tool declaration JSON: {e}"))?;
        let declaration = match parsed {
            DeclarationFileFormat::Single(decl) => {
                if let Some(tool) = selected_tool
                    && decl.tool_name != tool
                {
                    return Err(format!(
                        "declaration specifies tool '{}', but requested '{}'",
                        decl.tool_name, tool
                    ));
                }
                *decl
            }
            DeclarationFileFormat::Envelope { tools, .. } => {
                if tools.is_empty() {
                    return Err("declaration file contains no tools".to_string());
                }
                if let Some(tool) = selected_tool {
                    tools
                        .into_iter()
                        .find(|t| t.tool_name == tool)
                        .ok_or_else(|| format!("tool '{tool}' not found in declaration file"))?
                } else if tools.len() == 1 {
                    tools
                        .into_iter()
                        .next()
                        .ok_or_else(|| "declaration file contains no tools".to_string())?
                } else {
                    return Err(format!(
                        "declaration file contains multiple tools (count={}); specify tool name explicitly",
                        tools.len()
                    ));
                }
            }
        };
        declaration.validate()?;
        Ok(declaration)
    }

    /// Validate the declaration's schema, names, bounds, and binding consistency.
    ///
    /// # Errors
    /// Returns an error if any schema or binding rule is violated.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SUPPORTED_SCHEMA_VERSION && self.schema_version != "1.0" {
            return Err(format!(
                "unsupported schema_version '{}'; expected '{SUPPORTED_SCHEMA_VERSION}'",
                self.schema_version
            ));
        }
        marsh_daemon::PublishedName::parse(&self.tool_name)?;
        if let Some(generation) = &self.publication_generation
            && Uuid::parse_str(generation).map_or(true, |parsed| parsed.to_string() != *generation)
        {
            return Err("publication_generation must be a canonical UUID".into());
        }
        match &self.pipeline {
            Some(_) if !self.command.is_empty() => {
                return Err("declaration must select either command or pipeline".into());
            }
            Some(_) if self.kit_identity.is_some() || !self.bindings.argv.is_empty() => {
                return Err("pipeline declarations cannot bind Kit identity or argv".into());
            }
            Some(pipeline) => marsh_daemon::validate_publication_pipeline(pipeline)?,
            None => validate_command_name(&self.command)?,
        }

        if self.description.is_empty() {
            return Err("tool description cannot be empty".to_string());
        }

        if self.max_output_bytes == 0 || self.max_output_bytes > MAX_OUTPUT_BYTES_LIMIT {
            return Err(format!(
                "max_output_bytes ({}) must be between 1 and {MAX_OUTPUT_BYTES_LIMIT}",
                self.max_output_bytes
            ));
        }

        if self.timeout_ms == 0 || self.timeout_ms > MAX_TIMEOUT_MS {
            return Err(format!(
                "timeout_ms ({}) must be between 1 and {MAX_TIMEOUT_MS}",
                self.timeout_ms
            ));
        }

        if let Some(kit_id) = &self.kit_identity
            && kit_id.trim().is_empty()
        {
            return Err("kit_identity cannot be empty".to_string());
        }

        // Validate input_schema structure
        let schema_obj = self
            .input_schema
            .as_object()
            .ok_or_else(|| "input_schema must be a JSON object".to_string())?;

        // Strictly reject unsupported top-level JSON Schema keywords
        for key in schema_obj.keys() {
            if !matches!(
                key.as_str(),
                "type"
                    | "properties"
                    | "required"
                    | "additionalProperties"
                    | "$schema"
                    | "description"
                    | "title"
            ) {
                return Err(format!(
                    "unsupported JSON Schema keyword '{key}' in input_schema; only 'type', 'properties', 'required', 'additionalProperties', and description are supported"
                ));
            }
        }

        let schema_type = schema_obj.get("type").and_then(Value::as_str);
        if schema_type != Some("object") {
            return Err("input_schema 'type' must be 'object'".to_string());
        }

        if let Some(add_props) = schema_obj.get("additionalProperties")
            && add_props != &Value::Bool(false)
        {
            return Err(
                "input_schema 'additionalProperties' must be false (unknown properties are strictly rejected)"
                    .to_string(),
            );
        }

        let props_map = schema_obj
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| "input_schema 'properties' must be a JSON object".to_string())?;

        // Strictly validate each property schema and reject unsupported keywords
        for (prop_name, prop_val) in props_map {
            validate_identifier(prop_name, 64, "property name")?;
            let prop_obj = prop_val
                .as_object()
                .ok_or_else(|| format!("schema for property '{prop_name}' must be an object"))?;

            for key in prop_obj.keys() {
                if !matches!(key.as_str(), "type" | "description" | "title") {
                    return Err(format!(
                        "unsupported JSON Schema keyword '{key}' in property '{prop_name}'; export bindings support only 'type' and 'description'"
                    ));
                }
            }

            let prop_type = prop_obj.get("type").and_then(Value::as_str);
            if !matches!(prop_type, Some("string" | "boolean" | "integer" | "number")) {
                return Err(format!(
                    "unsupported or missing type for property '{prop_name}'; export bindings support only 'string', 'boolean', 'integer', and 'number'"
                ));
            }
        }

        // Validate required fields
        if let Some(required) = schema_obj.get("required") {
            let req_array = required
                .as_array()
                .ok_or_else(|| "input_schema 'required' must be an array".to_string())?;
            for req_val in req_array {
                let req_name = req_val.as_str().ok_or_else(|| {
                    "input_schema 'required' elements must be strings".to_string()
                })?;
                if !props_map.contains_key(req_name) {
                    return Err(format!(
                        "required field '{req_name}' is not declared in input_schema properties"
                    ));
                }
            }
        }

        let declared_properties: HashSet<String> = props_map.keys().cloned().collect();

        // Validate bindings against declared properties
        for (i, binding) in self.bindings.argv.iter().enumerate() {
            match binding {
                ArgBinding::Literal { value } => {
                    if value.contains('\0') {
                        return Err(format!(
                            "literal binding at index {i} contains NUL character"
                        ));
                    }
                }
                ArgBinding::Positional { field } => {
                    if !declared_properties.contains(field) {
                        return Err(format!(
                            "positional binding references field '{field}' which is not declared in input_schema properties"
                        ));
                    }
                }
                ArgBinding::NamedOption { option, field } => {
                    validate_option_syntax(option, &format!("argv binding {i}"))?;
                    if !declared_properties.contains(field) {
                        return Err(format!(
                            "named_option binding references field '{field}' which is not declared in input_schema properties"
                        ));
                    }
                }
                ArgBinding::Flag { option, field } => {
                    validate_option_syntax(option, &format!("argv flag binding {i}"))?;
                    if !declared_properties.contains(field) {
                        return Err(format!(
                            "flag binding references field '{field}' which is not declared in input_schema properties"
                        ));
                    }
                }
            }
        }

        if let Some(stdin) = &self.bindings.stdin {
            if !declared_properties.contains(&stdin.field) {
                return Err(format!(
                    "stdin binding references field '{}' which is not declared in input_schema properties",
                    stdin.field
                ));
            }
            if stdin.max_bytes == 0 || stdin.max_bytes > MAX_STDIN_BYTES_LIMIT {
                return Err(format!(
                    "stdin max_bytes ({}) must be between 1 and {MAX_STDIN_BYTES_LIMIT}",
                    stdin.max_bytes
                ));
            }
        }

        Ok(())
    }
}

fn validate_identifier(name: &str, max_len: usize, label: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > max_len {
        return Err(format!("{label} length must be between 1 and {max_len}"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "{label} '{name}' contains invalid characters; must be alphanumeric, '_', or '-'"
        ));
    }
    Ok(())
}

fn validate_command_name(name: &str) -> Result<(), String> {
    marsh_daemon::KitCommandName::parse(name)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn validate_option_syntax(opt: &str, label: &str) -> Result<(), String> {
    if !opt.starts_with('-') || opt == "-" || opt == "--" {
        return Err(format!(
            "{label} option '{opt}' must start with '-' or '--' followed by option name"
        ));
    }
    let after_dashes = opt.trim_start_matches('-');
    if !after_dashes
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(format!(
            "{label} option '{opt}' contains invalid characters"
        ));
    }
    Ok(())
}

fn kit_profile_matches(actual: &str, expected: &str) -> bool {
    if actual == expected {
        return true;
    }
    if let Some(suffix) = actual.strip_prefix(expected)
        && let Some(digest) = suffix.strip_prefix("@sha256:")
    {
        return !digest.is_empty() && digest.bytes().all(|b| b.is_ascii_hexdigit());
    }
    false
}

/// Validated configuration for export-only MCP server.
#[derive(Clone, Debug)]
pub struct ExportConfig {
    pub host_config: HostConfig,
    pub declaration: ToolDeclaration,
    pub declaration_path: Option<PathBuf>,
}

impl ExportConfig {
    /// Build and validate an `ExportConfig`.
    ///
    /// # Errors
    /// Returns an error if workspace, home, executables, or declaration fail validation.
    pub fn new(host_config: HostConfig, declaration: ToolDeclaration) -> Result<Self, String> {
        declaration.validate()?;
        if declaration.pipeline.is_some() && declaration.publication_generation.is_some() {
            let expected = declaration
                .workspace_identity
                .ok_or("published pipeline has no project identity; publish it again")?;
            if WorkspaceIdentity::for_directory(host_config.workspace())? != expected {
                return Err("published pipeline project identity changed".into());
            }
        }
        if let Some(pinned_ws) = &declaration.canonical_workspace {
            let canonical_pinned = pinned_ws.canonicalize().map_err(|e| {
                format!(
                    "cannot canonicalize pinned workspace {}: {e}",
                    pinned_ws.display()
                )
            })?;
            if canonical_pinned != host_config.workspace() {
                return Err(format!(
                    "declaration is pinned to workspace {}, but server was started with workspace {}",
                    canonical_pinned.display(),
                    host_config.workspace().display()
                ));
            }
        }
        // The selected home is guest writable. Resolve registration only through
        // the daemon's host-owned registry immediately before each invocation.
        Ok(Self {
            host_config,
            declaration,
            declaration_path: None,
        })
    }

    #[must_use]
    pub fn with_declaration_path(mut self, path: PathBuf) -> Self {
        self.declaration_path = Some(path);
        self
    }
}

/// Execution outcome visible to caller.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Success,
    ExitNonzero,
    Timeout,
    Cancelled,
    Failed,
}

/// Structured response data returned by calling the exported tool.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExportExecutionData {
    pub tool_name: String,
    pub command: String,
    pub outcome: ExecutionOutcome,
    /// Exact observed worker outcome from the bound receipt. Absent for a
    /// pipeline or an unavailable receipt; never reconstructed from exit prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<marsh_daemon::ExecutionOutcome>,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_base64: String,
    pub stderr_base64: String,
    /// State of the optional UTF-8 text view; base64 is the authoritative byte stream.
    pub stdout_text_state: String,
    pub stderr_text_state: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub cleanup_certainty: String,
    /// Host marsh process-group cleanup; pipeline sandbox cleanup remains separate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_cleanup_certainty: Option<String>,
    pub output_complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
}

/// Export-only MCP `ServerHandler`.
#[derive(Clone)]
pub struct ExportMcp {
    config: Arc<ExportConfig>,
    in_flight: Arc<TokioMutex<HashMap<u64, CancellationToken>>>,
    next_exec_id: Arc<AtomicU64>,
}

impl ExportMcp {
    #[must_use]
    pub fn new(config: ExportConfig) -> Self {
        Self {
            config: Arc::new(config),
            in_flight: Arc::new(TokioMutex::new(HashMap::new())),
            next_exec_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Request cancellation of all in-flight executions and wait boundedly for them to drain.
    ///
    /// # Errors
    /// Returns an error if executions do not terminate within the bounded shutdown interval.
    pub async fn shutdown_server(&self) -> Result<(), String> {
        let tokens: Vec<_> = {
            let map = self.in_flight.lock().await;
            map.values().cloned().collect()
        };
        for token in tokens {
            token.cancel();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(16);
        loop {
            let count = self.in_flight.lock().await.len();
            if count == 0 {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("timed out waiting for in-flight executions to finish".to_string());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[must_use]
    pub fn declaration(&self) -> &ToolDeclaration {
        &self.config.declaration
    }

    fn publication_current(&self) -> bool {
        self.config.declaration_path.as_ref().is_none_or(|path| {
            ToolDeclaration::load_from_path(path, Some(&self.config.declaration.tool_name)).as_ref()
                == Ok(&self.config.declaration)
        })
    }

    /// Validate caller inputs against schema and bindings, preventing unknown fields
    /// and option injection, and produce exact argv and stdin byte vectors.
    ///
    /// # Errors
    /// Returns an error if arguments violate schema, contain unknown fields, or attempt option injection.
    #[allow(clippy::too_many_lines, clippy::similar_names)]
    pub fn prepare_arguments(
        &self,
        arguments_value: &Value,
    ) -> Result<(Vec<Vec<u8>>, Vec<u8>), String> {
        let empty_map = Map::new();
        let args = match arguments_value {
            Value::Null => &empty_map,
            Value::Object(map) => map,
            _ => return Err("tool arguments must be a JSON object".to_string()),
        };

        let schema_obj = self
            .config
            .declaration
            .input_schema
            .as_object()
            .ok_or_else(|| "declaration schema must be an object".to_string())?;

        let properties = schema_obj
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        // MCP-01: Caller-supplied unknown fields are absent/rejected before job creation
        for key in args.keys() {
            if !properties.contains_key(key) {
                return Err(format!(
                    "unknown field '{key}' is not permitted by tool schema"
                ));
            }
        }

        // Required fields check
        if let Some(required) = schema_obj.get("required").and_then(Value::as_array) {
            for req in required {
                if let Some(req_name) = req.as_str()
                    && !args.contains_key(req_name)
                {
                    return Err(format!("missing required field '{req_name}'"));
                }
            }
        }

        // Validate types and length bounds
        for (key, val) in args {
            let Some(prop_schema) = properties.get(key) else {
                continue;
            };
            let expected_type = prop_schema.get("type").and_then(Value::as_str);
            match expected_type {
                Some("string") => {
                    let Some(s) = val.as_str() else {
                        return Err(format!("field '{key}' must be a string"));
                    };
                    if s.contains('\0') {
                        return Err(format!("field '{key}' cannot contain NUL bytes"));
                    }
                    if s.len() > MAX_ARG_STRING_BYTES {
                        return Err(format!(
                            "field '{key}' length ({}) exceeds maximum limit of {MAX_ARG_STRING_BYTES} bytes",
                            s.len()
                        ));
                    }
                }
                Some("boolean") if !val.is_boolean() => {
                    return Err(format!("field '{key}' must be a boolean"));
                }
                Some("integer") if !val.is_i64() && !val.is_u64() => {
                    return Err(format!("field '{key}' must be an integer"));
                }
                Some("number") if !val.is_number() => {
                    return Err(format!("field '{key}' must be a number"));
                }
                _ => {}
            }
        }

        // MCP-02: Option injection prevention
        // Validate that positional or option argument values do not start with '-'
        for binding in &self.config.declaration.bindings.argv {
            match binding {
                ArgBinding::Positional { field } => {
                    if let Some(val) = args.get(field)
                        && let Some(s) = val.as_str()
                        && s.starts_with('-')
                    {
                        return Err(format!(
                            "positional argument for field '{field}' cannot start with '-' (option injection prevented)"
                        ));
                    }
                }
                ArgBinding::NamedOption { option, field } => {
                    if let Some(val) = args.get(field)
                        && let Some(s) = val.as_str()
                        && s.starts_with('-')
                    {
                        return Err(format!(
                            "value for option '{option}' (field '{field}') cannot start with '-' (option injection prevented)"
                        ));
                    }
                }
                ArgBinding::Literal { .. } | ArgBinding::Flag { .. } => {}
            }
        }

        // Build exact argv
        let mut arg_vectors: Vec<Vec<u8>> = Vec::new();
        for binding in &self.config.declaration.bindings.argv {
            match binding {
                ArgBinding::Literal { value } => {
                    arg_vectors.push(value.as_bytes().to_vec());
                }
                ArgBinding::Positional { field } => {
                    if let Some(val) = args.get(field) {
                        arg_vectors.push(format_arg_value(val));
                    }
                }
                ArgBinding::NamedOption { option, field } => {
                    if let Some(val) = args.get(field) {
                        arg_vectors.push(option.as_bytes().to_vec());
                        arg_vectors.push(format_arg_value(val));
                    }
                }
                ArgBinding::Flag { option, field } => {
                    if let Some(val) = args.get(field)
                        && val.as_bool() == Some(true)
                    {
                        arg_vectors.push(option.as_bytes().to_vec());
                    }
                }
            }
        }

        // Build exact stdin
        let mut stdin_bytes: Vec<u8> = Vec::new();
        if let Some(stdin_binding) = &self.config.declaration.bindings.stdin
            && let Some(val) = args.get(&stdin_binding.field)
        {
            let bytes = match val {
                Value::String(s) => s.as_bytes().to_vec(),
                other => other.to_string().into_bytes(),
            };
            if bytes.len() > stdin_binding.max_bytes {
                return Err(format!(
                    "stdin field '{}' size ({}) exceeds configured limit of {} bytes",
                    stdin_binding.field,
                    bytes.len(),
                    stdin_binding.max_bytes
                ));
            }
            stdin_bytes = bytes;
        }

        Ok((arg_vectors, stdin_bytes))
    }

    #[allow(clippy::too_many_lines)]
    async fn execute_pipeline(
        &self,
        stdin: Vec<u8>,
        context: &RequestContext<RoleServer>,
        exec_cancel: CancellationToken,
    ) -> Result<ExportExecutionData, String> {
        self.config.host_config.validate_host_executables()?;
        let declaration = &self.config.declaration;
        if let Some(expected) = declaration.workspace_identity {
            let actual = WorkspaceIdentity::for_directory(self.config.host_config.workspace())?;
            if actual != expected {
                return Err("published pipeline project identity changed".into());
            }
        }
        let pipeline = declaration.pipeline.as_ref().ok_or("missing pipeline")?;
        let config = &self.config.host_config;
        let mut process = Command::new(config.marsh());
        process
            .arg("-c")
            .arg(pipeline)
            .current_dir(config.workspace())
            .env_clear()
            .env("MARSH_HOME", config.home())
            .env("MARSH_SBX", config.sbx())
            .env("USER", config.username())
            .env("LOGNAME", config.username())
            .env("PATH", crate::fixed_minimal_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        for name in crate::INHERITED_ENV {
            if let Some(value) = env::var_os(name) {
                process.env(name, value);
            }
        }
        for name in crate::RELAY_ENV {
            process.env_remove(name);
        }
        if let Some(expected) = declaration.workspace_identity {
            // The pathname can be replaced after the check above or chdir.
            // The host child must retain this publication identity through
            // daemon attachment and descriptor-backed mount admission.
            process
                .env("MARSH_MCP_EXPECTED_PROJECT_PATH", config.workspace())
                .env(
                    "MARSH_MCP_EXPECTED_PROJECT_DEV",
                    expected.device.to_string(),
                )
                .env("MARSH_MCP_EXPECTED_PROJECT_INO", expected.inode.to_string());
        }
        let mut child = process
            .spawn()
            .map_err(|e| format!("cannot start marsh: {e}"))?;
        let pid = child.id().ok_or("marsh process has no ID")?;
        let mut input = child.stdin.take().ok_or("marsh stdin unavailable")?;
        let output = child.stdout.take().ok_or("marsh stdout unavailable")?;
        let errors = child.stderr.take().ok_or("marsh stderr unavailable")?;
        let mut input_task = tokio::spawn(async move {
            match input.write_all(&stdin).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
                Err(error) => return Err(error.to_string()),
            }
            input.shutdown().await.map_err(|e| e.to_string())
        });
        let mut output_task = tokio::spawn(crate::read_bounded_stream(output));
        let mut error_task = tokio::spawn(crate::read_bounded_stream(errors));
        let deadline = tokio::time::Instant::now() + Duration::from_millis(declaration.timeout_ms);
        let (status, mut outcome, mut host_cleanup) = tokio::select! {
            result = child.wait() => {
                let status = result.map_err(|e| format!("cannot wait for marsh: {e}"))?;
                let outcome = if status.success() { ExecutionOutcome::Success } else { ExecutionOutcome::ExitNonzero };
                (status.code(), outcome, "verified")
            }
            () = context.ct.cancelled() => {
                let certainty = if crate::terminate_pipeline_process_group(pid, &mut child).await.is_ok() { "verified" } else { "uncertain" };
                (None, ExecutionOutcome::Cancelled, certainty)
            }
            () = exec_cancel.cancelled() => {
                let certainty = if crate::terminate_pipeline_process_group(pid, &mut child).await.is_ok() { "verified" } else { "uncertain" };
                (None, ExecutionOutcome::Cancelled, certainty)
            }
            () = tokio::time::sleep_until(deadline) => {
                let certainty = if crate::terminate_pipeline_process_group(pid, &mut child).await.is_ok() { "verified" } else { "uncertain" };
                (None, ExecutionOutcome::Timeout, certainty)
            }
        };
        let drain_deadline = if matches!(
            outcome,
            ExecutionOutcome::Timeout | ExecutionOutcome::Cancelled
        ) {
            tokio::time::Instant::now() + Duration::from_secs(2)
        } else {
            deadline
        };
        let drained = tokio::time::timeout_at(drain_deadline, async {
            let stdout = (&mut output_task).await.map_err(|e| e.to_string())??;
            let stderr = (&mut error_task).await.map_err(|e| e.to_string())??;
            let input = (&mut input_task).await.map_err(|e| e.to_string())?;
            Ok::<_, String>((stdout, stderr, input))
        })
        .await;
        let ((mut stdout, mut stdout_truncated), (mut stderr, mut stderr_truncated), input_result) =
            match drained {
                Ok(Ok(value)) => value,
                other => {
                    let certainty = if crate::terminate_pipeline_process_group(pid, &mut child)
                        .await
                        .is_ok()
                    {
                        "verified"
                    } else {
                        "uncertain"
                    };
                    output_task.abort();
                    error_task.abort();
                    input_task.abort();
                    return Err(format!(
                        "cannot drain marsh pipeline streams ({other:?}); cleanup_certainty={certainty}"
                    ));
                }
            };
        if let Err(error) = input_result {
            let certainty = if crate::terminate_pipeline_process_group(pid, &mut child)
                .await
                .is_ok()
            {
                "verified"
            } else {
                "uncertain"
            };
            return Err(format!(
                "cannot send marsh stdin: {error}; host_cleanup_certainty={certainty}"
            ));
        }
        if host_cleanup == "verified"
            && crate::process_group_exists(
                i32::try_from(pid).map_err(|_| "marsh process ID overflow")?,
            )
            .unwrap_or(true)
        {
            host_cleanup = if crate::terminate_pipeline_process_group(pid, &mut child)
                .await
                .is_ok()
            {
                "verified"
            } else {
                "uncertain"
            };
        }
        if stdout.len() > declaration.max_output_bytes {
            stdout.truncate(declaration.max_output_bytes);
            stdout_truncated = true;
        }
        if stderr.len() > declaration.max_output_bytes {
            stderr.truncate(declaration.max_output_bytes);
            stderr_truncated = true;
        }
        if host_cleanup != "verified" {
            outcome = ExecutionOutcome::Failed;
        }
        let output_complete = !stdout_truncated
            && !stderr_truncated
            && matches!(
                outcome,
                ExecutionOutcome::Success | ExecutionOutcome::ExitNonzero
            );
        let (stdout_text, stdout_text_state) = text_view(&stdout);
        let (stderr_text, stderr_text_state) = text_view(&stderr);
        Ok(ExportExecutionData {
            tool_name: declaration.tool_name.clone(),
            command: declaration.tool_name.clone(),
            outcome,
            execution: None,
            exit_code: status,
            stdout: stdout_text,
            stderr: stderr_text,
            stdout_base64: base64::engine::general_purpose::STANDARD.encode(stdout),
            stderr_base64: base64::engine::general_purpose::STANDARD.encode(stderr),
            stdout_text_state: stdout_text_state.into(),
            stderr_text_state: stderr_text_state.into(),
            stdout_truncated,
            stderr_truncated,
            cleanup_certainty: "uncertain".into(),
            host_cleanup_certainty: Some(host_cleanup.into()),
            output_complete,
            receipt_selector: None,
            job_id: None,
        })
    }

    /// Execute the prepared command through the existing daemon Kit path.
    #[allow(clippy::too_many_lines)]
    async fn execute_command(
        &self,
        argv: Vec<Vec<u8>>,
        stdin: Vec<u8>,
        context: &RequestContext<RoleServer>,
        exec_cancel: CancellationToken,
    ) -> Result<ExportExecutionData, String> {
        if self.config.declaration.pipeline.is_some() {
            return self.execute_pipeline(stdin, context, exec_cancel).await;
        }
        self.config.host_config.validate_host_executables()?;

        let home = self.config.host_config.home().to_path_buf();
        let marshd = self.config.host_config.marshd().to_path_buf();
        let sbx = self.config.host_config.sbx().to_path_buf();
        let command = self.config.declaration.command.clone();
        let tool_name = self.config.declaration.tool_name.clone();
        let max_output_bytes = self.config.declaration.max_output_bytes;
        let timeout = Duration::from_millis(self.config.declaration.timeout_ms);
        let workspace = self.config.host_config.workspace().to_path_buf();
        let username = self.config.host_config.username().to_string();

        let cancel_token = CancellationToken::new();
        let ct_clone = cancel_token.clone();
        let mcp_ct = context.ct.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = mcp_ct.cancelled() => ct_clone.cancel(),
                () = exec_cancel.cancelled() => ct_clone.cancel(),
            }
        });

        // Connect to the real daemon endpoint
        let client = Client::connect_if_running(&home)
            .map_err(|e| format!("cannot check daemon at {}: {e}", home.display()))?
            .map_or_else(|| Client::ensure_running(&home, &marshd, &sbx), Ok)
            .map_err(|e| format!("cannot connect to daemon at {}: {e}", home.display()))?;

        // MCP-01/MCP-03: Missing, changed, or unregistered Kit identity fails closed before invocation
        let registered_commands = client
            .registered_commands()
            .map_err(|e| format!("cannot query registered commands from daemon: {e}"))?;
        if !registered_commands.contains(&command) {
            return Err(format!(
                "command '{command}' is not registered in this marsh scope; available commands: {registered_commands:?}"
            ));
        }

        if let Some(expected_kit) = &self.config.declaration.kit_identity {
            let registered_kits = client
                .registered_kits()
                .map_err(|e| format!("cannot query registered kits from daemon: {e}"))?;
            let actual_kit = registered_kits.get(&command).ok_or_else(|| {
                format!("command '{command}' has no registered kit identity in daemon")
            })?;
            if !kit_profile_matches(actual_kit, expected_kit) {
                return Err(format!(
                    "command '{command}' registered kit identity '{actual_kit}' does not match declared kit_identity '{expected_kit}'"
                ));
            }
        }

        // Attach shell session authority for this execution
        // Keep the shell's natural guest HOME distinct from the host-side
        // backing directory, exactly as ordinary marsh command sessions do.
        let guest_home = natural_guest_home()?;
        let session_authority = SessionAuthority {
            username: username.clone(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            launch_directory: workspace.clone(),
            guest_home: guest_home.clone(),
            home_backing: home.clone(),
            ephemeral_home: false,
        };
        let session_id = attach_shell(&client, std::process::id(), session_authority)?;
        let mut session_guard = SessionGuard::new(&client, session_id.clone());

        let spec = ExecuteSpec {
            process: None,
            // Protocol-created jobs retain the admitted publishing project root.
            working_directory: None,
            placement: marsh_daemon::Placement::Local,
            environment: std::collections::BTreeMap::new(),
            command: command.clone(),
            arguments: argv,
            session: SessionSpec {
                session_id: session_guard.session_id().to_string(),
                username,
                uid: rustix::process::getuid().as_raw(),
                gid: rustix::process::getgid().as_raw(),
                launch_directory: workspace,
                guest_home,
                home_backing: home,
                ephemeral_home: false,
                terminal: false,
                terminal_size: None,
            },
        };

        let execution = match client.start_execution(spec) {
            Ok(exec) => exec,
            Err(error) => {
                let _ = session_guard.detach();
                return Err(format!("failed to start execution: {error}"));
            }
        };

        // Stream stdin
        if !stdin.is_empty()
            && let Err(error) = execution.send(&AttachmentFrame::Stdin { bytes: stdin })
        {
            let _ = session_guard.detach();
            return Err(format!("failed to stream stdin: {error}"));
        }
        if let Err(error) = execution.send(&AttachmentFrame::StdinEof) {
            let _ = session_guard.detach();
            return Err(format!("failed to send stdin EOF: {error}"));
        }

        // Read execution frames concurrently with timeout and cancellation monitoring
        let exec_clone = execution.clone();
        let receive_task = tokio::task::spawn_blocking(move || {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut stdout_truncated = false;
            let mut stderr_truncated = false;
            let mut exit_code = None;
            let mut failure = None;
            let mut early_eof = false;

            loop {
                match exec_clone.receive() {
                    Ok(AttachmentFrame::Stdout { bytes }) => {
                        let remaining = max_output_bytes.saturating_sub(stdout.len());
                        if bytes.len() > remaining {
                            stdout.extend_from_slice(&bytes[..remaining]);
                            stdout_truncated = true;
                        } else {
                            stdout.extend_from_slice(&bytes);
                        }
                    }
                    Ok(AttachmentFrame::Stderr { bytes }) => {
                        let remaining = max_output_bytes.saturating_sub(stderr.len());
                        if bytes.len() > remaining {
                            stderr.extend_from_slice(&bytes[..remaining]);
                            stderr_truncated = true;
                        } else {
                            stderr.extend_from_slice(&bytes);
                        }
                    }
                    Ok(AttachmentFrame::Exited { code }) => {
                        exit_code = Some(code);
                        break;
                    }
                    Ok(AttachmentFrame::Failed { message }) => {
                        failure = Some(message);
                        break;
                    }
                    Ok(AttachmentFrame::ColdBoot { .. } | _) => {}
                    Err(DaemonError::Io(err))
                        if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                    {
                        early_eof = true;
                        break;
                    }
                    Err(err) => {
                        early_eof = true;
                        failure = Some(err.to_string());
                        break;
                    }
                }
            }
            (
                stdout,
                stdout_truncated,
                stderr,
                stderr_truncated,
                exit_code,
                failure,
                early_eof,
            )
        });

        let mut receive_handle = receive_task;
        let stdout: Vec<u8>;
        let stdout_truncated: bool;
        let mut stderr: Vec<u8>;
        let stderr_truncated: bool;
        let exit_code: Option<i32>;
        let early_eof_occurred: bool;
        let mut outcome: ExecutionOutcome;

        tokio::select! {
            res = &mut receive_handle => {
                match res {
                    Ok((out, out_trunc, mut err, err_trunc, code, fail, eof)) => {
                        stdout = out;
                        stdout_truncated = out_trunc;
                        stderr_truncated = err_trunc;
                        exit_code = code;
                        early_eof_occurred = eof;

                        if early_eof_occurred {
                            outcome = ExecutionOutcome::Failed;
                            if err.is_empty() {
                                err = b"daemon connection closed unexpectedly before exit".to_vec();
                            }
                        } else if let Some(msg) = fail {
                            outcome = ExecutionOutcome::Failed;
                            if err.is_empty() {
                                err = msg.into_bytes();
                            }
                        } else if code == Some(0) {
                            outcome = ExecutionOutcome::Success;
                        } else {
                            outcome = ExecutionOutcome::ExitNonzero;
                        }
                        stderr = err;
                    }
                    Err(e) => {
                        stdout = Vec::new();
                        stdout_truncated = false;
                        stderr = format!("internal execution task panicked: {e}").into_bytes();
                        stderr_truncated = false;
                        exit_code = None;
                        early_eof_occurred = true;
                        outcome = ExecutionOutcome::Failed;
                    }
                }
            }
            () = cancel_token.cancelled() => {
                outcome = ExecutionOutcome::Cancelled;
                let (out, out_trunc, err, err_trunc, code, _fail, eof) =
                    escalate_and_drain(&execution, &mut receive_handle).await;
                stdout = out;
                stdout_truncated = out_trunc;
                stderr = err;
                stderr_truncated = err_trunc;
                exit_code = code;
                early_eof_occurred = eof;
            }
            () = tokio::time::sleep(timeout) => {
                outcome = ExecutionOutcome::Timeout;
                let (out, out_trunc, err, err_trunc, code, _fail, eof) =
                    escalate_and_drain(&execution, &mut receive_handle).await;
                stdout = out;
                stdout_truncated = out_trunc;
                stderr = err;
                stderr_truncated = err_trunc;
                exit_code = code;
                early_eof_occurred = eof;
            }
        }

        // Bounded cleanup: detach the session immediately after execution terminates
        let _ = session_guard.detach();

        // Fetch receipt information: bind strictly to EXACT session_id
        let mut receipt_selector = None;
        let mut job_id = None;
        let mut bound_receipt: Option<marsh_daemon::JobReceipt> = None;

        if let Ok(jobs_doc) = client.jobs() {
            for summary in &jobs_doc.jobs {
                if summary.command == command
                    && let Ok(receipt) = client.job(summary.job_id.clone())
                    && receipt.session_id == session_id
                {
                    receipt_selector = Some(receipt.cursor.to_string());
                    job_id = Some(receipt.job_id.clone());
                    bound_receipt = Some(receipt);
                    break;
                }
            }
        }

        let mut kit_mismatch_error = None;
        if let Some(expected_kit) = &self.config.declaration.kit_identity
            && let Some(receipt) = &bound_receipt
            && !kit_profile_matches(&receipt.kit_ref, expected_kit)
        {
            kit_mismatch_error = Some(format!(
                "job receipt kit_ref '{}' does not match declared kit_identity '{expected_kit}'",
                receipt.kit_ref
            ));
        }

        if matches!(outcome, ExecutionOutcome::Success)
            && bound_receipt.as_ref().is_none_or(|receipt| {
                !matches!(receipt.cleanup, marsh_daemon::CleanupState::Verified)
                    || !receipt.output_complete
            })
        {
            // Exit zero is not a successful tool delivery when the exact job
            // receipt cannot prove output completeness and container cleanup.
            outcome = ExecutionOutcome::Failed;
        }

        if let Some(msg) = kit_mismatch_error {
            outcome = ExecutionOutcome::Failed;
            if stderr.is_empty() {
                stderr = msg.into_bytes();
            } else {
                stderr.extend_from_slice(b"\n");
                stderr.extend_from_slice(msg.as_bytes());
            }
        }

        let cleanup_certainty = match (&outcome, &bound_receipt, early_eof_occurred) {
            (_, _, true) | (_, None, _) => "uncertain".to_string(),
            (
                ExecutionOutcome::Cancelled | ExecutionOutcome::Timeout | ExecutionOutcome::Failed,
                Some(receipt),
                false,
            ) => match receipt.cleanup {
                marsh_daemon::CleanupState::Verified => "verified".to_string(),
                _ => "uncertain".to_string(),
            },
            (ExecutionOutcome::Success | ExecutionOutcome::ExitNonzero, Some(receipt), false) => {
                match receipt.cleanup {
                    marsh_daemon::CleanupState::Verified
                    | marsh_daemon::CleanupState::NotRequired => "verified".to_string(),
                    marsh_daemon::CleanupState::Uncertain => "uncertain".to_string(),
                    marsh_daemon::CleanupState::Pending => "pending".to_string(),
                }
            }
        };

        let output_complete = !stdout_truncated
            && !stderr_truncated
            && !early_eof_occurred
            && (outcome == ExecutionOutcome::Success || outcome == ExecutionOutcome::ExitNonzero)
            && bound_receipt.as_ref().is_some_and(|r| r.output_complete);

        let (stdout_str, stdout_text_state) = text_view(&stdout);
        let (stderr_str, stderr_text_state) = text_view(&stderr);

        Ok(ExportExecutionData {
            tool_name,
            command,
            outcome,
            execution: bound_receipt
                .as_ref()
                .map(|receipt| receipt.execution.clone()),
            exit_code,
            stdout: stdout_str,
            stderr: stderr_str,
            stdout_base64: base64::engine::general_purpose::STANDARD.encode(stdout),
            stderr_base64: base64::engine::general_purpose::STANDARD.encode(stderr),
            stdout_text_state: stdout_text_state.into(),
            stderr_text_state: stderr_text_state.into(),
            stdout_truncated,
            stderr_truncated,
            cleanup_certainty,
            host_cleanup_certainty: None,
            output_complete,
            receipt_selector,
            job_id,
        })
    }
}

fn natural_guest_home() -> Result<PathBuf, String> {
    let uid = nix::unistd::Uid::effective();
    let account = nix::unistd::User::from_uid(uid)
        .map_err(|error| format!("cannot resolve effective UID {uid}: {error}"))?
        .ok_or_else(|| format!("effective UID {uid} has no passwd account"))?;
    if !account.dir.is_absolute() {
        return Err("effective passwd home is not absolute".into());
    }
    Ok(account.dir)
}

pub type TaskOutput = (
    Vec<u8>,
    bool,
    Vec<u8>,
    bool,
    Option<i32>,
    Option<String>,
    bool,
);

#[doc(hidden)]
pub async fn test_escalate_and_drain(
    execution: &marsh_daemon::ClientExecution,
    receive_handle: &mut tokio::task::JoinHandle<TaskOutput>,
) -> TaskOutput {
    escalate_and_drain(execution, receive_handle).await
}

async fn escalate_and_drain(
    execution: &marsh_daemon::ClientExecution,
    receive_handle: &mut tokio::task::JoinHandle<TaskOutput>,
) -> TaskOutput {
    let _ = execution.send(&AttachmentFrame::Signal {
        signal: "SIGINT".to_string(),
    });
    if let Ok(res) = tokio::time::timeout(Duration::from_millis(500), &mut *receive_handle).await {
        return match res {
            Ok(output) => output,
            Err(e) => (
                Vec::new(),
                false,
                format!("join error: {e}").into_bytes(),
                false,
                None,
                None,
                true,
            ),
        };
    }

    let _ = execution.send(&AttachmentFrame::Signal {
        signal: "SIGTERM".to_string(),
    });
    if let Ok(res) = tokio::time::timeout(Duration::from_millis(500), &mut *receive_handle).await {
        return match res {
            Ok(output) => output,
            Err(e) => (
                Vec::new(),
                false,
                format!("join error: {e}").into_bytes(),
                false,
                None,
                None,
                true,
            ),
        };
    }

    let _ = execution.send(&AttachmentFrame::Signal {
        signal: "SIGKILL".to_string(),
    });
    if let Ok(res) = tokio::time::timeout(Duration::from_millis(500), &mut *receive_handle).await {
        return match res {
            Ok(output) => output,
            Err(e) => (
                Vec::new(),
                false,
                format!("join error: {e}").into_bytes(),
                false,
                None,
                None,
                true,
            ),
        };
    }

    let _ = execution.shutdown();
    if let Ok(res) = tokio::time::timeout(Duration::from_secs(1), receive_handle).await {
        return match res {
            Ok(output) => output,
            Err(e) => (
                Vec::new(),
                false,
                format!("join error: {e}").into_bytes(),
                false,
                None,
                None,
                true,
            ),
        };
    }

    (Vec::new(), false, Vec::new(), false, None, None, true)
}

struct SessionGuard<'a> {
    client: &'a Client,
    session_id: String,
    active: bool,
}

impl<'a> SessionGuard<'a> {
    fn new(client: &'a Client, session_id: String) -> Self {
        Self {
            client,
            session_id,
            active: true,
        }
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn detach(&mut self) -> Result<(), String> {
        if self.active {
            self.active = false;
            detach_shell(self.client, &self.session_id)
        } else {
            Ok(())
        }
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let _ = self.detach();
    }
}

struct InFlightGuard {
    in_flight: Arc<TokioMutex<HashMap<u64, CancellationToken>>>,
    id: u64,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let in_flight = Arc::clone(&self.in_flight);
        let id = self.id;
        tokio::spawn(async move {
            let mut map = in_flight.lock().await;
            map.remove(&id);
        });
    }
}

pub(crate) fn truncate_to_char_boundary(s: &mut String, mut target_len: usize) {
    if target_len >= s.len() {
        return;
    }
    while target_len > 0 && !s.is_char_boundary(target_len) {
        target_len -= 1;
    }
    s.truncate(target_len);
}

fn text_view(bytes: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), "utf8"),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), "lossy"),
    }
}

pub(crate) fn measure_call_tool_result_frame(data: &ExportExecutionData) -> usize {
    let is_err = data.outcome != ExecutionOutcome::Success;
    let payload = json!({
        "ok": !is_err,
        "data": data
    });
    let mut res = CallToolResult::structured(payload);
    res.is_error = Some(is_err);
    res.content = vec![execution_text(data)];
    let envelope = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": res
    });
    serde_json::to_vec(&envelope).map_or(usize::MAX, |v| v.len())
}

fn execution_text(data: &ExportExecutionData) -> ContentBlock {
    use std::fmt::Write as _;
    let outcome = match data.outcome {
        ExecutionOutcome::Success => "success",
        ExecutionOutcome::ExitNonzero => "exit_nonzero",
        ExecutionOutcome::Timeout => "timeout",
        ExecutionOutcome::Cancelled => "cancelled",
        ExecutionOutcome::Failed => "failed",
    };
    let cleanup_label = if data.host_cleanup_certainty.is_some() {
        "sandbox_cleanup"
    } else {
        "cleanup"
    };
    let mut text = format!(
        "{}: {outcome}; exit_code={}; {cleanup_label}={}.\n",
        data.tool_name,
        data.exit_code
            .map_or_else(|| "unknown".to_string(), |code| code.to_string()),
        data.cleanup_certainty
    );
    if let Some(host_cleanup) = &data.host_cleanup_certainty {
        let _ = writeln!(text, "host_process_group_cleanup={host_cleanup}.");
    }
    for (label, stream, encoded, truncated) in [
        (
            "stdout",
            &data.stdout,
            &data.stdout_base64,
            data.stdout_truncated,
        ),
        (
            "stderr",
            &data.stderr,
            &data.stderr_base64,
            data.stderr_truncated,
        ),
    ] {
        if !stream.is_empty()
            && stream
                .chars()
                .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
        {
            let mut preview = stream.clone();
            truncate_to_char_boundary(&mut preview, 8_192);
            let _ = write!(text, "{label}: {preview}");
            if preview.len() < stream.len() || truncated {
                text.push_str(" [truncated]");
            }
            text.push('\n');
        } else if !encoded.is_empty() {
            let _ = writeln!(
                text,
                "{label}: bytes available in structuredContent ({})",
                if truncated { "truncated" } else { "exact" }
            );
        }
    }
    ContentBlock::text(text)
}

pub(crate) fn bound_response_data(data: &mut ExportExecutionData) {
    const FRAME_PAYLOAD_LIMIT: usize = 1_000_000;
    let mut frame_len = measure_call_tool_result_frame(data);
    if frame_len <= FRAME_PAYLOAD_LIMIT {
        return;
    }
    // The base64 fields are authoritative bytes. Text duplicates may be omitted
    // without losing output or changing completion semantics.
    if !data.stdout_base64.is_empty() || !data.stderr_base64.is_empty() {
        if !data.stdout.is_empty() {
            data.stdout_text_state = "omitted".into();
        }
        if !data.stderr.is_empty() {
            data.stderr_text_state = "omitted".into();
        }
        data.stdout.clear();
        data.stderr.clear();
        frame_len = measure_call_tool_result_frame(data);
        if frame_len <= FRAME_PAYLOAD_LIMIT {
            return;
        }
    }
    data.output_complete = false;

    for _ in 0..100 {
        if frame_len <= FRAME_PAYLOAD_LIMIT {
            break;
        }
        let excess = frame_len - FRAME_PAYLOAD_LIMIT;

        if data.stdout.len() >= data.stderr.len() && data.stdout.len() > 256 {
            let max_cut = (data.stdout.len() - 256) / 2 + 1;
            let cut = (excess / 4).clamp(256, max_cut);
            let target = data.stdout.len() - cut;
            truncate_to_char_boundary(&mut data.stdout, target);
            data.stdout_truncated = true;
            data.stdout_text_state = "truncated".into();
        } else if data.stderr.len() > 256 {
            let max_cut = (data.stderr.len() - 256) / 2 + 1;
            let cut = (excess / 4).clamp(256, max_cut);
            let target = data.stderr.len() - cut;
            truncate_to_char_boundary(&mut data.stderr, target);
            data.stderr_truncated = true;
            data.stderr_text_state = "truncated".into();
        } else if !data.stdout.is_empty() {
            let target = data.stdout.len().saturating_sub(excess / 4 + 1);
            truncate_to_char_boundary(&mut data.stdout, target);
            data.stdout_truncated = true;
            data.stdout_text_state = "truncated".into();
        } else if !data.stderr.is_empty() {
            let target = data.stderr.len().saturating_sub(excess / 4 + 1);
            truncate_to_char_boundary(&mut data.stderr, target);
            data.stderr_truncated = true;
            data.stderr_text_state = "truncated".into();
        } else {
            break;
        }

        let new_len = measure_call_tool_result_frame(data);
        if new_len >= frame_len {
            if !data.stdout.is_empty() {
                data.stdout.pop();
                data.stdout_truncated = true;
                data.stdout_text_state = "truncated".into();
            } else if !data.stderr.is_empty() {
                data.stderr.pop();
                data.stderr_truncated = true;
                data.stderr_text_state = "truncated".into();
            } else {
                break;
            }
            frame_len = measure_call_tool_result_frame(data);
        } else {
            frame_len = new_len;
        }
    }
}

fn attach_shell(client: &Client, pid: u32, session: SessionAuthority) -> Result<String, String> {
    match client
        .request(PublicRequest::AttachShell { pid, session })
        .map_err(|e| format!("daemon request failed: {e}"))?
    {
        PublicReply::ShellAttached { session_id } => Ok(session_id),
        other => Err(format!(
            "unexpected daemon reply to attach_shell: {other:?}"
        )),
    }
}

fn detach_shell(client: &Client, session_id: &str) -> Result<(), String> {
    match client
        .request(PublicRequest::DetachShell {
            session_id: session_id.to_string(),
        })
        .map_err(|e| format!("daemon request failed: {e}"))?
    {
        PublicReply::Detached => Ok(()),
        other => Err(format!(
            "unexpected daemon reply to detach_shell: {other:?}"
        )),
    }
}

fn format_arg_value(value: &Value) -> Vec<u8> {
    match value {
        Value::String(s) => s.as_bytes().to_vec(),
        Value::Bool(b) => (if *b { "true" } else { "false" }).as_bytes().to_vec(),
        Value::Number(n) => n.to_string().into_bytes(),
        other => other.to_string().into_bytes(),
    }
}

impl ServerHandler for ExportMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "marsh-export",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Export-only MCP server for one explicitly declared typed Kit command.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        if !self.publication_current() {
            return Ok(ListToolsResult::with_all_items(Vec::new()));
        }
        let input_schema = match serde_json::to_value(&self.config.declaration.input_schema) {
            Ok(Value::Object(map)) => Arc::new(map),
            _ => Arc::new(Map::new()),
        };
        let tool = Tool::new(
            self.config.declaration.tool_name.clone(),
            self.config.declaration.description.clone(),
            input_schema,
        );
        Ok(ListToolsResult::with_all_items(vec![tool]))
    }

    #[allow(clippy::too_many_lines)] // Early rejection and execution share this protocol boundary.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let arguments_value = request.arguments.map_or(Value::Null, Value::Object);
        if !self.publication_current() {
            let mut res = CallToolResult::structured(json!({
                "ok": false,
                "error": "publication was removed or changed; reload this MCP tool"
            }));
            res.is_error = Some(true);
            return Ok(res.into());
        }
        // MCP-01/MCP-05: Disallow any tool other than the single declared tool
        if request.name != self.config.declaration.tool_name {
            let error_payload = json!({
                "ok": false,
                "error": format!(
                    "tool '{}' not found; export-only server exposes only '{}'",
                    request.name, self.config.declaration.tool_name
                )
            });
            let mut res = CallToolResult::structured(error_payload);
            res.is_error = Some(true);
            return Ok(res.into());
        }

        let (argv, stdin) = match self.prepare_arguments(&arguments_value) {
            Ok(prepared) => prepared,
            Err(validation_error) => {
                let error_payload = json!({
                    "ok": false,
                    "error": validation_error
                });
                let mut res = CallToolResult::structured(error_payload);
                res.is_error = Some(true);
                return Ok(res.into());
            }
        };

        let exec_id = self.next_exec_id.fetch_add(1, Ordering::Relaxed);
        let exec_cancel = CancellationToken::new();
        {
            let mut map = self.in_flight.lock().await;
            map.insert(exec_id, exec_cancel.clone());
        }
        let in_flight_map = Arc::clone(&self.in_flight);
        let _in_flight_guard = InFlightGuard {
            in_flight: in_flight_map,
            id: exec_id,
        };

        match self
            .execute_command(argv, stdin, &context, exec_cancel)
            .await
        {
            Ok(mut data) => {
                bound_response_data(&mut data);
                let is_err = data.outcome != ExecutionOutcome::Success;
                let payload = json!({
                    "ok": !is_err,
                    "data": data
                });
                let mut res = CallToolResult::structured(payload);
                res.is_error = Some(is_err);
                res.content = vec![execution_text(&data)];
                Ok(res.into())
            }
            Err(exec_error) => {
                let error_payload = json!({
                    "ok": false,
                    "error": exec_error
                });
                let mut res = CallToolResult::structured(error_payload);
                res.is_error = Some(true);
                Ok(res.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_declaration_json() -> &'static str {
        r#"{
            "schema_version": "marsh.published_tool/v1",
            "tool_name": "project_test",
            "description": "Run project test suite",
            "command": "test-report",
            "input_schema": {
                "type": "object",
                "properties": {
                    "filter": { "type": "string", "description": "Test name filter" },
                    "verbose": { "type": "boolean", "description": "Enable verbose output" },
                    "payload": { "type": "string", "description": "Standard input data" }
                },
                "required": ["filter"],
                "additionalProperties": false
            },
            "bindings": {
                "argv": [
                    { "type": "literal", "value": "--run" },
                    { "type": "named_option", "option": "--filter", "field": "filter" },
                    { "type": "flag", "option": "--verbose", "field": "verbose" }
                ],
                "stdin": {
                    "field": "payload",
                    "max_bytes": 1024
                }
            },
            "max_output_bytes": 1024,
            "timeout_ms": 5000
        }"#
    }

    #[test]
    fn parse_valid_declaration() {
        let decl = ToolDeclaration::load_from_str(sample_declaration_json(), None).unwrap();
        assert_eq!(decl.tool_name, "project_test");
        assert_eq!(decl.command, "test-report");
        assert_eq!(decl.bindings.argv.len(), 3);
        assert!(decl.bindings.stdin.is_some());
    }

    #[test]
    fn pipeline_declaration_rejects_kit_command_and_argv() {
        let mut declaration =
            ToolDeclaration::load_from_str(sample_declaration_json(), None).unwrap();
        declaration.pipeline = Some("cat | wc -c".into());
        assert!(
            declaration
                .validate()
                .unwrap_err()
                .contains("either command or pipeline")
        );
        declaration.command.clear();
        assert!(
            declaration
                .validate()
                .unwrap_err()
                .contains("cannot bind Kit identity or argv")
        );
        declaration.bindings.argv.clear();
        declaration.kit_identity = None;
        assert!(declaration.validate().is_ok());
    }

    #[test]
    fn reject_unknown_schema_version() {
        let json = sample_declaration_json().replace("marsh.published_tool/v1", "unsupported/v99");
        let err = ToolDeclaration::load_from_str(&json, None).unwrap_err();
        assert!(err.contains("unsupported schema_version"));
    }

    #[test]
    fn reject_binding_referencing_undeclared_property() {
        let json = sample_declaration_json()
            .replace("\"field\": \"filter\"", "\"field\": \"nonexistent\"");
        let err = ToolDeclaration::load_from_str(&json, None).unwrap_err();
        assert!(err.contains("not declared in input_schema properties"));
    }

    #[test]
    fn reject_option_injection_in_binding() {
        let json1 = sample_declaration_json()
            .replace("\"option\": \"--filter\"", "\"option\": \"bad_option\"");
        let err1 = ToolDeclaration::load_from_str(&json1, None).unwrap_err();
        assert!(err1.contains("must start with '-' or '--'"));

        let json2 = sample_declaration_json()
            .replace("\"option\": \"--filter\"", "\"option\": \"--bad option\"");
        let err2 = ToolDeclaration::load_from_str(&json2, None).unwrap_err();
        assert!(err2.contains("contains invalid characters"));
    }

    #[test]
    fn prepare_arguments_valid() {
        let decl = ToolDeclaration::load_from_str(sample_declaration_json(), None).unwrap();
        let export_mcp = ExportMcp::new(ExportConfig {
            host_config: fake_host_config(),
            declaration: decl,
            declaration_path: None,
        });
        let input = json!({
            "filter": "auth_test",
            "verbose": true,
            "payload": "test input data"
        });
        let (argv, stdin) = export_mcp.prepare_arguments(&input).unwrap();
        let argv_strings: Vec<String> = argv
            .into_iter()
            .map(|b| String::from_utf8(b).unwrap())
            .collect();
        assert_eq!(
            argv_strings,
            vec!["--run", "--filter", "auth_test", "--verbose"]
        );
        assert_eq!(String::from_utf8(stdin).unwrap(), "test input data");
    }

    #[test]
    fn prepare_arguments_rejects_unknown_field_mcp01() {
        let decl = ToolDeclaration::load_from_str(sample_declaration_json(), None).unwrap();
        let export_mcp = ExportMcp::new(ExportConfig {
            host_config: fake_host_config(),
            declaration: decl,
            declaration_path: None,
        });
        let input = json!({
            "filter": "auth_test",
            "unknown_injected_field": "injected"
        });
        let err = export_mcp.prepare_arguments(&input).unwrap_err();
        assert!(err.contains("unknown field 'unknown_injected_field' is not permitted"));
    }

    #[test]
    fn prepare_arguments_rejects_missing_required_field() {
        let decl = ToolDeclaration::load_from_str(sample_declaration_json(), None).unwrap();
        let export_mcp = ExportMcp::new(ExportConfig {
            host_config: fake_host_config(),
            declaration: decl,
            declaration_path: None,
        });
        let input = json!({
            "verbose": true
        });
        let err = export_mcp.prepare_arguments(&input).unwrap_err();
        assert!(err.contains("missing required field 'filter'"));
    }

    #[test]
    fn prepare_arguments_rejects_option_injection_mcp02() {
        let decl = ToolDeclaration::load_from_str(sample_declaration_json(), None).unwrap();
        let export_mcp = ExportMcp::new(ExportConfig {
            host_config: fake_host_config(),
            declaration: decl,
            declaration_path: None,
        });
        let input = json!({
            "filter": "--delete-all-data",
            "verbose": false
        });
        let err = export_mcp.prepare_arguments(&input).unwrap_err();
        assert!(err.contains("cannot start with '-' (option injection prevented)"));
    }

    #[test]
    fn prepare_arguments_rejects_oversized_stdin() {
        let decl = ToolDeclaration::load_from_str(sample_declaration_json(), None).unwrap();
        let export_mcp = ExportMcp::new(ExportConfig {
            host_config: fake_host_config(),
            declaration: decl,
            declaration_path: None,
        });
        let input = json!({
            "filter": "test",
            "payload": "x".repeat(2000) // max_bytes is 1024
        });
        let err = export_mcp.prepare_arguments(&input).unwrap_err();
        assert!(err.contains("exceeds configured limit of 1024 bytes"));
    }

    #[test]
    fn reject_unsupported_json_schema_keyword_pattern() {
        let json = sample_declaration_json().replace(
            "\"type\": \"string\", \"description\": \"Test name filter\"",
            "\"type\": \"string\", \"pattern\": \"^[a-z]+$\"",
        );
        let err = ToolDeclaration::load_from_str(&json, None).unwrap_err();
        assert!(err.contains("unsupported JSON Schema keyword 'pattern'"));
    }

    #[test]
    fn reject_unsupported_json_schema_keyword_enum() {
        let json = sample_declaration_json().replace(
            "\"type\": \"string\", \"description\": \"Test name filter\"",
            "\"type\": \"string\", \"enum\": [\"a\", \"b\"]",
        );
        let err = ToolDeclaration::load_from_str(&json, None).unwrap_err();
        assert!(err.contains("unsupported JSON Schema keyword 'enum'"));
    }

    #[test]
    fn reject_unsupported_additional_properties_true() {
        let json = sample_declaration_json().replace(
            "\"additionalProperties\": false",
            "\"additionalProperties\": true",
        );
        let err = ToolDeclaration::load_from_str(&json, None).unwrap_err();
        assert!(err.contains("additionalProperties"));
    }

    #[test]
    fn prepare_arguments_rejects_nul_bytes() {
        let decl = ToolDeclaration::load_from_str(sample_declaration_json(), None).unwrap();
        let export_mcp = ExportMcp::new(ExportConfig {
            host_config: fake_host_config(),
            declaration: decl,
            declaration_path: None,
        });
        let input = json!({
            "filter": "test\0injected"
        });
        let err = export_mcp.prepare_arguments(&input).unwrap_err();
        assert!(err.contains("cannot contain NUL bytes"));
    }

    fn fake_host_config() -> HostConfig {
        let temp = tempfile::tempdir().unwrap();
        let ws = temp.path().join("ws");
        fs::create_dir(&ws).unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let marsh = temp.path().join("marsh");
        fs::write(&marsh, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&marsh, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let marshd = temp.path().join("marshd");
        fs::write(&marshd, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&marshd, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let sbx = temp.path().join("sbx");
        fs::write(&sbx, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&sbx, fs::Permissions::from_mode(0o755)).unwrap();
        }
        HostConfig::new(&ws, &home, &marsh, &sbx, false).unwrap()
    }

    #[test]
    fn kit_profile_matches_exact_and_sha256_suffix() {
        let expected = "local-v3:/path/to/test-kit";
        // Exact match
        assert!(kit_profile_matches("local-v3:/path/to/test-kit", expected));
        // Valid exact source + sha256 suffix
        let with_digest = "local-v3:/path/to/test-kit@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert!(kit_profile_matches(with_digest, expected));

        // Sibling path must NOT match
        assert!(!kit_profile_matches(
            "local-v3:/path/to/test-kit-sibling",
            expected
        ));
        assert!(!kit_profile_matches(
            "local-v3:/path/to/test-kit2",
            expected
        ));
        assert!(!kit_profile_matches(
            "local-v3:/path/to/test-kit/child",
            expected
        ));
        // Reverse sibling path must NOT match
        assert!(!kit_profile_matches(
            expected,
            "local-v3:/path/to/test-kit-sibling"
        ));

        // Invalid digest suffixes
        assert!(!kit_profile_matches(
            "local-v3:/path/to/test-kit@sha256:",
            expected
        ));
        assert!(!kit_profile_matches(
            "local-v3:/path/to/test-kit@sha256:not-hex-chars!",
            expected
        ));
        assert!(!kit_profile_matches(
            "local-v3:/path/to/test-kit@md5:0123456789abcdef",
            expected
        ));
    }

    #[test]
    fn bound_response_data_utf8_safe_no_panics() {
        let mut data = ExportExecutionData {
            tool_name: "test".to_string(),
            command: "test".to_string(),
            outcome: ExecutionOutcome::Success,
            execution: None,
            exit_code: Some(0),
            // Multi-byte characters (4-byte emojis, 3-byte CJK) and escape characters
            stdout: "🦀🚀✨\n\"\\\t\r你好世界".repeat(100_000),
            stderr: "приветαβγδε\"\\\n\t".repeat(100_000),
            stdout_base64: String::new(),
            stderr_base64: String::new(),
            stdout_text_state: "utf8".to_string(),
            stderr_text_state: "utf8".to_string(),
            stdout_truncated: false,
            stderr_truncated: false,
            output_complete: true,
            job_id: Some("job_123".to_string()),
            receipt_selector: Some("cursor_123".to_string()),
            cleanup_certainty: "verified".to_string(),
            host_cleanup_certainty: None,
        };

        bound_response_data(&mut data);

        assert!(data.stdout_truncated || data.stderr_truncated);
        assert!(!data.output_complete);
        let frame_len = measure_call_tool_result_frame(&data);
        assert!(
            frame_len <= 1_048_576,
            "frame_len {frame_len} exceeds 1 MiB"
        );
        // Verify both strings remain valid UTF-8 and don't panic
        assert!(!data.stdout.is_empty());
        assert!(!data.stderr.is_empty());
    }
}
