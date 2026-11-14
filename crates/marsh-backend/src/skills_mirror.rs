//! The skills mirror (`marsh config skills-mirror`, `docs/kits.md`): stock
//! SBX's shared skills store is the one source of skills for every job, and
//! the daemon keeps it in step with the folders `sbx skills import` scans
//! (`~/.agents/skills`, `~/.claude/skills`, ...).
//!
//! Once the folders change and settle, the daemon imports the skills whose
//! store copy differs, and removes the ones it imported whose source is gone.
//! It only ever replaces or removes a store entry it put there itself: one
//! whose content is what it last imported (its record in the control home).
//! An entry with the folder's content that it did not import is left alone
//! and never recorded; one with other content (an `sbx skills add`) is a
//! clash: the store's copy wins and `marsh status` names it. `sbx skills ls`
//! does not say where a skill came from, so the content is the evidence.
//!
//! A skill a registered Kit bundles (`agent-skill@1`) is never imported:
//! stock SBX refuses to create a VM whose bundled skill shares a name with
//! a store entry, and the Kit's copy wins. One the daemon imported before
//! (marsh 0.3.10) is removed again. Until every registered Kit's
//! descriptor has been read (or `BUNDLED_WAIT` passed) nothing is imported.
//!
//! `sbx skills import` takes no names, so the import runs with a HOME the
//! daemon prepares: `.agents/skills` holds a copy of just the skills to
//! import, read without following links out of each skill's folder, and
//! the store's path leads to the real store.

use marsh_daemon::{SkillChange, SkillsMirrorStatus};
use marsh_sbx::StockSbx;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use marsh_daemon::SKILLS_MIRROR_CONFIG_KEY as CONFIG_KEY;
/// Where marsh 0.3 kept the record, per scope: read once, then moved aside.
const LEGACY_RECORD_FILE: &str = "skills-mirror.json";
/// Beside the scopes' control homes: what the mirror of every scope shares,
/// since the store belongs to the Mac account, not to a scope.
const ACCOUNT_DIR: &str = "skills-mirror";
/// What `sync` fails with while another scope's mirror holds the lock: not
/// an error to report, just a pass to try again shortly.
const BUSY: &str = "another marsh scope is updating the skills store";
const STAGING_DIR: &str = "skills-mirror-home";
/// Each change to the store, with the copy it replaced and a diff, in the
/// account's mirror folder.
const HISTORY_DIR: &str = "history";
/// How many changes of one skill the history keeps.
const HISTORY_KEEP: usize = 20;
/// Files bigger than this are named in a history diff, not diffed.
const MAX_DIFF_FILE: u64 = 1024 * 1024;
/// How long one history diff may take.
const DIFF_TIME: Duration = Duration::from_secs(1);
/// How many changes `marsh status` lists.
const HISTORY_SHOWN: usize = 10;
/// How long the folders must stay unchanged before an import.
const SETTLE: Duration = Duration::from_secs(2);
/// After a failed pass, when the same folders are tried again.
const RETRY: Duration = Duration::from_mins(1);
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_SKILL_FILES: usize = 4096;
const MAX_SKILL_BYTES: u64 = 64 * 1024 * 1024;
/// How many folder levels a skill may have.
const MAX_SKILL_DEPTH: usize = 64;
const MAX_STAMPS: usize = 65_536;
/// How long the first sync waits for registered Kits' descriptors.
const BUNDLED_WAIT: Duration = Duration::from_secs(30);

/// What the registered Kits bundle: each skill name with the commands
/// whose Kit bundles it, and whether a Kit's descriptor is still unread.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Bundled {
    pub(crate) skills: BTreeMap<String, Vec<String>>,
    pub(crate) pending: bool,
}

/// What the daemon imported: each name with its store content's digest.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct Record {
    #[serde(default)]
    skills: BTreeMap<String, String>,
    /// The scope (control home name) whose mirror imported each skill. A
    /// name one scope's Kit bundles is removed only when that scope's own
    /// mirror imported it, never another scope's import.
    #[serde(default)]
    owners: BTreeMap<String, String>,
}

#[derive(Default)]
pub(crate) struct Mirror {
    state: Mutex<State>,
    /// `MARSH_SKILLS_HOME` at launch: whose folders are scanned, instead of
    /// the stock HOME's (tests).
    pub(crate) scan_home: Option<PathBuf>,
}

#[derive(Default)]
struct State {
    folders: Option<Vec<PathBuf>>,
    /// The folder stamps last seen, and since when.
    seen: Option<(Vec<Stamp>, Instant)>,
    /// The stamps the store is in step with.
    synced: Option<Vec<Stamp>>,
    retry_at: Option<Instant>,
    /// The bundled skills the store is in step with.
    synced_bundled: Option<BTreeMap<String, Vec<String>>>,
    /// When the mirror first waited for a Kit's descriptor.
    waiting_since: Option<Instant>,
    status: SkillsMirrorStatus,
}

type Stamp = (PathBuf, u64, i64, i64);

/// `skills_mirror` in the scope's `config.json`; missing or unreadable is on.
pub(crate) fn enabled(scope_root: &Path) -> bool {
    fs::read(scope_root.join("config.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get(CONFIG_KEY).and_then(serde_json::Value::as_bool))
        .unwrap_or(true)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

impl Mirror {
    pub(crate) fn status(&self, scope_root: &Path, control_home: &Path) -> SkillsMirrorStatus {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut history = history(control_home);
        history.truncate(HISTORY_SHOWN);
        SkillsMirrorStatus {
            enabled: enabled(scope_root),
            history,
            ..state.status.clone()
        }
    }

    /// One pass: sync at start, then once the folders changed and settled.
    pub(crate) fn pass(
        &self,
        sbx: &StockSbx,
        scope_root: &Path,
        control_home: &Path,
        bundled: &Bundled,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !enabled(scope_root) {
            state.synced = None;
            return;
        }
        if !bundled.pending {
            state.waiting_since = None;
        } else if state
            .waiting_since
            .get_or_insert_with(Instant::now)
            .elapsed()
            < BUNDLED_WAIT
        {
            return;
        }
        if state.folders.is_none() {
            if state.retry_at.is_some_and(|at| Instant::now() < at) {
                return;
            }
            match sbx.skills_import_folders(self.scan_home.as_deref()) {
                Ok(folders) => state.folders = Some(folders),
                Err(error) => {
                    fail(
                        &mut state,
                        format!("cannot read the sbx skills folders: {error}"),
                    );
                    return;
                }
            }
        }
        let folders = state.folders.clone().unwrap_or_default();
        let stamps = stamps(&folders);
        if state.retry_at.is_some_and(|at| Instant::now() >= at) {
            state.retry_at = None;
            state.synced = None;
        }
        let in_step = state.synced.as_ref() == Some(&stamps);
        if in_step && state.synced_bundled.as_ref() == Some(&bundled.skills) {
            return;
        }
        let first = state.synced.is_none() && state.status.last_import.is_none();
        if !in_step {
            match &state.seen {
                Some((seen, since)) if *seen == stamps => {
                    if !first && since.elapsed() < SETTLE {
                        return;
                    }
                }
                _ => {
                    state.seen = Some((stamps.clone(), Instant::now()));
                    if !first {
                        return;
                    }
                }
            }
        }
        match sync(sbx, &folders, control_home, &bundled.skills) {
            Ok(synced) => {
                state.status.clashes = synced.clashes;
                state.status.last_import = Some(now_ms());
                state.status.errors = synced.refused;
            }
            // Another scope's mirror is syncing: try again on the next pass.
            Err(message) if message == BUSY => return,
            Err(message) => fail(&mut state, message),
        }
        state.synced = Some(stamps);
        state.synced_bundled = Some(bundled.skills.clone());
    }
}

fn fail(state: &mut State, message: String) {
    state.status.errors = vec![message];
    state.retry_at = Some(Instant::now() + RETRY);
}

/// Path, size, mtime and ctime of everything under the folders (top-level
/// links followed, as `sbx skills import` does).
fn stamps(folders: &[PathBuf]) -> Vec<Stamp> {
    use std::os::unix::fs::MetadataExt as _;
    let mut found = Vec::new();
    let mut pending: Vec<(PathBuf, bool)> = folders.iter().map(|f| (f.clone(), true)).collect();
    while let Some((path, follow)) = pending.pop() {
        if found.len() >= MAX_STAMPS {
            break;
        }
        let metadata = if follow {
            fs::metadata(&path)
        } else {
            fs::symlink_metadata(&path)
        };
        let Ok(metadata) = metadata else { continue };
        found.push((
            path.clone(),
            metadata.size(),
            metadata.mtime_nsec() + metadata.mtime() * 1_000_000_000,
            metadata.ctime(),
        ));
        if metadata.is_dir()
            && let Ok(entries) = fs::read_dir(&path)
        {
            let top = folders.contains(&path);
            for entry in entries.flatten() {
                pending.push((entry.path(), top));
            }
        }
    }
    found.sort();
    found
}

/// The first copy of each skill, in folder order: a directory (or a link
/// to one) with a `SKILL.md`.
fn sources(folders: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    for folder in folders {
        let Ok(entries) = fs::read_dir(folder) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = entry.path();
            // Any `SKILL.md` (a link too, even a dangling one) makes this a
            // skill: `read_skill` refuses a bad one, which keeps the store's
            // last good copy instead of removing it.
            if valid_name(&name) && fs::symlink_metadata(path.join("SKILL.md")).is_ok() {
                found.entry(name).or_insert(path);
            }
        }
    }
    found
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// One skill's content: each regular file's path in the skill folder, and
/// whether it is executable, with its bytes.
type Files = BTreeMap<PathBuf, (bool, Vec<u8>)>;

/// Reads a skill folder by descriptor. The folder's path is resolved once
/// (a top-level link is followed: its target is the skill folder), then
/// opened one component at a time without following links, so a folder
/// swapped for a link after that fails instead of leading elsewhere; its
/// `SKILL.md` must be a regular file read through that handle. The walk
/// follows no link below the folder and is not recursive, bounded at
/// `MAX_SKILL_DEPTH` levels and `MAX_SKILL_FILES` entries. A link inside
/// counts as the regular file it names in the same folder, by a relative
/// path that stays in the folder; any other link, or anything but a
/// directory or regular file, refuses the skill. The import gets a copy of
/// these files, never a link to the folder, so whatever stock SBX does with
/// links, nothing outside the folder reaches the store.
/// A source also may not hold a file with other hard links, which could be
/// any file of yours; the store's copies, which stock SBX writes, may.
fn read_skill(root: &Path) -> Result<Files, String> {
    read_skill_with(root, false, &mut |_, _| {})
}

fn read_source(root: &Path) -> Result<Files, String> {
    read_skill_with(root, true, &mut |_, _| {})
}

/// The points between the steps of a read where `read_skill_with` calls
/// its hook: after the folder's path is resolved, and before a folder or a
/// file in it is opened. Tests change the folder there.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Resolved,
    OpenFolder,
    OpenFile,
}

fn read_skill_with(
    root: &Path,
    source: bool,
    hook: &mut dyn FnMut(Step, &Path),
) -> Result<Files, String> {
    use rustix::fs::{Mode, OFlags};
    let folder = std::rc::Rc::new(open_skill_folder(root, hook)?);
    let mut walk = Walk {
        source,
        ..Walk::default()
    };
    let mut pending = Vec::new();
    for (name, path) in walk.read(&folder, Path::new(""), hook)? {
        pending.push((std::rc::Rc::clone(&folder), name, path));
    }
    while let Some((parent, name, path)) = pending.pop() {
        if path.components().count() > MAX_SKILL_DEPTH {
            return Err(format!("more than {MAX_SKILL_DEPTH} folder levels"));
        }
        hook(Step::OpenFolder, &path);
        let inner = rustix::fs::openat(
            &*parent,
            &name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| format!("{}: {error}", path.display()))?;
        drop(parent);
        let inner = std::rc::Rc::new(inner);
        for (name, path) in walk.read(&inner, &path, hook)? {
            pending.push((std::rc::Rc::clone(&inner), name, path));
        }
    }
    let Walk {
        mut files,
        links,
        mut total,
        ..
    } = walk;
    if !files.contains_key(Path::new("SKILL.md")) {
        return Err("SKILL.md is not a regular file in the skill folder".into());
    }
    for (link, target) in links {
        let Some(file) = files.get(&target).cloned() else {
            return Err(format!(
                "{} is a link to {}, which is not a regular file in the skill folder",
                link.display(),
                target.display()
            ));
        };
        total += file.1.len() as u64;
        if total > MAX_SKILL_BYTES {
            return Err("too big".into());
        }
        files.insert(link, file);
    }
    Ok(files)
}

/// Opens the folder `root` resolves to now, one component at a time, never
/// following a link.
fn open_skill_folder(
    root: &Path,
    hook: &mut dyn FnMut(Step, &Path),
) -> Result<std::os::fd::OwnedFd, String> {
    use rustix::fs::{Mode, OFlags};
    use std::path::Component;
    let resolved = fs::canonicalize(root).map_err(|error| error.to_string())?;
    hook(Step::Resolved, &resolved);
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut folder = rustix::fs::open("/", flags, Mode::empty()).map_err(|e| e.to_string())?;
    for component in resolved.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                folder = rustix::fs::openat(&folder, name, flags, Mode::empty())
                    .map_err(|error| format!("the skill folder changed while read: {error}"))?;
            }
            _ => return Err(format!("unexpected path {}", resolved.display())),
        }
    }
    Ok(folder)
}

