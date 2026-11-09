//! Folder local data in checkpoints and lanes (C5, DECISIONS 26 and 27;
//! `docs/design/services.md` "Folder local data").
//!
//! The checkpoint's `tree/` leaves out what Git ignores, so a branch of it
//! would start without the user's `.env`, `.wrangler/state`, SQLite files and
//! the like. A checkpoint therefore also clones **every git-ignored path** of
//! the project into `<checkpoint>/ignored/` (same relative paths), except:
//!
//! - a built-in list of rebuildable junk (`JUNK`: `node_modules`, `.next`,
//!   `target`, `dist`, `build`, `.venv`, `__pycache__`, `.cache`, `*.log`, ...);
//! - anything the project's `.marsh/branch` excludes. That file holds
//!   `include <glob>` and `exclude <glob>` lines (gitignore-style globs);
//!   an include wins over the junk list, an exclude wins over everything.
//!
//! "Ignored" is decided the way the checkpoint's own tree snapshot decides it
//! (the user's global excludes, `.git/info/exclude`, and every `.gitignore`,
//! read as data through descriptors: marsh never runs `git` against a
//! repository whose config a job can write), so the two together are the whole
//! project. A path that is already in `tree/` (tracked) is not copied again.
//! Directories are saved as trees, file by file: every file is cloned with the
//! helpers the tree snapshot uses (a descriptor walk with `O_NOFOLLOW`,
//! `clonefileat(CLONE_NOFOLLOW)`, a stability re-check), so **a symbolic link
//! is never followed**, a FIFO or device is never opened, nothing named `.git`
//! or `.marsh` is copied, and a file that changes into a link while it is
//! cloned is removed again. The bounds (`Limits`, defaults 20000 files and
//! 2 GiB, cheap because APFS clones share data; `services_ignored_max_files`
//! and `services_ignored_max_bytes` in `config.json`) never fail a checkpoint:
//! what does not fit is listed in `skipped` with the reason, once per saved
//! path, small things (`.env`) first.
//!
//! `copy_ignored_into_lane` is the other half: a split `--from` the checkpoint
//! copies `ignored/` into each lane **before** the Docker restore runs, with
//! the same refusals again (`.marsh` is guest-writable, so `ignored/` is read
//! as untrusted) and bounded by what the checkpoint recorded.

use crate::split_fs::{self, Dir, Rules, SMALL_LIMIT, is_dotgit, logical};
use rustix::fs::FileType;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path, PathBuf};

/// Default file-count bound of one checkpoint's folder local data.
pub const DEFAULT_MAX_FILES: u64 = 20_000;
/// Default total-size bound (2 GiB).
pub const DEFAULT_MAX_BYTES: u64 = 2 << 30;
/// Skipped entries listed in a record; the rest are counted.
const MAX_SKIPPED: usize = 64;
/// Saved paths named in a record (and in the restore line's source).
const MAX_ROOTS: usize = 64;
/// Directory entries looked at while looking for ignored paths, and again
/// while saving them.
const SCAN_ENTRIES: usize = 500_000;
const WALK_ENTRIES: usize = 500_000;
const WALK_DEPTH: usize = 64;

/// Rebuildable junk, as gitignore patterns (matched on every path of an
/// ignored folder, at any depth): dependencies, build output, caches, logs.
/// A `.marsh/branch` `include` line overrides it.
const JUNK: &[&str] = &[
    "node_modules/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    ".angular/",
    ".nx/",
    "target/",
    "dist/",
    "build/",
    "out/",
    ".venv/",
    "venv/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".ruff_cache/",
    ".tox/",
    ".turbo/",
    ".cache/",
    ".parcel-cache/",
    ".gradle/",
    ".terraform/",
    "coverage/",
    ".nyc_output/",
    "*.log",
    "*.pyc",
    "*.tsbuildinfo",
    ".DS_Store",
];

/// Test-only defects (`docs/design/test-breaks.md`).
pub mod breaks {
    /// No ignored state is captured at all.
    pub const IGNORED_SKIP: &str = "ignored-skip";
    /// The TEST-PLAN name of the same defect (SVC-CAP-09 c1).
    pub const IGNORED_CAPTURE_OFF: &str = "ignored-capture-off";
    /// A symbolic link is followed (the bytes it points at are copied).
    pub const IGNORED_FOLLOW_SYMLINK: &str = "ignored-follow-symlink";
    /// The TEST-PLAN name of the same defect (SVC-CAP-10 c2).
    pub const IGNORED_FOLLOW_OUTSIDE_SYMLINK: &str = "ignored-follow-outside-symlink";
}

/// What a checkpoint may take of the project's git-ignored state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_files: u64,
    pub max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_files: DEFAULT_MAX_FILES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// One thing that was not copied, and why (plain, bounded text).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
    /// Data the user may expect is missing (over a limit, or could not be
    /// read), as opposed to what is left out on purpose (junk, `exclude`,
    /// links).
    #[serde(default, skip_serializing_if = "is_false")]
    pub lost: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's signature.
fn is_false(value: &bool) -> bool {
    !*value
}

/// What was copied and what was not (`checkpoints --json`: `ignored`).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct IgnoredRecord {
    pub files: u64,
    pub bytes: u64,
    pub skipped: Vec<Skipped>,
    /// The paths that were saved, as the restore line names them
    /// (`.env`, `.wrangler/state`): the highest ignored path of each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
}

struct Tally {
    limits: Limits,
    files: u64,
    bytes: u64,
    skipped: Vec<Skipped>,
    omitted: usize,
    /// The saved path being worked on: limit skips are said once per path.
    scope: Option<String>,
    said: BTreeSet<String>,
}

impl Tally {
    fn new(limits: Limits) -> Self {
        Self {
            limits,
            files: 0,
            bytes: 0,
            skipped: Vec::new(),
            omitted: 0,
            scope: None,
            said: BTreeSet::new(),
        }
    }

