//! Bind-source pinning (A3).
//!
//! Docker re-resolves a bind source on every `start`, and the project folder
//! is writable by the job at the same time, so rewriting the path is not a
//! fix. Instead the source is resolved once, beneath a held project-root fd
//! (`openat2` with `RESOLVE_BENEATH|NO_MAGICLINKS|NO_XDEV`), cloned with
//! `open_tree(OPEN_TREE_CLONE)` and attached with `move_mount` at a root-only
//! path. Docker is pointed at that path, which no job can re-aim.
//!
//! `.git` and `.marsh` are protected: a pin of either (or inside either) is
//! read-only, and a pin of the project root carries them as read-only
//! sub-mounts. Pins are recreated from the ledger by [`restore_all`] before the
//! production engine starts. A recreated pin follows its recorded name, so a
//! directory renamed into that name while the engine was down is what it
//! binds; if that is `.git` or `.marsh` (or inside one), the pin is read-only.
// Pedantic lints this module trades for readability: the HTTP and policy code is
// long, table-like and full of one-letter names for a request, a head and a
// connection.
#![allow(
    clippy::missing_errors_doc,
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::format_push_string,
    clippy::collapsible_if,
    clippy::collapsible_else_if
)]

use std::io;
use std::path::Path;

use crate::ledger::{Ledger, Paths};

pub const PROTECTED: [&str; 2] = [".git", ".marsh"];

#[derive(Debug)]
pub enum PinError {
    /// The request is refused, with a reason fit for the client.
    Refused(String),
    /// The agent could not do its job (mounts unavailable, disk error).
    Internal(String),
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::Refused(s) | PinError::Internal(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    /// Pins left absent (`id`, reason). Each is poisoned (a dangling symlink at
    /// its path) and flagged in the ledger, and every container holding one
    /// is listed in `blocked_containers`: it must not start (A3).
    pub failed: Vec<(String, String)>,
    pub blocked_containers: Vec<String>,
    /// Substituted-socket paths that exist again (bound, not yet served).
    pub sockets_reserved: usize,
}

/// Recreate every pin in the ledger and reserve the substituted-socket paths.
/// The engine-up entry point calls this before the production dockerd starts
/// (A3); an `Err` means restoration itself could not run and the engine must
/// not start. Per-pin failures are in the report and fail closed.
pub fn restore_all() -> io::Result<RestoreReport> {
    let paths = Paths::default();
    let ledger = Ledger::open(&paths.ledger)?;
    restore_all_in(&paths, &ledger)
}

pub fn restore_all_in(paths: &Paths, ledger: &Ledger) -> io::Result<RestoreReport> {
    let mut rep = imp::restore_all_in(paths, ledger)?;
    rep.sockets_reserved = reserve_socket_paths(paths, ledger)?;
    Ok(rep)
}

/// A bind of a socket path that does not exist makes dockerd create a
/// *directory* there, which the agent could never serve. Leave a socket file
/// (bound, then dropped: connections are refused until the agent serves it).
pub fn reserve_socket_paths(paths: &Paths, ledger: &Ledger) -> io::Result<usize> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let mut n = 0;
    for rec in ledger.sockets() {
        let dir = paths.socket_root.join(&rec.scope).join(&rec.attempt);
        std::fs::create_dir_all(&dir)?;
        for d in [
            &paths.socket_root,
            &paths.socket_root.join(&rec.scope),
            &dir,
        ] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join("docker.sock");
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_socket()) {
            n += 1;
            continue;
        }
        let _ = std::fs::remove_dir(&path);
        let _ = std::fs::remove_file(&path);
        drop(std::os::unix::net::UnixListener::bind(&path)?);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
        n += 1;
    }
    Ok(n)
}

pub use imp::{PinManager, ProjectRoot, observe};

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn random_id() -> io::Result<String> {
    use std::io::Read as _;
    let mut b = [0u8; 12];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().fold(String::new(), |mut acc, x| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{x:02x}");
        acc
    }))
}

