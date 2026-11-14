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

use marsh_daemon::SkillsMirrorStatus;
use marsh_sbx::StockSbx;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
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
const STAGING_DIR: &str = "skills-mirror-home";
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
    pub(crate) fn status(&self, scope_root: &Path) -> SkillsMirrorStatus {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SkillsMirrorStatus {
            enabled: enabled(scope_root),
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
    let key = marsh_contracts::process::hex(&Sha256::digest(store.as_os_str().as_encoded_bytes()));
    account_home(control_home).join(format!("record-{}.json", &key[..16]))
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
        record.skills.entry(name).or_insert(digest);
    }
    (record, Some(legacy))
}

fn write_record(
    control_home: &Path,
    store: &Path,
    record: &Record,
    legacy: Option<&Path>,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(account_home(control_home))
        .map_err(|error| error.to_string())?;
    temporary
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    temporary
        .persist(record_path(control_home, store))
        .map_err(|error| error.error.to_string())?;
    if let Some(legacy) = legacy {
        let _ = fs::rename(legacy, legacy.with_extension("json.migrated"));
    }
    Ok(())
}

/// The HOME for one import: a copy of each chosen skill's files, and the
/// store's parent directory at its place under HOME.
fn stage(
    control_home: &Path,
    stock_home: &Path,
    store: &Path,
    chosen: &[(&String, &Files)],
) -> Result<PathBuf, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let staging = control_home.join(STAGING_DIR);
    if fs::symlink_metadata(&staging).is_ok() {
        fs::remove_dir_all(&staging)
            .map_err(|error| format!("cannot clear {}: {error}", staging.display()))?;
    }
    let skills = staging.join(".agents/skills");
    fs::create_dir_all(&skills).map_err(|error| error.to_string())?;
    for (name, files) in chosen {
        for (relative, (executable, bytes)) in *files {
            let path = skills.join(name).join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(if *executable { 0o755 } else { 0o644 })
                .open(&path)
                .and_then(|mut file| file.write_all(bytes))
                .map_err(|error| format!("cannot stage {}: {error}", path.display()))?;
        }
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

/// Brings the store in step.
fn sync(
    sbx: &StockSbx,
    folders: &[PathBuf],
    control_home: &Path,
    bundled: &BTreeMap<String, Vec<String>>,
) -> Result<Synced, String> {
    let Some(_lock) = lock_account(control_home)? else {
        return Err("another marsh scope is updating the skills store".into());
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
    let plan = plan(&source_digests, &store_digests, &record.skills, bundled);
    record.skills.retain(|name, _| {
        (found.contains_key(name) && !bundled.contains_key(name) && !plan.clashes.contains(name))
            || plan.remove.contains(name)
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
        let staging = stage(control_home, &home, &store.directory, &chosen)?;
        result = sbx
            .skills_import(&staging)
            .map_err(|error| format!("sbx skills import failed: {error}"));
        let _ = fs::remove_dir_all(&staging);
        // Only what this import put there is recorded: never a copy that
        // appeared beside a failed import (an `sbx skills add`, even of the
        // same content). A failed import takes no name it did not own.
        for (name, files) in &chosen {
            let staged = digest(files);
            let owned = result.is_ok() || record.skills.contains_key(*name);
            if owned && store_digest(name).as_ref() == Some(&staged) {
                record.skills.insert((*name).clone(), staged);
            }
        }
    }
    if result.is_ok() && !plan.remove.is_empty() {
        result = sbx
            .skills_remove(&plan.remove)
            .map_err(|error| format!("sbx skills rm failed: {error}"));
        if result.is_ok() {
            record.skills.retain(|name, _| !plan.remove.contains(name));
        }
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
                    if let Some((name, body)) = self.added_during_import.lock().unwrap().take() {
                        fs::create_dir_all(store.join(&name)).unwrap();
                        fs::write(store.join(&name).join("SKILL.md"), body).unwrap();
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
        let errors = mirror.status(&scope).errors;
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
