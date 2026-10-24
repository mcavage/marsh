//! Host side of cloud workspace transfer (`docs/design/cloud-placement.md` s4):
//! the snapshot written from the caller's admitted view and the returned
//! delta applied back into it.
//!
//! Both directions use [`split_fs::Dir`]: every component is opened by
//! descriptor with `O_NOFOLLOW`, so a symlink in a parent position is refused
//! rather than followed. The delta is validated completely before the first
//! write and applied per entry; there is no multi-file atomicity and no
//! content comparison with the destination (overlapping edits are overwritten).

use crate::split_fs::{self, Dir, Rules};
use marsh_contracts::cloud::{self, CloudRequest, Kind, Reader, Writer};
use rustix::fs::FileType;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

/// What a snapshot carried.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SnapshotStats {
    pub files: u64,
    pub bytes: u64,
}

/// Where the caller's Git metadata comes from, for a private remote copy.
#[derive(Clone, Debug)]
pub enum GitView {
    /// No repository (or none to transfer).
    None,
    /// The project's own `.git` directory.
    Project,
    /// A split fork: its daemon-written admin directory, plus the object
    /// stores a fork reads (the split's `store.git/objects` and the project's
    /// `.git/objects`), all opened from the pinned project component by
    /// component without following links. The admin's own `alternates`
    /// file is never trusted (a branch job can write it). The copy's
    /// alternates name the copies under `fork` (the same path remotely).
    Fork {
        project: PathBuf,
        split: String,
        label: String,
        fork: PathBuf,
    },
}

/// Git metadata larger than this is not uploaded (the command still runs).
pub const GIT_LIMIT: u64 = 512 << 20;

/// What the snapshot did with the caller's Git metadata.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum GitOutcome {
    #[default]
    Absent,
    Copied,
    /// Not uploaded, with the reason (reported on stderr).
    Skipped(String),
}

/// Write `request` and the view at `root` into `out`. `filtered` applies the
/// project's Git ignore rules (the first crossing from a session project); a
/// split fork is already a filtered snapshot and is copied as it is now.
/// `.git` components, top-level `.marsh`, and special files never cross.
///
/// # Errors
/// Returns the first unreadable entry, an unstable file, or the byte limit.
pub fn write_snapshot<W: Write>(
    root: &Dir,
    filtered: bool,
    git_view: &GitView,
    request: &CloudRequest,
    out: W,
    limit: u64,
) -> Result<(W, SnapshotStats, GitOutcome), String> {
    let mut writer = Writer::new(out, limit).map_err(|e| e.to_string())?;
    writer.request(request).map_err(|e| e.to_string())?;
    let mut rules = Rules(Vec::new());
    let git = filtered
        && root
            .stat(".git")
            .is_ok_and(|stat| stat.kind == FileType::Directory);
    if git {
        if let Some(global) =
            Some(ignore::gitignore::Gitignore::global().0).filter(|g| !g.is_empty())
        {
            rules.0.push(global);
        }
        if let Some(exclude) = root
            .open(".git")
            .and_then(|git| git.open("info"))
            .and_then(|info| info.read("exclude", split_fs::SMALL_LIMIT))
            .ok()
            .and_then(|bytes| Rules::parse(Path::new("/"), &bytes))
        {
            rules.0.push(exclude);
        }
    }
    let mut stats = SnapshotStats::default();
    walk(root, b"", &mut rules, git, &mut writer, &mut stats)?;
    let outcome = write_git(root, git_view, &mut writer)?;
    let out = writer.finish().map_err(|e| e.to_string())?;
    Ok((out, stats, outcome))
}

/// Git paths a remote copy never needs, or that could act on the remote
/// side: hooks, logs, locks, linked worktrees, submodule checkouts, LFS
/// storage, and the alternates file (rewritten for a fork).
fn git_skipped(path: &[u8]) -> bool {
    // An allowlist: what `git status`, `diff`, `log`, and commits need. Hooks,
    // configs (`config.worktree` included), logs, tool state such as
    // git-crypt keys, and alternates (host paths) never cross.
    let kept = matches!(
        path,
        b"HEAD"
            | b"index"
            | b"packed-refs"
            | b"shallow"
            | b"info"
            | b"info/exclude"
            | b"refs"
            | b"objects"
            | b"reftable"
    ) || path.starts_with(b"refs/")
        || path.starts_with(b"objects/")
        || path.starts_with(b"reftable/");
    !kept || path == b"objects/info/alternates" || path.ends_with(b".lock")
}

