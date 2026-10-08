//! Versioned native Kit adapter declarations and agent registry.
//!
//! Enforces PRD ACP-01:
//! - Maps distinct explicit names (e.g. `claude-session`) to registered ACP-speaking workloads.
//! - Rejects overrides of image, entrypoint, mounts, network, or credentials. A
//!   declaration may only append a short fixed argument tail (e.g. `--acp`) to
//!   the Kit's own entrypoint, as a user's typed arguments would be.
//! - Forbids fake claims that ordinary CLI tools (such as plain `claude`) support ACP.

use crate::error::AgentError;
use crate::protocol::AgentCapabilities;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Supported protocol modes for agents.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProtocol {
    /// Agent Client Protocol v1 over stdio JSON-RPC.
    AcpV1,
    /// Standard CLI/TUI command; does not speak ACP JSON-RPC.
    NativeCli,
}

const MAX_ARGUMENTS: usize = 8;
const MAX_ARGUMENT_BYTES: usize = 128;

/// Versioned declaration mapping an agent command name to an ACP-capable native Kit workload.
///
/// Under PRD ACP-01, this declaration references an existing registered native Kit
/// command and workload generation. It CANNOT override the OCI entrypoint,
/// image, mounts, credentials, or runtime policies; `arguments` is appended to
/// the Kit entrypoint exactly like a user's command-line arguments.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAdapterDeclaration {
    /// Schema version for the declaration (must be 1).
    pub schema_version: u32,
    /// Explicit shell command name (e.g. `claude-session`).
    pub name: String,
    /// Protocol spoken by the workload entrypoint.
    pub protocol: AgentProtocol,
    /// Name of registered native Kit command in `commands.json`.
    pub command: String,
    /// Exact workload generation identity. May be omitted only for a registered
    /// local source Kit; the daemon resolves and pins it during startup.
    #[serde(default)]
    pub workload_digest: String,
    /// Required ACP capabilities that must be negotiated before turns begin.
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Fixed arguments appended to the Kit entrypoint when an ACP session
    /// starts, e.g. `["--acp"]` to select an agent Kit's ACP mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,
    /// Optional human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl AgentAdapterDeclaration {
    /// Validates the declaration for schema correctness and authority invariant rules.
    ///
    /// # Errors
    /// Returns [`AgentError`] if declaration is invalid or falsely claims CLI supports ACP.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.schema_version != 1 {
            return Err(AgentError::InvalidConfiguration(format!(
                "unsupported schema version {}; expected 1",
                self.schema_version
            )));
        }
        if self.name.trim().is_empty() {
            return Err(AgentError::InvalidConfiguration(
                "agent name cannot be empty".into(),
            ));
        }
        if self.command.trim().is_empty() {
            return Err(AgentError::InvalidConfiguration(
                "referenced command cannot be empty".into(),
            ));
        }
        if !self.workload_digest.is_empty() && self.workload_digest.trim().is_empty() {
            return Err(AgentError::InvalidConfiguration(
                "workload digest cannot contain only whitespace".into(),
            ));
        }

        if self.arguments.len() > MAX_ARGUMENTS
            || self.arguments.iter().any(|argument| {
                argument.is_empty()
                    || argument.len() > MAX_ARGUMENT_BYTES
                    || argument.chars().any(char::is_control)
            })
        {
            return Err(AgentError::InvalidConfiguration(format!(
                "agent arguments must be at most {MAX_ARGUMENTS} non-empty printable values of at most {MAX_ARGUMENT_BYTES} bytes"
            )));
        }

        for capability in &self.required_capabilities {
            if capability != "loadSession" && capability != "sessionCapabilities.resume" {
                return Err(AgentError::InvalidConfiguration(format!(
                    "unsupported required ACP capability '{capability}'"
                )));
            }
        }

        // Truthfulness guard: plain CLI tools must not be registered as ACP.
        // Specifically, the standard `claude` command is a CLI tool, not an ACP server.
        // Only explicit names like `claude-session` referencing dedicated ACP workloads are valid.
        if self.name == "claude" && self.protocol == AgentProtocol::AcpV1 {
            return Err(AgentError::UnsupportedProtocol {
                command: "claude".into(),
                reason: "plain 'claude' CLI does not speak ACP stdio JSON-RPC; use an explicit name like 'claude-session' referencing a tested ACP workload".into(),
            });
        }

        Ok(())
    }

    /// Returns the first required capability absent from the negotiated handshake.
    #[must_use]
    pub fn missing_capability(&self, capabilities: Option<&AgentCapabilities>) -> Option<&str> {
        self.required_capabilities.iter().find_map(|required| {
            let advertised = match required.as_str() {
                "loadSession" => capabilities.is_some_and(AgentCapabilities::can_load_session),
                "sessionCapabilities.resume" => {
                    capabilities.is_some_and(AgentCapabilities::can_resume_session)
                }
                _ => false,
            };
            (!advertised).then_some(required.as_str())
        })
    }
}

/// Registry of agent adapters loaded from `agents.json`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AgentRegistry {
    adapters: BTreeMap<String, AgentAdapterDeclaration>,
}

