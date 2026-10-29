//! Automatic recovery of quarantined Kit VMs (`docs/troubleshooting.md`).
//!
//! A quarantined VM is retired, as `marsh workers reset` would, once no job
//! record on it is unfinished and the adapter verified that no container can
//! still be running there (the VM is gone or stopped, or its Docker lists no
//! running container). Uncertain receipts stay uncertain; nothing is
//! replayed. When verification is impossible the VM stays quarantined and
//! `marsh status` says why (`WorkerQuarantine::recovery`).

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use marsh_daemon::{
    DaemonError, DaemonStore, WorkerHealth, WorkerQuarantine, WorkerRecovery, WorkerStatus,
};
use marsh_sbx::KitVmSpec;

use super::{StockDaemonBackend, backend_error, unix_millis};

impl StockDaemonBackend {
    /// Every Kit VM marsh refuses new jobs for: the adapter's quarantines and
    /// the store's quarantined workers.
    fn quarantined_vm_names(&self, store: &DaemonStore) -> BTreeSet<String> {
        let mut names = self
            .sbx
            .quarantined_vms()
            .into_keys()
            .collect::<BTreeSet<_>>();
        names.extend(
            store
                .status(None)
                .workers
                .into_iter()
                .filter(|worker| worker.health == WorkerHealth::Quarantined)
                .map(|worker| worker.vm_id),
        );
        names
    }

    pub(super) fn vm_quarantined(&self, store: &DaemonStore, vm: &str) -> bool {
        self.sbx.quarantined_vms().contains_key(vm) || store.worker_quarantine(vm).is_some()
    }

    /// The commands whose Kit VM is `vm`, and its spec when one names it.
    fn spec_for_vm(&self, vm: &str) -> (Vec<String>, Option<KitVmSpec>) {
        let mut commands = Vec::new();
        let mut found = None;
        for command in self.command_names() {
            if let Ok(spec) = self.kit_spec(&command)
                && spec.name == vm
            {
                commands.push(command);
                found.get_or_insert(spec);
            }
        }
        if found.is_none() {
            found = self
                .draining_kits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(vm)
                .map(|(command, spec)| {
                    commands.push(command.clone());
                    spec.clone()
                })
                .or_else(|| {
                    self.remembered_ephemeral_kits()
                        .into_iter()
                        .find(|spec| spec.name == vm)
                });
        }
        (commands, found)
    }

    /// The recorded quarantine of `vm`: the store's (a job's end could not be
    /// verified) or else the adapter's (a VM-level failure).
    fn quarantine_record(&self, store: &DaemonStore, vm: &str) -> WorkerQuarantine {
        if let Some(quarantine) = store.worker_quarantine(vm)
            && !quarantine.reason.is_empty()
        {
            return quarantine;
        }
        self.sbx
            .quarantined_vms()
            .get(vm)
            .map(|quarantine| WorkerQuarantine {
                reason: quarantine.reason.clone(),
                since: quarantine.since_unix_ms,
                ..WorkerQuarantine::default()
            })
            .unwrap_or_default()
    }

    /// Why automatic recovery last left `vm` quarantined.
    pub(super) fn recovery_note(&self, vm: &str) -> Option<String> {
        self.quarantine_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(vm)
            .cloned()
    }

    fn note_recovery(&self, store: &DaemonStore, vm: &str, why: Option<String>) {
        store.set_quarantine_recovery(vm, why.clone());
        let mut notes = self
            .quarantine_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match why {
            Some(why) => notes.insert(vm.to_owned(), why),
            None => notes.remove(vm),
        };
    }

    /// Try to retire every quarantined Kit VM (the background pass).
    pub(super) fn recover_quarantined(&self, store: &DaemonStore) {
        for vm in self.quarantined_vm_names(store) {
            let _ = self.recover_quarantined_vm(store, &vm);
        }
    }

