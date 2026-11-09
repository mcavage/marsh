//! Descriptor-relative filesystem operations for snapshots (Linux only).
//!
//! Everything under the pool is reached from one held directory fd with
//! `openat`/`openat2`, never from a path string built from engine data. A
//! volume's data directory comes from the engine's own data-root layout
//! (`<pool>/docker/volumes/<name>/_data`) for a validated single-component
//! `<name>`, opened from the data-root directory with `RESOLVE_BENEATH | NO_SYMLINKS | NO_MAGICLINKS |
//! NO_XDEV`, so a symlinked parent or a mount point is refused. The `Mountpoint`
//! and label fields the engine reports are display data and are never used
//! here. Trees are cloned with `FICLONE` (reflink), entry by entry, with
//! `O_NOFOLLOW`; symlinks are recreated as symlinks, and FIFOs, devices and
//! sockets are skipped and reported as gaps. Removal walks by fd and never
//! follows a link or crosses a mount.
#![allow(
    clippy::missing_errors_doc,
    clippy::useless_conversion,
    clippy::unnecessary_fallible_conversions,
    clippy::cast_possible_truncation
)]

use rustix::fs::{
    AtFlags, Dir, FileType, FlockOperation, Gid, Mode, OFlags, RenameFlags, ResolveFlags, Stat,
    Timespec, Timestamps, Uid, chownat, fchmod, fchown, fstat, futimens, ioctl_ficlone, linkat,
    mkdirat, openat, openat2, readlinkat, renameat_with, statat, symlinkat, unlinkat, utimensat,
};
use rustix::io::Errno;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use crate::breaks::break_mode;

/// The production engine's data root inside the pool.
pub const DATA_ROOT: &str = "docker";
const MAX_DEPTH: u32 = 128;
const MAX_GAPS_PER_VOLUME: usize = 16;

pub type Fd = Arc<OwnedFd>;

/// Resolution rules for every descriptor-relative open below a volume or a
/// snapshot: stay beneath the parent, never follow a symlink, and refuse to
/// cross any mount point, including a bind mount on the same device (which
/// `st_dev` cannot see).
fn beneath() -> ResolveFlags {
    ResolveFlags::BENEATH
        | ResolveFlags::NO_MAGICLINKS
        | ResolveFlags::NO_SYMLINKS
        | ResolveFlags::NO_XDEV
}

fn open_beneath(parent: &impl AsFd, name: &CStr, flags: OFlags) -> io::Result<OwnedFd> {
    let mut resolve = beneath();
    if break_mode("copy-no-xdev") {
        // Deliberately wrong (a break mode): only `st_dev` guards mounts.
        resolve.remove(ResolveFlags::NO_XDEV);
    }
    Ok(openat2(
        parent,
        name,
        flags | OFlags::CLOEXEC,
        Mode::empty(),
        resolve,
    )?)
}

