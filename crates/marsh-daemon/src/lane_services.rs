//! Local data in lanes (C3): what `marsh split --open --from CHECKPOINT` does for
//! a checkpoint that holds a Docker snapshot, and what closing a lane undoes.
//! The folder's own local data (git-ignored files, `crate::ignored`) is copied
//! before this runs and folded into the lane's result and line here.
//!
//! Everything the services VM says is hostile (SECURITY-DESIGN A8, E1-R2). The
//! restore result reaches the daemon as raw VM text, so **every field is
//! validated before anything is used**: file names are exactly
//! `compose.fork.yaml` and `.env` (below an ordinary relative folder), contents
//! are size-capped and valid UTF-8 with no NUL, ports are integers in range,
//! service and volume names are bounded single tokens, text is plain and
//! bounded. Only then are the lane's two files written, **by this process on
//! the Mac** (A9: relative to a held directory descriptor, `O_NOFOLLOW`, a
//! temporary file and an atomic rename), and only then is the restore line
//! built, on the Mac, from the validated fields and words of its own.
//!
//! The order of one lane's restore:
//!
//! 1. `restore_prepare` through the relay (the snapshot is leased by the
//!    caller: the checkpoint's record, which keeps it, cannot be removed);
//! 2. validate the whole result; on any defect the fork is dropped, nothing was
//!    written, and the lane opens with `ok: false` and a gap;
//! 3. write the lane files (`hold-lane-files-write` is the harness barrier);
//! 4. `restore_up` through the relay, and validate that result too.
//!
//! A restore that fails is never silent: `ok: false`, the gaps, and a line that
//! says so. The lane still opens with its files.

use crate::DaemonBackend;
use crate::split_fs::Dir;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The two files the daemon writes into a lane (A9). Nothing else is accepted.
pub const OVERRIDE_NAME: &str = "compose.fork.yaml";
pub const ENV_NAME: &str = ".env";
/// A lane file's content is at most this many bytes (the agent's own bound).
pub const MAX_FILE_BYTES: usize = 128 * 1024;
const MAX_FILES: usize = 16;
const MAX_DIR_DEPTH: usize = 4;
const MAX_ITEMS: usize = 128;
const MAX_PORTS: usize = 512;
const MAX_GAPS: usize = 64;
const MAX_NAMES: usize = 64;
const MAX_TOKEN: usize = 128;
/// Display text kept from the VM, per field.
const TEXT_BYTES: usize = 240;
/// A string longer than this is refused outright, not shortened.
const TEXT_REFUSE_BYTES: usize = 16 * 1024;
/// The restore line: one line of at most this many characters (U6).
pub const LINE_CHARS: usize = 400;
/// Each name in the line is at most this long, ending in `…` when cut.
const NAME_CHARS: usize = 32;
const LANE_FILE_MODE: u32 = 0o644;

/// Test-only defects and holds (`docs/design/test-breaks.md`), read through
/// `marsh_contracts::break_mode`, which is `false` in every release build.
pub mod breaks {
    /// The agent writes the lane files (the daemon skips its write): A9's witness.
    pub const RESTORE_FILES_FROM_AGENT: &str = "restore-files-from-agent";
    /// Raw VM text goes into the restore line.
    pub const RESTORE_LINE_TRUST_VM: &str = "restore-line-trust-vm";
    /// A lane's endpoint gets the project's root scope instead of its own.
    pub const LANE_SCOPE_ROOT: &str = "lane-scope-root";
    /// Closing, cancelling or removing a split drops no lane's services.
    pub const LANE_DROP_OFF: &str = "lane-drop-off";
    /// The test plan's name for the same defect.
    pub const DROP_NOOP: &str = "drop-noop";
    /// A partial restore reports `ok: true`.
    pub const RESTORE_SILENT_FAIL: &str = "restore-silent-fail";
    /// Every restore says it worked.
    pub const LINE_ALWAYS_OK: &str = "line-always-ok";
    pub const LINE_OMIT_FRESH: &str = "line-omit-fresh";
    pub const LINE_INTERNAL_WORDS: &str = "line-internal-words";
    /// The lane's ports are not mapped back to the original numbers.
    pub const LANE_PORTS_UNMAPPED: &str = "lane-ports-unmapped";
    /// The lane files are written through whatever the name points at.
    pub const LANE_FILES_FOLLOW_SYMLINKS: &str = "lane-files-follow-symlinks";
    /// `split apply` carries the generated lane files into the project.
    pub const APPLY_INCLUDE_GENERATED: &str = "apply-include-generated";
    /// Holds: `<point>.reached` appears under `test-holds` until `<point>.release`.
    pub const HOLD_LANE_FILES_WRITE: &str = "hold-lane-files-write";
    pub const HOLD_RESTORE_AFTER_LEASE: &str = "hold-restore-after-lease";

    #[must_use]
    pub fn drop_off() -> bool {
        marsh_contracts::break_mode(LANE_DROP_OFF) || marsh_contracts::break_mode(DROP_NOOP)
    }
}

// ------------------------------------------------------------------ results

/// One saved-data item that came back, as the agent reported it and the Mac
/// validated it: a service (`kind: service`) or a volume kept without its
/// container (`kind: volume`).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RestoredItem {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarted: Option<bool>,
}

/// Something that started fresh or was not restored, and why.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FreshItem {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub reason: String,
}

impl FreshItem {
    fn label(&self) -> &str {
        self.service
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or("?")
    }
}

/// One container the post-restore check looked at.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckedContainer {
    pub service: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
}

/// What the agent computed after bringing the fork up.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Check {
    pub ok: bool,
    pub containers: Vec<CheckedContainer>,
}

/// What came along from the folder into a lane: its git-ignored local data
/// (`.env`, `.wrangler/state`, SQLite files), as the checkpoint saved it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FolderData {
    pub files: u64,
    pub bytes: u64,
    /// The saved paths, as the line names them.
    pub copied: Vec<String>,
    /// What was left out, and why, in plain words.
    pub skipped: Vec<crate::ignored::Skipped>,
}

impl From<crate::ignored::IgnoredRecord> for FolderData {
    fn from(record: crate::ignored::IgnoredRecord) -> Self {
        Self {
            files: record.files,
            bytes: record.bytes,
            copied: record.roots,
            skipped: record.skipped,
        }
    }
}

impl FolderData {
    /// Whether there is anything to say about the folder.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files == 0 && self.skipped.is_empty()
    }
}

fn why_not_copied(reason: &str) -> &'static str {
    if reason.contains("size limit") {
        "over the size limit"
    } else if reason.contains("limit of") {
        "over the file limit"
    } else {
        "could not be copied"
    }
}

/// One lane's restore, as `split --open --from` reports it (`restore` of the
/// `split` event and of `split --json`; `docs/design/services.md`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LaneRestore {
    /// True iff everything the checkpoint's stacks saved came back with its
    /// data, verified (TEST-PLAN A.4).
    pub ok: bool,
    /// One line for people, built on the Mac from validated fields.
    pub line: String,
    pub restored: Vec<RestoredItem>,
    pub fresh: Vec<FreshItem>,
    pub gaps: Vec<String>,
    /// Original port number (as text, a JSON object key) to the fork's published
    /// number. Inside the lane's jobs the original numbers work (A6).
    pub ports: BTreeMap<String, u16>,
    pub check: Option<Check>,
    /// The folder's local data that came along (DECISIONS 26, 27).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<FolderData>,
    /// Why the Docker restore did not happen (not serialized: only to build
    /// the line again).
    #[serde(skip)]
    problem: Option<Problem>,
}

