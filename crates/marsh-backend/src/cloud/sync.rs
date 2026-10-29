//! Sync while running (`marsh cloud sync`, `docs/cloud.md`): write back a
//! running attached block's changes so far, with the detached write-back's
//! base check per path. A path also changed here since it was uploaded or
//! last synced is skipped and reported; the rest is applied, and the
//! sandbox's baseline then takes those entries, so later syncs and the
//! final write-back carry only newer changes. The block keeps running.

use super::detach::{conflicting, parse_base, read_result};
use super::{Ctl, STEP_TIMEOUT, SharedBuffer, actionable, os_args, step, summary};
use marsh_contracts::cloud::{Kind, MAX_ARCHIVE, Reader, Writer};
use marsh_daemon::cloud_detached::SyncReport;
use marsh_daemon::cloud_fs::{self, ApplyReport};
use marsh_daemon::split_fs::Dir;
use marsh_daemon::{AttachmentFrame, ServerAttachment};
use marsh_runtime::Attachment;
use marsh_sbx::cloud::{CloudSandboxId, CloudSbx};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A running attached block that `marsh cloud sync` can reach.
pub(crate) struct SyncTarget {
    pub job_id: String,
    pub cursor: u64,
    pub adapter: Arc<CloudSbx>,
    pub id: CloudSandboxId,
    pub ctl: Ctl,
    pub root: PathBuf,
    pub root_text: String,
    pub identity: (u64, u64),
    pub attachment: ServerAttachment,
    pub results: PathBuf,
    pub import: Arc<Mutex<()>>,
    /// Held for a whole sync; `closed` once the block is being captured.
    pub state: Mutex<SyncState>,
}

#[derive(Default)]
pub(crate) struct SyncState {
    pub closed: bool,
    /// A sync ran: the final write-back is base-checked too, since paths
    /// skipped as conflicts still differ from the sandbox's baseline.
    pub synced: bool,
}

/// What one checked apply did.
pub(crate) struct Checked {
    pub report: Result<ApplyReport, cloud_fs::ApplyError>,
    pub conflicts: Vec<String>,
    /// The applied entries, in the baseline's form (files as SHA-256).
    pub rebase: Vec<u8>,
}

/// Run a control step in the attached sandbox, its stdout into `sink`.
fn control(
    target: &SyncTarget,
    args: &[&str],
    input: Option<Vec<u8>>,
    sink: Box<dyn io::Write + Send>,
) -> Result<Vec<u8>, String> {
    let (program, words) = target.ctl.step(args);
    let attachment: Attachment = target
        .adapter
        .spawn(&target.id, &program, &os_args(&words))
        .map_err(|error| actionable(&error))?;
    let input = input.map(|bytes| Box::new(io::Cursor::new(bytes)) as Box<dyn Read + Send>);
    let result = step(attachment, input, sink, MAX_ARCHIVE, STEP_TIMEOUT)?;
    if result.ok() {
        Ok(result.stderr)
    } else {
        Err(result.diagnostic())
    }
}

/// The sandbox's current baseline (`--internal-cloud baseline`).
pub(crate) fn fetch_base(target: &SyncTarget) -> Result<Vec<u8>, String> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    control(
        target,
        &["--internal-cloud", "baseline"],
        None,
        Box::new(SharedBuffer(Arc::clone(&buffer))),
    )
    .map_err(|message| format!("reading the sandbox's baseline: {message}"))?;
    Ok(std::mem::take(
        &mut *buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ))
}

struct Hashing<'a, R> {
    inner: R,
    hasher: &'a mut Sha256,
}

impl<R: Read> Read for Hashing<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..count]);
        Ok(count)
    }
}