impl AgentRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            adapters: BTreeMap::new(),
        }
    }

    /// Parses and validates an `agents.json` document.
    ///
    /// # Errors
    /// Returns [`AgentError`] if parsing or validation fails.
    pub fn from_json_str(json: &str) -> Result<Self, AgentError> {
        let declarations: Vec<AgentAdapterDeclaration> = serde_json::from_str(json)?;
        let mut registry = Self::new();
        for decl in declarations {
            registry.register(decl)?;
        }
        Ok(registry)
    }

    /// Registers a single agent adapter declaration after validation.
    ///
    /// # Errors
    /// Returns [`AgentError`] if declaration validation fails.
    pub fn register(&mut self, decl: AgentAdapterDeclaration) -> Result<(), AgentError> {
        decl.validate()?;
        if self.adapters.contains_key(&decl.name) {
            return Err(AgentError::DuplicateName(decl.name));
        }
        self.adapters.insert(decl.name.clone(), decl);
        Ok(())
    }

    /// Looks up an adapter by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AgentAdapterDeclaration> {
        self.adapters.get(name)
    }

    /// Declarations for daemon startup binding to registered Kit generations.
    pub fn declarations(&self) -> impl Iterator<Item = &AgentAdapterDeclaration> {
        self.adapters.values()
    }

    /// Resolves an ACP adapter by explicit name.
    ///
    /// Explicitly rejects ordinary CLI names (e.g. `claude`) with [`AgentError::UnsupportedProtocol`].
    ///
    /// # Errors
    /// Returns [`AgentError::UnsupportedProtocol`] if command is CLI only.
    /// Returns [`AgentError::NotFound`] if command is unknown.
    pub fn resolve_acp(&self, name: &str) -> Result<&AgentAdapterDeclaration, AgentError> {
        if name == "claude" {
            return Err(AgentError::UnsupportedProtocol {
                command: "claude".into(),
                reason: "plain 'claude' is a native CLI command; ACP session control requires an explicitly registered ACP workload such as 'claude-session'".into(),
            });
        }

        let decl = self
            .adapters
            .get(name)
            .ok_or_else(|| AgentError::NotFound(name.into()))?;

        if decl.protocol != AgentProtocol::AcpV1 {
            return Err(AgentError::UnsupportedProtocol {
                command: name.into(),
                reason: format!("protocol for '{name}' is not ACP v1"),
            });
        }

        Ok(decl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adapter_validation() {
        let valid = AgentAdapterDeclaration {
            schema_version: 1,
            name: "codex-session".into(),
            protocol: AgentProtocol::AcpV1,
            command: "codex-workload".into(),
            workload_digest: "sha256:abcd".into(),
            required_capabilities: vec![],
            arguments: vec![],
            description: None,
        };
        assert!(valid.validate().is_ok());

        let invalid_schema = AgentAdapterDeclaration {
            schema_version: 2,
            ..valid.clone()
        };
        assert!(invalid_schema.validate().is_err());

        let empty_name = AgentAdapterDeclaration {
            name: String::new(),
            ..valid.clone()
        };
        assert!(empty_name.validate().is_err());

        let empty_command = AgentAdapterDeclaration {
            command: String::new(),
            ..valid.clone()
        };
        assert!(empty_command.validate().is_err());

        let empty_digest = AgentAdapterDeclaration {
            workload_digest: String::new(),
            ..valid
        };
        assert!(empty_digest.validate().is_ok());

        let whitespace_digest = AgentAdapterDeclaration {
            workload_digest: "  ".into(),
            ..empty_digest
        };
        assert!(whitespace_digest.validate().is_err());

        let mode = AgentAdapterDeclaration {
            arguments: vec!["--acp".into()],
            ..whitespace_digest.clone()
        };
        assert!(mode.validate().is_err(), "digest is still checked");
        let mode = AgentAdapterDeclaration {
            workload_digest: String::new(),
            ..mode
        };
        assert!(mode.validate().is_ok());
        for arguments in [
            vec![String::new()],
            vec!["a\nb".into()],
            vec!["x".into(); 9],
        ] {
            let bad = AgentAdapterDeclaration {
                arguments,
                ..mode.clone()
            };
            assert!(bad.validate().is_err());
        }
    }

    #[test]
    fn packaged_local_source_adapter_may_omit_generation() {
        let registry = AgentRegistry::from_json_str(
            r#"[{"schema_version":1,"name":"claude-session","protocol":"acp_v1","command":"claude-acp","required_capabilities":[]}]"#,
        )
        .unwrap();
        assert_eq!(
            registry
                .resolve_acp("claude-session")
                .unwrap()
                .workload_digest,
            ""
        );
        assert_eq!(registry.declarations().count(), 1);
    }

    #[test]
    fn packaged_registry_selects_each_agent_kit_acp_mode() {
        let registry =
            AgentRegistry::from_json_str(include_str!("../../../packaging/agents.json")).unwrap();
        for (session, command) in [
            ("claude-session", "claude"),
            ("codex-session", "codex"),
            ("pi-session", "pi"),
        ] {
            let declaration = registry.resolve_acp(session).unwrap();
            assert_eq!(declaration.command, command);
            assert_eq!(declaration.arguments, ["--acp"]);
        }
    }

    #[test]
    fn test_registry_lookup() {
        let mut reg = AgentRegistry::new();
        let decl = AgentAdapterDeclaration {
            schema_version: 1,
            name: "claude-session".into(),
            protocol: AgentProtocol::AcpV1,
            command: "claude-workload".into(),
            workload_digest: "sha256:1234".into(),
            required_capabilities: vec![],
            arguments: vec![],
            description: None,
        };
        reg.register(decl).unwrap();

        assert!(reg.resolve_acp("claude-session").is_ok());
        assert!(reg.resolve_acp("unknown").is_err());
        // Claude CLI is never resolved as ACP
        assert!(reg.resolve_acp("claude").is_err());
    }
}
