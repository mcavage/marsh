//! Linux cgroup-v2 ownership for one admitted shell, never a whole VM.
//!
//! Root must enroll the shell before any user code runs. SID, ancestry and UID
//! are observations, not containment: fork inherits a cgroup across setsid,
//! double-fork and setuid. Deliberate root migration/tampering is outside the
//! same-VM trust domain; no writable cgroup handles are delegated to the shell.

use nix::{
    fcntl::{OFlag, openat},
    sys::stat::Mode,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const RECORD_ROOT: &str = "/run/marsh-containment";
const MAX_MEMBERS: usize = 16_384;

pub(super) struct Group {
    directory: File,
    pub(super) relative: PathBuf,
    pub(super) device: u64,
    pub(super) inode: u64,
}

pub(super) fn require_root() -> io::Result<()> {
    if nix::unistd::Uid::effective().is_root() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "shell containment requires the fixed root helper",
        ))
    }
}

// The relay directory is guest writable. It is ONLY a selector; authoritative
// metadata never comes from it. Restrict even the selector before root effects.
pub(super) fn record_path(selector: &Path, uid: u32) -> io::Result<PathBuf> {
    let prefix = Path::new("/run/marsh").join(uid.to_string());
    let suffix = selector
        .strip_prefix(&prefix)
        .map_err(|_| invalid("invalid session record selector"))?;
    let parts = suffix.components().collect::<Vec<_>>();
    let [Component::Normal(session), Component::Normal(name)] = parts.as_slice() else {
        return Err(invalid("invalid session record selector"));
    };
    let session = session
        .to_str()
        .ok_or_else(|| invalid("invalid session identifier"))?;
    if *name != "shell"
        || session.is_empty()
        || session.len() > 128
        || !session
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || selector
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(invalid("invalid session record selector"));
    }
    Ok(Path::new(RECORD_ROOT).join(format!("{uid}-{session}")))
}

pub(super) fn prepare_records() -> io::Result<()> {
    require_root()?;
    // Atomic private mode: a concurrent first opener must never observe a
    // transient 0755 directory between mkdir and chmod and quarantine itself.
    match fs::DirBuilder::new().mode(0o700).create(RECORD_ROOT) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_records()
}

pub(super) fn validate_records() -> io::Result<()> {
    let metadata = fs::symlink_metadata(RECORD_ROOT)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        return Err(invalid(
            "shell containment record directory is not root-private",
        ));
    }
    Ok(())
}

pub(super) fn open_record(path: &Path) -> io::Result<File> {
    validate_records()?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
        || !(1..=2).contains(&metadata.nlink())
    {
        return Err(invalid("invalid root-owned shell containment record"));
    }
    Ok(file)
}

pub(super) fn boot_id() -> io::Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

pub(super) fn process_group_path(pid: u32) -> io::Result<PathBuf> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let mut lines = text.lines();
    let relative = lines
        .next()
        .and_then(|line| line.strip_prefix("0::/"))
        .ok_or_else(|| invalid("shell containment requires unified cgroup v2"))?;
    if lines.next().is_some() {
        return Err(invalid("shell containment requires unified cgroup v2"));
    }
    let path = PathBuf::from(relative);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid("unsafe kernel cgroup path"));
    }
    Ok(path)
}

impl Group {
    pub(super) fn create(key: &str) -> io::Result<Self> {
        require_root()?;
        let parent = process_group_path(std::process::id())?;
        // Never create under the VM-wide root. Stock exec must be inside its
        // existing sandbox container, not a host/global service cgroup.
        if parent.as_os_str().is_empty() {
            return Err(invalid("stock shell exec has no owned parent cgroup"));
        }
        let relative = parent.join(format!("marsh-session-{key}"));
        let path = Path::new(CGROUP_ROOT).join(&relative);
        fs::create_dir(&path).map_err(|error| io::Error::new(error.kind(), format!(
            "required per-session cgroup v2 unavailable at {}: {error}; shell was not started; use a stock DHI shell with writable delegated cgroup2 (no SID fallback)", path.display())))?;
        let outcome = (|| {
            let group = Self::open_path(relative)?;
            // Verify the actual kernel interface BEFORE enrollment/publication.
            // Opening all three for writing rejects readonly/unsupported setups.
            for name in ["cgroup.procs", "cgroup.freeze", "cgroup.kill"] {
                let _ = group.open(name, OFlag::O_WRONLY)?;
            }
            if group.events()?.0 {
                return Err(invalid("new session cgroup is unexpectedly populated"));
            }
            // Exercise the actual primitive on ONLY this new empty group. A
            // writable file descriptor alone does not prove kill/freeze support.
            // Thaw before enrollment so the registrar cannot freeze itself.
            group.write("cgroup.freeze", "1")?;
            group.write("cgroup.kill", "1")?;
            group.write("cgroup.freeze", "0")?;
            group.write("cgroup.procs", &std::process::id().to_string())?;
            if process_group_path(std::process::id())? != group.relative {
                return Err(invalid("shell cgroup enrollment was not observed"));
            }
            Ok(group)
        })();
        if outcome.is_err() {
            // This succeeds only before enrollment. If enrollment happened, the
            // failed helper exits without running user code; retain evidence.
            let _ = fs::remove_dir(&path);
        }
        outcome
    }

