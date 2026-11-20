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

use std::sync::atomic::{AtomicU64, Ordering};

use marsh_daemon::{DaemonError, LoadSelection};
use marsh_sbx::{KitVmSpec, NativeKitRef};

use super::{StockDaemonBackend, backend_error};

/// One install that has not registered its command. Records are keyed by a
/// unique attempt id, never by the reusable command name, so retiring or
/// finishing one attempt can only ever remove that attempt's record.
#[derive(Clone, Debug)]
pub(super) struct InstallAttempt {
    pub(super) command: String,
    pub(super) reference: String,
    pub(super) vm: String,
    /// Still preparing, or finishing, its install: not retired under it.
    pub(super) running: bool,
}

/// What a worker reset retires for an unfinished install.
pub(super) struct InstallTarget {
    pub(super) id: u64,
    pub(super) command: String,
    pub(super) spec: KitVmSpec,
}

/// Holds an install's record while it runs. Dropping it settles the record
/// (the install failed; the VM stays reachable); [`Self::succeeded`] removes
/// it once the command is registered.
pub(super) struct InstallGuard<'a> {
    backend: &'a StockDaemonBackend,
    id: u64,
}

impl InstallGuard<'_> {
    pub(super) fn succeeded(self) {
        self.backend.forget_install_attempt(self.id);
    }
}

impl Drop for InstallGuard<'_> {
    fn drop(&mut self) {
        if let Some(attempt) = self
            .backend
            .install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&self.id)
        {
            attempt.running = false;
        }
    }
}

static NEXT_INSTALL_ATTEMPT: AtomicU64 = AtomicU64::new(1);

