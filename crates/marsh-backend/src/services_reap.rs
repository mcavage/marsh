//! Reaping a job's services containers when its attempt ends (E4; DECISIONS 11;
//! `docs/design/services.md`, "Reaping").
//!
//! An attempt that reaches its project's services VM is first recorded in
//! `services-reaps.json` in the daemon's home, before any channel reaches the
//! VM (so a container can never exist without a record). The record is marked
//! running while the attempt lives. When the attempt ends, the daemon asks the
//! services agent to reap the attempt's containers in the base workspace; the
//! record goes once the agent reports the reap complete. A daemon that dies
//! first leaves its records behind, and the recovery after a restart
//! reaps them once the project's relay is back. Failed reaps keep their record
//! with the reason and are asked again at the next trigger (a job's end, or the
//! next daemon start), once ten seconds have passed. There is no periodic retry. Nothing here
//! starts a services VM: a record waits until its VM runs again.
//!
//! Shell attempts are never recorded: a shell's objects persist until they are
//! removed (DECISIONS 10 and 11).

use super::StockDaemonBackend;
use marsh_sbx::services_relay::{BASE_WORKSPACE, Relay};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write as _},
    os::unix::fs::OpenOptionsExt as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const RECORDS_SCHEMA: &str = "marsh.services-reaps/v1";
/// Records held at once. A job that would need one more is refused before its
/// first channel reaches the VM.
const MAX_RECORDS: usize = 256;
/// How long a failed reap waits before it is asked again (at the next trigger).
const RETRY: Duration = Duration::from_secs(10);

/// Test breaks of the daemon's half (`docs/design/test-breaks.md`).
pub(crate) mod breaks {
    /// The daemon never reaps: no record is kept and no reap is asked at a job's
    /// end or at a restart (E4 evidence: EP-03 c3).
    pub const REAP_OFF: &str = "reap-off";
    /// A shell's objects are reaped when the shell exits (E4 evidence: EP-09 c2).
    pub const SHELL_REAP_ON_EXIT: &str = "shell-reap-on-exit";
}