impl LaneRestore {
    /// Folds in what came along from the folder: the line names it, and what
    /// could not be copied is a gap and makes `ok` false. Nothing to say
    /// leaves the result as it is.
    #[must_use]
    pub fn with_folder(mut self, folder: FolderData) -> Self {
        if folder.is_empty() {
            return self;
        }
        let lost: Vec<String> = folder
            .skipped
            .iter()
            .filter(|skipped| skipped.lost)
            .map(|skipped| {
                format!(
                    "not copied: {} ({})",
                    skipped.path,
                    why_not_copied(&skipped.reason)
                )
            })
            .collect();
        if !lost.is_empty() {
            self.ok = false;
            for gap in lost {
                if self.gaps.len() < MAX_GAPS {
                    self.gaps.push(gap);
                }
            }
        }
        self.folder = Some(folder);
        self.line = line(
            &self,
            self.problem,
            marsh_contracts::break_mode(breaks::RESTORE_LINE_TRUST_VM),
        );
        self
    }

    /// The result for a lane whose project has no Docker data to restore but
    /// whose folder's local data came along.
    #[must_use]
    pub fn folder_only(folder: FolderData) -> Self {
        let restore = Self {
            ok: true,
            line: String::new(),
            restored: Vec::new(),
            fresh: Vec::new(),
            gaps: Vec::new(),
            ports: BTreeMap::new(),
            check: None,
            folder: None,
            problem: None,
        };
        let mut restore = restore.with_folder(folder);
        if restore.line.is_empty() {
            restore.line = line(&restore, None, false);
        }
        restore
    }

    /// `ports` with numeric keys.
    #[must_use]
    pub fn port_map(&self) -> BTreeMap<u16, u16> {
        self.ports
            .iter()
            .filter_map(|(original, fork)| Some((original.parse().ok()?, *fork)))
            .collect()
    }

    /// A restore that did not happen, and why: `ok: false`, never silent.
    #[must_use]
    pub fn failed(problem: Problem, gaps: Vec<String>) -> Self {
        let mut restore = Self {
            ok: false,
            line: String::new(),
            restored: Vec::new(),
            fresh: Vec::new(),
            gaps: gaps.into_iter().take(MAX_GAPS).collect(),
            ports: BTreeMap::new(),
            check: None,
            folder: None,
            problem: Some(problem),
        };
        restore.line = line(&restore, Some(problem), false);
        restore
    }
}

/// Why a restore did not happen, in words of the Mac's own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Problem {
    /// The checkpoint holds no app data for this turn (no snapshot).
    NotSaved,
    /// Docker is off for the project.
    Off,
    /// Docker's VM or its relay is not usable.
    Unavailable,
    /// The agent or the daemon refused or failed it.
    Failed,
    /// The result from the VM was not understood: nothing was written.
    BadResult,
    /// The lane folder changed or could not take its files.
    LaneFiles,
    /// The lane was closed or cancelled while it restored.
    Aborted,
    /// The lane's name cannot be a workspace name.
    LaneName,
}

impl Problem {
    fn phrase(self) -> &'static str {
        match self {
            Self::NotSaved => {
                "This turn's Docker data wasn't saved, so the lane starts without it."
            }
            Self::Off => "Docker is turned off for this project, so no Docker data was restored.",
            Self::Unavailable => "Docker isn't running, so no Docker data was restored.",
            Self::Failed => "The Docker data restore failed, so the lane starts without it.",
            Self::BadResult => {
                "The Docker data restore returned something that wasn't understood, so nothing was restored."
            }
            Self::LaneFiles => {
                "The lane's files could not be prepared, so no Docker data was restored."
            }
            Self::Aborted => "The lane was closed before its Docker data finished restoring.",
            Self::LaneName => {
                "This lane's name is too long for Docker data, so it starts without it."
            }
        }
    }
}

/// What the backend says about a failed call, with the class in the Mac's terms
/// and the detail as display text.
#[derive(Clone, Debug)]
pub struct CallFailure {
    pub problem: Problem,
    pub detail: String,
}

impl CallFailure {
    #[must_use]
    pub fn new(problem: Problem, detail: impl Into<String>) -> Self {
        Self {
            problem,
            detail: detail.into(),
        }
    }
}

/// What a workspace drop came to, for the daemon to decide whether to retry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DropOutcome {
    /// The project has no services VM: there was nothing to drop.
    Nothing,
    /// The agent dropped the workspace; `left` is anything still there.
    Dropped {
        containers: u32,
        volumes: u32,
        networks: u32,
        left: Vec<String>,
    },
    /// The VM is not ready (stopped, starting); try again later.
    Later(String),
}

// ------------------------------------------------------------ validation

/// A lane file the agent asked the daemon to write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaneFile {
    /// The folder below the lane (ordinary names, at most four deep; empty
    /// for the lane itself).
    pub dir: Vec<String>,
    pub name: &'static str,
    pub content: String,
}

impl LaneFile {
    /// `dir/name` as the generated-file list records it.
    #[must_use]
    pub fn relative(&self) -> String {
        if self.dir.is_empty() {
            self.name.to_owned()
        } else {
            format!("{}/{}", self.dir.join("/"), self.name)
        }
    }
}

/// The fields of a restore result, validated.
#[derive(Debug, Default)]
pub struct Fields {
    pub agent_ok: bool,
    pub restored: Vec<RestoredItem>,
    pub fresh: Vec<FreshItem>,
    pub gaps: Vec<String>,
    pub failures: Vec<String>,
    pub ports: BTreeMap<u16, u16>,
    pub files: Vec<LaneFile>,
    pub check: Option<Check>,
}

fn plain(text: &str) -> String {
    marsh_sbx::vm_text::plain_text(text, TEXT_BYTES)
}

/// A bounded single token: `[A-Za-z0-9][A-Za-z0-9._-]*`.
fn token(value: &Value, what: &str) -> Result<String, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{what} is not text"))?;
    let bytes = text.as_bytes();
    if bytes.is_empty()
        || bytes.len() > MAX_TOKEN
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(format!("{what} is not a name"));
    }
    Ok(text.to_owned())
}

fn optional_token(object: &Value, key: &str) -> Result<Option<String>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => token(value, key).map(Some),
    }
}

fn tokens(value: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| format!("{what} is not a list"))?;
    if list.len() > MAX_NAMES {
        return Err(format!("{what} is too long"));
    }
    list.iter().map(|item| token(item, what)).collect()
}

fn display(value: &Value, what: &str) -> Result<String, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{what} is not text"))?;
    if text.len() > TEXT_REFUSE_BYTES {
        return Err(format!("{what} is oversized"));
    }
    Ok(plain(text))
}

fn text_list(value: Option<&Value>, what: &str, max: usize) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| format!("{what} is not a list"))?;
    if list.len() > max {
        return Err(format!("{what} is too long"));
    }
    list.iter().map(|item| display(item, what)).collect()
}

fn entries(value: Option<&Value>, what: &str) -> Result<Vec<Value>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| format!("{what} is not a list"))?;
    if list.len() > MAX_ITEMS {
        return Err(format!("{what} is too long"));
    }
    Ok(list.clone())
}

fn port(value: &Value, what: &str) -> Result<u16, String> {
    value
        .as_u64()
        .and_then(|number| u16::try_from(number).ok())
        .filter(|number| *number != 0)
        .ok_or_else(|| format!("{what} is not a port"))
}

