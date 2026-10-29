//! Detached cloud runs (`docs/design/cloud-detach.md`): the durable record
//! written before the run starts, collection (read-only `state`, `tail`,
//! `fetch`; never a second start), retire-before-apply, the base-checked
//! write-back that keeps a conflicting result as a patch, and the CLI's
//! control requests.

use super::{
    CONTROL_TIMEOUT, Ctl, STEP_TIMEOUT, Settlement, SharedBuffer, Step, actionable,
    failed_before_start, os_args, retried_step, step, summary,
};
use crate::{StockDaemonBackend, backend_error};
use marsh_contracts::cloud::{Kind, MAX_ARCHIVE, Reader};
use marsh_daemon::cloud_detached::{
    CloudDetachedReply, CloudDetachedRequest, DETACHED_CHUNK, DetachedView,
};
use marsh_daemon::cloud_fs;
use marsh_daemon::split_fs::Dir;
use marsh_daemon::{
    AttachmentFrame, CleanupState, DaemonError, DaemonStore, ExecutionOutcome, ExitStatus, JobState,
};
use marsh_sbx::cloud::{CloudError, CloudSandboxId, CloudSbx};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// A detached run's time limit unless `--ttl` is given.
pub(super) const DEFAULT_TTL: Duration = Duration::from_hours(2);
/// How often outstanding detached runs are collected.
const COLLECT_INTERVAL: Duration = Duration::from_mins(2);
/// How long an uncollected sandbox is kept (from the job's start).
const RETENTION: Duration = Duration::from_hours(14 * 24);
/// INT, then KILL after this grace (`marsh cloud stop`).
const STOP_GRACE: Duration = Duration::from_secs(10);
/// A stopped (paused) sandbox starts on the next exec; allow for that.
const RESUME_TIMEOUT: Duration = Duration::from_mins(5);
/// At most one remote output refresh per job this often (attach).
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// A run that never reported its start is given up after this long.
const START_GRACE: Duration = Duration::from_mins(10);
const RECORD_VERSION: u32 = 1;

/// The durable record of one detached run, in
/// `control_home/cloud/detached/JOB.json` (fsynced before the run starts).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // Independent persisted facts of one record.
pub(super) struct Record {
    pub version: u32,
    pub job_id: String,
    pub cursor: u64,
    pub sandbox_id: String,
    pub sandbox_name: String,
    /// Control steps run as the Kit's user (a direct Kit call).
    pub kit: bool,
    /// What ran, for display (the request's argv; its environment is not
    /// kept here).
    pub command: Vec<String>,
    pub cwd: PathBuf,
    /// The destination: the project or split fork, and its identity.
    pub root: PathBuf,
    pub root_identity: (u64, u64),
    pub fork: bool,
    pub ttl_seconds: u64,
    /// SHA-256 of `JOB.base`: path, type, mode and SHA-256 of every
    /// uploaded entry (the remote helper's hashed baseline).
    pub base_sha256: String,
    pub created_unix_ms: u64,
    pub state: String,
    #[serde(default)]
    pub stdout: u64,
    #[serde(default)]
    pub stderr: u64,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub settled: bool,
    #[serde(default)]
    pub notified: bool,
    #[serde(default)]
    pub sandbox_gone: bool,
    #[serde(default)]
    pub finished_unix_ms: Option<u64>,
    /// The Docker ID whose token the run's coordinator signed in with
    /// (`marsh config cloud-delegate on`); never the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate: Option<String>,
    /// Agent sandboxes the run's coordinator reported owning, last seen:
    /// deleted with the run if it could not delete them itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_sandboxes: Vec<ChildSandbox>,
}

/// An agent sandbox a detached run's coordinator created (its record).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct ChildSandbox {
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub agent: Option<String>,
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn io_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::InvalidState(error.to_string())
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Write `bytes` to `path` durably: a private temporary file, fsync,
/// rename, and fsync of the directory.
fn durable_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path.parent().unwrap_or(Path::new("/"));
    private_dir(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    File::open(directory)?.sync_all()
}

/// A job id is a UUID: safe as a file name.
fn valid_job_id(job: &str) -> bool {
    !job.is_empty()
        && job.len() <= 64
        && job
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

pub(super) struct Paths {
    pub detached: PathBuf,
    pub results: PathBuf,
}

impl Paths {
    pub(super) fn new(control_home: &Path) -> Self {
        let cloud = control_home.join("cloud");
        Self {
            detached: cloud.join("detached"),
            results: cloud.join("results"),
        }
    }
    fn record(&self, job: &str) -> PathBuf {
        self.detached.join(format!("{job}.json"))
    }
    pub(super) fn base(&self, job: &str) -> PathBuf {
        self.detached.join(format!("{job}.base"))
    }
    fn output(&self, job: &str, stream: u8) -> PathBuf {
        self.detached.join(format!("{job}.out{stream}"))
    }
    fn payload(&self, job: &str) -> PathBuf {
        self.results.join(format!("{job}.delta"))
    }
    fn diff(&self, job: &str) -> PathBuf {
        self.results.join(format!("{job}.diff"))
    }
}

impl Record {
    fn load(paths: &Paths, job: &str) -> Result<Self, DaemonError> {
        if !valid_job_id(job) {
            return Err(DaemonError::NotFound(job.into()));
        }
        let bytes = fs::read(paths.record(job)).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => {
                DaemonError::Refused(format!("cloud: job {job} is not a detached cloud run"))
            }
            _ => io_error(error),
        })?;
        let record: Self = serde_json::from_slice(&bytes).map_err(io_error)?;
        if record.version != RECORD_VERSION || record.job_id != job {
            return Err(DaemonError::InvalidState(format!(
                "cloud: the detached record of {job} is not readable by this marsh"
            )));
        }
        Ok(record)
    }

    pub(super) fn save(&self, paths: &Paths) -> Result<(), DaemonError> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io_error)?;
        durable_write(&paths.record(&self.job_id), &bytes).map_err(io_error)
    }

    fn all(paths: &Paths) -> Vec<Self> {
        let Ok(entries) = fs::read_dir(&paths.detached) else {
            return Vec::new();
        };
        let mut records = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let job = name.to_str()?.strip_suffix(".json")?.to_owned();
                Self::load(paths, &job).ok()
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|record| record.cursor);
        records
    }

    fn view(&self) -> DetachedView {
        DetachedView {
            job_id: self.job_id.clone(),
            cursor: self.cursor,
            state: self.state.clone(),
            exit_code: self.exit_code,
            detail: self.detail.clone(),
            settled: self.settled,
            output_truncated: self.truncated,
        }
    }

    /// The one-line finish notice.
    fn notice(&self) -> String {
        let exit = self
            .exit_code
            .map_or_else(String::new, |code| format!(" (exit {code})"));
        match self.state.as_str() {
            "collected" => format!(
                "marsh: cloud job {} finished{exit}; {}",
                self.cursor,
                if self.detail.is_empty() {
                    "no file changes"
                } else {
                    &self.detail
                }
            ),
            "kept-as-patch" => format!(
                "marsh: cloud job {} finished{exit}; {}. Review: marsh cloud diff {}",
                self.cursor, self.detail, self.cursor
            ),
            state => format!(
                "marsh: cloud job {} {state}{exit}{}",
                self.cursor,
                if self.detail.is_empty() {
                    String::new()
                } else {
                    format!(": {}", self.detail)
                }
            ),
        }
    }
}