#[derive(Debug, Deserialize, Serialize)]
struct RecordsFile {
    #[serde(default)]
    schema: String,
    #[serde(default)]
    records: Vec<Record>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Record {
    attempt: String,
    project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// One attempt's record, in memory.
#[derive(Debug)]
struct Pending {
    project: PathBuf,
    /// Why the last reap did not finish; kept with the record.
    reason: Option<String>,
    /// The attempt is still running in this daemon: no pass reaps it.
    running: bool,
    /// A reap of this record is in flight.
    reaping: bool,
    /// When it may be asked again (`None`: now).
    next: Option<Instant>,
}

/// The daemon's records of attempts whose services containers must be reaped.
pub(crate) struct ReapRecords {
    path: PathBuf,
    state: Mutex<BTreeMap<String, Pending>>,
    /// Why the file could not be read at start. While set, nothing is recorded
    /// and nothing is reaped: an unreadable record is never overwritten.
    unreadable: Option<String>,
    /// The file held records when the daemon started: a dead daemon left them.
    inherited: bool,
}

impl ReapRecords {
    /// The records of the scope whose daemon home is `daemon_home`.
    pub(crate) fn open(daemon_home: &Path) -> Self {
        let path = daemon_home.join("services-reaps.json");
        let (state, unreadable) = match read(&path) {
            Ok(state) => (state, None),
            Err(error) => (BTreeMap::new(), Some(error)),
        };
        Self {
            path,
            inherited: !state.is_empty(),
            state: Mutex::new(state),
            unreadable,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Pending>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records that `attempt` uses `project`'s services VM. Refuses (with the
    /// reason) when the record cannot be kept.
    fn track(&self, project: &Path, attempt: &str) -> Result<(), String> {
        if let Some(error) = &self.unreadable {
            return Err(format!("the reap records are unreadable ({error})"));
        }
        let mut state = self.lock();
        if let Some(pending) = state.get_mut(attempt) {
            pending.running = true;
            return Ok(());
        }
        if state.len() >= MAX_RECORDS {
            return Err(format!(
                "{MAX_RECORDS} attempts are waiting for their services containers to be reaped"
            ));
        }
        state.insert(
            attempt.to_owned(),
            Pending {
                project: project.to_owned(),
                reason: None,
                running: true,
                reaping: false,
                next: None,
            },
        );
        if let Err(error) = persist(&self.path, &state) {
            state.remove(attempt);
            return Err(format!("the attempt cannot be recorded: {error}"));
        }
        Ok(())
    }

    /// The attempt ended: its record may be reaped.
    fn finish(&self, attempt: &str) {
        if let Some(pending) = self.lock().get_mut(attempt) {
            pending.running = false;
            pending.next = None;
        }
    }

    fn inherited(&self) -> bool {
        self.inherited && self.unreadable.is_none()
    }

    /// Attempts that may be reaped now, and their projects.
    fn candidates(&self, now: Instant) -> Vec<(String, PathBuf)> {
        if self.unreadable.is_some() {
            return Vec::new();
        }
        self.lock()
            .iter()
            .filter(|(_, pending)| {
                !pending.running && !pending.reaping && pending.next.is_none_or(|at| at <= now)
            })
            .map(|(attempt, pending)| (attempt.clone(), pending.project.clone()))
            .collect()
    }

    /// Claims a record for one reap; `false` when it is no longer eligible.
    fn begin(&self, attempt: &str) -> bool {
        match self.lock().get_mut(attempt) {
            Some(pending) if !pending.running && !pending.reaping => {
                pending.reaping = true;
                true
            }
            _ => false,
        }
    }

    /// The reap finished (the record goes) or did not (the record stays, with
    /// the reason, and is asked again later).
    fn end(&self, attempt: &str, outcome: Result<(), String>, now: Instant) {
        let mut state = self.lock();
        match outcome {
            Ok(()) => {
                state.remove(attempt);
                if let Err(error) = persist(&self.path, &state) {
                    eprintln!("marshd: services reap records not saved: {error}");
                }
            }
            Err(reason) => {
                if let Some(pending) = state.get_mut(attempt) {
                    pending.reaping = false;
                    pending.reason = Some(reason);
                    pending.next = Some(now + RETRY);
                }
                if let Err(error) = persist(&self.path, &state) {
                    eprintln!("marshd: services reap records not saved: {error}");
                }
            }
        }
    }

    /// `services rm` removed the project's VM and its containers with it.
    fn forget_project(&self, project: &Path) {
        let mut state = self.lock();
        let before = state.len();
        state.retain(|_, pending| pending.project != project);
        if state.len() != before
            && let Err(error) = persist(&self.path, &state)
        {
            eprintln!("marshd: services reap records not saved: {error}");
        }
    }
}

fn read(path: &Path) -> Result<BTreeMap<String, Pending>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let file: RecordsFile =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    if file.schema != RECORDS_SCHEMA {
        return Err(format!(
            "{}: unknown schema {:?}",
            path.display(),
            file.schema
        ));
    }
    Ok(file
        .records
        .into_iter()
        .map(|record| {
            (
                record.attempt,
                Pending {
                    project: PathBuf::from(record.project),
                    reason: record.reason,
                    running: false,
                    reaping: false,
                    next: None,
                },
            )
        })
        .collect())
}

/// Writes the records atomically (a private temporary file, then a rename).
fn persist(path: &Path, state: &BTreeMap<String, Pending>) -> Result<(), String> {
    let file = RecordsFile {
        schema: RECORDS_SCHEMA.into(),
        records: state
            .iter()
            .map(|(attempt, pending)| Record {
                attempt: attempt.clone(),
                project: pending.project.to_string_lossy().into_owned(),
                reason: pending.reason.clone(),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.new");
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    output
        .write_all(&bytes)
        .and_then(|()| output.sync_all())
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))
}

/// Asks one project's relay to reap an attempt's containers. Only a complete
/// report is success.
fn reap_on(relay: &Relay, attempt: &str) -> Result<(u32, u32), String> {
    match relay.reap(BASE_WORKSPACE, attempt) {
        Ok(reply) if reply.complete => Ok((reply.removed, reply.kept)),
        Ok(reply) => Err(reply.gaps.join("; ")),
        Err(error) => Err(error.to_string()),
    }
}

impl StockDaemonBackend {
    fn reap_records(&self) -> Arc<ReapRecords> {
        Arc::clone(
            self.services
                .reaps
                .get_or_init(|| Arc::new(ReapRecords::open(&self.config.daemon_home))),
        )
    }

    /// Records that an attempt uses its project's services VM, before its first
    /// channel reaches it (E2's endpoint calls this with the hold).
    pub(crate) fn services_reap_track(&self, root: &Path, attempt: &str) -> Result<(), String> {
        if marsh_contracts::break_mode(breaks::REAP_OFF) {
            return Ok(());
        }
        self.reap_records().track(root, attempt)
    }

    /// The attempt ended. Its containers are reaped now when the project's relay
    /// is up, and by a later pass otherwise.
    pub(crate) fn services_reap_attempt_ended(&self, attempt: &str) {
        if marsh_contracts::break_mode(breaks::REAP_OFF) {
            return;
        }
        self.reap_records().finish(attempt);
        self.services_reap_pass();
    }

    /// Reaps every record whose attempt has ended and whose project's relay is
    /// up. Runs at a job's end and after a daemon start (the recovery pass).
    pub(super) fn services_reap_pass(&self) {
        let Some(relays) = self.services_relays() else {
            return;
        };
        let records = self.reap_records();
        for (attempt, root) in records.candidates(Instant::now()) {
            let Ok(entry) = relays.get(&root) else {
                continue;
            };
            if !records.begin(&attempt) {
                continue;
            }
            let relay = entry.relay().clone();
            let records = Arc::clone(&records);
            thread::spawn(move || {
                let started = Instant::now();
                let outcome = reap_on(&relay, &attempt);
                match &outcome {
                    Ok((removed, kept)) => eprintln!(
                        "marshd: reaped the services containers of attempt {attempt}: removed {removed}, kept {kept} ({} ms)",
                        started.elapsed().as_millis()
                    ),
                    Err(reason) => eprintln!(
                        "marshd: reaping the services containers of attempt {attempt} did not finish after {} ms ({reason}); it is asked again",
                        started.elapsed().as_millis()
                    ),
                }
                records.end(&attempt, outcome.map(|_| ()), Instant::now());
            });
        }
    }

    /// The records a dead daemon left are reaped once, when this daemon's relays
    /// are back (the recovery pass). A daemon that started with none does nothing.
    pub(super) fn services_reap_startup(&self) {
        if self.reap_records().inherited() {
            self.services_reap_pass();
        }
    }

    /// The shell exited: the test break reaps the shell's objects now. The
    /// normal daemon never does.
    pub(crate) fn services_reap_now(&self, root: &Path, attempt: &str) -> Result<(), String> {
        let relays = self
            .services_relays()
            .ok_or_else(|| "no services relays".to_owned())?;
        let entry = relays.get(root).map_err(|error| error.to_string())?;
        reap_on(entry.relay(), attempt).map(|_| ())
    }

    /// The project's VM and its containers are gone (`services rm`, a confirmed
    /// reset): its records have nothing left to reap.
    pub(crate) fn services_reap_forget_project(&self, root: &Path) {
        self.reap_records().forget_project(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A running attempt is never a candidate; a record left by a dead daemon is
    /// due at once; a failed reap waits for the retry delay; a complete one goes.
    #[test]
    fn a_record_outlives_its_daemon_and_is_reaped_only_when_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let project = PathBuf::from("/p");
        let records = ReapRecords::open(dir.path());
        records.track(&project, "a1").unwrap();
        assert!(
            records.candidates(Instant::now()).is_empty(),
            "still running"
        );
        let after_restart = ReapRecords::open(dir.path());
        assert_eq!(
            after_restart.candidates(Instant::now()),
            vec![("a1".into(), project)]
        );
        assert!(after_restart.begin("a1"));
        after_restart.end("a1", Err("the relay is busy".into()), Instant::now());
        assert!(
            after_restart.candidates(Instant::now()).is_empty(),
            "a failed reap waits"
        );
        assert_eq!(after_restart.candidates(Instant::now() + RETRY).len(), 1);
        assert!(after_restart.begin("a1"));
        after_restart.end("a1", Ok(()), Instant::now());
        assert_eq!(
            ReapRecords::open(dir.path())
                .candidates(Instant::now())
                .len(),
            0
        );
    }
}
