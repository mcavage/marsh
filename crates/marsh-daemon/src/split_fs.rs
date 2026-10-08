//! Host-side workspace files for daemon-owned splits (`docs/design/workspaces.md`
//! sections 5 and 8): snapshot, forks with daemon-written Git metadata,
//! capture verification, the patch writer, and removal.
//!
//! Everything under `<root>/.marsh` is guest-writable, so the daemon never
//! resolves a path there: every directory is opened component by component
//! with `O_NOFOLLOW` relative to an already-open directory, files are
//! created with `O_CREAT | O_EXCL | O_NOFOLLOW`, read only when `fstat` says
//! they are regular (opened `O_NONBLOCK`, so a FIFO cannot block), and cloned
//! with `clonefileat(..., CLONE_NOFOLLOW)`. The daemon never executes `git`
//! and never reads repository configuration; ignore rules are read as data.

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const INDEX_LIMIT: u64 = 64 << 20;
const SMALL_LIMIT: u64 = 1 << 20;
/// Capture bounds (M1): one file, all changed bytes, one text diff.
pub const FILE_LIMIT: u64 = 32 << 20;
pub const CAPTURE_LIMIT: u64 = 256 << 20;
const DIFF_DEADLINE: Duration = Duration::from_secs(5);
/// The branch ref a fork's `HEAD` names, so commits never write `HEAD`.
pub const BRANCH_REF: &str = "refs/heads/marsh-split";

fn rustix_err(error: Errno) -> io::Error {
    io::Error::from(error)
}

/// A directory opened by descriptor; every operation is relative to it.
#[derive(Debug)]
pub struct Dir(OwnedFd);

/// `(kind, size, mtime ns, ctime ns, mode, dev, ino)` without following links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: FileType,
    pub size: u64,
    pub mtime: i128,
    pub ctime: i128,
    pub mode: u32,
    pub identity: (u64, u64),
}

#[allow(
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::useless_conversion,
    clippy::unnecessary_cast
)]
fn stat_of(stat: &rustix::fs::Stat) -> Stat {
    Stat {
        kind: FileType::from_raw_mode(stat.st_mode as _),
        size: stat.st_size as u64,
        mtime: i128::from(stat.st_mtime as i64) * 1_000_000_000
            + i128::from(stat.st_mtime_nsec as i64),
        ctime: i128::from(stat.st_ctime as i64) * 1_000_000_000
            + i128::from(stat.st_ctime_nsec as i64),
        mode: u32::from(stat.st_mode as u32) & 0o7777,
        identity: (stat.st_dev as u64, stat.st_ino as u64),
    }
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

impl Dir {
    /// Open an absolute, already-admitted root (the session's project).
    pub fn open_root(path: &Path) -> io::Result<Self> {
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(Self)
        .map_err(rustix_err)
    }

    pub fn open(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        rustix::fs::openat(&self.0, name.as_ref(), DIR_FLAGS, Mode::empty())
            .map(Self)
            .map_err(rustix_err)
    }

    /// Open a relative path of plain components, one `O_NOFOLLOW` step each.
    pub fn open_path(&self, relative: &Path) -> io::Result<Self> {
        let mut current = self.dup()?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "not a plain path",
                ));
            };
            current = current.open(name)?;
        }
        Ok(current)
    }

    pub fn dup(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    /// Create (or reuse) a real subdirectory; a symlink is refused.
    pub fn ensure(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        match rustix::fs::mkdirat(&self.0, name.as_ref(), Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(Errno::EXIST) => self.open(name),
            Err(error) => Err(rustix_err(error)),
        }
    }

    /// Create a new subdirectory that must not exist yet.
    pub fn create(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        rustix::fs::mkdirat(&self.0, name.as_ref(), Mode::from_raw_mode(0o755))
            .map_err(rustix_err)?;
        self.open(name)
    }

    pub fn fstat(&self) -> io::Result<Stat> {
        rustix::fs::fstat(&self.0)
            .map(|s| stat_of(&s))
            .map_err(rustix_err)
    }

    pub fn stat(&self, name: impl AsRef<OsStr>) -> io::Result<Stat> {
        rustix::fs::statat(&self.0, name.as_ref(), AtFlags::SYMLINK_NOFOLLOW)
            .map(|s| stat_of(&s))
            .map_err(rustix_err)
    }

    /// Create a new file (never through a symlink, never over an entry).
    pub fn write_new(&self, name: impl AsRef<OsStr>, bytes: &[u8]) -> io::Result<()> {
        self.write_new_mode(name, bytes, 0o644)
    }

    pub fn write_new_mode(
        &self,
        name: impl AsRef<OsStr>,
        bytes: &[u8],
        mode: u32,
    ) -> io::Result<()> {
        let fd = rustix::fs::openat(
            &self.0,
            name.as_ref(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(mode.try_into().unwrap_or(0o644)),
        )
        .map_err(rustix_err)?;
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes)
    }

    /// Read a regular file of at most `limit` bytes (a FIFO or device is
    /// refused without blocking).
    pub fn read(&self, name: impl AsRef<OsStr>, limit: u64) -> io::Result<Vec<u8>> {
        let fd = rustix::fs::openat(
            &self.0,
            name.as_ref(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(rustix_err)?;
        let stat = stat_of(&rustix::fs::fstat(&fd).map_err(rustix_err)?);
        if stat.kind != FileType::RegularFile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not a regular file",
            ));
        }
        if stat.size > limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "file exceeds the capture limit",
            ));
        }
        let mut bytes = Vec::new();
        std::fs::File::from(fd)
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "file exceeds the capture limit",
            ));
        }
        Ok(bytes)
    }

    pub fn entries(&self) -> io::Result<Vec<(OsString, FileType)>> {
        let mut entries = Vec::new();
        for entry in rustix::fs::Dir::read_from(&self.0).map_err(rustix_err)? {
            let entry = entry.map_err(rustix_err)?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let name = OsString::from_vec(name.to_vec());
            // d_type may be unknown on some filesystems; ask without following.
            let kind = match entry.file_type() {
                FileType::Unknown => self.stat(&name)?.kind,
                kind => kind,
            };
            entries.push((name, kind));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(entries)
    }

    pub fn read_link(&self, name: impl AsRef<OsStr>) -> io::Result<OsString> {
        rustix::fs::readlinkat(&self.0, name.as_ref(), Vec::new())
            .map(|target| OsString::from_vec(target.into_bytes()))
            .map_err(rustix_err)
    }

    pub fn symlink(&self, target: &OsStr, name: impl AsRef<OsStr>) -> io::Result<()> {
        rustix::fs::symlinkat(target, &self.0, name.as_ref()).map_err(rustix_err)
    }

    pub fn unlink(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        rustix::fs::unlinkat(&self.0, name.as_ref(), AtFlags::empty()).map_err(rustix_err)
    }

    pub fn rename(
        &self,
        name: impl AsRef<OsStr>,
        to: &Self,
        to_name: impl AsRef<OsStr>,
    ) -> io::Result<()> {
        rustix::fs::renameat(&self.0, name.as_ref(), &to.0, to_name.as_ref()).map_err(rustix_err)
    }

    /// Clone `name` (file, symlink, or directory tree) to `to/to_name`
    /// without following a symlink; a copy where `clonefileat` is missing.
    pub fn clone_to(&self, name: &OsStr, to: &Self, to_name: &OsStr) -> io::Result<()> {
        match marsh_host_identity::clone_at(&self.0, name, &to.0, to_name) {
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                self.copy_to(name, to, to_name)
            }
            result => result,
        }
    }

    fn copy_to(&self, name: &OsStr, to: &Self, to_name: &OsStr) -> io::Result<()> {
        let stat = self.stat(name)?;
        match stat.kind {
            FileType::Symlink => to.symlink(&self.read_link(name)?, to_name),
            FileType::Directory => {
                let (source, target) = (self.open(name)?, to.create(to_name)?);
                for (entry, _) in source.entries()? {
                    source.copy_to(&entry, &target, &entry)?;
                }
                Ok(())
            }
            FileType::RegularFile => {
                to.write_new_mode(to_name, &self.read(name, u64::MAX)?, stat.mode)
            }
            _ => Ok(()),
        }
    }

    /// Remove `name` (any kind) by descriptors; directories recursively.
    pub fn remove_tree(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = name.as_ref();
        if self.stat(name)?.kind != FileType::Directory {
            return self.unlink(name);
        }
        let child = self.open(name)?;
        let _ = rustix::fs::fchmod(&child.0, Mode::from_raw_mode(0o700));
        for (entry, _) in child.entries()? {
            child.remove_tree(&entry)?;
        }
        rustix::fs::unlinkat(&self.0, name, AtFlags::REMOVEDIR).map_err(rustix_err)
    }
}

