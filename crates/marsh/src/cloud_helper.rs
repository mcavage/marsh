//! Remote side of cloud placement (`docs/design/cloud-placement.md` s4), run
//! by the static `marsh-local` artifact inside a daemon-owned cloud sandbox.
//! It is an internal protocol, not a user command: the host daemon uploads
//! the artifact and a snapshot, then drives these steps over stock `sbx exec`.
//!
//! The baseline records each extracted entry with a content hash, so the
//! returned delta is exact: only created, modified (including mode), retyped,
//! and deleted paths, never the unchanged snapshot.
//!
//! Every step but the workload runs as root (`sudo -n`). The workload runs as
//! the dedicated `marsh-run` user, which has no sudo, Docker group, or access
//! to the root-owned control state (helper, baseline, request, status). After
//! it exits, every process of that user is killed before the status is
//! written, so a status means no workload writer remains.

use marsh_contracts::cloud::{self, CloudMode, CloudRequest, Kind, Reader, Writer};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Write};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod coordinator;
mod detached;

/// Control state: root-owned in a session shell sandbox; in a Kit sandbox
/// (no root there) the helper runs as the Kit's user and keeps it private
/// to that user.
const STATE: &str = "/var/lib/marsh-cloud";
const USER_STATE: &str = "/tmp/marsh-cloud/state";

fn privileged() -> bool {
    nix::unistd::geteuid().is_root()
}

fn state_path() -> PathBuf {
    PathBuf::from(if privileged() { STATE } else { USER_STATE })
}

/// The helper the workload runs (its shell) and command links name.
fn helper_path() -> PathBuf {
    if privileged() {
        PathBuf::from(INSTALLED)
    } else {
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from(INSTALLED))
    }
}
/// Root-owned copy of this helper; also the workload's shell.
const INSTALLED: &str = cloud::INSTALLED_HELPER;
const RUN_USER: &str = "marsh-run";
/// Trusted sandbox network settings, inherited from the root step's own
/// environment (stock `sbx` sets them; sudo keeps them), never from the caller.
const TRUSTED_ENV: [&str; 10] = [
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "JAVA_TOOL_OPTIONS",
];
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// `marsh-local --internal-cloud OP ...`.
#[must_use]
pub fn main(arguments: &[OsString]) -> i32 {
    let words = arguments
        .iter()
        .map(OsString::as_os_str)
        .collect::<Vec<_>>();
    let result = match words.as_slice() {
        [op, root] if *op == "prepare" => prepare(Path::new(root), RUN_USER),
        [op, root] if *op == "archive" => archive(Path::new(root)),
        [op, root] if *op == "apply" => apply(Path::new(root)),
        [op, archive, root] if *op == "unpack" => unpack(Path::new(archive), Path::new(root)),
        [op] if *op == "run" => return run(),
        [op, flag] if *op == "run" && *flag == "--detached" => return detached::run(),
        [op] if *op == "supervise" => return detached::supervise(),
        [op] if *op == "state" => {
            detached::state();
            Ok(())
        }
        [op, stream, offset] if *op == "tail" => detached::tail(stream, offset),
        [op] if *op == "fetch" => detached::fetch(),
        [op] if *op == "baseline" => detached::baseline(),
        [op] if *op == "delegate" => coordinator::store_delegate(),
        [op] if *op == "status" => status(),
        [op, name] if *op == "signal" => signal(name),
        [op, root, out] if *op == "delta" => delta(Path::new(root), Path::new(out)),
        [op, root] if *op == "sync" => sync_delta(Path::new(root)),
        [op] if *op == "rebase" => rebase(),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: --internal-cloud prepare|unpack|run [--detached]|status|state|tail|fetch|baseline|delegate|signal|delta|sync|rebase",
        )),
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("marsh cloud: {error}");
            1
        }
    }
}

fn absolute(root: &Path) -> io::Result<()> {
    if root.is_absolute()
        && root.parent().is_some()
        && root
            .components()
            .all(|component| !matches!(component, std::path::Component::ParentDir))
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace root must be an absolute path",
        ))
    }
}

/// The workload's user, chosen once by `prepare`: the dedicated `marsh-run`
/// (session shell profile) or the sandbox's own `agent` (a cloud agent
/// binding, confined by no-new-privileges instead).
fn run_user() -> io::Result<nix::unistd::User> {
    if !privileged() {
        return nix::unistd::User::from_uid(nix::unistd::getuid())
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("the sandbox user is unknown"));
    }
    let name = fs::read_to_string(state_path().join("user"))
        .map_or_else(|_| RUN_USER.to_owned(), |name| name.trim().to_owned());
    nix::unistd::User::from_name(&name)
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("workload user is missing"))
}

/// As root: create the workload user, install this helper root-owned, and
/// create the natural workspace path with its leaf owned by the workload user.
fn prepare(root: &Path, user_name: &str) -> io::Result<()> {
    absolute(root)?;
    if !privileged() {
        // A Kit sandbox: the marsh workspace mixin made the natural path at
        // startup, owned by this user, and empty.
        state_dir()?;
        let metadata = fs::symlink_metadata(root)?;
        if !metadata.is_dir()
            || std::os::unix::fs::MetadataExt::uid(&metadata) != nix::unistd::getuid().as_raw()
            || fs::read_dir(root)?.next().is_some()
        {
            return Err(io::Error::other(
                "the Kit sandbox did not prepare an empty workspace path for this user",
            ));
        }
        let _ = user_name;
        return Ok(());
    }
    if !nix::unistd::geteuid().is_root() {
        return Err(io::Error::other("prepare must run as root"));
    }
    if nix::unistd::User::from_name(RUN_USER)
        .map_err(io::Error::other)?
        .is_none()
    {
        let status = Command::new("useradd")
            .args([
                "--create-home",
                "--user-group",
                "--shell",
                "/bin/bash",
                RUN_USER,
            ])
            .status()?;
        if !status.success() {
            return Err(io::Error::other("could not create the workload user"));
        }
    }
    state_dir()?;
    fs::write(state_path().join("user"), user_name)?;
    let user = run_user()?;
    let installed = Path::new(INSTALLED);
    let directory = installed.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o755))?;
    let temporary = directory.join(".marsh-local.new");
    let _ = fs::remove_file(&temporary);
    fs::copy(std::env::current_exe()?, &temporary)?;
    std::os::unix::fs::chown(&temporary, Some(0), Some(0))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o755))?;
    fs::rename(&temporary, installed)?;
    state_dir()?;
    if fs::symlink_metadata(root).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "workspace path already exists in the sandbox",
        ));
    }
    fs::create_dir_all(root)?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o755))?;
    std::os::unix::fs::chown(root, Some(user.uid.as_raw()), Some(user.gid.as_raw()))
}