/// Copy `payload` without the conflicting paths into `filtered`; the
/// copied entries also go into the baseline-form archive returned.
fn filter(payload: &Path, skip: &BTreeSet<Vec<u8>>, filtered: &Path) -> Result<Vec<u8>, String> {
    let error = |e: io::Error| e.to_string();
    let mut reader = Reader::new(
        BufReader::new(File::open(payload).map_err(error)?),
        MAX_ARCHIVE,
    )
    .map_err(error)?;
    let out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(filtered)
        .map_err(error)?;
    let mut writer = Writer::new(BufWriter::new(out), MAX_ARCHIVE).map_err(error)?;
    let mut rebase = Writer::new(Vec::new(), MAX_ARCHIVE).map_err(error)?;
    while let Some(header) = reader.next_header().map_err(error)? {
        if skip.contains(&header.path) {
            continue;
        }
        let path = &header.path;
        match header.kind {
            Kind::Delete => {
                writer.delete(path).map_err(error)?;
                rebase.delete(path).map_err(error)?;
            }
            Kind::Directory => {
                writer.directory(path, header.mode).map_err(error)?;
                rebase.directory(path, header.mode).map_err(error)?;
            }
            Kind::File => {
                let mut hasher = Sha256::new();
                writer
                    .file(
                        path,
                        header.mode,
                        header.len,
                        &mut Hashing {
                            inner: reader.data_reader(),
                            hasher: &mut hasher,
                        },
                    )
                    .map_err(error)?;
                let digest = hasher.finalize();
                rebase
                    .file(path, header.mode, 32, &mut &digest[..])
                    .map_err(error)?;
            }
            Kind::Symlink => {
                let target = reader.data().map_err(error)?;
                writer.symlink(path, &target).map_err(error)?;
                rebase.symlink(path, &target).map_err(error)?;
            }
            Kind::Request => return Err("a delta carries no request".into()),
        }
    }
    writer
        .finish()
        .and_then(|out| out.into_inner().map_err(io::Error::other))
        .and_then(|file| file.sync_all())
        .map_err(error)?;
    rebase.finish().map_err(error)
}

/// The base check against `base`, then apply what passes, under the root's
/// import lock. `Err` before anything was written.
pub(crate) fn checked_apply(
    root: &Path,
    identity: (u64, u64),
    import: &Mutex<()>,
    base: Vec<u8>,
    payload: &Path,
) -> Result<Checked, String> {
    let base = parse_base(base)?;
    let result = read_result(payload)?;
    let filtered = payload.with_extension("checked");
    let _held = import
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = Dir::open_root(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if dir.fstat().map(|stat| stat.identity).ok() != Some(identity) {
        return Err(format!(
            "{} was replaced since the block started",
            root.display()
        ));
    }
    let skip = conflicting(&dir, &base, &result)
        .into_iter()
        .collect::<BTreeSet<_>>();
    let rebase = filter(payload, &skip, &filtered);
    let rebase = match rebase {
        Ok(rebase) => rebase,
        Err(message) => {
            let _ = fs::remove_file(&filtered);
            return Err(message);
        }
    };
    let report = cloud_fs::apply_delta(root, identity, &filtered, MAX_ARCHIVE);
    let _ = fs::remove_file(&filtered);
    Ok(Checked {
        report,
        conflicts: skip
            .iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect(),
        rebase,
    })
}

/// `; N conflicts (kept local): a, b`, or nothing.
pub(crate) fn conflict_note(conflicts: &[String]) -> String {
    if conflicts.is_empty() {
        return String::new();
    }
    let shown = conflicts
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let more = conflicts.len().saturating_sub(5);
    format!(
        "; {} conflict{} (changed here too, kept local): {shown}{}",
        conflicts.len(),
        if conflicts.len() == 1 { "" } else { "s" },
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )
}

/// One sync of a running block. Errors are reported in the result.
#[allow(clippy::too_many_lines)] // One ordered fetch, check, apply, rebase sequence.
pub(crate) fn sync(target: &SyncTarget) -> SyncReport {
    let mut state = target
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut report = SyncReport {
        job_id: target.job_id.clone(),
        cursor: target.cursor,
        ..SyncReport::default()
    };
    let failed = |mut report: SyncReport, message: String| {
        report.message = format!("job {}: sync failed: {message}", target.cursor);
        report.error = Some(message);
        report
    };
    if state.closed {
        return failed(
            report,
            "the block is ending; its final write-back carries its changes".into(),
        );
    }
    if let Err(error) = fs::DirBuilder::new()
        .recursive(true)
        .create(&target.results)
    {
        return failed(report, error.to_string());
    }
    let payload = target.results.join(format!("{}.sync.delta", target.job_id));
    let mut ignored = 0;
    let outcome = (|| -> Result<Checked, String> {
        let base = fetch_base(target)?;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&payload)
            .map_err(|e| e.to_string())?;
        let stderr = control(
            target,
            &["--internal-cloud", "sync", &target.root_text],
            None,
            Box::new(BufWriter::new(file)),
        )
        .map_err(|message| format!("reading the sandbox's changes: {message}"))?;
        ignored = marsh_contracts::cloud::ignored_count(&stderr);
        // From the first sync on, the final write-back is base-checked.
        state.synced = true;
        checked_apply(
            &target.root,
            target.identity,
            &target.import,
            base,
            &payload,
        )
    })();
    let _ = fs::remove_file(&payload);
    let checked = match outcome {
        Ok(checked) => checked,
        Err(message) => return failed(report, message),
    };
    report.conflicts = checked.conflicts;
    report.ignored = ignored;
    let applied = match checked.report {
        Ok(mut applied) => {
            applied.ignored = ignored;
            applied
        }
        Err(error) => {
            // Not rebased: the next sync carries these paths again (those
            // already written here match and are not conflicts).
            report.created = error.applied.created;
            report.modified = error.applied.modified;
            report.deleted = error.applied.deleted;
            return failed(
                report,
                format!(
                    "write-back stopped: {}; {} already",
                    error.message,
                    summary(&error.applied)
                ),
            );
        }
    };
    refresh(target, &applied);
    report.created = applied.created;
    report.modified = applied.modified;
    report.deleted = applied.deleted;
    report.kept.clone_from(&applied.kept);
    let line = summary(&applied).replacen("wrote back", "synced", 1);
    if let Err(message) = control(
        target,
        &["--internal-cloud", "rebase"],
        Some(checked.rebase),
        Box::new(io::sink()),
    ) {
        // Written here, but a later write-back carries them again.
        return failed(
            report,
            format!("{line}, but the sandbox's baseline was not updated: {message}"),
        );
    }
    report.message = format!(
        "job {}: {line}{}",
        target.cursor,
        conflict_note(&report.conflicts)
    );
    report
}