fn restored_item(value: &Value) -> Result<RestoredItem, String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("a restored item has no kind")?;
    match kind {
        "service" => Ok(RestoredItem {
            kind: "service".into(),
            service: Some(token(
                value.get("service").unwrap_or(&Value::Null),
                "a restored service",
            )?),
            stack: optional_token(value, "stack")?,
            volumes: tokens(value.get("volumes"), "a service's data names")?,
            ..RestoredItem::default()
        }),
        "volume" => Ok(RestoredItem {
            kind: "volume".into(),
            name: Some(token(
                value.get("name").unwrap_or(&Value::Null),
                "a kept data name",
            )?),
            from: Some(token(
                value.get("from").unwrap_or(&Value::Null),
                "a kept data origin",
            )?),
            containers: tokens(value.get("containers"), "a kept data user")?,
            restarted: Some(value.get("restarted").and_then(Value::as_bool) == Some(true)),
            ..RestoredItem::default()
        }),
        _ => Err("a restored item of an unknown kind".into()),
    }
}

fn fresh_item(value: &Value) -> Result<FreshItem, String> {
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or("a fresh item has no kind")?;
    if !matches!(kind, "service" | "testcontainers" | "container") {
        return Err("a fresh item of an unknown kind".into());
    }
    let reason = display(value.get("reason").unwrap_or(&Value::Null), "a reason")?;
    Ok(FreshItem {
        kind: kind.to_owned(),
        service: optional_token(value, "service")?,
        stack: optional_token(value, "stack")?,
        name: optional_token(value, "name")?,
        reason,
    })
    .and_then(|item| {
        if item.service.is_none() && item.name.is_none() {
            Err("a fresh item has no name".to_owned())
        } else {
            Ok(item)
        }
    })
}

/// A key of `files`: an optional ordinary folder, then exactly one of the two
/// file names.
fn lane_file_key(key: &str) -> Result<(Vec<String>, &'static str), String> {
    let (dir, name) = match key.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", key),
    };
    let name = match name {
        OVERRIDE_NAME => OVERRIDE_NAME,
        ENV_NAME => ENV_NAME,
        _ => return Err("a lane file with a name that is not allowed".into()),
    };
    let mut parts = Vec::new();
    if !dir.is_empty() {
        for part in dir.split('/') {
            let bytes = part.as_bytes();
            if bytes.is_empty()
                || bytes.len() > 64
                || part == "."
                || part == ".."
                || part == ".marsh"
                || part == ".git"
                || !bytes
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            {
                return Err("a lane file in a folder that is not allowed".into());
            }
            parts.push(part.to_owned());
        }
    }
    if parts.len() > MAX_DIR_DEPTH {
        return Err("a lane file too deep".into());
    }
    Ok((parts, name))
}

fn lane_files(value: &Value) -> Result<Vec<LaneFile>, String> {
    let object = value.as_object().ok_or("files is not an object")?;
    if object.len() > MAX_FILES {
        return Err("too many lane files".into());
    }
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    for (key, content) in object {
        let (dir, name) = lane_file_key(key)?;
        let content = content.as_str().ok_or("a lane file is not text")?;
        if content.len() > MAX_FILE_BYTES {
            return Err("a lane file is oversized".into());
        }
        if content.contains('\0') {
            return Err("a lane file holds a NUL".into());
        }
        let file = LaneFile {
            dir,
            name,
            content: content.to_owned(),
        };
        if !seen.insert(file.relative()) {
            return Err("a lane file is named twice".into());
        }
        files.push(file);
    }
    Ok(files)
}

fn ports(value: &Value) -> Result<BTreeMap<u16, u16>, String> {
    let object = value.as_object().ok_or("ports is not an object")?;
    if object.len() > MAX_PORTS {
        return Err("too many ports".into());
    }
    let mut map = BTreeMap::new();
    let mut targets = BTreeSet::new();
    for (key, value) in object {
        let original = key
            .parse::<u16>()
            .ok()
            .filter(|number| *number != 0 && number.to_string() == *key)
            .ok_or("a port key is not a port")?;
        let mapped = port(value, "a mapped port")?;
        if !targets.insert(mapped) {
            return Err("two ports map to one".into());
        }
        map.insert(original, mapped);
    }
    Ok(map)
}

fn check_of(value: &Value) -> Result<Check, String> {
    let ok = value
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or("check has no verdict")?;
    let mut containers = Vec::new();
    for item in entries(value.get("containers"), "the check's containers")? {
        let state = item
            .get("state")
            .and_then(Value::as_str)
            .filter(|state| {
                matches!(
                    *state,
                    "running"
                        | "exited"
                        | "created"
                        | "restarting"
                        | "paused"
                        | "dead"
                        | "removing"
                        | "unknown"
                )
            })
            .ok_or("a checked container has a state that is not known")?
            .to_owned();
        let health = match item.get("health") {
            None | Some(Value::Null) => None,
            Some(Value::String(health))
                if matches!(
                    health.as_str(),
                    "healthy" | "unhealthy" | "starting" | "none"
                ) =>
            {
                Some(health.clone())
            }
            Some(_) => return Err("a checked container has a health that is not known".into()),
        };
        containers.push(CheckedContainer {
            service: token(
                item.get("service").unwrap_or(&Value::Null),
                "a checked service",
            )?,
            state,
            health,
        });
    }
    // The problems are VM text: they are shown as gaps by the caller.
    let _ = text_list(value.get("problems"), "the check's problems", MAX_GAPS)?;
    Ok(Check { ok, containers })
}

/// Validates one restore result (`restore_prepare`'s when `want_files`,
/// `restore_up`'s otherwise) in full. Nothing is used before this returns.
///
/// # Errors
/// What was wrong, as plain text. The caller drops the fork and writes nothing.
pub fn validate(value: &Value, want_files: bool) -> Result<Fields, String> {
    let object = value.as_object().ok_or("the result is not an object")?;
    let agent_ok = object
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or("the result has no verdict")?;
    let mut fields = Fields {
        agent_ok,
        ..Fields::default()
    };
    for item in entries(object.get("restored"), "restored")? {
        fields.restored.push(restored_item(&item)?);
    }
    for item in entries(object.get("fresh"), "fresh")? {
        fields.fresh.push(fresh_item(&item)?);
    }
    fields.gaps = text_list(object.get("gaps"), "gaps", MAX_GAPS)?;
    fields.failures = text_list(object.get("failures"), "failures", MAX_GAPS)?;
    if let Some(value) = object.get("ports") {
        fields.ports = ports(value)?;
    }
    // `restore_prepare` must name the lane files; `restore_up` repeats them (the
    // agent's reply carries the same map). Either way they are validated in full
    // and only the first is written.
    match (object.get("files"), want_files) {
        (Some(files), _) => {
            let files = lane_files(files)?;
            if want_files {
                fields.files = files;
            }
        }
        (None, true) => return Err("the result names no lane files".into()),
        (None, false) => {}
    }
    if !want_files {
        fields.check = Some(check_of(
            object.get("check").ok_or("the result has no check")?,
        )?);
    }
    Ok(fields)
}

// -------------------------------------------------------------- the line

/// Why something started fresh or was not restored, in the Mac's own words:
/// the agent's text is classified, never shown in the line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reason {
    NoSavedData,
    Incomplete,
    DidNotStart,
    Unsafe,
    NotReadable,
    NotInstalled,
    NotRestarted,
    NotRestored,
}

impl Reason {
    fn of(item: &FreshItem) -> Self {
        let text = item.reason.to_ascii_lowercase();
        if item.kind == "container" {
            Self::NotRestarted
        } else if item.kind == "testcontainers" {
            Self::NotRestored
        } else if text.contains("incomplete") {
            Self::Incomplete
        } else if text.contains("did not start") {
            Self::DidNotStart
        } else if text.contains("forked safely") || text.contains("outside the project") {
            Self::Unsafe
        } else if text.contains("compose files") {
            Self::NotReadable
        } else if text.contains("could not be installed") {
            Self::NotInstalled
        } else if text.starts_with("no saved data") {
            Self::NoSavedData
        } else {
            Self::NotRestored
        }
    }

