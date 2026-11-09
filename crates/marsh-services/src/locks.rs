//! Per-workspace serialization (SECURITY-DESIGN A4, ENG-PLAN D3).
//!
//! Two levels per workspace:
//!
//! - the **operation lock** (`op`): one capture, drop or restore at a time;
//! - the **API gate**: a reader/writer lock (`gate_shared` / `gate_exclusive`).
//!   In the agent the capture's freeze is E3's own gate (`Freezer`, implemented
//!   by `relay::Filtered` with `Services::freeze_scope_within`), because the
//!   filter admits mutations on long-lived connections inside its own process.
//!   This gate is the same contract for any endpoint that wants to share the
//!   lock files (the tests' stand-in did).
//!
//! The identity of a workspace is its exact validated string, never a
//! truncated display form: the in-process table is keyed by the string and the
//! lock files by its SHA-256. The locks are also `flock`s on files in the
//! pool (`pool/locks/<sha256>.op|.gate`), so a second agent process (a
//! replacement or a duplicate) excludes the first, and the kernel releases
//! them when a process dies. In-process, writers are preferred so a waiting
//! capture is not starved by a stream of mutations.
//!
//! One `ScopeLocks` is shared by everything in an agent: the relay builds it
//! first and hands the same `Arc` to the capture and drop code (the operation
//! lock is what serializes them across agent processes).

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Slot {
    op: Option<&'static str>,
    readers: usize,
    writer: bool,
    writers_waiting: usize,
}

impl Slot {
    fn idle(&self) -> bool {
        self.op.is_none() && self.readers == 0 && !self.writer && self.writers_waiting == 0
    }
}

/// Why a lock could not be taken in time.
#[derive(Debug, Clone, Copy)]
pub struct Busy {
    /// What holds it, when that is known (only for holders in this process).
    pub holder: Option<&'static str>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    Op,
    Gate,
}

#[derive(Default)]
pub struct ScopeLocks {
    directory: Option<PathBuf>,
    state: Mutex<HashMap<String, Slot>>,
    changed: Condvar,
}

/// What a capture holds while it freezes a workspace: nothing that changes the
/// workspace's state may be admitted until it is dropped. E3's `Services`
/// implements it with `freeze_scope_within`; `ScopeLocks` with the exclusive
/// API gate (the unit tests and stand-in endpoints).
pub trait Freezer: Send + Sync {
    /// # Errors
    /// The reason it could not be taken within `wait` (a request is still running).
    fn freeze(&self, workspace: &str, wait: Duration) -> Result<Box<dyn Send>, String>;
}

impl Freezer for Arc<ScopeLocks> {
    fn freeze(&self, workspace: &str, wait: Duration) -> Result<Box<dyn Send>, String> {
        self.gate_exclusive(workspace, wait)
            .map(|guard| Box::new(guard) as Box<dyn Send>)
            .map_err(|_| "a request in this workspace is still running".to_owned())
    }
}

pub struct OpGuard {
    locks: Arc<ScopeLocks>,
    key: String,
    _file: Option<File>,
}

pub struct GateGuard {
    locks: Arc<ScopeLocks>,
    key: String,
    exclusive: bool,
    _file: Option<File>,
}

fn lock_name(workspace: &str, kind: Kind) -> String {
    let mut name = String::with_capacity(70);
    for byte in Sha256::digest(workspace.as_bytes()) {
        let _ = write!(name, "{byte:02x}");
    }
    name.push_str(if kind == Kind::Op { ".op" } else { ".gate" });
    name
}

impl ScopeLocks {
    /// Locks backed by files in `directory` (created private if missing), so
    /// they hold across agent processes.
    ///
    /// # Errors
    /// Fails if the directory cannot be created.
    pub fn open(directory: &Path) -> io::Result<Arc<Self>> {
        match std::fs::DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        Ok(Arc::new(Self {
            directory: Some(directory.to_owned()),
            ..Self::default()
        }))
    }