    /// Retire one quarantined Kit VM if it is verifiably safe to. `Ok` is the
    /// recorded recovery; `Err` says why the VM stays quarantined. The caller
    /// must not hold the Kit's preparation lock.
    pub(super) fn recover_quarantined_vm(
        &self,
        store: &DaemonStore,
        vm: &str,
    ) -> Result<WorkerRecovery, String> {
        if !self.vm_quarantined(store, vm) {
            return Err("it is not quarantined".into());
        }
        let (commands, spec) = self.spec_for_vm(vm);
        let lock = spec.as_ref().and_then(|spec| self.kit_lock(spec).ok());
        let _guard = lock.as_ref().map(|lock| {
            lock.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let outcome = (|| {
            if store.worker_has_unfinished_jobs(vm) {
                return Err("a job on it has not finished yet".to_owned());
            }
            let record = self.quarantine_record(store, vm);
            let verified = self.sbx.verify_no_running_containers(vm)?;
            self.retire_recovered(store, vm, spec.as_ref())
                .map_err(|error| format!("its VM could not be removed: {error}"))?;
            let mut kits = commands.clone();
            kits.extend(store.uncertain_job_commands(vm));
            kits.sort();
            kits.dedup();
            // Its uncertain jobs' slots end with the VM, as after a reset.
            store.release_process_slots(&kits);
            let recovery = WorkerRecovery {
                vm_id: vm.to_owned(),
                kits,
                reason: record.reason,
                since: record.since,
                jobs: record.jobs,
                verified,
                retired: unix_millis(std::time::SystemTime::now()),
            };
            eprintln!(
                "marshd: retired quarantined Kit VM {vm} ({}); quarantined because {}; verified: {}",
                recovery.kits.join(","),
                recovery.reason,
                recovery.verified
            );
            store.record_worker_recovery(recovery.clone());
            Ok(recovery)
        })();
        match &outcome {
            Ok(_) => self.note_recovery(store, vm, None),
            Err(why) => self.note_recovery(store, vm, Some(why.clone())),
        }
        outcome
    }

    fn retire_recovered(
        &self,
        store: &DaemonStore,
        vm: &str,
        spec: Option<&KitVmSpec>,
    ) -> Result<(), DaemonError> {
        if let Some(spec) = spec {
            let claim = Self::claim_retirement(store, vec![vm.to_owned()])?;
            return self.retire_claimed(
                store,
                &BTreeMap::from([(vm.to_owned(), spec.clone())]),
                &claim,
            );
        }
        // No command names this VM (a failed install, an update's new image).
        self.sbx.retire_quarantined_vm(vm).map_err(backend_error)?;
        self.ready_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, ready| ready.name != vm);
        self.registered_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        self.configured_mcp_vms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        if store.begin_workers_reset(&[vm.to_owned()]).is_ok() {
            store.finish_worker_reset(vm, true)?;
        }
        Ok(())
    }

    /// Before a job prepares `spec`'s VM: recover it if it is quarantined.
    /// `Some(Ok)` replaced it, `Some(Err)` says why it stays quarantined.
    pub(super) fn recover_before_prepare(
        &self,
        spec: &KitVmSpec,
        store: &DaemonStore,
    ) -> Option<Result<WorkerRecovery, String>> {
        if !self.vm_quarantined(store, &spec.name) {
            return None;
        }
        // A quarantine just published by a sibling's lost transport: give
        // its executors a moment to record their jobs before deciding.
        let deadline = Instant::now() + RECOVERY_JOB_WAIT;
        while store.worker_has_unfinished_jobs(&spec.name) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
        }
        Some(self.recover_quarantined_vm(store, &spec.name))
    }

    /// `marsh workers reset all`: Kit VMs quarantined by the adapter that no
    /// command names any more (a failed `kit install`) are retired too.
    pub(super) fn retire_unnamed_quarantined(
        &self,
        store: &DaemonStore,
    ) -> Result<(), DaemonError> {
        for vm in self.sbx.quarantined_vms().into_keys() {
            if self.spec_for_vm(&vm).1.is_some() || store.worker_has_unfinished_jobs(&vm) {
                continue;
            }
            self.retire_recovered(store, &vm, None)?;
            self.note_recovery(store, &vm, None);
        }
        Ok(())
    }

    /// `marsh status`: quarantines the adapter holds (with their reasons)
    /// on the store's workers, and VMs that are not registered workers.
    pub(super) fn annotate_quarantines(&self, workers: &mut Vec<WorkerStatus>, scope_id: &str) {
        let notes = self
            .quarantine_recovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for (vm, quarantine) in self.sbx.quarantined_vms() {
            let record = WorkerQuarantine {
                reason: quarantine.reason.clone(),
                since: quarantine.since_unix_ms,
                ..WorkerQuarantine::default()
            };
            if let Some(worker) = workers.iter_mut().find(|worker| worker.vm_id == vm) {
                worker.health = WorkerHealth::Quarantined;
                worker.warm = false;
                worker.quarantine.get_or_insert(record);
            } else {
                let (kits, spec) = self.spec_for_vm(&vm);
                workers.push(WorkerStatus {
                    worker_id: vm.clone(),
                    vm_id: vm.clone(),
                    scope_id: scope_id.to_owned(),
                    kit_ref: spec
                        .map(|spec| spec.workload_kit.identity().to_owned())
                        .unwrap_or_default(),
                    kits,
                    warm: false,
                    health: WorkerHealth::Quarantined,
                    container_capacity: marsh_contracts::WORKER_CONTAINER_CAPACITY,
                    active_container_ids: Vec::new(),
                    quarantine: Some(record),
                });
            }
        }
        for worker in workers.iter_mut() {
            if worker.health == WorkerHealth::Quarantined {
                let quarantine = worker
                    .quarantine
                    .get_or_insert_with(WorkerQuarantine::default);
                if quarantine.recovery.is_none() {
                    quarantine.recovery = notes.get(&worker.vm_id).cloned();
                }
            }
        }
    }
}

/// How long a pre-job recovery waits for a sibling job's executor to record
/// the end of its job on the same VM.
const RECOVERY_JOB_WAIT: Duration = Duration::from_secs(20);
