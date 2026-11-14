//! `marsh workers stop KIT[,KIT...]|all` (docs/kits.md "Stopping Kit VMs"):
//! stop idle Kit VMs in stock SBX, keeping each VM and its disk; the Kit's
//! next job, or `marsh --load KIT`, starts the same VM again.
//!
//! A stop never ends running work and never waits for it: a VM with running
//! or starting jobs, or pinned by an attached shell, is refused with the
//! count, and nothing about it changes. An idle VM is claimed exactly as an
//! automatic retirement claims it (`automatic_claim`: no reservation, start,
//! unfinished or cleanup-uncertain job, under the Kit lock), so no job is
//! admitted to it while `sbx stop` runs under the VM's transition fence.
//! A stopped VM is not registered as a worker; `marsh status` lists it as
//! `stopped`. Garbage collection still retires it once it has gone unused
//! for `vm-idle-days`; it is never superseded or uncertain for being stopped.

use super::{KitFlightCompletion, StockDaemonBackend, backend_error};
use marsh_contracts::WORKER_CONTAINER_CAPACITY;
use marsh_daemon::{
    DaemonError, DaemonStore, LoadSelection, WorkerHealth, WorkerStatus, WorkerStopEntry,
    WorkersStopReport,
};
use marsh_sbx::{KitVmSpec, KitVmStop};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const STOP_TIMEOUT: Duration = Duration::from_mins(2);

impl StockDaemonBackend {
    pub(super) fn stop_selected_workers(
        &self,
        selection: &LoadSelection,
        store: &DaemonStore,
    ) -> Result<WorkersStopReport, DaemonError> {
        let mut selected = match selection {
            LoadSelection::All => self.command_names(),
            LoadSelection::Kits(commands) => commands.clone(),
        };
        selected.sort();
        selected.dedup();
        if selected.is_empty() {
            return Err(DaemonError::InvalidState(
                "worker stop selection cannot be empty".into(),
            ));
        }
        if selected.iter().any(|command| command == "cloud") {
            return Err(DaemonError::InvalidState(
                "cloud sandboxes are not stopped this way; `marsh workers reset cloud` removes them"
                    .into(),
            ));
        }
        // Resolve every name before stopping anything. Names that run the
        // same Kit share one VM.
        let mut vms = BTreeMap::<String, (KitVmSpec, Vec<String>)>::new();
        for command in &selected {
            let spec = self.kit_spec(command)?;
            vms.entry(spec.name.clone())
                .or_insert_with(|| (spec, Vec::new()))
                .1
                .push(command.clone());
        }
        Ok(WorkersStopReport {
            entries: vms
                .into_values()
                .map(|(spec, kits)| self.stop_one_worker(store, &spec, &kits))
                .collect(),
        })
    }

    #[allow(clippy::too_many_lines)] // One stop: refusals, claim, stop, and what it settled.
    fn stop_one_worker(
        &self,
        store: &DaemonStore,
        spec: &KitVmSpec,
        kits: &[String],
    ) -> WorkerStopEntry {
        let label = kits.join(",");
        let entry = |action: &str, running_jobs: usize, message: String| WorkerStopEntry {
            kits: kits.to_vec(),
            vm: spec.name.clone(),
            action: action.into(),
            running_jobs,
            message,
        };
        // Under the Kit lock no preparation of this Kit runs: the claim below
        // then holds off every new job until the stop is done.
        let lock = match self.kit_lock(spec) {
            Ok(lock) => lock,
            Err(error) => return entry("failed", 0, format!("{label}: {error}")),
        };
        let _guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let jobs = store.worker_job_count(&spec.name);
        if jobs > 0 {
            return entry(
                "refused",
                jobs,
                format!(
                    "{label} has {jobs} running job{}; not stopped (nothing was cancelled; stop it once {} ended)",
                    if jobs == 1 { "" } else { "s" },
                    if jobs == 1 { "it has" } else { "they have" }
                ),
            );
        }
        if self.vm_quarantined(store, &spec.name) {
            return entry(
                "refused",
                0,
                format!(
                    "{label}'s VM is quarantined; not stopped (`marsh status` says why; `marsh workers reset {}` clears it)",
                    kits.first().map_or("all", String::as_str)
                ),
            );
        }
        let live = store.attached_session_ids();
        if self.sbx.kit_vm_in_use(&spec.name, &live) {
            return entry(
                "refused",
                0,
                format!(
                    "{label}'s VM is held by a job's mounts or by an attached shell that ran {label}; not stopped (exit that shell, or retry once the job ended)"
                ),
            );
        }
        let Some(claim) = self.automatic_claim(store, &spec.name) else {
            return entry(
                "refused",
                0,
                format!(
                    "{label}: a job may be starting, or a job's cleanup is not verified yet; not stopped (retry)"
                ),
            );
        };
        let stopped =
            self.sbx
                .stop_idle_kit_vm(&spec.name, &claim.live, Instant::now() + STOP_TIMEOUT);
        // Whatever happened, the worker lease is gone: the next job prepares
        // the VM afresh (and starts it if it is stopped).
        self.forget_prepared_worker(spec);
        if !claim.worker_ids.is_empty() {
            match &stopped {
                // A stopped VM is no registered worker; its next job
                // registers it again.
                Ok(_) => {
                    if let Err(error) = store.finish_worker_reset(&spec.name, true) {
                        return entry("failed", 0, format!("{label}: {error}"));
                    }
                }
                // A stop that did not settle: the VM is retired for good
                // (durably) and its removal is retried; the claim stays.
                Err(marsh_sbx::SbxError::QuarantinedVm(_)) => {}
                Err(_) => store.cancel_workers_reset(&claim.worker_ids),
            }
        }
        match stopped {
            Ok(KitVmStop::Stopped) => entry(
                "stopped",
                0,
                format!(
                    "stopped {label}; its next job starts the same VM (or `marsh --load {label}`)"
                ),
            ),
            Ok(KitVmStop::AlreadyStopped) => {
                entry("already_stopped", 0, format!("{label} is already stopped"))
            }
            Ok(KitVmStop::Absent) => entry("absent", 0, format!("{label} has no VM yet")),
            Ok(KitVmStop::Retired) => entry(
                "removed",
                0,
                format!(
                    "{label}: the stop did not settle, so its VM was removed instead; the next call creates a new one"
                ),
            ),
            Err(marsh_sbx::SbxError::ActiveWorkerState(_)) => entry(
                "refused",
                0,
                format!("{label}: a job started on it meanwhile; not stopped"),
            ),
            Err(marsh_sbx::SbxError::QuarantinedVm(why)) => entry(
                "failed",
                0,
                format!("{label}: not stopped: VM {why}; the next call creates a new VM"),
            ),
            Err(error) => entry(
                "failed",
                0,
                format!("{label}: not stopped: {}", backend_error(error)),
            ),
        }
    }