    fn words(self) -> &'static str {
        match self {
            Self::NoSavedData => "no saved data",
            Self::Incomplete => "its saved data was incomplete",
            Self::DidNotStart => "it did not start",
            Self::Unsafe => "it can't be forked safely",
            Self::NotReadable => "its compose files could not be read",
            Self::NotInstalled => "its data could not be put back",
            Self::NotRestarted => "not restarted",
            Self::NotRestored => "not restored",
        }
    }

    /// A reason that is a failure to restore what had saved data, as opposed to
    /// something that never had any.
    fn is_failure(self) -> bool {
        matches!(
            self,
            Self::Incomplete | Self::DidNotStart | Self::NotReadable | Self::NotInstalled
        )
    }
}

/// `name` at most `NAME_CHARS` characters, ending in `…` when cut.
fn short(name: &str) -> String {
    if name.chars().count() <= NAME_CHARS {
        name.to_owned()
    } else {
        let mut cut: String = name.chars().take(NAME_CHARS - 1).collect();
        cut.push('…');
        cut
    }
}

fn truncate_line(mut text: String) -> String {
    if text.chars().count() > LINE_CHARS {
        text = text.chars().take(LINE_CHARS - 1).collect();
        text.push('…');
    }
    text
}

/// The names of the services something went wrong for: fresh items whose
/// reason is a failure or a refusal, and containers the check found not
/// running or not healthy.
fn trouble(restore: &LaneRestore) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for item in &restore.fresh {
        let reason = Reason::of(item);
        if reason.is_failure() || reason == Reason::Unsafe {
            let name = item.label().to_owned();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    if let Some(check) = &restore.check {
        for container in &check.containers {
            let good = match container.state.as_str() {
                "running" => container
                    .health
                    .as_deref()
                    .is_none_or(|health| matches!(health, "healthy" | "none")),
                "exited" => true,
                _ => false,
            };
            if !good && !names.contains(&container.service) {
                names.push(container.service.clone());
            }
        }
    }
    names
}

fn join_budget(names: &[String], room: usize) -> (String, usize) {
    let mut out = String::new();
    let mut used = 0;
    for name in names {
        let shown = short(name);
        let piece = if out.is_empty() {
            shown
        } else {
            format!(", {shown}")
        };
        if out.chars().count() + piece.chars().count() > room {
            break;
        }
        out.push_str(&piece);
        used += 1;
    }
    (out, names.len() - used)
}

/// The restore line (U6): one line, at most [`LINE_CHARS`] characters, in words
/// of the Mac's own. The failure clause is written first and never dropped;
/// each name is cut to [`NAME_CHARS`]; the optional clauses are added while they
/// fit, and `and K more not restored` says what did not.
#[must_use]
#[allow(clippy::too_many_lines)] // One line, written in its priority order.
pub fn line(restore: &LaneRestore, problem: Option<Problem>, trust_vm: bool) -> String {
    let always_ok = marsh_contracts::break_mode(breaks::LINE_ALWAYS_OK);
    let mut parts: Vec<String> = Vec::new();
    // The headline.
    let services: Vec<String> = restore
        .restored
        .iter()
        .filter_map(|item| item.service.clone())
        .collect();
    let folder: &[String] = restore
        .folder
        .as_ref()
        .map_or(&[], |folder| folder.copied.as_slice());
    let mut optional: Vec<(u8, String)> = Vec::new();
    let mut omitted = 0_usize;
    let failure = if restore.ok || always_ok {
        None
    } else if let Some(problem) = problem {
        Some(problem.phrase().to_owned())
    } else {
        let names = trouble(restore);
        let (list, more) = join_budget(&names, 120);
        Some(if names.is_empty() {
            "Not everything came back.".to_owned()
        } else if more == 0 {
            format!("Not everything came back: {list}.")
        } else {
            format!("Not everything came back: {list} and {more} more.")
        })
    };
    if let Some(failure) = &failure {
        parts.push(failure.clone());
    }
    let reserve = format!(" and {MAX_ITEMS} more not restored.")
        .chars()
        .count();
    if problem.is_none() || !services.is_empty() || !folder.is_empty() {
        let (folder_list, folder_more) = join_budget(folder, 100);
        let (docker_list, docker_more) = join_budget(&services, 100);
        let mut pieces = Vec::new();
        if !folder_list.is_empty() {
            pieces.push(folder_list);
        }
        if !docker_list.is_empty() {
            pieces.push(format!("{docker_list} (Docker)"));
        }
        let more = folder_more + docker_more;
        if pieces.is_empty() {
            parts.insert(0, "No local data came back.".to_owned());
        } else if more == 0 {
            parts.insert(0, format!("Local data restored: {}.", pieces.join(", ")));
        } else {
            parts.insert(
                0,
                format!(
                    "Local data restored: {} and {more} more.",
                    pieces.join(", ")
                ),
            );
        }
    }
    // Optional clauses, most important first.
    if !marsh_contracts::break_mode(breaks::LINE_OMIT_FRESH) && problem.is_none() {
        for item in &restore.fresh {
            let reason = Reason::of(item);
            let why = if trust_vm {
                item.reason.clone()
            } else {
                reason.words().to_owned()
            };
            let name = short(item.label());
            let (group, text) = match reason {
                Reason::NoSavedData => (3, format!("{name} ({why})")),
                _ => (1, format!("{name} ({why})")),
            };
            optional.push((group, text));
        }
        if let Some(folder) = &restore.folder {
            for skipped in folder.skipped.iter().filter(|skipped| skipped.lost) {
                let name = if skipped.path.is_empty() {
                    "some files".to_owned()
                } else {
                    short(&skipped.path)
                };
                optional.push((0, format!("{name} ({})", why_not_copied(&skipped.reason))));
            }
        }
        for item in &restore.restored {
            if item.kind == "volume" {
                let name = item.name.as_deref().unwrap_or("?");
                let owner = item
                    .containers
                    .first()
                    .map(String::as_str)
                    .or(item.from.as_deref())
                    .unwrap_or("?");
                optional.push((
                    2,
                    format!("{} as {} (not started)", short(owner), short(name)),
                ));
            }
        }
    }
    optional.sort_by_key(|(group, _)| *group);
    let mut sections: BTreeMap<u8, Vec<String>> = BTreeMap::new();
    let mut length: usize = parts.iter().map(|p| p.chars().count() + 1).sum();
    for (group, text) in optional {
        let label_cost = if sections.contains_key(&group) {
            0
        } else {
            label(group).chars().count() + 2
        };
        let cost = text.chars().count() + 2 + label_cost;
        if length + cost + reserve > LINE_CHARS {
            omitted += 1;
            continue;
        }
        length += cost;
        sections.entry(group).or_default().push(text);
    }
    for (group, items) in sections {
        parts.push(format!("{}: {}.", label(group), items.join(", ")));
    }
    if omitted > 0 {
        parts.push(format!("and {omitted} more not restored."));
    }
    if parts.is_empty() {
        parts.push("Local data restored.".to_owned());
    }
    let mut text = parts.join(" ");
    if marsh_contracts::break_mode(breaks::LINE_INTERNAL_WORDS) {
        text.push_str(" (3 volumes restored from snapshot s1 on the pool)");
    }
    truncate_line(text)
}

fn label(group: u8) -> &'static str {
    match group {
        0 => "Not copied",
        1 => "Not restored",
        2 => "Data kept",
        _ => "Started empty",
    }
}

// ------------------------------------------------------------- the result

