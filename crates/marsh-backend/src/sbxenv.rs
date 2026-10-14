//! Optional native SBX environment additions, outside the guest home.
//! Only MCP and additional workspaces are accepted: marsh owns VM lifecycle,
//! the workload, primary workspace and HOME. Files are snapshotted at startup.

use crate::{StockDaemonBackend, backend_error, identity_digest};
use marsh_contracts::MountAccess;
use marsh_daemon::{DaemonError, SessionSpec};
use marsh_sbx::{AdmittedHostGrant, ReadyKitVm};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_MOUNTS: usize = 16;
const MAX_SERVERS: usize = 32;

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Environment {
    pub mounts: Vec<Workspace>,
    pub servers: Vec<marsh_sbx::McpServer>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RegisteredKit, tests::backend_for_kit_identity};
    use marsh_sbx::NativeKitRef;

    fn backend(root: &Path) -> StockDaemonBackend {
        backend_for_kit_identity(
            root,
            BTreeMap::from([
                (
                    "pi".into(),
                    RegisteredKit {
                        workload: NativeKitRef::immutable_oci(format!(
                            "example/pi@sha256:{}",
                            "a".repeat(64)
                        ))
                        .unwrap(),
                    },
                ),
                (
                    "codex".into(),
                    RegisteredKit {
                        workload: NativeKitRef::immutable_oci(format!(
                            "example/codex@sha256:{}",
                            "b".repeat(64)
                        ))
                        .unwrap(),
                    },
                ),
            ]),
        )
    }

    #[test]
    fn native_layers_are_additive_per_kit_and_snapshotted() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().canonicalize().unwrap();
        fs::create_dir(scope.join("sbxenvs")).unwrap();
        fs::create_dir(scope.join("reference")).unwrap();
        fs::create_dir(scope.join("notes")).unwrap();
        fs::write(
            scope.join("sbxenv.yaml"),
            "schemaVersion: '1'\nadditionalWorkspaces:\n  - path: reference\n    readOnly: true\n",
        )
        .unwrap();
        fs::write(scope.join("sbxenvs/pi.yaml"), "schemaVersion: '1'\nadditionalWorkspaces:\n  - path: ../notes\nmcp:\n  servers:\n    - name: granola\n      url: https://mcp.granola.ai/mcp\n").unwrap();
        let configured = backend(&scope).with_sbxenvs().unwrap();
        let pi = configured.environment("pi");
        assert_eq!(pi.mounts.len(), 2);
        assert_eq!(pi.mounts[0].path, scope.join("reference"));
        assert!(pi.mounts[0].read_only);
        assert_eq!(pi.mounts[1].path, scope.join("notes"));
        assert!(!pi.mounts[1].read_only);
        assert_eq!(pi.servers[0].name, "granola");
        assert_eq!(configured.environment("codex").mounts.len(), 1);
        assert_eq!(configured.environment("codex").servers, Vec::new());
        let before = configured.kit_spec("pi").unwrap().name;
        fs::write(scope.join("sbxenvs/pi.yaml"), "schemaVersion: '1'\n").unwrap();
        assert_eq!(configured.kit_spec("pi").unwrap().name, before);
        let restarted = backend(&scope).with_sbxenvs().unwrap();
        assert_ne!(restarted.kit_spec("pi").unwrap().name, before);
    }

    #[test]
    fn unsupported_fields_duplicate_layers_and_symlinks_fail_before_stock_calls() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().canonicalize().unwrap();
        let file = scope.join("sbxenv.yaml");
        for document in [
            "schemaVersion: '9'\n",
            "schemaVersion: '1'\nagent: pi\n",
            "schemaVersion: '1'\nsecrets: {}\n",
            "schemaVersion: '1'\nlifecycle: {}\n",
            "schemaVersion: '1'\nmcp:\n  servers:\n    - name: x\n      url: https://example.com/mcp\n      command: /usr/bin/printf\n",
            "schemaVersion: '1'\nadditionalWorkspaces:\n  - path: missing\n",
            "schemaVersion: '1'\nschemaVersion: '1'\n",
        ] {
            fs::write(&file, document).unwrap();
            assert!(backend(&scope).with_sbxenvs().is_err(), "{document}");
            assert_eq!(fs::read_to_string(&file).unwrap(), document);
        }
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(scope.join("missing"), &file).unwrap();
        assert!(backend(&scope).with_sbxenvs().is_err());
        fs::remove_file(&file).unwrap();
        fs::create_dir(scope.join("sbxenvs")).unwrap();
        let duplicate = "schemaVersion: '1'\nmcp:\n  servers:\n    - name: x\n      url: https://example.com/mcp\n";
        fs::write(&file, duplicate).unwrap();
        fs::write(scope.join("sbxenvs/pi.yaml"), duplicate).unwrap();
        assert!(backend(&scope).with_sbxenvs().is_err());
    }

    #[test]
    fn aliases_cannot_give_one_workload_two_vm_authorities() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().canonicalize().unwrap();
        fs::create_dir(scope.join("sbxenvs")).unwrap();
        fs::write(scope.join("sbxenvs/alias.yaml"), "schemaVersion: '1'\nmcp:\n  servers:\n    - name: x\n      url: https://example.com/mcp\n").unwrap();
        let mut configured = backend(&scope);
        configured
            .commands
            .insert("alias".into(), configured.commands["pi"].clone());
        assert!(
            configured
                .with_sbxenvs()
                .err()
                .unwrap()
                .to_string()
                .contains("aliases")
        );
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workspace {
    pub path: PathBuf,
    #[serde(default, rename = "readOnly")]
    pub read_only: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(rename = "schemaVersion")]
    schema: String,
    #[serde(default, rename = "additionalWorkspaces")]
    mounts: Vec<Workspace>,
    #[serde(default)]
    mcp: Mcp,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mcp {
    #[serde(default)]
    servers: Vec<marsh_sbx::McpServer>,
}

