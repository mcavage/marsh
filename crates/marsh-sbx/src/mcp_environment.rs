//! Register native environment MCP declarations without replacing host state.

use super::{ReadyShellVm, SbxError, StockSbx, VmClass, validate_name};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpServer {
    pub name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl McpServer {
    /// Validate the native name/url-or-command shape before any stock effect.
    ///
    /// # Errors
    /// Rejects malformed names, ambiguous transports and unbounded argv.
    pub fn validate(&self) -> Result<(), SbxError> {
        if self.name.is_empty()
            || self.name.len() > 128
            || !self
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            || self.name.starts_with('-')
        {
            return Err(SbxError::InvalidIdentifier(self.name.clone()));
        }
        if self.url.is_empty() == self.command.is_empty()
            || (!self.url.is_empty() && !self.args.is_empty())
            || self.args.len() > 128
            || self.url.len()
                + self.command.len()
                + self.args.iter().map(String::len).sum::<usize>()
                > 32 * 1024
            || std::iter::once(&self.command)
                .chain(std::iter::once(&self.url))
                .chain(self.args.iter())
                .any(|value| value.contains('\0') || value.contains('\n') || value.contains('\r'))
        {
            return Err(SbxError::LifecycleSchema {
                operation: "environment MCP declaration",
                detail: "expected exactly one url or command, with bounded literal arguments"
                    .into(),
            });
        }
        Ok(())
    }
}

impl StockSbx {
    /// Reuse a matching registration, or register a missing definition without
    /// interactive authentication. Never replace a different host registration.
    ///
    /// # Errors
    /// Fails closed on inspect errors, conflicting definitions, add errors or timeouts.
    pub fn ensure_mcp_server(&self, server: &McpServer) -> Result<(), SbxError> {
        server.validate()?;
        let output = self.run_bounded_os(
            "inspect environment MCP server",
            self.preparation_timeouts.sbx,
            ["mcp", "inspect", &server.name, "--json"],
        )?;
        if output.succeeded() {
            let value: serde_json::Value =
                serde_json::from_slice(&output.stdout).map_err(|_| SbxError::LifecycleSchema {
                    operation: "inspect environment MCP server",
                    detail: "invalid MCP inspect JSON".into(),
                })?;
            let expected_command = std::iter::once(server.command.clone())
                .chain(server.args.clone())
                .collect::<Vec<_>>();
            let matches = value.get("name").and_then(serde_json::Value::as_str)
                == Some(server.name.as_str())
                && if server.command.is_empty() {
                    value.get("url").and_then(serde_json::Value::as_str)
                        == Some(server.url.as_str())
                        || value
                            .get("registry_url")
                            .and_then(serde_json::Value::as_str)
                            == Some(server.url.as_str())
                        || value.get("image").and_then(serde_json::Value::as_str)
                            == Some(server.url.as_str())
                } else {
                    value.get("command") == Some(&serde_json::json!(expected_command))
                };
            if !matches {
                return Err(SbxError::LifecycleSchema {
                    operation: "environment MCP registration",
                    detail: format!(
                        "{} already has a different host-global definition; choose another name or explicitly update it with sbx mcp",
                        server.name
                    ),
                });
            }
            return Ok(());
        }
        let missing = String::from_utf8_lossy(&output.stderr).contains("mcp server not found");
        if !missing {
            return Self::require_success("inspect environment MCP server", output);
        }
        let mut args = vec!["mcp".into(), "add".into(), server.name.clone()];
        if server.command.is_empty() {
            args.extend(["--url".into(), server.url.clone(), "--skip-auth".into()]);
        } else {
            args.extend(["--command".into(), server.command.clone()]);
            if !server.args.is_empty() {
                // SBX's --args is a CSV string slice, not whitespace splitting.
                let csv = server
                    .args
                    .iter()
                    .map(|argument| format!("\"{}\"", argument.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(",");
                args.extend(["--args".into(), csv]);
            }
        }
        Self::require_success(
            "register environment MCP server",
            self.run_bounded_os(
                "register environment MCP server",
                self.preparation_timeouts.sbx,
                args,
            )?,
        )
    }

    /// Load a registered server into an exactly-owned shell VM.
    ///
    /// # Errors
    /// Rejects invalid names, foreign/quarantined VMs and failed stock loads.
    pub fn load_shell_mcp_server(
        &self,
        ready: &ReadyShellVm,
        server: &str,
    ) -> Result<(), SbxError> {
        validate_name(server)?;
        self.reject_quarantined_shell(&ready.name)?;
        match self.classify_vm(&ready.name, false)? {
            VmClass::Owned { .. } => {}
            VmClass::Absent | VmClass::Foreign => {
                return Err(SbxError::ForeignVm(ready.name.clone()));
            }
        }
        Self::require_success(
            "load shell environment MCP server",
            self.run_bounded_os(
                "load shell environment MCP server",
                self.preparation_timeouts.sbx,
                ["mcp", "load", server, "--sandbox", &ready.name],
            )?,
        )
    }

    /// Unload a server from an exactly-owned shell VM.
    ///
    /// # Errors
    /// Rejects invalid names, foreign/quarantined VMs and failed stock unloads.
    pub fn unload_shell_mcp_server(
        &self,
        ready: &ReadyShellVm,
        server: &str,
    ) -> Result<(), SbxError> {
        validate_name(server)?;
        self.reject_quarantined_shell(&ready.name)?;
        match self.classify_vm(&ready.name, false)? {
            VmClass::Owned { .. } => {}
            VmClass::Absent | VmClass::Foreign => {
                return Err(SbxError::ForeignVm(ready.name.clone()));
            }
        }
        Self::require_success(
            "unload shell environment MCP server",
            self.run_bounded_os(
                "unload shell environment MCP server",
                self.preparation_timeouts.sbx,
                ["mcp", "unload", server, "--sandbox", &ready.name],
            )?,
        )
    }
}