/// Split the client's absolute source into root-relative components.
/// Unclean paths are refused: Docker's CLI cleans them, so an unclean one is
/// not from a normal client.
pub fn relative_components(project_root: &Path, source: &str) -> Result<Vec<String>, PinError> {
    let root = project_root
        .to_str()
        .ok_or_else(|| PinError::Internal("project root is not UTF-8".into()))?
        .trim_end_matches('/');
    let rest = if source == root {
        ""
    } else {
        source
            .strip_prefix(root)
            .and_then(|r| r.strip_prefix('/'))
            .ok_or_else(|| PinError::Refused("bind source is outside the project".into()))?
    };
    if rest.is_empty() {
        return if source == root {
            Ok(Vec::new())
        } else {
            Err(PinError::Refused(
                "bind source must be a clean absolute path inside the project".into(),
            ))
        };
    }
    let mut out = Vec::new();
    for c in rest.split('/') {
        if c.is_empty() || c == "." || c == ".." {
            return Err(PinError::Refused(
                "bind source must be a clean absolute path inside the project".into(),
            ));
        }
        out.push(c.to_string());
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{PROTECTED, PinError, RestoreReport, random_id, relative_components};
    use crate::breaks::break_mode;
    use crate::ledger::{Ledger, Paths, PinRec, ProtectedId, now};
    use rustix::fd::{AsFd, AsRawFd, OwnedFd};
    use rustix::fs::{
        AtFlags, CWD, Dir, FileType, Mode, OFlags, ResolveFlags, fstat, mkdirat, openat, openat2,
        readlinkat, statat,
    };
    use rustix::io::Errno;
    use rustix::mount::{
        MountFlags, MountPropagationFlags, MoveMountFlags, OpenTreeFlags, UnmountFlags,
        mount_change, mount_remount, move_mount, open_tree, unmount,
    };
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    /// A project root held open for the life of the scope.
    pub struct ProjectRoot {
        fd: OwnedFd,
        canonical: String,
        pub given: PathBuf,
    }

    impl ProjectRoot {
        /// Opens `path`. Below a `.marsh` component (a lane's copy, inside a
        /// project its jobs could write) no component may be a symlink, so
        /// a planted link can't move the root, and so everything pinned
        /// "beneath" it, out of the project.
        pub fn open(path: &Path) -> Result<Self, PinError> {
            let failed =
                |e: Errno| PinError::Internal(format!("open project root {}: {e}", path.display()));
            let parts = path.components().collect::<Vec<_>>();
            let split = parts
                .iter()
                .position(|part| part.as_os_str() == ".marsh")
                .unwrap_or(parts.len());
            let fd = openat(
                CWD,
                parts[..split].iter().collect::<PathBuf>(),
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(failed)?;
            let fd = if split == parts.len() {
                fd
            } else {
                openat2(
                    &fd,
                    parts[split..].iter().collect::<PathBuf>(),
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                    ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS | ResolveFlags::NO_SYMLINKS,
                )
                .map_err(|e| match e {
                    Errno::LOOP => PinError::Refused(format!(
                        "project root {} goes through a symbolic link",
                        path.display()
                    )),
                    e => failed(e),
                })?
            };
            let canonical = fd_path(&fd)?;
            Ok(Self {
                fd,
                canonical,
                given: path.to_path_buf(),
            })
        }

        #[must_use]
        pub fn canonical(&self) -> &str {
            &self.canonical
        }
    }

    fn fd_path(fd: &impl AsFd) -> Result<String, PinError> {
        let link = format!("/proc/self/fd/{}", fd.as_fd().as_raw_fd());
        let p = readlinkat(CWD, link, Vec::new())
            .map_err(|e| PinError::Internal(format!("readlink /proc/self/fd: {e}")))?;
        String::from_utf8(p.into_bytes())
            .map_err(|_| PinError::Internal("path is not UTF-8".into()))
    }

    fn refuse_errno(e: Errno, what: &str) -> PinError {
        match e {
            Errno::XDEV => PinError::Refused(format!(
                "{what} leaves the project or crosses a mount point"
            )),
            Errno::LOOP => PinError::Refused(format!(
                "{what} goes through a symbolic link that is not allowed"
            )),
            Errno::NOENT => PinError::Refused(format!("{what} does not exist")),
            Errno::NOTDIR => PinError::Refused(format!(
                "{what} goes through a path component that is not a directory"
            )),
            Errno::PERM | Errno::ACCESS => PinError::Refused(format!("{what} is not accessible")),
            e => PinError::Internal(format!("resolve {what}: {e}")),
        }
    }

    fn cstr_rel(comps: &[String]) -> String {
        if comps.is_empty() {
            ".".to_string()
        } else {
            comps.join("/")
        }
    }

    /// Resolve beneath the root. This is the whole defence against symlink
    /// and `..` tricks: the kernel confines the walk.
    fn resolve(
        root: &ProjectRoot,
        rel: &str,
        extra_deny: ResolveFlags,
        no_symlinks: bool,
    ) -> Result<OwnedFd, Errno> {
        let mut flags = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS | extra_deny;
        if no_symlinks {
            flags |= ResolveFlags::NO_SYMLINKS;
        }
        openat2(
            root.fd.as_fd(),
            rel,
            OFlags::PATH | OFlags::CLOEXEC,
            Mode::empty(),
            flags,
        )
    }

    fn create_missing(root: &ProjectRoot, comps: &[String]) -> Result<OwnedFd, PinError> {
        let mut cur = rustix::io::fcntl_dupfd_cloexec(&root.fd, 0)
            .map_err(|e| PinError::Internal(format!("dup: {e}")))?;
        for c in comps {
            let flags = ResolveFlags::BENEATH
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_XDEV
                | ResolveFlags::NO_SYMLINKS;
            let step = |cur: &OwnedFd| {
                openat2(
                    cur.as_fd(),
                    c.as_str(),
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                    flags,
                )
            };
            cur = match step(&cur) {
                Ok(fd) => fd,
                Err(Errno::NOENT) => {
                    mkdirat(cur.as_fd(), c.as_str(), Mode::from_raw_mode(0o755))
                        .map_err(|e| PinError::Internal(format!("mkdir {c}: {e}")))?;
                    step(&cur).map_err(|e| refuse_errno(e, "bind source"))?
                }
                Err(e) => return Err(refuse_errno(e, "bind source")),
            };
        }
        Ok(cur)
    }

    /// What each protected name resolves to now, by identity. The shared folder
    /// may be case-insensitive (APFS over virtiofs), so names cannot decide
    /// what is protected: `.GIT` is `.git`. Identity can.
    pub fn observe(root: &ProjectRoot) -> Result<Vec<ProtectedId>, PinError> {
        let mut ids = Vec::new();
        for name in PROTECTED {
            // The overlay at .git/.marsh may be a separate mount, so this walk
            // may cross it; it is still beneath the root.
            match resolve(root, name, ResolveFlags::empty(), false) {
                Ok(fd) => {
                    let st = fstat(&fd).map_err(|e| PinError::Internal(format!("fstat: {e}")))?;
                    ids.push(ProtectedId {
                        name: name.to_string(),
                        dev: st.st_dev as u64,
                        ino: st.st_ino as u64,
                    });
                }
                Err(Errno::NOENT) => {}
                Err(e) => return Err(refuse_errno(e, "protected path")),
            }
        }
        Ok(ids)
    }

    /// What is protected: what the names resolve to now, and what the ledger
    /// recorded earlier (the names may have been moved since).
    fn protected_set(
        root: &ProjectRoot,
        known: &[ProtectedId],
    ) -> Result<Vec<(u64, u64)>, PinError> {
        let mut ids: Vec<(u64, u64)> = observe(root)?.iter().map(|id| (id.dev, id.ino)).collect();
        if !break_mode("protected-identity-off") {
            for id in known {
                if !ids.contains(&(id.dev, id.ino)) {
                    ids.push((id.dev, id.ino));
                }
            }
        }
        Ok(ids)
    }

    fn identity(fd: &impl AsFd) -> Result<(u64, u64), PinError> {
        let st = fstat(fd).map_err(|e| PinError::Internal(format!("fstat: {e}")))?;
        Ok((st.st_dev as u64, st.st_ino as u64))
    }

    /// What [`PinManager::restore`] attaches for one ledger record: the
    /// root and source re-resolved by name, the protected sub-mounts, and
    /// the access (DA2-01: read-only when the name now leads into `.git` or
    /// `.marsh`).
    pub(super) struct Restoring {
        pub(super) root: ProjectRoot,
        pub(super) fd: OwnedFd,
        pub(super) is_dir: bool,
        pub(super) read_only: bool,
        pub(super) subs: Vec<String>,
    }

    pub(super) fn restore_plan(rec: &PinRec, known: &[ProtectedId]) -> Result<Restoring, PinError> {
        let root = ProjectRoot::open(Path::new(&rec.project_root))?;
        let fd = if break_mode("bind-repin-unchecked") {
            // Defect: re-resolve by plain path, following symlinks.
            let p = Path::new(&rec.project_root).join(&rec.rel);
            openat(CWD, p, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
                .map_err(|e| refuse_errno(e, "pinned path"))?
        } else {
            let rel = if rec.rel.is_empty() {
                ".".to_string()
            } else {
                rec.rel.clone()
            };
            resolve(&root, &rel, ResolveFlags::NO_XDEV, true)
                .map_err(|e| refuse_errno(e, "pinned path"))?
        };
        // The protected sub-mounts are recomputed from what exists now, not
        // from what existed when the pin was made.
        let subs: Vec<String> = if rec.rel.is_empty() && !break_mode("protected-subpin-off") {
            if !break_mode("protected-marsh-late-off") {
                ensure_marsh(&root)?;
            }
            root_subs(&root, &protected_set(&root, known)?)?
        } else {
            rec.subs.clone()
        };
        let read_only = restored_read_only(&root, &fd, rec, known)?;
        Ok(Restoring {
            root,
            fd,
            is_dir: rec.is_dir,
            read_only,
            subs,
        })
    }

    /// A re-made pin's access: what was recorded, or read-only when the
    /// name now leads into `.git` or `.marsh` (a directory renamed into the
    /// pinned name while the engine was down) (DA2-01).
    fn restored_read_only(
        root: &ProjectRoot,
        fd: &OwnedFd,
        rec: &PinRec,
        known: &[ProtectedId],
    ) -> Result<bool, PinError> {
        if rec.read_only || break_mode("protected-subpin-off") {
            return Ok(rec.read_only);
        }
        inside_protected(root, fd, rec.is_dir, &rec.rel, &protected_set(root, known)?)
    }

    /// Is the target a protected tree, or inside one? The target is compared by
    /// identity, and so is every ancestor: for a directory by walking up from
    /// the handle itself (`..`, so a rename of an ancestor after the open
    /// changes nothing), for a file by resolving its ancestors by name.
    fn inside_protected(
        root: &ProjectRoot,
        fd: &OwnedFd,
        is_dir: bool,
        rel_resolved: &str,
        ids: &[(u64, u64)],
    ) -> Result<bool, PinError> {
        if ids.is_empty() || rel_resolved.is_empty() {
            return Ok(false);
        }
        if is_dir {
            let root_id = identity(&root.fd)?;
            let mut current = rustix::io::fcntl_dupfd_cloexec(fd, 0)
                .map_err(|e| PinError::Internal(format!("dup: {e}")))?;
            for _ in 0..4096 {
                let here = identity(&current)?;
                if ids.contains(&here) {
                    return Ok(true);
                }
                if here == root_id {
                    return Ok(false);
                }
                let up = openat(
                    current.as_fd(),
                    "..",
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|e| refuse_errno(e, "protected path"))?;
                if identity(&up)? == here {
                    return Err(PinError::Refused(
                        "bind source moved out of the project".into(),
                    ));
                }
                current = up;
            }
            return Err(PinError::Refused("bind source is nested too deeply".into()));
        }
        let mut prefix = String::new();
        for comp in rel_resolved.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(comp);
            match resolve(root, &prefix, ResolveFlags::empty(), false) {
                Ok(fd) => {
                    if ids.contains(&identity(&fd)?) {
                        return Ok(true);
                    }
                }
                // Crossing into a mount here is refused for the pin itself.
                Err(Errno::XDEV | Errno::NOENT) => {}
                Err(e) => return Err(refuse_errno(e, "protected path")),
            }
        }
        Ok(false)
    }

    /// The names under a project root that a root pin carries read-only: the
    /// protected names, and whatever else in the root is a protected identity
    /// (a `.git` renamed in place).
    fn root_subs(root: &ProjectRoot, ids: &[(u64, u64)]) -> Result<Vec<String>, PinError> {
        let mut subs: Vec<String> = PROTECTED.iter().map(|s| (*s).to_string()).collect();
        if ids.is_empty() {
            return Ok(subs);
        }
        let listing = openat(
            root.fd.as_fd(),
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| PinError::Internal(format!("open project root for reading: {e}")))?;
        let dir = Dir::read_from(&listing)
            .map_err(|e| PinError::Internal(format!("read project root: {e}")))?;
        for entry in dir {
            let entry = entry.map_err(|e| PinError::Internal(format!("read project root: {e}")))?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let Ok(name) = std::str::from_utf8(name) else {
                continue;
            };
            let Ok(st) = statat(&listing, name, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            if ids.contains(&(st.st_dev, st.st_ino)) && !subs.iter().any(|known| known == name) {
                subs.push(name.to_string());
            }
        }
        Ok(subs)
    }

    /// A pin of the project root carries `.marsh` read-only. If it does not
    /// exist yet (the first checkpoint or split of a new project), make it, so
    /// that the later `.marsh/checkpoints` and `.marsh/split/*` are covered.
    /// `.git` is not created: an empty one would break `git init`.
    fn ensure_marsh(root: &ProjectRoot) -> Result<(), PinError> {
        match mkdirat(root.fd.as_fd(), ".marsh", Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(Errno::EXIST) => Ok(()),
            Err(e) => Err(PinError::Internal(format!("create .marsh: {e}"))),
        }
    }

    pub struct PinManager {
        pub bind_root: PathBuf,
    }

    pub struct Pinned {
        pub id: String,
        pub path: PathBuf,
        pub rec: PinRec,
    }

    impl PinManager {
        #[must_use]
        pub fn new(bind_root: &Path) -> Self {
            Self {
                bind_root: bind_root.to_path_buf(),
            }
        }

        fn scope_dir(&self, scope: &str) -> Result<PathBuf, PinError> {
            io_err(
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&self.bind_root),
                "create bind root",
            )?;
            io_err(
                std::fs::set_permissions(&self.bind_root, std::fs::Permissions::from_mode(0o700)),
                "chmod bind root",
            )?;
            let sd = self.bind_root.join(scope);
            io_err(
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&sd),
                "create scope dir",
            )?;
            Ok(sd)
        }

        /// Admission: resolve, clone, attach. The returned record is the
        /// ledger entry; the caller stores it.
        pub fn pin(
            &self,
            root: &ProjectRoot,
            scope: &str,
            source: &str,
            want_read_only: bool,
            create: bool,
            known: &[ProtectedId],
        ) -> Result<Pinned, PinError> {
            let comps = relative_components(&root.given, source)?;
            let rel = cstr_rel(&comps);
            let fd = if break_mode("filter-no-resolve") {
                // Defect: a plain open follows symlinks out of the project.
                openat(CWD, source, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
                    .map_err(|e| refuse_errno(e, "bind source"))?
            } else {
                let xdev = if break_mode("pin-allow-xdev") {
                    ResolveFlags::empty()
                } else {
                    ResolveFlags::NO_XDEV
                };
                match resolve(root, &rel, xdev, false) {
                    Ok(fd) => fd,
                    Err(Errno::NOENT) if create => create_missing(root, &comps)?,
                    Err(e) => return Err(refuse_errno(e, "bind source")),
                }
            };
            let st = fstat(&fd).map_err(|e| PinError::Internal(format!("fstat: {e}")))?;
            let is_dir = FileType::from_raw_mode(st.st_mode) == FileType::Directory;
            let resolved = fd_path(&fd)?;
            let rel_resolved = if break_mode("filter-no-resolve") {
                String::new()
            } else {
                let r = resolved
                    .strip_prefix(root.canonical())
                    .map(|r| r.trim_start_matches('/').to_string())
                    .ok_or_else(|| {
                        PinError::Refused("bind source resolved outside the project".into())
                    })?;
                if resolved.ends_with(" (deleted)") {
                    return Err(PinError::Refused("bind source was removed".into()));
                }
                r
            };
            let ids = if break_mode("protected-subpin-off") {
                Vec::new()
            } else {
                protected_set(root, known)?
            };
            let protected = if break_mode("protected-by-name") {
                // Defect: decide by the spelling of the first component.
                PROTECTED.contains(&rel_resolved.split('/').next().unwrap_or(""))
                    && !break_mode("protected-subpin-off")
            } else {
                inside_protected(root, &fd, is_dir, &rel_resolved, &ids)?
            };
            let read_only = want_read_only || protected;
            let id = random_id().map_err(|e| PinError::Internal(format!("random id: {e}")))?;
            let at_root = rel_resolved.is_empty() && !break_mode("filter-no-resolve");
            let subs: Vec<String> = if at_root && !break_mode("protected-subpin-off") {
                if !break_mode("protected-marsh-late-off") {
                    ensure_marsh(root)?;
                }
                root_subs(root, &ids)?
            } else {
                Vec::new()
            };
            let mut rec = PinRec {
                scope: scope.to_string(),
                project_root: root.given.to_string_lossy().into_owned(),
                rel: rel_resolved,
                read_only,
                is_dir,
                subs: Vec::new(),
                dev: st.st_dev as u64,
                ino: st.st_ino as u64,
                created: now(),
                failed: false,
            };
            if break_mode("bind-no-pin") {
                // Defect (the claimed fix): hand back the resolved path and
                // let Docker re-resolve it on every start.
                let path = PathBuf::from(&resolved);
                return Ok(Pinned {
                    id: String::new(),
                    path,
                    rec,
                });
            }
            let path = self.scope_dir(scope)?.join(&id);
            rec.subs = self.attach(root, &fd, &path, is_dir, read_only, &subs)?;
            Ok(Pinned { id, path, rec })
        }

        /// Clone `fd` onto `path` (and the protected sub-mounts). On any
        /// failure everything done so far is undone: fail closed.
        #[allow(clippy::unused_self)]
        fn attach(
            &self,
            root: &ProjectRoot,
            fd: &OwnedFd,
            path: &Path,
            is_dir: bool,
            read_only: bool,
            subs: &[String],
        ) -> Result<Vec<String>, PinError> {
            make_mountpoint(path, is_dir)?;
            let mut mounted: Vec<String> = Vec::new();
            let result = (|| -> Result<(), PinError> {
                clone_onto(fd, path)?;
                if read_only {
                    remount_ro(path)?;
                }
                for name in subs {
                    // The overlay at .git/.marsh may be a separate mount, so
                    // this walk may cross it; it is still beneath the root.
                    let sub = match resolve(root, name, ResolveFlags::empty(), false) {
                        Ok(fd) => fd,
                        Err(Errno::NOENT) => continue,
                        Err(e) => return Err(refuse_errno(e, "protected path")),
                    };
                    let target = path.join(name);
                    clone_onto(&sub, &target)?;
                    mounted.push(name.clone());
                    remount_ro(&target)?;
                }
                Ok(())
            })();
            if let Err(e) = result {
                for n in mounted.iter().rev() {
                    let _ = unmount(path.join(n), UnmountFlags::DETACH);
                }
                let _ = unmount(path, UnmountFlags::DETACH);
                remove_mountpoint(path, is_dir);
                return Err(e);
            }
            Ok(mounted)
        }

        /// Recreate a pin at its original path from the ledger record.
        pub fn restore(
            &self,
            id: &str,
            rec: &PinRec,
            known: &[ProtectedId],
        ) -> Result<(), PinError> {
            let plan = restore_plan(rec, known)?;
            let path = self.scope_dir(&rec.scope)?.join(id);
            // A stale mount from before an engine restart cannot exist, but
            // a re-run of restore must be idempotent. A poisoned path from an
            // earlier failed restore is replaced.
            let _ = unmount(&path, UnmountFlags::DETACH);
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = std::fs::remove_file(&path);
            }
            self.attach_plan(&plan, &path)
        }

        /// Attaches what [`restore_plan`] decided, and only that.
        fn attach_plan(&self, plan: &Restoring, path: &Path) -> Result<(), PinError> {
            self.attach(
                &plan.root,
                &plan.fd,
                path,
                plan.is_dir,
                plan.read_only,
                &plan.subs,
            )
            .map(|_| ())
        }

        /// Leave a path that makes `docker start` fail: a dangling symlink.
        /// An absent path would be created as an empty directory by dockerd,
        /// and the container would start on empty data.
        pub fn poison(&self, scope: &str, id: &str) -> Result<(), PinError> {
            let path = self.scope_dir(scope)?.join(id);
            let _ = unmount(&path, UnmountFlags::DETACH);
            remove_any(&path);
            std::os::unix::fs::symlink("/marsh-pin-not-restored", &path)
                .map_err(|e| PinError::Internal(format!("poison {}: {e}", path.display())))
        }

        pub fn unpin(&self, scope: &str, id: &str, rec: Option<&PinRec>) {
            if break_mode("bind-pin-leak") {
                return;
            }
            let path = self.bind_root.join(scope).join(id);
            for n in rec.map_or(&[][..], |r| &r.subs[..]).iter().rev() {
                let _ = unmount(path.join(n), UnmountFlags::DETACH);
            }
            let _ = unmount(&path, UnmountFlags::DETACH);
            // A lazily detached mount can stay busy for a moment while a
            // dying container's namespace lets go of it.
            for _ in 0..10 {
                remove_any(&path);
                if std::fs::symlink_metadata(&path).is_err() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }

    fn io_err<T>(r: std::io::Result<T>, what: &str) -> Result<T, PinError> {
        r.map_err(|e| PinError::Internal(format!("{what}: {e}")))
    }

    /// Docker creates a missing bind source as an empty directory when a
    /// container whose pin is absent starts; that leftover is a valid mount
    /// point.
    fn make_mountpoint(path: &Path, is_dir: bool) -> Result<(), PinError> {
        if is_dir {
            match std::fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
                Err(e) => Err(PinError::Internal(format!("create pin directory: {e}"))),
            }
        } else {
            io_err(
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .mode(0o600)
                    .open(path)
                    .map(|_| ()),
                "create pin file",
            )
        }
    }

    fn remove_mountpoint(path: &Path, is_dir: bool) {
        let _ = if is_dir {
            std::fs::remove_dir(path)
        } else {
            std::fs::remove_file(path)
        };
    }

    /// Remove whatever stands at a pin path: a directory mountpoint, a file
    /// mountpoint, or a poison symlink. The kind is looked at, not assumed.
    fn remove_any(path: &Path) {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => {
                let _ = std::fs::remove_dir(path);
            }
            // Defect: the mountpoint of a file pin is not removed.
            Ok(_) if break_mode("pin-file-leak") => {}
            Ok(_) => {
                let _ = std::fs::remove_file(path);
            }
            Err(_) => {}
        }
    }

    fn clone_onto(fd: &OwnedFd, target: &Path) -> Result<(), PinError> {
        let tree = open_tree(
            fd.as_fd(),
            "",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::OPEN_TREE_CLOEXEC
                | OpenTreeFlags::AT_EMPTY_PATH,
        )
        .map_err(|e| PinError::Internal(format!("open_tree(OPEN_TREE_CLONE): {e}")))?;
        move_mount(
            tree.as_fd(),
            "",
            CWD,
            target,
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
        .map_err(|e| PinError::Internal(format!("move_mount to {}: {e}", target.display())))?;
        // rprivate: nothing mounted later in the pin propagates elsewhere.
        mount_change(target, MountPropagationFlags::PRIVATE)
            .map_err(|e| PinError::Internal(format!("make pin private: {e}")))
    }

    fn remount_ro(target: &Path) -> Result<(), PinError> {
        mount_remount(
            target,
            MountFlags::BIND | MountFlags::RDONLY | MountFlags::NOSUID | MountFlags::NODEV,
            "",
        )
        .map_err(|e| PinError::Internal(format!("remount read-only: {e}")))
    }

    pub fn restore_all_in(paths: &Paths, ledger: &Ledger) -> std::io::Result<RestoreReport> {
        let mgr = PinManager::new(&paths.bind_root);
        let mut rep = RestoreReport::default();
        let ledger_err = |e: crate::ledger::LedgerError| std::io::Error::other(e.to_string());
        for (id, rec) in ledger.pins() {
            match mgr.restore(&id, &rec, &ledger.protected(&rec.project_root)) {
                Ok(()) => {
                    ledger.set_pin_failed(&id, false).map_err(ledger_err)?;
                    rep.restored.push(id);
                }
                Err(e) => {
                    if !break_mode("restore-fail-open") {
                        mgr.poison(&rec.scope, &id)
                            .map_err(|p| std::io::Error::other(p.to_string()))?;
                        ledger.set_pin_failed(&id, true).map_err(ledger_err)?;
                    }
                    rep.failed.push((id, e.to_string()));
                }
            }
        }
        for (id, _) in &rep.failed {
            for c in ledger.containers_holding(id) {
                if !rep.blocked_containers.contains(&c) {
                    rep.blocked_containers.push(c);
                }
            }
        }
        Ok(rep)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{PinError, RestoreReport};
    use crate::ledger::{Ledger, Paths, PinRec, ProtectedId};
    use std::path::{Path, PathBuf};

    pub struct ProjectRoot {
        pub given: PathBuf,
    }

    impl ProjectRoot {
        pub fn open(_: &Path) -> Result<Self, PinError> {
            Err(PinError::Internal("bind pinning needs Linux".into()))
        }
    }

    pub struct PinManager {
        pub bind_root: PathBuf,
    }

    pub struct Pinned {
        pub id: String,
        pub path: PathBuf,
        pub rec: PinRec,
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn observe(_: &ProjectRoot) -> Result<Vec<ProtectedId>, PinError> {
        Ok(Vec::new())
    }

    impl PinManager {
        #[must_use]
        pub fn new(bind_root: &Path) -> Self {
            Self {
                bind_root: bind_root.to_path_buf(),
            }
        }

        pub fn pin(
            &self,
            _: &ProjectRoot,
            _: &str,
            _: &str,
            _: bool,
            _: bool,
            _: &[ProtectedId],
        ) -> Result<Pinned, PinError> {
            Err(PinError::Internal("bind pinning needs Linux".into()))
        }

        pub fn restore(&self, _: &str, _: &PinRec, _: &[ProtectedId]) -> Result<(), PinError> {
            Err(PinError::Internal("bind pinning needs Linux".into()))
        }

        pub fn poison(&self, _: &str, _: &str) -> Result<(), PinError> {
            Ok(())
        }

        pub fn unpin(&self, _: &str, _: &str, _: Option<&PinRec>) {}
    }

    #[allow(clippy::unnecessary_wraps)]
    pub fn restore_all_in(_: &Paths, _: &Ledger) -> std::io::Result<RestoreReport> {
        Ok(RestoreReport::default())
    }
}

pub use imp::Pinned;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_paths_must_be_clean_and_inside() {
        let root = Path::new("/Users/x/proj");
        let ok = |s: &str| relative_components(root, s).map_err(|e| e.to_string());
        assert_eq!(ok("/Users/x/proj").unwrap(), Vec::<String>::new());
        assert_eq!(ok("/Users/x/proj/a/b").unwrap(), ["a", "b"]);
        for bad in [
            "/Users/x/proj/../..",
            "/Users/x/proj/a/../b",
            "/Users/x/proj-evil",
            "/Users/x/proj-evil/a",
            "/Users/x/proj/",
            "/Users/x/proj//a",
            "/Users/x/proj/./a",
            "/",
            "/Users/x",
        ] {
            assert!(ok(bad).is_err(), "{bad}");
        }
    }

    /// A lane root is inside a project: a link planted under `.marsh` must
    /// not move the root (and so every pin beneath it) elsewhere.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_lane_root_never_follows_a_symlink() {
        let project = std::env::temp_dir().join(format!("pin-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(&project).unwrap();
        let project = std::fs::canonicalize(&project).unwrap();
        let lane = project.join(".marsh/split/s1/a");
        std::fs::create_dir_all(&lane).unwrap();
        let root = ProjectRoot::open(&lane).unwrap();
        assert_eq!(root.canonical(), lane.to_str().unwrap());
        std::fs::remove_dir(&lane).unwrap();
        std::os::unix::fs::symlink("/", &lane).unwrap();
        assert!(ProjectRoot::open(&lane).is_err());
        std::fs::remove_file(&lane).unwrap();
        std::fs::rename(project.join(".marsh/split/s1"), project.join("s1")).unwrap();
        std::os::unix::fs::symlink("/", project.join(".marsh/split/s1")).unwrap();
        assert!(ProjectRoot::open(&lane).is_err());
        // Above `.marsh` the project's own path is the user's.
        let linked = project.join("linked");
        std::os::unix::fs::symlink(&project, &linked).unwrap();
        assert!(ProjectRoot::open(&linked).is_ok());
        std::fs::remove_dir_all(&project).unwrap();
    }

    /// DA2-01 through `PinManager::restore` itself: the re-made pin of a
    /// name `.git` was renamed into refuses writes. It mounts, so it needs
    /// root: `cargo test -p marsh-services --lib -- --ignored` as root
    /// (run so in `audit-linux` for B5); CI's unprivileged run skips it.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "mounts: needs root"]
    fn restore_attaches_a_pin_renamed_into_git_read_only() {
        let base = std::env::temp_dir().join(format!("pin-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join("project");
        std::fs::create_dir_all(project.join(".git/hooks")).unwrap();
        std::fs::write(project.join(".git/f"), "").unwrap();
        std::fs::create_dir_all(project.join("foo")).unwrap();
        std::fs::write(project.join("foo/f"), "").unwrap();
        let project = std::fs::canonicalize(&project).unwrap();
        let known = observe(&ProjectRoot::open(&project).unwrap()).unwrap();
        let rec = |rel: &str, is_dir: bool| crate::ledger::PinRec {
            scope: "s".into(),
            project_root: project.to_string_lossy().into_owned(),
            rel: rel.into(),
            read_only: false,
            is_dir,
            subs: Vec::new(),
            dev: 0,
            ino: 0,
            created: 0,
            failed: false,
        };
        let pins = PinManager::new(&base.join("binds"));
        let (dir, file) = (base.join("binds/s/d"), base.join("binds/s/f"));
        // Restores both pins; whether each takes a write, then unmounts.
        let restore_writable = || {
            pins.restore("d", &rec("foo", true), &known).unwrap();
            pins.restore("f", &rec("foo/f", false), &known).unwrap();
            let writable = (
                std::fs::write(dir.join("probe"), "x").is_ok(),
                std::fs::OpenOptions::new().write(true).open(&file).is_ok(),
            );
            for path in [&dir, &file] {
                let _ = rustix::mount::unmount(path, rustix::mount::UnmountFlags::DETACH);
            }
            writable
        };
        assert_eq!(restore_writable(), (true, true), "plain pins stay writable");
        std::fs::rename(project.join("foo"), project.join("foo.x")).unwrap();
        std::fs::rename(project.join(".git"), project.join("foo")).unwrap();
        assert_eq!(
            restore_writable(),
            (false, false),
            "a restored pin of .git is writable"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// DA2-01: a read-write pin re-made after `.git` was renamed into its
    /// name while the engine was down is read-only, directory or file; the
    /// plan is exactly what `restore` attaches.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_restored_pin_renamed_into_git_is_read_only() {
        let project = std::env::temp_dir().join(format!("pin-restore-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&project);
        std::fs::create_dir_all(project.join(".git/hooks")).unwrap();
        std::fs::write(project.join(".git/f"), "").unwrap();
        std::fs::create_dir_all(project.join("foo")).unwrap();
        std::fs::write(project.join("foo/f"), "").unwrap();
        let project = std::fs::canonicalize(&project).unwrap();
        let known = observe(&ProjectRoot::open(&project).unwrap()).unwrap();
        let rec = |rel: &str, is_dir: bool| crate::ledger::PinRec {
            scope: "s".into(),
            project_root: project.to_string_lossy().into_owned(),
            rel: rel.into(),
            read_only: false,
            is_dir,
            subs: Vec::new(),
            dev: 0,
            ino: 0,
            created: 0,
            failed: false,
        };
        let read_only = |rel: &str, is_dir: bool| {
            imp::restore_plan(&rec(rel, is_dir), &known)
                .unwrap()
                .read_only
        };
        assert!(!read_only("foo", true));
        assert!(!read_only("foo/f", false));
        std::fs::rename(project.join("foo"), project.join("foo.x")).unwrap();
        std::fs::rename(project.join(".git"), project.join("foo")).unwrap();
        assert!(read_only("foo", true));
        assert!(read_only("foo/f", false));
        std::fs::remove_dir_all(&project).unwrap();
    }
}