/// What the remote `state` step reports.
#[derive(Debug, Default, Deserialize)]
struct RemoteState {
    started: bool,
    supervisor_alive: bool,
    status: Option<i32>,
    done: bool,
    failed: Option<String>,
    failed_start: Option<String>,
    stdout: Option<Range>,
    stderr: Option<Range>,
    /// Agent calls the coordinator finished.
    #[serde(default)]
    children: Vec<RemoteCall>,
    /// Agent sandboxes the coordinator owns now.
    #[serde(default)]
    sandboxes: Vec<ChildSandbox>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteCall {
    kit: String,
    #[serde(default)]
    args: Vec<String>,
    exit: i32,
    #[serde(default)]
    summary: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct Range {
    start: u64,
    end: u64,
}

/// Per-job collection locks and timing, beside the cloud adapter.
#[derive(Default)]
pub(crate) struct Collector {
    jobs: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    refreshed: Mutex<BTreeMap<String, Instant>>,
    last_pass: Mutex<Option<Instant>>,
}

impl Collector {
    fn lock(&self, job: &str) -> Arc<Mutex<()>> {
        Arc::clone(
            self.jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(job.to_owned())
                .or_default(),
        )
    }
}

/// One control step in a detached sandbox, retried when the stock CLI
/// failed before reaching it (every detached step is repeatable).
fn control_step(
    adapter: &CloudSbx,
    id: &CloudSandboxId,
    ctl: Ctl,
    args: &[&str],
    sink: impl Fn() -> io::Result<Box<dyn io::Write + Send>>,
    limit: u64,
    timeout: Duration,
) -> Result<Step, String> {
    let (program, words) = ctl.step(args);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let attachment = adapter
            .control_detached(id, &program, &os_args(&words))
            .map_err(|error| actionable(&error))?;
        let result = step(
            attachment,
            None,
            sink().map_err(|e| e.to_string())?,
            limit,
            timeout,
        )?;
        if attempt >= 4 || !failed_before_start(&result) {
            return Ok(result);
        }
        thread::sleep(Duration::from_secs(2 * attempt));
    }
}

fn small_control(
    adapter: &CloudSbx,
    id: &CloudSandboxId,
    ctl: Ctl,
    args: &[&str],
    limit: u64,
    timeout: Duration,
) -> Result<(Step, Vec<u8>), String> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let step = control_step(
        adapter,
        id,
        ctl,
        args,
        || {
            let buffer = Arc::clone(&buffer);
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            Ok(Box::new(SharedBuffer(buffer)))
        },
        limit,
        timeout,
    )?;
    let bytes = std::mem::take(
        &mut *buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    Ok((step, bytes))
}

/// An entry's contents as the base check compares them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Entry {
    Absent,
    Directory(u32),
    File(u32, Vec<u8>),
    Symlink(Vec<u8>),
    /// Something else (a device, a socket) or unreadable: never equal.
    Other,
}

fn hash_reader(reader: &mut dyn io::Read) -> io::Result<Vec<u8>> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_vec())
}

/// The uploaded entries' digests (the remote baseline's format).
fn read_base(path: &Path, expected: &str) -> Result<BTreeMap<Vec<u8>, Entry>, String> {
    let bytes = fs::read(path).map_err(|e| format!("base manifest: {e}"))?;
    if super::hex(&Sha256::digest(&bytes)) != expected {
        return Err("base manifest changed since the run started".into());
    }
    parse_base(bytes)
}

/// A remote baseline (`--internal-cloud baseline`) as the base check reads it.
pub(super) fn parse_base(bytes: Vec<u8>) -> Result<BTreeMap<Vec<u8>, Entry>, String> {
    let mut reader = Reader::new(io::Cursor::new(bytes), u64::MAX).map_err(|e| e.to_string())?;
    let mut entries = BTreeMap::new();
    while let Some(header) = reader.next_header().map_err(|e| e.to_string())? {
        let entry = match header.kind {
            Kind::Directory => Entry::Directory(header.mode),
            Kind::File => Entry::File(header.mode, reader.data().map_err(|e| e.to_string())?),
            Kind::Symlink => Entry::Symlink(reader.data().map_err(|e| e.to_string())?),
            Kind::Delete | Kind::Request => return Err("invalid base manifest".into()),
        };
        entries.insert(header.path, entry);
    }
    Ok(entries)
}

/// The delta's resulting entries (verifies the whole archive).
pub(super) fn read_result(path: &Path) -> Result<Vec<(Vec<u8>, Entry)>, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut reader = Reader::new(BufReader::new(file), MAX_ARCHIVE).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    while let Some(header) = reader.next_header().map_err(|e| e.to_string())? {
        let entry = match header.kind {
            Kind::Delete => Entry::Absent,
            Kind::Directory => Entry::Directory(header.mode),
            Kind::File => {
                let mut hasher = Sha256::new();
                reader
                    .copy_data(&mut HashSink(&mut hasher))
                    .map_err(|e| e.to_string())?;
                Entry::File(header.mode, hasher.finalize().to_vec())
            }
            Kind::Symlink => Entry::Symlink(reader.data().map_err(|e| e.to_string())?),
            Kind::Request => return Err("a delta carries no request".into()),
        };
        entries.push((header.path, entry));
    }
    Ok(entries)
}

/// The last check and the import, one step under the root's import lock:
/// `None` (nothing written) when the run's lane split was cancelled first.
fn import_unless_cancelled<R>(
    store: &DaemonStore,
    job: &str,
    lock: &Mutex<()>,
    import: impl FnOnce() -> R,
) -> Option<R> {
    let _held = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (!store.in_cancelled_split(job)).then(import)
}

struct HashSink<'a>(&'a mut Sha256);

impl io::Write for HashSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The entry at `relative` under `root`, never following a link.
fn local_entry(root: &Dir, relative: &[u8]) -> Entry {
    let parts = relative.split(|byte| *byte == b'/').collect::<Vec<_>>();
    let Some((name, parents)) = parts.split_last() else {
        return Entry::Other;
    };
    let Ok(mut dir) = root.dup() else {
        return Entry::Other;
    };
    for parent in parents {
        dir = match dir.open(OsStr::from_bytes(parent)) {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Entry::Absent,
            Err(_) => return Entry::Other,
        };
    }
    let name = OsStr::from_bytes(name);
    let stat = match dir.stat(name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Entry::Absent,
        Err(_) => return Entry::Other,
    };
    match stat.kind {
        rustix::fs::FileType::Directory => Entry::Directory(stat.mode & 0o777),
        rustix::fs::FileType::Symlink => dir.read_link(name).map_or(Entry::Other, |target| {
            Entry::Symlink(target.as_bytes().to_vec())
        }),
        rustix::fs::FileType::RegularFile => dir
            .open_file(name)
            .and_then(|mut file| hash_reader(&mut file))
            .map_or(Entry::Other, |digest| {
                Entry::File(stat.mode & 0o777, digest)
            }),
        _ => Entry::Other,
    }
}

/// Paths the delta would change whose local entry is neither the uploaded
/// one nor already the result.
fn conflicts(
    root: &Dir,
    base: &BTreeMap<Vec<u8>, Entry>,
    result: &[(Vec<u8>, Entry)],
) -> Vec<String> {
    conflicting(root, base, result)
        .iter()
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect()
}