/// `<root>/.marsh/split`, opened without following any component; a planted
/// symlink anywhere on the way is refused. Creating it inside a Git worktree
/// also writes `.marsh/.gitignore` (`*`) so Git never sees split state; a
/// plain directory gets no Git file.
///
/// # Errors
/// Returns the open failure (`ELOOP`/`ENOTDIR` for a symlink).
pub fn splits_dir(root: &Path, create: bool) -> io::Result<Dir> {
    let in_git = create && root.ancestors().any(|dir| dir.join(".git").exists());
    let root = Dir::open_root(root)?;
    let marsh = if create {
        let marsh = root.ensure(".marsh")?;
        if in_git {
            match marsh.write_new(".gitignore", b"*\n") {
                Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
                _ => {}
            }
        }
        marsh
    } else {
        root.open(".marsh")?
    };
    if create {
        marsh.ensure("split")
    } else {
        marsh.open("split")
    }
}

/// The split directory `<id>`, only if it is still the directory recorded at
/// create (`workspace replaced` otherwise).
///
/// # Errors
/// Returns `workspace replaced` or the open failure.
pub fn open_split(root: &Path, id: &str, identity: (u64, u64)) -> Result<Dir, String> {
    let dir = splits_dir(root, false)
        .and_then(|splits| splits.open(id))
        .map_err(|_| "workspace replaced".to_owned())?;
    if dir.fstat().map_err(|e| e.to_string())?.identity != identity {
        return Err("workspace replaced".into());
    }
    Ok(dir)
}

/// The Git side of a snapshot as journaled: `HEAD` contents and the object
/// directories the fork reads through `alternates` (written as data).
#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct GitSource {
    pub head: String,
    pub objects: Vec<PathBuf>,
}

/// What a fork's admin directory is built from (read at create, by fd).
#[derive(Clone, Debug, Default)]
pub struct GitSnapshot {
    pub source: GitSource,
    pub index: Option<Vec<u8>>,
    pub packed_refs: Vec<u8>,
    pub exclude: Vec<u8>,
}

fn valid_ref(name: &str) -> bool {
    name.starts_with("refs/")
        && Path::new(name)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn resolve(git_dir: &Dir, reference: &str, packed: &[u8]) -> Option<String> {
    let path = Path::new(reference);
    let parent = git_dir.open_path(path.parent()?).ok()?;
    if let Ok(bytes) = parent.read(path.file_name()?, 4096) {
        let text = String::from_utf8_lossy(&bytes).trim().to_owned();
        return Some(text).filter(|id| id.len() >= 40 && id.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    String::from_utf8_lossy(packed).lines().find_map(|line| {
        let (oid, name) = line.split_once(' ')?;
        (name == reference).then(|| oid.to_owned())
    })
}

/// Read a Git directory's `HEAD` (resolved), index, packed-refs, and
/// `info/exclude` through descriptors.
fn read_git_dir(git_dir: &Dir) -> Result<GitSnapshot, String> {
    let packed_refs = git_dir.read("packed-refs", INDEX_LIMIT).unwrap_or_default();
    let head = git_dir
        .read("HEAD", 4096)
        .map_err(|e| format!("HEAD: {e}"))?;
    let head = String::from_utf8_lossy(&head).trim().to_owned();
    let head = match head.strip_prefix("ref: ") {
        Some(name) if valid_ref(name) => {
            resolve(git_dir, name, &packed_refs).unwrap_or_else(|| format!("ref: {name}"))
        }
        Some(_) => return Err("unsupported HEAD".into()),
        None => head,
    };
    let index = match git_dir.read("index", INDEX_LIMIT) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("index: {error}")),
    };
    let exclude = git_dir
        .open("info")
        .and_then(|info| info.read("exclude", SMALL_LIMIT))
        .unwrap_or_default();
    Ok(GitSnapshot {
        source: GitSource {
            head,
            objects: Vec::new(),
        },
        index,
        packed_refs,
        exclude,
    })
}

/// The user's repository at `root`, or `None` for a plain directory.
///
/// # Errors
/// Refuses a `.git` that is a symlink or a gitfile (a linked worktree or a
/// submodule checkout: split from the main checkout instead).
pub fn git_source(root: &Path) -> Result<Option<GitSnapshot>, String> {
    let project = Dir::open_root(root).map_err(|e| e.to_string())?;
    let kind = match project.stat(".git") {
        Ok(stat) => stat.kind,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!(".git: {error}")),
    };
    match kind {
        FileType::Directory => {}
        FileType::RegularFile => {
            return Err(format!(
                "{}/.git is a gitfile (a linked worktree or submodule); split from the main checkout",
                root.display()
            ));
        }
        _ => return Err(format!("{}/.git is not a directory", root.display())),
    }
    let git_dir = project.open(".git").map_err(|e| format!(".git: {e}"))?;
    git_dir
        .open("objects")
        .map_err(|e| format!(".git/objects: {e}"))?;
    let mut snapshot = read_git_dir(&git_dir)?;
    snapshot.source.objects.push(root.join(".git/objects"));
    Ok(Some(snapshot))
}

