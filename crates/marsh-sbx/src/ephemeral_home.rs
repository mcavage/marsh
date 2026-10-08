//! Bounded private ephemeral HOME slots.
//!
//! Each slot is an owner-only directory holding `lease.lock` and `home`. An
//! exclusive `flock` on `lease.lock` is the only allocation record: there is no
//! global history or cross-daemon ledger. A crash releases the lock; the next
//! allocation reuses the slot only when its `home` is empty, so retained data
//! is never erased implicitly. Explicit release/recovery empties `home` while
//! holding the lock.

use super::{AdmittedHostGrant, SbxError, source_chain::SourceChain};
use marsh_contracts::MountAccess;
use rustix::fs::{FlockOperation, Mode, OFlags, flock, mkdirat, openat};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const SLOTS: u8 = 16;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const MAX_CLEANUP_DEPTH: usize = 128;

/// Untrusted wire token naming a bounded slot, never a caller-supplied path.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EphemeralHomeToken {
    slot: u8,
    operation: String,
}

impl fmt::Debug for EphemeralHomeToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EphemeralHomeToken")
            .field("slot", &self.slot)
            .finish_non_exhaustive()
    }
}

/// Exclusive slot ownership. Dropping this releases only the file lock; a
/// non-empty `home` keeps the slot unavailable until explicit release, which
/// removes `home` entirely.
#[derive(Debug)]
pub struct EphemeralHomeLease {
    token: EphemeralHomeToken,
    parent: SourceChain,
    home: PathBuf,
    _lock: File,
}

fn invalid(detail: impl Into<String>) -> SbxError {
    SbxError::HostGrantFence(detail.into())
}

/// Fixed per-UID pool root (production). Tests supply their own root.
pub(super) fn default_root() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = Path::new("/Users/Shared");
    #[cfg(not(target_os = "macos"))]
    let base = Path::new("/var/tmp");
    base.join(format!(
        ".marsh-ephemeral-homes-{}",
        rustix::process::geteuid().as_raw()
    ))
}

fn private_directory(path: &Path) -> Result<SourceChain, SbxError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("missing pool parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("missing pool name"))?;
    let parent_chain = SourceChain::open_parent(parent)?;
    match mkdirat(
        parent_chain.leaf().as_ref(),
        name,
        Mode::from_raw_mode(0o700),
    ) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    let chain = SourceChain::open_parent(path)?;
    let metadata = chain.leaf().metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o777 != 0o700 {
        return Err(invalid(format!(
            "ephemeral home pool {} is not an owner-only (0700) directory",
            path.display()
        )));
    }
    Ok(chain)
}

fn slot_path(root: &Path, slot: u8) -> Result<PathBuf, SbxError> {
    if slot >= SLOTS {
        return Err(invalid("invalid ephemeral home slot"));
    }
    Ok(root.join(format!("slot-{slot:02}")))
}

fn open_lock(parent: &SourceChain) -> Result<File, SbxError> {
    let lock = File::from(openat(
        parent.leaf().as_ref(),
        "lease.lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::from_raw_mode(0o600),
    )?);
    let metadata = lock.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.nlink() != 1
    {
        return Err(invalid("ephemeral slot lock is not a private regular file"));
    }
    Ok(lock)
}

