//! The durable side of a freeze (Linux only): what a capture intends to pause,
//! what is confirmed thawed, and the watchdog that enforces both.
//!
//! A capture's staging directory `pool/snapshots/.tmp-<id>` holds:
//!
//! - `intent.json`: the id and the exact workspace;
//! - `journal.json`: every container the capture *may* have paused. It is
//!   written before the first pause call and grows (atomically) before any
//!   later pause, so a pause whose outcome is unknown (timeout, killed agent)
//!   is covered;
//! - `settled/<container id>`: written only after the engine has confirmed
//!   that container is not paused (or no longer exists);
//! - `alive`: a heartbeat the capture refreshes only on real progress;
//! - `revoked`: created by the watchdog or by recovery *before* they thaw
//!   anything. A capture that sees it has lost its freeze and must not publish.
//!
//! The journal is the intent to thaw. Nothing deletes it, or the staging
//! directory, until every id in it is settled; an unreadable journal counts as
//! unsettled. Capture, watchdog and startup recovery all use `thaw` and keep
//! retrying until the engine confirms.

use crate::breaks::break_mode;
use crate::engine_api::{Engine, valid_container_id};
use crate::fsops;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::{Duration, Instant};

pub const INTENT: &str = "intent.json";
pub const JOURNAL: &str = "journal.json";
pub const ALIVE: &str = "alive";
pub const REVOKED: &str = "revoked";
pub const SETTLED: &str = "settled";

#[derive(Serialize, Deserialize)]
pub struct Journal {
    pub id: String,
    pub containers: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Intent {
    pub id: String,
    pub workspace: String,
}

/// Rewrites the journal atomically.
///
/// # Errors
/// Fails if the file cannot be written.
pub fn write_journal(stage: &OwnedFd, id: &str, containers: &[String]) -> io::Result<()> {
    let bytes = serde_json::to_vec(&Journal {
        id: id.to_owned(),
        containers: containers.to_vec(),
    })
    .map_err(io::Error::other)?;
    fsops::replace_file_at(stage, JOURNAL, &bytes)
}

/// The journal, `None` if the capture never wrote one.
///
/// # Errors
/// An unreadable or corrupt journal is an error, never "nothing to do".
pub fn read_journal(stage: &OwnedFd) -> io::Result<Option<Journal>> {
    match fsops::read_file_at(stage, JOURNAL, 256 * 1024) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Journal ids not yet confirmed thawed.
///
/// # Errors
/// Fails if the journal is unreadable or corrupt.
pub fn unsettled(stage: &OwnedFd) -> io::Result<Vec<String>> {
    let Some(journal) = read_journal(stage)? else {
        return Ok(Vec::new());
    };
    let settled: HashSet<String> = match fsops::open_dir_at(stage, SETTLED) {
        Ok(dir) => fsops::list_names(&dir)?
            .into_iter()
            .filter_map(|name| String::from_utf8(name).ok())
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => HashSet::new(),
        Err(error) => return Err(error),
    };
    Ok(journal
        .containers
        .into_iter()
        .filter(|id| !settled.contains(id))
        .collect())
}

fn mark_settled(stage: &OwnedFd, id: &str) -> io::Result<()> {
    let dir = match fsops::open_dir_at(stage, SETTLED) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fsops::mkdir_new(stage, SETTLED) {
                Ok(dir) => dir,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    fsops::open_dir_at(stage, SETTLED)?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    match fsops::write_new_file_at(&dir, id, b"") {
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => Err(error),
        _ => Ok(()),
    }
}

#[must_use]
pub fn is_revoked(stage: &OwnedFd) -> bool {
    fsops::exists_at(stage, REVOKED)
}

/// Marks the freeze as lost. Idempotent.
pub fn revoke(stage: &OwnedFd) {
    let _ = fsops::write_new_file_at(stage, REVOKED, b"");
}

/// One container: true once the engine says it is not paused (or is gone).
fn thaw_one(engine: &Engine, id: &str) -> bool {
    match engine.is_paused(id) {
        Ok(false) => true,
        Err(error) if error.is_not_found() => true,
        Ok(true) => {
            let _ = engine.unpause(id);
            match engine.is_paused(id) {
                Ok(false) => true,
                Err(error) => error.is_not_found(),
                Ok(true) => false,
            }
        }
        Err(_) => false,
    }
}

/// Thaws every unsettled id, up to `attempts` passes (100 ms apart), writing a
/// settled marker per confirmed container. Returns the ids still unsettled:
/// engine errors, timeouts and "still paused" all leave an id unsettled.
///
/// # Errors
/// Fails if the journal cannot be read.
pub fn thaw(engine: &Engine, stage: &OwnedFd, attempts: u32) -> io::Result<Vec<String>> {
    let mut remaining = unsettled(stage)?;
    for attempt in 0..attempts.max(1) {
        if remaining.is_empty() {
            break;
        }
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut still = Vec::new();
        for chunk in remaining.chunks(32) {
            let results: Vec<(String, bool)> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|id| scope.spawn(move || (id.clone(), thaw_one(engine, id))))
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            for (id, settled) in results {
                if settled && valid_container_id(&id) && mark_settled(stage, &id).is_ok() {
                    continue;
                }
                still.push(id);
            }
        }
        remaining = still;
    }
    Ok(remaining)
}

/// `capture-watchdog --engine-socket S --dir D --bound-ms N`: the independent
/// timer, a separate process that outlives a killed or hung agent.
///
/// While the journal has unsettled ids it watches the heartbeat. When the
/// heartbeat is older than the bound it first creates `revoked` (so the capture
/// can no longer publish), then thaws, and keeps retrying every 100 ms until the
/// engine has confirmed every container. It exits when nothing is unsettled.
/// `D` is built by the agent from a validated id.
#[must_use]
pub fn watchdog_main(args: &[String]) -> i32 {
    let mut socket = None;
    let mut dir = None;
    let mut bound_ms = 3000_u64;
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match (flag.as_str(), iter.next()) {
            ("--engine-socket", Some(v)) => socket = Some(v.clone()),
            ("--dir", Some(v)) => dir = Some(std::path::PathBuf::from(v)),
            ("--bound-ms", Some(v)) => bound_ms = v.parse().unwrap_or(bound_ms),
            #[cfg(feature = "test-breaks")]
            ("--break-modes", Some(v)) => crate::breaks::install(
                &v.split(',')
                    .filter(|mode| !mode.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
            ),
            _ => return 2,
        }
    }
    let (Some(socket), Some(dir)) = (socket, dir) else {
        return 2;
    };
    let Ok(stage) = fsops::open_dir_by_path(&dir) else {
        return 0;
    };
    let engine = Engine::new(socket);
    let bound = Duration::from_millis(bound_ms);
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(100));
        match unsettled(&stage) {
            Ok(ids) if ids.is_empty() => return 0,
            Ok(_) => {}
            // An unreadable journal is never "done"; keep looking.
            Err(_) => continue,
        }
        let age = heartbeat_age(&dir);
        let overdue = age.map_or(started.elapsed() > bound, |age| age > bound);
        if !overdue && !is_revoked(&stage) {
            continue;
        }
        revoke(&stage);
        let _ = thaw(&engine, &stage, 1);
        if break_mode("watchdog-discard-journal") {
            // Deliberately wrong (a break mode): give up after one attempt.
            let _ = fsops::remove_file_at(&stage, JOURNAL);
            return 0;
        }
    }
}

fn heartbeat_age(dir: &Path) -> Option<Duration> {
    std::fs::metadata(dir.join(ALIVE))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
}
