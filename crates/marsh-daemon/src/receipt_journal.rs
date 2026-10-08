use crate::{DaemonError, JobReceipt, JobState};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

const JOURNAL_SCHEMA: &str = "marsh.receipt-journal/v1";
const STATE_DIRECTORY: &str = "state";
const JOURNAL_FILE: &str = "results.journal";
const OWNER_LOCK_FILE: &str = "owner.lock";
const MAX_RECORD_BYTES: usize = 128 * 1024;
pub(crate) const MAX_RETAINED_RECEIPTS: usize = 1_000;
const COMPACT_TO_RECEIPTS: usize = 900;
const MAX_JOURNAL_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct JournalRecord {
    schema: String,
    receipt: JobReceipt,
}

#[derive(Debug)]
pub(crate) struct ReceiptJournal {
    directory: PathBuf,
    path: PathBuf,
    file: File,
    _owner_lock: File,
    length: u64,
    healthy: bool,
}

impl ReceiptJournal {
    pub(crate) fn open(home: &Path) -> Result<(Self, BTreeMap<String, JobReceipt>), DaemonError> {
        let directory = home.join(STATE_DIRECTORY);
        ensure_private_directory(&directory)?;
        let owner_lock_path = directory.join(OWNER_LOCK_FILE);
        let owner_lock = open_private_file(&owner_lock_path)?;
        rustix::fs::flock(
            &owner_lock,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .map_err(|error| {
            DaemonError::InvalidState(format!(
                "receipt journal is already owned by another daemon or cannot be locked: {error}"
            ))
        })?;
        let path = directory.join(JOURNAL_FILE);
        let mut file = open_private_file(&path)?;
        let receipts = replay(&mut file)?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::End(0))?;
        Ok((
            Self {
                directory,
                path,
                file,
                _owner_lock: owner_lock,
                length,
                healthy: true,
            },
            receipts,
        ))
    }

    pub(crate) fn append(
        &mut self,
        receipt: &JobReceipt,
        durable: bool,
    ) -> Result<(), DaemonError> {
        if !self.healthy {
            return Err(DaemonError::InvalidState(
                "receipt journal is unavailable after an I/O failure".into(),
            ));
        }
        let payload = serde_json::to_vec(&JournalRecord {
            schema: JOURNAL_SCHEMA.into(),
            receipt: receipt.clone(),
        })?;
        if payload.len() > MAX_RECORD_BYTES {
            return Err(DaemonError::InvalidState(
                "receipt journal record exceeds its size limit".into(),
            ));
        }
        let length = u32::try_from(payload.len()).map_err(|_| {
            DaemonError::InvalidState("receipt journal record length overflow".into())
        })?;
        let mut frame = Vec::with_capacity(payload.len() + 36);
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&Sha256::digest(&payload));
        let start = self.length;
        let write = self.file.write_all(&frame).and_then(|()| self.file.flush());
        let write = if durable {
            write.and_then(|()| self.file.sync_data())
        } else {
            write
        };
        if let Err(error) = write {
            if self
                .file
                .set_len(start)
                .and_then(|()| self.file.sync_all())
                .is_err()
            {
                self.healthy = false;
            }
            return Err(error.into());
        }
        self.length = start.saturating_add(u64::try_from(frame.len()).unwrap_or(u64::MAX));
        Ok(())
    }

    pub(crate) fn compact(
        &mut self,
        receipts: &BTreeMap<String, JobReceipt>,
    ) -> Result<(), DaemonError> {
        if !self.healthy {
            return Err(DaemonError::InvalidState(
                "receipt journal is unavailable after an I/O failure".into(),
            ));
        }
        let temporary = self.directory.join("results.journal.tmp");
        if temporary.exists() {
            verify_private_regular_file(&temporary)?;
            fs::remove_file(&temporary)?;
        }
        let mut replacement = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nofollow_flag())
            .open(&temporary)?;
        let mut ordered: Vec<_> = receipts.values().collect();
        ordered.sort_by_key(|receipt| receipt.cursor);
        for receipt in ordered {
            write_record(&mut replacement, receipt)?;
        }
        replacement.sync_all()?;
        fs::rename(&temporary, &self.path)?;
        let reopened = OpenOptions::new()
            .read(true)
            .append(true)
            .mode(0o600)
            .custom_flags(nofollow_flag())
            .open(&self.path);
        let reopened = match reopened {
            Ok(file) => file,
            Err(error) => {
                self.healthy = false;
                return Err(error.into());
            }
        };
        self.file = reopened;
        self.length = match self.file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                self.healthy = false;
                return Err(error.into());
            }
        };
        if let Err(error) = File::open(&self.directory).and_then(|directory| directory.sync_all()) {
            self.healthy = false;
            return Err(error.into());
        }
        Ok(())
    }

    pub(crate) fn exceeds_size_limit(&self) -> bool {
        self.length > MAX_JOURNAL_BYTES
    }
}