/// A minimal config: repository format and extensions only, never remotes,
/// credential helpers, hooks paths, or commands from the caller's config.
fn sanitized_config(config: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(config);
    let mut section = String::new();
    let mut version = "0".to_owned();
    let mut extensions = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_ascii_lowercase();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        let plain = |text: &str| {
            !text.is_empty()
                && text
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        };
        if section == "core" && key == "repositoryformatversion" && plain(value) {
            value.clone_into(&mut version);
        } else if section == "extensions"
            && matches!(
                key.as_str(),
                "objectformat" | "compatobjectformat" | "refstorage"
            )
            && plain(value)
        {
            extensions.push(format!("\t{key} = {value}\n"));
        }
    }
    let mut out = format!(
        "[core]\n\trepositoryformatversion = {version}\n\tfilemode = true\n\tbare = false\n"
    );
    if !extensions.is_empty() {
        out.push_str("[extensions]\n");
        out.extend(extensions);
    }
    out.into_bytes()
}

/// One Git source tree to copy: (directory, archive prefix).
struct GitTree {
    dir: Dir,
    prefix: Vec<u8>,
    skip: bool,
}

fn measure(dir: &Dir, relative: &[u8], skip: bool, total: &mut u64) -> Result<(), String> {
    for (name, kind) in dir.entries().map_err(|e| e.to_string())? {
        let path = join_relative(relative, name.as_bytes());
        // Raw object stores keep everything but their own alternates (host paths).
        if (skip && git_skipped(&path)) || path == b"info/alternates" {
            continue;
        }
        match kind {
            FileType::Directory => {
                let child = dir.open(&name).map_err(|e| e.to_string())?;
                measure(&child, &path, skip, total)?;
            }
            FileType::RegularFile => {
                let size = dir.stat(&name).map_err(|e| e.to_string())?.size;
                if size > cloud::MAX_FILE {
                    return Err(format!(
                        "{} is larger than {} MiB",
                        String::from_utf8_lossy(&path),
                        cloud::MAX_FILE >> 20
                    ));
                }
                *total += size;
                if *total > GIT_LIMIT {
                    return Err(format!("larger than {} MiB", GIT_LIMIT >> 20));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn join_relative(relative: &[u8], name: &[u8]) -> Vec<u8> {
    if relative.is_empty() {
        name.to_vec()
    } else {
        [relative, b"/", name].concat()
    }
}

fn copy_git<W: Write>(
    dir: &Dir,
    relative: &[u8],
    prefix: &[u8],
    skip: bool,
    writer: &mut Writer<W>,
) -> Result<(), String> {
    for (name, kind) in dir.entries().map_err(|e| e.to_string())? {
        let path = join_relative(relative, name.as_bytes());
        // Raw object stores keep everything but their own alternates (host paths).
        if (skip && git_skipped(&path)) || path == b"info/alternates" {
            continue;
        }
        let archived = [prefix, b"/", &path].concat();
        if !cloud::valid_relative(&archived) {
            continue;
        }
        match kind {
            FileType::Directory => {
                let child = dir.open(&name).map_err(|e| transfer_error(&archived, &e))?;
                let mode = child
                    .fstat()
                    .map_err(|e| transfer_error(&archived, &e))?
                    .mode;
                writer
                    .directory(&archived, mode | 0o700)
                    .map_err(|e| transfer_error(&archived, &e))?;
                copy_git(&child, &path, prefix, skip, writer)?;
            }
            FileType::RegularFile => {
                let (mode, bytes) = stable_read(dir, &name, &archived)?;
                writer
                    .file(&archived, mode | 0o600, bytes.len() as u64, &mut &bytes[..])
                    .map_err(|e| transfer_error(&archived, &e))?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Copy the caller's Git metadata into a private, hook-free `.git` in the
/// snapshot. It is never returned (`.git` is protected on return), so remote
/// commits, index changes, and new objects stay in the sandbox.
#[allow(clippy::too_many_lines)] // Two source shapes, one measured, ordered copy.
fn write_git<W: Write>(
    root: &Dir,
    view: &GitView,
    writer: &mut Writer<W>,
) -> Result<GitOutcome, String> {
    let (trees, config, alternates) = match view {
        GitView::None => return Ok(GitOutcome::Absent),
        GitView::Project => {
            let Ok(dir) = root.open(".git") else {
                return Ok(GitOutcome::Absent);
            };
            let config = sanitized_config(
                &dir.read("config", split_fs::SMALL_LIMIT)
                    .unwrap_or_default(),
            );
            let tree = GitTree {
                dir,
                prefix: b".git".to_vec(),
                skip: true,
            };
            (vec![tree], config, None)
        }
        GitView::Fork {
            project,
            split,
            label,
            fork,
        } => {
            let Ok(project) = Dir::open_root(project) else {
                return Ok(GitOutcome::Absent);
            };
            let split_dir = Path::new(".marsh/split").join(split);
            let Ok(dir) = project.open_path(&split_dir.join(".admin").join(label)) else {
                return Ok(GitOutcome::Absent);
            };
            let config = sanitized_config(
                &dir.read("config", split_fs::SMALL_LIMIT)
                    .unwrap_or_default(),
            );
            let mut trees = vec![GitTree {
                dir,
                prefix: b".git".to_vec(),
                skip: true,
            }];
            let mut remote = Vec::new();
            let stores = [
                split_dir.join("store.git/objects"),
                PathBuf::from(".git/objects"),
            ];
            for (index, store) in stores.iter().enumerate() {
                let Ok(objects) = project.open_path(store) else {
                    continue;
                };
                let prefix = format!(".git/marsh-objects/{index}").into_bytes();
                remote.push(format!("{}/.git/marsh-objects/{index}", fork.display()));
                trees.push(GitTree {
                    dir: objects,
                    prefix,
                    skip: false,
                });
            }
            (trees, config, Some(remote))
        }
    };
    let mut total = 0;
    for tree in &trees {
        if let Err(reason) = measure(&tree.dir, b"", tree.skip, &mut total) {
            return Ok(GitOutcome::Skipped(reason));
        }
    }
    writer
        .directory(b".git", 0o755)
        .map_err(|e| e.to_string())?;
    for tree in &trees {
        if tree.prefix != b".git" {
            if tree.prefix.starts_with(b".git/marsh-objects/") {
                let _ = writer.directory(b".git/marsh-objects", 0o755);
            }
            writer
                .directory(&tree.prefix, 0o755)
                .map_err(|e| e.to_string())?;
        }
        copy_git(&tree.dir, b"", &tree.prefix, tree.skip, writer)?;
    }
    writer
        .file(b".git/config", 0o644, config.len() as u64, &mut &config[..])
        .map_err(|e| e.to_string())?;
    if let Some(remote) = alternates {
        let text = remote.iter().fold(String::new(), |mut text, line| {
            text.push_str(line);
            text.push('\n');
            text
        });
        let _ = writer.directory(b".git/objects", 0o755);
        let _ = writer.directory(b".git/objects/info", 0o755);
        writer
            .file(
                b".git/objects/info/alternates",
                0o644,
                text.len() as u64,
                &mut text.as_bytes(),
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(GitOutcome::Copied)
}

fn transfer_error(path: &[u8], error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::FileTooLarge {
        format!(
            "{}: {error} (cloud transfers are limited to {} MiB per file and {} MiB per snapshot)",
            String::from_utf8_lossy(path),
            cloud::MAX_FILE >> 20,
            SNAPSHOT_LIMIT >> 20
        )
    } else {
        format!("{}: {error}", String::from_utf8_lossy(path))
    }
}

/// Snapshot upload limit.
pub const SNAPSHOT_LIMIT: u64 = 1 << 30;

fn walk<W: Write>(
    dir: &Dir,
    relative: &[u8],
    rules: &mut Rules,
    git: bool,
    writer: &mut Writer<W>,
    stats: &mut SnapshotStats,
) -> Result<(), String> {
    let logical = |path: &[u8]| split_fs::logical(Path::new(OsStr::from_bytes(path)));
    let pushed = git
        && dir
            .read(".gitignore", split_fs::SMALL_LIMIT)
            .ok()
            .and_then(|bytes| Rules::parse(&logical(relative), &bytes))
            .map(|level| rules.0.push(level))
            .is_some();
    for (name, kind) in dir.entries().map_err(|e| transfer_error(relative, &e))? {
        let path = if relative.is_empty() {
            name.as_bytes().to_vec()
        } else {
            [relative, b"/", name.as_bytes()].concat()
        };
        if cloud::protected(&path) || split_fs::is_dotgit(&name) {
            continue;
        }
        if !cloud::valid_relative(&path) {
            return Err(format!(
                "{}: name cannot be transferred",
                String::from_utf8_lossy(&path)
            ));
        }
        let directory = kind == FileType::Directory;
        if git && rules.ignored(&logical(&path), directory) {
            continue;
        }
        match kind {
            FileType::Directory => {
                let child = dir.open(&name).map_err(|e| transfer_error(&path, &e))?;
                let mode = child.fstat().map_err(|e| transfer_error(&path, &e))?.mode;
                writer
                    .directory(&path, mode)
                    .map_err(|e| transfer_error(&path, &e))?;
                walk(&child, &path, rules, git, writer, stats)?;
            }
            FileType::RegularFile => {
                let (mode, bytes) = stable_read(dir, &name, &path)?;
                stats.files += 1;
                stats.bytes += bytes.len() as u64;
                writer
                    .file(&path, mode, bytes.len() as u64, &mut &bytes[..])
                    .map_err(|e| transfer_error(&path, &e))?;
            }
            FileType::Symlink => {
                let target = dir
                    .read_link(&name)
                    .map_err(|e| transfer_error(&path, &e))?;
                writer
                    .symlink(&path, target.as_bytes())
                    .map_err(|e| transfer_error(&path, &e))?;
            }
            _ => {}
        }
    }
    if pushed {
        rules.0.pop();
    }
    Ok(())
}

/// One file's bytes, re-read up to three times if it changed meanwhile.
fn stable_read(dir: &Dir, name: &OsStr, path: &[u8]) -> Result<(u32, Vec<u8>), String> {
    for _ in 0..3 {
        let before = dir.stat(name).map_err(|e| transfer_error(path, &e))?;
        let bytes = dir
            .read(name, cloud::MAX_FILE)
            .map_err(|e| transfer_error(path, &e))?;
        let after = dir.stat(name).map_err(|e| transfer_error(path, &e))?;
        if (before.size, before.mtime, before.identity) == (after.size, after.mtime, after.identity)
        {
            return Ok((after.mode & 0o777, bytes));
        }
    }
    Err(format!(
        "{}: file kept changing",
        String::from_utf8_lossy(path)
    ))
}

/// What one apply changed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ApplyReport {
    pub created: u64,
    pub modified: u64,
    pub deleted: u64,
    /// Directories deleted remotely but kept here because they still hold
    /// local files that were never uploaded (ignored or created meanwhile).
    pub kept: Vec<String>,
    /// Directories (relative; empty is the root) whose listing changed: a
    /// caller that caches listings (a VM's file share) refreshes these.
    pub listings: std::collections::BTreeSet<Vec<u8>>,
}

impl ApplyReport {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.created + self.modified + self.deleted
    }
}

/// A failed apply: why, and what had already been written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyError {
    pub message: String,
    pub applied: ApplyReport,
}

#[derive(Clone, Debug)]
struct Op {
    kind: Kind,
    path: Vec<u8>,
    mode: u32,
    offset: u64,
    len: u64,
}

#[allow(clippy::naive_bytecount)] // Short paths; no bytecount dependency.
fn depth(path: &[u8]) -> usize {
    path.iter().filter(|byte| **byte == b'/').count()
}

fn split_name(path: &[u8]) -> (&[u8], &OsStr) {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(index) => (&path[..index], OsStr::from_bytes(&path[index + 1..])),
        None => (&[][..], OsStr::from_bytes(path)),
    }
}

/// Validate every entry before any write: plain relative paths, protected
/// names, ASCII case aliases within the delta, at most one delete and one
/// creation per path, and no descendants under a created file or symlink.
fn manifest(archive: &Path, limit: u64) -> Result<Vec<Op>, String> {
    let invalid = |message: &str| format!("returned delta rejected: {message}");
    let mut reader = Reader::new(
        BufReader::new(File::open(archive).map_err(|e| invalid(&e.to_string()))?),
        limit,
    )
    .map_err(|e| invalid(&e.to_string()))?;
    let mut ops = Vec::new();
    let mut position = 4_u64;
    while let Some(header) = reader.next_header().map_err(|e| invalid(&e.to_string()))? {
        position += 17 + header.path.len() as u64;
        if header.kind == Kind::Request {
            return Err(invalid("unexpected request entry"));
        }
        if cloud::protected(&header.path) {
            return Err(invalid(&format!(
                "protected path {}",
                String::from_utf8_lossy(&header.path)
            )));
        }
        ops.push(Op {
            kind: header.kind,
            path: header.path,
            mode: header.mode,
            offset: position,
            len: header.len,
        });
        position += header.len;
    }
    let mut deletes = BTreeSet::new();
    let mut creates = BTreeMap::<Vec<u8>, Kind>::new();
    let mut folded = BTreeMap::<Vec<u8>, Vec<u8>>::new();
    for op in &ops {
        let fresh = if op.kind == Kind::Delete {
            deletes.insert(op.path.clone())
        } else {
            creates.insert(op.path.clone(), op.kind).is_none()
        };
        if !fresh {
            return Err(invalid(&format!(
                "duplicate entry {}",
                String::from_utf8_lossy(&op.path)
            )));
        }
        let key = op.path.to_ascii_lowercase();
        if let Some(other) = folded.insert(key, op.path.clone())
            && other != op.path
        {
            return Err(invalid(&format!(
                "{} and {} name the same file on a case-insensitive volume",
                String::from_utf8_lossy(&other),
                String::from_utf8_lossy(&op.path)
            )));
        }
    }
    for op in &ops {
        let mut parent = &op.path[..];
        while let Some(index) = parent.iter().rposition(|byte| *byte == b'/') {
            parent = &parent[..index];
            // Deleting a directory's old contents under a new file or
            // symlink of the same name is how a retype is expressed.
            let leaf = op.kind != Kind::Delete
                && creates
                    .get(parent)
                    .is_some_and(|kind| *kind != Kind::Directory);
            let deleted_dir = deletes.contains(parent)
                && op.kind != Kind::Delete
                && !creates.contains_key(parent);
            if leaf || deleted_dir {
                return Err(invalid(&format!(
                    "{} is under a replaced entry",
                    String::from_utf8_lossy(&op.path)
                )));
            }
        }
    }
    Ok(ops)
}

enum Existing {
    Absent,
    Present(split_fs::Stat),
}

/// Exact directory listings, read once per directory during one apply and
/// kept current as entries are created and removed.
#[derive(Default)]
struct Listings(BTreeMap<Vec<u8>, BTreeSet<std::ffi::OsString>>);

impl Listings {
    /// The entry `name` in the directory at `parent`, exactly as listed, or
    /// absent. A name the filesystem resolves only by case or Unicode folding
    /// is refused: writing it would replace a different file.
    fn existing(&mut self, dir: &Dir, parent: &[u8], name: &OsStr) -> Result<Existing, String> {
        let stat = match dir.stat(name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Existing::Absent),
            Err(error) => return Err(error.to_string()),
            Ok(stat) => stat,
        };
        if !self.0.contains_key(parent) {
            let names = dir
                .entries()
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|(entry, _)| entry)
                .collect();
            self.0.insert(parent.to_vec(), names);
        }
        if self.0[parent].contains(name) {
            Ok(Existing::Present(stat))
        } else {
            Err(format!(
                "{} aliases another name on this volume",
                name.to_string_lossy()
            ))
        }
    }

    fn added(&mut self, parent: &[u8], name: &OsStr) {
        if let Some(names) = self.0.get_mut(parent) {
            names.insert(name.to_owned());
        }
    }

    fn removed(&mut self, parent: &[u8], name: &OsStr) {
        if let Some(names) = self.0.get_mut(parent) {
            names.remove(name);
        }
    }

    /// Open `parent` under `root` component by component; missing
    /// directories are created (owner-writable) only when `create`. `None`
    /// means a missing parent.
    fn open_parent(
        &mut self,
        root: &Dir,
        parent: &[u8],
        create: bool,
    ) -> Result<Option<Dir>, String> {
        let mut current = root.dup().map_err(|e| e.to_string())?;
        if parent.is_empty() {
            return Ok(Some(current));
        }
        let mut walked = Vec::new();
        for component in parent.split(|byte| *byte == b'/') {
            let name = OsStr::from_bytes(component);
            match self.existing(&current, &walked, name)? {
                Existing::Present(stat) if stat.kind == FileType::Directory => {}
                Existing::Present(_) => {
                    return Err(format!(
                        "{}: a parent is not a directory (symlink parents are never followed)",
                        String::from_utf8_lossy(parent)
                    ));
                }
                Existing::Absent if create => {
                    current.mkdir_mode(name, 0o755).map_err(|e| e.to_string())?;
                    self.added(&walked, name);
                }
                Existing::Absent => return Ok(None),
            }
            current = current.open(name).map_err(|e| e.to_string())?;
            if !walked.is_empty() {
                walked.push(b'/');
            }
            walked.extend_from_slice(component);
        }
        Ok(Some(current))
    }
}

/// A short staging name: never longer than the system's name limit.
fn temporary_name() -> std::ffi::OsString {
    format!(".mc-{:016x}", rand_u64()).into()
}

fn rand_u64() -> u64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    u64::from_le_bytes(bytes[..8].try_into().unwrap_or_default())
}

fn not_empty(error: &io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error())
}

/// Apply a captured delta into the view at `root`, which must still be the
/// directory with `identity` (a replaced root is never written).
///
/// Order: deletions that make room for a retyped entry, then directories
/// (owner-writable while filling), files and symlinks, the remaining
/// deletions deepest first, and finally the delta's directory modes deepest
/// first. A remotely deleted directory that still holds local files is kept
/// and reported, not an error, so creations are never skipped behind it.
///
/// # Errors
/// Returns why the delta was rejected (nothing applied) or which entry failed,
/// with the changes already written.
#[allow(clippy::too_many_lines)] // One ordered, per-entry apply with one report.
pub fn apply_delta(
    root: &Path,
    identity: (u64, u64),
    archive: &Path,
    limit: u64,
) -> Result<ApplyReport, ApplyError> {
    let mut report = ApplyReport::default();
    let fail = |message: String, report: &ApplyReport| ApplyError {
        message,
        applied: report.clone(),
    };
    let ops = manifest(archive, limit).map_err(|message| fail(message, &report))?;
    let root_dir = Dir::open_root(root).map_err(|e| fail(e.to_string(), &report))?;
    if root_dir
        .fstat()
        .map_err(|e| fail(e.to_string(), &report))?
        .identity
        != identity
    {
        return Err(fail("workspace replaced; nothing written".into(), &report));
    }
    let mut data = File::open(archive).map_err(|e| fail(e.to_string(), &report))?;
    let created = ops
        .iter()
        .filter(|op| op.kind != Kind::Delete)
        .map(|op| (op.path.clone(), op.kind))
        .collect::<BTreeMap<_, _>>();
    // Deletions that make room for a created entry: the path itself and,
    // under a new file or symlink, its old directory's contents.
    let makes_room = |path: &[u8]| {
        created.contains_key(path) || {
            let mut parent = path;
            let mut under_leaf = false;
            while let Some(index) = parent.iter().rposition(|byte| *byte == b'/') {
                parent = &parent[..index];
                if created
                    .get(parent)
                    .is_some_and(|kind| *kind != Kind::Directory)
                {
                    under_leaf = true;
                    break;
                }
            }
            under_leaf
        }
    };
    let (mut retypes, mut deletes, mut directories, mut leaves) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for op in ops {
        match op.kind {
            Kind::Delete if makes_room(&op.path) => retypes.push(op),
            Kind::Delete => deletes.push(op),
            Kind::Directory => directories.push(op),
            _ => leaves.push(op),
        }
    }
    retypes.sort_by_key(|op| std::cmp::Reverse(depth(&op.path)));
    deletes.sort_by_key(|op| std::cmp::Reverse(depth(&op.path)));
    directories.sort_by_key(|op| depth(&op.path));
    let at = |op: &Op, message: String| format!("{}: {message}", String::from_utf8_lossy(&op.path));
    let mut listings = Listings::default();

    // Remove one entry; Ok(false) when it was already gone, Err(None) when a
    // directory still holds local files.
    let delete = |listings: &mut Listings, op: &Op| -> Result<bool, Option<String>> {
        let (parent, name) = split_name(&op.path);
        let Some(dir) = listings
            .open_parent(&root_dir, parent, false)
            .map_err(Some)?
        else {
            return Ok(false);
        };
        match listings.existing(&dir, parent, name).map_err(Some)? {
            Existing::Absent => Ok(false),
            Existing::Present(stat) => {
                let result = if stat.kind == FileType::Directory {
                    dir.rmdir(name)
                } else {
                    dir.unlink(name)
                };
                match result {
                    Ok(()) => {
                        listings.removed(parent, name);
                        Ok(true)
                    }
                    Err(error) if not_empty(&error) => Err(None),
                    Err(error) => Err(Some(error.to_string())),
                }
            }
        }
    };
    for op in &retypes {
        match delete(&mut listings, op) {
            Ok(true) => {
                report.deleted += 1;
                report.listings.insert(split_name(&op.path).0.to_vec());
            }
            Ok(false) => {}
            Err(message) => {
                let message = message
                    .unwrap_or_else(|| "directory still has local files; not replaced".into());
                return Err(fail(at(op, message), &report));
            }
        }
    }
    let mut modes = Vec::new();
    for op in &directories {
        let (parent, name) = split_name(&op.path);
        let result = (|| -> Result<bool, String> {
            let dir = listings
                .open_parent(&root_dir, parent, true)?
                .ok_or("parent missing")?;
            match listings.existing(&dir, parent, name)? {
                Existing::Absent => {
                    dir.mkdir_mode(name, op.mode | 0o700)
                        .map_err(|e| e.to_string())?;
                    listings.added(parent, name);
                    Ok(true)
                }
                Existing::Present(stat) if stat.kind == FileType::Directory => {
                    if stat.mode & 0o700 != 0o700 {
                        dir.open(name)
                            .and_then(|child| child.chmod((stat.mode & 0o777) | 0o700))
                            .map_err(|e| e.to_string())?;
                    }
                    Ok(false)
                }
                Existing::Present(_) => Err("a local file is in the way of a directory".into()),
            }
        })();
        match result {
            Ok(true) => {
                report.created += 1;
                report.listings.insert(parent.to_vec());
            }
            Ok(false) => {}
            Err(message) => return Err(fail(at(op, message), &report)),
        }
        modes.push(op);
    }
    for op in &leaves {
        let (parent, name) = split_name(&op.path);
        let result = (|| -> Result<bool, String> {
            let dir = listings
                .open_parent(&root_dir, parent, true)?
                .ok_or("parent missing")?;
            let existed = match listings.existing(&dir, parent, name)? {
                Existing::Present(stat) if stat.kind == FileType::Directory => {
                    return Err("a local directory is in the way".into());
                }
                Existing::Present(_) => true,
                Existing::Absent => false,
            };
            let temporary = temporary_name();
            data.seek(SeekFrom::Start(op.offset))
                .map_err(|e| e.to_string())?;
            let staged = (|| -> Result<(), String> {
                if op.kind == Kind::Symlink {
                    let mut target = Vec::new();
                    (&mut data)
                        .take(op.len)
                        .read_to_end(&mut target)
                        .map_err(|e| e.to_string())?;
                    dir.symlink(OsStr::from_bytes(&target), &temporary)
                        .map_err(|e| e.to_string())
                } else {
                    let mut file = dir
                        .create_file(&temporary, 0o600)
                        .map_err(|e| e.to_string())?;
                    let copied = io::copy(&mut (&mut data).take(op.len), &mut file)
                        .map_err(|e| e.to_string())?;
                    if copied != op.len {
                        return Err("truncated delta entry".into());
                    }
                    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(op.mode))
                        .map_err(|e| e.to_string())?;
                    file.sync_all().map_err(|e| e.to_string())
                }
            })();
            if let Err(message) = staged {
                let _ = dir.unlink(&temporary);
                return Err(message);
            }
            if let Err(error) = dir.rename(&temporary, &dir, name) {
                let _ = dir.unlink(&temporary);
                return Err(error.to_string());
            }
            listings.added(parent, name);
            Ok(existed)
        })();
        match result {
            Ok(true) => report.modified += 1,
            Ok(false) => {
                report.created += 1;
                report.listings.insert(parent.to_vec());
            }
            Err(message) => return Err(fail(at(op, message), &report)),
        }
    }
    for op in &deletes {
        match delete(&mut listings, op) {
            Ok(true) => {
                report.deleted += 1;
                report.listings.insert(split_name(&op.path).0.to_vec());
            }
            Ok(false) => {}
            Err(None) => report
                .kept
                .push(String::from_utf8_lossy(&op.path).into_owned()),
            Err(Some(message)) => return Err(fail(at(op, message), &report)),
        }
    }
    for op in modes.into_iter().rev() {
        let (parent, name) = split_name(&op.path);
        let result = listings
            .open_parent(&root_dir, parent, false)
            .and_then(|dir| dir.ok_or_else(|| "parent missing".to_owned()))
            .and_then(|dir| match listings.existing(&dir, parent, name)? {
                Existing::Present(stat) if stat.kind == FileType::Directory => {
                    if stat.mode & 0o777 == op.mode {
                        Ok(())
                    } else {
                        dir.open(name)
                            .and_then(|child| child.chmod(op.mode))
                            .map_err(|e| e.to_string())
                    }
                }
                _ => Ok(()),
            });
        if let Err(message) = result {
            return Err(fail(at(op, message), &report));
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    fn delta(build: impl FnOnce(&mut Writer<&mut Vec<u8>>)) -> tempfile::NamedTempFile {
        let mut bytes = Vec::new();
        let mut writer = Writer::new(&mut bytes, u64::MAX).unwrap();
        build(&mut writer);
        writer.finish().unwrap();
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), &bytes).unwrap();
        file
    }

    fn identity(path: &Path) -> (u64, u64) {
        let metadata = fs::metadata(path).unwrap();
        (metadata.dev(), metadata.ino())
    }

    #[test]
    fn returned_delta_writes_only_its_entries_and_never_follows_links() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("keep.txt"), "local edit").unwrap();
        fs::write(root.path().join("gone.txt"), "x").unwrap();
        fs::create_dir(root.path().join("busy")).unwrap();
        fs::write(root.path().join("busy/local.txt"), "x").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let archive = delta(|w| {
            w.delete(b"gone.txt").unwrap();
            w.directory(b"new", 0o755).unwrap();
            w.file(b"new/run.sh", 0o755, 2, &mut &b"hi"[..]).unwrap();
            w.symlink(b"new/up", b"../keep.txt").unwrap();
        });
        let report =
            apply_delta(root.path(), identity(root.path()), archive.path(), u64::MAX).unwrap();
        assert_eq!((report.created, report.modified, report.deleted), (3, 0, 1));
        assert_eq!(
            fs::read_to_string(root.path().join("keep.txt")).unwrap(),
            "local edit"
        );
        assert!(!root.path().join("gone.txt").exists());
        let script = root.path().join("new/run.sh");
        assert_eq!(
            fs::metadata(&script).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read_link(root.path().join("new/up")).unwrap(),
            Path::new("../keep.txt")
        );

        let archive = delta(|w| {
            w.file(b"link/escape", 0o644, 1, &mut &b"x"[..]).unwrap();
        });
        let error = apply_delta(root.path(), identity(root.path()), archive.path(), u64::MAX)
            .expect_err("symlink parent");
        assert_eq!(error.applied.total(), 0, "{}", error.message);
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);

        // A rename (`git mv busy moved`) whose old directory still holds a
        // local-only file: the new files land; the old directory is kept.
        fs::write(root.path().join("busy/tracked.txt"), "t").unwrap();
        let archive = delta(|w| {
            w.delete(b"busy/tracked.txt").unwrap();
            w.delete(b"busy").unwrap();
            w.directory(b"moved", 0o555).unwrap();
            w.file(b"moved/tracked.txt", 0o644, 1, &mut &b"t"[..])
                .unwrap();
        });
        let report =
            apply_delta(root.path(), identity(root.path()), archive.path(), u64::MAX).unwrap();
        assert_eq!(report.kept, ["busy"]);
        assert!(root.path().join("busy/local.txt").exists());
        assert!(!root.path().join("busy/tracked.txt").exists());
        assert_eq!(
            fs::read(root.path().join("moved/tracked.txt")).unwrap(),
            b"t"
        );
        // Read-only directory modes are applied after their contents.
        let mode = fs::metadata(root.path().join("moved"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o555);
        fs::set_permissions(root.path().join("moved"), fs::Permissions::from_mode(0o755)).unwrap();

        // A directory replaced by a symlink: its old contents go first.
        fs::create_dir(root.path().join("vendor")).unwrap();
        fs::write(root.path().join("vendor/a"), "a").unwrap();
        let archive = delta(|w| {
            w.delete(b"vendor/a").unwrap();
            w.delete(b"vendor").unwrap();
            w.symlink(b"vendor", b"keep.txt").unwrap();
        });
        apply_delta(root.path(), identity(root.path()), archive.path(), u64::MAX).unwrap();
        assert_eq!(
            fs::read_link(root.path().join("vendor")).unwrap(),
            Path::new("keep.txt")
        );

        let forged = delta(|w| {
            w.file(b"A.txt", 0o644, 1, &mut &b"x"[..]).unwrap();
            w.file(b"a.txt", 0o644, 1, &mut &b"y"[..]).unwrap();
        });
        let error =
            apply_delta(root.path(), identity(root.path()), forged.path(), u64::MAX).unwrap_err();
        assert!(
            error.message.contains("case-insensitive"),
            "{}",
            error.message
        );
        let other = tempfile::tempdir().unwrap();
        let error = apply_delta(
            root.path(),
            identity(other.path()),
            archive.path(),
            u64::MAX,
        )
        .unwrap_err();
        assert!(error.message.contains("replaced"));
    }
}