/// Builds the lane's result from the two validated phases.
#[must_use]
pub fn assemble(
    prepared: &Fields,
    up: &Fields,
    record_gaps: &[String],
    trust_vm: bool,
) -> LaneRestore {
    let mut gaps = up.gaps.clone();
    for gap in &prepared.gaps {
        if !gaps.contains(gap) {
            gaps.push(gap.clone());
        }
    }
    for failure in prepared.failures.iter().chain(&up.failures) {
        if !gaps.contains(failure) {
            gaps.push(failure.clone());
        }
    }
    for gap in record_gaps {
        gaps.push(format!("saved with a gap: {gap}"));
    }
    let check_ok = up.check.as_ref().is_some_and(|check| check.ok);
    let clean = up.failures.is_empty()
        && prepared.failures.is_empty()
        && up.gaps.is_empty()
        && prepared.gaps.is_empty()
        && record_gaps.is_empty();
    let ok = up.agent_ok && check_ok && clean;
    gaps.truncate(MAX_GAPS);
    let mut restore = LaneRestore {
        ok,
        line: String::new(),
        restored: up.restored.clone(),
        fresh: up.fresh.clone(),
        gaps,
        ports: up
            .ports
            .iter()
            .map(|(original, fork)| (original.to_string(), *fork))
            .collect(),
        check: up.check.clone(),
        folder: None,
        problem: None,
    };
    restore.line = line(&restore, None, trust_vm);
    if marsh_contracts::break_mode(breaks::RESTORE_SILENT_FAIL)
        || marsh_contracts::break_mode(breaks::LINE_ALWAYS_OK)
    {
        // Defect: a partial restore reports `ok: true` (the line still says
        // what went wrong under `restore-silent-fail`).
        restore.ok = true;
    }
    restore
}

// ---------------------------------------------------------- the lane files

fn identity_matches(dir: &Dir, expected: (u64, u64)) -> bool {
    dir.fstat().is_ok_and(|stat| stat.identity == expected)
}

fn tmp_name(name: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        ".marsh-lane-{}.{}.{}.tmp",
        name.trim_start_matches('.'),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// Writes the lane files **on the Mac** (A9): the lane is opened by name below
/// the split's held directory and its identity checked against the record, the
/// files go in by a temporary file (`O_EXCL | O_NOFOLLOW`) beside the target and
/// an atomic rename (which replaces a symlink, never follows it), at the mode
/// the file already had.
///
/// # Errors
/// Why the lane was refused or a file could not be written.
pub fn write_lane_files(
    split: &Dir,
    label: &str,
    identity: (u64, u64),
    lane_path: &Path,
    files: &[LaneFile],
) -> Result<(), String> {
    if marsh_contracts::break_mode(breaks::RESTORE_FILES_FROM_AGENT) {
        // Defect: the agent wrote them; the daemon does not.
        return Ok(());
    }
    if marsh_contracts::break_mode(breaks::LANE_FILES_FOLLOW_SYMLINKS) {
        // Defect: written through whatever the names point at.
        for file in files {
            let path = lane_path.join(file.dir.join("/")).join(file.name);
            std::fs::write(&path, file.content.as_bytes())
                .map_err(|error| format!("{}: {error}", file.relative()))?;
        }
        return Ok(());
    }
    let lane = split
        .open(label)
        .map_err(|error| format!("the lane folder is not usable: {error}"))?;
    if !identity_matches(&lane, identity) {
        return Err("the lane folder was replaced".into());
    }
    for file in files {
        let folder = lane
            .open_path(Path::new(&file.dir.join("/")))
            .map_err(|error| format!("{}: {error}", file.relative()))?;
        let mode = folder
            .stat(file.name)
            .ok()
            .filter(|stat| stat.kind.is_file())
            .map_or(LANE_FILE_MODE, |stat| stat.mode & 0o777)
            | 0o600;
        let temporary = tmp_name(file.name);
        folder
            .write_new_mode(&temporary, file.content.as_bytes(), mode)
            .map_err(|error| format!("{}: {error}", file.relative()))?;
        if let Err(error) = folder.rename(&temporary, &folder, file.name) {
            let _ = folder.unlink(&temporary);
            return Err(format!("{}: {error}", file.relative()));
        }
    }
    // The lane we wrote beneath is still the lane the record names.
    let again = split
        .open(label)
        .map_err(|error| format!("the lane folder is not usable: {error}"))?;
    if !identity_matches(&again, identity) {
        return Err("the lane folder was replaced".into());
    }
    Ok(())
}

// ---------------------------------------------------------- orchestration

/// One lane's restore request.
pub struct LaneRequest<'a> {
    /// The project root.
    pub root: &'a Path,
    /// The snapshot to restore (the record's own or the one it reuses).
    pub snapshot: &'a str,
    /// The lane's own workspace scope and fork (compose project) name.
    pub workspace: &'a str,
    pub fork: &'a str,
    /// Where the checkpointed stacks' folders are relative to, when that is not
    /// the project (a checkpoint of a lane).
    pub source: Option<&'a Path>,
    pub label: &'a str,
    pub lane_path: &'a Path,
    /// The split's folder, held open, and the lane's recorded identity.
    pub split: &'a Dir,
    pub identity: (u64, u64),
    /// The gaps the checkpoint's record carries.
    pub record_gaps: &'a [String],
    /// True once the lane was closed or cancelled.
    pub aborted: &'a (dyn Fn() -> bool + Sync),
}

/// What a lane restore produced: the result, and the generated files the
/// daemon wrote (kept out of `split apply`).
pub struct LaneOutcome {
    pub restore: LaneRestore,
    pub generated: Vec<String>,
}

fn drop_and_fail(
    backend: &dyn DaemonBackend,
    request: &LaneRequest<'_>,
    problem: Problem,
    gap: String,
) -> LaneOutcome {
    // The fork was made in the services VM before the problem was found:
    // leave nothing of it behind.
    let _ = backend.services_drop_workspace(request.root, request.workspace);
    LaneOutcome {
        restore: LaneRestore::failed(problem, vec![gap]),
        generated: Vec::new(),
    }
}

fn failure_of(failure: &CallFailure) -> (Problem, String) {
    (failure.problem, plain(&failure.detail))
}

/// Restores `request.snapshot` into the lane. Never panics and never fails the
/// split: every outcome is a [`LaneRestore`].
#[must_use]
pub fn restore_lane(backend: &dyn DaemonBackend, request: &LaneRequest<'_>) -> LaneOutcome {
    let trust_vm = marsh_contracts::break_mode(breaks::RESTORE_LINE_TRUST_VM);
    let call = crate::LaneCall {
        root: request.root,
        workspace: request.workspace,
        source: request.source,
        lane_dir: request.lane_path,
        fork: request.fork,
    };
    let prepared = match backend.services_restore_prepare(&call, request.snapshot) {
        Ok(value) => value,
        Err(failure) => {
            let (problem, gap) = failure_of(&failure);
            // A refusal before anything was made leaves nothing; a failure
            // after the agent started might.
            return drop_and_fail(backend, request, problem, gap);
        }
    };
    let prepared = match validate(&prepared, true) {
        Ok(fields) => fields,
        Err(why) => {
            return drop_and_fail(
                backend,
                request,
                Problem::BadResult,
                format!("the restore result was refused: {}", plain(&why)),
            );
        }
    };
    if (request.aborted)() {
        return drop_and_fail(
            backend,
            request,
            Problem::Aborted,
            "the lane was closed while it restored".into(),
        );
    }
    backend.test_hold(breaks::HOLD_LANE_FILES_WRITE);
    if let Err(why) = write_lane_files(
        request.split,
        request.label,
        request.identity,
        request.lane_path,
        &prepared.files,
    ) {
        return drop_and_fail(backend, request, Problem::LaneFiles, plain(&why));
    }
    let generated: Vec<String> = prepared.files.iter().map(LaneFile::relative).collect();
    if (request.aborted)() {
        let mut outcome = drop_and_fail(
            backend,
            request,
            Problem::Aborted,
            "the lane was closed while it restored".into(),
        );
        outcome.generated = generated;
        return outcome;
    }
    let up = match backend.services_restore_up(&call) {
        Ok(value) => value,
        Err(failure) => {
            let (problem, gap) = failure_of(&failure);
            let mut outcome = drop_and_fail(backend, request, problem, gap);
            outcome.generated = generated;
            return outcome;
        }
    };
    let up = match validate(&up, false) {
        Ok(fields) => fields,
        Err(why) => {
            let mut outcome = drop_and_fail(
                backend,
                request,
                Problem::BadResult,
                format!("the restore result was refused: {}", plain(&why)),
            );
            outcome.generated = generated;
            return outcome;
        }
    };
    let mut restore = assemble(&prepared, &up, request.record_gaps, trust_vm);
    // Ports are mapped back to the original numbers inside the lane's jobs; the
    // reply says which fork port each original became.
    if marsh_contracts::break_mode(breaks::LANE_PORTS_UNMAPPED) {
        restore.ports.clear();
    }
    LaneOutcome { restore, generated }
}