impl Environment {
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty() && self.servers.is_empty()
    }

    pub fn fingerprint(&self) -> String {
        identity_digest(&serde_json::to_string(self).expect("environment serializes"))
    }

    fn append_file(&mut self, path: &Path) -> Result<(), DaemonError> {
        let mut file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(invalid(path, &error.to_string())),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(invalid(path, "expected a regular file of at most 64 KiB"));
        }
        if metadata.uid() != rustix::process::getuid().as_raw()
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(invalid(
                path,
                "environment file must be owned by you and not writable by group or others",
            ));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid(path, "file exceeds 64 KiB"));
        }
        let document: Document = serde_yaml::from_slice(&bytes).map_err(|error| {
            invalid(
                path,
                &format!(
                    "{error}; supported fields: schemaVersion, mcp.servers, additionalWorkspaces"
                ),
            )
        })?;
        if document.schema != "1" {
            return Err(invalid(path, "schemaVersion must be \"1\""));
        }
        for mut mount in document.mounts {
            if mount.path.to_string_lossy().contains('$') || mount.path.starts_with("~") {
                return Err(invalid(
                    path,
                    "mount paths must be literal paths (no variable or ~ expansion)",
                ));
            }
            if mount.path.is_relative() {
                mount.path = path
                    .parent()
                    .expect("environment has parent")
                    .join(mount.path);
            }
            mount.path =
                fs::canonicalize(&mount.path).map_err(|error| invalid(path, &error.to_string()))?;
            if !mount.path.is_dir() {
                return Err(invalid(path, "additional workspace must be a directory"));
            }
            if self
                .mounts
                .iter()
                .any(|previous| overlaps(&previous.path, &mount.path))
            {
                return Err(invalid(
                    path,
                    "additional workspace paths overlap or are duplicated",
                ));
            }
            self.mounts.push(mount);
        }
        for server in document.mcp.servers {
            server.validate().map_err(backend_error)?;
            if self
                .servers
                .iter()
                .any(|previous| previous.name == server.name)
            {
                return Err(invalid(
                    path,
                    "duplicate MCP server name across environment layers",
                ));
            }
            self.servers.push(server);
        }
        if self.mounts.len() > MAX_MOUNTS || self.servers.len() > MAX_SERVERS {
            return Err(invalid(
                path,
                "at most 16 additional workspaces and 32 MCP servers per Kit",
            ));
        }
        Ok(())
    }
}