/// A nested split's Git source: the creator branch's admin directory.
///
/// # Errors
/// Returns why the creator's admin files cannot be read.
pub fn fork_git_source(
    split: &Dir,
    split_path: &Path,
    label: &str,
    parent: &GitSource,
) -> Result<GitSnapshot, String> {
    let admin = split
        .open(".admin")
        .and_then(|admin| admin.open(label))
        .map_err(|e| format!("creator admin: {e}"))?;
    let mut snapshot = read_git_dir(&admin)?;
    if snapshot.source.head.starts_with("ref: ") {
        snapshot.source.head.clone_from(&parent.head);
    }
    snapshot.source.objects.clone_from(&parent.objects);
    snapshot
        .source
        .objects
        .push(split_path.join("store.git/objects"));
    snapshot
        .source
        .objects
        .push(split_path.join(".admin").join(label).join("objects"));
    Ok(snapshot)
}

/// Gitignore rules for one directory level, read as data.
struct Rules(Vec<ignore::gitignore::Gitignore>);

impl Rules {
    fn parse(root: &Path, bytes: &[u8]) -> Option<ignore::gitignore::Gitignore> {
        let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
        for line in String::from_utf8_lossy(bytes).lines() {
            let _ = builder.add_line(None, line);
        }
        builder.build().ok().filter(|rules| !rules.is_empty())
    }

    /// Deepest rules first; the first decisive match wins.
    fn ignored(&self, path: &Path, dir: bool) -> bool {
        for rules in self.0.iter().rev() {
            match rules.matched(path, dir) {
                ignore::Match::Ignore(_) => return true,
                ignore::Match::Whitelist(_) => return false,
                ignore::Match::None => {}
            }
        }
        false
    }
}

/// Logical paths for matching only (never used for I/O).
fn logical(relative: &Path) -> PathBuf {
    Path::new("/").join(relative)
}

fn is_dotgit(name: &OsStr) -> bool {
    name.as_bytes().eq_ignore_ascii_case(b".git")
}

/// Copy the caller's tree (`source`) into the new, empty `base`.
///
/// Top-level entries that the root rules do not ignore are cloned whole;
/// then `base` is walked by descriptor: ignored entries, nested `.git`
/// entries (a submodule or vendored repository), and special files are
/// pruned, and each file whose size, mtime, or mode differs from the source
/// is re-cloned with a stat check (M2).
///
/// # Errors
/// Returns the first I/O failure or `file kept changing`.
pub fn snapshot(source: &Dir, base: &Dir, git: Option<&GitSnapshot>) -> Result<u64, String> {
    let io = |e: io::Error| e.to_string();
    let mut rules = Rules(Vec::new());
    if git.is_some() {
        if let Some(global) =
            Some(ignore::gitignore::Gitignore::global().0).filter(|g| !g.is_empty())
        {
            rules.0.push(global);
        }
        if let Some(exclude) = git.and_then(|git| Rules::parse(Path::new("/"), &git.exclude)) {
            rules.0.push(exclude);
        }
    }
    let mut top = Rules(rules.0.clone());
    if git.is_some()
        && let Some(root) = source
            .read(".gitignore", SMALL_LIMIT)
            .ok()
            .and_then(|bytes| Rules::parse(Path::new("/"), &bytes))
    {
        top.0.push(root);
    }
    for (name, kind) in source.entries().map_err(io)? {
        if name == ".marsh" || is_dotgit(&name) {
            continue;
        }
        if git.is_some() && top.ignored(&logical(Path::new(&name)), kind == FileType::Directory) {
            continue;
        }
        match kind {
            FileType::Directory | FileType::RegularFile | FileType::Symlink => {
                source
                    .clone_to(&name, base, &name)
                    .map_err(|e| format!("{}: {e}", Path::new(&name).display()))?;
            }
            _ => {}
        }
    }
    let mut files = 0;
    prune(
        source,
        base,
        Path::new(""),
        &mut rules,
        git.is_some(),
        &mut files,
    )?;
    Ok(files)
}

fn prune(
    source: &Dir,
    base: &Dir,
    relative: &Path,
    rules: &mut Rules,
    git: bool,
    files: &mut u64,
) -> Result<(), String> {
    let io = |e: io::Error| e.to_string();
    let pushed = git
        && base
            .read(".gitignore", SMALL_LIMIT)
            .ok()
            .and_then(|bytes| Rules::parse(&logical(relative), &bytes))
            .map(|level| rules.0.push(level))
            .is_some();
    for (name, kind) in base.entries().map_err(io)? {
        let path = relative.join(&name);
        let dir = kind == FileType::Directory;
        let special = !matches!(
            kind,
            FileType::Directory | FileType::RegularFile | FileType::Symlink
        );
        if is_dotgit(&name) || special || (git && rules.ignored(&logical(&path), dir)) {
            base.remove_tree(&name).map_err(io)?;
            continue;
        }
        match kind {
            FileType::Directory => {
                let child = base.open(&name).map_err(io)?;
                match source.open(&name) {
                    Ok(origin) => prune(&origin, &child, &path, rules, git, files)?,
                    // Gone from the caller's tree meanwhile: keep what we cloned.
                    Err(_) => prune(&child, &child, &path, rules, git, files)?,
                }
            }
            FileType::RegularFile => {
                *files += 1;
                let copy = base.stat(&name).map_err(io)?;
                let Ok(now) = source.stat(&name) else {
                    continue;
                };
                if (copy.size, copy.mtime, copy.mode) != (now.size, now.mtime, now.mode) {
                    base.unlink(&name).map_err(io)?;
                    clone_stable(source, base, &name, &path)?;
                }
            }
            _ => {}
        }
    }
    if pushed {
        rules.0.pop();
    }
    Ok(())
}

/// Clone one file, re-cloning up to three times if it changed meanwhile.
fn clone_stable(source: &Dir, base: &Dir, name: &OsStr, relative: &Path) -> Result<(), String> {
    for _ in 0..3 {
        let before = source.stat(name).map_err(|e| e.to_string())?;
        if before.kind != FileType::RegularFile {
            return Ok(());
        }
        source
            .clone_to(name, base, name)
            .map_err(|e| format!("{}: {e}", relative.display()))?;
        let after = source.stat(name).map_err(|e| e.to_string())?;
        if (before.size, before.mtime, before.mode, before.identity)
            == (after.size, after.mtime, after.mode, after.identity)
        {
            return Ok(());
        }
        let _ = base.unlink(name);
    }
    Err(format!("file kept changing: {}", relative.display()))
}