pub(crate) fn trim_receipts(receipts: &mut BTreeMap<String, JobReceipt>) -> bool {
    let mut terminal: Vec<_> = receipts
        .values()
        .filter(|receipt| !matches!(receipt.state, JobState::Queued | JobState::Running))
        .map(|receipt| (receipt.cursor, receipt.job_id.clone()))
        .collect();
    terminal.sort_by_key(|(cursor, _)| *cursor);
    if terminal.len() <= MAX_RETAINED_RECEIPTS {
        return false;
    }
    let remove = terminal.len().saturating_sub(COMPACT_TO_RECEIPTS);
    for (_, job_id) in terminal.into_iter().take(remove) {
        receipts.remove(&job_id);
    }
    true
}

pub(crate) fn verify_control_home(
    selected_home: &Path,
    control_home: &Path,
) -> Result<(), DaemonError> {
    let selected = fs::canonicalize(selected_home)?;
    let control = fs::canonicalize(control_home)?;
    if control != control_home || selected.starts_with(&control) || control.starts_with(&selected) {
        return Err(DaemonError::UnsafeHome(control_home.into()));
    }
    let metadata = fs::symlink_metadata(control_home)?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(DaemonError::UnsafeHome(control_home.into()));
    }
    Ok(())
}

fn write_record(file: &mut File, receipt: &JobReceipt) -> Result<(), DaemonError> {
    let payload = serde_json::to_vec(&JournalRecord {
        schema: JOURNAL_SCHEMA.into(),
        receipt: receipt.clone(),
    })?;
    if payload.len() > MAX_RECORD_BYTES {
        return Err(DaemonError::InvalidState(
            "receipt journal record exceeds its size limit".into(),
        ));
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| DaemonError::InvalidState("receipt journal length overflow".into()))?;
    file.write_all(&length.to_be_bytes())?;
    file.write_all(&payload)?;
    file.write_all(&Sha256::digest(&payload))?;
    Ok(())
}

fn replay(file: &mut File) -> Result<BTreeMap<String, JobReceipt>, DaemonError> {
    let maximum = MAX_JOURNAL_BYTES.saturating_add(
        u64::try_from(MAX_RECORD_BYTES)
            .unwrap_or(u64::MAX)
            .saturating_add(36),
    );
    if file.metadata()?.len() > maximum {
        return Err(corrupt_journal("journal exceeds its size limit"));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut offset = 0_usize;
    let mut receipts = BTreeMap::new();
    let mut cursors = BTreeMap::<u64, String>::new();
    while offset < bytes.len() {
        let record_start = offset;
        if bytes.len() - offset < 4 {
            truncate_tail(file, record_start)?;
            break;
        }
        let length =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("four bytes")) as usize;
        offset += 4;
        if length == 0 || length > MAX_RECORD_BYTES {
            return Err(corrupt_journal("invalid record length"));
        }
        let Some(record_end) = offset
            .checked_add(length)
            .and_then(|end| end.checked_add(32))
        else {
            return Err(corrupt_journal("record length overflow"));
        };
        if record_end > bytes.len() {
            truncate_tail(file, record_start)?;
            break;
        }
        let payload = &bytes[offset..offset + length];
        let checksum = &bytes[offset + length..record_end];
        if Sha256::digest(payload).as_slice() != checksum {
            return Err(corrupt_journal("record checksum mismatch"));
        }
        let record: JournalRecord = serde_json::from_slice(payload)
            .map_err(|_| corrupt_journal("invalid record payload"))?;
        validate_record(&record, &receipts, &cursors)?;
        cursors.insert(record.receipt.cursor, record.receipt.job_id.clone());
        receipts.insert(record.receipt.job_id.clone(), record.receipt);
        offset = record_end;
    }
    Ok(receipts)
}

fn validate_record(
    record: &JournalRecord,
    receipts: &BTreeMap<String, JobReceipt>,
    cursors: &BTreeMap<u64, String>,
) -> Result<(), DaemonError> {
    let receipt = &record.receipt;
    if record.schema != JOURNAL_SCHEMA
        || receipt.schema != "marsh.job/v1"
        || receipt.cursor == 0
        || uuid::Uuid::parse_str(&receipt.job_id).is_err()
        || uuid::Uuid::parse_str(&receipt.attempt_id).is_err()
    {
        return Err(corrupt_journal("invalid receipt structure"));
    }
    validate_receipt_state(receipt)?;
    if let Some(existing) = receipts.get(&receipt.job_id)
        && (existing.cursor != receipt.cursor
            || existing.attempt_id != receipt.attempt_id
            || existing.session_id != receipt.session_id
            || existing.command != receipt.command
            || existing.kit_ref != receipt.kit_ref
            || existing.workload_image != receipt.workload_image
            || existing.mounts != receipt.mounts
            || !valid_transition(existing.state, receipt.state))
    {
        return Err(corrupt_journal("receipt identity changed"));
    }
    if let Some(existing) = receipts.get(&receipt.job_id) {
        if existing.state == JobState::Running
            && (existing.worker_id != receipt.worker_id
                || existing.vm_id != receipt.vm_id
                || existing.container_id != receipt.container_id)
        {
            return Err(corrupt_journal("running assignment changed"));
        }
        if matches!(
            existing.state,
            JobState::Finished | JobState::Failed | JobState::Cancelled | JobState::Unknown
        ) && existing != receipt
        {
            return Err(corrupt_journal("terminal receipt changed"));
        }
    }
    if !receipts.contains_key(&receipt.job_id)
        && cursors
            .keys()
            .next_back()
            .is_some_and(|last| receipt.cursor <= *last)
    {
        return Err(corrupt_journal("receipt cursor moved backwards"));
    }
    if let Some(job_id) = cursors.get(&receipt.cursor)
        && job_id != &receipt.job_id
    {
        return Err(corrupt_journal("duplicate receipt cursor"));
    }
    Ok(())
}