fn invalid(path: &Path, message: &str) -> DaemonError {
    DaemonError::InvalidState(format!("{}: {message}", path.display()))
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

impl StockDaemonBackend {
    /// Snapshot the optional shared file and each registered command's layer.
    /// No stock calls, registrations or mounts occur during configuration.
    ///
    /// # Errors
    /// Rejects malformed/unsupported fields, unsafe paths and conflicting server definitions.
    pub fn with_sbxenvs(mut self) -> Result<Self, DaemonError> {
        let scope = self.config.daemon_home.canonicalize()?;
        let directory = scope.join("sbxenvs");
        if let Ok(metadata) = fs::symlink_metadata(&directory)
            && (!metadata.is_dir() || metadata.file_type().is_symlink())
        {
            return Err(invalid(
                &directory,
                "expected a real directory, not a symlink",
            ));
        }
        let mut shared = Environment::default();
        shared.append_file(&scope.join("sbxenv.yaml"))?;
        self.shared_sbxenv = shared.clone();
        let mut commands = self.command_names();
        if !commands.iter().any(|name| name == "shell") {
            commands.push("shell".into());
        }
        let mut servers = BTreeMap::new();
        let mut workloads = BTreeMap::new();
        for command in commands {
            let mut environment = shared.clone();
            environment.append_file(&directory.join(format!("{command}.yaml")))?;
            if let Ok(kit) = self.registered_kit(&command) {
                let fingerprint = environment.fingerprint();
                if workloads
                    .insert(kit.workload.identity().to_owned(), fingerprint.clone())
                    .is_some_and(|previous| previous != fingerprint)
                {
                    return Err(invalid(
                        &directory,
                        "aliases of the same Kit must have identical environments",
                    ));
                }
            }
            for server in &environment.servers {
                if servers
                    .insert(server.name.clone(), server.clone())
                    .is_some_and(|previous| previous != *server)
                {
                    return Err(invalid(
                        &directory,
                        "the same host-global MCP name has different definitions across Kits",
                    ));
                }
            }
            self.sbxenvs.insert(command, environment);
        }
        self.config
            .protected_guest_roots
            .extend([scope.join("sbxenv.yaml"), directory]);
        for server in servers.values().filter(|server| !server.command.is_empty()) {
            let command = Path::new(&server.command);
            let executable = if command.is_absolute() {
                command.to_path_buf()
            } else if command.components().count() == 1 {
                std::env::var_os("PATH")
                    .and_then(|path| {
                        std::env::split_paths(&path)
                            .map(|directory| directory.join(command))
                            .find(|candidate| {
                                candidate.metadata().is_ok_and(|metadata| {
                                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                                })
                            })
                    })
                    .ok_or_else(|| {
                        invalid(&scope, "local MCP command is not installed on the host")
                    })?
            } else {
                return Err(invalid(
                    &scope,
                    "local MCP commands must be absolute paths or installed executable names",
                ));
            };
            self.config
                .protected_guest_roots
                .push(executable.canonicalize()?);
        }
        let shell = self.environment("shell");
        if !shell.is_empty() {
            self.config.shell.name = self
                .sbx
                .vm_name(
                    marsh_sbx::VmPurpose::Shell,
                    &format!("shell:sbxenv:{}", shell.fingerprint()),
                )
                .map_err(backend_error)?;
        }
        Ok(self)
    }

    pub(crate) fn environment(&self, command: &str) -> Environment {
        self.sbxenvs
            .get(command)
            .cloned()
            .unwrap_or_else(|| self.shared_sbxenv.clone())
    }

    pub(crate) fn additional_grants(
        &self,
        session: &SessionSpec,
        environment: &Environment,
    ) -> Result<Vec<AdmittedHostGrant>, DaemonError> {
        if session.ephemeral_home {
            return Ok(Vec::new());
        }
        let project = session.launch_directory.canonicalize()?;
        let home = session.home_backing.canonicalize()?;
        let sources = environment
            .mounts
            .iter()
            .map(|mount| mount.path.as_path())
            .collect::<Vec<_>>();
        marsh_daemon::reject_guest_mount_overlap(&sources, &self.config.protected_guest_roots)
            .map_err(DaemonError::InvalidState)?;
        let grants = environment.mounts.iter().map(|mount| {
            if overlaps(&mount.path, &project) || overlaps(&mount.path, &home)
                || session.guest_home.starts_with(&mount.path)
            {
                return Err(DaemonError::InvalidState(format!(
                    "extra mount {} overlaps the project or managed HOME; use a separate external directory",
                    mount.path.display())));
            }
            AdmittedHostGrant::open(mount.path.clone(), mount.path.clone(),
                if mount.read_only { MountAccess::ReadOnly } else { MountAccess::ReadWrite })
                .map_err(backend_error)
        }).collect::<Result<Vec<_>, _>>()?;
        self.sbx
            .preflight_host_grants(&grants)
            .map_err(backend_error)?;
        Ok(grants)
    }

    pub(crate) fn inherited_environment(
        &self,
        mounts: &[marsh_daemon::PublicMount],
    ) -> Environment {
        // The parent's journal, never the child's Kit configuration, determines
        // which already-authorized additional sources are inherited.
        let mut environment = Environment::default();
        for mount in mounts {
            if let Some(source) = self
                .sbxenvs
                .values()
                .flat_map(|env| &env.mounts)
                .find(|source| source.path == mount.target)
            {
                let mut source = source.clone();
                source.read_only = mount.access != "read_write";
                environment.mounts.push(source);
            }
        }
        environment
    }

    pub(crate) fn configure_kit_mcp(
        &self,
        vm: &ReadyKitVm,
        environment: &Environment,
    ) -> Result<(), DaemonError> {
        let mut configured = self
            .configured_mcp_vms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !vm.cold_started && configured.contains(&vm.name) {
            return Ok(());
        }
        for server in &environment.servers {
            self.sbx.ensure_mcp_server(server).map_err(backend_error)?;
            self.sbx
                .load_mcp_server(vm, &server.name)
                .map_err(backend_error)?;
        }
        configured.insert(vm.name.clone());
        Ok(())
    }

    pub(crate) fn configure_prepared_mcp(&self, vm: &ReadyKitVm) -> Result<(), DaemonError> {
        if !self.sbxenvs.values().any(|env| !env.servers.is_empty()) {
            return Ok(());
        }
        // Compare the captured workload against its registered authority; do not
        // re-fingerprint live source files while finishing a preparation.
        let environment = self
            .sbxenvs
            .iter()
            .find_map(|(name, environment)| {
                self.registered_kit(name)
                    .ok()
                    .filter(|kit| {
                        vm.kit_ref == kit.workload.identity()
                            || vm
                                .kit_ref
                                .strip_prefix(kit.workload.identity())
                                .is_some_and(|rest| rest.starts_with('@'))
                    })
                    .map(|_| environment.clone())
            })
            .unwrap_or_else(|| self.shared_sbxenv.clone());
        self.configure_kit_mcp(vm, &environment)
    }

    pub(crate) fn configure_shell_mcp(
        &self,
        vm: &marsh_sbx::ReadyShellVm,
        environment: &Environment,
    ) -> Result<(), DaemonError> {
        let mut configured = self
            .configured_mcp_vms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !vm.cold_started && configured.contains(&vm.name) {
            return Ok(());
        }
        for server in &environment.servers {
            self.sbx.ensure_mcp_server(server).map_err(backend_error)?;
            self.sbx
                .load_shell_mcp_server(vm, &server.name)
                .map_err(backend_error)?;
        }
        configured.insert(vm.name.clone());
        Ok(())
    }
}