/// The admin files the daemon writes for one fork (relative name -> bytes).
#[must_use]
pub fn admin_contents(git: &GitSnapshot, store: &Path) -> BTreeMap<&'static str, Vec<u8>> {
    let mut alternates = format!("{}\n", store.join("objects").display());
    for objects in &git.source.objects {
        let _ = writeln!(alternates, "{}", objects.display());
    }
    BTreeMap::from([
        ("HEAD", format!("ref: {BRANCH_REF}\n").into_bytes()),
        (
            "config",
            b"[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n".to_vec(),
        ),
        ("info/exclude", git.exclude.clone()),
        ("packed-refs", git.packed_refs.clone()),
        ("objects/info/alternates", alternates.into_bytes()),
    ])
}

/// SHA-256 digests of the admin files, journaled for capture verification.
#[must_use]
pub fn admin_digest(contents: &BTreeMap<&'static str, Vec<u8>>) -> BTreeMap<String, String> {
    use sha2::Digest as _;
    contents
        .iter()
        .map(|(name, bytes)| ((*name).to_owned(), hex(&sha2::Sha256::digest(bytes))))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Write `.admin/<label>` and the fork's gitfile. The user's `.git` is
/// never written.
///
/// # Errors
/// Returns the first I/O failure.
pub fn write_admin(
    git: &GitSnapshot,
    contents: &BTreeMap<&'static str, Vec<u8>>,
    admin: &Dir,
    admin_path: &Path,
    fork: &Dir,
) -> io::Result<()> {
    let info = admin.create("info")?;
    let objects = admin.create("objects")?;
    objects.create("info")?;
    let refs = admin.create("refs")?;
    let heads = refs.create("heads")?;
    refs.create("tags")?;
    for (name, bytes) in contents {
        match *name {
            "info/exclude" => info.write_new("exclude", bytes)?,
            "objects/info/alternates" => objects.open("info")?.write_new("alternates", bytes)?,
            name => admin.write_new(name, bytes)?,
        }
    }
    if git.source.head.len() >= 40 && !git.source.head.starts_with("ref: ") {
        heads.write_new("marsh-split", format!("{}\n", git.source.head).as_bytes())?;
    }
    admin.write_new("index", git.index.as_deref().unwrap_or(&empty_index()))?;
    fork.write_new(".git", &gitfile(admin_path))
}

fn empty_index() -> Vec<u8> {
    let mut bytes = b"DIRC\0\0\0\x02\0\0\0\0".to_vec();
    let digest = Sha1::digest(&bytes);
    bytes.extend_from_slice(&digest);
    bytes
}

#[must_use]
pub fn gitfile(admin: &Path) -> Vec<u8> {
    format!("gitdir: {}\n", admin.display()).into_bytes()
}

/// What may appear in a fork's admin directory after a branch ran: Git's
/// own state files, never `commondir`, `gitdir`, `config.worktree`, or hooks.
const ADMIN_ALLOWED: [&str; 12] = [
    "HEAD",
    "config",
    "info",
    "packed-refs",
    "objects",
    "index",
    "index.lock",
    "refs",
    "logs",
    "ORIG_HEAD",
    "COMMIT_EDITMSG",
    "description",
];

/// Capture verification: the gitfile and admin files are byte-identical to
/// what the daemon wrote, the admin directory holds only Git state files,
/// and the fork contains no `.git` other than its gitfile.
///
/// # Errors
/// Returns the reason the fork is rejected.
pub fn verify(
    expected: Option<&BTreeMap<String, String>>,
    admin: Option<&Dir>,
    admin_path: &Path,
    fork: &Dir,
) -> Result<(), String> {
    use sha2::Digest as _;
    if let (Some(expected), Some(admin)) = (expected, admin) {
        if fork.read(".git", 4096).ok() != Some(gitfile(admin_path)) {
            return Err("fork .git was changed".into());
        }
        for (name, _) in admin.entries().map_err(|e| e.to_string())? {
            if !ADMIN_ALLOWED.iter().any(|allowed| name == *allowed) {
                return Err(format!(
                    "admin {} is not allowed",
                    Path::new(&name).display()
                ));
            }
        }
        for (name, digest) in expected {
            let path = Path::new(name);
            let bytes = path
                .parent()
                .map_or_else(|| admin.dup(), |parent| admin.open_path(parent))
                .and_then(|dir| dir.read(path.file_name().unwrap_or_default(), INDEX_LIMIT))
                .map_err(|_| format!("admin {name} was changed"))?;
            if hex(&sha2::Sha256::digest(&bytes)) != *digest {
                return Err(format!("admin {name} was changed"));
            }
        }
    } else if fork.stat(".git").is_ok() {
        return Err("nested .git at .git".into());
    }
    nested_git(fork, Path::new(""), expected.is_some())
}

fn nested_git(dir: &Dir, relative: &Path, top_gitfile: bool) -> Result<(), String> {
    for (name, kind) in dir.entries().map_err(|e| e.to_string())? {
        let path = relative.join(&name);
        if is_dotgit(&name) && !(top_gitfile && relative.as_os_str().is_empty() && name == ".git") {
            return Err(format!("nested .git at {}", path.display()));
        }
        if kind == FileType::Directory {
            nested_git(
                &dir.open(&name).map_err(|e| e.to_string())?,
                &path,
                top_gitfile,
            )?;
        }
    }
    Ok(())
}

/// One path's state in a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry {
    File {
        executable: bool,
        size: u64,
        mtime: i128,
        ctime: i128,
    },
    Link(Vec<u8>),
}

impl Entry {
    /// Same kind, mode, size, mtime, or link target (ctime is checked apart).
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::File {
                    executable,
                    size,
                    mtime,
                    ..
                },
                Self::File {
                    executable: other_executable,
                    size: other_size,
                    mtime: other_mtime,
                    ..
                },
            ) => (executable, size, mtime) == (other_executable, other_size, other_mtime),
            (Self::Link(a), Self::Link(b)) => a == b,
            _ => false,
        }
    }
}

fn tree(root: &Dir, skip_gitfile: bool) -> Result<BTreeMap<Vec<u8>, Entry>, String> {
    let mut entries = BTreeMap::new();
    let mut stack = vec![(root.dup().map_err(|e| e.to_string())?, PathBuf::new())];
    while let Some((dir, relative)) = stack.pop() {
        for (name, kind) in dir.entries().map_err(|e| e.to_string())? {
            if skip_gitfile && relative.as_os_str().is_empty() && name == ".git" {
                continue;
            }
            let path = relative.join(&name);
            let key = path.as_os_str().as_bytes().to_vec();
            match kind {
                FileType::Directory => {
                    stack.push((dir.open(&name).map_err(|e| e.to_string())?, path));
                }
                FileType::Symlink => {
                    let target = dir.read_link(&name).map_err(|e| e.to_string())?;
                    entries.insert(key, Entry::Link(target.into_vec()));
                }
                FileType::RegularFile => {
                    let stat = dir.stat(&name).map_err(|e| e.to_string())?;
                    entries.insert(
                        key,
                        Entry::File {
                            executable: stat.mode & 0o100 != 0,
                            size: stat.size,
                            mtime: stat.mtime,
                            ctime: stat.ctime,
                        },
                    );
                }
                _ => {}
            }
        }
    }
    Ok(entries)
}