fn validate_receipt_state(receipt: &JobReceipt) -> Result<(), DaemonError> {
    let container_has_placement =
        receipt.container_id.is_none() || (receipt.worker_id.is_some() && receipt.vm_id.is_some());
    let uncertain_cleanup_is_not_success = receipt.cleanup != crate::CleanupState::Uncertain
        || receipt
            .exit
            .as_ref()
            .is_some_and(|exit| exit.code != Some(0));
    let valid = match receipt.state {
        JobState::Queued => {
            (receipt.worker_id.is_none() && receipt.vm_id.is_none()
                || receipt.worker_id.is_some() && receipt.vm_id.is_some())
                && receipt.container_id.is_none()
                && receipt.exit.is_none()
                && !receipt.output_complete
                && receipt.cleanup == crate::CleanupState::Pending
                && receipt.finished_unix_ms.is_none()
        }
        JobState::Running => {
            receipt.worker_id.is_some()
                && receipt.vm_id.is_some()
                && receipt.container_id.is_some()
                && receipt.exit.is_none()
                && !receipt.output_complete
                && receipt.cleanup == crate::CleanupState::Pending
                && receipt.finished_unix_ms.is_none()
        }
        JobState::Finished | JobState::Failed | JobState::Cancelled | JobState::Unknown => {
            receipt.exit.is_some()
                && receipt.cleanup != crate::CleanupState::Pending
                && receipt.finished_unix_ms.is_some()
                && container_has_placement
                && uncertain_cleanup_is_not_success
        }
    };
    if !valid {
        return Err(corrupt_journal("receipt state evidence is inconsistent"));
    }
    Ok(())
}

fn valid_transition(previous: JobState, next: JobState) -> bool {
    match previous {
        JobState::Queued => true,
        JobState::Running => next != JobState::Queued,
        JobState::Finished | JobState::Failed | JobState::Cancelled | JobState::Unknown => {
            previous == next
        }
    }
}

fn truncate_tail(file: &mut File, length: usize) -> Result<(), DaemonError> {
    file.set_len(u64::try_from(length).map_err(|_| corrupt_journal("offset overflow"))?)?;
    file.sync_all()?;
    Ok(())
}

fn corrupt_journal(message: &str) -> DaemonError {
    DaemonError::InvalidState(format!("receipt journal is corrupt: {message}"))
}

fn ensure_private_directory(path: &Path) -> Result<(), DaemonError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != rustix::process::getuid().as_raw()
                || metadata.mode() & 0o077 != 0
            {
                return Err(DaemonError::UnsafeHome(path.into()));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn open_private_file(path: &Path) -> Result<File, DaemonError> {
    let existed = path.exists();
    if existed {
        verify_private_regular_file(path)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nofollow_flag())
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(DaemonError::UnsafeHome(path.into()));
    }
    if !existed {
        let directory = path
            .parent()
            .ok_or_else(|| DaemonError::UnsafeHome(path.into()))?;
        file.sync_all()?;
        File::open(directory)?.sync_all()?;
    }
    Ok(file)
}

fn verify_private_regular_file(path: &Path) -> Result<(), DaemonError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(DaemonError::UnsafeHome(path.into()));
    }
    Ok(())
}

fn nofollow_flag() -> i32 {
    i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits()).expect("O_NOFOLLOW fits i32")
}

#[cfg(test)]
pub(crate) fn journal_path(home: &Path) -> PathBuf {
    home.join(STATE_DIRECTORY).join(JOURNAL_FILE)
}

#[cfg(test)]
pub(crate) fn record_count(home: &Path) -> usize {
    let bytes = fs::read(journal_path(home)).expect("journal is readable");
    let mut offset = 0;
    let mut count = 0;
    while offset + 4 <= bytes.len() {
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("length"));
        offset += 4 + usize::try_from(length).expect("record length") + 32;
        if offset <= bytes.len() {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
pub(crate) fn replace_uncompacted(home: &Path, receipts: &[JobReceipt]) {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(journal_path(home))
        .expect("journal opens for test replacement");
    for receipt in receipts {
        write_record(&mut file, receipt).expect("test receipt serializes");
    }
    file.sync_all().expect("test journal syncs");
}