    /// Remove the VMs the ownership map marks retired for good (a stop that
    /// did not settle, in this or an earlier daemon run, or a VM the removed
    /// `marsh --dev` left). Each background
    /// pass, starting when the daemon starts.
    pub(super) fn remove_retired_vms(&self) {
        let (removed, failed) = self.sbx.remove_retired_kit_vms();
        for vm in removed {
            self.forget_retired(&vm);
            eprintln!("marshd: removed retired VM {vm}");
        }
        for (vm, error) in failed {
            eprintln!("marshd: retired VM {vm} not removed yet: {error}");
        }
    }

    fn forget_retired(&self, vm: &str) {
        self.ready_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, ready| ready.name != vm);
        self.registered_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
    }

    /// Drop what this daemon run cached about a prepared Kit VM, so the next
    /// job validates it again (and starts it when stopped).
    fn forget_prepared_worker(&self, spec: &KitVmSpec) {
        self.ready_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, ready| ready.name != spec.name);
        self.registered_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&spec.name);
        if let Ok(key) = self.kit_flight_key(spec, false) {
            self.kit_validation_flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(key)
                .and_modify(|completion| {
                    completion.epoch = completion.epoch.saturating_add(1);
                    completion.error = None;
                })
                .or_insert(KitFlightCompletion {
                    epoch: 1,
                    error: None,
                });
        }
    }

    /// `marsh status`: a registered command's Kit VM that stock lists stopped
    /// and this daemon does not serve is a `stopped` worker.
    pub(super) fn annotate_stopped(&self, workers: &mut Vec<WorkerStatus>, scope_id: &str) {
        let served = workers
            .iter()
            .map(|worker| worker.vm_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let stopped = self.sbx.stopped_kit_vms(&served);
        if stopped.is_empty() {
            return;
        }
        let mut by_vm = BTreeMap::<String, (String, Vec<String>)>::new();
        for command in self.command_names() {
            if let Ok(spec) = self.kit_spec(&command)
                && stopped.contains(&spec.name)
            {
                by_vm
                    .entry(spec.name.clone())
                    .or_insert_with(|| (spec.workload_kit.identity().to_owned(), Vec::new()))
                    .1
                    .push(command);
            }
        }
        for (vm, (kit_ref, kits)) in by_vm {
            if workers.iter().any(|worker| worker.vm_id == vm) {
                continue;
            }
            workers.push(WorkerStatus {
                worker_id: vm.clone(),
                vm_id: vm,
                scope_id: scope_id.to_owned(),
                kit_ref,
                kits,
                warm: false,
                health: WorkerHealth::Stopped,
                container_capacity: WORKER_CONTAINER_CAPACITY,
                active_container_ids: Vec::new(),
                quarantine: None,
            });
        }
    }
}