/// What a skill-folder walk found so far.
#[derive(Default)]
struct Walk {
    /// Refuse files with other hard links.
    source: bool,
    files: Files,
    links: Vec<(PathBuf, PathBuf)>,
    total: u64,
    entries: usize,
}

impl Walk {
    /// Reads one folder's files and links; returns its subfolders.
    fn read(
        &mut self,
        folder: &std::os::fd::OwnedFd,
        relative: &Path,
        hook: &mut dyn FnMut(Step, &Path),
    ) -> Result<Vec<(std::ffi::CString, PathBuf)>, String> {
        use rustix::fs::{AtFlags, FileType, Mode, OFlags};
        use std::os::unix::ffi::OsStrExt as _;
        let mut folders = Vec::new();
        let mut reader = rustix::fs::Dir::read_from(folder).map_err(|error| error.to_string())?;
        while let Some(entry) = reader.read() {
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            self.entries += 1;
            if self.entries > MAX_SKILL_FILES {
                return Err("too many files".into());
            }
            let path = relative.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
            let stat = rustix::fs::statat(folder, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| format!("{}: {error}", path.display()))?;
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => folders.push((name.to_owned(), path)),
                FileType::RegularFile => {
                    hook(Step::OpenFile, &path);
                    let file = rustix::fs::openat(
                        folder,
                        name,
                        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                    let stat = rustix::fs::fstat(&file).map_err(|error| error.to_string())?;
                    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                        return Err(format!("{} changed while read", path.display()));
                    }
                    if self.source && stat.st_nlink > 1 {
                        return Err(format!("{} has other hard links", path.display()));
                    }
                    let mut bytes = Vec::new();
                    fs::File::from(file)
                        .take(MAX_SKILL_BYTES + 1 - self.total)
                        .read_to_end(&mut bytes)
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                    self.total += bytes.len() as u64;
                    if self.total > MAX_SKILL_BYTES {
                        return Err("too big".into());
                    }
                    self.files.insert(path, (stat.st_mode & 0o111 != 0, bytes));
                }
                FileType::Symlink => {
                    let target = rustix::fs::readlinkat(folder, name, Vec::new())
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                    let target = Path::new(std::ffi::OsStr::from_bytes(target.as_bytes()));
                    let resolved = inside(relative, target).ok_or_else(|| {
                        format!(
                            "{} is a link to {}, outside the skill folder",
                            path.display(),
                            target.display()
                        )
                    })?;
                    self.links.push((path, resolved));
                }
                _ => return Err(format!("{} is not a regular file", path.display())),
            }
        }
        Ok(folders)
    }
}

/// Where a relative link target in `folder` leads, when that stays inside
/// the skill folder.
fn inside(folder: &Path, target: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut resolved = folder.to_owned();
    for component in target.components() {
        match component {
            Component::Normal(name) => resolved.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(resolved)
}

/// A digest of a skill's files (paths and bytes).
fn digest(files: &Files) -> String {
    let mut hash = Sha256::new();
    for (file, (_, bytes)) in files {
        hash.update(file.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    }
    marsh_contracts::process::hex(&hash.finalize())
}

/// What one sync does with each name.
#[derive(Debug, Default, Eq, PartialEq)]
struct Plan {
    import: Vec<String>,
    remove: Vec<String>,
    /// Sources whose store copy came from elsewhere (`sbx skills add`).
    clashes: Vec<String>,
    /// Sources a registered Kit bundles, with those Kits.
    bundled: Vec<(String, Vec<String>)>,
    record: BTreeMap<String, String>,
}

/// `store` holds the digest of each store entry; `sources` of each source
/// (`None`: refused, so its store copy is left as it is); `bundled` the
/// commands whose Kit bundles each name.
fn plan(
    sources: &BTreeMap<String, Option<String>>,
    store: &BTreeMap<String, Option<String>>,
    record: &BTreeMap<String, String>,
    bundled: &BTreeMap<String, Vec<String>>,
) -> Plan {
    let mut plan = Plan::default();
    for (name, source) in sources {
        if let Some(kits) = bundled.get(name) {
            plan.bundled.push((name.clone(), kits.clone()));
            continue;
        }
        let Some(source) = source else { continue };
        let recorded = record.get(name);
        match store.get(name) {
            None => plan.import.push(name.clone()),
            Some(current) => {
                // Only what the mirror imported is its own: the same content
                // from `sbx skills add` stays the user's, in step or not.
                let ours = current.is_some() && current.as_ref() == recorded;
                if current.as_ref() == Some(source) {
                    if ours {
                        plan.record.insert(name.clone(), source.clone());
                    }
                } else if ours {
                    plan.import.push(name.clone());
                } else {
                    plan.clashes.push(name.clone());
                }
            }
        }
    }
    for (name, recorded) in record {
        if sources.contains_key(name) && !bundled.contains_key(name) {
            continue;
        }
        if store
            .get(name)
            .is_some_and(|current| current.as_ref() == Some(recorded))
        {
            plan.remove.push(name.clone());
        }
    }
    plan
}

/// The account's mirror folder beside the scopes' control homes
/// (`CONTROL/skills-mirror`): the one place every scope's mirror records
/// what it imported into the account's store.
fn account_home(control_home: &Path) -> PathBuf {
    control_home
        .parent()
        .unwrap_or(control_home)
        .join(ACCOUNT_DIR)
}

/// The record of one store, by its path.
fn record_path(control_home: &Path, store: &Path) -> PathBuf {
    account_home(control_home).join(format!("record-{}.json", store_key(store)))
}

/// What names one store in the account's mirror folder.
fn store_key(store: &Path) -> String {
    let key = marsh_contracts::process::hex(&Sha256::digest(store.as_os_str().as_encoded_bytes()));
    key[..16].to_owned()
}

/// Takes the account's mirror lock, so one scope's mirror at a time
/// changes the store; `None` while another holds it.
fn lock_account(control_home: &Path) -> Result<Option<std::os::fd::OwnedFd>, String> {
    use rustix::fs::{FlockOperation, Mode, OFlags};
    let home = account_home(control_home);
    fs::create_dir_all(&home)
        .map_err(|error| format!("cannot create {}: {error}", home.display()))?;
    let lock = rustix::fs::open(
        home.join("lock"),
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| format!("cannot open the skills mirror lock: {error}"))?;
    match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(lock)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(error) => Err(format!("cannot lock the skills mirror: {error}")),
    }
}

fn read_record_file(path: &Path) -> Option<Record> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes))
        .ok()?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// The store's record. A scope's own record from an older marsh is merged
/// in (the account's entries win) and moved aside once the merged record is
/// written, so what any scope imported stays the mirror's.
fn read_record(control_home: &Path, store: &Path) -> (Record, Option<PathBuf>) {
    let mut record = read_record_file(&record_path(control_home, store)).unwrap_or_default();
    let legacy = control_home.join(LEGACY_RECORD_FILE);
    let Some(old) = read_record_file(&legacy) else {
        return (record, None);
    };
    for (name, digest) in old.skills {
        if !record.skills.contains_key(&name) {
            record.owners.insert(name.clone(), scope_name(control_home));
            record.skills.insert(name, digest);
        }
    }
    (record, Some(legacy))
}

/// The name the record knows a scope by: its control home's.
fn scope_name(control_home: &Path) -> String {
    control_home
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Writes the record durably (synced, then renamed into place and the
/// folder synced). A merged older record is then moved aside; if that
/// fails it is deleted, and if that fails too the pass fails, so it is
/// never merged again over newer entries.
fn write_record(
    control_home: &Path,
    store: &Path,
    record: &Record,
    legacy: Option<&Path>,
) -> Result<(), String> {
    let mut record = record.clone();
    let names = record.skills.clone();
    record.owners.retain(|name, _| names.contains_key(name));
    let mut bytes = serde_json::to_vec_pretty(&record).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let home = account_home(control_home);
    let mut temporary =
        tempfile::NamedTempFile::new_in(&home).map_err(|error| error.to_string())?;
    temporary
        .write_all(&bytes)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    temporary
        .persist(record_path(control_home, store))
        .map_err(|error| error.error.to_string())?;
    fs::File::open(&home)
        .and_then(|folder| folder.sync_all())
        .map_err(|error| error.to_string())?;
    if let Some(legacy) = legacy
        && fs::symlink_metadata(legacy).is_ok()
        && fs::rename(legacy, legacy.with_extension("json.migrated")).is_err()
    {
        fs::remove_file(legacy)
            .map_err(|error| format!("cannot move {} aside: {error}", legacy.display()))?;
    }
    Ok(())
}

/// Writes a skill's files under `root`, as new regular files; `durable`
/// syncs each file and folder to disk.
fn write_files(root: &Path, files: &Files, durable: bool) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let mut folders = BTreeSet::from([root.to_owned()]);
    for (relative, (executable, bytes)) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            folders.extend(
                parent
                    .ancestors()
                    .take_while(|at| at.starts_with(root))
                    .map(Path::to_owned),
            );
        }
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if *executable { 0o755 } else { 0o644 })
            .open(&path)
            .and_then(|mut file| {
                file.write_all(bytes)?;
                if durable { file.sync_all() } else { Ok(()) }
            })
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    if durable {
        for folder in folders {
            sync_folder(&folder)?;
        }
    }
    Ok(())
}

fn sync_folder(folder: &Path) -> Result<(), String> {
    fs::File::open(folder)
        .and_then(|folder| folder.sync_all())
        .map_err(|error| format!("cannot sync {}: {error}", folder.display()))
}

/// One store's history, beside its record: a store's mirror never
/// settles or prunes another store's entries.
fn history_dir(control_home: &Path, store: &Path) -> PathBuf {
    account_home(control_home)
        .join(HISTORY_DIR)
        .join(store_key(store))
}

/// A unified diff from `before` to `after` (either `None`: no skill). A
/// file over `MAX_DIFF_FILE` bytes is only named, and the line diffs share
/// a `DIFF_TIME` deadline (past it, a file's diff is coarser but correct),
/// so a big skill edit cannot hold the mirror (and `marsh status`) up.
fn diff(name: &str, before: Option<&Files>, after: Option<&Files>) -> String {
    diff_until(name, before, after, Instant::now() + DIFF_TIME)
}

fn diff_until(
    name: &str,
    before: Option<&Files>,
    after: Option<&Files>,
    deadline: Instant,
) -> String {
    use std::fmt::Write as _;
    let empty = Files::new();
    let (before, after) = (before.unwrap_or(&empty), after.unwrap_or(&empty));
    let text = |file: Option<&(bool, Vec<u8>)>| match file {
        None => Some(String::new()),
        Some((_, bytes)) => std::str::from_utf8(bytes)
            .ok()
            .filter(|text| !text.contains('\0'))
            .map(ToOwned::to_owned),
    };
    let mut out = String::new();
    let paths: BTreeSet<_> = before.keys().chain(after.keys()).collect();
    for path in paths {
        let (old, new) = (before.get(path), after.get(path));
        if old == new {
            continue;
        }
        let shown = Path::new(name).join(path).display().to_string();
        let header = |file: Option<&(bool, Vec<u8>)>, side: &str| {
            file.map_or_else(|| "/dev/null".to_owned(), |_| format!("{side}/{shown}"))
        };
        let large = |file: Option<&(bool, Vec<u8>)>| {
            file.is_some_and(|(_, bytes)| bytes.len() as u64 > MAX_DIFF_FILE)
        };
        if large(old) || large(new) {
            let _ = writeln!(out, "large file changed: {shown}");
            continue;
        }
        match (text(old), text(new)) {
            (Some(old_text), Some(new_text)) if old_text != new_text || old.is_none() => {
                let _ = write!(
                    out,
                    "{}",
                    similar::TextDiff::configure()
                        .deadline(deadline)
                        .diff_lines(&old_text, &new_text)
                        .unified_diff()
                        .context_radius(3)
                        .header(&header(old, "a"), &header(new, "b"))
                );
            }
            (Some(_), Some(_)) => {
                let _ = writeln!(out, "mode changed: {shown}");
            }
            _ => {
                let _ = writeln!(out, "binary file changed: {shown}");
            }
        }
    }
    out
}