    fn push(&mut self, path: &str, reason: impl Into<String>, lost: bool) {
        if self.skipped.len() < MAX_SKIPPED {
            self.skipped.push(Skipped {
                path: crate::checkpoint::plain(path),
                reason: reason.into(),
                lost,
            });
        } else {
            self.omitted += 1;
        }
    }

    /// Something the user may expect is missing.
    fn skip(&mut self, path: &str, reason: impl Into<String>) {
        self.push(path, reason, true);
    }

    fn skip_path(&mut self, rel: &Path, reason: impl Into<String>) {
        self.skip(&rel.to_string_lossy(), reason);
    }

    /// Something left out on purpose (junk, `exclude`, a link).
    fn note_path(&mut self, rel: &Path, reason: impl Into<String>) {
        self.push(&rel.to_string_lossy(), reason, false);
    }

    /// A limit skip, said once for the saved path it belongs to.
    fn over(&mut self, rel: &Path, reason: &str) {
        let key = self
            .scope
            .clone()
            .unwrap_or_else(|| rel.to_string_lossy().into_owned());
        if self.said.insert(key.clone()) {
            self.skip(&key, reason);
        }
    }

    /// Whether one more file of `size` fits; otherwise it is skipped, said.
    fn admit(&mut self, rel: &Path, size: u64) -> bool {
        if self.files >= self.limits.max_files {
            let reason = format!(
                "over the limit of {} files for saved local data; the rest of it wasn't copied",
                self.limits.max_files
            );
            self.over(rel, &reason);
            return false;
        }
        if self.bytes.saturating_add(size) > self.limits.max_bytes {
            let reason = format!(
                "over the size limit of {} for saved local data; the rest of it wasn't copied",
                size_text(self.limits.max_bytes)
            );
            self.over(rel, &reason);
            return false;
        }
        true
    }

    /// The size check again, against what was actually cloned (a file can grow
    /// between the check and the clone): over the bound, the clone is removed
    /// by the caller and the size-limit skip is recorded here.
    fn admit_cloned(&mut self, rel: &Path, size: u64) -> bool {
        if self.bytes.saturating_add(size) > self.limits.max_bytes {
            let reason = format!(
                "grew while it was saved and is over the size limit of {} for saved local data",
                size_text(self.limits.max_bytes)
            );
            self.over(rel, &reason);
            return false;
        }
        true
    }

    fn finish(mut self) -> IgnoredRecord {
        if self.omitted > 0 {
            let reason = format!("{} more were skipped and are not listed", self.omitted);
            self.skipped.push(Skipped {
                path: String::new(),
                reason,
                lost: false,
            });
        }
        IgnoredRecord {
            files: self.files,
            bytes: self.bytes,
            skipped: self.skipped,
            roots: Vec::new(),
        }
    }
}

fn size_text(bytes: u64) -> String {
    if bytes >= 1 << 30 && bytes.is_multiple_of(1 << 30) {
        format!("{} GiB", bytes >> 30)
    } else if bytes >= 1 << 20 && bytes.is_multiple_of(1 << 20) {
        format!("{} MiB", bytes >> 20)
    } else {
        format!("{bytes} bytes")
    }
}

fn special_name(name: &OsStr) -> bool {
    is_dotgit(name) || name.as_bytes() == b".marsh"
}

// ------------------------------------------------------------ .marsh/branch

/// What `.marsh/branch` says: the globs of its `include` and `exclude` lines,
/// and the lines that were not understood.
#[derive(Debug, Default, Eq, PartialEq)]
struct BranchFile {
    include: Vec<String>,
    exclude: Vec<String>,
    problems: Vec<String>,
}

/// One `include <glob>` or `exclude <glob>` per line; blank lines and `#`
/// comments are ignored.
fn parse_branch_file(text: &str) -> BranchFile {
    let mut file = BranchFile::default();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, glob) = line
            .split_once(char::is_whitespace)
            .map_or((line, ""), |(word, rest)| (word, rest.trim()));
        match (word, glob.is_empty()) {
            ("include", false) => file.include.push(glob.to_owned()),
            ("exclude", false) => file.exclude.push(glob.to_owned()),
            _ => file.problems.push(format!(
                "line {} of .marsh/branch was not understood (use `include GLOB` or `exclude GLOB`)",
                index + 1
            )),
        }
    }
    file
}

fn matcher(lines: &[&str]) -> Option<ignore::gitignore::Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new("/");
    for line in lines {
        let _ = builder.add_line(None, line);
    }
    builder.build().ok().filter(|rules| !rules.is_empty())
}

/// Which ignored paths are copied: the junk list and `.marsh/branch`.
struct Policy {
    junk: Option<ignore::gitignore::Gitignore>,
    include: Option<ignore::gitignore::Gitignore>,
    exclude: Option<ignore::gitignore::Gitignore>,
}

impl Policy {
    fn new(file: &BranchFile) -> Self {
        fn lines(globs: &[String]) -> Vec<&str> {
            globs.iter().map(String::as_str).collect()
        }
        Self {
            junk: matcher(JUNK),
            include: matcher(&lines(&file.include)),
            exclude: matcher(&lines(&file.exclude)),
        }
    }

    fn matches(rules: Option<&ignore::gitignore::Gitignore>, rel: &Path, dir: bool) -> bool {
        rules.is_some_and(|rules| {
            rules
                .matched_path_or_any_parents(logical(rel), dir)
                .is_ignore()
        })
    }

    fn excluded(&self, rel: &Path, dir: bool) -> bool {
        Self::matches(self.exclude.as_ref(), rel, dir)
    }

    fn included(&self, rel: &Path, dir: bool) -> bool {
        Self::matches(self.include.as_ref(), rel, dir)
    }