/// The port map of a lane started from a **checkpoint of a lane**: the
/// restore remapped the source lane's published numbers, and the user knows the
/// project's original ones. `source` is the source lane's map (original to
/// published); `restored` is the new lane's (the source's published numbers to
/// the new ones). The result maps each original to the new number. A number the
/// source did not remap (a port the lane published itself) stays as it is.
#[must_use]
pub fn compose_ports(
    source: &BTreeMap<String, u16>,
    restored: &BTreeMap<String, u16>,
) -> BTreeMap<String, u16> {
    if source.is_empty() {
        return restored.clone();
    }
    let originals: BTreeMap<String, &String> = source
        .iter()
        .map(|(original, published)| (published.to_string(), original))
        .collect();
    restored
        .iter()
        .map(|(number, fork)| {
            (
                originals
                    .get(number)
                    .map_or_else(|| number.clone(), |original| (*original).clone()),
                *fork,
            )
        })
        .collect()
}

// ------------------------------------------------------------ lane scopes

/// The compose project name of a lane's fork: lowercase letters, digits, `-`
/// and `_`, at most 48 characters, and unique to the lane (its split id is in
/// it; a name that had to be changed carries a short digest of the workspace).
#[must_use]
pub fn fork_name(workspace: &str) -> String {
    let lowered: String = workspace
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    if lowered == workspace && lowered.len() <= 48 {
        return lowered;
    }
    let digest = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        workspace.hash(&mut hasher);
        format!("{:08x}", hasher.finish() & 0xffff_ffff)
    };
    let keep: String = lowered.chars().take(39).collect();
    format!("{keep}-{digest}")
}

// ------------------------------------------------------------- lane drops

/// A lane whose services are still to be dropped (the VM was not ready, or
/// the agent left something). Journaled, so a restart does not forget it.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PendingDrop {
    pub root: PathBuf,
    pub workspace: String,
}

/// The journal of pending drops (`lane-drops.json` in the daemon's home).
pub struct LaneDrops {
    path: PathBuf,
    pending: std::sync::Mutex<BTreeSet<PendingDrop>>,
}