/// What `change.json` in a history entry holds.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct HistoryEntry {
    name: String,
    change: String,
    at: u64,
    /// The digest of the store's copy before the change (`null` when it
    /// added the skill), and of the copy the change installs (`null` when
    /// it removed the skill): what recovery compares the store with.
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    after: Option<String>,
    /// The scope whose mirror prepared the change: a new skill recovery
    /// takes as the mirror's is that scope's.
    #[serde(default)]
    scope: Option<String>,
    /// Set right after the import, before the record is written: whether
    /// the import succeeded and the store then held exactly the staged copy.
    /// Only that proves a new skill's store copy is the mirror's (a failed
    /// import beside a user's identical add leaves the same bytes).
    #[serde(default)]
    imported: Option<bool>,
}

/// Prepares one change for the history before the store is changed:
/// `history/STORE/.pending-AT-NAME/` holds `change.json`, `diff`, and
/// `before/`, the store's copy it replaces (none for a new skill). It is
/// written as `.tmp-AT-NAME`, synced to disk and renamed, so a pending entry
/// is always whole. Only once the change is made and the record written
/// does `publish_history` move it to `AT-NAME/`; `discard_history` drops it
/// when the store did not change, so a failing retry never adds an entry or
/// pushes out a real one. After a crash, `recover_history` decides from
/// the store.
fn keep_history(
    control_home: &Path,
    store: &Path,
    name: &str,
    before: Option<&Files>,
    after: Option<&Files>,
) -> Result<PathBuf, String> {
    keep_history_with(control_home, store, name, before, after, &mut |_| {})
}

/// `keep_history`, calling `hook` with the written entry just before it is
/// renamed into place (tests look there).
fn keep_history_with(
    control_home: &Path,
    store: &Path,
    name: &str,
    before: Option<&Files>,
    after: Option<&Files>,
    hook: &mut dyn FnMut(&Path),
) -> Result<PathBuf, String> {
    let history = history_dir(control_home, store);
    fs::create_dir_all(&history)
        .map_err(|error| format!("cannot keep the skills history: {error}"))?;
    let mut at = now_ms();
    let (temporary, entry) = loop {
        let names = [
            format!(".tmp-{at:013}-{name}"),
            format!(".pending-{at:013}-{name}"),
            format!("{at:013}-{name}"),
        ];
        if names[1..].iter().any(|taken| history.join(taken).exists()) {
            at += 1;
            continue;
        }
        let temporary = history.join(&names[0]);
        match fs::create_dir(&temporary) {
            Ok(()) => break (temporary, history.join(&names[1])),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => at += 1,
            Err(error) => return Err(format!("cannot keep the skills history: {error}")),
        }
    };
    let change = match (before, after) {
        (None, _) => "added",
        (Some(_), Some(_)) => "updated",
        (Some(_), None) => "removed",
    };
    let record = HistoryEntry {
        name: name.to_owned(),
        change: change.to_owned(),
        at,
        before: before.map(digest),
        after: after.map(digest),
        scope: Some(scope_name(control_home)),
        imported: None,
    };
    let kept = (|| {
        if let Some(before) = before {
            write_files(&temporary.join("before"), before, true)?;
        }
        let bytes = serde_json::to_vec_pretty(&record).map_err(|e| e.to_string())?;
        let files = Files::from([
            (
                "diff".into(),
                (false, diff(name, before, after).into_bytes()),
            ),
            ("change.json".into(), (false, bytes)),
        ]);
        write_files(&temporary, &files, true)?;
        hook(&temporary);
        fs::rename(&temporary, &entry).map_err(|e| e.to_string())?;
        sync_folder(&history)
    })();
    if let Err(error) = kept {
        let _ = fs::remove_dir_all(&temporary);
        let _ = fs::remove_dir_all(&entry);
        return Err(format!("cannot keep the skills history: {error}"));
    }
    Ok(entry)
}

/// Drops prepared history entries whose change was not made.
fn discard_history(pending: &[PathBuf]) {
    for entry in pending {
        let _ = fs::remove_dir_all(entry);
    }
}

/// Moves prepared entries into the history, then keeps the newest
/// `HISTORY_KEEP` of each skill.
fn publish_history(control_home: &Path, store: &Path, pending: &[PathBuf]) {
    let history = history_dir(control_home, store);
    let mut names = BTreeSet::new();
    for entry in pending {
        let Some(published) = entry
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix(".pending-"))
        else {
            continue;
        };
        if fs::rename(entry, history.join(published)).is_ok()
            && let Some((_, name)) = published.split_once('-')
        {
            names.insert(name.to_owned());
        }
    }
    let _ = sync_folder(&history);
    let entries: Vec<_> = fs::read_dir(&history)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    for name in names {
        let mut older: Vec<_> = entries
            .iter()
            .filter(|entry| !entry.starts_with('.'))
            .filter(|entry| entry.split_once('-').is_some_and(|(_, rest)| rest == name))
            .collect();
        older.sort();
        let excess = older.len().saturating_sub(HISTORY_KEEP);
        for old in &older[..excess] {
            let _ = fs::remove_dir_all(history.join(old));
        }
    }
}

/// How a pending entry left by a crash is settled.
#[derive(Debug, Eq, PartialEq)]
enum Settled {
    /// The store holds what the change installed (or the removal is done).
    Made,
    /// The store holds something else: a partial import, a copy it cannot
    /// read, or a user's replacement. The entry is kept as evidence, and
    /// the store's copy is not taken as the mirror's.
    Uncertain,
    /// The store still holds what the change replaced: nothing happened.
    NotMade,
}

/// How the store, as listed now (`now`: the name's digest, `None` for a
/// copy it cannot read; absent: no such skill), settles `change`.
fn settle(change: &HistoryEntry, now: Option<&Option<String>>) -> Settled {
    match (now, change.change.as_str()) {
        (None, "removed") => Settled::Made,
        (None, _) if change.before.is_none() => Settled::NotMade,
        (Some(Some(now)), _) if Some(now) == change.before.as_ref() => Settled::NotMade,
        (Some(Some(now)), _) if Some(now) == change.after.as_ref() => Settled::Made,
        _ => Settled::Uncertain,
    }
}

/// After a crash: drops half-written entries (no change was made before
/// every entry was whole), drops each pending entry whose change was not
/// made, and returns the rest with how each settled. The caller updates the
/// record, writes it, and only then publishes them.
fn recover_history(
    control_home: &Path,
    store: &Path,
    now: &BTreeMap<String, Option<String>>,
) -> Vec<(PathBuf, HistoryEntry, Settled)> {
    let history = history_dir(control_home, store);
    let mut settled = Vec::new();
    for entry in fs::read_dir(&history).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if file.starts_with(".tmp-") {
            let _ = fs::remove_dir_all(entry.path());
            continue;
        }
        if !file.starts_with(".pending-") {
            continue;
        }
        let Some(change) = read_change(&entry.path()) else {
            continue;
        };
        match settle(&change, now.get(&change.name)) {
            Settled::NotMade => discard_history(&[entry.path()]),
            how => settled.push((entry.path(), change, how)),
        }
    }
    settled
}

/// What a change made before a crash means for the record: the record
/// follows a change only when it owned the copy the change replaced (or
/// already holds what it installed); a skill the change added becomes the
/// mirror's, and the preparing scope's, only when the entry says the import
/// succeeded with exactly that copy. An uncertain change never moves the
/// record.
fn follow(record: &mut Record, change: &HistoryEntry, how: &Settled) {
    if *how != Settled::Made {
        return;
    }
    let name = &change.name;
    let held = record.skills.get(name);
    let owned = held.is_some() && (held == change.before.as_ref() || held == change.after.as_ref());
    match (&change.after, change.before.is_none()) {
        (None, _) if owned => {
            record.skills.remove(name);
        }
        (Some(after), false) if owned => {
            record.skills.insert(name.clone(), after.clone());
        }
        (Some(after), true) if held.is_none() || held == Some(after) => {
            if let (Some(true), Some(scope)) = (change.imported, &change.scope) {
                record.skills.insert(name.clone(), after.clone());
                record.owners.insert(name.clone(), scope.clone());
            }
        }
        _ => {}
    }
}

/// Replaces an entry's `change.json`, durably.
fn rewrite_change(entry: &Path, change: &HistoryEntry) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(change).map_err(|e| e.to_string())?;
    let temporary = entry.join(".change.json");
    let _ = fs::remove_file(&temporary);
    write_files(
        entry,
        &Files::from([(".change.json".into(), (false, bytes))]),
        true,
    )?;
    fs::rename(&temporary, entry.join("change.json")).map_err(|e| e.to_string())?;
    sync_folder(entry)
}

fn read_change(entry: &Path) -> Option<HistoryEntry> {
    let mut bytes = Vec::new();
    fs::File::open(entry.join("change.json"))
        .ok()?
        .take(4096)
        .read_to_end(&mut bytes)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The changes in every store's history, newest first.
fn history(control_home: &Path) -> Vec<SkillChange> {
    let stores = account_home(control_home).join(HISTORY_DIR);
    let mut found: Vec<_> = fs::read_dir(&stores)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|store| fs::read_dir(store.path()).into_iter().flatten().flatten())
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| {
            let change = read_change(&entry.path())?;
            Some(SkillChange {
                name: change.name,
                change: change.change,
                at: change.at,
                diff: entry.path().join("diff").display().to_string(),
            })
        })
        .collect();
    found.sort_by(|a, b| (b.at, &b.name).cmp(&(a.at, &a.name)));
    found
}

/// The HOME for one import: a copy of each chosen skill's files, and the
/// store's parent directory at its place under HOME.
fn stage(
    control_home: &Path,
    stock_home: &Path,
    store: &Path,
    chosen: &[(&String, &Files)],
) -> Result<PathBuf, String> {
    let staging = control_home.join(STAGING_DIR);
    if fs::symlink_metadata(&staging).is_ok() {
        fs::remove_dir_all(&staging)
            .map_err(|error| format!("cannot clear {}: {error}", staging.display()))?;
    }
    let skills = staging.join(".agents/skills");
    fs::create_dir_all(&skills).map_err(|error| error.to_string())?;
    for (name, files) in chosen {
        write_files(&skills.join(name), files, false)?;
    }
    let parent = store
        .parent()
        .ok_or_else(|| format!("unexpected skills store {}", store.display()))?;
    let relative = parent.strip_prefix(stock_home).map_err(|_| {
        format!(
            "the skills store {} is not under {}",
            store.display(),
            stock_home.display()
        )
    })?;
    let place = staging.join(relative);
    if let Some(above) = place.parent() {
        fs::create_dir_all(above).map_err(|error| error.to_string())?;
    }
    std::os::unix::fs::symlink(parent, &place).map_err(|error| error.to_string())?;
    Ok(staging)
}

/// What one sync left for `marsh status`.
#[derive(Debug, Default, Eq, PartialEq)]
struct Synced {
    clashes: Vec<String>,
    /// Skills not imported, each with why (a link out of its folder).
    refused: Vec<String>,
}

fn entries(pending: &[(&str, PathBuf, Option<String>)]) -> Vec<PathBuf> {
    pending.iter().map(|(_, entry, _)| entry.clone()).collect()
}