/// What capture leaves out of a fork in a Git project: paths the branch
/// created that the project's ignore rules exclude (`core.excludesFile`,
/// `info/exclude`, and the fork's `.gitignore` files), as `git status`
/// would. A path the snapshot has is always compared, like a tracked file.
struct Untracked<'a> {
    before: &'a BTreeMap<Vec<u8>, Entry>,
    /// Directories the snapshot has (every ancestor of a `before` path).
    dirs: BTreeSet<Vec<u8>>,
}

impl<'a> Untracked<'a> {
    fn new(before: &'a BTreeMap<Vec<u8>, Entry>) -> Self {
        let mut dirs = BTreeSet::new();
        for path in before.keys() {
            let mut path = Path::new(OsStr::from_bytes(path));
            while let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                if !dirs.insert(parent.as_os_str().as_bytes().to_vec()) {
                    break;
                }
                path = parent;
            }
        }
        Self { before, dirs }
    }

    fn snapshotted(&self, key: &[u8]) -> bool {
        self.before.contains_key(key) || self.dirs.contains(key)
    }
}

/// The fork's entries, without the ignored paths the branch created.
fn fork_tree(
    fork: &Dir,
    before: &BTreeMap<Vec<u8>, Entry>,
    exclude: &[u8],
) -> Result<BTreeMap<Vec<u8>, Entry>, String> {
    let mut rules = Rules(Vec::new());
    if let Some(global) = Some(ignore::gitignore::Gitignore::global().0).filter(|g| !g.is_empty()) {
        rules.0.push(global);
    }
    if let Some(exclude) = Rules::parse(Path::new("/"), exclude) {
        rules.0.push(exclude);
    }
    let untracked = Untracked::new(before);
    let mut entries = BTreeMap::new();
    walk_fork(fork, Path::new(""), &mut rules, &untracked, &mut entries)?;
    Ok(entries)
}

fn walk_fork(
    dir: &Dir,
    relative: &Path,
    rules: &mut Rules,
    untracked: &Untracked<'_>,
    entries: &mut BTreeMap<Vec<u8>, Entry>,
) -> Result<(), String> {
    let io = |e: io::Error| e.to_string();
    let pushed = dir
        .read(".gitignore", SMALL_LIMIT)
        .ok()
        .and_then(|bytes| Rules::parse(&logical(relative), &bytes))
        .map(|level| rules.0.push(level))
        .is_some();
    for (name, kind) in dir.entries().map_err(io)? {
        if relative.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let path = relative.join(&name);
        let key = path.as_os_str().as_bytes().to_vec();
        if !untracked.snapshotted(&key)
            && rules.ignored(&logical(&path), kind == FileType::Directory)
        {
            continue;
        }
        match kind {
            FileType::Directory => {
                walk_fork(
                    &dir.open(&name).map_err(io)?,
                    &path,
                    rules,
                    untracked,
                    entries,
                )?;
            }
            FileType::Symlink => {
                let target = dir.read_link(&name).map_err(io)?;
                entries.insert(key, Entry::Link(target.into_vec()));
            }
            FileType::RegularFile => {
                let stat = dir.stat(&name).map_err(io)?;
                entries.insert(
                    key,
                    Entry::File {
                        executable: stat.mode & 0o100 != 0,
                        size: stat.size,
                        mtime: stat.mtime,
                        ctime: stat.ctime,
                    },
                );
            }
            _ => {}
        }
    }
    if pushed {
        rules.0.pop();
    }
    Ok(())
}

/// Read one file of a tree by descriptor walk (no symlink followed).
fn read_at(root: &Dir, path: &[u8], limit: u64) -> io::Result<Vec<u8>> {
    let path = Path::new(OsStr::from_bytes(path));
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => root.open_path(parent)?,
        _ => root.dup()?,
    };
    parent.read(path.file_name().unwrap_or_default(), limit)
}

/// One changed path between the snapshot and a fork.
#[derive(Clone, Debug)]
pub struct Change {
    pub path: Vec<u8>,
    pub kind: char,
}