    /// Process-local locks only (tests, and hosts without a pool).
    #[must_use]
    pub fn in_memory() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn table(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Takes the cross-process file lock, if there is a directory.
    fn file_lock(
        &self,
        workspace: &str,
        kind: Kind,
        exclusive: bool,
        deadline: Instant,
    ) -> Result<Option<File>, ()> {
        let Some(directory) = &self.directory else {
            return Ok(None);
        };
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(directory.join(lock_name(workspace, kind)))
            .map_err(|_| ())?;
        let operation = if exclusive {
            rustix::fs::FlockOperation::NonBlockingLockExclusive
        } else {
            rustix::fs::FlockOperation::NonBlockingLockShared
        };
        loop {
            if rustix::fs::flock(&file, operation).is_ok() {
                return Ok(Some(file));
            }
            if Instant::now() >= deadline {
                return Err(());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// # Errors
    /// `Busy` (with what holds it, if it is this process) if not free within `wait`.
    pub fn op(
        self: &Arc<Self>,
        workspace: &str,
        holder: &'static str,
        wait: Duration,
    ) -> Result<OpGuard, Busy> {
        let deadline = Instant::now() + wait;
        let mut table = self.table();
        loop {
            let slot = table.entry(workspace.to_owned()).or_default();
            if slot.op.is_none() {
                slot.op = Some(holder);
                break;
            }
            let held = slot.op;
            let now = Instant::now();
            if now >= deadline {
                return Err(Busy { holder: held });
            }
            table = self
                .changed
                .wait_timeout(table, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        drop(table);
        if let Ok(file) = self.file_lock(workspace, Kind::Op, true, deadline) {
            Ok(OpGuard {
                locks: Arc::clone(self),
                key: workspace.to_owned(),
                _file: file,
            })
        } else {
            self.release_op(workspace);
            Err(Busy { holder: None })
        }
    }

    /// Exclusive API gate: waits for in-flight mutations to finish and blocks
    /// new ones.
    ///
    /// # Errors
    /// `Busy` if not free within `wait`.
    pub fn gate_exclusive(
        self: &Arc<Self>,
        workspace: &str,
        wait: Duration,
    ) -> Result<GateGuard, Busy> {
        let deadline = Instant::now() + wait;
        let mut table = self.table();
        table
            .entry(workspace.to_owned())
            .or_default()
            .writers_waiting += 1;
        loop {
            let slot = table.entry(workspace.to_owned()).or_default();
            if !slot.writer && slot.readers == 0 {
                slot.writers_waiting -= 1;
                slot.writer = true;
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                slot.writers_waiting -= 1;
                if slot.idle() {
                    table.remove(workspace);
                }
                drop(table);
                self.changed.notify_all();
                return Err(Busy { holder: None });
            }
            table = self
                .changed
                .wait_timeout(table, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        drop(table);
        if let Ok(file) = self.file_lock(workspace, Kind::Gate, true, deadline) {
            Ok(GateGuard {
                locks: Arc::clone(self),
                key: workspace.to_owned(),
                exclusive: true,
                _file: file,
            })
        } else {
            self.release_gate(workspace, true);
            Err(Busy { holder: None })
        }
    }

    /// Shared API gate, for the policy's mutating requests.
    ///
    /// # Errors
    /// `Busy` if a capture holds the gate for longer than `wait`.
    pub fn gate_shared(
        self: &Arc<Self>,
        workspace: &str,
        wait: Duration,
    ) -> Result<GateGuard, Busy> {
        let deadline = Instant::now() + wait;
        let mut table = self.table();
        loop {
            let slot = table.entry(workspace.to_owned()).or_default();
            if !slot.writer && slot.writers_waiting == 0 {
                slot.readers += 1;
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                if slot.idle() {
                    table.remove(workspace);
                }
                return Err(Busy { holder: None });
            }
            table = self
                .changed
                .wait_timeout(table, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        drop(table);
        if let Ok(file) = self.file_lock(workspace, Kind::Gate, false, deadline) {
            Ok(GateGuard {
                locks: Arc::clone(self),
                key: workspace.to_owned(),
                exclusive: false,
                _file: file,
            })
        } else {
            self.release_gate(workspace, false);
            Err(Busy { holder: None })
        }
    }

    fn release_op(&self, key: &str) {
        let mut table = self.table();
        if let Some(slot) = table.get_mut(key) {
            slot.op = None;
            if slot.idle() {
                table.remove(key);
            }
        }
        drop(table);
        self.changed.notify_all();
    }

    fn release_gate(&self, key: &str, exclusive: bool) {
        let mut table = self.table();
        if let Some(slot) = table.get_mut(key) {
            if exclusive {
                slot.writer = false;
            } else {
                slot.readers = slot.readers.saturating_sub(1);
            }
            if slot.idle() {
                table.remove(key);
            }
        }
        drop(table);
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn is_idle(&self) -> bool {
        self.table().is_empty()
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        self.locks.release_op(&self.key);
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.locks.release_gate(&self.key, self.exclusive);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(60);

    #[test]
    fn a_capture_excludes_mutations_and_other_captures_in_its_workspace_only() {
        let locks = ScopeLocks::in_memory();
        let capture_op = locks.op("w1", "capture", SHORT).unwrap();
        let capture_gate = locks.gate_exclusive("w1", SHORT).unwrap();
        assert_eq!(
            locks.op("w1", "capture", SHORT).err().unwrap().holder,
            Some("capture")
        );
        assert!(locks.gate_shared("w1", SHORT).is_err());
        assert!(locks.op("w2", "capture", SHORT).is_ok());
        assert!(locks.gate_shared("w2", SHORT).is_ok());
        drop(capture_gate);
        drop(capture_op);
        let mutation = locks.gate_shared("w1", SHORT).unwrap();
        let second = locks.gate_shared("w1", SHORT).unwrap();
        assert!(locks.gate_exclusive("w1", SHORT).is_err());
        drop((mutation, second));
        assert!(locks.gate_exclusive("w1", SHORT).is_ok());
        assert!(locks.is_idle(), "idle slots are released");
    }

    #[test]
    fn workspaces_that_share_a_long_prefix_are_different_locks_and_files() {
        let long_a = format!("{}A", "x".repeat(200));
        let long_b = format!("{}B", "x".repeat(200));
        assert_ne!(lock_name(&long_a, Kind::Op), lock_name(&long_b, Kind::Op));
        let locks = ScopeLocks::in_memory();
        let _a = locks.op(&long_a, "capture", SHORT).unwrap();
        assert!(locks.op(&long_b, "capture", SHORT).is_ok());
        assert!(locks.op(&long_a, "capture", SHORT).is_err());
    }

    #[test]
    fn file_locks_exclude_a_second_holder_with_its_own_table() {
        let dir = std::env::temp_dir().join(format!("s3-locks-{}", std::process::id()));
        let first = ScopeLocks::open(&dir).unwrap();
        let second = ScopeLocks::open(&dir).unwrap(); // stands in for another agent process
        let held = first.op("w", "capture", SHORT).unwrap();
        assert!(second.op("w", "capture", SHORT).is_err());
        let gate = first.gate_shared("w", SHORT).unwrap();
        assert!(second.gate_exclusive("w", SHORT).is_err());
        assert!(second.gate_shared("w", SHORT).is_ok());
        drop((held, gate));
        assert!(second.op("w", "capture", SHORT).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