    /// On the junk list by this entry's own name (its parents are judged as
    /// the walk passes them).
    fn junk(&self, rel: &Path, dir: bool) -> bool {
        self.junk
            .as_ref()
            .is_some_and(|rules| rules.matched(logical(rel), dir).is_ignore())
    }
}

fn read_branch_file(root: &Path) -> BranchFile {
    let bytes = Dir::open_root(root)
        .and_then(|dir| dir.open(".marsh"))
        .and_then(|dir| dir.read("branch", SMALL_LIMIT));
    match bytes {
        Ok(bytes) => parse_branch_file(&String::from_utf8_lossy(&bytes)),
        Err(_) => BranchFile::default(),
    }
}

struct Capture<'a> {
    root: &'a Path,
    source: &'a Dir,
    tree: &'a Dir,
    out: &'a Dir,
    tally: Tally,
    policy: Policy,
    has_includes: bool,
    copied: BTreeSet<PathBuf>,
    roots: Vec<String>,
    walked: usize,
    walk_said: bool,
    junk_files: u64,
}

/// Clone the project's git-ignored files into `out` (`<checkpoint>/ignored`).
/// `source` is the live project (or lane) directory, `tree` the checkpoint's
/// own `tree/` (what is already saved is not copied again), `root` the
/// project's path (where `.marsh/branch` is read, and used by the
/// follow-symlink defect), `exclude` the repository's `info/exclude` (`None`:
/// not a Git project, so nothing is ignored). Never fails: what was not copied
/// is in the record.
#[must_use]
pub fn capture(
    root: &Path,
    source: &Dir,
    tree: &Dir,
    out: &Dir,
    limits: Limits,
    exclude: Option<&[u8]>,
) -> IgnoredRecord {
    if marsh_contracts::break_mode(breaks::IGNORED_SKIP)
        || marsh_contracts::break_mode(breaks::IGNORED_CAPTURE_OFF)
    {
        return IgnoredRecord::default();
    }
    let Some(exclude) = exclude else {
        return IgnoredRecord::default();
    };
    let branch = read_branch_file(root);
    let mut run = Capture {
        root,
        source,
        tree,
        out,
        tally: Tally::new(limits),
        policy: Policy::new(&branch),
        has_includes: !branch.include.is_empty(),
        copied: BTreeSet::new(),
        roots: Vec::new(),
        walked: 0,
        walk_said: false,
        junk_files: 0,
    };
    for problem in &branch.problems {
        run.tally.note_path(Path::new(""), problem.clone());
    }
    let candidates = run.discover(exclude);
    for rel in &candidates {
        run.take(rel);
    }
    if run.junk_files > 0 {
        let reason = format!(
            "{} log and cache files (such as *.log) weren't copied: tools recreate them",
            run.junk_files
        );
        run.tally.note_path(Path::new(""), reason);
    }
    let roots = std::mem::take(&mut run.roots);
    let mut record = run.tally.finish();
    record.roots = roots;
    record
}