/// Whether an open failed because a mount point is in the way.
fn is_mount_crossing(error: &io::Error) -> bool {
    error.raw_os_error() == Some(Errno::XDEV.raw_os_error())
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// The pool root, held open for the agent's lifetime.
pub struct Pool {
    root: OwnedFd,
}

impl Pool {
    /// # Errors
    /// Fails if `path` is not an accessible directory (a symlink is refused).
    pub fn open(path: &std::path::Path) -> io::Result<Self> {
        let root = rustix::fs::open(path, dir_flags(), Mode::empty())?;
        Ok(Self { root: root.into() })
    }

    /// Bytes the pool's filesystem has available, read from the descriptor the
    /// agent already holds open on the pool root (`fstatvfs`): no path, no
    /// name-addressed operation, nothing the VM's own users could redirect.
    ///
    /// # Errors
    /// Fails if the filesystem cannot be queried.
    pub fn free_bytes(&self) -> io::Result<u64> {
        let stat = rustix::fs::fstatvfs(&self.root)?;
        Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
    }

    /// `<pool>/snapshots`, created (private) if missing.
    ///
    /// # Errors
    /// Fails if it exists as a non-directory or a symlink.
    pub fn snapshots(&self) -> io::Result<OwnedFd> {
        match mkdirat(&self.root, "snapshots", Mode::from_raw_mode(0o700)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(openat(&self.root, "snapshots", dir_flags(), Mode::empty())?)
    }

    /// The data directory of the engine's local volume `name`, found only by
    /// the engine's layout beneath the pool fd.
    ///
    /// The engine makes its data root a mount of its own (a bind mount of
    /// itself), so the walk is in two steps: a plain `openat` of the data-root
    /// directory from the pool fd (a symlink there is refused, and crossing
    /// that one mount is expected), then `openat2` from the data root with
    /// `RESOLVE_BENEATH | NO_SYMLINKS | NO_MAGICLINKS | NO_XDEV`, which refuses
    /// any symlink or mount point on the way to the volume.
    ///
    /// # Errors
    /// Fails for an invalid name, a symlink or mount point in the way, or a
    /// missing directory.
    pub fn volume_source(&self, name: &str) -> io::Result<OwnedFd> {
        if !valid_volume_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "volume name is not a single safe component",
            ));
        }
        let data_root = openat(&self.root, DATA_ROOT, dir_flags(), Mode::empty())?;
        let relative = format!("volumes/{name}/_data");
        Ok(openat2(
            &data_root,
            relative.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_XDEV,
        )?)
    }
}

impl Pool {
    /// The directory of the engine's local volume `name` (`volumes/<name>`, the
    /// parent of `_data`), found the same way as [`Self::volume_source`].
    /// Restore installs into a sibling of `_data` and swaps them.
    ///
    /// # Errors
    /// Fails for an invalid name, a symlink or mount point in the way, or a
    /// missing directory.
    pub fn volume_dir(&self, name: &str) -> io::Result<OwnedFd> {
        if !valid_volume_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "volume name is not a single safe component",
            ));
        }
        let data_root = openat(&self.root, DATA_ROOT, dir_flags(), Mode::empty())?;
        let relative = format!("volumes/{name}");
        Ok(openat2(
            &data_root,
            relative.as_str(),
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            beneath(),
        )?)
    }
}

/// Atomically swaps two entries of one directory.
pub fn exchange(parent: &impl AsFd, first: &str, second: &str) -> io::Result<()> {
    Ok(renameat_with(
        parent,
        first,
        parent,
        second,
        RenameFlags::EXCHANGE,
    )?)
}

/// The engine's own rule for local volume names, tightened to a single path
/// component: `[A-Za-z0-9][A-Za-z0-9_.-]{0,254}`.
#[must_use]
pub fn valid_volume_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 255
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// `prefix` plus `bytes` random bytes in hex, from the kernel.
///
/// # Errors
/// Fails if `/dev/urandom` cannot be read.
pub fn random_id(prefix: &str, bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0_u8; bytes];
    File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    let mut out = String::from(prefix);
    for byte in buffer {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

pub fn open_dir_at(parent: &impl AsFd, name: &str) -> io::Result<OwnedFd> {
    Ok(openat(parent, name, dir_flags(), Mode::empty())?)
}

/// Creates `name` (mode 0700), failing if it exists.
pub fn mkdir_new(parent: &impl AsFd, name: &str) -> io::Result<OwnedFd> {
    mkdirat(parent, name, Mode::from_raw_mode(0o700))?;
    open_dir_at(parent, name)
}

#[must_use]
pub fn exists_at(parent: &impl AsFd, name: &str) -> bool {
    statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).is_ok()
}

/// Atomic publish: `old` becomes `new`, failing if `new` already exists.
pub fn publish(parent: &impl AsFd, old: &str, new: &str) -> io::Result<()> {
    Ok(renameat_with(
        parent,
        old,
        parent,
        new,
        RenameFlags::NOREPLACE,
    )?)
}

/// Writes a new file (never following a link, never replacing one).
pub fn write_new_file_at(dir: &impl AsFd, name: &str, bytes: &[u8]) -> io::Result<()> {
    let fd = openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?;
    let mut file = File::from(fd);
    file.write_all(bytes)?;
    Ok(())
}