impl LaneDrops {
    #[must_use]
    pub fn open(journal_home: &Path) -> Self {
        let path = journal_home.join("lane-drops.json");
        let pending = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BTreeSet<PendingDrop>>(&bytes).ok())
            .unwrap_or_default();
        Self {
            path,
            pending: std::sync::Mutex::new(pending),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeSet<PendingDrop>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn save(&self, pending: &BTreeSet<PendingDrop>) {
        if let Err(error) = crate::split::SplitEngine::write_durable(&self.path, pending) {
            eprintln!("marshd: could not journal the pending lane drops: {error}");
        }
    }

    /// Drops one lane's services now; what could not be dropped is journaled
    /// and retried by [`Self::retry`].
    pub fn drop_lane(&self, backend: &dyn DaemonBackend, root: &Path, workspace: &str) {
        if breaks::drop_off() {
            return;
        }
        let entry = PendingDrop {
            root: root.to_owned(),
            workspace: workspace.to_owned(),
        };
        {
            // Journaled first: a crash between here and the drop is retried.
            let mut pending = self.lock();
            if pending.insert(entry.clone()) {
                self.save(&pending);
            }
        }
        self.try_drop(backend, &entry);
    }

    fn try_drop(&self, backend: &dyn DaemonBackend, entry: &PendingDrop) {
        let finished = match backend.services_drop_workspace(&entry.root, &entry.workspace) {
            DropOutcome::Nothing => true,
            DropOutcome::Dropped { left, .. } if left.is_empty() => true,
            DropOutcome::Dropped { left, .. } => {
                eprintln!(
                    "marshd: dropping lane {} left {}; will retry",
                    entry.workspace,
                    left.join(", ")
                );
                false
            }
            DropOutcome::Later(_) => false,
        };
        if finished {
            let mut pending = self.lock();
            if pending.remove(entry) {
                self.save(&pending);
            }
        }
    }

    /// Retries every pending drop (from the services timer).
    pub fn retry(&self, backend: &dyn DaemonBackend) {
        let entries: Vec<PendingDrop> = self.lock().iter().cloned().collect();
        for entry in entries {
            self.try_drop(backend, &entry);
        }
    }

    /// The lanes still waiting to be dropped.
    #[must_use]
    pub fn pending(&self) -> Vec<PendingDrop> {
        self.lock().iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    //! Table-driven checks of the Mac's side of the restore: what it refuses
    //! from the VM, and the shape of the line. The end-to-end evidence is
    //! `tests/acceptance/services_c3_lanes.py`.
    use super::*;
    use serde_json::json;

    type Defect = (&'static str, Box<dyn Fn(&mut Value)>);

    fn prepared() -> Value {
        json!({
            "ok": true,
            "restored": [{"kind": "service", "stack": "s", "service": "db", "volumes": ["pgdata"]}],
            "fresh": [],
            "gaps": [],
            "failures": [],
            "ports": {"5432": 15432},
            "files": {".env": "A=1\n", "compose.fork.yaml": "services: {}\n"},
        })
    }

    #[test]
    fn a_good_result_is_accepted_and_every_defect_is_refused() {
        assert!(validate(&prepared(), true).is_ok());
        let defects: Vec<Defect> = vec![
            (
                "file name",
                Box::new(|v| v["files"]["../../x/.env"] = json!("x")),
            ),
            (
                "other file",
                Box::new(|v| v["files"]["id_rsa"] = json!("x")),
            ),
            (
                "folder dots",
                Box::new(|v| v["files"]["a/../.env"] = json!("x")),
            ),
            (
                "marsh folder",
                Box::new(|v| v["files"][".marsh/.env"] = json!("x")),
            ),
            (
                "deep",
                Box::new(|v| v["files"]["a/b/c/d/e/.env"] = json!("x")),
            ),
            (
                "oversized",
                Box::new(|v| v["files"][".env"] = json!("A".repeat(MAX_FILE_BYTES + 1))),
            ),
            ("nul", Box::new(|v| v["files"][".env"] = json!("a\u{0}b"))),
            ("not text", Box::new(|v| v["files"][".env"] = json!(5))),
            ("port zero", Box::new(|v| v["ports"] = json!({"0": 1}))),
            (
                "port range",
                Box::new(|v| v["ports"] = json!({"5432": 70000})),
            ),
            ("port text", Box::new(|v| v["ports"] = json!({"abc": 1}))),
            ("port sign", Box::new(|v| v["ports"] = json!({"+5432": 1}))),
            (
                "port dup",
                Box::new(|v| v["ports"] = json!({"1": 9, "2": 9})),
            ),
            ("port not int", Box::new(|v| v["ports"] = json!({"1": 9.5}))),
            (
                "service name",
                Box::new(|v| v["restored"][0]["service"] = json!("a b")),
            ),
            (
                "service newline",
                Box::new(|v| v["restored"][0]["service"] = json!("a\nb")),
            ),
            (
                "service long",
                Box::new(|v| v["restored"][0]["service"] = json!("a".repeat(129))),
            ),
            (
                "volume name",
                Box::new(|v| v["restored"][0]["volumes"] = json!(["../x"])),
            ),
            (
                "kind",
                Box::new(|v| v["restored"][0]["kind"] = json!("shell")),
            ),
            ("gaps type", Box::new(|v| v["gaps"] = json!("one"))),
            (
                "gaps oversized",
                Box::new(|v| v["gaps"] = json!(["x".repeat(TEXT_REFUSE_BYTES + 1)])),
            ),
            (
                "no verdict",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("ok");
                }),
            ),
            (
                "no files",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("files");
                }),
            ),
            (
                "too many files",
                Box::new(|v| {
                    for n in 0..20 {
                        v["files"][format!("d{n}/.env")] = json!("x");
                    }
                }),
            ),
        ];
        for (what, change) in defects {
            let mut value = prepared();
            change(&mut value);
            assert!(validate(&value, true).is_err(), "{what} must be refused");
        }
    }

    /// The reply travels as an internally tagged frame (buffered, so map keys
    /// are text): a restore with ports must survive it.
    #[test]
    fn a_restore_with_ports_crosses_the_reply_frame() {
        let mut restore = lane(&["db"], &[], true);
        restore.ports = BTreeMap::from([("5432".to_owned(), 15432), ("1025".to_owned(), 11025)]);
        let reply = crate::PublicReply::SplitOpened {
            id: "abc".into(),
            lanes: BTreeMap::new(),
            restore: Some(BTreeMap::from([("a".to_owned(), restore.clone())])),
            restore_note: None,
        };
        let wire = serde_json::to_vec(&reply).unwrap();
        let back: crate::PublicReply = serde_json::from_slice(&wire).unwrap();
        match back {
            crate::PublicReply::SplitOpened {
                restore: Some(map), ..
            } => {
                assert_eq!(map["a"], restore);
                assert_eq!(
                    map["a"].port_map(),
                    BTreeMap::from([(5432, 15432), (1025, 11025)])
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_lane_of_a_lane_names_the_original_ports_again() {
        let map = |pairs: &[(&str, u16)]| -> BTreeMap<String, u16> {
            pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
        };
        // 5432 -> 15432 in the first lane; the second restore moved 15432 -> 25432.
        let composed = compose_ports(
            &map(&[("5432", 15432), ("6379", 16379)]),
            &map(&[("15432", 25432), ("16379", 26379), ("9999", 29999)]),
        );
        assert_eq!(
            composed,
            map(&[("5432", 25432), ("6379", 26379), ("9999", 29999)])
        );
        // A checkpoint of the project itself: the restore's own map.
        assert_eq!(
            compose_ports(&map(&[]), &map(&[("5432", 15432)])),
            map(&[("5432", 15432)])
        );
    }

    #[test]
    fn the_second_phase_repeats_the_files_and_is_validated_the_same_way() {
        let mut up = prepared();
        up["check"] = json!({"ok": true, "containers": [{"service": "db", "state": "running", "health": "healthy"}],
            "problems": []});
        let fields = validate(&up, false).unwrap();
        assert!(
            fields.files.is_empty(),
            "the second phase's files are never written"
        );
        assert!(fields.check.as_ref().unwrap().ok);
        up["files"]["../x/.env"] = json!("x");
        assert!(validate(&up, false).is_err());
        let mut no_check = prepared();
        assert!(validate(&no_check, false).is_err());
        no_check["check"] =
            json!({"ok": true, "containers": [{"service": "db", "state": "bogus"}]});
        assert!(validate(&no_check, false).is_err());
    }

    #[test]
    fn text_from_the_vm_is_made_plain_and_bounded_not_shown_raw() {
        let mut value = prepared();
        value["fresh"] = json!([{"kind": "service", "service": "mail",
            "reason": "\u{1b}[31mred\u{1b}[0m\r\nnext\u{202e}line"}]);
        value["gaps"] = json!(["a\u{1b}[2Jb"]);
        let fields = validate(&value, true).unwrap();
        assert!(!fields.fresh[0].reason.contains('\u{1b}'));
        assert!(!fields.fresh[0].reason.contains('\n'));
        assert!(!fields.fresh[0].reason.contains('\u{202e}'));
        assert!(!fields.gaps[0].contains('\u{1b}'));
    }

    fn lane(restored: &[&str], fresh: &[(&str, &str, &str)], ok: bool) -> LaneRestore {
        LaneRestore {
            ok,
            line: String::new(),
            restored: restored
                .iter()
                .map(|name| RestoredItem {
                    kind: "service".into(),
                    service: Some((*name).into()),
                    ..RestoredItem::default()
                })
                .collect(),
            fresh: fresh
                .iter()
                .map(|(kind, name, reason)| FreshItem {
                    kind: (*kind).into(),
                    service: (*kind == "service").then(|| (*name).into()),
                    name: (*kind != "service").then(|| (*name).into()),
                    reason: (*reason).into(),
                    stack: None,
                })
                .collect(),
            gaps: Vec::new(),
            ports: BTreeMap::new(),
            check: None,
            folder: None,
            problem: None,
        }
    }

    fn r10_words(line: &str) -> bool {
        let lower = line.to_ascii_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        words.iter().any(|w| {
            matches!(
                *w,
                "volume"
                    | "volumes"
                    | "snapshot"
                    | "snapshots"
                    | "btrfs"
                    | "xfs"
                    | "reflink"
                    | "pool"
                    | "vm"
            )
        })
    }

    #[test]
    fn the_line_is_one_short_line_in_the_macs_words() {
        let mut cases: Vec<LaneRestore> = vec![
            lane(&["db", "app"], &[], true),
            lane(
                &["db", "app"],
                &[
                    (
                        "service",
                        "redis",
                        "no saved data: it keeps nothing in volumes",
                    ),
                    (
                        "service",
                        "mail",
                        "no saved data: it keeps nothing in volumes",
                    ),
                    ("testcontainers", "tc-1", "not restored"),
                ],
                true,
            ),
            lane(
                &["db"],
                &[("service", "app", "not restored: it did not start")],
                false,
            ),
            lane(&[], &[], false),
        ];
        // 200-character names, 30 services, multi-byte names, hostile reasons.
        cases.push(lane(
            &[&"é".repeat(200)],
            &[(
                "service",
                &"x".repeat(200),
                "its saved data for 'pgdata' was incomplete",
            )],
            false,
        ));
        let many: Vec<String> = (0..30).map(|n| format!("service-number-{n}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        cases.push(lane(
            &many,
            &many
                .iter()
                .map(|n| ("service", *n, "not restored: it cannot be forked safely"))
                .collect::<Vec<_>>(),
            false,
        ));
        for mut case in cases {
            let ok = case.ok;
            case.line = line(&case, None, false);
            let text = &case.line;
            assert!(!text.contains('\n'), "{text}");
            assert!(
                text.chars().count() <= LINE_CHARS,
                "{} chars: {text}",
                text.chars().count()
            );
            assert!(!r10_words(text), "{text}");
            if !ok {
                assert!(text.contains("Not everything came back"), "{text}");
            }
        }
    }

    #[test]
    fn the_line_names_what_came_back_and_what_did_not() {
        let mut case = lane(
            &["db", "app"],
            &[
                (
                    "service",
                    "redis",
                    "no saved data: it keeps nothing in volumes",
                ),
                ("testcontainers", "tc-1", "not restored"),
            ],
            true,
        );
        case.restored.push(RestoredItem {
            kind: "volume".into(),
            name: Some("fork-x-notes".into()),
            from: Some("notes".into()),
            containers: vec!["keeper".into()],
            restarted: Some(false),
            ..RestoredItem::default()
        });
        let text = line(&case, None, false);
        assert!(
            text.starts_with("Local data restored: db, app (Docker)."),
            "{text}"
        );
        assert!(
            text.contains("keeper as fork-x-notes (not started)"),
            "{text}"
        );
        assert!(text.contains("tc-1 (not restored)"), "{text}");
        assert!(text.contains("redis (no saved data)"), "{text}");
        // The agent's own words, which hold R10 words, are not in it.
        assert!(!text.contains("keeps nothing"), "{text}");
    }

    fn folder(copied: &[&str], skipped: &[(&str, &str, bool)]) -> FolderData {
        FolderData {
            files: copied.len() as u64,
            bytes: 10,
            copied: copied.iter().map(|c| (*c).to_owned()).collect(),
            skipped: skipped
                .iter()
                .map(|(path, reason, lost)| crate::ignored::Skipped {
                    path: (*path).into(),
                    reason: (*reason).into(),
                    lost: *lost,
                })
                .collect(),
        }
    }

    /// What came along from the folder and from Docker, across both places
    /// (table of the line's shape).
    #[test]
    fn the_line_names_local_data_from_the_folder_and_docker() {
        let redis = (
            "service",
            "redis",
            "no saved data: it keeps nothing in volumes",
        );
        for (restore, expected) in [
            (
                lane(&["db"], &[redis], true)
                    .with_folder(folder(&[".wrangler/state", ".env"], &[])),
                "Local data restored: .wrangler/state, .env, db (Docker). Started empty: redis (no saved data).",
            ),
            (
                lane(&["db"], &[], true).with_folder(folder(
                    &[],
                    &[("node_modules", "wasn't copied: tools rebuild it", false)],
                )),
                "Local data restored: db (Docker).",
            ),
            (
                LaneRestore::folder_only(folder(&[".env"], &[])),
                "Local data restored: .env.",
            ),
            (
                LaneRestore::folder_only(folder(
                    &[".env"],
                    &[(
                        "big",
                        "over the size limit of 2 GiB for saved local data",
                        true,
                    )],
                )),
                "Local data restored: .env. Not everything came back. Not copied: big (over the size limit).",
            ),
            (
                lane(&[], &[], true).with_folder(folder(&[".env"], &[])),
                "Local data restored: .env.",
            ),
            (
                LaneRestore::failed(Problem::Off, Vec::new()).with_folder(folder(&[".env"], &[])),
                "Local data restored: .env. Docker is turned off for this project, so no Docker data was restored.",
            ),
        ] {
            assert_eq!(restore.line, expected);
            assert!(!r10_words(&restore.line), "{}", restore.line);
        }
        let lost = LaneRestore::folder_only(folder(
            &[".env"],
            &[("big", "over the size limit of 2 GiB", true)],
        ));
        assert!(!lost.ok);
        assert_eq!(lost.gaps, ["not copied: big (over the size limit)"]);
        assert!(LaneRestore::folder_only(folder(&[".env"], &[])).ok);
        // An empty folder leaves the result as it was.
        let plain = lane(&["db"], &[], true);
        assert_eq!(plain.clone().with_folder(FolderData::default()), plain);
    }

    #[test]
    fn a_failed_restore_says_what_the_mac_knows_and_the_gaps_say_the_rest() {
        let failed = LaneRestore::failed(Problem::NotSaved, vec!["no snapshot".into()]);
        assert!(!failed.ok);
        assert!(failed.line.contains("wasn't saved"), "{}", failed.line);
        for problem in [
            Problem::NotSaved,
            Problem::Off,
            Problem::Unavailable,
            Problem::Failed,
            Problem::BadResult,
            Problem::LaneFiles,
            Problem::Aborted,
            Problem::LaneName,
        ] {
            let text = LaneRestore::failed(problem, Vec::new()).line;
            assert!(!r10_words(&text), "{text}");
            assert!(text.chars().count() <= LINE_CHARS);
        }
    }

    #[test]
    fn fork_names_are_valid_compose_projects_and_unique_per_lane() {
        for workspace in [
            "fork-abc123def456-claude",
            "fork-abc123def456-Claude",
            "fork-abc123def456-a.b",
            &format!("fork-abc123def456-{}", "z".repeat(40)),
        ] {
            let name = fork_name(workspace);
            assert!(name.len() <= 48, "{name}");
            assert!(
                name.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'-'
                    || b == b'_'),
                "{name}"
            );
        }
        assert_ne!(
            fork_name("fork-abc123def456-A"),
            fork_name("fork-abc123def456-a")
        );
    }

    #[test]
    fn lane_files_are_written_beneath_the_held_directory_and_never_through_a_link() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir_all(root.join("split/lane/sub")).unwrap();
        std::fs::create_dir_all(root.join("canary")).unwrap();
        std::fs::write(root.join("canary/env"), b"CANARY").unwrap();
        let split = Dir::open_root(&root.join("split")).unwrap();
        let lane = split.open("lane").unwrap();
        let identity = lane.fstat().unwrap().identity;
        // `.env` is a symlink to a canary: the write replaces the link.
        symlink(root.join("canary/env"), root.join("split/lane/.env")).unwrap();
        let files = vec![
            LaneFile {
                dir: vec![],
                name: ENV_NAME,
                content: "A=1\n".into(),
            },
            LaneFile {
                dir: vec!["sub".into()],
                name: OVERRIDE_NAME,
                content: "x: 1\n".into(),
            },
        ];
        write_lane_files(&split, "lane", identity, &root.join("split/lane"), &files).unwrap();
        assert_eq!(std::fs::read(root.join("canary/env")).unwrap(), b"CANARY");
        let written = std::fs::symlink_metadata(root.join("split/lane/.env")).unwrap();
        assert!(written.file_type().is_file());
        assert_eq!(
            std::fs::read(root.join("split/lane/.env")).unwrap(),
            b"A=1\n"
        );
        assert_eq!(
            std::fs::read(root.join("split/lane/sub/compose.fork.yaml")).unwrap(),
            b"x: 1\n"
        );
        // No temporary file is left behind.
        let left: Vec<_> = std::fs::read_dir(root.join("split/lane"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        // A lane replaced by another directory is refused.
        std::fs::rename(root.join("split/lane"), root.join("split/moved")).unwrap();
        std::fs::create_dir(root.join("split/lane")).unwrap();
        let error = write_lane_files(&split, "lane", identity, &root.join("split/lane"), &files)
            .unwrap_err();
        assert!(error.contains("replaced"), "{error}");
        // A lane that became a symlink is refused.
        std::fs::remove_dir(root.join("split/lane")).unwrap();
        symlink(root.join("canary"), root.join("split/lane")).unwrap();
        assert!(
            write_lane_files(&split, "lane", identity, &root.join("split/lane"), &files).is_err()
        );
        assert_eq!(std::fs::read(root.join("canary/env")).unwrap(), b"CANARY");
        assert!(!root.join("canary/compose.fork.yaml").exists());
    }
}