/// Compare `fork` with `base`, write `diff.patch` and `files` into `out`
/// (absent when nothing changed) and the blobs into `store/objects`.
///
/// A file whose size, mode, and mtime match is still compared byte for byte
/// when its ctime is at or after `forked_ns` (the fork's creation): ctime
/// cannot be set by the branch, so an edit that restores the mtime is seen.
///
/// In a Git project (`exclude` is the fork's `info/exclude`), a path the
/// branch created that the ignore rules exclude is not captured.
///
/// # Errors
/// Returns I/O failures and capture limit breaches.
pub fn capture(
    base: &Dir,
    fork: &Dir,
    exclude: Option<&[u8]>,
    out: &Dir,
    store: &Dir,
    forked_ns: i128,
) -> Result<Vec<Change>, String> {
    let before = tree(base, false)?;
    let after = match exclude {
        Some(exclude) => fork_tree(fork, &before, exclude)?,
        None => tree(fork, false)?,
    };
    let mut changes = Vec::new();
    let mut patch = Vec::new();
    let mut total = 0_u64;
    let paths = before
        .keys()
        .chain(after.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for path in paths {
        let (old, new) = (before.get(&path), after.get(&path));
        let kind = match (old, new) {
            (None, None) => continue,
            (Some(a), Some(b)) if a.same(b) => match b {
                Entry::File { ctime, .. } if *ctime >= forked_ns => 'M',
                _ => continue,
            },
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            (Some(Entry::Link(_)), Some(Entry::File { .. }))
            | (Some(Entry::File { .. }), Some(Entry::Link(_))) => 'T',
            _ => 'M',
        };
        let mut read =
            |root: &Dir, entry: Option<&Entry>| -> Result<Option<(u32, Vec<u8>)>, String> {
                Ok(match entry {
                    None => None,
                    Some(Entry::Link(target)) => Some((0o120_000, target.clone())),
                    Some(Entry::File { executable, .. }) => {
                        let bytes = read_at(root, &path, FILE_LIMIT).map_err(|e| {
                            format!("capture limit: {}: {e}", String::from_utf8_lossy(&path))
                        })?;
                        total += bytes.len() as u64;
                        if total > CAPTURE_LIMIT {
                            return Err("capture limit: changed bytes exceed 256 MiB".into());
                        }
                        Some((if *executable { 0o100_755 } else { 0o100_644 }, bytes))
                    }
                })
            };
        let old = read(base, old)?;
        let new = read(fork, new)?;
        if kind == 'M' && old == new {
            continue;
        }
        for (_, bytes) in old.iter().chain(new.iter()) {
            write_blob(store, bytes)?;
        }
        if kind == 'T' {
            write_patch(&mut patch, &path, old.as_ref(), None);
            write_patch(&mut patch, &path, None, new.as_ref());
        } else {
            write_patch(&mut patch, &path, old.as_ref(), new.as_ref());
        }
        changes.push(Change { path, kind });
    }
    if !changes.is_empty() {
        let mut files = Vec::new();
        for change in &changes {
            files.push(change.kind as u8);
            files.push(b'\t');
            files.extend_from_slice(listing(&change.path).as_bytes());
            files.push(b'\n');
        }
        out.write_new("files", &files)
            .map_err(|e| format!("out/files: {e}"))?;
        out.write_new("diff.patch", &patch)
            .map_err(|e| format!("out/diff.patch: {e}"))?;
    }
    Ok(changes)
}

/// A `files` path: raw UTF-8 unless it holds control bytes, `"`, `\`, or
/// invalid UTF-8, which are C-quoted as Git does, so one line is one path.
#[must_use]
pub fn listing(path: &[u8]) -> String {
    match std::str::from_utf8(path) {
        Ok(text)
            if !text
                .chars()
                .any(|c| c.is_control() || c == '"' || c == '\\') =>
        {
            text.to_owned()
        }
        _ => quoted("", path),
    }
}

fn blob_id(bytes: &[u8]) -> String {
    let mut digest = Sha1::new();
    digest.update(format!("blob {}\0", bytes.len()));
    digest.update(bytes);
    digest.finalize().iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A loose blob in `store.git/objects` (the `--3way` preimages).
fn write_blob(store: &Dir, bytes: &[u8]) -> Result<(), String> {
    let id = blob_id(bytes);
    let dir = store
        .ensure("objects")
        .and_then(|objects| objects.ensure(&id[..2]))
        .map_err(|e| format!("store.git: {e}"))?;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let _ = encoder.write_all(format!("blob {}\0", bytes.len()).as_bytes());
    let _ = encoder.write_all(bytes);
    let data = encoder.finish().map_err(|e| e.to_string())?;
    match dir.write_new_mode(&id[2..], &data, 0o444) {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            Err(format!("store.git: {error}"))
        }
        _ => Ok(()),
    }
}

/// Git's C-style quoting for unusual path bytes (like `core.quotePath`).
fn quoted(prefix: &str, path: &[u8]) -> String {
    let plain = path
        .iter()
        .all(|b| (0x20..0x7f).contains(b) && *b != b'"' && *b != b'\\');
    if plain {
        return format!("{prefix}{}", String::from_utf8_lossy(path));
    }
    let mut text = format!("\"{prefix}");
    for byte in path {
        match byte {
            b'"' => text.push_str("\\\""),
            b'\\' => text.push_str("\\\\"),
            b'\t' => text.push_str("\\t"),
            b'\n' => text.push_str("\\n"),
            0x20..0x7f => text.push(char::from(*byte)),
            _ => {
                let _ = write!(text, "\\{byte:03o}");
            }
        }
    }
    text.push('"');
    text
}

fn binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0) || std::str::from_utf8(bytes).is_err()
}

const ZERO_ID: &str = "0000000000000000000000000000000000000000";

/// One `git apply` block from `old` to `new` (`None` = absent).
fn write_patch(
    out: &mut Vec<u8>,
    path: &[u8],
    old: Option<&(u32, Vec<u8>)>,
    new: Option<&(u32, Vec<u8>)>,
) {
    let (a, b) = (quoted("a/", path), quoted("b/", path));
    let _ = writeln!(out, "diff --git {a} {b}");
    let old_id = old.map_or(ZERO_ID.to_owned(), |(_, bytes)| blob_id(bytes));
    let new_id = new.map_or(ZERO_ID.to_owned(), |(_, bytes)| blob_id(bytes));
    match (old, new) {
        (None, Some((mode, _))) => {
            let _ = writeln!(out, "new file mode {mode:o}\nindex {old_id}..{new_id}");
        }
        (Some((mode, _)), None) => {
            let _ = writeln!(out, "deleted file mode {mode:o}\nindex {old_id}..{new_id}");
        }
        (Some((old_mode, _)), Some((new_mode, _))) if old_mode != new_mode => {
            let _ = writeln!(out, "old mode {old_mode:o}\nnew mode {new_mode:o}");
            if old_id != new_id {
                let _ = writeln!(out, "index {old_id}..{new_id}");
            }
        }
        (Some((mode, _)), Some(_)) => {
            let _ = writeln!(out, "index {old_id}..{new_id} {mode:o}");
        }
        (None, None) => return,
    }
    if old_id == new_id {
        return;
    }
    let empty = Vec::new();
    let old_bytes = old.map_or(&empty, |(_, bytes)| bytes);
    let new_bytes = new.map_or(&empty, |(_, bytes)| bytes);
    let minus = if old.is_some() { a } else { "/dev/null".into() };
    let plus = if new.is_some() { b } else { "/dev/null".into() };
    if binary(old_bytes) || binary(new_bytes) {
        let _ = writeln!(out, "GIT binary patch");
        literal(out, new_bytes);
        literal(out, old_bytes);
        return;
    }
    let _ = writeln!(out, "--- {minus}\n+++ {plus}");
    let old_text = std::str::from_utf8(old_bytes).unwrap_or_default();
    let new_text = std::str::from_utf8(new_bytes).unwrap_or_default();
    let diff = similar::TextDiff::configure()
        .deadline(Instant::now() + DIFF_DEADLINE)
        .diff_lines(old_text, new_text);
    let mut unified = diff.unified_diff();
    unified.context_radius(3).missing_newline_hint(true);
    for hunk in unified.iter_hunks() {
        let _ = hunk.to_writer(&mut *out);
    }
}

const BASE85: &[u8; 85] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

/// A `literal` hunk: zlib-deflated bytes in Git's base85 line format.
fn literal(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = encoder.write_all(bytes);
    let data = encoder.finish().unwrap_or_default();
    let _ = writeln!(out, "literal {}", bytes.len());
    for line in data.chunks(52) {
        let length = u8::try_from(line.len()).unwrap_or(52);
        out.push(if length <= 26 {
            b'A' + length - 1
        } else {
            b'a' + length - 27
        });
        for group in line.chunks(4) {
            let mut word = [0u8; 4];
            word[..group.len()].copy_from_slice(group);
            let mut value = u32::from_be_bytes(word);
            let mut digits = [0u8; 5];
            for slot in digits.iter_mut().rev() {
                *slot = BASE85[(value % 85) as usize];
                value /= 85;
            }
            out.extend_from_slice(&digits);
        }
        out.push(b'\n');
    }
    out.push(b'\n');
}

