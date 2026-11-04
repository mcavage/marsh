//! The skills mirror (`marsh config skills-mirror`, `docs/kits.md`): stock
//! SBX's shared skills store is the one source of skills for every job, and
//! the daemon keeps it in step with the folders `sbx skills import` scans
//! (`~/.agents/skills`, `~/.claude/skills`, ...).
//!
//! Once the folders change and settle, the daemon imports the skills whose
//! store copy differs, and removes the ones it imported whose source is gone.
//! It only ever replaces or removes a store entry it put there itself: one
//! whose content is what it last imported (its record in the control home)
//! or what the folder holds now. Anything else (an `sbx skills add`) is a
//! clash: the store's copy wins and `marsh status` names it. `sbx skills ls`
//! does not say where a skill came from, so the content is the evidence.
//!
//! `sbx skills import` takes no names, so the import runs with a HOME the
//! daemon prepares: `.agents/skills` holds links to just the skills to
//! import, and the store's path leads to the real store.

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
const RECORD_FILE: &str = "skills-mirror.json";
const STAGING_DIR: &str = "skills-mirror-home";
/// How long the folders must stay unchanged before an import.
const SETTLE: Duration = Duration::from_secs(2);
/// After a failed pass, when the same folders are tried again.
const RETRY: Duration = Duration::from_mins(1);
const MAX_RECORD_BYTES: u64 = 1024 * 1024;
const MAX_SKILL_FILES: usize = 4096;
const MAX_SKILL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_STAMPS: usize = 65_536;

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
    pub(crate) fn pass(&self, sbx: &StockSbx, scope_root: &Path, control_home: &Path) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !enabled(scope_root) {
            state.synced = None;
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
        if state.synced.as_ref() == Some(&stamps) {
            return;
        }
        let first = state.synced.is_none() && state.status.last_import.is_none();
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
        match sync(sbx, &folders, control_home) {
            Ok(clashes) => {
                state.status.clashes = clashes;
                state.status.last_import = Some(now_ms());
                state.status.errors.clear();
            }
            Err(message) => fail(&mut state, message),
        }
        state.synced = Some(stamps);
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
            if valid_name(&name) && path.join("SKILL.md").is_file() {
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

/// A digest of a skill's regular files (links inside are skipped, as
/// `sbx skills import` skips them); `None` when unreadable or too big.
fn digest(root: &Path) -> Option<String> {
    let mut files = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        for entry in fs::read_dir(root.join(&relative)).ok()?.flatten() {
            let kind = entry.file_type().ok()?;
            let path = relative.join(entry.file_name());
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                files.push(path);
            }
            if files.len() > MAX_SKILL_FILES {
                return None;
            }
        }
    }
    files.sort();
    let mut hash = Sha256::new();
    let mut total = 0;
    for file in files {
        let mut bytes = Vec::new();
        fs::File::open(root.join(&file))
            .ok()?
            .take(MAX_SKILL_BYTES + 1 - total)
            .read_to_end(&mut bytes)
            .ok()?;
        total += bytes.len() as u64;
        if total > MAX_SKILL_BYTES {
            return None;
        }
        hash.update(file.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Some(marsh_contracts::process::hex(&hash.finalize()))
}

/// What one sync does with each name.
#[derive(Debug, Default, Eq, PartialEq)]
struct Plan {
    import: Vec<String>,
    remove: Vec<String>,
    clashes: Vec<String>,
    record: BTreeMap<String, String>,
}

/// `store` holds the digest of each store entry; `sources` of each source.
fn plan(
    sources: &BTreeMap<String, Option<String>>,
    store: &BTreeMap<String, Option<String>>,
    record: &BTreeMap<String, String>,
) -> Plan {
    let mut plan = Plan::default();
    for (name, source) in sources {
        let recorded = record.get(name);
        match store.get(name) {
            None => plan.import.push(name.clone()),
            Some(current) => {
                let ours = current.is_some() && (current.as_ref() == recorded || current == source);
                if !ours {
                    plan.clashes.push(name.clone());
                    continue;
                }
                if current != source || source.is_none() {
                    plan.import.push(name.clone());
                } else if let Some(current) = current {
                    plan.record.insert(name.clone(), current.clone());
                }
            }
        }
    }
    for (name, recorded) in record {
        if sources.contains_key(name) {
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

fn read_record(control_home: &Path) -> Record {
    let mut bytes = Vec::new();
    let read = fs::File::open(control_home.join(RECORD_FILE))
        .and_then(|file| file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes));
    if read.is_err() || bytes.len() as u64 > MAX_RECORD_BYTES {
        return Record::default();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn write_record(control_home: &Path, record: &Record) -> Result<(), String> {
    let mut bytes = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let mut temporary =
        tempfile::NamedTempFile::new_in(control_home).map_err(|error| error.to_string())?;
    temporary
        .write_all(&bytes)
        .map_err(|error| error.to_string())?;
    temporary
        .persist(control_home.join(RECORD_FILE))
        .map_err(|error| error.error.to_string())?;
    Ok(())
}

/// The HOME for one import: links to the chosen sources, and the store's
/// parent directory at its place under HOME.
fn stage(
    control_home: &Path,
    stock_home: &Path,
    store: &Path,
    chosen: &[(&String, &PathBuf)],
) -> Result<PathBuf, String> {
    let staging = control_home.join(STAGING_DIR);
    if fs::symlink_metadata(&staging).is_ok() {
        fs::remove_dir_all(&staging)
            .map_err(|error| format!("cannot clear {}: {error}", staging.display()))?;
    }
    let skills = staging.join(".agents/skills");
    fs::create_dir_all(&skills).map_err(|error| error.to_string())?;
    for (name, source) in chosen {
        std::os::unix::fs::symlink(source, skills.join(name)).map_err(|error| error.to_string())?;
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

/// Brings the store in step; returns the clashes.
fn sync(sbx: &StockSbx, folders: &[PathBuf], control_home: &Path) -> Result<Vec<String>, String> {
    let store = sbx
        .skills_store()
        .map_err(|error| format!("cannot list the sbx skills store: {error}"))?;
    let found = sources(folders);
    let source_digests: BTreeMap<_, _> = found
        .iter()
        .map(|(name, path)| (name.clone(), digest(path)))
        .collect();
    let store_digests: BTreeMap<_, _> = store
        .skills
        .iter()
        .map(|name| (name.clone(), digest(&store.directory.join(name))))
        .collect();
    let mut record = read_record(control_home);
    let plan = plan(&source_digests, &store_digests, &record.skills);
    record
        .skills
        .retain(|name, _| found.contains_key(name) && !plan.clashes.contains(name));
    record.skills.extend(plan.record.clone());
    let mut result = Ok(());
    if !plan.import.is_empty() {
        let chosen: Vec<_> = plan
            .import
            .iter()
            .filter_map(|name| found.get_key_value(name))
            .collect();
        let home = sbx
            .stock_home()
            .ok_or_else(|| "the stock SBX HOME is unknown".to_owned())?;
        let staging = stage(control_home, &home, &store.directory, &chosen)?;
        result = sbx
            .skills_import(&staging)
            .map_err(|error| format!("sbx skills import failed: {error}"));
        let _ = fs::remove_dir_all(&staging);
        for name in &plan.import {
            if let Some(digest) = digest(&store.directory.join(name)) {
                record.skills.insert(name.clone(), digest);
            }
        }
    }
    if result.is_ok() && !plan.remove.is_empty() {
        result = sbx
            .skills_remove(&plan.remove)
            .map_err(|error| format!("sbx skills rm failed: {error}"));
    }
    write_record(control_home, &record)?;
    result?;
    Ok(plan.clashes)
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
            ("gone".to_owned(), "g".to_owned()),
            ("changed-since".to_owned(), "c".to_owned()),
        ]);
        let plan = plan(&sources, &store, &record);
        assert_eq!(plan.import, ["edited", "new"]);
        assert_eq!(plan.remove, ["gone"]);
        assert_eq!(plan.clashes, ["added"]);
        assert_eq!(plan.record, BTreeMap::from([("same".into(), "s".into())]));
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
        let before = digest(&a.join("x")).unwrap();
        fs::write(a.join("x/SKILL.md"), "edited").unwrap();
        assert_ne!(digest(&a.join("x")).unwrap(), before);
    }
}