/// Replaces `name` atomically with `bytes` via a temporary sibling.
pub fn replace_file_at(dir: &impl AsFd, name: &str, bytes: &[u8]) -> io::Result<()> {
    let tmp = format!(".{name}.new");
    match unlinkat(dir, tmp.as_str(), AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    write_new_file_at(dir, &tmp, bytes)?;
    Ok(renameat_with(
        dir,
        tmp.as_str(),
        dir,
        name,
        RenameFlags::empty(),
    )?)
}

/// Reads a regular file of at most `max` bytes.
pub fn read_file_at(dir: &impl AsFd, name: &str, max: u64) -> io::Result<Vec<u8>> {
    let fd = openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let stat = fstat(&fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut out = Vec::new();
    File::from(fd).take(max + 1).read_to_end(&mut out)?;
    if out.len() as u64 > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "file too large"));
    }
    Ok(out)
}

pub fn remove_file_at(dir: &impl AsFd, name: &str) -> io::Result<()> {
    match unlinkat(dir, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Entry names of a directory (excluding `.` and `..`) as raw bytes.
pub fn list_names(dir: &impl AsFd) -> io::Result<Vec<Vec<u8>>> {
    let mut reader = Dir::read_from(dir)?;
    let mut names = Vec::new();
    while let Some(entry) = reader.read() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name != b"." && name != b".." {
            names.push(name.to_vec());
        }
    }
    Ok(names)
}

/// Takes an exclusive, non-blocking advisory lock on a directory fd. A live
/// capture holds it on its staging directory, so crash recovery can tell a
/// dead capture from one still running. The kernel drops it on process death.
pub fn try_lock_dir(dir: &impl AsFd) -> bool {
    rustix::fs::flock(dir, FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// Removes `name` under `parent` and everything beneath it, by descriptor.
/// Symlinks are unlinked, never followed; a different filesystem (a mount
/// point) is an error rather than something to descend into.
///
/// # Errors
/// Fails on the first entry that cannot be removed.
pub fn remove_tree_at(parent: &impl AsFd, name: &CStr) -> io::Result<()> {
    remove_tree_depth(parent, name, 0)
}

fn remove_tree_depth(parent: &impl AsFd, name: &CStr, depth: u32) -> io::Result<()> {
    if depth > MAX_DEPTH + 2 {
        return Err(io::Error::other("directory nesting too deep to remove"));
    }
    let before = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(before.st_mode) != FileType::Directory {
        return Ok(unlinkat(parent, name, AtFlags::empty())?);
    }
    let dir = open_beneath(parent, name, OFlags::RDONLY | OFlags::DIRECTORY).map_err(|error| {
        if is_mount_crossing(&error) {
            io::Error::other("refusing to remove across a mount point")
        } else {
            error
        }
    })?;
    let here = fstat(&dir)?;
    if here.st_dev != statat(parent, c".", AtFlags::SYMLINK_NOFOLLOW)?.st_dev {
        return Err(io::Error::other("refusing to remove across a mount point"));
    }
    let mut names: Vec<CString> = Vec::new();
    {
        let mut reader = Dir::read_from(&dir)?;
        while let Some(entry) = reader.read() {
            let entry = entry?;
            let entry_name = entry.file_name();
            if entry_name != c"." && entry_name != c".." {
                names.push(entry_name.to_owned());
            }
        }
    }
    for entry in &names {
        remove_tree_depth(&dir, entry, depth + 1)?;
    }
    drop(dir);
    Ok(unlinkat(parent, name, AtFlags::REMOVEDIR)?)
}

// ---------------------------------------------------------------- copying

struct VolumeState {
    display: String,
    dev: u64,
    pending: AtomicUsize,
    failed: AtomicBool,
    aborted: AtomicBool,
    files: AtomicU64,
    gaps: Mutex<Vec<String>>,
    extra_gaps: AtomicUsize,
    hardlinks: Mutex<HashMap<u64, (Fd, CString)>>,
}

impl VolumeState {
    fn gap(&self, text: String) {
        let mut gaps = self
            .gaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if gaps.len() < MAX_GAPS_PER_VOLUME {
            gaps.push(text);
        } else {
            self.extra_gaps.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn fail(&self, text: String) {
        if !self.failed.swap(true, Ordering::SeqCst) {
            self.gap(text);
        }
    }
}

enum JobKind {
    Root,
    Child(CString),
}

struct Job {
    volume: usize,
    src: Fd,
    dst: Fd,
    kind: JobKind,
    depth: u32,
}

/// Jobs wait in one stack per volume. A worker takes from the volume that has
/// had the least work done so far, so small volumes finish first and a huge
/// one cannot starve them of the freeze budget.
#[derive(Default)]
struct Queue {
    stacks: Vec<Vec<Job>>,
    done: Vec<u64>,
    pending: usize,
    closed: bool,
}

impl Queue {
    fn push(&mut self, job: Job) {
        if self.stacks.len() <= job.volume {
            self.stacks.resize_with(job.volume + 1, Vec::new);
            self.done.resize(job.volume + 1, 0);
        }
        self.stacks[job.volume].push(job);
    }

    fn pop(&mut self) -> Option<Job> {
        let index = (0..self.stacks.len())
            .filter(|i| !self.stacks[*i].is_empty())
            .min_by_key(|i| self.done[*i])?;
        self.stacks[index].pop()
    }

    fn credit(&mut self, volume: usize, work: u64) {
        if let Some(done) = self.done.get_mut(volume) {
            *done += work;
        }
    }
}

/// One volume to clone: both ends are already-open directory fds.
pub struct VolumeCopy {
    pub display: String,
    pub src: OwnedFd,
    pub dst: OwnedFd,
}

/// What happened to one volume.
#[derive(Debug, Default)]
pub struct VolumeResult {
    pub complete: bool,
    pub files: u64,
    pub gaps: Vec<String>,
}

/// A pool of worker threads that clone directory trees. Start workers with
/// `worker` (one per thread), `submit` the volumes, `wait`, then `shutdown`.
pub struct Copier {
    queue: Mutex<Queue>,
    wake: Condvar,
    volumes: Mutex<Vec<Arc<VolumeState>>>,
    deadline: Mutex<Option<Instant>>,
    expired: AtomicBool,
    progress: AtomicU64,
    use_clone: bool,
}

impl Copier {
    #[must_use]
    pub fn new(use_clone: bool) -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
            volumes: Mutex::new(Vec::new()),
            deadline: Mutex::new(None),
            expired: AtomicBool::new(false),
            progress: AtomicU64::new(0),
            use_clone,
        }
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts the clock: work not finished by `deadline` is abandoned.
    pub fn set_deadline(&self, deadline: Option<Instant>) {
        *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = deadline;
    }

    /// Queues the volumes. Returns the index of the first one.
    ///
    /// # Errors
    /// Fails if a source root cannot be inspected.
    pub fn submit(&self, volumes: Vec<VolumeCopy>) -> io::Result<usize> {
        let mut states = self
            .volumes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let first = states.len();
        let mut jobs = Vec::new();
        for volume in volumes {
            let stat = fstat(&volume.src)?;
            let index = states.len();
            states.push(Arc::new(VolumeState {
                display: volume.display,
                dev: stat.st_dev.into(),
                pending: AtomicUsize::new(1),
                failed: AtomicBool::new(false),
                aborted: AtomicBool::new(false),
                files: AtomicU64::new(0),
                gaps: Mutex::new(Vec::new()),
                extra_gaps: AtomicUsize::new(0),
                hardlinks: Mutex::new(HashMap::new()),
            }));
            jobs.push(Job {
                volume: index,
                src: Arc::new(volume.src),
                dst: Arc::new(volume.dst),
                kind: JobKind::Root,
                depth: 0,
            });
        }
        drop(states);
        let mut queue = self.queue();
        queue.pending += jobs.len();
        for job in jobs {
            queue.push(job);
        }
        drop(queue);
        self.wake.notify_all();
        Ok(first)
    }

    /// Blocks until the queue is drained, calling `tick` about every 50 ms.
    pub fn wait(&self, tick: &mut dyn FnMut()) {
        let mut queue = self.queue();
        while queue.pending > 0 {
            let (guard, _) = self
                .wake
                .wait_timeout(queue, std::time::Duration::from_millis(50))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue = guard;
            drop(queue);
            tick();
            queue = self.queue();
        }
    }

    /// Entries (files, links, directories) handled so far. A heartbeat that
    /// follows this counter reflects real work, not just a live thread.
    #[must_use]
    pub fn progress(&self) -> u64 {
        self.progress.load(Ordering::Relaxed)
    }

    /// Stops all copying now: no further entry is touched, every unfinished
    /// volume is marked incomplete, and `wait` returns. A worker stuck inside a
    /// single filesystem call is left behind (it holds only its own fds) and
    /// exits whenever that call returns.
    pub fn abandon(&self) {
        self.expired.store(true, Ordering::SeqCst);
        for state in self
            .volumes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            if state.pending.load(Ordering::SeqCst) > 0 {
                state.aborted.store(true, Ordering::SeqCst);
            }
        }
        let mut queue = self.queue();
        for stack in &mut queue.stacks {
            stack.clear();
        }
        queue.closed = true;
        queue.pending = 0;
        drop(queue);
        self.wake.notify_all();
    }

    pub fn shutdown(&self) {
        self.queue().closed = true;
        self.wake.notify_all();
    }

    /// The outcome of the volume submitted at `index`.
    #[must_use]
    pub fn result(&self, index: usize) -> VolumeResult {
        let states = self
            .volumes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = &states[index];
        let mut gaps = state
            .gaps
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let extra = state.extra_gaps.load(Ordering::Relaxed);
        if extra > 0 {
            gaps.push(format!("{extra} more entries had the same problem"));
        }
        let aborted = state.aborted.load(Ordering::SeqCst);
        if aborted {
            gaps.push("took too long: not saved".into());
        }
        VolumeResult {
            complete: state.pending.load(Ordering::SeqCst) == 0
                && !state.failed.load(Ordering::SeqCst)
                && !aborted,
            files: state.files.load(Ordering::Relaxed),
            gaps,
        }
    }

    /// Worker loop; returns after `shutdown`.
    pub fn worker(&self) {
        loop {
            let job = {
                let mut queue = self.queue();
                loop {
                    if let Some(job) = queue.pop() {
                        break Some(job);
                    }
                    if queue.closed {
                        break None;
                    }
                    queue = self
                        .wake
                        .wait(queue)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            let Some(job) = job else { return };
            let state = {
                let states = self
                    .volumes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                Arc::clone(&states[job.volume])
            };
            let volume = job.volume;
            let mut children = Vec::new();
            let mut work = 1_u64;
            if !state.failed.load(Ordering::SeqCst)
                && !state.aborted.load(Ordering::SeqCst)
                && let Err(error) = self.process(&state, &job, volume, &mut children, &mut work)
            {
                state.fail(format!("{}: {error}", state.display));
            }
            state.pending.fetch_add(children.len(), Ordering::SeqCst);
            let mut queue = self.queue();
            queue.credit(volume, work);
            queue.pending += children.len();
            for child in children {
                queue.push(child);
            }
            state.pending.fetch_sub(1, Ordering::SeqCst);
            queue.pending = queue.pending.saturating_sub(1);
            let done = queue.pending == 0;
            drop(queue);
            if done {
                self.wake.notify_all();
            } else {
                self.wake.notify_one();
            }
        }
    }

    fn out_of_time(&self) -> bool {
        if self.expired.load(Ordering::Relaxed) {
            return true;
        }
        let deadline = *self
            .deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            self.expired.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    fn process(
        &self,
        state: &VolumeState,
        job: &Job,
        volume: usize,
        children: &mut Vec<Job>,
        work: &mut u64,
    ) -> io::Result<()> {
        let (src_dir, dst_dir): (Fd, Fd) = match &job.kind {
            JobKind::Root => (Arc::clone(&job.src), Arc::clone(&job.dst)),
            JobKind::Child(name) => {
                let src = match open_beneath(
                    &*job.src,
                    name.as_c_str(),
                    OFlags::RDONLY | OFlags::DIRECTORY,
                ) {
                    Ok(src) => src,
                    Err(error) if is_mount_crossing(&error) => {
                        state.gap(format!(
                            "mount point {} inside the volume was not saved",
                            display_name(name)
                        ));
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                mkdirat(&*job.dst, name.as_c_str(), Mode::from_raw_mode(0o700))?;
                let dst = openat(&*job.dst, name.as_c_str(), dir_flags(), Mode::empty())?;
                (Arc::new(src), Arc::new(dst))
            }
        };
        let own = fstat(&*src_dir)?;
        if u64::from(own.st_dev) != state.dev {
            state.gap("a mount point inside the volume was not saved".into());
            return Ok(());
        }
        let mut reader = Dir::read_from(&*src_dir)?;
        while let Some(entry) = reader.read() {
            let entry = entry?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            self.progress.fetch_add(1, Ordering::Relaxed);
            if self.out_of_time() {
                state.aborted.store(true, Ordering::SeqCst);
                return Ok(());
            }
            if state.failed.load(Ordering::SeqCst) {
                return Ok(());
            }
            let stat = statat(&*src_dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    if job.depth + 1 > MAX_DEPTH {
                        return Err(io::Error::other("directories nested too deeply"));
                    }
                    if u64::from(stat.st_dev) != state.dev {
                        state.gap(format!(
                            "mount point {} inside the volume was not saved",
                            display_name(name)
                        ));
                        continue;
                    }
                    children.push(Job {
                        volume,
                        src: Arc::clone(&src_dir),
                        dst: Arc::clone(&dst_dir),
                        kind: JobKind::Child(name.to_owned()),
                        depth: job.depth + 1,
                    });
                }
                FileType::RegularFile => {
                    self.copy_file(state, &src_dir, &dst_dir, name, &stat)?;
                    state.files.fetch_add(1, Ordering::Relaxed);
                    *work += 1;
                }
                FileType::Symlink => {
                    copy_symlink(&src_dir, &dst_dir, name, &stat)?;
                    state.files.fetch_add(1, Ordering::Relaxed);
                    *work += 1;
                }
                other => state.gap(format!(
                    "{} is a {} and was not saved",
                    display_name(name),
                    special_kind(other)
                )),
            }
        }
        apply_metadata(&*dst_dir, &own)?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn copy_file(
        &self,
        state: &VolumeState,
        src_dir: &Fd,
        dst_dir: &Fd,
        name: &CStr,
        stat: &Stat,
    ) -> io::Result<()> {
        let linked = u64::from(stat.st_nlink) > 1;
        let src = match open_beneath(
            &**src_dir,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY,
        ) {
            Ok(src) => src,
            Err(error) if is_mount_crossing(&error) => {
                state.gap(format!(
                    "mount point {} inside the volume was not saved",
                    display_name(name)
                ));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let opened = fstat(&src)?;
        if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
            || opened.st_ino != stat.st_ino
        {
            state.gap(format!("{} changed while it was saved", display_name(name)));
            return Ok(());
        }
        let create = || {
            openat(
                &**dst_dir,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
        };
        let dst = if linked && break_mode("hardlink-racy") {
            // Deliberately wrong (a break mode): look up, then create and record
            // in separate critical sections, so two workers can both be first.
            let found = state
                .hardlinks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&stat.st_ino)
                .map(|(dir, first)| (Arc::clone(dir), first.clone()));
            if let Some((first_dir, first_name)) = found {
                linkat(
                    &*first_dir,
                    first_name.as_c_str(),
                    &**dst_dir,
                    name,
                    AtFlags::empty(),
                )?;
                return Ok(());
            }
            let dst = create()?;
            state
                .hardlinks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(stat.st_ino, (Arc::clone(dst_dir), name.to_owned()));
            dst
        } else if linked {
            // Electing the first copy of an inode is one critical section:
            // look up, create the destination file and record it, so two
            // workers can never both decide to be first.
            let mut map = state
                .hardlinks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((first_dir, first_name)) = map.get(&stat.st_ino) {
                linkat(
                    &**first_dir,
                    first_name.as_c_str(),
                    &**dst_dir,
                    name,
                    AtFlags::empty(),
                )?;
                return Ok(());
            }
            let dst = create()?;
            map.insert(stat.st_ino, (Arc::clone(dst_dir), name.to_owned()));
            dst
        } else {
            create()?
        };
        if break_mode("fault-copy-stall") {
            // A filesystem operation that never returns (a test fault).
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
        let dst = if self.use_clone {
            ioctl_ficlone(&dst, &src).map_err(|error| match error {
                Errno::OPNOTSUPP | Errno::XDEV | Errno::INVAL => io::Error::other(format!(
                    "reflink is not available for {} ({error})",
                    display_name(name)
                )),
                other => io::Error::from(other),
            })?;
            dst
        } else {
            // A plain read/write loop (std's `io::copy` between two files would
            // use copy_file_range, which reflinks on XFS and hide the cost).
            let mut from = File::from(src);
            let mut to = File::from(dst);
            let mut buffer = vec![0_u8; 1 << 20];
            loop {
                let read = from.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                to.write_all(&buffer[..read])?;
            }
            OwnedFd::from(to)
        };
        fchown(
            &dst,
            Some(Uid::from_raw(opened.st_uid)),
            Some(Gid::from_raw(opened.st_gid)),
        )?;
        fchmod(&dst, Mode::from_raw_mode(opened.st_mode & 0o7777))?;
        futimens(&dst, &timestamps(&opened))?;
        Ok(())
    }
}

fn copy_symlink(src_dir: &Fd, dst_dir: &Fd, name: &CStr, stat: &Stat) -> io::Result<()> {
    let target = readlinkat(&**src_dir, name, Vec::new())?;
    symlinkat(target.as_c_str(), &**dst_dir, name)?;
    chownat(
        &**dst_dir,
        name,
        Some(Uid::from_raw(stat.st_uid)),
        Some(Gid::from_raw(stat.st_gid)),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    utimensat(
        &**dst_dir,
        name,
        &timestamps(stat),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    Ok(())
}

/// Copies a directory's ownership, mode and times onto its clone.
///
/// # Errors
/// Fails if any of the three updates fails.
pub fn apply_metadata(dst: &impl AsFd, stat: &Stat) -> io::Result<()> {
    fchown(
        dst,
        Some(Uid::from_raw(stat.st_uid)),
        Some(Gid::from_raw(stat.st_gid)),
    )?;
    fchmod(dst, Mode::from_raw_mode(stat.st_mode & 0o7777))?;
    futimens(dst, &timestamps(stat))?;
    Ok(())
}

fn timestamps(stat: &Stat) -> Timestamps {
    Timestamps {
        last_access: Timespec {
            tv_sec: stat.st_atime.try_into().unwrap_or(0),
            tv_nsec: stat.st_atime_nsec.try_into().unwrap_or(0),
        },
        last_modification: Timespec {
            tv_sec: stat.st_mtime.try_into().unwrap_or(0),
            tv_nsec: stat.st_mtime_nsec.try_into().unwrap_or(0),
        },
    }
}

/// An entry name made safe to show: lossy UTF-8, control characters replaced,
/// bounded.
#[must_use]
pub fn display_name(name: &CStr) -> String {
    let text = String::from_utf8_lossy(name.to_bytes());
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(80)
        .collect();
    if text.chars().count() > 80 {
        out.push('…');
    }
    format!("'{out}'")
}

fn special_kind(kind: FileType) -> &'static str {
    match kind {
        FileType::Fifo => "named pipe",
        FileType::Socket => "socket",
        FileType::CharacterDevice => "character device",
        FileType::BlockDevice => "block device",
        _ => "special file",
    }
}

pub fn fstat_dir(fd: &impl AsFd) -> io::Result<Stat> {
    Ok(fstat(fd)?)
}

/// Opens an existing file for writing (used for the capture heartbeat).
pub fn open_for_write_at(dir: &impl AsFd, name: &str) -> io::Result<File> {
    let fd = openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

/// Opens a directory by an ordinary path. Only the deliberately broken
/// `snapshot-name-from-label` mode uses this; the real code never builds a
/// path from engine data.
pub fn open_dir_by_path(path: &std::path::Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}