/// Brings the store in step.
#[allow(clippy::too_many_lines)] // One ordered read, history, import, removal and record pass.
fn sync(
    sbx: &StockSbx,
    folders: &[PathBuf],
    control_home: &Path,
    bundled: &BTreeMap<String, Vec<String>>,
) -> Result<Synced, String> {
    let Some(_lock) = lock_account(control_home)? else {
        return Err(BUSY.into());
    };
    let store = sbx
        .skills_store()
        .map_err(|error| format!("cannot list the sbx skills store: {error}"))?;
    let found = sources(folders);
    let read: BTreeMap<_, _> = found
        .iter()
        .filter(|(name, _)| !bundled.contains_key(*name))
        .map(|(name, path)| (name.clone(), read_source(path)))
        .collect();
    let refused = read
        .iter()
        .filter_map(|(name, files)| {
            files
                .as_ref()
                .err()
                .map(|why| format!("skill {name} not imported: {why}"))
        })
        .collect();
    let source_digests: BTreeMap<_, _> = found
        .keys()
        .map(|name| {
            let files = read.get(name).and_then(|files| files.as_ref().ok());
            (name.clone(), files.map(digest))
        })
        .collect();
    let store_digest = |name: &String| {
        read_skill(&store.directory.join(name))
            .ok()
            .map(|files| digest(&files))
    };
    let store_digests: BTreeMap<_, _> = store
        .skills
        .iter()
        .map(|name| (name.clone(), store_digest(name)))
        .collect();
    let (mut record, legacy) = read_record(control_home, &store.directory);
    let scope = scope_name(control_home);
    // Changes a crash left pending: the record follows them, is written,
    // and only then are their entries published.
    let recovered = recover_history(control_home, &store.directory, &store_digests);
    if !recovered.is_empty() {
        for (path, change, how) in &recovered {
            follow(&mut record, change, how);
            // A change that may never have happened is not shown as one.
            if *how == Settled::Uncertain {
                let kept = HistoryEntry {
                    change: "uncertain".into(),
                    ..change.clone()
                };
                let _ = rewrite_change(path, &kept);
            }
        }
        write_record(control_home, &store.directory, &record, None)?;
        let paths: Vec<_> = recovered.iter().map(|(path, _, _)| path.clone()).collect();
        publish_history(control_home, &store.directory, &paths);
    }
    // A name this scope's Kits bundle but another scope's mirror imported
    // is that scope's: this pass neither removes nor forgets it.
    let others: BTreeSet<String> = record
        .skills
        .keys()
        .filter(|name| bundled.contains_key(*name))
        .filter(|name| {
            record
                .owners
                .get(*name)
                .is_some_and(|owner| *owner != scope)
        })
        .cloned()
        .collect();
    let visible: BTreeMap<_, _> = record
        .skills
        .iter()
        .filter(|(name, _)| !others.contains(*name))
        .map(|(name, digest)| (name.clone(), digest.clone()))
        .collect();
    let plan = plan(&source_digests, &store_digests, &visible, bundled);
    record.skills.retain(|name, _| {
        (found.contains_key(name) && !bundled.contains_key(name) && !plan.clashes.contains(name))
            || plan.remove.contains(name)
            || others.contains(name)
    });
    record.skills.extend(plan.record.clone());
    let mut result = Ok(());
    if !plan.import.is_empty() {
        let chosen: Vec<_> = plan
            .import
            .iter()
            .filter_map(|name| Some((name, read.get(name)?.as_ref().ok()?)))
            .collect();
        let home = sbx
            .stock_home()
            .ok_or_else(|| "the stock SBX HOME is unknown".to_owned())?;
        // Each change's history is prepared first (none is made when that
        // fails); after the import the record is written, and only then
        // are the entries of the skills whose store copy changed published.
        let mut pending = Vec::new();
        for (name, files) in &chosen {
            let before = store
                .skills
                .contains(name)
                .then(|| read_skill(&store.directory.join(name)))
                .transpose()
                .map_err(|error| format!("cannot read the store's copy of {name}: {error}"));
            let kept = before.and_then(|before| {
                let entry = keep_history(
                    control_home,
                    &store.directory,
                    name,
                    before.as_ref(),
                    Some(files),
                )?;
                Ok((name.as_str(), entry, before.as_ref().map(digest)))
            });
            match kept {
                Ok(kept) => pending.push(kept),
                Err(error) => {
                    discard_history(&entries(&pending));
                    return Err(error);
                }
            }
        }
        let staging = match stage(control_home, &home, &store.directory, &chosen) {
            Ok(staging) => staging,
            Err(error) => {
                discard_history(&entries(&pending));
                return Err(error);
            }
        };
        result = sbx
            .skills_import(&staging)
            .map_err(|error| format!("sbx skills import failed: {error}"));
        let _ = fs::remove_dir_all(&staging);
        let (changed, unchanged): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .partition(|(name, _, before)| store_digest(&(*name).to_owned()) != *before);
        discard_history(&entries(&unchanged));
        // Say in each entry whether this import put its copy there, before
        // the record is written: recovery trusts only that.
        for (name, entry, _) in &changed {
            let staged = read
                .get(*name)
                .and_then(|files| files.as_ref().ok())
                .map(digest);
            let imported =
                result.is_ok() && staged.is_some() && store_digest(&(*name).to_owned()) == staged;
            if let Some(change) = read_change(entry) {
                rewrite_change(
                    entry,
                    &HistoryEntry {
                        imported: Some(imported),
                        ..change
                    },
                )?;
            }
        }
        // Only what this import put there is recorded: never a copy that
        // appeared beside a failed import (an `sbx skills add`, even of the
        // same content). A failed import takes no name it did not own.
        for (name, files) in &chosen {
            let staged = digest(files);
            let owned = result.is_ok() || record.skills.contains_key(*name);
            if owned && store_digest(name).as_ref() == Some(&staged) {
                record.skills.insert((*name).clone(), staged);
                record.owners.insert((*name).clone(), scope.clone());
            }
        }
        // A crash or failure from here on leaves the entries pending, and
        // the next pass settles them against the record written here.
        write_record(control_home, &store.directory, &record, legacy.as_deref())?;
        publish_history(control_home, &store.directory, &entries(&changed));
    }
    if result.is_ok() && !plan.remove.is_empty() {
        let mut pending = Vec::new();
        let mut removing = Vec::new();
        for name in &plan.remove {
            let before = read_skill(&store.directory.join(name))
                .map_err(|error| format!("cannot read the store's copy of {name}: {error}"));
            // Changed since the plan (a user's edit): not the mirror's now.
            if let Ok(before) = &before
                && record.skills.get(name) != Some(&digest(before))
            {
                continue;
            }
            let kept = before.and_then(|before| {
                keep_history(control_home, &store.directory, name, Some(&before), None)
            });
            match kept {
                Ok(entry) => {
                    removing.push(name.clone());
                    pending.push((name.as_str(), entry, None));
                }
                Err(error) => {
                    discard_history(&entries(&pending));
                    return Err(error);
                }
            }
        }
        if !removing.is_empty() {
            result = sbx
                .skills_remove(&removing)
                .map_err(|error| format!("sbx skills rm failed: {error}"));
        }
        let (gone, kept): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .partition(|(name, _, _)| fs::symlink_metadata(store.directory.join(name)).is_err());
        discard_history(&entries(&kept));
        record
            .skills
            .retain(|name, _| !gone.iter().any(|(gone, _, _)| *gone == name.as_str()));
        write_record(control_home, &store.directory, &record, legacy.as_deref())?;
        publish_history(control_home, &store.directory, &entries(&gone));
    }
    write_record(control_home, &store.directory, &record, legacy.as_deref())?;
    result?;
    let mut clashes = plan.clashes;
    clashes.extend(
        plan.bundled
            .iter()
            .map(|(name, kits)| format!("{name} (bundled by {})", kits.join(", "))),
    );
    Ok(Synced { clashes, refused })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Option<&str>)]) -> BTreeMap<String, Option<String>> {
        entries
            .iter()
            .map(|(name, digest)| ((*name).to_owned(), digest.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn replaces_and_removes_only_what_it_put_in_the_store() {
        let sources = map(&[
            ("new", Some("n")),
            ("edited", Some("e2")),
            ("same", Some("s")),
            ("added", Some("mine")),
        ]);
        let store = map(&[
            ("edited", Some("e1")),
            ("same", Some("s")),
            ("added", Some("from-repo")),
            ("gone", Some("g")),
            ("changed-since", Some("other")),
        ]);
        let record = BTreeMap::from([
            ("edited".to_owned(), "e1".to_owned()),
            ("same".to_owned(), "s".to_owned()),
            ("gone".to_owned(), "g".to_owned()),
            ("changed-since".to_owned(), "c".to_owned()),
        ]);
        let plan = plan(&sources, &store, &record, &BTreeMap::new());
        assert_eq!(plan.import, ["edited", "new"]);
        assert_eq!(plan.remove, ["gone"]);
        assert_eq!(plan.clashes, ["added"]);
        assert_eq!(plan.record, BTreeMap::from([("same".into(), "s".into())]));
    }

    /// An `sbx skills add` copy that happens to match a folder's skill is
    /// never adopted, so it stays when that folder's skill goes.
    #[test]
    fn an_added_skill_with_the_same_content_is_never_removed() {
        let store = map(&[("added", Some("same"))]);
        let bundled = BTreeMap::new();
        let first = plan(
            &map(&[("added", Some("same"))]),
            &store,
            &BTreeMap::new(),
            &bundled,
        );
        assert_eq!(first, Plan::default());
        let gone = plan(&BTreeMap::new(), &store, &first.record, &bundled);
        assert!(gone.remove.is_empty(), "{gone:?}");
    }

    /// A skill added to the store while an import failed is not recorded
    /// as the mirror's, so it stays when the folder's copy goes.
    #[test]
    fn a_failed_import_never_adopts_a_skill_added_meanwhile() {
        let rig = Rig::new();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "mine").unwrap();
        *rig.fake.added_during_import.lock().unwrap() =
            Some(("tool".to_owned(), "added".to_owned()));
        assert!(rig.sync().is_err());
        fs::remove_dir_all(&skill).unwrap();
        rig.sync().unwrap();
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "added".to_owned())])
        );
    }

    /// Even a same-content skill added while an import failed stays the
    /// user's: a failed import proves nothing about what is in the store.
    #[test]
    fn a_failed_import_never_adopts_a_same_content_skill_added_meanwhile() {
        let rig = Rig::new();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "mine").unwrap();
        *rig.fake.added_during_import.lock().unwrap() =
            Some(("tool".to_owned(), "mine".to_owned()));
        assert!(rig.sync().is_err());
        fs::remove_dir_all(&skill).unwrap();
        rig.sync().unwrap();
        assert!(rig.stored("tool").exists());
    }

    /// The store belongs to the Mac account, so what one scope's mirror
    /// imported another scope's mirror may update and remove.
    #[test]
    fn a_skill_one_scope_imported_is_another_scopes_to_update() {
        let rig = Rig::new();
        let (a, b) = (
            rig.root.path().join("scopes/a"),
            rig.root.path().join("scopes/b"),
        );
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync_in(&a).unwrap();
        fs::write(skill.join("SKILL.md"), "two").unwrap();
        assert_eq!(rig.sync_in(&b).unwrap(), Synced::default());
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "two".to_owned())])
        );
        fs::remove_dir_all(&skill).unwrap();
        rig.sync_in(&a).unwrap();
        assert!(!rig.stored("tool").exists());
    }

    /// A scope's record from an older marsh still makes its imports the
    /// mirror's, and is moved aside once merged.
    #[test]
    fn an_older_scope_record_is_merged_into_the_account_record() {
        let rig = Rig::new();
        let (a, b) = (
            rig.root.path().join("scopes/a"),
            rig.root.path().join("scopes/b"),
        );
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync_in(&a).unwrap();
        let account = record_path(&a, &rig.fake.store());
        fs::create_dir_all(&b).unwrap();
        fs::rename(&account, b.join(LEGACY_RECORD_FILE)).unwrap();
        fs::write(skill.join("SKILL.md"), "two").unwrap();

        assert_eq!(rig.sync_in(&b).unwrap(), Synced::default());
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "two".to_owned())])
        );
        assert!(b.join("skills-mirror.json.migrated").exists());
        assert!(!b.join(LEGACY_RECORD_FILE).exists());
    }

    /// A name one scope's Kit bundles is not removed by that scope's pass
    /// when another scope's mirror imported it (review round 3 probe).
    #[test]
    fn a_kit_bundled_name_in_one_scope_keeps_another_scopes_import() {
        let rig = Rig::new();
        let (a, b) = (
            rig.root.path().join("scopes/a"),
            rig.root.path().join("scopes/b"),
        );
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync_in(&b).unwrap();
        fs::create_dir_all(&a).unwrap();
        let bundled = BTreeMap::from([("tool".to_owned(), vec!["pix".to_owned()])]);
        sync(&rig.sbx, &[rig.folder()], &a, &bundled).unwrap();
        assert!(
            rig.stored("tool").exists(),
            "scope A removed scope B's import"
        );
        // B still owns it: an edit is imported, not a clash.
        fs::write(skill.join("SKILL.md"), "two").unwrap();
        assert_eq!(rig.sync_in(&b).unwrap(), Synced::default());
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "two".to_owned())])
        );
        // What scope A itself imported, its own Kit's name still removes.
        let rig = Rig::new();
        let a = rig.root.path().join("scopes/a");
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync_in(&a).unwrap();
        sync(&rig.sbx, &[rig.folder()], &a, &bundled).unwrap();
        assert!(!rig.stored("tool").exists());
    }

    /// The lock is held while stock SBX imports, and released after.
    #[test]
    fn the_account_lock_is_held_through_the_import() {
        let rig = Rig::new();
        let control = rig.root.path().join("control");
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        let sibling = rig.root.path().join("other-control");
        *rig.fake.during_import.lock().unwrap() = Some(Box::new(move || {
            assert!(
                lock_account(&sibling).unwrap().is_none(),
                "lock released before the import"
            );
        }));
        rig.sync().unwrap();
        assert!(lock_account(&control).unwrap().is_some());
    }

    /// Waiting for another scope's mirror is not an error in `marsh status`.
    #[test]
    fn a_busy_store_is_not_reported_as_an_error() {
        let rig = Rig::new();
        let home = rig.root.path().join("home");
        let skill = home.join(".claude/skills/tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "tool").unwrap();
        let mirror = Mirror {
            scan_home: Some(home),
            ..Mirror::default()
        };
        let scope = rig.root.path().join("scope");
        fs::create_dir_all(&scope).unwrap();
        let control = rig.root.path().join("control");
        fs::create_dir_all(&control).unwrap();
        let held = lock_account(&control).unwrap().unwrap();
        mirror.pass(&rig.sbx, &scope, &control, &Bundled::default());
        let status = mirror.status(&scope, &control);
        assert!(status.errors.is_empty(), "{status:?}");
        assert!(status.last_import.is_none());
        drop(held);
        mirror.pass(&rig.sbx, &scope, &control, &Bundled::default());
        assert!(rig.stored("tool").exists());
    }

    /// When the account record and an older scope record disagree, the
    /// account record wins.
    #[test]
    fn the_account_record_wins_over_an_older_scope_record() {
        let rig = Rig::new();
        let control = rig.root.path().join("scopes/a");
        fs::create_dir_all(&control).unwrap();
        fs::create_dir_all(account_home(&control)).unwrap();
        let store = rig.fake.store();
        let account = Record {
            skills: BTreeMap::from([("tool".to_owned(), "new".to_owned())]),
            ..Record::default()
        };
        write_record(&control, &store, &account, None).unwrap();
        fs::write(
            control.join(LEGACY_RECORD_FILE),
            r#"{"skills": {"tool": "old", "other": "o"}}"#,
        )
        .unwrap();
        let (merged, legacy) = read_record(&control, &store);
        assert_eq!(merged.skills["tool"], "new");
        assert_eq!(merged.skills["other"], "o");
        assert_eq!(merged.owners["other"], "a");
        assert!(legacy.is_some());
    }

    /// One scope's mirror at a time changes the store.
    #[test]
    fn a_sync_waits_while_another_scope_syncs() {
        let rig = Rig::new();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        let control = rig.root.path().join("control");
        let held = lock_account(&control).unwrap().unwrap();
        let error = rig.sync().unwrap_err();
        assert!(error.contains("another marsh scope"), "{error}");
        assert!(!rig.stored("tool").exists());
        drop(held);
        rig.sync().unwrap();
        assert!(rig.stored("tool").exists());
    }

    #[test]
    fn a_kit_bundled_name_is_never_imported_and_an_earlier_import_goes() {
        let sources = map(&[
            ("leftover", Some("l")),
            ("fresh", Some("f")),
            ("added", Some("mine")),
        ]);
        // `leftover` is what an earlier marsh imported; `added` is an
        // `sbx skills add` copy, which stays (the VM create falls back).
        let store = map(&[("leftover", Some("l")), ("added", Some("from-repo"))]);
        let record = BTreeMap::from([("leftover".to_owned(), "l".to_owned())]);
        let bundled: BTreeMap<_, _> = ["leftover", "fresh", "added"]
            .map(|name| (name.to_owned(), vec!["pix".to_owned()]))
            .into();
        let plan = plan(&sources, &store, &record, &bundled);
        assert!(plan.import.is_empty(), "{plan:?}");
        assert_eq!(plan.remove, ["leftover"]);
        assert_eq!(plan.clashes, [] as [String; 0]);
        assert_eq!(
            plan.bundled
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["added", "fresh", "leftover"]
        );
        assert!(plan.record.is_empty());
    }

    #[test]
    fn first_copy_wins_and_a_skill_needs_skill_md() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        for (folder, body) in [(&a, "from a"), (&b, "from b")] {
            fs::create_dir_all(folder.join("x")).unwrap();
            fs::write(folder.join("x/SKILL.md"), body).unwrap();
        }
        fs::create_dir_all(b.join("no-skill")).unwrap();
        let found = sources(&[a.clone(), b]);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["x"]);
        assert_eq!(found["x"], a.join("x"));
        let before = digest(&read_skill(&a.join("x")).unwrap());
        fs::write(a.join("x/SKILL.md"), "edited").unwrap();
        assert_ne!(digest(&read_skill(&a.join("x")).unwrap()), before);
    }

    /// Stock SBX's skills CLI over a store under `home`. `import` copies
    /// what HOME's `.agents/skills` holds following every link (`cp -RL`):
    /// the worst case, since what stock SBX does with links is not
    /// documented.
    struct FakeSbx {
        home: PathBuf,
        imports: Mutex<usize>,
        /// Runs as the next import starts (a job changing a source then).
        during_import: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// The next import fails after this skill (name, `SKILL.md`)
        /// appeared in the store, as an `sbx skills add` meanwhile would.
        added_during_import: Mutex<Option<(String, String)>>,
        fail_import: Mutex<bool>,
    }

    impl FakeSbx {
        fn store(&self) -> PathBuf {
            self.home.join(".sbx/skills")
        }
    }

    fn copy_following(from: &Path, to: &Path) {
        if fs::metadata(from).unwrap().is_dir() {
            fs::create_dir_all(to).unwrap();
            for entry in fs::read_dir(from).unwrap().flatten() {
                copy_following(&entry.path(), &to.join(entry.file_name()));
            }
        } else {
            fs::copy(from, to).unwrap();
        }
    }

    fn output(stdout: Vec<u8>) -> marsh_runtime::CommandOutput {
        marsh_runtime::CommandOutput {
            exit_code: Some(0),
            stdout,
            stderr: Vec::new(),
        }
    }

    impl marsh_runtime::CommandRunner for FakeSbx {
        fn stock_control_environment(
            &self,
            _program: &Path,
        ) -> Option<(PathBuf, Vec<(std::ffi::OsString, std::ffi::OsString)>)> {
            Some((self.home.clone(), Vec::new()))
        }

        fn run(
            &self,
            invocation: &marsh_runtime::Invocation,
        ) -> std::io::Result<marsh_runtime::CommandOutput> {
            let arguments: Vec<_> = invocation
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            let store = self.store();
            fs::create_dir_all(&store).unwrap();
            match arguments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                ["skills", "ls", "--json"] => {
                    let mut names: Vec<_> = fs::read_dir(&store)
                        .unwrap()
                        .flatten()
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    Ok(output(
                        serde_json::to_vec(&serde_json::json!({"store": store, "skills": names}))
                            .unwrap(),
                    ))
                }
                ["skills", "import", "--help"] => {
                    Ok(output(b"Directories:\n  1. ~/.claude/skills\n".to_vec()))
                }
                ["skills", "import", "--force", "--no-input"] => {
                    if let Some(change) = self.during_import.lock().unwrap().take() {
                        change();
                    }
                    let added = self.added_during_import.lock().unwrap().take();
                    if let Some((name, body)) = &added {
                        fs::create_dir_all(store.join(name)).unwrap();
                        fs::write(store.join(name).join("SKILL.md"), body).unwrap();
                    }
                    if added.is_some() || *self.fail_import.lock().unwrap() {
                        return Ok(marsh_runtime::CommandOutput {
                            exit_code: Some(1),
                            stdout: Vec::new(),
                            stderr: b"import failed".to_vec(),
                        });
                    }
                    *self.imports.lock().unwrap() += 1;
                    let (_, home) = invocation
                        .environment
                        .iter()
                        .find(|(key, _)| key == "HOME")
                        .unwrap();
                    let home = Path::new(home);
                    let into = home.join(".sbx/skills");
                    for entry in fs::read_dir(home.join(".agents/skills")).unwrap().flatten() {
                        let to = into.join(entry.file_name());
                        let _ = fs::remove_dir_all(&to);
                        copy_following(&entry.path(), &to);
                    }
                    Ok(output(Vec::new()))
                }
                ["skills", "rm", "--force", "--no-input", ref names @ ..] => {
                    for name in names {
                        fs::remove_dir_all(store.join(name)).unwrap();
                    }
                    Ok(output(Vec::new()))
                }
                _ => Err(std::io::Error::other(format!(
                    "unexpected sbx {arguments:?}"
                ))),
            }
        }

        fn run_bounded(
            &self,
            invocation: &marsh_runtime::Invocation,
            _timeout: Duration,
        ) -> std::io::Result<marsh_runtime::CommandOutput> {
            self.run(invocation)
        }

        fn spawn_attached(
            &self,
            _invocation: &marsh_runtime::Invocation,
        ) -> std::io::Result<marsh_runtime::Attachment> {
            Err(std::io::Error::other("unexpected attached sbx"))
        }
    }

    struct Rig {
        root: tempfile::TempDir,
        fake: std::sync::Arc<FakeSbx>,
        sbx: StockSbx,
    }

    impl Rig {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let fake = std::sync::Arc::new(FakeSbx {
                home: root.path().join("stock-home"),
                imports: Mutex::new(0),
                during_import: Mutex::new(None),
                added_during_import: Mutex::new(None),
                fail_import: Mutex::new(false),
            });
            fs::create_dir_all(root.path().join("control")).unwrap();
            fs::create_dir_all(root.path().join("folder")).unwrap();
            let sbx = StockSbx::new("sbx", fake.clone());
            Self { root, fake, sbx }
        }

        fn folder(&self) -> PathBuf {
            self.root.path().join("folder")
        }

        fn sync(&self) -> Result<Synced, String> {
            self.sync_in(&self.root.path().join("control"))
        }

        /// A sync by the scope whose control home is `control`.
        fn sync_in(&self, control: &Path) -> Result<Synced, String> {
            fs::create_dir_all(control).unwrap();
            sync(&self.sbx, &[self.folder()], control, &BTreeMap::new())
        }

        fn stored(&self, name: &str) -> PathBuf {
            self.fake.store().join(name)
        }
    }

    /// Every regular file under `root`, with its bytes; panics on a link.
    fn tree(root: &Path) -> BTreeMap<String, String> {
        let mut found = BTreeMap::new();
        let mut pending = vec![root.to_owned()];
        while let Some(path) = pending.pop() {
            for entry in fs::read_dir(&path).unwrap().flatten() {
                let kind = entry.file_type().unwrap();
                assert!(
                    !kind.is_symlink(),
                    "a link in the store: {:?}",
                    entry.path()
                );
                if kind.is_dir() {
                    pending.push(entry.path());
                } else {
                    let relative = entry.path().strip_prefix(root).unwrap().to_owned();
                    found.insert(
                        relative.to_string_lossy().into_owned(),
                        fs::read_to_string(entry.path()).unwrap(),
                    );
                }
            }
        }
        found
    }

    #[test]
    fn a_link_out_of_a_skill_folder_never_reaches_the_store() {
        let rig = Rig::new();
        let secret = rig.root.path().join("id_ed25519");
        fs::write(&secret, "PRIVATE KEY").unwrap();
        let skill = rig.folder().join("leaky");
        fs::create_dir_all(skill.join("docs")).unwrap();
        fs::write(skill.join("SKILL.md"), "leaky").unwrap();
        std::os::unix::fs::symlink(&secret, skill.join("docs/key")).unwrap();
        std::os::unix::fs::symlink("../../id_ed25519", skill.join("relative")).unwrap();
        let other = rig.folder().join("other-outside");
        fs::create_dir_all(&other).unwrap();
        std::os::unix::fs::symlink(&secret, other.join("SKILL.md")).unwrap();

        let synced = rig.sync().unwrap();
        assert_eq!(synced.refused.len(), 2, "{synced:?}");
        assert!(synced.refused[0].contains("skill leaky"), "{synced:?}");
        assert!(
            synced.refused[1].contains("skill other-outside"),
            "{synced:?}"
        );
        assert!(!rig.stored("leaky").exists());
        assert!(!rig.stored("other-outside").exists());
        assert_eq!(*rig.fake.imports.lock().unwrap(), 0);
    }

    #[test]
    fn a_link_planted_later_keeps_the_last_good_copy() {
        let rig = Rig::new();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "good").unwrap();
        rig.sync().unwrap();
        fs::write(skill.join("SKILL.md"), "edited").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", skill.join("hosts")).unwrap();

        let synced = rig.sync().unwrap();
        assert_eq!(synced.refused.len(), 1, "{synced:?}");
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "good".to_owned())])
        );
        assert_eq!(*rig.fake.imports.lock().unwrap(), 1);
    }

    /// A skill folder swapped for a link to another folder after `sources()`
    /// saw its `SKILL.md` is not read through that link.
    #[test]
    fn a_folder_swapped_after_the_scan_is_not_read() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("folder");
        let skill = folder.join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "tool").unwrap();
        let ssh = root.path().join("ssh");
        fs::create_dir_all(&ssh).unwrap();
        fs::write(ssh.join("id_ed25519"), "PRIVATE KEY").unwrap();
        let found = sources(std::slice::from_ref(&folder));
        fs::remove_dir_all(&skill).unwrap();
        std::os::unix::fs::symlink(&ssh, &skill).unwrap();
        let error = read_skill(&found["tool"]).unwrap_err();
        assert!(error.contains("SKILL.md"), "{error}");
        // A link to SKILL.md does not make a folder a skill either.
        std::os::unix::fs::symlink(skill.join("id_ed25519"), ssh.join("SKILL.md")).unwrap();
        assert!(read_skill(&found["tool"]).is_err());
    }

    /// A skill folder nested deeper than any skill needs is refused, without
    /// recursing (a deep tree once overflowed the mirror thread's stack).
    #[test]
    fn a_deep_tree_is_refused() {
        use rustix::fs::{Mode, OFlags};
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("deep");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "deep").unwrap();
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let mut folder = rustix::fs::open(&skill, flags, Mode::empty()).unwrap();
        for _ in 0..2000 {
            rustix::fs::mkdirat(&folder, "d", Mode::from_raw_mode(0o755)).unwrap();
            folder = rustix::fs::openat(&folder, "d", flags, Mode::empty()).unwrap();
        }
        let error = read_skill(&skill).unwrap_err();
        assert!(error.contains("folder levels"), "{error}");
    }

    #[test]
    fn a_link_target_must_stay_in_the_folder() {
        let at = |folder: &str, target: &str| inside(Path::new(folder), Path::new(target));
        assert_eq!(at("a", "../x"), Some(PathBuf::from("x")));
        assert_eq!(at("a", "./b/../c"), Some(PathBuf::from("a/c")));
        assert_eq!(at("", "../x"), None);
        assert_eq!(at("a", "b/../../../x"), None);
        assert_eq!(at("a", "/etc/hosts"), None);
    }

    /// Every change the mirror makes to the store (a job's edit of a skill
    /// in its project included) leaves the copy it replaced, and a diff.
    #[test]
    fn each_store_change_keeps_the_copy_it_replaced_and_a_diff() {
        let rig = Rig::new();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one\ntwo\n").unwrap();
        rig.sync().unwrap();
        fs::write(skill.join("SKILL.md"), "one\nthree\n").unwrap();
        rig.sync().unwrap();
        fs::remove_dir_all(&skill).unwrap();
        rig.sync().unwrap();

        let changes = history(&rig.root.path().join("control"));
        assert_eq!(
            changes
                .iter()
                .map(|change| (change.name.as_str(), change.change.as_str()))
                .collect::<Vec<_>>(),
            [("tool", "removed"), ("tool", "updated"), ("tool", "added")]
        );
        let updated = Path::new(&changes[1].diff);
        let diff = fs::read_to_string(updated).unwrap();
        assert!(diff.contains("-two\n+three\n"), "{diff}");
        let before = updated.parent().unwrap().join("before");
        assert_eq!(
            tree(&before),
            BTreeMap::from([("SKILL.md".to_owned(), "one\ntwo\n".to_owned())])
        );
        let removed = Path::new(&changes[0].diff).parent().unwrap().join("before");
        assert_eq!(
            tree(&removed),
            BTreeMap::from([("SKILL.md".to_owned(), "one\nthree\n".to_owned())])
        );
        assert!(
            !Path::new(&changes[2].diff)
                .parent()
                .unwrap()
                .join("before")
                .exists()
        );
    }

    #[test]
    fn the_history_keeps_the_newest_changes_of_each_skill() {
        let root = tempfile::tempdir().unwrap();
        // The history is beside the control home: keep it in `root`.
        let control = root.path().join("control");
        let files = |body: &str| Files::from([("SKILL.md".into(), (false, body.into()))]);
        let keep = |name: &str, before: Option<&Files>, after: &Files| {
            let store = Path::new("/store");
            let entry = keep_history(&control, store, name, before, Some(after)).unwrap();
            publish_history(&control, store, &[entry]);
        };
        keep("other", None, &files("o"));
        for round in 0..=HISTORY_KEEP {
            keep("tool", Some(&files(&round.to_string())), &files("x"));
        }
        let changes = history(&control);
        let tool: Vec<_> = changes.iter().filter(|c| c.name == "tool").collect();
        assert_eq!(tool.len(), HISTORY_KEEP);
        assert_eq!(changes.iter().filter(|c| c.name == "other").count(), 1);
        let oldest = Path::new(&tool[HISTORY_KEEP - 1].diff).parent().unwrap();
        assert_eq!(
            fs::read_to_string(oldest.join("before/SKILL.md")).unwrap(),
            "1"
        );
    }

    /// When the history cannot be written the store is not changed.
    #[test]
    fn an_unwritable_history_leaves_the_store_alone() {
        let rig = Rig::new();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync().unwrap();
        let history = history_dir(&rig.root.path().join("control"), &rig.fake.store());
        fs::remove_dir_all(&history).unwrap();
        fs::write(&history, "not a folder").unwrap();
        fs::write(skill.join("SKILL.md"), "two").unwrap();

        let error = rig.sync().unwrap_err();
        assert!(error.contains("cannot keep the skills history"), "{error}");
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "one".to_owned())])
        );
        assert_eq!(*rig.fake.imports.lock().unwrap(), 1);
        fs::remove_dir_all(&skill).unwrap();
        assert!(rig.sync().is_err());
        assert!(rig.stored("tool").exists());
    }

    /// A failing import leaves no history entry, however often it is
    /// retried, so the real rollback points are never pushed out.
    #[test]
    fn a_failed_import_adds_no_history() {
        let rig = Rig::new();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync().unwrap();
        fs::write(skill.join("SKILL.md"), "two").unwrap();
        *rig.fake.fail_import.lock().unwrap() = true;
        for _ in 0..=HISTORY_KEEP {
            assert!(rig.sync().is_err());
        }
        let control = rig.root.path().join("control");
        let changes = |history: Vec<SkillChange>| {
            history
                .into_iter()
                .map(|change| change.change)
                .collect::<Vec<_>>()
        };
        assert_eq!(changes(history(&control)), ["added"]);
        assert_eq!(
            fs::read_dir(history_dir(&control, &rig.fake.store()))
                .unwrap()
                .count(),
            1,
            "no pending entries left"
        );
        *rig.fake.fail_import.lock().unwrap() = false;
        rig.sync().unwrap();
        assert_eq!(changes(history(&control)), ["updated", "added"]);
    }

    #[test]
    fn a_large_file_is_named_not_diffed() {
        let big = |byte: u8| {
            Files::from([(
                "data.txt".into(),
                (
                    false,
                    vec![byte; usize::try_from(MAX_DIFF_FILE).unwrap() + 1],
                ),
            )])
        };
        let diff = diff("tool", Some(&big(b'a')), Some(&big(b'b')));
        assert_eq!(diff, "large file changed: tool/data.txt\n");
    }

    #[test]
    fn a_dangling_skill_md_link_keeps_the_last_good_copy() {
        let rig = Rig::new();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "good").unwrap();
        rig.sync().unwrap();
        fs::remove_file(skill.join("SKILL.md")).unwrap();
        std::os::unix::fs::symlink("/nonexistent/SKILL.md", skill.join("SKILL.md")).unwrap();

        let synced = rig.sync().unwrap();
        assert_eq!(synced.refused.len(), 1, "{synced:?}");
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "good".to_owned())])
        );
    }

    /// A skill folder and, beside it, a folder that also looks like a
    /// skill but holds a secret: what a job would point a swap at.
    fn swap_rig() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("tool");
        fs::create_dir_all(skill.join("docs")).unwrap();
        fs::write(skill.join("SKILL.md"), "tool").unwrap();
        fs::write(skill.join("docs/notes"), "notes").unwrap();
        let other = root.path().join("other");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("SKILL.md"), "other").unwrap();
        fs::write(other.join("notes"), "PRIVATE KEY").unwrap();
        (root, skill, other)
    }

    fn swap_for_link(path: &Path, target: &Path) {
        if fs::symlink_metadata(path).unwrap().is_dir() {
            fs::rename(path, path.with_extension("moved")).unwrap();
        } else {
            fs::remove_file(path).unwrap();
        }
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    /// Each open in a read follows no link: a folder or file swapped for a
    /// link at that moment fails the read instead of leading elsewhere.
    #[test]
    fn a_swap_at_any_step_of_a_read_fails_it() {
        for (step, at, target) in [
            (Step::Resolved, "", "other"),
            (Step::OpenFolder, "docs", "other"),
            (Step::OpenFile, "docs/notes", "other/notes"),
        ] {
            let (root, skill, other) = swap_rig();
            let target = root.path().join(target);
            let mut swapped = false;
            let read = read_skill_with(&skill, true, &mut |now, path| {
                let ours = if now == Step::Resolved {
                    path == fs::canonicalize(&skill).unwrap()
                } else {
                    path == Path::new(at)
                };
                if now == step && ours && !swapped {
                    swapped = true;
                    let path = if at.is_empty() {
                        skill.clone()
                    } else {
                        skill.join(at)
                    };
                    swap_for_link(&path, &target);
                }
            });
            assert!(swapped, "{step:?}");
            assert!(read.is_err(), "{step:?}: {read:?}");
            drop(other);
        }
    }

    /// A file swapped for a FIFO between its stat and its open is caught by
    /// the type check on the opened file.
    #[test]
    fn a_file_swapped_for_a_fifo_before_open_fails_the_read() {
        let (_root, skill, _other) = swap_rig();
        let mut swapped = false;
        let read = read_skill_with(&skill, true, &mut |now, path| {
            if now == Step::OpenFile && path == Path::new("docs/notes") && !swapped {
                swapped = true;
                fs::remove_file(skill.join("docs/notes")).unwrap();
                nix::unistd::mkfifo(
                    &skill.join("docs/notes"),
                    nix::sys::stat::Mode::from_bits_truncate(0o644),
                )
                .unwrap();
            }
        });
        assert!(swapped);
        let error = read.unwrap_err();
        assert!(error.contains("changed while read"), "{error}");
    }

    /// A link planted in a source while stock SBX imports never reaches the
    /// store: the import reads a copy, not the source.
    #[test]
    fn a_link_planted_during_the_import_never_reaches_the_store() {
        let rig = Rig::new();
        let secret = rig.root.path().join("id_ed25519");
        fs::write(&secret, "PRIVATE KEY").unwrap();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "tool").unwrap();
        let plant = skill.join("key");
        *rig.fake.during_import.lock().unwrap() = Some(Box::new(move || {
            std::os::unix::fs::symlink(&secret, &plant).unwrap();
        }));
        rig.sync().unwrap();
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "tool".to_owned())])
        );
    }

    /// A refused skill is named in `marsh status`.
    #[test]
    fn a_refused_skill_is_listed_in_status() {
        let rig = Rig::new();
        let home = rig.root.path().join("home");
        let skill = home.join(".claude/skills/leaky");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "leaky").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", skill.join("hosts")).unwrap();
        let mirror = Mirror {
            scan_home: Some(home),
            ..Mirror::default()
        };
        let scope = rig.root.path().join("scope");
        fs::create_dir_all(&scope).unwrap();
        let control = rig.root.path().join("control");
        mirror.pass(&rig.sbx, &scope, &control, &Bundled::default());
        let errors = mirror.status(&scope, &control).errors;
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].starts_with("skill leaky not imported: hosts is a link"),
            "{errors:?}"
        );
    }

    #[test]
    fn folders_count_toward_the_entry_cap() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("SKILL.md"), "wide").unwrap();
        for index in 0..MAX_SKILL_FILES {
            fs::create_dir(root.path().join(index.to_string())).unwrap();
        }
        assert_eq!(read_skill(root.path()).unwrap_err(), "too many files");
    }

    #[test]
    fn a_source_file_with_other_hard_links_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let skill = root.path().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "tool").unwrap();
        fs::write(root.path().join("secret"), "PRIVATE KEY").unwrap();
        fs::hard_link(root.path().join("secret"), skill.join("notes")).unwrap();
        let error = read_source(&skill).unwrap_err();
        assert_eq!(error, "notes has other hard links");
        assert!(read_skill(&skill).is_ok());
    }

    fn skill_md(body: &str) -> Files {
        Files::from([("SKILL.md".into(), (false, body.into()))])
    }

    /// A crash after the import but before the entry was published keeps
    /// the rollback copy, and the change stays the mirror's.
    #[test]
    fn a_crash_after_the_import_keeps_the_rollback_copy() {
        let rig = Rig::new();
        let control = rig.root.path().join("control");
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync().unwrap();
        // The pass that crashed: entry prepared, store changed, nothing more.
        keep_history(
            &control,
            &rig.fake.store(),
            "tool",
            Some(&skill_md("one")),
            Some(&skill_md("two")),
        )
        .unwrap();
        fs::write(rig.stored("tool").join("SKILL.md"), "two").unwrap();
        fs::write(skill.join("SKILL.md"), "two").unwrap();

        rig.sync().unwrap();
        let changes = history(&control);
        assert_eq!(
            changes
                .iter()
                .map(|c| c.change.as_str())
                .collect::<Vec<_>>(),
            ["updated", "added"]
        );
        let before = Path::new(&changes[0].diff).parent().unwrap().join("before");
        assert_eq!(fs::read_to_string(before.join("SKILL.md")).unwrap(), "one");
        fs::write(skill.join("SKILL.md"), "three").unwrap();
        assert_eq!(rig.sync().unwrap(), Synced::default());
        assert_eq!(
            tree(&rig.stored("tool")),
            BTreeMap::from([("SKILL.md".to_owned(), "three".to_owned())])
        );
    }

    /// A crash before the import drops its prepared entry, and a
    /// half-written one, without touching published ones.
    #[test]
    fn a_crash_before_the_import_leaves_no_entry() {
        let rig = Rig::new();
        let control = rig.root.path().join("control");
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync().unwrap();
        keep_history(
            &control,
            &rig.fake.store(),
            "tool",
            Some(&skill_md("one")),
            Some(&skill_md("two")),
        )
        .unwrap();
        let half = history_dir(&control, &rig.fake.store()).join(".tmp-0000000000001-tool");
        fs::create_dir_all(half.join("before")).unwrap();

        // The store still holds "one": the change was never made.
        rig.sync().unwrap();
        assert_eq!(
            history(&control)
                .iter()
                .map(|c| c.change.as_str())
                .collect::<Vec<_>>(),
            ["added"]
        );
        assert_eq!(
            fs::read_dir(history_dir(&control, &rig.fake.store()))
                .unwrap()
                .count(),
            1
        );
    }

    fn changes(control: &Path) -> Vec<String> {
        history(control).iter().map(|c| c.change.clone()).collect()
    }

    /// A rig whose store holds `tool` = "one", imported by the mirror.
    fn imported_rig() -> (Rig, PathBuf, PathBuf) {
        let rig = Rig::new();
        let control = rig.root.path().join("control");
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        rig.sync().unwrap();
        (rig, control, skill)
    }

    fn stored_text(rig: &Rig) -> String {
        fs::read_to_string(rig.stored("tool").join("SKILL.md")).unwrap()
    }

    /// Crash after a removal, before its entry was published (review
    /// round 3 probe).
    #[test]
    fn a_crash_after_a_removal_publishes_it_and_forgets_the_skill() {
        let (rig, control, skill) = imported_rig();
        let store = rig.fake.store();
        keep_history(&control, &store, "tool", Some(&skill_md("one")), None).unwrap();
        fs::remove_dir_all(rig.stored("tool")).unwrap();
        fs::remove_dir_all(&skill).unwrap();
        rig.sync().unwrap();
        assert_eq!(changes(&control), ["removed", "added"]);
        let (record, _) = read_record(&control, &store);
        assert!(record.skills.is_empty(), "{record:?}");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "again").unwrap();
        rig.sync().unwrap();
        assert_eq!(stored_text(&rig), "again");
    }

    /// Crash after the entry was published but before the record was
    /// written: the record order makes this the pending case, which the
    /// next pass settles (review round 3 probes, Claude and GPT).
    #[test]
    fn a_crash_between_the_import_and_the_record_keeps_ownership() {
        let (rig, control, skill) = imported_rig();
        let store = rig.fake.store();
        // What sync leaves at that point now: the entry still pending.
        keep_history(
            &control,
            &store,
            "tool",
            Some(&skill_md("one")),
            Some(&skill_md("two")),
        )
        .unwrap();
        fs::write(rig.stored("tool").join("SKILL.md"), "two").unwrap();
        fs::write(skill.join("SKILL.md"), "two").unwrap();
        rig.sync().unwrap();
        fs::write(skill.join("SKILL.md"), "three").unwrap();
        assert_eq!(rig.sync().unwrap(), Synced::default());
        assert_eq!(stored_text(&rig), "three");
        assert_eq!(changes(&control), ["updated", "updated", "added"]);
    }

    /// When the record cannot be written after an import, the entry stays
    /// pending (it is published only after the record), so the next pass
    /// still finds the change and keeps the skill the mirror's.
    #[test]
    fn an_unwritten_record_leaves_the_entry_pending() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return; // Root writes a read-only folder anyway.
        }
        let (rig, control, skill) = imported_rig();
        let home = account_home(&control);
        fs::write(skill.join("SKILL.md"), "two").unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap();
        let failed = rig.sync();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err(), "{failed:?}");
        assert_eq!(stored_text(&rig), "two");
        assert_eq!(changes(&control), ["added"]);
        rig.sync().unwrap();
        assert_eq!(changes(&control), ["updated", "added"]);
        fs::write(skill.join("SKILL.md"), "three").unwrap();
        assert_eq!(rig.sync().unwrap(), Synced::default());
        assert_eq!(stored_text(&rig), "three");
    }

    /// A failure after a successful import (here, preparing a removal's
    /// history) keeps the import's record (review round 3 probe).
    #[test]
    fn a_failure_after_the_import_keeps_its_record() {
        let rig = Rig::new();
        let (tool, gone) = (rig.folder().join("tool"), rig.folder().join("gone"));
        for (folder, body) in [(&tool, "one"), (&gone, "g")] {
            fs::create_dir_all(folder).unwrap();
            fs::write(folder.join("SKILL.md"), body).unwrap();
        }
        rig.sync().unwrap();
        fs::remove_dir_all(&gone).unwrap();
        fs::write(tool.join("SKILL.md"), "two").unwrap();
        let victim = rig.stored("gone").join("SKILL.md");
        *rig.fake.during_import.lock().unwrap() = Some(Box::new(move || {
            fs::remove_file(&victim).unwrap();
        }));
        let error = rig.sync().unwrap_err();
        assert!(error.contains("store's copy of gone"), "{error}");
        fs::write(tool.join("SKILL.md"), "three").unwrap();
        let synced = rig.sync().unwrap();
        assert_eq!(synced.clashes, [] as [String; 0], "{synced:?}");
    }

    /// Recovery never takes a user's replacement of a skill as the
    /// mirror's, so never deletes it (review round 3 probe, GPT).
    #[test]
    fn recovery_never_adopts_a_users_replacement() {
        let (rig, control, skill) = imported_rig();
        keep_history(
            &control,
            &rig.fake.store(),
            "tool",
            Some(&skill_md("one")),
            Some(&skill_md("two")),
        )
        .unwrap();
        fs::write(rig.stored("tool").join("SKILL.md"), "user-added").unwrap();
        fs::remove_dir_all(&skill).unwrap();
        rig.sync().unwrap();
        assert_eq!(stored_text(&rig), "user-added");
        // The rollback copy is kept as evidence.
        assert_eq!(changes(&control), ["uncertain", "added"]);
    }

    /// One store's recovery never settles another store's pending entry
    /// (review round 3 probe, GPT).
    #[test]
    fn another_stores_pass_leaves_this_stores_history_alone() {
        let a = Rig::new();
        let b = Rig::new();
        let control_a = a.root.path().join("scopes/a");
        let control_b = a.root.path().join("scopes/b");
        let skill = a.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        a.sync_in(&control_a).unwrap();
        let pending = keep_history(
            &control_a,
            &a.fake.store(),
            "tool",
            Some(&skill_md("one")),
            Some(&skill_md("two")),
        )
        .unwrap();
        fs::write(a.stored("tool").join("SKILL.md"), "two").unwrap();
        fs::write(skill.join("SKILL.md"), "two").unwrap();
        b.sync_in(&control_b).unwrap();
        assert!(pending.exists(), "store B settled store A's entry");
        a.sync_in(&control_a).unwrap();
        assert_eq!(
            history(&control_a)
                .iter()
                .filter(|c| c.change == "updated")
                .count(),
            1
        );
    }

    /// A store copy recovery cannot read is uncertain: the entry is kept,
    /// and the record does not follow.
    #[test]
    fn recovery_keeps_an_entry_when_the_store_copy_is_unreadable() {
        let (rig, control, _skill) = imported_rig();
        let store = rig.fake.store();
        keep_history(
            &control,
            &store,
            "tool",
            Some(&skill_md("one")),
            Some(&skill_md("two")),
        )
        .unwrap();
        fs::remove_file(rig.stored("tool").join("SKILL.md")).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", rig.stored("tool").join("SKILL.md")).unwrap();
        rig.sync().unwrap();
        assert_eq!(changes(&control), ["uncertain", "added"]);
        // Not taken as the mirror's: it shows as a clash, and stays.
        let (record, _) = read_record(&control, &store);
        assert!(!record.skills.contains_key("tool"), "{record:?}");
        assert!(rig.stored("tool").join("SKILL.md").is_symlink());
    }

    /// The record follows a change made before a crash only when it owned
    /// what the change replaced.
    #[test]
    fn recovery_does_not_take_a_skill_the_record_did_not_own() {
        let mut record = Record::default();
        let change = |change: &str, before: Option<&str>, after: Option<&str>| HistoryEntry {
            name: "tool".into(),
            change: change.into(),
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
            ..HistoryEntry::default()
        };
        follow(
            &mut record,
            &change("updated", Some("1"), Some("2")),
            &Settled::Made,
        );
        assert!(record.skills.is_empty());
        follow(
            &mut record,
            &change("removed", Some("1"), None),
            &Settled::Made,
        );
        assert!(record.skills.is_empty());
        // A new skill needs proof the import put it there, and its scope.
        let proven = HistoryEntry {
            scope: Some("b".into()),
            imported: Some(true),
            ..change("added", None, Some("2"))
        };
        follow(&mut record, &proven, &Settled::Uncertain);
        assert!(record.skills.is_empty());
        follow(
            &mut record,
            &change("added", None, Some("2")),
            &Settled::Made,
        );
        assert!(record.skills.is_empty());
        let failed = HistoryEntry {
            imported: Some(false),
            ..proven.clone()
        };
        follow(&mut record, &failed, &Settled::Made);
        assert!(record.skills.is_empty());
        follow(&mut record, &proven, &Settled::Made);
        assert_eq!(record.skills["tool"], "2");
        assert_eq!(record.owners["tool"], "b");
        record.skills.insert("tool".into(), "other".into());
        follow(
            &mut record,
            &change("updated", Some("1"), Some("2")),
            &Settled::Made,
        );
        assert_eq!(record.skills["tool"], "other");
        follow(
            &mut record,
            &change("removed", Some("1"), None),
            &Settled::Made,
        );
        assert_eq!(record.skills["tool"], "other");
    }

    /// The settling rules, case by case.
    #[test]
    fn recovery_settles_by_what_the_store_holds() {
        let change = |change: &str, before: Option<&str>, after: Option<&str>| HistoryEntry {
            name: "tool".into(),
            change: change.into(),
            at: 0,
            before: before.map(str::to_owned),
            after: after.map(str::to_owned),
            ..HistoryEntry::default()
        };
        let now = |digest: &str| Some(digest.to_owned());
        let update = change("updated", Some("1"), Some("2"));
        assert_eq!(settle(&update, Some(&now("1"))), Settled::NotMade);
        assert_eq!(settle(&update, Some(&now("2"))), Settled::Made);
        assert_eq!(settle(&update, Some(&now("3"))), Settled::Uncertain);
        assert_eq!(settle(&update, Some(&None)), Settled::Uncertain);
        assert_eq!(settle(&update, None), Settled::Uncertain);
        let add = change("added", None, Some("2"));
        assert_eq!(settle(&add, None), Settled::NotMade);
        assert_eq!(settle(&add, Some(&now("2"))), Settled::Made);
        assert_eq!(settle(&add, Some(&now("half"))), Settled::Uncertain);
        let remove = change("removed", Some("1"), None);
        assert_eq!(settle(&remove, None), Settled::Made);
        assert_eq!(settle(&remove, Some(&now("1"))), Settled::NotMade);
        assert_eq!(settle(&remove, Some(&now("3"))), Settled::Uncertain);
    }

    /// A crash during a new skill's import that left a partial copy: the
    /// copy is never taken as the mirror's (it shows as a clash), and the
    /// entry is kept.
    #[test]
    fn a_half_imported_new_skill_is_not_trusted() {
        let rig = Rig::new();
        let control = rig.root.path().join("control");
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "whole").unwrap();
        fs::create_dir_all(rig.fake.store()).unwrap();
        keep_history(
            &control,
            &rig.fake.store(),
            "tool",
            None,
            Some(&skill_md("whole")),
        )
        .unwrap();
        fs::create_dir_all(rig.stored("tool")).unwrap();
        fs::write(rig.stored("tool").join("SKILL.md"), "who").unwrap();
        let synced = rig.sync().unwrap();
        assert_eq!(synced.clashes, ["tool"]);
        assert_eq!(stored_text(&rig), "who");
        assert_eq!(changes(&control), ["uncertain"]);
    }

    /// An entry is whole before it is pending: it is written under a
    /// temporary name and renamed.
    #[test]
    fn a_history_entry_is_written_whole_before_it_is_pending() {
        let root = tempfile::tempdir().unwrap();
        let control = root.path().join("control");
        let store = Path::new("/store");
        let mut seen = None;
        let entry = keep_history_with(
            &control,
            store,
            "tool",
            Some(&skill_md("one")),
            Some(&skill_md("two")),
            &mut |written| {
                let names: Vec<_> = fs::read_dir(history_dir(&control, store))
                    .unwrap()
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect();
                seen = Some((names, written.join("change.json").exists()));
            },
        )
        .unwrap();
        let (names, whole) = seen.unwrap();
        assert_eq!(names.len(), 1);
        assert!(names[0].starts_with(".tmp-"), "{names:?}");
        assert!(whole);
        assert!(
            entry
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".pending-")
        );
    }

    /// Past its deadline the line diff is coarser, but still correct: one
    /// hunk spanning both changes instead of two.
    #[test]
    fn the_diff_stops_refining_at_its_deadline() {
        let lines = |changed: &[usize]| {
            let text: String = (0..2000)
                .map(|n| {
                    if changed.contains(&n) {
                        format!("changed {n}\n")
                    } else {
                        format!("line {n}\n")
                    }
                })
                .collect();
            Files::from([("SKILL.md".into(), (false, text.into_bytes()))])
        };
        let (old, new) = (lines(&[]), lines(&[100, 1900]));
        let fine = diff("tool", Some(&old), Some(&new));
        assert_eq!(fine.matches("@@ ").count(), 2, "{fine}");
        let coarse = diff_until("tool", Some(&old), Some(&new), Instant::now());
        assert!(coarse.contains("-line 1000\n"), "{coarse}");
    }

    #[test]
    fn recovery_keeps_the_importing_scope() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return; // Root writes a read-only folder anyway.
        }
        let rig = Rig::new();
        let a = rig.root.path().join("scopes/a");
        let b = rig.root.path().join("scopes/b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::create_dir_all(account_home(&b)).unwrap();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        // Drive B's real sync: successful import, but record writing fails.
        let home = account_home(&b);
        let fail_home = home.clone();
        *rig.fake.during_import.lock().unwrap() = Some(Box::new(move || {
            fs::set_permissions(&fail_home, fs::Permissions::from_mode(0o500)).unwrap();
        }));
        let failed = rig.sync_in(&b);
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err());
        assert_eq!(stored_text(&rig), "one");
        assert_eq!(history(&b), []);
        // Scope A recovers first; its Kit bundles that same name.
        let bundled = BTreeMap::from([("tool".to_owned(), vec!["pix".to_owned()])]);
        sync(&rig.sbx, &[rig.folder()], &a, &bundled).unwrap();
        assert!(
            rig.stored("tool").exists(),
            "recovery relabelled B's import as A's and removed it"
        );
        let (record, _) = read_record(&a, &rig.fake.store());
        assert_eq!(record.owners.get("tool").map(String::as_str), Some("b"));
    }

    #[test]
    fn a_failed_import_then_a_failed_record_never_adopts_a_user_add() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return; // Root writes a read-only folder anyway.
        }
        let rig = Rig::new();
        let control = rig.root.path().join("control");
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "mine").unwrap();
        // Let preparation work, but fail the record write after sbx returns.
        let home = account_home(&control);
        let fail_home = home.clone();
        *rig.fake.during_import.lock().unwrap() = Some(Box::new(move || {
            fs::set_permissions(&fail_home, fs::Permissions::from_mode(0o500)).unwrap();
        }));
        *rig.fake.added_during_import.lock().unwrap() = Some(("tool".into(), "mine".into()));
        let failed = rig.sync();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err());
        assert_eq!(stored_text(&rig), "mine");
        assert!(history(&control).is_empty(), "entry must still be pending");
        fs::remove_dir_all(&skill).unwrap();
        rig.sync().unwrap();
        assert!(
            rig.stored("tool").exists(),
            "recovery claimed and deleted a user add after a failed import"
        );
    }

    #[test]
    fn recovery_leaves_entries_pending_until_its_record_is_written() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return; // Root writes a read-only folder anyway.
        }
        let (rig, control, skill) = imported_rig();
        let home = account_home(&control);
        fs::write(skill.join("SKILL.md"), "two").unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap();
        let first = rig.sync();
        let first_history = changes(&control);
        // A second pass now takes the separate recovery branch.
        let recovered = rig.sync();
        let recovered_history = changes(&control);
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(first.is_err());
        assert_eq!(first_history, ["added"]);
        assert!(recovered.is_err());
        assert_eq!(
            recovered_history,
            ["added"],
            "recovery published before its record could be saved"
        );
        fs::write(skill.join("SKILL.md"), "three").unwrap();
        assert_eq!(rig.sync().unwrap(), Synced::default());
        assert_eq!(stored_text(&rig), "three");
    }

    #[test]
    fn a_failed_import_then_a_failed_publication_never_adopts_a_user_add() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return; // Root writes a read-only folder anyway.
        }
        let rig = Rig::new();
        let control = rig.root.path().join("control");
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "mine").unwrap();
        let history_home = history_dir(&control, &rig.fake.store());
        let fail_history = history_home.clone();
        *rig.fake.during_import.lock().unwrap() = Some(Box::new(move || {
            fs::set_permissions(&fail_history, fs::Permissions::from_mode(0o500)).unwrap();
        }));
        *rig.fake.added_during_import.lock().unwrap() = Some(("tool".into(), "mine".into()));
        let failed = rig.sync();
        fs::set_permissions(&history_home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.unwrap_err().contains("sbx skills import failed"));
        let (record, _) = read_record(&control, &rig.fake.store());
        assert!(
            record.skills.is_empty(),
            "the completed failed-import pass did not own it"
        );
        assert_eq!(history(&control), []);
        fs::remove_dir_all(&skill).unwrap();
        rig.sync().unwrap();
        assert!(
            rig.stored("tool").exists(),
            "a failed history rename made recovery adopt and delete a user add"
        );
    }

    /// The record forgets a removed skill, so a user's identical add
    /// afterwards stays (review round 4 probe, Claude).
    #[test]
    fn an_identical_add_after_a_removal_is_kept() {
        let (rig, _control, skill) = imported_rig();
        fs::remove_dir_all(&skill).unwrap();
        rig.sync().unwrap();
        assert!(!rig.stored("tool").exists());
        fs::create_dir_all(rig.stored("tool")).unwrap();
        fs::write(rig.stored("tool").join("SKILL.md"), "one").unwrap();
        rig.sync().unwrap();
        assert!(
            rig.stored("tool").exists(),
            "user's identical skill removed"
        );
    }

    /// A store copy changed after the plan (a user's edit during the
    /// pass) is not removed: it is no longer what the mirror imported.
    #[test]
    fn a_store_copy_edited_during_the_pass_is_not_removed() {
        let rig = Rig::new();
        let (tool, gone) = (rig.folder().join("tool"), rig.folder().join("gone"));
        for (folder, body) in [(&tool, "one"), (&gone, "g")] {
            fs::create_dir_all(folder).unwrap();
            fs::write(folder.join("SKILL.md"), body).unwrap();
        }
        rig.sync().unwrap();
        fs::remove_dir_all(&gone).unwrap();
        fs::write(tool.join("SKILL.md"), "two").unwrap();
        let edited = rig.stored("gone").join("SKILL.md");
        *rig.fake.during_import.lock().unwrap() = Some(Box::new(move || {
            fs::write(&edited, "the user's edit").unwrap();
        }));
        rig.sync().unwrap();
        assert_eq!(
            fs::read_to_string(rig.stored("gone").join("SKILL.md")).unwrap(),
            "the user's edit"
        );
    }

    /// A removal whose record cannot be written stays pending, and the
    /// next pass publishes it and forgets the skill.
    #[test]
    fn an_unwritten_record_leaves_a_removal_pending() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return; // Root writes a read-only folder anyway.
        }
        let (rig, control, skill) = imported_rig();
        let home = account_home(&control);
        fs::remove_dir_all(&skill).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o500)).unwrap();
        let failed = rig.sync();
        let after_failure = changes(&control);
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err(), "{failed:?}");
        assert!(!rig.stored("tool").exists());
        assert_eq!(after_failure, ["added"], "published before the record");
        rig.sync().unwrap();
        assert_eq!(changes(&control), ["removed", "added"]);
        let (record, _) = read_record(&control, &rig.fake.store());
        assert!(record.skills.is_empty(), "{record:?}");
    }

    /// A new skill whose import succeeded but whose record was not
    /// written is the mirror's after recovery: the entry says so.
    #[test]
    fn a_proven_new_import_is_recovered_as_the_mirrors() {
        use std::os::unix::fs::PermissionsExt as _;
        if rustix::process::geteuid().is_root() {
            return; // Root writes a read-only folder anyway.
        }
        let rig = Rig::new();
        let control = rig.root.path().join("control");
        fs::create_dir_all(account_home(&control)).unwrap();
        let skill = rig.folder().join("tool");
        fs::create_dir_all(&skill).unwrap();
        fs::write(skill.join("SKILL.md"), "one").unwrap();
        let home = account_home(&control);
        let fail_home = home.clone();
        *rig.fake.during_import.lock().unwrap() = Some(Box::new(move || {
            fs::set_permissions(&fail_home, fs::Permissions::from_mode(0o500)).unwrap();
        }));
        let failed = rig.sync();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err());
        rig.sync().unwrap();
        assert_eq!(changes(&control), ["added"]);
        fs::write(skill.join("SKILL.md"), "two").unwrap();
        assert_eq!(rig.sync().unwrap(), Synced::default());
        assert_eq!(stored_text(&rig), "two");
    }

    #[test]
    fn the_store_gets_a_real_copy_of_the_skill_folder() {
        let rig = Rig::new();
        // A skill kept elsewhere and linked in: its folder is the target.
        let kept = rig.root.path().join("repo/linked");
        fs::create_dir_all(kept.join("scripts")).unwrap();
        fs::write(kept.join("SKILL.md"), "linked").unwrap();
        fs::write(kept.join("scripts/run.sh"), "echo hi").unwrap();
        std::os::unix::fs::symlink("scripts/run.sh", kept.join("run")).unwrap();
        std::os::unix::fs::symlink(&kept, rig.folder().join("linked")).unwrap();

        assert_eq!(rig.sync().unwrap(), Synced::default());
        assert_eq!(
            tree(&rig.stored("linked")),
            BTreeMap::from([
                ("SKILL.md".to_owned(), "linked".to_owned()),
                ("run".to_owned(), "echo hi".to_owned()),
                ("scripts/run.sh".to_owned(), "echo hi".to_owned()),
            ])
        );
        // In step: nothing to import again.
        rig.sync().unwrap();
        assert_eq!(*rig.fake.imports.lock().unwrap(), 1);
    }
}