    fn open_path(relative: PathBuf) -> io::Result<Self> {
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || !relative
                .file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.starts_with("marsh-session-"))
        {
            return Err(invalid("invalid owned cgroup path"));
        }
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW)
            .open(Path::new(CGROUP_ROOT).join(&relative))?;
        if nix::sys::statfs::fstatfs(&directory)?.filesystem_type()
            != nix::sys::statfs::CGROUP2_SUPER_MAGIC
        {
            return Err(invalid("session containment is not a cgroup2 filesystem"));
        }
        let metadata = directory.metadata()?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(invalid("session cgroup is not root owned"));
        }
        Ok(Self {
            directory,
            relative,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    pub(super) fn restore(relative: PathBuf, device: u64, inode: u64) -> io::Result<Self> {
        require_root()?;
        // Control exec is outside the session, in the same stock parent.
        if relative.parent() != Some(process_group_path(std::process::id())?.as_path()) {
            return Err(invalid(
                "session cgroup does not belong to this stock exec parent",
            ));
        }
        let group = Self::open_path(relative)?;
        if group.device != device || group.inode != inode {
            return Err(invalid(
                "session cgroup kernel identity changed; cleanup unverified",
            ));
        }
        Ok(group)
    }

    fn open(&self, name: &str, flags: OFlag) -> io::Result<File> {
        Ok(File::from(openat(
            &self.directory,
            name,
            flags | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )?))
    }

    fn write(&self, name: &str, value: &str) -> io::Result<()> {
        self.open(name, OFlag::O_WRONLY)
            .and_then(|mut file| file.write_all(value.as_bytes()))
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "owned cgroup {} operation {name}: {error}",
                        self.relative.display()
                    ),
                )
            })
    }

    fn events(&self) -> io::Result<(bool, bool)> {
        let mut text = String::new();
        self.open("cgroup.events", OFlag::O_RDONLY)?
            .take(4097)
            .read_to_string(&mut text)?;
        if text.len() > 4096 {
            return Err(invalid("oversized cgroup events"));
        }
        let field = |name| -> io::Result<bool> {
            let value = text.lines().find_map(|line| line.strip_prefix(name));
            match value {
                Some("0") => Ok(false),
                Some("1") => Ok(true),
                _ => Err(invalid("missing or invalid cgroup event")),
            }
        };
        Ok((field("populated ")?, field("frozen ")?))
    }

    pub(super) fn contains(&self, pid: u32) -> io::Result<bool> {
        Ok(process_group_path(pid)?.starts_with(&self.relative))
    }

    pub(super) fn freeze(&self) -> io::Result<Frozen<'_>> {
        self.write("cgroup.freeze", "1")?;
        let guard = Frozen(self);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let (populated, frozen) = self.events()?;
            if frozen || !populated {
                return Ok(guard);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "session cgroup {} did not freeze (possibly D-state); cleanup/control unverified",
                        self.relative.display()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn members(&self) -> io::Result<Vec<u32>> {
        let mut members = Vec::new();
        let mut directories = vec![(self.directory.try_clone()?, 0)];
        let mut visited = 0;
        while let Some((directory, depth)) = directories.pop() {
            visited += 1;
            if visited > 1024 || depth > 32 {
                return Err(invalid("session cgroup subtree exceeds inventory bound"));
            }
            let mut text = String::new();
            File::from(openat(
                &directory,
                "cgroup.procs",
                OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                Mode::empty(),
            )?)
            .take(256 * 1024)
            .read_to_string(&mut text)?;
            for line in text.lines() {
                let pid = line
                    .parse::<u32>()
                    .map_err(|_| invalid("invalid cgroup member PID"))?;
                if pid <= 1 || members.len() >= MAX_MEMBERS {
                    return Err(invalid("session cgroup member bound violated"));
                }
                members.push(pid);
            }
            for entry in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    let fd = openat(
                        &directory,
                        entry.file_name().as_os_str(),
                        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW,
                        Mode::empty(),
                    )?;
                    directories.push((File::from(fd), depth + 1));
                }
            }
        }
        members.sort_unstable();
        members.dedup();
        Ok(members)
    }

    pub(super) fn kill_and_wait(&self) -> io::Result<()> {
        // cgroup.kill is fork/migration synchronized by the kernel, recursive,
        // and independent of PID, SID, process state and credentials. Attempt it
        // even if a D-state member prevents frozen=1, but never certify before
        // populated=0. The caller retains metadata on every uncertain error.
        let frozen = self.freeze();
        self.write("cgroup.kill", "1")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !self.events()?.0 {
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "owned session cgroup {} remains populated after KILL; possible D-state task; mounts retained, close other attached shells then run marsh stop for explicit recovery",
                        self.relative.display()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(frozen);
        Ok(())
    }

    pub(super) fn remove(self) -> io::Result<()> {
        if self.events()?.0 {
            return Err(invalid("cannot remove populated session containment"));
        }
        let mut visited = 0;
        remove_empty(
            &Path::new(CGROUP_ROOT).join(&self.relative),
            0,
            &mut visited,
            Instant::now() + Duration::from_secs(1),
        )
    }
}

// No workload can create descendants after populated=0. Remove only empty
// cgroup directories (never files or unrelated parent groups).
fn remove_empty(
    path: &Path,
    depth: usize,
    visited: &mut usize,
    deadline: Instant,
) -> io::Result<()> {
    *visited += 1;
    if depth > 32 || *visited > 1024 || Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "empty session cgroup subtree removal exceeded bound; retained record requires explicit scope recovery",
        ));
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            remove_empty(&entry.path(), depth + 1, visited, deadline)?;
        }
    }
    fs::remove_dir(path)
}

pub(super) struct Frozen<'a>(&'a Group);
impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        let _ = self.0.write("cgroup.freeze", "0");
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