impl StockDaemonBackend {
    pub(super) fn begin_install_attempt(
        &self,
        command: &str,
        reference: &str,
        vm: &str,
    ) -> InstallGuard<'_> {
        let id = NEXT_INSTALL_ATTEMPT.fetch_add(1, Ordering::Relaxed);
        self.install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                InstallAttempt {
                    command: command.to_owned(),
                    reference: reference.to_owned(),
                    vm: vm.to_owned(),
                    running: true,
                },
            );
        InstallGuard { backend: self, id }
    }

    /// Compare-and-remove: only the record with this attempt id.
    pub(super) fn forget_install_attempt(&self, id: u64) {
        self.install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    /// Drop the records a registered command has made redundant: an install
    /// whose name is now registered (through another route) for the same
    /// Kit VM. One for another VM stays, so that VM is still reachable.
    pub(super) fn prune_registered_attempts(&self) {
        let registered = self.command_names();
        let redundant = self
            .install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, attempt)| {
                !attempt.running
                    && registered.contains(&attempt.command)
                    && self
                        .kit_spec(&attempt.command)
                        .is_ok_and(|spec| spec.name == attempt.vm)
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in redundant {
            self.forget_install_attempt(id);
        }
    }

    /// The names of the unfinished installs whose VM is `vm`: what
    /// `marsh workers reset` takes for a VM no registered command names.
    pub(super) fn install_names_for_vm(&self, vm: &str) -> Vec<String> {
        self.prune_registered_attempts();
        let mut names = self
            .install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|attempt| attempt.vm == vm)
            .map(|attempt| attempt.command.clone())
            .collect::<Vec<_>>();
        names.dedup();
        names
    }

    /// The VMs of unfinished installs a worker reset selects: those of the
    /// named installs, or of all of them for `all`. An install still running
    /// is refused when named and left alone for `all`; so is one whose
    /// create may still land (reset again once it settles: the record and the
    /// quarantine stay until the VM is retired or the create has settled).
    pub(super) fn failed_install_targets(
        &self,
        selection: &LoadSelection,
    ) -> Result<Vec<InstallTarget>, DaemonError> {
        self.prune_registered_attempts();
        let attempts = self
            .install_attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut targets = Vec::new();
        for (id, attempt) in attempts {
            let named = match selection {
                LoadSelection::All => false,
                LoadSelection::Kits(commands) => commands.contains(&attempt.command),
            };
            if !named && *selection != LoadSelection::All {
                continue;
            }
            let command = &attempt.command;
            if attempt.running {
                if named {
                    return Err(DaemonError::InvalidState(format!(
                        "`marsh kit install {command}` is still running; its VM can be reset once it fails"
                    )));
                }
                continue;
            }
            if let Some(remaining) = self.sbx.create_settling(&attempt.vm) {
                if named {
                    return Err(DaemonError::InvalidState(format!(
                        "the VM create of `marsh kit install {command}` did not report and may still land; nothing was removed: retry `marsh workers reset {command}` in {} s",
                        remaining.as_secs() + 1
                    )));
                }
                eprintln!(
                    "marshd: workers reset all: VM {} of the failed install {command} may still be created; left for the next reset",
                    attempt.vm
                );
                continue;
            }
            let workload =
                NativeKitRef::immutable_oci(attempt.reference.clone()).map_err(backend_error)?;
            let spec = self.kit_spec_for_workload(workload)?;
            if spec.name != attempt.vm {
                return Err(DaemonError::InvalidState(format!(
                    "the VM recorded for the failed install {command} changed; not removing {}",
                    spec.name
                )));
            }
            targets.push(InstallTarget {
                id,
                command: attempt.command,
                spec,
            });
        }
        Ok(targets)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        RegisteredKit,
        tests::{backend_with_stock, test_stock},
    };
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
        before_list: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        listed_id: Mutex<String>,
    }

    impl SlowCreate {
        fn new(lands: bool) -> Self {
            Self {
                lands,
                vms: Mutex::new(Vec::new()),
                removed: Mutex::new(Vec::new()),
                before_list: Mutex::new(None),
                listed_id: Mutex::new(LANDED_ID.into()),
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
                    if let Some(hook) = self.before_list.lock().unwrap().take() {
                        hook();
                    }
                    let rows = self
                        .vms
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|name| {
                            format!(
                                r#"{{"name":"{name}","id":"{}","status":"running"}}"#,
                                self.listed_id.lock().unwrap()
                            )
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

    fn other_reference() -> String {
        reference().replace(&"c".repeat(64), &"d".repeat(64))
    }

    /// A backend over [`SlowCreate`] and the failed `kit install hf`: its
    /// create timed out. `settle` is how long that create may still land.
    fn failed_install(
        lands: bool,
        settle: Duration,
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
        let backend = backend_with_stock(
            home.path(),
            std::collections::BTreeMap::default(),
            test_stock(runner.clone()).with_create_settle_for_test(settle),
        );
        let store = DaemonStore::new(home.path());
        let error = backend
            .install_command("hf".into(), reference(), Some(&store))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        (home, runner, backend, store)
    }

    fn records(backend: &crate::StockDaemonBackend) -> Vec<(String, bool)> {
        backend
            .install_attempts
            .lock()
            .unwrap()
            .values()
            .map(|attempt| (attempt.command.clone(), attempt.running))
            .collect()
    }

    fn register(backend: &crate::StockDaemonBackend, command: &str, reference: String) {
        backend.installed_commands.lock().unwrap().insert(
            command.into(),
            RegisteredKit {
                workload: marsh_sbx::NativeKitRef::immutable_oci(reference).unwrap(),
            },
        );
    }

    fn reset(
        backend: &crate::StockDaemonBackend,
        store: &DaemonStore,
        names: &[&str],
    ) -> Result<Vec<String>, marsh_daemon::DaemonError> {
        let selection = if names.is_empty() {
            LoadSelection::All
        } else {
            LoadSelection::Kits(names.iter().map(|name| (*name).to_owned()).collect())
        };
        backend.reset_workers(&selection, store.clone())
    }

    /// #194: the VM of a `kit install` whose create timed out is quarantined
    /// (right: the create may still land) but nothing registered names it.
    /// `workers reset NAME` with the install's name must clear it, and the
    /// retry's refusal must name that command. Resetting also forgets the
    /// install's record.
    #[test]
    fn reset_by_install_name_clears_the_quarantine_of_a_failed_install() {
        let (_home, _runner, backend, store) = failed_install(false, Duration::ZERO);
        assert_eq!(backend.sbx.quarantined_vms().len(), 1);
        assert_eq!(records(&backend), [("hf".to_owned(), false)]);
        let mut workers = Vec::new();
        backend.annotate_quarantines(&mut workers, "scope");
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].kits, ["hf"], "status names the install");

        assert_eq!(reset(&backend, &store, &["hf"]).unwrap(), ["hf"]);
        assert!(backend.sbx.quarantined_vms().is_empty());
        assert_eq!(
            records(&backend).len(),
            0,
            "a reset forgets what it retired"
        );
    }

    /// The retry's refusal names the command that clears the VM and promises
    /// no automatic replacement it cannot show.
    #[test]
    fn the_retry_of_a_failed_install_names_a_command_that_works() {
        let (_home, _runner, backend, store) = failed_install(false, Duration::from_mins(2));
        let retry = backend
            .install_command("hf".into(), reference(), Some(&store))
            .unwrap_err()
            .to_string();
        assert!(retry.contains("marsh workers reset hf"), "{retry}");
        assert!(retry.contains("not replaced automatically"), "{retry}");
        assert!(!retry.contains("replaces it on its own"), "{retry}");
    }

    /// The create landed after it timed out: the VM is removed, by the name
    /// recorded for the install and fenced to the UUID the daemon recorded.
    #[test]
    fn reset_by_install_name_removes_a_vm_that_landed_after_the_timeout() {
        let (_home, runner, backend, store) = failed_install(true, Duration::from_mins(2));
        assert_eq!(backend.sbx.quarantined_vms().len(), 1);

        assert_eq!(reset(&backend, &store, &["hf"]).unwrap(), ["hf"]);
        assert!(backend.sbx.quarantined_vms().is_empty());
        assert_eq!(runner.removed.lock().unwrap().len(), 1);
        assert!(runner.vms.lock().unwrap().is_empty());
    }

    #[test]
    fn reset_all_retires_the_vm_of_a_failed_install_and_reports_it() {
        let (_home, runner, backend, store) = failed_install(true, Duration::from_mins(2));
        let done = reset(&backend, &store, &[]).unwrap();
        assert!(done.contains(&"hf".to_owned()), "{done:?}");
        assert!(backend.sbx.quarantined_vms().is_empty());
        assert!(runner.vms.lock().unwrap().is_empty());
    }

    /// While an install is still running (preparing, or finishing its
    /// registration), `reset NAME` says so rather than waiting on it or
    /// pulling the VM from under it, and `reset all` leaves it alone.
    #[test]
    fn reset_leaves_an_install_that_is_still_running() {
        let (_home, runner, backend, store) = failed_install(false, Duration::from_mins(2));
        register(&backend, "fixture", other_reference());
        let _running = backend.begin_install_attempt("hf", &reference(), "unused");
        backend
            .install_attempts
            .lock()
            .unwrap()
            .values_mut()
            .for_each(|attempt| attempt.running = true);

        let error = reset(&backend, &store, &["hf"]).unwrap_err();
        assert!(error.to_string().contains("still running"), "{error}");
        assert_eq!(reset(&backend, &store, &[]).unwrap(), ["fixture"]);
        assert_eq!(backend.sbx.quarantined_vms().len(), 1);
        assert!(runner.removed.lock().unwrap().is_empty());
    }

    /// A reset by a name nothing registered and no install attempted stays
    /// an error, and never reaches stock.
    #[test]
    fn reset_of_an_unknown_name_is_still_not_found() {
        let (_home, runner, backend, store) = failed_install(false, Duration::ZERO);
        let error = reset(&backend, &store, &["nope"]).unwrap_err();
        assert!(error.to_string().contains("not found"), "{error}");
        assert!(runner.removed.lock().unwrap().is_empty());
    }

    /// GPT review: a create that did not report may land after a reset has
    /// seen the VM absent. The reset then removes nothing (it says to retry),
    /// and keeps the record, the quarantine and the intent, so that when the
    /// VM lands the next reset, by name or `all`, retires it by its UUID.
    #[test]
    fn a_create_landing_after_reset_stays_resettable() {
        for by_name in [true, false] {
            let (_home, runner, backend, store) = failed_install(false, Duration::from_mins(2));
            register(&backend, "fixture", other_reference());

            let error = reset(&backend, &store, &["hf"]).unwrap_err();
            assert!(error.to_string().contains("may still land"), "{error}");
            assert_eq!(reset(&backend, &store, &[]).unwrap(), ["fixture"]);
            assert_eq!(records(&backend).len(), 1, "the handle must survive");
            assert_eq!(backend.sbx.quarantined_vms().len(), 1);

            // The remote create finishes only now.
            let vm = backend
                .install_attempts
                .lock()
                .unwrap()
                .values()
                .next()
                .unwrap()
                .vm
                .clone();
            runner.vms.lock().unwrap().push(vm);
            let done = if by_name {
                reset(&backend, &store, &["hf"])
            } else {
                reset(&backend, &store, &[])
            }
            .unwrap();
            assert!(done.contains(&"hf".to_owned()), "{done:?}");
            assert!(runner.vms.lock().unwrap().is_empty(), "late VM survived");
            assert_eq!(runner.removed.lock().unwrap().len(), 1);
            assert_eq!(records(&backend).len(), 0);
            assert!(backend.sbx.quarantined_vms().is_empty());
        }
    }

    /// Once the create has had its settle time and still no VM is listed,
    /// the reset retires the quarantine and forgets the install.
    #[test]
    fn a_create_that_never_landed_is_retired_once_it_has_settled() {
        let (_home, runner, backend, store) = failed_install(false, Duration::ZERO);
        assert_eq!(reset(&backend, &store, &["hf"]).unwrap(), ["hf"]);
        assert_eq!(records(&backend).len(), 0);
        assert!(backend.sbx.quarantined_vms().is_empty());
        assert!(runner.removed.lock().unwrap().is_empty());
    }

    /// GPT review: resetting an older attempt must not erase the record of a
    /// newer attempt of the same name that began meanwhile.
    #[test]
    fn retiring_an_old_attempt_does_not_forget_a_new_attempt() {
        let (_home, runner, backend, store) = failed_install(false, Duration::ZERO);
        let backend = std::sync::Arc::new(backend);
        let other = other_reference();
        let other_vm = backend
            .kit_spec_for_workload(marsh_sbx::NativeKitRef::immutable_oci(other.clone()).unwrap())
            .unwrap()
            .name;
        // The reset's first stock listing comes after its snapshot of the
        // records and without `registry_install`: a retry of another Kit can
        // begin right there.
        let during_reset = backend.clone();
        *runner.before_list.lock().unwrap() = Some(Box::new(move || {
            std::mem::forget(during_reset.begin_install_attempt("hf", &other, &other_vm));
        }));
        reset(&backend, &store, &["hf"]).unwrap();
        assert_eq!(
            records(&backend),
            [("hf".to_owned(), true)],
            "the old reset erased the running retry's record"
        );
    }

    /// An install that registers its command forgets its record; a failed one
    /// (its guard dropped) keeps it, settled.
    #[test]
    fn a_record_ends_with_a_successful_install_and_settles_on_failure() {
        let (_home, _runner, backend, _store) = failed_install(false, Duration::ZERO);
        let ok = backend.begin_install_attempt("good", &other_reference(), "vm");
        assert_eq!(records(&backend).len(), 2);
        ok.succeeded();
        assert_eq!(records(&backend), [("hf".to_owned(), false)]);
        drop(backend.begin_install_attempt("bad", &other_reference(), "vm"));
        assert!(records(&backend).contains(&("bad".to_owned(), false)));
    }

    /// A record whose name is later registered for the same Kit VM through
    /// another route is dropped; one for a different VM stays until a reset
    /// retires it, and `reset NAME` then retires both.
    #[test]
    fn a_registered_name_cleans_or_keeps_its_install_record() {
        let (_home, runner, backend, store) = failed_install(false, Duration::ZERO);
        register(&backend, "hf", reference());
        let mut workers = Vec::new();
        backend.annotate_quarantines(&mut workers, "scope");
        assert_eq!(records(&backend).len(), 0, "redundant record left behind");

        let (_home, runner2, backend2, store2) = failed_install(false, Duration::ZERO);
        register(&backend2, "hf", other_reference());
        let mut workers = Vec::new();
        backend2.annotate_quarantines(&mut workers, "scope");
        assert_eq!(records(&backend2).len(), 1, "the other VM stays reachable");
        assert_eq!(reset(&backend2, &store2, &["hf"]).unwrap(), ["hf"]);
        assert_eq!(records(&backend2).len(), 0);
        assert!(backend2.sbx.quarantined_vms().is_empty());
        let _ = (runner, runner2, store);
    }
}