fn state_dir() -> io::Result<PathBuf> {
    let state = state_path();
    match fs::create_dir_all(&state) {
        Ok(()) => fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(&state)?;
    if !metadata.is_dir()
        || std::os::unix::fs::MetadataExt::uid(&metadata) != nix::unistd::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::other("cloud state directory is not private"));
    }
    Ok(state)
}

fn join(root: &Path, relative: &[u8]) -> PathBuf {
    root.join(OsStr::from_bytes(relative))
}

/// Refuse to write through a symlink placed in a parent position.
fn real_parents(root: &Path, relative: &[u8]) -> io::Result<()> {
    let mut current = root.to_path_buf();
    let components = relative.split(|byte| *byte == b'/').collect::<Vec<_>>();
    for component in &components[..components.len().saturating_sub(1)] {
        current.push(OsStr::from_bytes(component));
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(io::Error::other("archive parent is not a directory")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                let user = run_user()?;
                std::os::unix::fs::chown(
                    &current,
                    Some(user.uid.as_raw()),
                    Some(user.gid.as_raw()),
                )?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct Hashing<'a, W: Write> {
    inner: W,
    hasher: &'a mut Sha256,
}

impl<W: Write> Write for Hashing<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.hasher.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn hash_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut hasher = Sha256::new();
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)?;
    io::copy(
        &mut BufReader::new(file),
        &mut Hashing {
            inner: io::sink(),
            hasher: &mut hasher,
        },
    )?;
    Ok(hasher.finalize().to_vec())
}

/// Extract the snapshot under ROOT, recording the baseline and the request.
fn unpack(archive: &Path, root: &Path) -> io::Result<()> {
    absolute(root)?;
    let state = state_dir()?;
    let user = run_user()?;
    let own = |path: &Path| {
        std::os::unix::fs::lchown(path, Some(user.uid.as_raw()), Some(user.gid.as_raw()))
    };
    let input: Box<dyn io::Read> = if archive == Path::new("-") {
        Box::new(io::stdin().lock())
    } else {
        Box::new(File::open(archive)?)
    };
    let mut reader = Reader::new(BufReader::new(input), cloud::MAX_ARCHIVE)?;
    let mut baseline = Writer::new(
        BufWriter::new(File::create(state.join("baseline"))?),
        u64::MAX,
    )?;
    let mut directories = Vec::new();
    let mut request = false;
    while let Some(header) = reader.next_header()? {
        // The private Git copy (`.git/...`) is unpacked but never part of
        // the baseline: it is not captured, so it can never return.
        let tracked = !cloud::protected(&header.path);
        match header.kind {
            Kind::Request => {
                fs::write(state.join("request"), reader.data()?)?;
                request = true;
            }
            Kind::Directory => {
                real_parents(root, &header.path)?;
                let path = join(root, &header.path);
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        if !fs::symlink_metadata(&path)?.is_dir() {
                            return Err(io::Error::other("archive directory is not a directory"));
                        }
                    }
                    Err(error) => return Err(error),
                }
                own(&path)?;
                if tracked {
                    baseline.directory(&header.path, header.mode)?;
                }
                directories.push((path, header.mode));
            }
            Kind::File => {
                real_parents(root, &header.path)?;
                let path = join(root, &header.path);
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
                    .open(&path)?;
                let mut hasher = Sha256::new();
                reader.copy_data(&mut Hashing {
                    inner: &mut file,
                    hasher: &mut hasher,
                })?;
                file.set_permissions(fs::Permissions::from_mode(header.mode))?;
                own(&path)?;
                let digest = hasher.finalize();
                if tracked {
                    baseline.file(&header.path, header.mode, 32, &mut &digest[..])?;
                }
            }
            Kind::Symlink => {
                real_parents(root, &header.path)?;
                let target = reader.data()?;
                let path = join(root, &header.path);
                std::os::unix::fs::symlink(OsStr::from_bytes(&target), &path)?;
                own(&path)?;
                if tracked {
                    baseline.symlink(&header.path, &target)?;
                }
            }
            Kind::Delete => return Err(io::Error::other("a snapshot cannot delete")),
        }
    }
    if !request {
        return Err(io::Error::other("snapshot carries no request"));
    }
    // Restrictive directory modes last, deepest first, after their contents.
    for (path, mode) in directories.into_iter().rev() {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))?;
    }
    baseline.finish()?.flush()
}

/// This process and its ancestors (the stock exec session that runs it).
fn own_lineage() -> std::collections::BTreeSet<i32> {
    let mut lineage = std::collections::BTreeSet::new();
    let mut pid = i32::try_from(std::process::id()).unwrap_or(0);
    while pid > 1 && lineage.insert(pid) {
        pid = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                // `pid (comm) state ppid ...`; comm may contain spaces.
                let rest = &stat[stat.rfind(')')? + 1..];
                rest.split_whitespace().nth(1)?.parse().ok()
            })
            .unwrap_or(0);
    }
    lineage
}

