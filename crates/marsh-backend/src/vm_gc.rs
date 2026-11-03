//! Garbage collection of this daemon's own VMs (`marsh gc`,
//! docs/troubleshooting.md "Removing VMs"). Only VMs this daemon's ownership
//! map records, with their recorded UUID, are ever removed; a `marsh-` VM it
//! does not record is listed as not ours and left alone.
//!
//! Deliberately narrow: a recorded VM is removed only when stock SBX lists it
//! **stopped** (so no container of this or an earlier daemon runs in it), it
//! is not quarantined, no job recorded on it is unfinished or
//! cleanup-uncertain, no job reserved it, no mount or attached-shell pin holds
//! it, it is not finishing a Kit change, and it is either superseded (no
//! registered command runs its Kit any more, or a shell VM of an older
//! selector) or unused for `vm_idle_days` (default 3; the next use creates it
//! again). Every condition is checked again where it is enforced: the store
//! claims the worker atomically (`DaemonStore::begin_automatic_retire`), and
//! marsh-sbx lists the VM afresh under its transition fence before removing
//! it. A running VM is never removed here; `marsh reset` or `marsh stop` does
//! that. Passes run when the daemon starts, then hourly.

use super::{StockDaemonBackend, backend_error, unix_millis};
use marsh_daemon::{DaemonError, DaemonStore, VmGcEntry, VmGcReport, VmGcStatus};
use marsh_sbx::{RecordedVm, VmPurpose};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant, SystemTime},
};

const RECORD_FILE: &str = "vm-gc.json";
/// Key in `$MARSH_HOME/config.json`, written by `marsh config vm-idle-days`.
const CONFIG_KEY: &str = "vm_idle_days";
const DEFAULT_IDLE_DAYS: u64 = 3;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
const PASS_INTERVAL_MS: u64 = 60 * 60 * 1000;
const MAX_RECENT: usize = 20;
const MAX_RECORD_BYTES: u64 = 256 * 1024;
const REMOVAL_TIMEOUT: Duration = Duration::from_mins(5);

/// What garbage collection remembers between daemon runs: removals (for
/// `marsh status`) and when this marsh last saw each of its VMs used.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pass_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<VmGcEntry>,
    /// VM name to the last time a job or shell of this marsh used it, or the
    /// first time a pass saw it with no other record of its use.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub used: BTreeMap<String, u64>,
}

pub(crate) fn read(control_home: &Path) -> Record {
    let path = control_home.join(RECORD_FILE);
    if !fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        return Record::default();
    }
    let mut bytes = Vec::new();
    let read = fs::File::open(&path)
        .and_then(|file| file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes));
    if read.is_err() || bytes.len() as u64 > MAX_RECORD_BYTES {
        return Record::default();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn write(control_home: &Path, record: &Record) -> Result<(), DaemonError> {
    let mut bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| DaemonError::InvalidState(error.to_string()))?;
    bytes.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(control_home)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(control_home.join(RECORD_FILE))
        .map_err(|error| error.error)?;
    Ok(())
}