impl Capture<'_> {
    /// The highest ignored path of each place in the project, small things
    /// first (files, then folders; each sorted: the order of the result, and
    /// of any gap, is the same every time).
    fn discover(&mut self, exclude: &[u8]) -> Vec<PathBuf> {
        let mut rules = Rules(Vec::new());
        if let Some(global) =
            Some(ignore::gitignore::Gitignore::global().0).filter(|g| !g.is_empty())
        {
            rules.0.push(global);
        }
        if let Some(exclude) = Rules::parse(Path::new("/"), exclude) {
            rules.0.push(exclude);
        }
        let (mut files, mut dirs) = (BTreeSet::new(), BTreeSet::new());
        let mut seen = 0_usize;
        match self.source.dup() {
            Ok(top) => self.scan(
                &top,
                Path::new(""),
                &mut rules,
                0,
                &mut seen,
                (&mut files, &mut dirs),
            ),
            Err(_) => self
                .tally
                .skip("", "the project folder could not be read for local data"),
        }
        files.into_iter().chain(dirs).collect()
    }

    fn scan(
        &mut self,
        dir: &Dir,
        rel: &Path,
        rules: &mut Rules,
        depth: usize,
        seen: &mut usize,
        found: (&mut BTreeSet<PathBuf>, &mut BTreeSet<PathBuf>),
    ) {
        let Ok(entries) = dir.entries() else { return };
        // A nested repository is its own project.
        if !rel.as_os_str().is_empty() && entries.iter().any(|(name, _)| is_dotgit(name)) {
            return;
        }
        let (files, dirs) = found;
        let pushed = dir
            .read(".gitignore", SMALL_LIMIT)
            .ok()
            .and_then(|bytes| Rules::parse(&logical(rel), &bytes))
            .map(|level| rules.0.push(level))
            .is_some();
        for (name, kind) in entries {
            if special_name(&name) {
                continue;
            }
            *seen += 1;
            if *seen > SCAN_ENTRIES {
                self.tally.skip(
                    "",
                    "stopped looking for local data: too many files in the project",
                );
                break;
            }
            let path = rel.join(&name);
            let is_dir = kind == FileType::Directory;
            if rules.ignored(&logical(&path), is_dir) {
                if is_dir {
                    dirs.insert(path);
                } else {
                    files.insert(path);
                }
            } else if is_dir {
                if depth >= WALK_DEPTH {
                    continue;
                }
                if let Ok(child) = dir.open(&name) {
                    self.scan(&child, &path, rules, depth + 1, seen, (files, dirs));
                }
            }
        }
        if pushed {
            rules.0.pop();
        }
    }

    fn in_tree(&self, rel: &Path) -> bool {
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
            return false;
        };
        self.tree
            .open_path(parent)
            .and_then(|dir| dir.stat(name))
            .is_ok()
    }

    /// The directory of `out` for `rel`'s parent, created one real directory at
    /// a time (a link is refused).
    fn out_parent(&self, rel: &Path) -> io::Result<Dir> {
        let mut dir = self.out.dup()?;
        for component in rel.parent().unwrap_or(Path::new("")).components() {
            if let Component::Normal(name) = component {
                dir = dir.ensure(name)?;
            }
        }
        Ok(dir)
    }

    /// One ignored path found by the scan.
    fn take(&mut self, rel: &Path) {
        let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) else {
            return;
        };
        let follow = marsh_contracts::break_mode(breaks::IGNORED_FOLLOW_SYMLINK)
            || marsh_contracts::break_mode(breaks::IGNORED_FOLLOW_OUTSIDE_SYMLINK);
        let parent_dir = match self.source.open_path(parent) {
            Ok(dir) => dir,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(_) if follow => return self.follow(rel),
            Err(_) => {
                self.tally.skip_path(
                    rel,
                    "a link or a file is in the way on its path; links are never followed",
                );
                return;
            }
        };
        let stat = match parent_dir.stat(name) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.tally
                    .skip_path(rel, format!("could not be read: {error}"));
                return;
            }
        };
        if stat.kind == FileType::Symlink && follow {
            return self.follow(rel);
        }
        self.tally.scope = Some(rel.to_string_lossy().into_owned());
        let before = self.tally.files;
        self.visit(&parent_dir, rel, stat.kind, stat.size, false, 0);
        self.tally.scope = None;
        if self.tally.files > before && self.roots.len() < MAX_ROOTS {
            self.roots
                .push(crate::checkpoint::plain(&rel.to_string_lossy()));
        }
    }

    /// One entry of an ignored path: left out (`exclude`, junk), saved, or
    /// walked into. `pending`: inside a junk folder, where only what an
    /// `include` names is taken.
    fn visit(
        &mut self,
        parent: &Dir,
        rel: &Path,
        kind: FileType,
        size: u64,
        pending: bool,
        depth: usize,
    ) {
        let Some(name) = rel.file_name() else { return };
        if special_name(name) {
            return;
        }
        let is_dir = kind == FileType::Directory;
        if self.policy.excluded(rel, is_dir) {
            if !pending {
                self.tally
                    .note_path(rel, "is left out by an `exclude` line in .marsh/branch");
            }
            return;
        }
        let included = self.policy.included(rel, is_dir);
        let mut pending = pending;
        if !pending && !included && self.policy.junk(rel, is_dir) {
            if !is_dir {
                self.junk_files += 1;
            } else if self.has_includes {
                let before = self.tally.files;
                self.descend(parent, rel, true, depth);
                if self.tally.files == before {
                    self.junk_skip(rel);
                }
            } else {
                self.junk_skip(rel);
            }
            return;
        }
        if pending && included {
            pending = false;
        }
        match kind {
            FileType::Directory => self.descend(parent, rel, pending, depth),
            FileType::RegularFile if pending && !included => {}
            FileType::RegularFile => self.file(parent, rel, size),
            _ if pending => {}
            FileType::Symlink => self.tally.note_path(
                rel,
                "is a symbolic link; links are never followed or copied",
            ),
            _ => self.tally.note_path(
                rel,
                "is not a regular file (a pipe or device is never opened)",
            ),
        }
    }

    fn junk_skip(&mut self, rel: &Path) {
        let reason = format!(
            "wasn't copied: tools rebuild it (dependencies, build output, caches). To copy it, add `include {}` to .marsh/branch",
            rel.display()
        );
        self.tally.note_path(rel, reason);
    }

    fn descend(&mut self, parent: &Dir, rel: &Path, pending: bool, depth: usize) {
        let Some(name) = rel.file_name() else { return };
        if depth >= WALK_DEPTH {
            self.tally.skip_path(rel, "is nested too deep");
            return;
        }
        let Ok(dir) = parent.open(name) else {
            self.tally
                .skip_path(rel, "a link is in the way; links are never followed");
            return;
        };
        let Ok(entries) = dir.entries() else { return };
        // A nested repository is its own project.
        if !pending && entries.iter().any(|(entry, _)| is_dotgit(entry)) {
            self.tally
                .note_path(rel, "is a repository of its own, which is not copied");
            return;
        }
        if !pending {
            // A saved folder keeps its empty subfolders too.
            let _ = self.out_parent(&rel.join("x"));
        }
        for (entry, kind) in entries {
            self.walked += 1;
            if self.walked > WALK_ENTRIES {
                if !self.walk_said {
                    self.walk_said = true;
                    self.tally
                        .skip_path(rel, "stopped looking: too many files in this folder");
                }
                return;
            }
            let child = rel.join(&entry);
            let size = match kind {
                FileType::RegularFile => match dir.stat(&entry) {
                    Ok(stat) => stat.size,
                    Err(error) => {
                        self.tally
                            .skip_path(&child, format!("could not be read: {error}"));
                        continue;
                    }
                },
                _ => 0,
            };
            self.visit(&dir, &child, kind, size, pending, depth + 1);
        }
    }

    fn file(&mut self, parent: &Dir, rel: &Path, size: u64) {
        let Some(name) = rel.file_name() else { return };
        if self.copied.contains(rel) || self.in_tree(rel) {
            return;
        }
        if !self.tally.admit(rel, size) {
            return;
        }
        let out = match self.out_parent(rel) {
            Ok(out) => out,
            Err(error) => {
                self.tally
                    .skip_path(rel, format!("could not be saved: {error}"));
                return;
            }
        };
        match clone_regular(parent, &out, name) {
            Ok(saved) if !self.tally.admit_cloned(rel, saved) => {
                let _ = out.remove_tree(name);
            }
            Ok(saved) => {
                self.tally.files += 1;
                self.tally.bytes += saved;
                self.copied.insert(rel.to_path_buf());
            }
            Err(reason) => self.tally.skip_path(rel, reason),
        }
    }

    /// Defect (`ignored-follow-symlink`): copy what a link points at, by path.
    fn follow(&mut self, rel: &Path) {
        let path = self.root.join(rel);
        self.follow_copy(&path, rel);
    }

    fn follow_copy(&mut self, path: &Path, rel: &Path) {
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let (Ok(parent), Some(name)) = (self.out_parent(rel), rel.file_name()) else {
            return;
        };
        if meta.is_dir() {
            if parent.ensure(name).is_err() {
                return;
            }
            for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
                self.follow_copy(&entry.path(), &rel.join(entry.file_name()));
            }
        } else if meta.is_file()
            && let Ok(bytes) = std::fs::read(path)
            && parent.write_new(name, &bytes).is_ok()
        {
            self.tally.files += 1;
            self.tally.bytes += bytes.len() as u64;
        }
    }
}