/// Pids of every workload process: real or effective uid `uid`. In a Kit
/// sandbox the helper shares that user: only processes under
/// no-new-privileges (the runner sets it before starting the workload; stock
/// exec sessions and control steps lack it) count, sparing this process,
/// its ancestors, and the runner.
fn processes_of(uid: u32) -> Vec<i32> {
    let confined_only = !privileged();
    let spared = if confined_only {
        let mut spared = own_lineage();
        if let Some(runner) = fs::read_to_string(state_path().join("runner"))
            .ok()
            .and_then(|pid| pid.trim().parse::<i32>().ok())
        {
            spared.insert(runner);
        }
        spared
    } else {
        std::collections::BTreeSet::new()
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| !spared.contains(pid))
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| {
                let owned = status
                    .lines()
                    .find_map(|line| line.strip_prefix("Uid:"))
                    .is_some_and(|ids| {
                        ids.split_whitespace()
                            .take(2)
                            .any(|id| id.parse::<u32>() == Ok(uid))
                    });
                let confined = status
                    .lines()
                    .find_map(|line| line.strip_prefix("NoNewPrivs:"))
                    .is_some_and(|value| value.trim() == "1");
                // A zombie writes nothing and cannot be killed again. A leader
                // thread that exited while other threads run also shows `Z`:
                // that process is alive.
                let zombie = status
                    .lines()
                    .find_map(|line| line.strip_prefix("State:"))
                    .is_some_and(|state| state.trim_start().starts_with('Z'))
                    && status
                        .lines()
                        .find_map(|line| line.strip_prefix("Threads:"))
                        .is_some_and(|threads| threads.trim() == "1");
                owned && !zombie && (confined || !confined_only)
            })
        })
        .collect()
}

