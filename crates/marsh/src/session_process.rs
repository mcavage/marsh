//! Fixed root helpers for per-shell Linux descendant containment and control.
//!
//! Process identity is independent of the VM and containment identity. Neither
//! SID scans nor a missing/reused leader can prove that descendants are gone.

use std::{io, path::Path};

#[cfg(target_os = "linux")]
#[path = "session_containment.rs"]
mod containment;

/// Enrolls the true session leader before any user code, publishes root-owned
/// containment evidence, and drops to the admitted shell user's credentials.
///
/// # Errors
/// Fails closed if the required cgroup2/pidfd interfaces, leader property, root
/// authority, safe selector, or admitted user are unavailable. No SID fallback.
pub fn record_current(path: &Path, expected_uid: u32, group_id: u32) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let outcome = linux::record_current(path, expected_uid, group_id);
        if let Err(error) = &outcome {
            linux::record_startup_failure(path, expected_uid, error);
        }
        outcome
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, expected_uid, group_id);
        Err(unsupported())
    }
}

/// Signals only pidfd-pinned members of the recorded kernel containment, or
/// recursively kills it and proves `populated 0` for CLEANUP.
///
/// # Errors
/// Reports control errors without declaring controller loss. Missing startup
/// publication is `WouldBlock`; identity mismatch and cleanup uncertainty are not
/// retryable absence. A D-state task retains containment/mount recovery evidence.
pub fn signal_recorded(path: &Path, expected_uid: u32, name: &str) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        linux::signal_recorded(path, expected_uid, name).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "session containment {} operation {name} failed: {error}",
                    path.display()
                ),
            )
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (path, expected_uid, name);
        Err(unsupported())
    }
}