/// Clone one regular file with the tree snapshot's helper and check what
/// arrived: an entry that became a link, a pipe or a directory while it was
/// cloned is removed again (as a tree: a cloned directory is a whole tree).
/// `Ok(bytes)` of the clone.
fn clone_regular(from: &Dir, to: &Dir, name: &OsStr) -> Result<u64, String> {
    let shown = Path::new(name);
    if let Err(error) = split_fs::clone_stable(from, to, name, shown) {
        let _ = to.remove_tree(name);
        return Err(format!("could not be saved: {error}"));
    }
    match to.stat(name) {
        Ok(stat) if stat.kind == FileType::RegularFile => Ok(stat.size),
        Ok(_) => {
            let _ = to.remove_tree(name);
            Err("changed into a link or another kind of file while it was saved".into())
        }
        Err(_) => Err("changed while it was saved".into()),
    }
}

/// Copy `<checkpoint>/ignored/` into a lane's folder, before the Docker
/// restore (C3 calls this; the split path calls it for `--from`). `expected`
/// is the checkpoint's own record: nothing beyond the files and bytes it
/// recorded is copied, since `.marsh` is guest-writable. A file the lane
/// already has is left alone; links, pipes and anything named `.git` or
/// `.marsh` are never copied. Never fails: what was not copied is said, and
/// what the checkpoint itself left out comes along in `skipped`.
#[must_use]
pub fn copy_ignored_into_lane(
    checkpoint: &Dir,
    lane: &Dir,
    expected: &IgnoredRecord,
) -> IgnoredRecord {
    let mut tally = Tally::new(Limits {
        max_files: expected.files,
        max_bytes: expected.bytes,
    });
    match checkpoint.open("ignored") {
        Ok(saved) => copy_into(&saved, lane, Path::new(""), 0, &mut tally),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => tally.skip(
            "",
            "the saved local data could not be opened; none was copied",
        ),
    }
    let mut record = tally.finish();
    let mut skipped = expected.skipped.clone();
    skipped.append(&mut record.skipped);
    record.skipped = skipped;
    if record.files > 0 {
        record.roots = expected
            .roots
            .iter()
            .filter(|root| {
                let root = Path::new(root);
                root.is_relative()
                    && root.components().all(|c| matches!(c, Component::Normal(_)))
                    && root
                        .parent()
                        .and_then(|parent| lane.open_path(parent).ok())
                        .zip(root.file_name())
                        .is_some_and(|(parent, name)| parent.stat(name).is_ok())
            })
            .take(MAX_ROOTS)
            .cloned()
            .collect();
    }
    record
}