/// Kill every workload process, including ones that left the process group
/// (`setsid`, `nohup`). True only when none remains.
fn kill_workload(uid: u32) -> bool {
    for _ in 0..100 {
        let pids = processes_of(uid);
        if pids.is_empty() {
            return true;
        }
        for pid in pids {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    processes_of(uid).is_empty()
}

/// Create the workload's cwd if the snapshot did not carry it (an ignored
/// directory), owned by the workload user.
fn ensure_cwd(cwd: &Path, user: &nix::unistd::User) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut current = cwd;
    while fs::symlink_metadata(current).is_err() {
        missing.push(current.to_path_buf());
        current = current
            .parent()
            .ok_or_else(|| io::Error::other("cwd has no existing ancestor"))?;
    }
    for path in missing.into_iter().rev() {
        fs::create_dir(&path)?;
        std::os::unix::fs::chown(&path, Some(user.uid.as_raw()), Some(user.gid.as_raw()))?;
    }
    Ok(())
}

fn record_status(code: i32) -> i32 {
    // Its presence is the host's evidence that no workload writer remains.
    match fs::write(state_path().join("status"), code.to_string()) {
        Ok(()) => code,
        Err(error) => {
            eprintln!("marsh cloud: {error}");
            125
        }
    }
}

/// Where the workload's command links and their socket live.
fn link_dir() -> PathBuf {
    if privileged() {
        PathBuf::from(cloud::LINK_DIR)
    } else {
        PathBuf::from("/tmp/marsh-cloud/links")
    }
}

/// The link socket's path, given to the workload.
const LINK_VARIABLE: &str = "MARSH_CLOUD_LINK";

/// Command links for cloud-bound agent Kits: a stub per name and a socket
/// the workload user can reach. Each call is relayed to the host.
fn prepare_links(
    kits: &[String],
    user: &nix::unistd::User,
) -> io::Result<std::os::unix::net::UnixListener> {
    let dir = &link_dir();
    let bin = dir.join("bin");
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(&bin)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    for name in kits {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            continue;
        }
        let stub = bin.join(name);
        fs::write(
            &stub,
            format!("#!{} --cloud-link={name}\n", helper_path().display()),
        )?;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))?;
    }
    let socket = dir.join("cap.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket)?;
    std::os::unix::fs::chown(&socket, Some(user.uid.as_raw()), Some(user.gid.as_raw()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The workload's command: as the workload user, in its cwd (created if
/// the snapshot lacked it), with the request's environment over the trusted
/// sandbox settings. `terminal` starts it under `setsid --ctty`; `links`
/// puts the command links first on PATH. Stdio is the caller's to set.
fn workload_command(
    request: &CloudRequest,
    user: &nix::unistd::User,
    terminal: bool,
    links: bool,
) -> io::Result<Command> {
    let program = |command: &mut Command| {
        if terminal {
            command.arg("--ctty");
        }
    };
    let mut command = match request.mode {
        CloudMode::Shell => {
            let mut command = if terminal {
                let mut command = Command::new("setsid");
                program(&mut command);
                command.arg(helper_path());
                command
            } else {
                let mut command = Command::new(helper_path());
                command.arg0("marsh");
                command
            };
            command.args(request.argv.iter().map(|arg| OsStr::from_bytes(arg)));
            command
        }
        CloudMode::Argv => {
            let (first, rest) = request
                .argv
                .split_first()
                .ok_or_else(|| io::Error::other("empty cloud argv"))?;
            let mut command = if terminal {
                let mut command = Command::new("setsid");
                program(&mut command);
                command.arg(OsStr::from_bytes(first));
                command
            } else {
                Command::new(OsStr::from_bytes(first))
            };
            command.args(rest.iter().map(|arg| OsStr::from_bytes(arg)));
            command
        }
    };
    let cwd = PathBuf::from(OsString::from_vec(request.cwd.clone()));
    ensure_cwd(&cwd, user)?;
    // A Kit sandbox (no root): the workload is the Kit's own user, in the
    // environment the stock sandbox gives it (network proxy, credentials).
    let trusted = if privileged() {
        TRUSTED_ENV
            .iter()
            .filter_map(|name| Some(((*name).to_owned(), std::env::var_os(name)?)))
            .collect::<Vec<_>>()
    } else {
        std::env::vars_os()
            .filter(|(name, _)| {
                !matches!(name.to_str(), Some("HOME" | "USER" | "LOGNAME" | "PATH"))
            })
            .map(|(name, value)| (name.to_string_lossy().into_owned(), value))
            .collect::<Vec<_>>()
    };
    let path = {
        let base = if privileged() {
            PATH.to_owned()
        } else {
            std::env::var("PATH").unwrap_or_else(|_| PATH.to_owned())
        };
        if links {
            format!("{}/bin:{base}", link_dir().display())
        } else {
            base
        }
    };
    command
        .env_clear()
        .envs(
            request
                .env
                .iter()
                .filter(|(name, _)| {
                    !TRUSTED_ENV.contains(&name.as_str())
                        && !matches!(name.as_str(), "HOME" | "USER" | "LOGNAME" | "PATH")
                })
                .map(|(name, value)| (name, OsStr::from_bytes(value))),
        )
        .envs(trusted)
        .env("HOME", &user.dir)
        .env("USER", &user.name)
        .env("LOGNAME", &user.name)
        .env("PATH", path)
        .env(
            crate::cloud_split::ROOT_VARIABLE,
            OsStr::from_bytes(&request.root),
        )
        .env(LINK_VARIABLE, link_dir().join("cap.sock"))
        .current_dir(&cwd);
    if privileged() {
        command.uid(user.uid.as_raw()).gid(user.gid.as_raw());
    }
    Ok(command)
}

/// As root: run the request as the workload user in a new process group.
/// After its leader exits, every workload process is killed; only then is
/// the status recorded.
#[allow(clippy::too_many_lines)] // One ordered spawn, relay, stop, and status sequence.
fn run() -> i32 {
    type Started = (
        std::process::Child,
        i32,
        u32,
        Option<std::os::fd::OwnedFd>,
        Option<std::os::unix::net::UnixListener>,
    );
    // At most once per sandbox: a run whose start was not confirmed to the
    // host may be retried, and must then refuse rather than run again.
    let claimed = state_dir().and_then(|state| {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(state.join("started"))
    });
    if let Err(error) = claimed {
        eprintln!("marsh cloud: the command was already started in this sandbox: {error}");
        return 125;
    }
    // The start signal: an empty acknowledgement, before anything else.
    {
        let mut out = io::stdout().lock();
        if marsh_contracts::cloud::run_frame::write(
            &mut out,
            marsh_contracts::cloud::run_frame::ACK,
            &0_u64.to_be_bytes(),
        )
        .and_then(|()| io::Write::flush(&mut out))
        .is_err()
        {
            return 125;
        }
    }
    // The workload's stdin pipe when the caller's stdin is a terminal.
    let mut stdin_writer: Option<std::os::fd::OwnedFd> = None;
    let started = (|| -> io::Result<Started> {
        let state = state_dir()?;
        let user = run_user()?;
        if !privileged() {
            // Marks the workload (inherited, irreversible) apart from the
            // stock exec sessions of the same user.
            fs::write(state.join("runner"), std::process::id().to_string())?;
            #[cfg(target_os = "linux")]
            nix::sys::prctl::set_no_new_privs().map_err(io::Error::from)?;
        }
        let request: CloudRequest =
            serde_json::from_slice(&fs::read(state.join("request"))?).map_err(io::Error::other)?;
        // A terminal run starts in its own session with the pty as its
        // controlling terminal (util-linux `setsid --ctty`, which execs in
        // place: the child is not a group leader, so its pid is the group).
        let terminal = request
            .terminal
            .map(|(rows, columns)| {
                nix::pty::openpty(
                    Some(&nix::pty::Winsize {
                        ws_row: rows,
                        ws_col: columns,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    }),
                    None,
                )
                .map_err(io::Error::other)
            })
            .transpose()?;
        let links = (!request.kits.is_empty())
            .then(|| prepare_links(&request.kits, &user))
            .transpose()?;
        let mut command = workload_command(&request, &user, terminal.is_some(), links.is_some())?;
        let master = if let Some(pty) = terminal {
            if !request.env.contains_key("TERM") {
                command.env("TERM", "xterm-256color");
            }
            command
                .stdin(Stdio::from(pty.slave.try_clone()?))
                .stdout(Stdio::from(pty.slave.try_clone()?))
                .stderr(Stdio::from(pty.slave));
            Some(pty.master)
        } else {
            if request.stdin_terminal {
                // The caller typed into a terminal: name this exact pipe so
                // `fanout` treats it as a terminal (empty input), as it would
                // locally, while `cat` and `read` still get the keystrokes.
                let (read, write) = nix::unistd::pipe().map_err(io::Error::from)?;
                nix::fcntl::fcntl(
                    &write,
                    nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
                )
                .map_err(io::Error::from)?;
                command.env(
                    marsh_contracts::cloud::TERMINAL_STDIN,
                    pipe_identity(&read)?,
                );
                command.stdin(Stdio::from(read));
                stdin_writer = Some(write);
            } else {
                command.stdin(Stdio::piped());
            }
            command
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            None
        };
        let child = command.spawn()?;
        let group = i32::try_from(child.id()).unwrap_or(i32::MAX);
        // The workload runs from here on; a failed record only weakens INT
        // delivery (KILL still stops every process of the user).
        if let Err(error) = fs::write(state.join("pgid"), group.to_string()) {
            eprintln!("marsh cloud: {error}");
        }
        Ok((child, group, user.uid.as_raw(), master, links))
    })();
    let (mut child, group, uid, master, links) = match started {
        Ok(started) => started,
        Err(error) => {
            eprintln!("marsh cloud: {error}");
            // Nothing of the workload started (or it is killed below).
            let stopped = run_user().is_ok_and(|user| kill_workload(user.uid.as_raw()));
            // No workload output exists; tell the host none was lost.
            let _ = marsh_contracts::cloud::run_frame::write(
                &mut io::stdout(),
                marsh_contracts::cloud::run_frame::END,
                &[],
            );
            return if stopped { record_status(127) } else { 125 };
        }
    };
    if let Some(writer) = stdin_writer.take() {
        child.stdin = Some(std::process::ChildStdin::from(writer));
    }
    let pumps = relay_for(&mut child, master, links);
    let status = child.wait();
    let _ = nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(group),
        nix::sys::signal::Signal::SIGKILL,
    );
    let code = match status {
        Ok(status) => status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(125),
        Err(error) => {
            eprintln!("marsh cloud: {error}");
            125
        }
    };
    if !kill_workload(uid) {
        eprintln!("marsh cloud: workload processes did not stop; no status recorded");
        return 125;
    }
    // Every holder of the output pipes is gone: drain them to the end.
    for pump in pumps {
        let _ = pump.join();
    }
    record_status(code)
}

/// The stream relay for a started workload: its pty, or its three pipes.
/// `DEV:INO` of a pipe end (both ends share the inode).
fn pipe_identity(fd: &impl std::os::fd::AsFd) -> io::Result<String> {
    let stat = nix::sys::stat::fstat(fd).map_err(io::Error::from)?;
    Ok(format!("{}:{}", stat.st_dev, stat.st_ino))
}

fn relay_for(
    child: &mut std::process::Child,
    master: Option<std::os::fd::OwnedFd>,
    links: Option<std::os::unix::net::UnixListener>,
) -> Vec<std::thread::JoinHandle<()>> {
    use marsh_contracts::cloud::run_frame;
    if let Some(master) = master {
        let (Ok(reader), Ok(writer)) = (master.try_clone(), master.try_clone()) else {
            return Vec::new();
        };
        return relay_streams(
            Some(Box::new(File::from(writer))),
            vec![(
                Box::new(File::from(reader)) as Box<dyn io::Read + Send>,
                run_frame::STDOUT,
            )],
            Some(master),
            links,
        );
    }
    let mut outputs: Vec<(Box<dyn io::Read + Send>, u8)> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        outputs.push((Box::new(stdout), run_frame::STDOUT));
    }
    if let Some(stderr) = child.stderr.take() {
        outputs.push((Box::new(stderr), run_frame::STDERR));
    }
    relay_streams(
        child
            .stdin
            .take()
            .map(|stdin| Box::new(stdin) as Box<dyn Write + Send>),
        outputs,
        None,
        links,
    )
}

/// One registered-command call: its request line, then its stdin frames,
/// relayed to the host with the call's id. Replies arrive on the carrier.
type CallReplies =
    std::sync::Mutex<std::collections::HashMap<u32, std::sync::mpsc::Sender<(u8, Vec<u8>)>>>;

fn relay_call<F>(
    id: u32,
    mut stream: std::os::unix::net::UnixStream,
    send: &F,
    window: &OutputWindow,
    calls: &CallReplies,
) where
    F: Fn(u8, &[u8]) -> io::Result<()> + Clone + Send + 'static,
{
    use marsh_contracts::cloud::run_frame;
    let tagged = |body: &[u8]| {
        let mut payload = id.to_be_bytes().to_vec();
        payload.extend_from_slice(body);
        payload
    };
    let Ok(Some((run_frame::DATA, request))) = run_frame::read(&mut stream) else {
        return;
    };
    let Ok(mut reply) = stream.try_clone() else {
        return;
    };
    // Replies to this call are written by their own thread: a caller that
    // stops reading never blocks the control reader (or other calls).
    let (replies, queue) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();
    {
        let send = send.clone();
        std::thread::spawn(move || {
            while let Ok((tag, body)) = queue.recv() {
                let _ = run_frame::write(&mut reply, tag, &body);
                if tag == run_frame::EXIT {
                    let _ = reply.shutdown(std::net::Shutdown::Both);
                    return;
                }
                // Delivered (or the caller is gone): open this call's window.
                let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
                let mut ack = id.to_be_bytes().to_vec();
                ack.extend_from_slice(&len.to_be_bytes());
                let _ = send(run_frame::KIT_ACK, &ack);
            }
        });
    }
    calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(id, replies);
    window.reserve(u64::try_from(request.len()).unwrap_or(u64::MAX));
    if send(run_frame::KIT_OPEN, &tagged(&request)).is_err() {
        return;
    }
    while let Ok(Some((run_frame::DATA, bytes))) = run_frame::read(&mut stream) {
        window.reserve(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        if send(run_frame::KIT_IN, &tagged(&bytes)).is_err() {
            return;
        }
    }
    let _ = send(run_frame::KIT_IN_EOF, &tagged(&[]));
}

/// Output bytes sent but not yet acknowledged by the host.
#[derive(Default)]
struct OutputWindow {
    state: std::sync::Mutex<(u64, bool)>,
    changed: std::sync::Condvar,
}

impl OutputWindow {
    fn reserve(&self, len: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.1
            && state.0 > 0
            && state.0 + len > marsh_contracts::cloud::run_frame::OUTPUT_WINDOW
        {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.0 += len;
    }

    fn acknowledge(&self, len: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = state.0.saturating_sub(len);
        self.changed.notify_all();
    }

    /// The host is gone: never wait for it again.
    fn open(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = true;
        self.changed.notify_all();
    }
}

/// Relay the workload's streams over this carrier as `run_frame`s. Stdin
/// arrives framed from the host; each chunk is acknowledged once the workload
/// accepted it. Output is sent within the host-acknowledged window. The
/// control reader never blocks on the workload, so acknowledgements always
/// flow. The returned pumps finish when every output holder is gone.
#[allow(clippy::too_many_lines)] // Control reader, stdin writer, and pumps share one window.
fn relay_streams(
    stdin: Option<Box<dyn Write + Send>>,
    outputs: Vec<(Box<dyn io::Read + Send>, u8)>,
    terminal: Option<std::os::fd::OwnedFd>,
    links: Option<std::os::unix::net::UnixListener>,
) -> Vec<std::thread::JoinHandle<()>> {
    use marsh_contracts::cloud::run_frame;
    use std::io::Read as _;
    let out = std::sync::Arc::new(std::sync::Mutex::new(io::stdout()));
    let send = {
        let out = std::sync::Arc::clone(&out);
        move |tag: u8, payload: &[u8]| {
            let mut out = out
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            run_frame::write(&mut *out, tag, payload)
        }
    };
    let window = std::sync::Arc::new(OutputWindow::default());
    // Workload stdin writer: fed by the control reader, bounded by the host's
    // stdin window.
    let (input, queue) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
    if let Some(mut stdin) = stdin {
        let send = send.clone();
        std::thread::spawn(move || {
            while let Ok(Some(bytes)) = queue.recv() {
                if stdin.write_all(&bytes).is_err() {
                    let _ = send(run_frame::CLOSED, &[]);
                    return;
                }
                let accepted = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                if send(run_frame::ACK, &accepted.to_be_bytes()).is_err() {
                    return;
                }
            }
        });
    }
    // Registered-command calls from the workload, relayed to the host.
    let calls: std::sync::Arc<CallReplies> = std::sync::Arc::default();
    if let Some(listener) = links {
        let send = send.clone();
        let window = std::sync::Arc::clone(&window);
        let calls = std::sync::Arc::clone(&calls);
        std::thread::spawn(move || {
            let mut next = 1_u32;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let id = next;
                next = next.wrapping_add(1).max(1);
                let (send, window, calls) = (
                    send.clone(),
                    std::sync::Arc::clone(&window),
                    std::sync::Arc::clone(&calls),
                );
                std::thread::spawn(move || relay_call(id, stream, &send, &window, &calls));
            }
        });
    }
    {
        let window = std::sync::Arc::clone(&window);
        let send = send.clone();
        let calls = std::sync::Arc::clone(&calls);
        // Not joined: it may wait on a host that never ends its stdin.
        std::thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut input = Some(input);
            loop {
                match run_frame::read(&mut stdin) {
                    Ok(Some((run_frame::DATA, bytes))) => {
                        if let Some(sender) = &input
                            && sender.send(Some(bytes)).is_err()
                        {
                            input = None;
                        }
                    }
                    Ok(Some((run_frame::EOF, _))) => {
                        if let Some(sender) = input.take() {
                            let _ = sender.send(None);
                        }
                    }
                    Ok(Some((run_frame::RESIZE, size))) => {
                        if let (Some(terminal), [r0, r1, c0, c1]) = (&terminal, size.as_slice()) {
                            let _ = rustix::termios::tcsetwinsize(
                                terminal,
                                rustix::termios::Winsize {
                                    ws_row: u16::from_be_bytes([*r0, *r1]),
                                    ws_col: u16::from_be_bytes([*c0, *c1]),
                                    ws_xpixel: 0,
                                    ws_ypixel: 0,
                                },
                            );
                        }
                    }
                    Ok(Some((
                        tag @ (run_frame::KIT_OUT | run_frame::KIT_ERR | run_frame::KIT_EXIT),
                        payload,
                    ))) if payload.len() >= 4 => {
                        let id =
                            u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                        let body = &payload[4..];
                        let local = match tag {
                            run_frame::KIT_OUT => run_frame::STDOUT,
                            run_frame::KIT_ERR => run_frame::STDERR,
                            _ => run_frame::EXIT,
                        };
                        let mut calls = calls
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let queued = calls
                            .get(&id)
                            .is_some_and(|replies| replies.send((local, body.to_vec())).is_ok());
                        if tag == run_frame::KIT_EXIT {
                            calls.remove(&id);
                        } else if !queued {
                            // No caller left: open the call's window here.
                            let len = u64::try_from(body.len()).unwrap_or(u64::MAX);
                            let mut ack = id.to_be_bytes().to_vec();
                            ack.extend_from_slice(&len.to_be_bytes());
                            let _ = send(run_frame::KIT_ACK, &ack);
                        }
                    }
                    Ok(Some((run_frame::OUTPUT_ACK, count))) => {
                        window.acknowledge(count.try_into().map_or(u64::MAX, u64::from_be_bytes));
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => {
                        window.open();
                        if let Some(sender) = input.take() {
                            let _ = sender.send(None);
                        }
                        return;
                    }
                }
            }
        });
    }
    let mut pumps = Vec::new();
    for (mut stream, tag) in outputs {
        let send = send.clone();
        let window = std::sync::Arc::clone(&window);
        pumps.push(std::thread::spawn(move || {
            let mut buffer = vec![0_u8; run_frame::MAX_PAYLOAD];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        window.reserve(u64::try_from(count).unwrap_or(u64::MAX));
                        if send(tag, &buffer[..count]).is_err() {
                            break;
                        }
                    }
                }
            }
        }));
    }
    let send_end = send;
    let joined = std::thread::spawn(move || {
        for pump in pumps {
            let _ = pump.join();
        }
        let _ = send_end(run_frame::END, &[]);
    });
    vec![joined]
}

