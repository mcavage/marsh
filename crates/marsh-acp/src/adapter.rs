//! The ACP session binding for one registered command whose Kit declares an
//! ACP adapter (`com.docker.sandbox/acp@1`). The daemon builds it from the
//! Kit descriptor; it names no image, entrypoint, mount, or credential.

use crate::error::AgentError;
use crate::protocol::AgentCapabilities;
use serde::{Deserialize, Serialize};

/// Supported protocol modes for agents.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProtocol {
    /// Agent Client Protocol v1 over stdio JSON-RPC.
    AcpV1,
    /// Standard CLI/TUI command; does not speak ACP JSON-RPC.
    NativeCli,
}

/// An ACP agent: a registered command and the exact Kit generation it ran.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentAdapterDeclaration {
    /// Schema version for the declaration (must be 1).
    pub schema_version: u32,
    /// The agent's name: its registered command (e.g. `claude`).
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

        for capability in &self.required_capabilities {
            if capability != "loadSession" && capability != "sessionCapabilities.resume" {
                return Err(AgentError::InvalidConfiguration(format!(
                    "unsupported required ACP capability '{capability}'"
                )));
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adapter_validation() {
        let valid = AgentAdapterDeclaration {
            schema_version: 1,
            name: "codex".into(),
            protocol: AgentProtocol::AcpV1,
            command: "codex".into(),
            workload_digest: "sha256:abcd".into(),
            required_capabilities: vec![],
            description: None,
        };
        assert!(valid.validate().is_ok());
        for invalid in [
            AgentAdapterDeclaration {
                schema_version: 2,
                ..valid.clone()
            },
            AgentAdapterDeclaration {
                name: String::new(),
                ..valid.clone()
            },
            AgentAdapterDeclaration {
                command: String::new(),
                ..valid.clone()
            },
            AgentAdapterDeclaration {
                workload_digest: "  ".into(),
                ..valid.clone()
            },
        ] {
            assert!(invalid.validate().is_err());
        }
    }
}