/// [`conflicts`], as the paths' bytes.
pub(super) fn conflicting(
    root: &Dir,
    base: &BTreeMap<Vec<u8>, Entry>,
    result: &[(Vec<u8>, Entry)],
) -> Vec<Vec<u8>> {
    result
        .iter()
        .filter(|(path, after)| {
            let local = local_entry(root, path);
            let before = base.get(path).cloned().unwrap_or(Entry::Absent);
            // A directory's mode change alone is not a conflict.
            let same = |left: &Entry, right: &Entry| match (left, right) {
                (Entry::Directory(_), Entry::Directory(_)) => true,
                _ => left == right,
            };
            !(same(&local, &before) || same(&local, after))
        })
        .map(|(path, _)| path.clone())
        .collect()
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

impl StockDaemonBackend {
    fn detached_paths(&self) -> Paths {
        Paths::new(&self.config.control_home)
    }

    /// Detached runs not yet settled.
    pub(super) fn outstanding_detached(&self) -> usize {
        Record::all(&self.detached_paths())
            .iter()
            .filter(|record| !record.settled)
            .count()
    }

    /// Start a detached run in a prepared sandbox (snapshot uploaded): keep
    /// the base manifest, write the durable record, mark the sandbox
    /// detached, start the run once, then leave the local pool. The caller
    /// gets the job id and returns at once.
    #[allow(clippy::too_many_arguments)] // The prepared node's identities, handed off whole.
    #[allow(clippy::too_many_lines)] // One ordered record, mark, start, detach sequence.
    pub(super) fn start_detached(
        &self,
        settle: Settlement<'_>,
        store: &DaemonStore,
        adapter: &Arc<CloudSbx>,
        id: &CloudSandboxId,
        name: &str,
        ctl: Ctl,
        command: Vec<String>,
        cwd: PathBuf,
        root: PathBuf,
        identity: (u64, u64),
        fork: bool,
        ttl: Duration,
        delegate: Option<String>,
    ) -> Result<(), DaemonError> {
        let paths = self.detached_paths();
        let job_id = settle.job_id.clone();
        let failed = |settle: Settlement<'_>, message: String| {
            settle.note(&message);
            let _ = adapter.quarantine(id);
            let cleanup = match adapter.retire(id) {
                Ok(()) => CleanupState::Verified,
                Err(_) => CleanupState::Uncertain,
            };
            let _ = fs::remove_file(paths.record(&job_id));
            let _ = fs::remove_file(paths.base(&job_id));
            settle.finish(
                ExecutionOutcome::NotStarted,
                125,
                format!("cloud_setup_failed: {message}"),
                cleanup,
            )
        };
        if let Err(error) = private_dir(&paths.detached).and_then(|()| private_dir(&paths.results))
        {
            return failed(settle, error.to_string());
        }
        // The base manifest: the helper's hashed record of every uploaded entry.
        let part = paths.detached.join(format!("{job_id}.base.part"));
        let fetched = (|| -> Result<(), String> {
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&part)
                .map_err(|e| e.to_string())?;
            let (program, words) = ctl.step(&["--internal-cloud", "baseline"]);
            let attachment = adapter
                .spawn(id, &program, &os_args(&words))
                .map_err(|e| actionable(&e))?;
            let result = step(
                attachment,
                None,
                Box::new(io::BufWriter::new(file)),
                MAX_ARCHIVE,
                STEP_TIMEOUT,
            )?;
            if !result.ok() {
                return Err(result.diagnostic());
            }
            let bytes = fs::read(&part).map_err(|e| e.to_string())?;
            durable_write(&paths.base(&job_id), &bytes).map_err(|e| e.to_string())?;
            let _ = fs::remove_file(&part);
            Ok(())
        })();
        if let Err(message) = fetched {
            let _ = fs::remove_file(&part);
            return failed(settle, format!("base manifest: {message}"));
        }
        let base_sha256 = match fs::read(paths.base(&job_id)) {
            Ok(bytes) => super::hex(&Sha256::digest(&bytes)),
            Err(error) => return failed(settle, format!("base manifest: {error}")),
        };
        let cursor = store.job(&job_id).map(|job| job.cursor).unwrap_or_default();
        let mut record = Record {
            version: RECORD_VERSION,
            job_id: job_id.clone(),
            cursor,
            sandbox_id: id.as_str().to_owned(),
            sandbox_name: name.to_owned(),
            kit: ctl.kit,
            command,
            cwd,
            root,
            root_identity: identity,
            fork,
            ttl_seconds: ttl.as_secs(),
            base_sha256,
            created_unix_ms: now_ms(),
            state: "starting".into(),
            stdout: 0,
            stderr: 0,
            truncated: false,
            exit_code: None,
            detail: String::new(),
            settled: false,
            notified: false,
            sandbox_gone: false,
            finished_unix_ms: None,
            delegate,
            child_sandboxes: Vec::new(),
        };
        // Durable before the run can start.
        if let Err(error) = record.save(&paths) {
            return failed(settle, error.to_string());
        }
        if let Err(error) = adapter.mark_detached(id) {
            return failed(settle, actionable(&error));
        }
        if let Err(error) = store.mark_cloud_running(&job_id, id.as_str()) {
            return failed(settle, error.to_string());
        }
        // At most once: the helper's exclusive `started` marker refuses a
        // second start, so a retry after a pre-start CLI failure is safe.
        let (program, words) = ctl.step(&["--internal-cloud", "run", "--detached"]);
        let started = retried_step(adapter, id, &program, &words, None, CONTROL_TIMEOUT);
        let confirmed = matches!(&started, Ok(step) if step.ok()
            && String::from_utf8_lossy(&step.stdout).lines().any(|line| line == "started"));
        // From here the sandbox is controlled only through collection.
        if let Err(error) = adapter.detach(id) {
            settle.note(&format!("detaching the sandbox record: {error}"));
        }
        let mut timing = settle.timing.clone();
        timing.wall_ms = u64::try_from(settle.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Err(error) = store.detach_cloud_job(&job_id, timing) {
            settle.note(&format!("leaving the local job pool: {error}"));
        }
        if confirmed {
            record.state = "running".into();
            let _ = record.save(&paths);
            settle.note(&format!(
                "job {cursor} is running detached in Docker Cloud. Follow it: marsh cloud attach {cursor}; \
                 its changes come back by themselves when it finishes (time limit {}, which pauses it rather than stopping it)",
                super::detach::format_ttl(ttl)
            ));
            return settle.attachment.send(&AttachmentFrame::Exited { code: 0 });
        }
        // The start was not confirmed: find out now, never start it again.
        let diagnostic = match &started {
            Ok(step) => step.diagnostic(),
            Err(message) => message.clone(),
        };
        let diagnostic = diagnostic
            .strip_prefix("marsh cloud: ")
            .map_or_else(|| diagnostic.clone(), str::to_owned);
        let collected = self.collect_job(store, &job_id, Trigger::User);
        let record = collected.unwrap_or(record);
        let code = if record.settled {
            settle.note(&format!(
                "the detached run did not start: {}",
                if diagnostic.is_empty() {
                    record.detail.clone()
                } else {
                    diagnostic
                }
            ));
            125
        } else {
            settle.note(&format!(
                "job {cursor} detached, but its start was not confirmed ({diagnostic}); \
                 marsh checks it again by itself; see marsh cloud collect {cursor}"
            ));
            0
        };
        settle.attachment.send(&AttachmentFrame::Exited { code })
    }

    /// Collect one job: read its state; tail new output; when it is done,
    /// fetch its delta, delete the sandbox, then write back (or keep).
    #[allow(clippy::too_many_lines)] // One ordered observe, tail, fetch, retire, write-back pass.
    fn collect_job(
        &self,
        store: &DaemonStore,
        job: &str,
        trigger: Trigger,
    ) -> Result<Record, DaemonError> {
        let paths = self.detached_paths();
        let lock = self.cloud.collector.lock(job);
        let _held = if trigger == Trigger::Refresh {
            match lock.try_lock() {
                Ok(held) => held,
                Err(_) => return Record::load(&paths, job),
            }
        } else {
            lock.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let mut record = Record::load(&paths, job)?;
        if record.settled {
            return Ok(record);
        }
        // A cancel of the run's lane split outlives this daemon (it is
        // journaled with the split): the run is discarded on every pass until
        // its sandbox is verified gone, and nothing is written back.
        if store.in_cancelled_split(job) {
            return self.discard_loaded(store, &paths, record);
        }
        if record.sandbox_gone {
            // Fetched and the sandbox is deleted: only the write-back is left.
            return self.write_back(store, &paths, record);
        }
        let adapter = self.cloud.adapter(self)?;
        let id = CloudSandboxId::parse(record.sandbox_id.clone()).map_err(backend_error)?;
        let ctl = Ctl { kit: record.kit };
        let age = Duration::from_millis(now_ms().saturating_sub(record.created_unix_ms));
        if age > retention() {
            return Self::discard_unsettled(
                store,
                &paths,
                &adapter,
                &id,
                record,
                "lost",
                "kept longer than the retention period; deleted",
            );
        }
        let status = match adapter.detached_status(&id) {
            Ok(status) => status,
            Err(CloudError::NotOwned(_)) => None,
            Err(error) => {
                record.detail = format!("cannot reach the sandbox now ({error}); marsh retries");
                record.save(&paths)?;
                return Ok(record);
            }
        };
        let Some(status) = status else {
            // Absent from a complete inventory: verify, then it is lost.
            let _ = adapter.retire(&id);
            if let Err(message) = delete_children(&adapter, &record.child_sandboxes) {
                record.detail = format!(
                    "its sandbox is gone; deleting its agent sandboxes ({message}); marsh retries"
                );
                record.save(&paths)?;
                return Ok(record);
            }
            record.child_sandboxes.clear();
            return Self::settle(
                store,
                &paths,
                record,
                Settle {
                    label: "lost",
                    detail: "its sandbox is gone; nothing came back".into(),
                    execution: ExecutionOutcome::Unknown,
                    exit: 125,
                    final_state: Some(JobState::Unknown),
                },
            );
        };
        match status.as_str() {
            "running" => {}
            // A stopped sandbox starts on the next exec; resuming continues
            // the paused run, which is what collection is for.
            "stopped" => {
                record.state = "paused".into();
                record.detail = "paused at its time limit; collecting resumes it".into();
                record.save(&paths)?;
                store.set_detached_state(job, "paused");
                if trigger == Trigger::Refresh {
                    return Ok(record);
                }
            }
            other => {
                // Stopping, hibernating, starting: try again when it settles.
                if record.state != "starting" {
                    record.state = "paused".into();
                    store.set_detached_state(job, "paused");
                }
                record.detail = format!("the sandbox is {other}; marsh retries");
                record.save(&paths)?;
                return Ok(record);
            }
        }
        let timeout = if status == "running" {
            CONTROL_TIMEOUT
        } else {
            RESUME_TIMEOUT
        };
        let remote = match small_control(
            &adapter,
            &id,
            ctl,
            &["--internal-cloud", "state"],
            64 * 1024,
            timeout,
        ) {
            Ok((step, bytes)) if step.ok() => serde_json::from_slice::<RemoteState>(&bytes)
                .map_err(|e| format!("unreadable state: {e}")),
            Ok((step, _)) => Err(step.diagnostic()),
            Err(message) => Err(message),
        };
        let remote = match remote {
            Ok(remote) => remote,
            Err(message) => {
                record.detail =
                    format!("cannot read the run's state now ({message}); marsh retries");
                record.save(&paths)?;
                return Ok(record);
            }
        };
        if !remote.started {
            if record.state == "starting" && age < START_GRACE {
                return Ok(record);
            }
            return Self::discard_unsettled(
                store,
                &paths,
                &adapter,
                &id,
                record,
                "failed",
                "the run never started",
            );
        }
        Self::tail_output(&adapter, &id, ctl, &paths, &mut record, &remote);
        record.child_sandboxes.clone_from(&remote.sandboxes);
        record.save(&paths)?;
        if !remote.children.is_empty() {
            store.set_cloud_children(
                job,
                remote
                    .children
                    .iter()
                    .map(|call| marsh_daemon::CloudChildCall {
                        kit: call.kit.clone(),
                        args: call.args.clone(),
                        exit: call.exit,
                        summary: call.summary.clone(),
                    })
                    .collect(),
            );
        }
        if !remote.done {
            if !remote.supervisor_alive && remote.status.is_none() {
                // The supervisor is gone without an end (the sandbox was
                // restarted, not resumed): nothing will finish it.
                return Self::discard_unsettled(
                    store,
                    &paths,
                    &adapter,
                    &id,
                    record,
                    "lost",
                    "the run stopped without recording its end",
                );
            }
            if record.state != "running" {
                record.state = "running".into();
                record.detail.clear();
                record.save(&paths)?;
                store.set_detached_state(job, "running");
            }
            return Ok(record);
        }
        // Done: the status is the workload's.
        record.exit_code = remote.status;
        if let Some(message) = remote.failed_start {
            // The workload never ran: not started (125), not its 127.
            record.exit_code = None;
            return Self::discard_unsettled(
                store,
                &paths,
                &adapter,
                &id,
                record,
                "failed",
                &format!("the run never started: {}", message.trim()),
            );
        }
        if let Some(message) = remote.failed {
            return Self::discard_unsettled(
                store,
                &paths,
                &adapter,
                &id,
                record,
                "failed",
                &format!("its changes could not be prepared: {}", message.trim()),
            );
        }
        let payload = paths.payload(job);
        let fetched = (|| -> Result<(), String> {
            let part = paths.results.join(format!("{job}.delta.part"));
            let sink_path = part.clone();
            let result = control_step(
                &adapter,
                &id,
                ctl,
                &["--internal-cloud", "fetch"],
                move || {
                    let file = OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .mode(0o600)
                        .open(&sink_path)?;
                    Ok(Box::new(io::BufWriter::new(file)) as Box<dyn io::Write + Send>)
                },
                MAX_ARCHIVE,
                STEP_TIMEOUT,
            )?;
            if !result.ok() {
                let _ = fs::remove_file(&part);
                return Err(result.diagnostic());
            }
            // Verify: a complete, valid archive.
            read_result(&part)?;
            File::open(&part)
                .and_then(|file| file.sync_all())
                .map_err(|e| e.to_string())?;
            fs::rename(&part, &payload).map_err(|e| e.to_string())
        })();
        if let Err(message) = fetched {
            record.detail = format!("downloading its changes ({message}); marsh retries");
            record.save(&paths)?;
            return Ok(record);
        }
        record.state = "finished".into();
        record.finished_unix_ms = Some(now_ms());
        record.detail = "deleting its sandbox".into();
        record.save(&paths)?;
        store.set_detached_state(job, "finished");
        // Agent sandboxes the coordinator could not delete go first.
        if let Err(message) = delete_children(&adapter, &record.child_sandboxes) {
            record.detail = format!("deleting its agent sandboxes ({message}); marsh retries");
            record.save(&paths)?;
            return Ok(record);
        }
        record.child_sandboxes.clear();
        // Retire before apply: the result is local and verified.
        match retire_settled(&adapter, &id) {
            Ok(()) => {
                record.sandbox_gone = true;
                record.save(&paths)?;
            }
            Err(message) => {
                record.detail = format!("deleting its sandbox ({message}); marsh retries");
                record.save(&paths)?;
                return Ok(record);
            }
        }
        self.write_back(store, &paths, record)
    }

    /// Fetch output past what is stored, appending to the job's files.
    fn tail_output(
        adapter: &CloudSbx,
        id: &CloudSandboxId,
        ctl: Ctl,
        paths: &Paths,
        record: &mut Record,
        remote: &RemoteState,
    ) {
        for (stream, range) in [(1_u8, remote.stdout), (2, remote.stderr)] {
            let Some(range) = range else { continue };
            let stored = if stream == 1 {
                record.stdout
            } else {
                record.stderr
            };
            if stored < range.start {
                record.truncated = true;
            }
            for _ in 0..64 {
                let offset = if stream == 1 {
                    record.stdout
                } else {
                    record.stderr
                };
                // Everything before `offset` is stored; local files hold
                // only what arrived, so a truncated start is recorded.
                if offset >= range.end {
                    break;
                }
                let offset_text = offset.to_string();
                let stream_text = stream.to_string();
                let Ok((step, bytes)) = small_control(
                    adapter,
                    id,
                    ctl,
                    &["--internal-cloud", "tail", &stream_text, &offset_text],
                    (1 << 20) + 64,
                    CONTROL_TIMEOUT,
                ) else {
                    return;
                };
                if !step.ok() {
                    return;
                }
                let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
                    return;
                };
                let Some(start) = std::str::from_utf8(&bytes[..newline])
                    .ok()
                    .and_then(|start| start.parse::<u64>().ok())
                else {
                    return;
                };
                let data = &bytes[newline + 1..];
                if start > offset {
                    record.truncated = true;
                }
                let appended = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(paths.output(&record.job_id, stream))
                    .and_then(|mut file| file.write_all(data).and_then(|()| file.sync_data()));
                if appended.is_err() {
                    return;
                }
                let next = start + data.len() as u64;
                if stream == 1 {
                    record.stdout = next;
                } else {
                    record.stderr = next;
                }
                if data.is_empty() {
                    break;
                }
            }
        }
    }

    /// The base check, then `apply_delta` when every touched path is as
    /// uploaded (or already as returned); otherwise keep the patch.
    fn write_back(
        &self,
        store: &DaemonStore,
        paths: &Paths,
        mut record: Record,
    ) -> Result<Record, DaemonError> {
        let payload = paths.payload(&record.job_id);
        let keep = |record: Record, detail: String| {
            Self::settle(
                store,
                paths,
                record,
                Settle {
                    label: "kept-as-patch",
                    detail,
                    execution: ExecutionOutcome::Unknown,
                    exit: 0,
                    final_state: None,
                },
            )
        };
        let Ok(root) = Dir::open_root(&record.root) else {
            let place = if record.fork {
                "its split fork is gone"
            } else {
                "its workspace is gone"
            };
            return keep(record, place.into());
        };
        if root.fstat().map(|stat| stat.identity).ok() != Some(record.root_identity) {
            return keep(record, "its workspace was replaced since it started".into());
        }
        let base = match read_base(&paths.base(&record.job_id), &record.base_sha256) {
            Ok(base) => base,
            Err(message) => return keep(record, message),
        };
        let result = match read_result(&payload) {
            Ok(result) => result,
            Err(message) => return keep(record, format!("its changes are unreadable: {message}")),
        };
        let changed = conflicts(&root, &base, &result);
        if !changed.is_empty() {
            return keep(
                record,
                format!(
                    "{} changed here since it started",
                    plural(changed.len(), "file", "files")
                ),
            );
        }
        // A scope lifecycle transition (`marsh stop`) waits for this write.
        let Ok(_admission) = store.begin_background_request() else {
            record.detail = "write-back waits for a lifecycle change to finish".into();
            record.save(paths)?;
            return Ok(record);
        };
        // The last check and the import are one step under the root's import
        // lock: a cancel of the run's lane split (marked under the store
        // lock) that came before this check discards the run instead; one
        // that comes after it finds the run live (a lane waits for it), so
        // nothing is written into a copy after its cancel has finished.
        let applied = import_unless_cancelled(
            store,
            &record.job_id,
            &self.cloud.import_lock(&record.root),
            || cloud_fs::apply_delta(&record.root, record.root_identity, &payload, MAX_ARCHIVE),
        );
        let Some(applied) = applied else {
            return self.discard_loaded(store, paths, record);
        };
        match applied {
            Ok(report) => {
                let _ = fs::remove_file(&payload);
                let detail = if report.total() > 0 || !report.kept.is_empty() {
                    format!("changes applied: {}", summary(&report))
                } else {
                    "no file changes".into()
                };
                Self::settle(
                    store,
                    paths,
                    record,
                    Settle {
                        label: "collected",
                        detail,
                        execution: ExecutionOutcome::Unknown,
                        exit: 0,
                        final_state: None,
                    },
                )
            }
            Err(error) => keep(
                record,
                format!(
                    "write-back stopped ({}; {} already written)",
                    error.message,
                    summary(&error.applied)
                ),
            ),
        }
    }

    /// Delete the sandbox of an unsettled run and settle it.
    #[allow(clippy::too_many_arguments)] // The record, its sandbox, and the outcome.
    fn discard_unsettled(
        store: &DaemonStore,
        paths: &Paths,
        adapter: &CloudSbx,
        id: &CloudSandboxId,
        mut record: Record,
        label: &'static str,
        detail: &str,
    ) -> Result<Record, DaemonError> {
        // The run's agent sandboxes go with it: ask its coordinator (the
        // run may be stopped; this resumes it), else use the last list seen.
        if record.delegate.is_some() {
            // Stop the workload first, so its coordinator starts no further
            // agent sandbox after the list below (and deletes its own).
            let _ = small_control(
                adapter,
                id,
                Ctl { kit: record.kit },
                &["--internal-cloud", "signal", "KILL"],
                64 * 1024,
                RESUME_TIMEOUT,
            );
            if let Ok((step, bytes)) = small_control(
                adapter,
                id,
                Ctl { kit: record.kit },
                &["--internal-cloud", "state"],
                64 * 1024,
                RESUME_TIMEOUT,
            ) && step.ok()
                && let Ok(remote) = serde_json::from_slice::<RemoteState>(&bytes)
            {
                record.child_sandboxes = remote.sandboxes;
            }
            if let Err(message) = delete_children(adapter, &record.child_sandboxes) {
                record.detail =
                    format!("{detail}; deleting its agent sandboxes ({message}); marsh retries");
                record.save(paths)?;
                return Ok(record);
            }
            record.child_sandboxes.clear();
        }
        if let Err(message) = retire_settled(adapter, id) {
            record.detail = format!("{detail}; deleting its sandbox ({message}); marsh retries");
            record.save(paths)?;
            return Ok(record);
        }
        record.sandbox_gone = true;
        let (execution, exit, final_state) = match (label, record.exit_code) {
            ("failed", Some(code)) => (ExecutionOutcome::Exited { code }, code, None),
            ("failed", None) => (ExecutionOutcome::NotStarted, 125, None),
            ("discarded", _) => (ExecutionOutcome::Unknown, 130, Some(JobState::Cancelled)),
            _ => (ExecutionOutcome::Unknown, 125, Some(JobState::Unknown)),
        };
        Self::settle(
            store,
            paths,
            record,
            Settle {
                label,
                detail: detail.into(),
                execution,
                exit,
                final_state,
            },
        )
    }

    /// Settle the receipt and the record (once).
    fn settle(
        store: &DaemonStore,
        paths: &Paths,
        mut record: Record,
        outcome: Settle,
    ) -> Result<Record, DaemonError> {
        record.state = outcome.label.into();
        record.detail = outcome.detail;
        record.settled = true;
        record.notified = false;
        record.finished_unix_ms.get_or_insert_with(now_ms);
        let execution = match (outcome.execution, record.exit_code) {
            (ExecutionOutcome::Unknown, Some(code)) if record.sandbox_gone => {
                ExecutionOutcome::Exited { code }
            }
            (execution, _) => execution,
        };
        let code = match execution {
            ExecutionOutcome::Exited { code } => code,
            _ => outcome.exit,
        };
        let cause = if record.detail.is_empty() {
            outcome.label.to_owned()
        } else {
            format!("{}: {}", outcome.label, record.detail)
        };
        let cleanup = if record.sandbox_gone {
            CleanupState::Verified
        } else {
            CleanupState::Uncertain
        };
        let mut timing = store
            .job(&record.job_id)
            .map(|job| job.timing)
            .unwrap_or_default();
        // The job's time is the run's, not its start step's.
        if let Some(finished) = record.finished_unix_ms {
            timing.wall_ms = finished.saturating_sub(record.created_unix_ms);
        }
        record.save(paths)?;
        if store
            .finish_detached_job(
                &record.job_id,
                execution,
                ExitStatus {
                    code: Some(code),
                    cause,
                },
                true,
                cleanup,
                timing,
                outcome.label,
                outcome.final_state,
            )
            .is_err()
        {
            // Already settled (a restart repaired its receipt): label it.
            store.relabel_settled_detached(&record.job_id, outcome.label);
        }
        Ok(record)
    }

    /// The collector's pass: every outstanding run at daemon start, then
    /// every [`COLLECT_INTERVAL`] while any is outstanding.
    pub(crate) fn collect_detached_pass(&self, store: &DaemonStore) {
        // A cloud job that was still being set up when an earlier daemon went
        // away left a sandbox (billing since its creation) that no run will
        // use: retire it.
        let orphans = store.cloud_setup_orphans();
        if !orphans.is_empty()
            && let Ok(adapter) = self.cloud.adapter(self)
        {
            for orphan in orphans {
                let Ok(id) = CloudSandboxId::parse(orphan.clone()) else {
                    store.cloud_setup_orphan_retired(&orphan);
                    continue;
                };
                // An ownership read that fails is not "not owned": retry.
                let Ok(records) = adapter.records() else {
                    continue;
                };
                let owned = records.iter().any(|r| r.id.as_ref() == Some(&id));
                if !owned || adapter.retire(&id).is_ok() {
                    store.cloud_setup_orphan_retired(&orphan);
                } else {
                    eprintln!("marshd: could not retire the setup sandbox {orphan}; marsh retries");
                }
            }
        }
        let paths = self.detached_paths();
        let records = Record::all(&paths);
        // Settled records (and their output) are kept for the retention period.
        for record in records.iter().filter(|record| record.settled) {
            let finished = record.finished_unix_ms.unwrap_or(record.created_unix_ms);
            if Duration::from_millis(now_ms().saturating_sub(finished)) > retention()
                && record.state != "kept-as-patch"
            {
                for path in [
                    paths.record(&record.job_id),
                    paths.base(&record.job_id),
                    paths.output(&record.job_id, 1),
                    paths.output(&record.job_id, 2),
                    paths.diff(&record.job_id),
                ] {
                    let _ = fs::remove_file(path);
                }
            }
        }
        let outstanding = records
            .iter()
            .filter(|record| !record.settled)
            .map(|record| record.job_id.clone())
            .collect::<Vec<_>>();
        if outstanding.is_empty() {
            return;
        }
        {
            let mut last = self
                .cloud
                .collector
                .last_pass
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if last.is_some_and(|at| at.elapsed() < COLLECT_INTERVAL) {
                return;
            }
            *last = Some(Instant::now());
        }
        for job in outstanding {
            if let Err(error) = self.collect_job(store, &job, Trigger::Timer) {
                eprintln!("marshd: collecting cloud job {job}: {error}");
            }
        }
    }

    /// `marsh cloud attach|collect|diff|apply|discard|stop`, notices.
    #[allow(clippy::too_many_lines)] // One dispatch over the request kinds.
    pub(crate) fn cloud_detached_request(
        &self,
        request: CloudDetachedRequest,
        store: &DaemonStore,
    ) -> Result<CloudDetachedReply, DaemonError> {
        let paths = self.detached_paths();
        let resolve = |selector: &str| -> Result<String, DaemonError> {
            let job = store.job(selector)?;
            if !paths.record(&job.job_id).exists() {
                return Err(DaemonError::Refused(format!(
                    "cloud: job {} is not a detached cloud run",
                    job.cursor
                )));
            }
            Ok(job.job_id)
        };
        match request {
            CloudDetachedRequest::Collect { job } => {
                let job = resolve(&job)?;
                let record = self.collect_job(store, &job, Trigger::User)?;
                Ok(CloudDetachedReply {
                    message: describe(&record),
                    view: Some(record.view()),
                    ..CloudDetachedReply::default()
                })
            }
            CloudDetachedRequest::Output {
                job,
                stream,
                offset,
                refresh,
            } => {
                let job = resolve(&job)?;
                if !matches!(stream, 1 | 2) {
                    return Err(DaemonError::Refused("cloud: unknown output stream".into()));
                }
                let mut record = Record::load(&paths, &job)?;
                if refresh && !record.settled && self.refresh_due(&job) {
                    record = self.collect_job(store, &job, Trigger::Refresh)?;
                }
                let (bytes, total) = read_chunk(&paths.output(&job, stream), offset)?;
                Ok(CloudDetachedReply {
                    view: Some(record.view()),
                    bytes,
                    offset,
                    total,
                    ..CloudDetachedReply::default()
                })
            }
            CloudDetachedRequest::Diff { job, offset } => {
                let job = resolve(&job)?;
                let record = Record::load(&paths, &job)?;
                if !paths.payload(&job).exists() {
                    return Err(DaemonError::Refused(format!(
                        "cloud: job {} has no kept patch ({})",
                        record.cursor, record.state
                    )));
                }
                if offset == 0 {
                    let text = render_diff(&record, &paths).map_err(io_error)?;
                    durable_write(&paths.diff(&job), &text).map_err(io_error)?;
                }
                let (bytes, total) = read_chunk(&paths.diff(&job), offset)?;
                Ok(CloudDetachedReply {
                    view: Some(record.view()),
                    bytes,
                    offset,
                    total,
                    ..CloudDetachedReply::default()
                })
            }
            CloudDetachedRequest::Apply { job } => {
                let job = resolve(&job)?;
                self.apply_kept(store, &paths, &job)
            }
            CloudDetachedRequest::Discard { job } => {
                let job = resolve(&job)?;
                let record = self.discard(store, &paths, &job)?;
                Ok(CloudDetachedReply {
                    message: format!("cloud job {}: {}", record.cursor, record.state),
                    view: Some(record.view()),
                    ..CloudDetachedReply::default()
                })
            }
            CloudDetachedRequest::Stop { job } => {
                let job = resolve(&job)?;
                self.stop_detached(store, &paths, &job)
            }
            CloudDetachedRequest::Sync { job } => {
                let targets = {
                    let syncs = self
                        .cloud
                        .syncs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match &job {
                        None => syncs.values().cloned().collect::<Vec<_>>(),
                        Some(selector) => {
                            let job = store.job(selector)?;
                            vec![syncs.get(&job.job_id).cloned().ok_or_else(|| {
                                DaemonError::Refused(format!(
                                    "cloud: job {} is not a running attached cloud block from a session; \
                                     only those sync (a detached run's changes come back when it is collected)",
                                    job.cursor
                                ))
                            })?]
                        }
                    }
                };
                let synced = targets
                    .iter()
                    .map(|target| super::sync::sync(target))
                    .collect::<Vec<_>>();
                Ok(CloudDetachedReply {
                    message: if synced.is_empty() {
                        "no running attached cloud blocks to sync".into()
                    } else {
                        String::new()
                    },
                    synced,
                    ..CloudDetachedReply::default()
                })
            }
            CloudDetachedRequest::DiscardAll => {
                let mut discarded = 0_u64;
                let mut failures = Vec::new();
                for record in Record::all(&paths).into_iter().filter(|r| !r.settled) {
                    match self.discard(store, &paths, &record.job_id) {
                        Ok(record) if record.settled => discarded += 1,
                        Ok(record) => {
                            failures.push(format!("{}: {}", record.cursor, record.detail));
                        }
                        Err(error) => failures.push(format!("{}: {error}", record.cursor)),
                    }
                }
                if !failures.is_empty() {
                    return Err(DaemonError::InvalidState(format!(
                        "cloud: some detached runs could not be discarded: {}",
                        failures.join("; ")
                    )));
                }
                Ok(CloudDetachedReply {
                    message: format!(
                        "discarded {}",
                        plural(
                            usize::try_from(discarded).unwrap_or(usize::MAX),
                            "detached cloud run",
                            "detached cloud runs"
                        )
                    ),
                    total: discarded,
                    ..CloudDetachedReply::default()
                })
            }
            CloudDetachedRequest::Outstanding => Ok(CloudDetachedReply {
                total: self.outstanding_detached() as u64,
                ..CloudDetachedReply::default()
            }),
            CloudDetachedRequest::Notices => {
                let mut notices = Vec::new();
                for mut record in Record::all(&paths) {
                    if record.settled && !record.notified {
                        notices.push(record.notice());
                        record.notified = true;
                        let _ = record.save(&paths);
                    }
                }
                Ok(CloudDetachedReply {
                    notices,
                    total: self.outstanding_detached() as u64,
                    ..CloudDetachedReply::default()
                })
            }
        }
    }

    fn refresh_due(&self, job: &str) -> bool {
        let mut refreshed = self
            .cloud
            .collector
            .refreshed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let due = refreshed
            .get(job)
            .is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL);
        if due {
            refreshed.insert(job.to_owned(), Instant::now());
        }
        due
    }

    fn apply_kept(
        &self,
        store: &DaemonStore,
        paths: &Paths,
        job: &str,
    ) -> Result<CloudDetachedReply, DaemonError> {
        let lock = self.cloud.collector.lock(job);
        let _held = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut record = Record::load(paths, job)?;
        let payload = paths.payload(job);
        if record.state != "kept-as-patch" || !payload.exists() {
            return Err(DaemonError::Refused(format!(
                "cloud: job {} has no kept patch to apply ({})",
                record.cursor, record.state
            )));
        }
        let identity = Dir::open_root(&record.root)
            .and_then(|root| root.fstat())
            .map(|stat| stat.identity)
            .ok();
        if identity != Some(record.root_identity) {
            return Err(DaemonError::Refused(format!(
                "cloud: {} is gone or was replaced; the patch stays at {}",
                record.root.display(),
                payload.display()
            )));
        }
        let applied = {
            let lock = self.cloud.import_lock(&record.root);
            let _held = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cloud_fs::apply_delta(&record.root, record.root_identity, &payload, MAX_ARCHIVE)
        };
        match applied {
            Ok(report) => {
                let _ = fs::remove_file(&payload);
                let _ = fs::remove_file(paths.diff(job));
                record.state = "collected".into();
                record.detail = format!("patch applied: {}", summary(&report));
                record.notified = true;
                record.save(paths)?;
                store.relabel_settled_detached(job, "collected");
                Ok(CloudDetachedReply {
                    message: format!("cloud job {}: {}", record.cursor, record.detail),
                    view: Some(record.view()),
                    ..CloudDetachedReply::default()
                })
            }
            Err(error) => Err(DaemonError::InvalidState(format!(
                "cloud: write-back stopped: {}; {} already written; the patch stays at {}",
                error.message,
                summary(&error.applied),
                payload.display()
            ))),
        }
    }

    fn discard(
        &self,
        store: &DaemonStore,
        paths: &Paths,
        job: &str,
    ) -> Result<Record, DaemonError> {
        let lock = self.cloud.collector.lock(job);
        let _held = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record = Record::load(paths, job)?;
        self.discard_loaded(store, paths, record)
    }

    /// `discard` with the collector lock held and the record loaded.
    fn discard_loaded(
        &self,
        store: &DaemonStore,
        paths: &Paths,
        mut record: Record,
    ) -> Result<Record, DaemonError> {
        let job = record.job_id.clone();
        let job = job.as_str();
        let _ = fs::remove_file(paths.payload(job));
        let _ = fs::remove_file(paths.diff(job));
        if record.settled {
            if record.state == "kept-as-patch" {
                record.state = "discarded".into();
                record.detail = "the kept patch was discarded".into();
                record.notified = true;
                record.save(paths)?;
                store.relabel_settled_detached(job, "discarded");
            }
            return Ok(record);
        }
        if record.sandbox_gone {
            return Self::settle(
                store,
                paths,
                record,
                Settle {
                    label: "discarded",
                    detail: "nothing was written back".into(),
                    execution: ExecutionOutcome::Unknown,
                    exit: 130,
                    final_state: Some(JobState::Cancelled),
                },
            );
        }
        let adapter = self.cloud.adapter(self)?;
        let id = CloudSandboxId::parse(record.sandbox_id.clone()).map_err(backend_error)?;
        let mut record = Self::discard_unsettled(
            store,
            paths,
            &adapter,
            &id,
            record,
            "discarded",
            "nothing was written back",
        )?;
        record.notified = true;
        record.save(paths)?;
        Ok(record)
    }

    fn stop_detached(
        &self,
        store: &DaemonStore,
        paths: &Paths,
        job: &str,
    ) -> Result<CloudDetachedReply, DaemonError> {
        let record = Record::load(paths, job)?;
        if record.settled || record.sandbox_gone {
            return Ok(CloudDetachedReply {
                message: format!(
                    "cloud job {} has already ended ({})",
                    record.cursor, record.state
                ),
                view: Some(record.view()),
                ..CloudDetachedReply::default()
            });
        }
        let adapter = self.cloud.adapter(self)?;
        let id = CloudSandboxId::parse(record.sandbox_id.clone()).map_err(backend_error)?;
        let ctl = Ctl { kit: record.kit };
        let done = || {
            small_control(
                &adapter,
                &id,
                ctl,
                &["--internal-cloud", "state"],
                64 * 1024,
                RESUME_TIMEOUT,
            )
            .ok()
            .filter(|(step, _)| step.ok())
            .and_then(|(_, bytes)| serde_json::from_slice::<RemoteState>(&bytes).ok())
            .is_some_and(|state| state.done || state.status.is_some())
        };
        for signal in ["INT", "KILL"] {
            if done() {
                break;
            }
            let _ = small_control(
                &adapter,
                &id,
                ctl,
                &["--internal-cloud", "signal", signal],
                64 * 1024,
                RESUME_TIMEOUT,
            );
            let deadline = Instant::now() + STOP_GRACE;
            while Instant::now() < deadline && !done() {
                thread::sleep(Duration::from_secs(1));
            }
        }
        let record = self.collect_job(store, job, Trigger::User)?;
        Ok(CloudDetachedReply {
            message: describe(&record),
            view: Some(record.view()),
            ..CloudDetachedReply::default()
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Trigger {
    Timer,
    User,
    /// An attach refresh: skipped while another collection runs.
    Refresh,
}

struct Settle {
    label: &'static str,
    detail: String,
    execution: ExecutionOutcome,
    exit: i32,
    final_state: Option<JobState>,
}

fn retention() -> Duration {
    std::env::var("MARSH_CLOUD_DETACHED_RETENTION")
        .ok()
        .and_then(|text| super::duration(&text))
        .unwrap_or(RETENTION)
}

pub(super) fn format_ttl(ttl: Duration) -> String {
    let seconds = ttl.as_secs();
    if seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// Delete a sandbox, retrying while it is in a transitional state (a
/// delete during hibernation is refused until it settles).
fn retire_settled(adapter: &CloudSbx, id: &CloudSandboxId) -> Result<(), String> {
    let mut last = String::new();
    for attempt in 0..6 {
        match adapter.retire(id) {
            Ok(()) | Err(CloudError::NotOwned(_)) => return Ok(()),
            Err(error) => last = error.to_string(),
        }
        thread::sleep(Duration::from_secs(5 * (attempt + 1)));
    }
    Err(last)
}

/// Delete agent sandboxes a detached run's coordinator left: each is
/// adopted into this daemon's ownership (only while a complete inventory
/// lists that name with that ID), then deleted and its absence observed.
fn delete_children(adapter: &CloudSbx, children: &[ChildSandbox]) -> Result<(), String> {
    let mut failures = Vec::new();
    for child in children {
        // A create that never reported its ID is found by name (the record
        // is the coordinator's own, root-only), or it never happened.
        let id = match child.id.clone().map(CloudSandboxId::parse) {
            Some(Ok(id)) => id,
            Some(Err(_)) => {
                failures.push(format!("{}: invalid recorded ID", child.name));
                continue;
            }
            None => match adapter
                .inventory()
                .map_err(|error| error.to_string())?
                .get(&child.name)
            {
                Some(vm) => vm.id.clone(),
                None => continue,
            },
        };
        match adapter.adopt(&child.name, &id, &child.image, child.agent.as_deref()) {
            Ok(false) => {}
            Ok(true) => {
                if let Err(message) = retire_settled(adapter, &id) {
                    failures.push(format!("{}: {message}", child.name));
                }
            }
            Err(error) => failures.push(format!("{}: {error}", child.name)),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// One chunk of a local file from `offset`, and its length.
fn read_chunk(path: &Path, offset: u64) -> Result<(Vec<u8>, u64), DaemonError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(io_error(error)),
    };
    let total = file.metadata().map_err(io_error)?.len();
    let mut bytes = Vec::new();
    if offset < total {
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        file.take(DETACHED_CHUNK as u64)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
    }
    Ok((bytes, total))
}

fn describe(record: &Record) -> String {
    let exit = record
        .exit_code
        .map_or_else(String::new, |code| format!(", exit {code}"));
    let detail = if record.detail.is_empty() {
        String::new()
    } else {
        format!(": {}", record.detail)
    };
    format!(
        "cloud job {}: {}{exit}{detail}",
        record.cursor, record.state
    )
}

/// A reviewable diff of a kept patch against the files here now.
fn render_diff(record: &Record, paths: &Paths) -> io::Result<Vec<u8>> {
    use std::fmt::Write as _;
    let payload = paths.payload(&record.job_id);
    let mut reader = Reader::new(BufReader::new(File::open(&payload)?), MAX_ARCHIVE)?;
    let root = Dir::open_root(&record.root).ok();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# cloud job {} ({}): changes kept as a patch for {}",
        record.cursor,
        record.state,
        record.root.display()
    );
    let _ = writeln!(
        out,
        "# apply: marsh cloud apply {0}   drop: marsh cloud discard {0}\n",
        record.cursor
    );
    let local_bytes = |path: &[u8]| -> Option<Vec<u8>> {
        let root = root.as_ref()?;
        let parts = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
        let (name, parents) = parts.split_last()?;
        let mut dir = root.dup().ok()?;
        for parent in parents {
            dir = dir.open(OsStr::from_bytes(parent)).ok()?;
        }
        dir.read(OsStr::from_bytes(name), 64 << 20).ok()
    };
    while let Some(header) = reader.next_header()? {
        let path = String::from_utf8_lossy(&header.path).into_owned();
        match header.kind {
            Kind::Delete => {
                let _ = writeln!(out, "deleted: {path}");
            }
            Kind::Directory => {
                let _ = writeln!(out, "directory: {path} (mode {:o})", header.mode);
            }
            Kind::Symlink => {
                let target = reader.data()?;
                let _ = writeln!(
                    out,
                    "symlink: {path} -> {}",
                    String::from_utf8_lossy(&target)
                );
            }
            Kind::File => {
                let mut new = Vec::new();
                reader.copy_data(&mut new)?;
                let old = local_bytes(&header.path);
                let text = |bytes: &[u8]| {
                    std::str::from_utf8(bytes)
                        .ok()
                        .filter(|text| !text.contains('\0'))
                        .map(ToOwned::to_owned)
                };
                let old_text = old.as_deref().and_then(text);
                let new_text = text(&new);
                match (old.is_some(), old_text, new_text) {
                    (present, old_text, Some(new_text)) if !present || old_text.is_some() => {
                        let old_text = old_text.unwrap_or_default();
                        let diff = similar::TextDiff::from_lines(&old_text, &new_text);
                        let header_old = if present {
                            format!("a/{path} (here)")
                        } else {
                            "/dev/null".into()
                        };
                        let _ = write!(
                            out,
                            "{}",
                            diff.unified_diff().context_radius(3).header(
                                &header_old,
                                &format!("b/{path} (cloud job {})", record.cursor)
                            )
                        );
                        if old_text == new_text {
                            let _ = writeln!(out, "mode: {path} -> {:o}", header.mode);
                        }
                    }
                    _ => {
                        let _ = writeln!(out, "binary file: {path} ({} bytes)", new.len());
                    }
                }
            }
            Kind::Request => {}
        }
    }
    Ok(out.into_bytes())
}

/// A registered Kit with a cloud form that a detached block would call
/// (it needs the laptop relay): the first word in a command position that
/// names one. Best effort over the block's text; the command links in the
/// sandbox refuse such a call too.
pub(super) fn nested_call(
    mode: marsh_contracts::cloud::CloudMode,
    argv: &[Vec<u8>],
    bound: &BTreeMap<String, super::Binding>,
) -> Option<String> {
    let named = |word: &str| {
        let name = word.rsplit('/').next().unwrap_or(word);
        bound.contains_key(name).then(|| name.to_owned())
    };
    match mode {
        marsh_contracts::cloud::CloudMode::Argv => named(&String::from_utf8_lossy(argv.first()?)),
        marsh_contracts::cloud::CloudMode::Shell => {
            let script = String::from_utf8_lossy(argv.get(1)?).into_owned();
            command_words(&script)
                .into_iter()
                .find_map(|word| named(&word))
        }
    }
}

/// Words in command position: the first word of each simple command
/// (after `;`, `|`, `&`, newlines, parentheses, braces, backquotes and
/// `$(`), past assignments, reserved words and command prefixes.
fn command_words(script: &str) -> Vec<String> {
    const SKIP: [&str; 16] = [
        "if", "then", "else", "elif", "do", "while", "until", "!", "time", "exec", "command",
        "nohup", "env", "sudo", "xargs", "timeout",
    ];
    let mut out = Vec::new();
    for segment in script.split(['\n', ';', '|', '&', '(', ')', '{', '}', '`']) {
        let mut words = segment.split_whitespace().map(|word| {
            word.trim_matches(|c| matches!(c, '\'' | '"' | '$'))
                .to_owned()
        });
        for word in words.by_ref() {
            let assignment = word
                .split_once('=')
                .is_some_and(|(name, _)| !name.is_empty() && !name.contains('/'));
            if word.is_empty()
                || assignment
                || word.starts_with('-')
                || SKIP.contains(&word.as_str())
            {
                continue;
            }
            // `timeout 5 cmd`: a bare number is an argument of a prefix.
            if word.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            out.push(word);
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_before_the_last_check_stops_the_import_and_one_after_it_does_not_block() {
        use marsh_daemon::{NewJob, PublicMount, SessionAuthority, process::ProcessLink};
        let home = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(
            1,
            SessionAuthority {
                username: "alice".into(),
                uid: 501,
                gid: 20,
                launch_directory: "/Users/example/project".into(),
                guest_home: "/Users/example".into(),
                home_backing: home.path().join("home"),
                ephemeral_home: false,
            },
        );
        let job = |store: &DaemonStore| {
            store
                .begin_cloud_job(
                    NewJob {
                        label: None,
                        session_id: session.clone(),
                        command: "cloud".into(),
                        kit_ref: "cloud-shell".into(),
                        workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                        mounts: vec![PublicMount {
                            target: "/Users/example/project/.marsh/split/abc123def456/lane".into(),
                            access: "read_write".into(),
                        }],
                    },
                    &ProcessLink::default(),
                    &[],
                )
                .unwrap()
                .0
        };
        let (first, second) = (job(&store), job(&store));
        let lock = Mutex::new(());
        let ran = std::cell::Cell::new(0);
        // Not cancelled: the import runs.
        assert!(
            import_unless_cancelled(&store, &first, &lock, || ran.set(ran.get() + 1)).is_some()
        );
        // A cancel that arrives while the import runs is not held up by it
        // (the lane waits for the run), and the import completes.
        let during = import_unless_cancelled(&store, &second, &lock, || {
            store.cancel_split("abc123def456");
            ran.set(ran.get() + 1);
        });
        assert!(during.is_some() && ran.get() == 2);
        // A cancel that came first: nothing is imported, for any run of it.
        assert!(import_unless_cancelled(&store, &first, &lock, || ran.set(99)).is_none());
        assert_eq!(ran.get(), 2);
    }

    #[test]
    fn command_positions_find_agent_calls_but_not_arguments() {
        let words = command_words(
            "cd src && FOO=1 claude -p 'x' | tee out; echo pi > /tmp/pi\n(time codex exec y)",
        );
        assert_eq!(words, ["cd", "claude", "tee", "echo", "codex"]);
        assert_eq!(command_words("x=$(pi --version)"), ["pi"]);
        assert_eq!(command_words("timeout 5 hermes"), ["hermes"]);
    }

    #[test]
    fn conflicting_local_edits_are_found_and_identical_results_are_not() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("same"), b"base").unwrap();
        fs::write(dir.path().join("edited"), b"mine").unwrap();
        fs::write(dir.path().join("done"), b"result").unwrap();
        let digest = |bytes: &[u8]| Sha256::digest(bytes).to_vec();
        let mode = |name: &str| {
            use std::os::unix::fs::PermissionsExt as _;
            fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        let base = BTreeMap::from([
            (b"same".to_vec(), Entry::File(mode("same"), digest(b"base"))),
            (
                b"edited".to_vec(),
                Entry::File(mode("edited"), digest(b"base")),
            ),
            (b"done".to_vec(), Entry::File(mode("done"), digest(b"base"))),
        ]);
        let result = vec![
            (b"same".to_vec(), Entry::File(mode("same"), digest(b"new"))),
            (
                b"edited".to_vec(),
                Entry::File(mode("edited"), digest(b"new")),
            ),
            (
                b"done".to_vec(),
                Entry::File(mode("done"), digest(b"result")),
            ),
            (b"created".to_vec(), Entry::File(0o644, digest(b"x"))),
            (b"gone".to_vec(), Entry::Absent),
        ];
        let root = Dir::open_root(dir.path()).unwrap();
        assert_eq!(conflicts(&root, &base, &result), ["edited"]);
        // A file created here where the run created one too.
        fs::write(dir.path().join("created"), b"also mine").unwrap();
        assert_eq!(conflicts(&root, &base, &result), ["edited", "created"]);
    }
}