fn copy_into(from: &Dir, to: &Dir, rel: &Path, depth: usize, tally: &mut Tally) {
    let Ok(entries) = from.entries() else { return };
    for (name, kind) in entries {
        let child = rel.join(&name);
        if depth == 0 {
            tally.scope = Some(child.to_string_lossy().into_owned());
        }
        if special_name(&name) {
            tally.note_path(
                &child,
                "is marsh's or Git's own state, which is never copied",
            );
            continue;
        }
        match kind {
            FileType::Directory if depth < WALK_DEPTH => {
                let (Ok(source), Ok(target)) = (from.open(&name), to.ensure(&name)) else {
                    tally.skip_path(&child, "a link is in the way in the branch; not copied");
                    continue;
                };
                copy_into(&source, &target, &child, depth + 1, tally);
            }
            FileType::RegularFile => {
                let size = match from.stat(&name) {
                    Ok(stat) => stat.size,
                    Err(error) => {
                        tally.skip_path(&child, format!("could not be read: {error}"));
                        continue;
                    }
                };
                if !tally.admit(&child, size) {
                    continue;
                }
                #[cfg(test)]
                split_fs::before_clone(&name);
                match from.clone_to(&name, to, &name) {
                    Ok(()) => match to.stat(&name) {
                        Ok(stat) if stat.kind == FileType::RegularFile => {
                            if tally.admit_cloned(&child, stat.size) {
                                tally.files += 1;
                                tally.bytes += stat.size;
                            } else {
                                let _ = to.remove_tree(&name);
                            }
                        }
                        _ => {
                            let _ = to.remove_tree(&name);
                            tally.skip_path(&child, "was not a regular file; not copied");
                        }
                    },
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        tally.note_path(&child, "is already in the branch; left as it is");
                    }
                    Err(error) => tally.skip_path(&child, format!("could not be copied: {error}")),
                }
            }
            _ => tally.note_path(
                &child,
                "is a link, a pipe or too deep; links are never copied",
            ),
        }
    }
    if depth == 0 {
        tally.scope = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `.marsh/branch` may say (table): include and exclude lines, with
    /// comments and blanks skipped; anything else is said.
    #[test]
    fn the_branch_file_has_include_and_exclude_lines() {
        let file = parse_branch_file(
            "# local data\n\ninclude node_modules/my-pkg\n  exclude   *.sqlite  \nexclude\nkeep x\ninclude data/big dir\n",
        );
        assert_eq!(file.include, ["node_modules/my-pkg", "data/big dir"]);
        assert_eq!(file.exclude, ["*.sqlite"]);
        assert_eq!(file.problems.len(), 2, "{:?}", file.problems);
        assert!(file.problems[0].contains("line 5"));
        assert!(file.problems[1].contains("line 6"));
    }

    /// The junk list, `include` and `exclude` on single entries (table:
    /// path, is a folder, junk, included, excluded).
    #[test]
    fn junk_include_and_exclude_judge_a_path() {
        let file = parse_branch_file(
            "include node_modules/mine\ninclude *.keep.log\nexclude secrets/\nexclude *.sqlite\n",
        );
        let policy = Policy::new(&file);
        for (path, dir, junk, included, excluded) in [
            ("node_modules", true, true, false, false),
            ("web/node_modules", true, true, false, false),
            ("target", true, true, false, false),
            (".venv", true, true, false, false),
            ("a/__pycache__", true, true, false, false),
            ("app.log", false, true, false, false),
            ("x.pyc", false, true, false, false),
            ("build", true, true, false, false),
            (".wrangler", true, false, false, false),
            (".env", false, false, false, false),
            ("db.sqlite", false, false, false, true),
            ("secrets", true, false, false, true),
            ("secrets/key", false, false, false, true),
            ("node_modules/mine", true, false, true, false),
            ("node_modules/mine/x.js", false, false, true, false),
            ("run.keep.log", false, true, true, false),
            // a file named like a junk folder is not junk
            ("build", false, false, false, false),
        ] {
            assert_eq!(policy.junk(Path::new(path), dir), junk, "junk {path}");
            assert_eq!(
                policy.included(Path::new(path), dir),
                included,
                "included {path}"
            );
            assert_eq!(
                policy.excluded(Path::new(path), dir),
                excluded,
                "excluded {path}"
            );
        }
    }

    /// The bounds, at the edges (cap-1, cap, cap+1), and the order of what is
    /// skipped.
    #[test]
    fn the_bounds_admit_up_to_the_cap_and_say_what_is_over() {
        for (files, bytes, expect_ok) in [(9, 1, true), (10, 1, false)] {
            let mut tally = Tally::new(Limits {
                max_files: 10,
                max_bytes: 100,
            });
            tally.files = files;
            assert_eq!(tally.admit(Path::new("a"), bytes), expect_ok);
        }
        for (have, size, expect_ok) in [
            (99, 1, true),
            (100, 0, true),
            (100, 1, false),
            (0, 101, false),
        ] {
            let mut tally = Tally::new(Limits {
                max_files: 10,
                max_bytes: 100,
            });
            tally.bytes = have;
            assert_eq!(
                tally.admit(Path::new("f"), size),
                expect_ok,
                "{have}+{size}"
            );
        }
        let mut tally = Tally::new(Limits {
            max_files: 0,
            max_bytes: 0,
        });
        for index in 0..(MAX_SKIPPED + 3) {
            assert!(!tally.admit(Path::new(&format!("f{index}")), 1));
        }
        let record = tally.finish();
        assert_eq!(record.skipped.len(), MAX_SKIPPED + 1);
        assert_eq!(record.skipped[0].path, "f0");
        assert!(record.skipped.last().unwrap().reason.starts_with("3 more"));
    }

    fn dirs(temp: &Path) -> (PathBuf, Dir, Dir, Dir) {
        let project = temp.join("project");
        std::fs::create_dir_all(&project).unwrap();
        for name in ["tree", "out"] {
            std::fs::create_dir_all(temp.join(name)).unwrap();
        }
        (
            project.clone(),
            Dir::open_root(&project).unwrap(),
            Dir::open_root(&temp.join("tree")).unwrap(),
            Dir::open_root(&temp.join("out")).unwrap(),
        )
    }

    /// The capture on a real folder: every git-ignored path is taken unless it
    /// is junk or excluded (an include beats junk), what the tree already has
    /// is not copied again, links and pipes are refused (and never read), the
    /// caps say what they left out, and the lane copy refuses what the record
    /// does not account for.
    #[test]
    #[allow(clippy::too_many_lines)] // One real folder, every refusal.
    fn capture_takes_every_ignored_path_except_junk_and_refuses_links() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let temp = std::fs::canonicalize(temp.path()).unwrap();
        let (project, source, tree, out) = dirs(&temp);
        let outside = temp.join("outside");
        std::fs::write(&outside, "SECRET-OUTSIDE").unwrap();
        let write = |path: &str, text: &str| {
            let path = project.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            ".gitignore",
            ".env*\n.wrangler/\n*.sqlite\nnode_modules/\ntarget/\n*.log\ndata/\nnested-link\ntracked/.env\n",
        );
        write("src/main.rs", "fn main() {}");
        write(".env", "FOO=1");
        write(".env.local", "BAR=1");
        write("web/.env", "W=1");
        write(".wrangler/state/v3/d1/db.sqlite", "d1");
        write(".wrangler/tmp/x", "tmp");
        write("local.sqlite", "rows");
        write("data/app.db", "app");
        write("data/node_modules/skip", "x");
        write("data/.git/config", "never");
        write("target/debug/big", "junk");
        write("node_modules/mine/index.js", "mine");
        write("node_modules/other/index.js", "other");
        write("debug.log", "log");
        write("tracked/.env", "IN-TREE");
        write("other-repo/.git/HEAD", "ref");
        write("other-repo/data.sqlite", "nested repo");
        write(
            ".marsh/branch",
            "include node_modules/mine\nexclude .wrangler/tmp\nexclude data/\nbad line here\n",
        );
        symlink(&outside, project.join(".env.evil")).unwrap();
        symlink(temp.join("outside"), project.join("nested-link")).unwrap();
        symlink(&outside, project.join(".wrangler/inner-link")).unwrap();
        // Already in the checkpoint's tree: not copied again.
        std::fs::create_dir_all(temp.join("tree/tracked")).unwrap();
        std::fs::write(temp.join("tree/tracked/.env"), "IN-TREE").unwrap();

        let record = capture(&project, &source, &tree, &out, Limits::default(), Some(b""));
        let saved = |path: &str| std::fs::read_to_string(temp.join("out").join(path)).ok();
        for (path, text) in [
            (".env", "FOO=1"),
            (".env.local", "BAR=1"),
            ("web/.env", "W=1"),
            (".wrangler/state/v3/d1/db.sqlite", "d1"),
            ("local.sqlite", "rows"),
            ("node_modules/mine/index.js", "mine"),
        ] {
            assert_eq!(saved(path).as_deref(), Some(text), "{path}");
        }
        for never in [
            ".env.evil",
            "nested-link",
            ".wrangler/inner-link",
            ".wrangler/tmp",
            "data",
            "target",
            "node_modules/other",
            "debug.log",
            "tracked",
            "src",
            "other-repo",
            ".marsh",
            ".gitignore",
        ] {
            assert!(!temp.join("out").join(never).exists(), "{never}");
        }
        assert_eq!(record.files, 6, "{record:?}");
        assert_eq!(
            record.roots,
            [
                ".env",
                ".env.local",
                "local.sqlite",
                "web/.env",
                ".wrangler",
                "node_modules"
            ],
            "{record:?}"
        );
        let said = |path: &str| record.skipped.iter().find(|s| s.path == path);
        for path in [
            ".env.evil",
            "nested-link",
            ".wrangler/inner-link",
            ".wrangler/tmp",
            "data",
            "target",
        ] {
            let skipped = said(path).unwrap_or_else(|| panic!("{path} is said: {record:?}"));
            assert!(!skipped.lost, "{path} is on purpose: {skipped:?}");
        }
        assert!(said("target").unwrap().reason.contains("include target"));
        assert!(
            record
                .skipped
                .iter()
                .any(|s| s.reason.contains("1 log and cache files")),
        );
        assert!(
            record
                .skipped
                .iter()
                .any(|s| s.reason.contains("line 4 of .marsh/branch")),
        );
        let grep = std::process::Command::new("grep")
            .args(["-r", "SECRET", temp.join("out").to_str().unwrap()])
            .output()
            .unwrap();
        assert_eq!(grep.stdout, Vec::<u8>::new());

        // Not a Git project: nothing is ignored.
        let (_, source0, tree0, out0) = {
            let none = temp.join("none");
            std::fs::create_dir_all(&none).unwrap();
            dirs(&none)
        };
        std::fs::write(temp.join("none/project/.env"), "x").unwrap();
        let nothing = capture(
            &temp.join("none/project"),
            &source0,
            &tree0,
            &out0,
            Limits::default(),
            None,
        );
        assert_eq!(nothing, IgnoredRecord::default());

        // The size cap: one byte less than the total keeps the last file out, said.
        let (_, source2, tree2, out2) = {
            let again = temp.join("again");
            std::fs::create_dir_all(&again).unwrap();
            dirs(&again)
        };
        for (name, text) in [
            (".gitignore", ".env*\n"),
            (".env", "12345"),
            (".env.a", "12345"),
        ] {
            std::fs::write(temp.join("again/project").join(name), text).unwrap();
        }
        let capped = capture(
            &temp.join("again/project"),
            &source2,
            &tree2,
            &out2,
            Limits {
                max_files: 10,
                max_bytes: 9,
            },
            Some(b""),
        );
        assert_eq!((capped.files, capped.bytes), (1, 5));
        assert_eq!(capped.skipped.len(), 1);
        assert_eq!(capped.skipped[0].path, ".env.a");
        assert!(capped.skipped[0].lost);

        // The lane copy: bounded by the record, links and `.git` refused; the
        // checkpoint's own skips come along.
        std::os::unix::fs::symlink(&outside, temp.join("out/planted")).unwrap();
        std::fs::create_dir_all(temp.join("out/.git")).unwrap();
        std::fs::write(temp.join("out/.git/hook"), "x").unwrap();
        std::fs::write(temp.join("out/extra"), "not recorded").unwrap();
        let checkpoint = Dir::open_root(&temp).unwrap();
        let lane_path = temp.join("lane");
        std::fs::create_dir(&lane_path).unwrap();
        let lane = Dir::open_root(&lane_path).unwrap();
        // `out` stands in for `<checkpoint>/ignored`.
        std::fs::rename(temp.join("out"), temp.join("ignored")).unwrap();
        let copied = copy_ignored_into_lane(&checkpoint, &lane, &record);
        assert_eq!(copied.files, 6);
        assert_eq!(copied.roots, record.roots);
        assert_eq!(
            std::fs::read_to_string(lane_path.join("web/.env")).unwrap(),
            "W=1"
        );
        assert_eq!(
            std::fs::read_to_string(lane_path.join(".wrangler/state/v3/d1/db.sqlite")).unwrap(),
            "d1"
        );
        for never in ["planted", ".git", "extra"] {
            assert!(!lane_path.join(never).exists(), "{never}");
        }
        assert!(copied.skipped.iter().any(|s| s.path == "target"));
        assert!(copied.skipped.iter().any(|s| s.path == "planted"));
        assert!(
            copied
                .skipped
                .iter()
                .any(|s| s.path == "extra" && s.reason.contains("limit"))
        );
    }

    /// Sets the between-check-and-clone hook for this thread; cleared on drop.
    struct Hook;
    impl Hook {
        fn set(f: impl Fn(&OsStr) + 'static) -> Self {
            split_fs::BEFORE_CLONE.with(|hook| *hook.borrow_mut() = Some(Box::new(f)));
            Self
        }
    }
    impl Drop for Hook {
        fn drop(&mut self) {
            split_fs::BEFORE_CLONE.with(|hook| *hook.borrow_mut() = None);
        }
    }

    /// `name` becomes a directory holding a file and a link to `outside`.
    fn swap_for_a_directory(at: &Path, outside: &Path) {
        std::fs::remove_file(at).unwrap();
        std::fs::create_dir(at).unwrap();
        std::fs::write(at.join("inner"), "SECRET-DIR").unwrap();
        std::os::unix::fs::symlink(outside, at.join("link")).unwrap();
    }

    fn once(at: PathBuf, name: &'static str, change: impl Fn(&Path) + 'static) -> Hook {
        let fired = std::cell::Cell::new(false);
        Hook::set(move |entry| {
            if entry == name && !fired.replace(true) {
                change(&at);
            }
        })
    }

    fn grow(at: &Path) {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().append(true).open(at).unwrap();
        file.write_all(&[b'x'; 20]).unwrap();
    }

    /// A file replaced by a directory between the check and `clonefileat`
    /// (which clones a whole tree, links included) is removed as a tree, in
    /// the capture and in the lane copy; nothing of it is left or counted.
    #[test]
    fn an_entry_that_becomes_a_directory_while_it_is_cloned_is_removed_whole() {
        let temp = tempfile::tempdir().unwrap();
        let temp = std::fs::canonicalize(temp.path()).unwrap();
        let (project, source, tree, out) = dirs(&temp);
        let outside = temp.join("outside");
        std::fs::write(&outside, "SECRET-OUTSIDE").unwrap();
        std::fs::write(project.join(".env"), "A=1").unwrap();
        std::fs::write(project.join(".gitignore"), ".env\n").unwrap();
        let hook = once(project.join(".env"), ".env", {
            let outside = outside.clone();
            move |at| swap_for_a_directory(at, &outside)
        });
        let record = capture(&project, &source, &tree, &out, Limits::default(), Some(b""));
        drop(hook);
        assert!(
            std::fs::symlink_metadata(temp.join("out/.env")).is_err(),
            "the cloned directory was left in ignored/"
        );
        assert_eq!(record.files, 0);
        assert!(
            record.skipped.iter().any(|s| s.path == ".env"),
            "{record:?}"
        );

        // The lane copy: `<checkpoint>/ignored/x.env` is swapped the same way.
        std::fs::create_dir_all(temp.join("ck/ignored")).unwrap();
        std::fs::write(temp.join("ck/ignored/x.env"), "123").unwrap();
        std::fs::create_dir(temp.join("lane")).unwrap();
        let (checkpoint, lane) = (
            Dir::open_root(&temp.join("ck")).unwrap(),
            Dir::open_root(&temp.join("lane")).unwrap(),
        );
        let expected = IgnoredRecord {
            files: 1,
            bytes: 3,
            ..IgnoredRecord::default()
        };
        let hook = once(temp.join("ck/ignored/x.env"), "x.env", {
            let outside = outside.clone();
            move |at| swap_for_a_directory(at, &outside)
        });
        let copied = copy_ignored_into_lane(&checkpoint, &lane, &expected);
        drop(hook);
        assert!(
            std::fs::symlink_metadata(temp.join("lane/x.env")).is_err(),
            "the cloned directory was left in the lane"
        );
        assert_eq!(copied.files, 0);
        assert!(
            copied.skipped.iter().any(|s| s.path == "x.env"),
            "{copied:?}"
        );
    }

    /// A file that grows between the check and the clone is held to the bound
    /// by what was cloned: the clone is removed and the size-limit skip said,
    /// in the capture and in the lane copy.
    #[test]
    fn a_file_that_grows_while_it_is_cloned_is_held_to_the_size_bound() {
        let temp = tempfile::tempdir().unwrap();
        let temp = std::fs::canonicalize(temp.path()).unwrap();
        let (project, source, tree, out) = dirs(&temp);
        std::fs::write(project.join(".env"), "12345").unwrap();
        std::fs::write(project.join(".gitignore"), ".env\n").unwrap();
        let hook = once(project.join(".env"), ".env", grow);
        let record = capture(
            &project,
            &source,
            &tree,
            &out,
            Limits {
                max_files: 10,
                max_bytes: 10,
            },
            Some(b""),
        );
        drop(hook);
        assert!(std::fs::symlink_metadata(temp.join("out/.env")).is_err());
        assert_eq!((record.files, record.bytes), (0, 0));
        assert!(
            record
                .skipped
                .iter()
                .any(|s| s.path == ".env" && s.reason.contains("size limit")),
            "{record:?}"
        );

        std::fs::create_dir_all(temp.join("ck/ignored")).unwrap();
        std::fs::write(temp.join("ck/ignored/y.env"), "12345").unwrap();
        std::fs::create_dir(temp.join("lane")).unwrap();
        let (checkpoint, lane) = (
            Dir::open_root(&temp.join("ck")).unwrap(),
            Dir::open_root(&temp.join("lane")).unwrap(),
        );
        let expected = IgnoredRecord {
            files: 1,
            bytes: 5,
            ..IgnoredRecord::default()
        };
        let hook = once(temp.join("ck/ignored/y.env"), "y.env", grow);
        let copied = copy_ignored_into_lane(&checkpoint, &lane, &expected);
        drop(hook);
        assert!(std::fs::symlink_metadata(temp.join("lane/y.env")).is_err());
        assert_eq!((copied.files, copied.bytes), (0, 0));
        assert!(
            copied
                .skipped
                .iter()
                .any(|s| s.path == "y.env" && s.reason.contains("size limit")),
            "{copied:?}"
        );
    }
}