fn try_lock(lock: &File) -> Result<bool, SbxError> {
    match flock(lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn ensure_home(parent: &SourceChain) -> Result<(), SbxError> {
    match mkdirat(parent.leaf().as_ref(), "home", Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

impl EphemeralHomeLease {
    /// Per-UID managed slot bound.
    pub const CAPACITY: u8 = SLOTS;

    pub(crate) fn allocate_in(root: &Path) -> Result<Self, SbxError> {
        let _pool = private_directory(root)?;
        let mut unavailable = 0;
        for slot in 0..SLOTS {
            let path = slot_path(root, slot)?;
            let parent = private_directory(&path)?;
            let lock = open_lock(&parent)?;
            if !try_lock(&lock)? {
                unavailable += 1;
                continue;
            }
            ensure_home(&parent)?;
            let home = path.join("home");
            if std::fs::read_dir(&home)?.next().is_some() {
                // Never erase retained data just to allocate.
                unavailable += 1;
                continue;
            }
            return Ok(Self {
                token: EphemeralHomeToken {
                    slot,
                    operation: uuid::Uuid::new_v4().to_string(),
                },
                parent,
                home,
                _lock: lock,
            });
        }
        Err(invalid(format!(
            "ephemeral home capacity exhausted: {unavailable}/{SLOTS} private slots are active or retain data; finish sessions or run `marsh recover-home SLOT --discard`"
        )))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.home
    }

    #[must_use]
    pub fn token(&self) -> EphemeralHomeToken {
        self.token.clone()
    }

    /// Retained for API compatibility; slot admission is the live lock itself.
    ///
    /// # Errors
    /// Never fails.
    pub fn close_admission(&self) -> Result<(), SbxError> {
        Ok(())
    }

    /// Keep the home data; the slot stays unavailable while it is non-empty.
    #[must_use]
    pub fn keep(self) -> PathBuf {
        self.home.clone()
    }

    /// Empty the private home while holding the slot lock.
    ///
    /// # Errors
    /// Retains the data when the leaf changed or cleanup cannot complete.
    pub fn release(&self) -> Result<(), SbxError> {
        cleanup_home(&self.parent, &self.home)
    }

    pub(crate) fn recover_in(root: &Path, slot: u8) -> Result<Option<PathBuf>, SbxError> {
        let path = slot_path(root, slot)?;
        if !path.exists() {
            return Ok(None);
        }
        let parent = private_directory(&path)?;
        let lock = open_lock(&parent)?;
        if !try_lock(&lock)? {
            return Err(invalid(format!(
                "ephemeral slot {slot} is held by a live session"
            )));
        }
        let home = path.join("home");
        if !home.exists() {
            return Ok(None);
        }
        cleanup_home(&parent, &home)?;
        Ok(Some(home))
    }
}

impl EphemeralHomeToken {
    pub(crate) fn path_in(&self, root: &Path) -> Result<PathBuf, SbxError> {
        Ok(slot_path(root, self.slot)?.join("home"))
    }

    /// The slot must currently be held by its allocating session.
    pub(crate) fn validate_in(
        &self,
        root: &Path,
        grant: &AdmittedHostGrant,
    ) -> Result<(), SbxError> {
        let path = self.path_in(root)?;
        if grant.source != path || grant.access != MountAccess::ReadWrite {
            return Err(invalid("ephemeral HOME differs from its host allocation"));
        }
        let parent = SourceChain::open_parent(&slot_path(root, self.slot)?)?;
        let lock = open_lock(&parent)?;
        if try_lock(&lock)? {
            return Err(invalid("ephemeral HOME slot is not held by a live session"));
        }
        Ok(())
    }
}

fn cleanup_home(parent: &SourceChain, home: &Path) -> Result<(), SbxError> {
    use rustix::fs::{AtFlags, statat};
    let parent = parent.leaf();
    match statat(parent.as_ref(), "home", AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => {}
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let leaf = File::from(openat(parent.as_ref(), "home", DIRECTORY, Mode::empty())?);
    let mut budget = 100_000;
    empty_directory(
        &leaf,
        0,
        &mut budget,
        Instant::now() + Duration::from_secs(3),
    )
    .and_then(|()| {
        rustix::fs::unlinkat(parent.as_ref(), "home", AtFlags::REMOVEDIR)?;
        Ok(())
    })
    .map_err(|error| {
        invalid(format!(
            "ephemeral cleanup of {} failed: {error}",
            home.display()
        ))
    })
}

fn empty_directory(
    dir: &File,
    depth: usize,
    budget: &mut usize,
    deadline: Instant,
) -> Result<(), SbxError> {
    use rustix::fs::{AtFlags, Dir, FileType, fstat, statat, unlinkat};
    if depth >= MAX_CLEANUP_DEPTH {
        return Err(invalid("ephemeral cleanup depth exceeded"));
    }
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        if *budget == 0 || Instant::now() >= deadline {
            return Err(invalid("ephemeral cleanup budget exceeded"));
        }
        *budget -= 1;
        let stat = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let child = File::from(openat(dir, name, DIRECTORY, Mode::empty())?);
            empty_directory(&child, depth + 1, budget, deadline)?;
            let now = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            let held = fstat(&child)?;
            if now.st_dev != held.st_dev || now.st_ino != held.st_ino {
                return Err(invalid("ephemeral cleanup entry changed"));
            }
            unlinkat(dir, name, AtFlags::REMOVEDIR)?;
        } else {
            unlinkat(dir, name, AtFlags::empty())?;
        }
    }
    Ok(())
}