/// Remove `name` from `parent` only if it is still the directory recorded
/// at create; recursion is by descriptor.
///
/// # Errors
/// Returns `workspace replaced` or the removal failure.
pub fn remove_verified(parent: &Dir, name: &str, expected: (u64, u64)) -> Result<(), String> {
    if parent.stat(name).map_err(|e| e.to_string())?.identity != expected {
        return Err("workspace replaced".into());
    }
    parent.remove_tree(name).map_err(|e| e.to_string())
}

/// Like [`remove_verified`], but renames the directory into `trash`
/// (through both descriptors) and deletes it in the background.
///
/// # Errors
/// Returns `workspace replaced` or the rename failure.
pub fn remove_verified_detached(
    parent: &Dir,
    name: &str,
    expected: (u64, u64),
    trash: &Dir,
) -> Result<(), String> {
    if parent.stat(name).map_err(|e| e.to_string())?.identity != expected {
        return Err("workspace replaced".into());
    }
    let moved = format!("{name}-{}", uuid::Uuid::new_v4().simple());
    parent
        .rename(name, trash, &moved)
        .map_err(|e| e.to_string())?;
    let trash = trash.dup().map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = remove_verified(&trash, &moved, expected);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Each test pins a boundary between guest-writable files and the host:
    //! descriptor-only access, the snapshot walker, and patches that
    //! reproduce a fork exactly under `git apply`.
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@x",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn contents(root: &Path) -> BTreeMap<PathBuf, (u32, Vec<u8>)> {
        let mut found = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let key = path.strip_prefix(root).unwrap().to_path_buf();
                if meta.file_type().is_symlink() {
                    found.insert(
                        key,
                        (0, fs::read_link(&path).unwrap().into_os_string().into_vec()),
                    );
                } else if meta.is_dir() {
                    if key != Path::new(".git") {
                        stack.push(path);
                    }
                } else {
                    found.insert(
                        key,
                        (meta.permissions().mode() & 0o100, fs::read(&path).unwrap()),
                    );
                }
            }
        }
        found
    }

    struct Split {
        _temp: tempfile::TempDir,
        root: PathBuf,
        base: PathBuf,
        fork: PathBuf,
    }

    fn split() -> Split {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        for dir in ["base", "out", "store"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        Split {
            base: root.join("base"),
            fork: root.join("fork"),
            root,
            _temp: temp,
        }
    }

    fn capture_in(split: &Split) -> Result<Vec<Change>, String> {
        capture_with(split, None)
    }

    fn capture_with(split: &Split, exclude: Option<&[u8]>) -> Result<Vec<Change>, String> {
        let root = Dir::open_root(&split.root).unwrap();
        capture(
            &root.open("base").unwrap(),
            &root.open("fork").unwrap(),
            exclude,
            &root.open("out").unwrap(),
            &root.open("store").unwrap(),
            0,
        )
    }

    #[test]
    fn planted_symlinks_are_never_followed_by_the_daemon() {
        let temp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(temp.path()).unwrap();
        let outside = root.join("outside");
        fs::create_dir_all(outside.join("target")).unwrap();
        fs::write(outside.join("secret"), "host secret\n").unwrap();
        let project = root.join("project");
        fs::create_dir_all(project.join(".marsh")).unwrap();
        symlink(&outside, project.join(".marsh/split")).unwrap();
        assert!(
            splits_dir(&project, true).is_err(),
            "planted .marsh/split followed"
        );
        assert!(!outside.join(".gitignore").exists());
        fs::remove_file(project.join(".marsh/split")).unwrap();
        let splits = splits_dir(&project, true).unwrap();
        let split = splits.create("s1").unwrap();
        // A branch replaces its out dir and a file in it with symlinks.
        let out = split.create("out").unwrap();
        symlink(&outside, project.join(".marsh/split/s1/out/a")).unwrap();
        assert!(out.open("a").is_err());
        let label = out.create("b").unwrap();
        symlink(
            outside.join("target/stdout"),
            project.join(".marsh/split/s1/out/b/stdout"),
        )
        .unwrap();
        assert!(label.write_new("stdout", b"branch bytes").is_err());
        assert!(!outside.join("target/stdout").exists());
        // Reads refuse symlinks and FIFOs (without blocking).
        symlink(
            outside.join("secret"),
            project.join(".marsh/split/s1/out/b/link"),
        )
        .unwrap();
        assert!(label.read("link", 1 << 20).is_err());
        let fifo = project.join(".marsh/split/s1/out/b/fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(label.read("fifo", 1 << 20).is_err());
    }

    #[test]
    fn capture_reads_no_file_through_a_symlinked_directory() {
        let split = split();
        let outside = split.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("f"), "host secret\n").unwrap();
        fs::create_dir(split.base.join("sub")).unwrap();
        fs::write(split.base.join("sub/f"), "mine\n").unwrap();
        fs::create_dir(&split.fork).unwrap();
        symlink(&outside, split.fork.join("sub")).unwrap();
        capture_in(&split).unwrap();
        let patch = fs::read_to_string(split.root.join("out/diff.patch")).unwrap();
        assert!(!patch.contains("host secret"), "{patch}");
        assert!(patch.contains("new file mode 120000"));
        // Reading a path through a symlinked component fails.
        let root = Dir::open_root(&split.root).unwrap();
        assert!(read_at(&root.open("fork").unwrap(), b"sub/f", 1 << 20).is_err());
    }

    #[test]
    fn snapshot_prunes_ignored_files_and_nested_repositories() {
        let split = split();
        let project = split.root.join("project");
        fs::create_dir_all(project.join("vendor/lib")).unwrap();
        fs::create_dir_all(project.join("src/gen")).unwrap();
        fs::create_dir_all(project.join("target")).unwrap();
        fs::write(project.join(".gitignore"), "target/\n*.log\n!keep.log\n").unwrap();
        fs::write(project.join("src/.gitignore"), "gen/\n").unwrap();
        fs::write(project.join("a.log"), "x").unwrap();
        fs::write(project.join("keep.log"), "x").unwrap();
        fs::write(project.join("target/big"), "x").unwrap();
        fs::write(project.join("src/gen/out.rs"), "x").unwrap();
        fs::write(project.join("src/lib.rs"), "x").unwrap();
        fs::write(project.join("vendor/lib/lib.c"), "x").unwrap();
        assert!(
            git(&project.join("vendor/lib"), &["init", "-q"])
                .status
                .success()
        );
        fs::write(project.join("vendor/lib/.GIT"), "gitdir: /elsewhere\n").ok();
        symlink("/etc/passwd", project.join("link")).unwrap();
        assert!(git(&project, &["init", "-q"]).status.success());
        let git = git_source(&project).unwrap().unwrap();
        let root = Dir::open_root(&split.root).unwrap();
        snapshot(
            &Dir::open_root(&project).unwrap(),
            &root.open("base").unwrap(),
            Some(&git),
        )
        .unwrap();
        let found = contents(&split.base);
        let names = found
            .keys()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            names,
            [
                ".gitignore",
                "keep.log",
                "link",
                "src/.gitignore",
                "src/lib.rs",
                "vendor/lib/lib.c"
            ]
            .into_iter()
            .map(String::from)
            .collect::<BTreeSet<_>>()
        );
        assert!(!split.base.join(".git").exists() && !split.base.join("vendor/lib/.git").exists());
        assert_eq!(
            fs::read_link(split.base.join("link")).unwrap(),
            Path::new("/etc/passwd")
        );
        // A gitfile project (linked worktree or submodule) is refused.
        let worktree = split.root.join("worktree");
        fs::create_dir(&worktree).unwrap();
        fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert!(git_source(&worktree).unwrap_err().contains("gitfile"));
    }

    #[test]
    fn hostile_names_round_trip_through_git_apply_and_files() {
        let split = split();
        let names: [&[u8]; 6] = [
            b"new\nline",
            b"tab\there",
            b"quo\"te",
            b"back\\slash",
            b"-rf",
            "sp ace \u{fc}".as_bytes(),
        ];
        fs::write(
            split.base.join("binary"),
            (0..=255u8).cycle().take(3000).collect::<Vec<_>>(),
        )
        .unwrap();
        fs::write(split.base.join("mode"), "#!/bin/sh\n").unwrap();
        fs::write(split.base.join("same-size"), "aaaa\n").unwrap();
        Command::new("cp")
            .args(["-Rp"])
            .arg(&split.base)
            .arg(&split.fork)
            .status()
            .unwrap();
        for name in names {
            fs::write(split.fork.join(OsStr::from_bytes(name)), b"x\n").unwrap();
        }
        fs::write(split.fork.join("binary"), [0u8, 1, 2]).unwrap();
        fs::set_permissions(split.fork.join("mode"), fs::Permissions::from_mode(0o755)).unwrap();
        // Same size and mtime restored: still seen (ctime after the fork).
        let mtime = fs::metadata(split.base.join("same-size"))
            .unwrap()
            .modified()
            .unwrap();
        fs::write(split.fork.join("same-size"), "bbbb\n").unwrap();
        fs::File::options()
            .write(true)
            .open(split.fork.join("same-size"))
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let changes = capture_in(&split).unwrap();
        assert_eq!(changes.len(), names.len() + 3);
        let files = fs::read(split.root.join("out/files")).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&files).lines().count(),
            changes.len()
        );
        assert!(files.windows(10).any(|w| w == b"\"new\\nline"));
        let applied = split.root.join("applied");
        Command::new("cp")
            .args(["-Rp"])
            .arg(&split.base)
            .arg(&applied)
            .status()
            .unwrap();
        let output = git(
            &applied,
            &[
                "apply",
                &split.root.join("out/diff.patch").to_string_lossy(),
            ],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(contents(&applied), contents(&split.fork));
    }

    #[test]
    fn store_blobs_let_git_apply_3way_merge_over_later_user_edits() {
        let split = split();
        let user = split.root.join("user");
        fs::create_dir(&user).unwrap();
        fs::write(user.join("f"), "a\nb\nc\n").unwrap();
        assert!(git(&user, &["init", "-q"]).status.success());
        git(&user, &["add", "f"]);
        git(&user, &["commit", "-qm", "seed"]);
        // The snapshot holds an uncommitted edit; its blob is only in store.
        fs::write(user.join("f"), "x\nb\nc\n").unwrap();
        fs::copy(user.join("f"), split.base.join("f")).unwrap();
        fs::create_dir(&split.fork).unwrap();
        fs::write(split.fork.join("f"), "x\nb\nZ\n").unwrap();
        capture_in(&split).unwrap();
        // Meanwhile the user changed the first line again.
        fs::write(user.join("f"), "y\nb\nc\n").unwrap();
        git(&user, &["add", "f"]);
        let patch = split.root.join("out/diff.patch");
        let plain = git(&user, &["apply", "--3way", &patch.to_string_lossy()]);
        assert!(
            !plain.status.success(),
            "3way should need the store's preimage"
        );
        let output = Command::new("git")
            .args(["apply", "--3way", &patch.to_string_lossy()])
            .current_dir(&user)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                split.root.join("store/objects"),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read_to_string(user.join("f")).unwrap(), "y\nb\nZ\n");
    }

    #[test]
    fn capture_leaves_out_ignored_files_a_branch_creates() {
        let split = split();
        let files: &[(&str, &str)] = &[
            (".gitignore", "__pycache__/\n*.log\n"),
            ("x.py", "import os\n"),
            // In the snapshot although it matches a rule: compared like a
            // tracked file.
            ("keep.log", "one\n"),
        ];
        for (path, bytes) in files {
            fs::write(split.base.join(path), bytes).unwrap();
        }
        Command::new("cp")
            .args(["-Rp"])
            .arg(&split.base)
            .arg(&split.fork)
            .status()
            .unwrap();
        fs::create_dir_all(split.fork.join("__pycache__")).unwrap();
        fs::write(split.fork.join("__pycache__/x.cpython-313.pyc"), b"\0pyc").unwrap();
        fs::create_dir_all(split.fork.join("src/__pycache__")).unwrap();
        fs::write(split.fork.join("src/__pycache__/y.pyc"), b"\0pyc").unwrap();
        fs::write(split.fork.join("src/y.py"), "y = 1\n").unwrap();
        fs::write(split.fork.join("run.log"), "new\n").unwrap();
        fs::write(split.fork.join("keep.log"), "two\n").unwrap();
        fs::write(split.fork.join("scratch.tmp"), "x\n").unwrap();
        // A `.gitignore` the branch writes applies to what it creates.
        fs::create_dir_all(split.fork.join("gen")).unwrap();
        fs::write(split.fork.join("gen/.gitignore"), "*.out\n").unwrap();
        fs::write(split.fork.join("gen/a.out"), "x\n").unwrap();
        let changes = capture_with(&split, Some(b"*.tmp\n")).unwrap();
        let mut paths = changes
            .iter()
            .map(|change| format!("{} {}", change.kind, String::from_utf8_lossy(&change.path)))
            .collect::<Vec<_>>();
        paths.sort();
        assert_eq!(
            paths,
            ["A gen/.gitignore", "A src/y.py", "M keep.log"],
            "{paths:?}"
        );
        let patch = fs::read_to_string(split.root.join("out/diff.patch")).unwrap();
        assert!(
            !patch.contains("pyc") && !patch.contains("run.log"),
            "{patch}"
        );
    }
}