#[cfg(test)]
mod git_tests {
    use super::*;
    use std::fs;

    fn names(archive: &[u8]) -> Vec<String> {
        let mut reader = Reader::new(archive, u64::MAX).unwrap();
        let mut names = Vec::new();
        while let Some(header) = reader.next_header().unwrap() {
            names.push(String::from_utf8_lossy(&header.path).into_owned());
        }
        names
    }

    fn request() -> CloudRequest {
        CloudRequest {
            cwd: b"/p".to_vec(),
            root: b"/p".to_vec(),
            mode: cloud::CloudMode::Shell,
            argv: vec![b"-c".to_vec(), b"true".to_vec()],
            env: BTreeMap::new(),
            terminal: None,
            kits: Vec::new(),
            stdin_terminal: false,
            detached: false,
            delegate: None,
        }
    }

    /// The Git copy is an allowlist, its config keeps no remotes or
    /// helpers, and a fork's planted `alternates` (a branch job can write
    /// it) never makes the copy read another directory or follow a link.
    #[test]
    fn git_copy_is_an_allowlist_and_ignores_planted_alternates() {
        let project = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        fs::write(secret.path().join("id_rsa"), "private").unwrap();
        let git = project.path().join(".git");
        for dir in [
            "objects/ab",
            "refs/heads",
            "hooks",
            "git-crypt/keys",
            "info",
        ] {
            fs::create_dir_all(git.join(dir)).unwrap();
        }
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git.join("objects/ab/cd"), "obj").unwrap();
        fs::write(git.join("hooks/pre-commit"), "evil").unwrap();
        fs::write(git.join("git-crypt/keys/default"), "key").unwrap();
        fs::write(git.join("config.worktree"), "[credential]\nhelper = x\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\nrepositoryformatversion = 1\n[remote \"o\"]\nurl = https://t:SECRET@h/r\n[extensions]\nworktreeConfig = true\nobjectFormat = sha1\n",
        )
        .unwrap();
        let root = Dir::open_root(project.path()).unwrap();
        let (archive, _, outcome) = write_snapshot(
            &root,
            true,
            &GitView::Project,
            &request(),
            Vec::new(),
            u64::MAX,
        )
        .unwrap();
        assert_eq!(outcome, GitOutcome::Copied);
        let listed = names(&archive);
        assert!(
            listed.contains(&".git/HEAD".to_owned())
                && listed.contains(&".git/objects/ab/cd".to_owned())
        );
        for leaked in [
            ".git/hooks/pre-commit",
            ".git/git-crypt/keys/default",
            ".git/config.worktree",
        ] {
            assert!(!listed.contains(&leaked.to_owned()), "{leaked}: {listed:?}");
        }
        let text = String::from_utf8_lossy(&archive);
        assert!(
            !text.contains("SECRET") && !text.contains("worktreeconfig"),
            "config leaked"
        );

        // A fork whose admin alternates name a symlink to a secret directory.
        let split = project.path().join(".marsh/split/abc123def456");
        let admin = split.join(".admin/fix");
        fs::create_dir_all(admin.join("objects/info")).unwrap();
        fs::create_dir_all(split.join("fix")).unwrap();
        fs::create_dir_all(split.join("store.git/objects")).unwrap();
        fs::write(admin.join("HEAD"), "ref: refs/heads/marsh-split\n").unwrap();
        std::os::unix::fs::symlink(secret.path(), split.join("fix/x")).unwrap();
        fs::write(
            admin.join("objects/info/alternates"),
            format!(
                "{}\n{}\n",
                split.join("fix/x").display(),
                secret.path().display()
            ),
        )
        .unwrap();
        let fork = Dir::open_root(&split.join("fix")).unwrap();
        let view = GitView::Fork {
            project: project.path().to_path_buf(),
            split: "abc123def456".into(),
            label: "fix".into(),
            fork: split.join("fix"),
        };
        let (archive, _, _) =
            write_snapshot(&fork, false, &view, &request(), Vec::new(), u64::MAX).unwrap();
        assert!(
            !String::from_utf8_lossy(&archive).contains("private"),
            "followed planted alternates"
        );
    }
}