/// Print the recorded exit status; fails when the command has not ended.
fn status() -> io::Result<()> {
    let code = fs::read_to_string(state_path().join("status"))?;
    println!("{}", code.trim());
    Ok(())
}

fn signal(name: &OsStr) -> io::Result<()> {
    let signal = match name.as_bytes() {
        b"INT" => nix::sys::signal::Signal::SIGINT,
        b"TERM" => nix::sys::signal::Signal::SIGTERM,
        b"KILL" => nix::sys::signal::Signal::SIGKILL,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown signal",
            ));
        }
    };
    // A signal can race the workload's start: wait briefly for its group.
    let pgid = state_path().join("pgid");
    for _ in 0..50 {
        if pgid.exists() || state_path().join("status").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if state_path().join("status").exists() {
        // Ended: the recorded group may be reused by an unrelated process.
        return Ok(());
    }
    let group: i32 = fs::read_to_string(&pgid)?
        .trim()
        .parse()
        .map_err(io::Error::other)?;
    let result = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), signal);
    if signal == nix::sys::signal::Signal::SIGKILL {
        kill_workload(run_user()?.uid.as_raw());
    }
    match result {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(io::Error::other(error)),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Entry {
    Directory(u32),
    File(u32, Vec<u8>),
    Symlink(Vec<u8>),
}

fn baseline() -> io::Result<BTreeMap<Vec<u8>, Entry>> {
    let mut reader = Reader::new(
        BufReader::new(File::open(state_path().join("baseline"))?),
        u64::MAX,
    )?;
    let mut entries = BTreeMap::new();
    while let Some(header) = reader.next_header()? {
        let entry = match header.kind {
            Kind::Directory => Entry::Directory(header.mode),
            Kind::File => Entry::File(header.mode, reader.data()?),
            Kind::Symlink => Entry::Symlink(reader.data()?),
            Kind::Delete | Kind::Request => return Err(io::Error::other("invalid baseline")),
        };
        entries.insert(header.path, entry);
    }
    Ok(entries)
}

fn walk(
    root: &Path,
    relative: &[u8],
    owner: u32,
    out: &mut BTreeMap<Vec<u8>, Entry>,
) -> io::Result<()> {
    let mut children = fs::read_dir(join_dir(root, relative))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    children.sort();
    for name in children {
        let path = if relative.is_empty() {
            name.as_bytes().to_vec()
        } else {
            [relative, b"/", name.as_bytes()].concat()
        };
        if cloud::protected(&path) || !cloud::valid_relative(&path) {
            continue;
        }
        let full = join(root, &path);
        let metadata = fs::symlink_metadata(&full)?;
        let mode = metadata.permissions().mode() & 0o777;
        if metadata.file_type().is_symlink() {
            out.insert(
                path,
                Entry::Symlink(fs::read_link(&full)?.into_os_string().into_vec()),
            );
        } else if metadata.is_dir() {
            out.insert(path.clone(), Entry::Directory(mode));
            walk(root, &path, owner, out)?;
        } else if metadata.is_file() {
            // Every workload file belongs to the workload user; another
            // owner means a hard link to a file it may not read.
            if std::os::unix::fs::MetadataExt::uid(&metadata) != owner {
                return Err(io::Error::other(format!(
                    "{}: not owned by the workload user",
                    String::from_utf8_lossy(&path)
                )));
            }
            out.insert(path, Entry::File(mode, hash_file(&full)?));
        }
    }
    Ok(())
}

fn join_dir(root: &Path, relative: &[u8]) -> PathBuf {
    if relative.is_empty() {
        root.to_path_buf()
    } else {
        join(root, relative)
    }
}

#[allow(clippy::naive_bytecount)] // Short paths; no bytecount dependency.
fn depth(path: &[u8]) -> usize {
    path.iter().filter(|byte| **byte == b'/').count()
}

/// Write the exact delta from the baseline: deletions deepest first, then
/// new or changed directories shallowest first, then files and symlinks.
fn delta(root: &Path, out: &Path) -> io::Result<()> {
    if !state_path().join("status").exists() {
        return Err(io::Error::other("the command has not ended; no capture"));
    }
    let output: Box<dyn Write> = if out == Path::new("-") {
        Box::new(io::stdout().lock())
    } else {
        Box::new(File::create(out)?)
    };
    write_delta(root, output)
}

/// `sync ROOT`: the delta so far while the command still runs (`marsh cloud
/// sync`), on stdout. It is staged first: a file the workload is writing
/// may change size while it is read, and then the walk is retried.
fn sync_delta(root: &Path) -> io::Result<()> {
    let state = state_dir()?;
    let staged = state.join("sync.delta");
    let mut last = None;
    for _ in 0..3 {
        match write_delta(root, Box::new(File::create(&staged)?)) {
            Ok(()) => {
                let mut file = File::open(&staged)?;
                let mut out = io::stdout().lock();
                io::copy(&mut file, &mut out)?;
                let _ = fs::remove_file(&staged);
                return out.flush();
            }
            Err(error) => last = Some(error),
        }
    }
    let _ = fs::remove_file(&staged);
    Err(last.unwrap_or_else(|| io::Error::other("sync failed")))
}

/// `rebase`: a synced change set's entries (stdin; files carry their
/// SHA-256) become the baseline for those paths, so later deltas carry only
/// newer changes.
fn rebase() -> io::Result<()> {
    let mut entries = baseline()?;
    let mut reader = Reader::new(BufReader::new(io::stdin().lock()), cloud::MAX_ARCHIVE)?;
    while let Some(header) = reader.next_header()? {
        if cloud::protected(&header.path) {
            return Err(io::Error::other("a rebase cannot name protected paths"));
        }
        match header.kind {
            Kind::Delete => {
                entries.remove(&header.path);
            }
            Kind::Directory => {
                entries.insert(header.path, Entry::Directory(header.mode));
            }
            Kind::File => {
                let digest = reader.data()?;
                entries.insert(header.path, Entry::File(header.mode, digest));
            }
            Kind::Symlink => {
                let target = reader.data()?;
                entries.insert(header.path, Entry::Symlink(target));
            }
            Kind::Request => return Err(io::Error::other("a rebase carries no request")),
        }
    }
    let state = state_dir()?;
    let next = state.join("baseline.new");
    let mut writer = Writer::new(BufWriter::new(File::create(&next)?), u64::MAX)?;
    for (path, entry) in &entries {
        match entry {
            Entry::Directory(mode) => writer.directory(path, *mode)?,
            Entry::File(mode, digest) => {
                writer.file(path, *mode, digest.len() as u64, &mut &digest[..])?;
            }
            Entry::Symlink(target) => writer.symlink(path, target)?,
        }
    }
    writer
        .finish()?
        .into_inner()
        .map_err(io::Error::other)?
        .sync_all()?;
    fs::rename(next, state.join("baseline"))
}

fn write_delta(root: &Path, output: Box<dyn Write>) -> io::Result<()> {
    absolute(root)?;
    let before = baseline()?;
    let mut after = BTreeMap::new();
    walk(root, b"", run_user()?.uid.as_raw(), &mut after)?;
    let mut deletions = before
        .iter()
        .filter(|(path, entry)| match after.get(*path) {
            None => true,
            Some(now) => std::mem::discriminant(now) != std::mem::discriminant(*entry),
        })
        .map(|(path, _)| path.clone())
        .collect::<Vec<_>>();
    deletions.sort_by_key(|path| std::cmp::Reverse(depth(path)));
    let changed = after
        .iter()
        .filter(|(path, entry)| before.get(*path) != Some(*entry))
        .collect::<Vec<_>>();
    let mut writer = Writer::new(BufWriter::new(output), cloud::MAX_ARCHIVE)?;
    for path in &deletions {
        writer.delete(path)?;
    }
    let mut directories = changed
        .iter()
        .filter_map(|(path, entry)| match entry {
            Entry::Directory(mode) => Some((*path, *mode)),
            _ => None,
        })
        .collect::<Vec<_>>();
    directories.sort_by_key(|(path, _)| depth(path));
    for (path, mode) in directories {
        writer.directory(path, mode)?;
    }
    for (path, entry) in &changed {
        match entry {
            Entry::File(mode, _) => {
                let context = |error: io::Error| {
                    io::Error::new(
                        error.kind(),
                        format!("{}: {error}", String::from_utf8_lossy(path)),
                    )
                };
                let mut file = OpenOptions::new()
                    .read(true)
                    .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
                    .open(join(root, path))
                    .map_err(context)?;
                let len = file.metadata()?.len();
                writer.file(path, *mode, len, &mut file).map_err(context)?;
            }
            Entry::Symlink(target) => writer.symlink(path, target)?,
            Entry::Directory(_) => {}
        }
    }
    writer.finish()?.flush()
}

/// As root: snapshot the workspace's current files (no ignore filtering: a
/// crossing from a materialized view keeps generated files) with the private
/// Git copy, carrying the request read from stdin; gzip on stdout. Feeds an
/// agent sandbox's `unpack`.
fn archive(root: &Path) -> io::Result<()> {
    use std::io::Read as _;
    absolute(root)?;
    let mut request = Vec::new();
    io::stdin()
        .lock()
        .take(cloud::MAX_REQUEST + 1)
        .read_to_end(&mut request)?;
    let request: CloudRequest = serde_json::from_slice(&request).map_err(io::Error::other)?;
    let dir = marsh_daemon::split_fs::Dir::open_root(root)?;
    let out = flate2::write::GzEncoder::new(
        BufWriter::new(io::stdout().lock()),
        flate2::Compression::fast(),
    );
    let (out, _, _) = marsh_daemon::cloud_fs::write_snapshot(
        &dir,
        false,
        &marsh_daemon::cloud_fs::GitView::Project,
        &request,
        out,
        marsh_daemon::cloud_fs::SNAPSHOT_LIMIT,
    )
    .map_err(io::Error::other)?;
    out.finish()?.flush()
}

/// As the workload user: apply an agent call's returned delta (stdin) to the
/// live workspace. Prints the report as JSON; nonzero on a failed apply.
fn apply(root: &Path) -> io::Result<()> {
    absolute(root)?;
    let mut staged = tempfile::NamedTempFile::new()?;
    io::copy(&mut io::stdin().lock(), &mut staged)?;
    staged.flush()?;
    let identity = std::os::unix::fs::MetadataExt::dev(&fs::metadata(root)?);
    let inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(root)?);
    let result = marsh_daemon::cloud_fs::apply_delta(
        root,
        (identity, inode),
        staged.path(),
        cloud::MAX_ARCHIVE,
    );
    let (report, error) = match result {
        Ok(report) => (report, None),
        Err(error) => (error.applied, Some(error.message)),
    };
    println!(
        "{}",
        serde_json::json!({
            "created": report.created,
            "modified": report.modified,
            "deleted": report.deleted,
            "kept": report.kept,
            "error": error,
        })
    );
    match error {
        Some(message) => Err(io::Error::other(message)),
        None => Ok(()),
    }
}