/// `vm_idle_days` from the scope's `config.json`: 0 is off; anything
/// unreadable is the default.
pub(crate) fn idle_days(scope_root: Option<&Path>) -> u64 {
    scope_root
        .and_then(|root| fs::read(root.join("config.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|config| config.get(CONFIG_KEY).and_then(serde_json::Value::as_u64))
        .filter(|days| *days <= 365)
        .unwrap_or(DEFAULT_IDLE_DAYS)
}

/// Unix milliseconds of an RFC 3339 time (stock's `last_used_at`).
pub(crate) fn utc_millis(text: &str) -> Option<u64> {
    u64::try_from(marsh_sbx::rfc3339_unix_nanos(text)? / 1_000_000).ok()
}

/// Why a chosen VM was not removed after all.
pub(crate) enum Removal {
    /// The enforcing check found it in use (or it started meanwhile).
    Kept(String),
    Failed(String),
}

/// What a pass knows about one recorded VM.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent facts about one VM.
pub(crate) struct Candidate {
    pub purpose: VmPurpose,
    /// A registered command's Kit, the shell VM, or a live session's.
    pub current: bool,
    /// Why something still uses it, if it does.
    pub in_use: Option<String>,
    pub quarantined: bool,
    pub running: bool,
    pub last_used_ms: Option<u64>,
}

/// How long a VM may go unused, in ms (0 is forever): `MARSH_VM_IDLE_SECONDS`
/// at daemon launch (for tests), else `idle_days`.
pub(crate) fn idle_limit_ms(environment: Option<&str>, idle_days: u64) -> u64 {
    environment
        .and_then(|seconds| seconds.trim().parse::<u64>().ok())
        .map_or(idle_days * DAY_MS, |seconds| seconds.saturating_mul(1000))
}

fn describe_idle(ms: u64) -> String {
    let (count, unit) = match ms {
        ms if ms >= DAY_MS => (ms / DAY_MS, "day"),
        ms if ms >= 60 * 60 * 1000 => (ms / (60 * 60 * 1000), "hour"),
        ms if ms >= 60_000 => (ms / 60_000, "minute"),
        ms => (ms / 1000, "second"),
    };
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

/// `Ok(reason)`: remove it; `Err(reason)`: keep it. `idle_ms` 0 keeps idle VMs.
pub(crate) fn decide(vm: &Candidate, now_ms: u64, idle_ms: u64) -> Result<String, String> {
    if vm.quarantined {
        return Err(
            "quarantined: automatic recovery or `marsh workers reset KIT` removes it".into(),
        );
    }
    if let Some(reason) = &vm.in_use {
        return Err(reason.clone());
    }
    if vm.running {
        return Err(
            "running: garbage collection removes only stopped VMs (`marsh reset` removes it)"
                .into(),
        );
    }
    if !vm.current {
        return Ok(match vm.purpose {
            VmPurpose::Kit => "superseded: no command runs this Kit any more".into(),
            VmPurpose::Shell => "superseded: an older shell VM".into(),
        });
    }
    let Some(last) = vm.last_used_ms else {
        return Err("no record of its last use yet".into());
    };
    let idle = now_ms.saturating_sub(last);
    if idle_ms > 0 && idle >= idle_ms {
        return Ok(format!(
            "unused for {}; the next use creates it again",
            describe_idle(idle)
        ));
    }
    Err("in use within its idle limit".into())
}

impl StockDaemonBackend {
    fn gc_idle_days(&self) -> u64 {
        idle_days(
            self.auto_update
                .as_ref()
                .map(|auto| auto.scope_root.as_path()),
        )
    }

    /// A job or shell of this marsh used `vm` now. Written at most every ten
    /// minutes per VM: the record is advisory.
    pub(super) fn note_vm_used(&self, vm: &str) {
        let now = unix_millis(SystemTime::now());
        let mut used = self
            .vm_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if used
            .get(vm)
            .is_some_and(|last| now.saturating_sub(*last) < 10 * 60 * 1000)
        {
            return;
        }
        used.insert(vm.to_owned(), now);
        drop(used);
        let _guard = self
            .vm_gc_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut record = read(&self.config.control_home);
        record.used.insert(vm.to_owned(), now);
        let _ = write(&self.config.control_home, &record);
    }

    /// The background pass: at the daemon's start, then hourly.
    pub(super) fn gc_pass(&self, store: &DaemonStore, now_ms: u64) {
        let due = &self.vm_gc_due_ms;
        if now_ms < due.load(std::sync::atomic::Ordering::Acquire)
            || store.lifecycle_active()
            || self.sbx.removed_executable().is_some()
        {
            return;
        }
        due.store(
            now_ms.saturating_add(PASS_INTERVAL_MS),
            std::sync::atomic::Ordering::Release,
        );
        if let Err(error) = self.collect_vms(store, now_ms, false) {
            eprintln!("marshd: VM garbage collection: {error}");
        }
    }

    /// `marsh gc [--dry-run]`.
    pub(super) fn gc_now(
        &self,
        dry_run: bool,
        store: &DaemonStore,
    ) -> Result<VmGcReport, DaemonError> {
        self.collect_vms(store, unix_millis(SystemTime::now()), dry_run)
    }

    pub(super) fn gc_status(&self) -> VmGcStatus {
        let record = read(&self.config.control_home);
        VmGcStatus {
            idle_days: self.gc_idle_days(),
            last_pass_unix_ms: record.last_pass_unix_ms,
            recent: record.recent,
        }
    }

    /// The VM names in use by registered commands and sessions now, by Kit
    /// identity or shell selector.
    fn current_vms(&self, live: &BTreeSet<String>) -> (BTreeSet<String>, BTreeSet<String>) {
        let mut kits = BTreeSet::new();
        for name in self.command_names() {
            let Some(identity) = self
                .registered_kit(&name)
                .ok()
                .and_then(|kit| kit.workload.capture_generation().ok())
                .map(|kit| kit.identity().to_owned())
            else {
                continue;
            };
            if let Some(vm) = self.sbx.recorded_vm_name(VmPurpose::Kit, &identity) {
                kits.insert(vm);
            }
        }
        let mut shells = BTreeSet::from([self.config.shell.name.clone()]);
        shells.extend(self.sbx.recorded_vm_name(VmPurpose::Shell, "dev"));
        for session in live {
            shells.extend(
                self.sbx
                    .recorded_vm_name(VmPurpose::Shell, &format!("ephemeral:{session}")),
            );
        }
        (kits, shells)
    }

    #[allow(clippy::too_many_lines)] // One pass: observe, decide, act, record.
    fn collect_vms(
        &self,
        store: &DaemonStore,
        now_ms: u64,
        dry_run: bool,
    ) -> Result<VmGcReport, DaemonError> {
        let _pass = self
            .vm_gc_pass
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idle_days = self.gc_idle_days();
        let idle_ms = idle_limit_ms(self.vm_idle_override.as_deref(), idle_days);
        let (recorded, foreign) = self.sbx.recorded_vms().map_err(backend_error)?;
        let live = store.attached_session_ids();
        let (current_kits, current_shells) = self.current_vms(&live);
        let draining = self
            .draining_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let ephemeral = self
            .remembered_ephemeral_kits()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<BTreeSet<_>>();
        let mut record = {
            let _guard = self
                .vm_gc_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            read(&self.config.control_home)
        };
        let mut entries = Vec::new();
        let mut first_seen = Vec::new();
        for vm in &recorded {
            let reported = vm.last_used_at.as_deref().and_then(utc_millis);
            let ours = record.used.get(&vm.name).copied().max(
                self.vm_used
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&vm.name)
                    .copied(),
            );
            let last_used_ms = reported.max(ours);
            if last_used_ms.is_none() {
                first_seen.push(vm.name.clone());
            }
            let in_use = self.vm_in_use(vm, store, &live, &draining, &ephemeral);
            let candidate = Candidate {
                purpose: vm.purpose,
                current: match vm.purpose {
                    VmPurpose::Kit => current_kits.contains(&vm.name),
                    VmPurpose::Shell => current_shells.contains(&vm.name),
                },
                in_use,
                quarantined: vm.quarantined || self.vm_quarantined(store, &vm.name),
                running: vm.running,
                last_used_ms,
            };
            let purpose = match vm.purpose {
                VmPurpose::Kit => "kit",
                VmPurpose::Shell => "shell",
            };
            let (action, reason) = match decide(&candidate, now_ms, idle_ms) {
                Err(reason) => ("kept", reason),
                Ok(reason) if dry_run => ("would_remove", reason),
                Ok(reason) => match self.remove_recorded(vm, &candidate, store, &live) {
                    Ok(()) => ("removed", reason),
                    Err(Removal::Kept(why)) => ("kept", why),
                    Err(Removal::Failed(error)) => {
                        ("failed", format!("{reason}; not removed: {error}"))
                    }
                },
            };
            entries.push(VmGcEntry {
                vm: vm.name.clone(),
                purpose: purpose.into(),
                action: action.into(),
                reason,
                unix_ms: now_ms,
            });
        }
        for name in foreign {
            entries.push(VmGcEntry {
                vm: name,
                purpose: String::new(),
                action: "not_ours".into(),
                reason: "not recorded by this marsh home; never touched".into(),
                unix_ms: now_ms,
            });
        }
        if !dry_run {
            let _guard = self
                .vm_gc_lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut latest = read(&self.config.control_home);
            latest.last_pass_unix_ms = Some(now_ms);
            for name in first_seen {
                latest.used.entry(name).or_insert(now_ms);
            }
            let names = recorded
                .iter()
                .map(|vm| vm.name.as_str())
                .collect::<BTreeSet<_>>();
            latest.used.retain(|name, _| names.contains(name.as_str()));
            for entry in entries.iter().filter(|entry| entry.action == "removed") {
                latest.used.remove(&entry.vm);
                latest.recent.push(entry.clone());
            }
            let excess = latest.recent.len().saturating_sub(MAX_RECENT);
            latest.recent.drain(..excess);
            record = latest;
            write(&self.config.control_home, &record)?;
        }
        Ok(VmGcReport {
            dry_run,
            idle_days,
            entries,
        })
    }

    fn vm_in_use(
        &self,
        vm: &RecordedVm,
        store: &DaemonStore,
        live: &BTreeSet<String>,
        draining: &BTreeSet<String>,
        ephemeral: &BTreeSet<String>,
    ) -> Option<String> {
        if draining.contains(&vm.name) {
            return Some("finishing a Kit change: removed when its jobs end".into());
        }
        if ephemeral.contains(&vm.name) {
            return Some("an ephemeral-home session's Kit VM".into());
        }
        match vm.purpose {
            VmPurpose::Kit => (store.worker_busy(&vm.name)
                || self.sbx.kit_vm_in_use(&vm.name, live))
            .then(|| "a job or an attached shell uses it".into()),
            VmPurpose::Shell => self
                .sbx
                .shell_vm_in_use(&vm.name)
                .then(|| "an attached shell uses it".into()),
        }
    }

    /// Removes one VM `decide` chose. Its conditions are enforced again here,
    /// atomically: the store claim refuses a reserved, quarantined or busy
    /// worker and one with unfinished or uncertain jobs; marsh-sbx refuses it
    /// unless it is listed stopped, under the VM's transition fence.
    fn remove_recorded(
        &self,
        vm: &RecordedVm,
        candidate: &Candidate,
        store: &DaemonStore,
        live: &BTreeSet<String>,
    ) -> Result<(), Removal> {
        // A lifecycle transition (`marsh stop`) is not started under this.
        let _admission = store
            .begin_background_request(format!("the garbage collection of VM {}", vm.name))
            .map_err(|error| Removal::Kept(error.to_string()))?;
        let deadline = Instant::now() + REMOVAL_TIMEOUT;
        if self.vm_quarantined(store, &vm.name) {
            return Err(Removal::Kept("quarantined".into()));
        }
        if vm.purpose == VmPurpose::Shell {
            let mut spec = self.config.shell.clone();
            spec.name.clone_from(&vm.name);
            return self
                .sbx
                .remove_stopped_shell_vm(&spec, deadline)
                .map(|_| ())
                .map_err(|error| Removal::Kept(error.to_string()));
        }
        // A current Kit's preparation holds its Kit lock: no job starts on
        // the VM while it is removed.
        let spec = candidate
            .current
            .then(|| self.spec_for_recorded_kit(&vm.name))
            .flatten();
        let lock = spec
            .as_ref()
            .map(|spec| self.kit_lock(spec))
            .transpose()
            .map_err(|error| Removal::Failed(error.to_string()))?;
        let _guard = lock.as_ref().map(|lock| {
            lock.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let registered = store
            .begin_automatic_retire(&vm.name)
            .map_err(|error| Removal::Kept(error.to_string()))?;
        let removed = self
            .sbx
            .remove_unused_kit_vm(&vm.name, &vm.key, live, deadline);
        if registered {
            if removed.is_ok() {
                store
                    .finish_worker_reset(&vm.name, true)
                    .map_err(|error| Removal::Failed(error.to_string()))?;
            } else {
                store.cancel_workers_reset(std::slice::from_ref(&vm.name));
            }
        }
        match removed {
            Ok(_) => {}
            Err(marsh_sbx::SbxError::ActiveWorkerState(_)) => {
                return Err(Removal::Kept("it started or is in use".into()));
            }
            Err(error) => return Err(Removal::Failed(backend_error(error).to_string())),
        }
        self.registered_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&vm.name);
        self.ready_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, ready| ready.name != vm.name);
        Ok(())
    }

    /// The spec of the registered command whose Kit VM is `vm`.
    fn spec_for_recorded_kit(&self, vm: &str) -> Option<marsh_sbx::KitVmSpec> {
        self.command_names()
            .into_iter()
            .filter_map(|name| self.kit_spec(&name).ok())
            .find(|spec| spec.name == vm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 100 * DAY_MS;

    fn vm(current: bool, last_used_ms: Option<u64>) -> Candidate {
        Candidate {
            purpose: VmPurpose::Kit,
            current,
            in_use: None,
            quarantined: false,
            running: false,
            last_used_ms,
        }
    }

    #[test]
    fn superseded_and_long_unused_vms_go_and_nothing_in_use_does() {
        assert!(
            decide(&vm(false, Some(NOW)), NOW, 3 * DAY_MS)
                .unwrap()
                .starts_with("superseded")
        );
        assert!(
            decide(&vm(true, Some(NOW - 4 * DAY_MS)), NOW, 3 * DAY_MS)
                .unwrap()
                .starts_with("unused for 4 days")
        );
        assert!(decide(&vm(true, Some(NOW - 2 * DAY_MS)), NOW, 3 * DAY_MS).is_err());
        assert!(
            decide(&vm(true, Some(0)), NOW, 0).is_err(),
            "idle retirement off"
        );
        assert_eq!(idle_limit_ms(None, 3), 3 * DAY_MS);
        assert_eq!(idle_limit_ms(Some("30"), 3), 30_000);
        assert!(
            decide(&vm(true, Some(NOW - 60_000)), NOW, 30_000)
                .unwrap()
                .starts_with("unused for 1 minute;")
        );
        assert!(decide(&vm(true, None), NOW, 3 * DAY_MS).is_err());
        let mut used = vm(false, Some(0));
        used.in_use = Some("a job uses it".into());
        assert_eq!(decide(&used, NOW, 3 * DAY_MS), Err("a job uses it".into()));
        let mut quarantined = vm(false, Some(0));
        quarantined.quarantined = true;
        assert!(decide(&quarantined, NOW, 3 * DAY_MS).is_err());
        // A running VM is never removed here, whoever served it.
        let mut running = vm(false, Some(0));
        running.running = true;
        assert!(decide(&running, NOW, 3 * DAY_MS).is_err());
        let mut shell = vm(false, Some(0));
        shell.purpose = VmPurpose::Shell;
        assert!(
            decide(&shell, NOW, 3 * DAY_MS)
                .unwrap()
                .contains("older shell VM")
        );
    }

    #[test]
    fn stock_times_are_read_as_utc() {
        assert_eq!(utc_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            utc_millis("2026-11-03T16:38:03.431+01:00"),
            Some(1_793_720_283_431)
        );
        assert_eq!(
            utc_millis("2026-11-03T15:38:03.431Z"),
            Some(1_793_720_283_431)
        );
        assert_eq!(utc_millis("2000-03-01T00:00:00.5Z"), Some(951_868_800_500));
        for bad in ["2026-11-03T15:38:03", "yesterday", "2026-13-03T00:00:00Z"] {
            assert_eq!(utc_millis(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_idle_limit_comes_from_the_scope_config() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(idle_days(None), 3);
        assert_eq!(idle_days(Some(root.path())), 3);
        fs::write(root.path().join("config.json"), r#"{"vm_idle_days":7}"#).unwrap();
        assert_eq!(idle_days(Some(root.path())), 7);
        fs::write(root.path().join("config.json"), r#"{"vm_idle_days":0}"#).unwrap();
        assert_eq!(idle_days(Some(root.path())), 0);
    }
}
