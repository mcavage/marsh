//! A `kit install` that did not finish registering its command.
//!
//! The install creates (or tries to create) the Kit's VM before the command
//! exists, so no registered name leads to that VM: a create that timed out is
//! quarantined (it may still land) and `workers reset NAME` found nothing
//! under the install's name. The install's name, resolved reference and VM
//! are recorded here from the moment it starts; `workers reset NAME` and
//! `workers reset all` retire that VM through the same fenced path as a
//! registered Kit's (recorded UUID only; a VM the daemon did not record is
//! never removed).

use std::collections::BTreeMap;

use marsh_daemon::{DaemonError, LoadSelection};
use marsh_sbx::{KitVmSpec, NativeKitRef};

use super::{StockDaemonBackend, backend_error};

/// One install that has not registered its command.
#[derive(Clone, Debug)]
pub(super) struct InstallAttempt {
    pub(super) reference: String,
    pub(super) vm: String,
    /// Still preparing its VM: not retired under it.
    pub(super) running: bool,
}

impl StockDaemonBackend {
    pub(super) fn begin_install_attempt(&self, command: &str, reference: &str, vm: &str) {
        self.install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                command.to_owned(),
                InstallAttempt {
                    reference: reference.to_owned(),
                    vm: vm.to_owned(),
                    running: true,
                },
            );
    }

    /// The install stopped preparing; its record stays until it registers
    /// the command or its VM is retired.
    pub(super) fn settle_install_attempt(&self, command: &str) {
        if let Some(attempt) = self
            .install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(command)
        {
            attempt.running = false;
        }
    }

    pub(super) fn forget_install_attempt(&self, command: &str) {
        self.install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(command);
    }

    /// The names of the unfinished installs whose VM is `vm`: what
    /// `marsh workers reset` takes for a VM no registered command names.
    pub(super) fn install_names_for_vm(&self, vm: &str) -> Vec<String> {
        self.install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, attempt)| attempt.vm == vm)
            .map(|(command, _)| command.clone())
            .collect()
    }

    /// The VMs of unfinished installs a worker reset selects: the named ones
    /// that no registered command now names, or all of them for `all` (an
    /// install still running is then left alone; naming it is an error).
    pub(super) fn failed_install_specs(
        &self,
        selection: &LoadSelection,
    ) -> Result<BTreeMap<String, KitVmSpec>, DaemonError> {
        let registered = self.command_names();
        let attempts = self
            .install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut specs = BTreeMap::new();
        for (command, attempt) in attempts {
            let named = match selection {
                LoadSelection::All => false,
                LoadSelection::Kits(commands) => commands.contains(&command),
            };
            if registered.contains(&command) || !(named || *selection == LoadSelection::All) {
                continue;
            }
            if attempt.running {
                if named {
                    return Err(DaemonError::InvalidState(format!(
                        "`marsh kit install {command}` is still running; its VM can be reset once it fails"
                    )));
                }
                continue;
            }
            let workload = NativeKitRef::immutable_oci(attempt.reference).map_err(backend_error)?;
            specs.insert(command, self.kit_spec_for_workload(workload)?);
        }
        Ok(specs)
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::backend_with_runner;
    use marsh_daemon::{DaemonBackend, DaemonStore, LoadSelection};
    use marsh_runtime::{Attachment, CommandOutput, CommandRunner, Invocation};
    use std::{io, os::unix::fs::PermissionsExt, sync::Mutex, time::Duration};

    const LANDED_ID: &str = "11111111-2222-4333-8444-555555555555";

    /// A stock SBX whose `create` does not report in time, as a hung or slow
    /// pull does. With `lands`, the VM then appears under its name anyway.
    struct SlowCreate {
        lands: bool,
        vms: Mutex<Vec<String>>,
        removed: Mutex<Vec<String>>,
    }

    impl SlowCreate {
        fn new(lands: bool) -> Self {
            Self {
                lands,
                vms: Mutex::new(Vec::new()),
                removed: Mutex::new(Vec::new()),
            }
        }

        fn ok(stdout: &str) -> CommandOutput {
            CommandOutput {
                exit_code: Some(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            }
        }
    }

    impl CommandRunner for SlowCreate {
        fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
            self.run_bounded(invocation, Duration::from_secs(1))
        }

        fn run_bounded(&self, invocation: &Invocation, _: Duration) -> io::Result<CommandOutput> {
            let arguments = invocation
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            match arguments.first().map(String::as_str) {
                Some("ls") => {
                    let rows = self
                        .vms
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|name| {
                            format!(r#"{{"name":"{name}","id":"{LANDED_ID}","status":"running"}}"#)
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    Ok(Self::ok(&format!(r#"{{"sandboxes":[{rows}]}}"#)))
                }
                Some("create") => {
                    let name = arguments
                        .iter()
                        .position(|argument| argument == "--name")
                        .map(|index| arguments[index + 1].clone())
                        .unwrap();
                    if self.lands {
                        self.vms.lock().unwrap().push(name);
                    }
                    Err(io::Error::new(io::ErrorKind::TimedOut, "create hung"))
                }
                Some("rm") => {
                    let name = arguments.last().unwrap().clone();
                    self.vms.lock().unwrap().retain(|vm| *vm != name);
                    self.removed.lock().unwrap().push(name);
                    Ok(Self::ok(""))
                }
                _ => Err(io::Error::other(format!(
                    "unexpected stock call {arguments:?}"
                ))),
            }
        }

        fn spawn_attached(&self, _: &Invocation) -> io::Result<Attachment> {
            Err(io::Error::other("unexpected attachment"))
        }
    }

    fn reference() -> String {
        format!("example/hf@sha256:{}", "c".repeat(64))
    }

    fn failed_install(
        lands: bool,
    ) -> (
        tempfile::TempDir,
        std::sync::Arc<SlowCreate>,
        crate::StockDaemonBackend,
        DaemonStore,
    ) {
        let home =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        for artifact in [
            "worker",
            "relay",
            "shell",
            "marsh-byte-exec-linux-arm64",
            "marsh-local-linux-arm64",
        ] {
            let path = home.path().join(artifact);
            std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let runner = std::sync::Arc::new(SlowCreate::new(lands));
        let backend = backend_with_runner(
            home.path(),
            std::collections::BTreeMap::default(),
            runner.clone(),
        );
        let store = DaemonStore::new(home.path());
        let error = backend
            .install_command("hf".into(), reference(), Some(&store))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        (home, runner, backend, store)
    }

    /// #194: the VM of a `kit install` whose create timed out is quarantined
    /// (right: the create may still land) but nothing registered names it.
    /// `workers reset NAME` with the install's name must clear it, and the
    /// retry's refusal must name that command.
    #[test]
    fn reset_by_install_name_clears_the_quarantine_of_a_failed_install() {
        let (_home, _runner, backend, store) = failed_install(false);
        assert_eq!(backend.sbx.quarantined_vms().len(), 1);
        let mut workers = Vec::new();
        backend.annotate_quarantines(&mut workers, "scope");
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].kits, ["hf"], "status names the install");

        let retry = backend
            .install_command("hf".into(), reference(), Some(&store))
            .unwrap_err()
            .to_string();
        assert!(retry.contains("marsh workers reset hf"), "{retry}");

        let done = backend
            .reset_workers(&LoadSelection::Kits(vec!["hf".into()]), store.clone())
            .unwrap();
        assert_eq!(done, ["hf"]);
        assert!(backend.sbx.quarantined_vms().is_empty());
    }

    /// The create landed after it timed out: the VM is removed, by the name
    /// recorded for the install and fenced to the UUID the daemon recorded.
    #[test]
    fn reset_by_install_name_removes_a_vm_that_landed_after_the_timeout() {
        let (_home, runner, backend, store) = failed_install(true);
        assert_eq!(backend.sbx.quarantined_vms().len(), 1);

        let done = backend
            .reset_workers(&LoadSelection::Kits(vec!["hf".into()]), store.clone())
            .unwrap();
        assert_eq!(done, ["hf"]);
        assert!(backend.sbx.quarantined_vms().is_empty());
        assert_eq!(runner.removed.lock().unwrap().len(), 1);
        assert!(runner.vms.lock().unwrap().is_empty());
    }

    #[test]
    fn reset_all_retires_the_vm_of_a_failed_install_and_reports_it() {
        let (_home, runner, backend, store) = failed_install(true);
        let done = backend
            .reset_workers(&LoadSelection::All, store.clone())
            .unwrap();
        assert!(done.contains(&"hf".to_owned()), "{done:?}");
        assert!(backend.sbx.quarantined_vms().is_empty());
        assert!(runner.vms.lock().unwrap().is_empty());
    }

    /// While an install is still preparing its VM, `reset NAME` says so
    /// rather than waiting on it or pulling the VM from under it, and
    /// `reset all` leaves it alone.
    #[test]
    fn reset_leaves_an_install_that_is_still_running() {
        let (_home, runner, backend, store) = failed_install(false);
        backend.begin_install_attempt("hf", &reference(), "unused");

        let error = backend
            .reset_workers(&LoadSelection::Kits(vec!["hf".into()]), store.clone())
            .unwrap_err();
        assert!(error.to_string().contains("still running"), "{error}");
        assert!(
            backend
                .reset_workers(&LoadSelection::All, store.clone())
                .is_err_and(|error| error.to_string().contains("cannot be empty"))
        );
        assert_eq!(backend.sbx.quarantined_vms().len(), 1);
        assert!(runner.removed.lock().unwrap().is_empty());
    }

    /// A reset by a name nothing registered and no install attempted stays
    /// an error, and never reaches stock.
    #[test]
    fn reset_of_an_unknown_name_is_still_not_found() {
        let (_home, runner, backend, store) = failed_install(false);
        let error = backend
            .reset_workers(&LoadSelection::Kits(vec!["nope".into()]), store)
            .unwrap_err();
        assert!(error.to_string().contains("not found"), "{error}");
        assert!(runner.removed.lock().unwrap().is_empty());
    }
}