#[cfg(not(target_os = "linux"))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "shell containment requires Linux cgroup2 and pidfds",
    )
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{Path, containment, io};
    use containment::Group;
    use nix::{
        fcntl::{Flock, FlockArg},
        unistd::{self, Gid, Uid},
    };
    use serde::{Deserialize, Serialize};
    use std::{
        ffi::CString,
        fs::{self, File, OpenOptions},
        io::{Read, Write},
        os::{fd::OwnedFd, unix::fs::OpenOptionsExt},
        path::PathBuf,
        time::{Duration, Instant},
    };

    #[derive(Clone, Copy, Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    #[allow(clippy::struct_field_names)] // Kernel process-group nomenclature.
    struct Process {
        pid: u32,
        start_time: u64,
        process_group: u32,
        session: u32,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Record {
        version: u32,
        uid: u32,
        boot: String,
        leader: Process,
        cgroup: PathBuf,
        device: u64,
        inode: u64,
    }

    struct Pinned {
        process: Process,
        foreground: Option<u32>,
        fd: OwnedFd,
    }

    impl Pinned {
        fn signal(&self, signal: rustix::process::Signal) -> io::Result<()> {
            match rustix::process::pidfd_send_signal(&self.fd, signal) {
                Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
    }

    pub(super) fn record_current(selector: &Path, uid: u32, gid: u32) -> io::Result<()> {
        containment::require_root()?;
        let path = containment::record_path(selector, uid)?;
        // A supervisor child starts as a plain child: become the session
        // leader and take an attached PTY as the controlling terminal before
        // publication. A child that already leads its session is unchanged.
        let (current, _) = process(std::process::id())?;
        if current.pid != current.session {
            unistd::setsid()?;
            let stdin = io::stdin();
            if unistd::isatty(&stdin)? {
                rustix::process::ioctl_tiocsctty(&stdin)?;
            }
        }
        let (leader, _) = process(std::process::id())?;
        if leader.pid <= 1 || leader.pid != leader.session || leader.pid != leader.process_group {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "shell must be its session/group leader before publication: pid={} sid={} pgid={}",
                    leader.pid, leader.session, leader.process_group
                ),
            ));
        }
        let user = unistd::User::from_uid(Uid::from_raw(uid))?
            .filter(|user| user.gid.as_raw() == gid && uid != 0)
            .ok_or_else(|| invalid("admitted shell UID/GID is not a nonroot account"))?;
        let username = CString::new(user.name).map_err(|_| invalid("invalid shell username"))?;
        // Probe pidfds now, not after admitting a workload we cannot control.
        let _ = rustix::process::pidfd_open(
            rustix::process::Pid::from_raw(i32::try_from(leader.pid).map_err(io::Error::other)?)
                .ok_or_else(|| invalid("invalid shell PID"))?,
            rustix::process::PidfdFlags::empty(),
        )?;
        containment::prepare_records()?;
        if path.try_exists()? {
            return Err(invalid("session containment already registered"));
        }
        let key = path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| invalid("invalid record key"))?;
        let group = Group::create(key)?;
        let record = Record {
            version: 1,
            uid,
            boot: containment::boot_id()?,
            leader,
            cgroup: group.relative.clone(),
            device: group.device,
            inode: group.inode,
        };
        let temporary = path.with_extension(format!("{}.tmp", leader.pid));
        let outcome = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&temporary)?;
            serde_json::to_writer(&mut file, &record).map_err(io::Error::other)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            // hard_link is no-replace publication, unlike rename. Root-private
            // parent prevents guest selector/symlink swaps from gaining authority.
            fs::hard_link(&temporary, &path)?;
            fs::remove_file(&temporary)?;
            Ok::<_, io::Error>(())
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        outcome?;
        let stdin = io::stdin();
        if unistd::isatty(&stdin)? {
            unistd::fchown(&stdin, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid)))?;
            nix::sys::stat::fchmod(
                &stdin,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )?;
        }
        // Do not use ptrace, no_new_privs, or capability restrictions which
        // change normal sudo/setuid behavior. Only this startup thread exists.
        unistd::initgroups(&username, Gid::from_raw(gid))?;
        unistd::setgid(Gid::from_raw(gid))?;
        unistd::setuid(Uid::from_raw(uid))?;
        if unistd::getuid().as_raw() != uid || unistd::getgid().as_raw() != gid {
            return Err(invalid("shell credential drop did not take effect"));
        }
        Ok(())
    }

    // Readiness cannot drain attached stderr before handing streams to their
    // owner. Preserve exact root startup rejection separately so the host sees
    // the missing primitive, not just a generic 15-second NOT_READY timeout.
    // This is diagnostic ONLY: it never grants a successful cleanup receipt.
    pub(super) fn record_startup_failure(selector: &Path, uid: u32, error: &io::Error) {
        let save = || -> io::Result<()> {
            containment::require_root()?;
            let path = containment::record_path(selector, uid)?;
            containment::prepare_records()?;
            if path.try_exists()? {
                return Ok(());
            }
            let failure = path.with_extension("failure");
            let temporary = path.with_extension(format!("{}.failure-tmp", std::process::id()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(&temporary)?;
            let result = (|| {
                writeln!(file, "shell startup rejected before user code: {error}")?;
                file.sync_all()?;
                fs::hard_link(&temporary, failure)
            })();
            let _ = fs::remove_file(temporary);
            result
        };
        let _ = save();
    }

    fn registration_file(path: &Path) -> io::Result<File> {
        match containment::open_record(path) {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match containment::open_record(&path.with_extension("failure")) {
                    Ok(file) => {
                        let mut detail = String::new();
                        file.take(8192).read_to_string(&mut detail)?;
                        Err(io::Error::other(detail.trim().to_owned()))
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "session containment record not ready",
                    )),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn signal_recorded(selector: &Path, uid: u32, name: &str) -> io::Result<()> {
        containment::require_root()?;
        let signal = match name {
            "INT" => rustix::process::Signal::INT,
            "TERM" | "CLEANUP" | "READY" => rustix::process::Signal::TERM,
            "KILL" => rustix::process::Signal::KILL,
            "HUP" => rustix::process::Signal::HUP,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid session signal",
                ));
            }
        };
        let path = containment::record_path(selector, uid)?;
        let file = registration_file(&path)?;
        let mut file = lock(file)?;
        let mut bytes = Vec::new();
        (&mut *file).take(8193).read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            return Err(invalid("oversized session containment record"));
        }
        let record: Record = serde_json::from_slice(&bytes)
            .map_err(|error| invalid(&format!("invalid session containment record: {error}")))?;
        if record.version != 1
            || record.uid != uid
            || record.boot != containment::boot_id()?
            || record.leader.pid <= 1
            || record.leader.pid != record.leader.session
            || record.leader.pid != record.leader.process_group
            || record.cgroup.file_name()
                != Some(
                    format!(
                        "marsh-session-{}",
                        path.file_name().unwrap().to_string_lossy()
                    )
                    .as_ref(),
                )
        {
            return Err(invalid("session containment binding mismatch"));
        }
        let group = Group::restore(record.cgroup, record.device, record.inode)?;
        if name == "READY" {
            return Ok(());
        }
        if name == "CLEANUP" {
            // TERM is courtesy, not proof. It may fork/setsid/double-fork. All
            // such descendants inherit the group and cgroup.kill still owns them.
            // Inventory failure cannot prevent the exact atomic kernel kill.
            if let Ok(mut targets) = pin(&group)
                && !targets.is_empty()
            {
                targets.sort_by_key(|target| !same_identity(target.process, record.leader));
                for target in targets {
                    let _ = target.signal(rustix::process::Signal::TERM);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            group.kill_and_wait()?;
            group.remove()?;
            fs::remove_file(path)?;
            return Ok(());
        }
        // Freeze to make selection stable across fork/PGID changes. Use each
        // member's pidfd, never a racy negative numeric process-group signal.
        let frozen = group.freeze()?;
        let mut targets = pin(&group)?;
        let foreground = targets
            .iter()
            .find(|target| same_identity(target.process, record.leader))
            .and_then(|target| target.foreground);
        if let Some(foreground) = foreground {
            targets.retain(|target| target.process.process_group == foreground);
            if targets.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "terminal foreground group has no owned live member",
                ));
            }
        } else {
            targets.sort_by_key(|target| !same_identity(target.process, record.leader));
        }
        for target in targets {
            target.signal(signal)?;
        }
        drop(frozen);
        Ok(())
    }

    fn lock(mut file: File) -> io::Result<Flock<File>> {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
                Ok(lock) => return Ok(lock),
                Err((returned, nix::errno::Errno::EAGAIN)) if Instant::now() < deadline => {
                    file = returned;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err((_, error)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("session control serialization failed: {error}"),
                    ));
                }
            }
        }
    }

    fn same_identity(a: Process, b: Process) -> bool {
        a.pid == b.pid && a.start_time == b.start_time
    }

    fn pin(group: &Group) -> io::Result<Vec<Pinned>> {
        let mut targets = Vec::new();
        for pid in group.members()? {
            let (before, _) = match process(pid) {
                Ok(process) => process,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let pid_arg =
                rustix::process::Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?)
                    .ok_or_else(|| invalid("invalid owned member PID"))?;
            let fd =
                match rustix::process::pidfd_open(pid_arg, rustix::process::PidfdFlags::empty()) {
                    Ok(fd) => fd,
                    Err(rustix::io::Errno::SRCH) => continue,
                    Err(error) => return Err(error.into()),
                };
            let (after, foreground) = match process(pid) {
                Ok(process) => process,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !same_identity(before, after) {
                continue;
            }
            match group.contains(pid) {
                Ok(true) => targets.push(Pinned {
                    process: after,
                    foreground,
                    fd,
                }),
                Ok(false) => {} // PID was reused outside this containment; never signal it.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(targets)
    }

    fn process(pid: u32) -> io::Result<(Process, Option<u32>)> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let (_, tail) = stat
            .rsplit_once(')')
            .ok_or_else(|| invalid("invalid process stat"))?;
        let fields = tail.split_whitespace().collect::<Vec<_>>();
        let field = |index: usize| {
            fields
                .get(index)
                .copied()
                .ok_or_else(|| invalid("truncated process stat"))
        };
        let number = |index| {
            field(index)?
                .parse::<u32>()
                .map_err(|_| invalid("invalid process stat field"))
        };
        let foreground = field(5)?
            .parse::<i32>()
            .map_err(|_| invalid("invalid terminal foreground group"))?;
        Ok((
            Process {
                pid,
                start_time: field(19)?
                    .parse()
                    .map_err(|_| invalid("invalid process start time"))?,
                process_group: number(2)?,
                session: number(3)?,
            },
            u32::try_from(foreground).ok().filter(|value| *value > 1),
        ))
    }

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, message)
    }
}