/// `--cloud-link=NAME` (a command link in a cloud workload): call the
/// cloud-bound agent NAME through the helper and relay its streams.
#[must_use]
pub fn link(name: &str, arguments: &[OsString]) -> i32 {
    use marsh_contracts::cloud::{LinkRequest, run_frame};
    use std::io::Read as _;
    if std::env::var_os(detached::DETACHED_VARIABLE).is_some() {
        eprintln!(
            "marsh: {name}: this detached cloud run cannot start agent sandboxes: turn that on with `marsh config cloud-delegate on` and a Docker access token in your Keychain (`security add-generic-password -s marsh-cloud-delegate -a YOUR_DOCKER_ID -w`); or run `cloud -d {{ {name} ...; }}` on its own, or the block without -d"
        );
        return 126;
    }
    let request = LinkRequest {
        command: name.to_owned(),
        argv: arguments
            .iter()
            .map(|arg| arg.as_bytes().to_vec())
            .collect(),
        cwd: std::env::current_dir()
            .map(|cwd| cwd.into_os_string().into_vec())
            .unwrap_or_default(),
        env: std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_vec())))
            .filter(|(name, _)| marsh_daemon::split::forwardable(name))
            .collect(),
    };
    let socket = std::env::var_os(LINK_VARIABLE).map_or_else(
        || Path::new(cloud::LINK_DIR).join("cap.sock"),
        PathBuf::from,
    );
    let mut stream = match std::os::unix::net::UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("marsh: {name}: cannot reach the cloud link: {error}");
            return 125;
        }
    };
    let Ok(body) = serde_json::to_vec(&request) else {
        return 125;
    };
    if body.len() > run_frame::MAX_PAYLOAD
        || run_frame::write(&mut stream, run_frame::DATA, &body).is_err()
    {
        eprintln!("marsh: {name}: the call is too large");
        return 125;
    }
    if let Ok(mut input) = stream.try_clone() {
        std::thread::spawn(move || {
            let mut buffer = vec![0_u8; 16 * 1024];
            let mut stdin = io::stdin().lock();
            loop {
                match stdin.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if run_frame::write(&mut input, run_frame::DATA, &buffer[..count]).is_err()
                        {
                            return;
                        }
                    }
                }
            }
            let _ = run_frame::write(&mut input, run_frame::EOF, &[]);
        });
    }
    let (mut stdout, mut stderr) = (io::stdout(), io::stderr());
    loop {
        match run_frame::read(&mut stream) {
            Ok(Some((run_frame::STDOUT, bytes))) => {
                let _ = stdout.write_all(&bytes).and_then(|()| stdout.flush());
            }
            Ok(Some((run_frame::STDERR, bytes))) => {
                let _ = stderr.write_all(&bytes).and_then(|()| stderr.flush());
            }
            Ok(Some((run_frame::EXIT, code))) if code.len() == 4 => {
                return i32::from_be_bytes([code[0], code[1], code[2], code[3]]);
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                eprintln!("marsh: {name}: the cloud link closed before the call ended");
                return 125;
            }
        }
    }
}