/// The caller's VM may cache listings: refresh those the sync changed.
pub(crate) fn refresh(target: &SyncTarget, applied: &ApplyReport) {
    if applied.listings.is_empty() {
        return;
    }
    let _ = target.attachment.send(&AttachmentFrame::Refresh {
        directories: applied
            .listings
            .iter()
            .map(|relative| {
                target
                    .root
                    .join(std::ffi::OsStr::from_bytes(relative))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect(),
    });
}

#[cfg(test)]
mod tests {
    use super::super::detach::Entry;
    use super::*;
    use std::collections::BTreeMap;

    fn archive(path: &Path, build: impl FnOnce(&mut Writer<&mut Vec<u8>>)) {
        let mut bytes = Vec::new();
        let mut writer = Writer::new(&mut bytes, MAX_ARCHIVE).unwrap();
        build(&mut writer);
        writer.finish().unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn digest(bytes: &[u8]) -> Vec<u8> {
        Sha256::digest(bytes).to_vec()
    }

    /// A local edit since the base is skipped and kept; the rest is applied
    /// and becomes the next base.
    #[test]
    fn conflicting_paths_are_skipped_and_the_rest_rebased() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("p");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("edited"), b"local").unwrap();
        fs::write(root.join("plain"), b"base").unwrap();
        let mode = |name: &str| {
            std::os::unix::fs::PermissionsExt::mode(
                &fs::metadata(root.join(name)).unwrap().permissions(),
            ) & 0o777
        };
        let base_path = dir.path().join("base");
        archive(&base_path, |w| {
            w.file(b"edited", mode("edited"), 32, &mut &digest(b"base")[..])
                .unwrap();
            w.file(b"plain", mode("plain"), 32, &mut &digest(b"base")[..])
                .unwrap();
        });
        let payload = dir.path().join("delta");
        archive(&payload, |w| {
            w.file(b"edited", 0o644, 5, &mut &b"cloud"[..]).unwrap();
            w.file(b"plain", 0o644, 5, &mut &b"cloud"[..]).unwrap();
            w.file(b"new", 0o644, 3, &mut &b"new"[..]).unwrap();
        });
        let identity = Dir::open_root(&root).unwrap().fstat().unwrap().identity;
        let checked = checked_apply(
            &root,
            identity,
            &Mutex::new(()),
            fs::read(&base_path).unwrap(),
            &payload,
        )
        .unwrap();
        assert_eq!(checked.conflicts, ["edited"]);
        let report = checked.report.unwrap();
        assert_eq!((report.created, report.modified), (1, 1));
        assert_eq!(fs::read(root.join("edited")).unwrap(), b"local");
        assert_eq!(fs::read(root.join("plain")).unwrap(), b"cloud");
        let rebased = parse_base(checked.rebase).unwrap();
        assert_eq!(
            rebased,
            BTreeMap::from([
                (b"new".to_vec(), Entry::File(0o644, digest(b"new"))),
                (b"plain".to_vec(), Entry::File(0o644, digest(b"cloud"))),
            ])
        );
    }
}
