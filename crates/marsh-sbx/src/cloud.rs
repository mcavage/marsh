//! Owned cloud resources through the public stock CLI. Uses the current sign-in;
//! this is not an account manager or a translation of local Kit v3 descriptors.

use crate::{SbxError, VmPurpose, vm_ownership::random_vm_name};
use marsh_contracts::OciImage;
use marsh_runtime::{Attachment, CommandOutput, CommandRunner, Invocation};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;

const MAX_DOCUMENT: usize = 4 * 1024 * 1024;
const MAX_OWNED: usize = 8;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
const CREATE_TIMEOUT: Duration = Duration::from_mins(5);
const REMOVE_TIMEOUT: Duration = Duration::from_secs(90);
const OBSERVE_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("cloud boundary: {0}")]
    Invalid(&'static str),
    #[error("cloud sandbox is not in this daemon's ownership map: {0}")]
    NotOwned(String),
    #[error("cloud sandbox identity changed; foreign resource left untouched: {0}")]
    Foreign(String),
    #[error("cloud sandbox is not ready; retire it before reuse: {0}")]
    NotReady(String),
    #[error(
        "cloud create outcome uncertain for {name}; intent retained, not retried: {diagnostic}"
    )]
    CreateUncertain { name: String, diagnostic: String },
    #[error("cloud cleanup uncertain for {0}; ownership retained")]
    CleanupUncertain(String),
    #[error("cloud {operation} failed (exit {code:?}): {diagnostic}")]
    Command {
        operation: &'static str,
        code: Option<i32>,
        diagnostic: String,
    },
    #[error(transparent)]
    Stock(#[from] SbxError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// An opaque cloud ID, deliberately distinct from local stock UUIDs. Parsing
/// never grants authority: every effect also consults this adapter's map.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CloudSandboxId(String);

impl CloudSandboxId {
    /// # Errors
    /// Rejects local UUIDs, names and malformed/noncanonical cloud IDs.
    pub fn parse(value: String) -> Result<Self, CloudError> {
        let tail = value
            .strip_prefix("sbx_")
            .ok_or(CloudError::Invalid("invalid cloud ID"))?;
        if tail.is_empty()
            || tail.len() > 64
            || !tail
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            return Err(CloudError::Invalid("invalid cloud ID"));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CloudSandboxId {
    type Error = CloudError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}
impl From<CloudSandboxId> for String {
    fn from(value: CloudSandboxId) -> Self {
        value.0
    }
}

/// First shell/worker provisioning primitive. Native v2 Kit preparation is a
/// separate path; inline image mode creates no Kit bake/template cache entry.
#[derive(Clone, Debug)]
pub struct CloudImageSpec {
    pub image: OciImage,
    /// A digest-pinned v3 Kit (`sbx --cloud create KIT`) with marsh mixins
    /// instead of the inline image: the Kit's own image, entrypoint, network
    /// policy, and credentials. `image` is then the Kit reference itself.
    pub kit: Option<CloudKit>,
    pub cpus: u32,
    pub memory_mib: u32,
    pub ttl: Duration,
}

/// A cloud sandbox from a v3 Kit: mixin directories (`--kit`) and
/// sandbox environment (`-e NAME=VALUE`) the mixins read at startup.
#[derive(Clone, Debug)]
pub struct CloudKit {
    /// The Kit's declared name: what the inventory reports as the sandbox's
    /// agent (with mixins the image is a stock-built template, not the Kit).
    pub name: String,
    pub mixins: Vec<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl CloudImageSpec {
    fn create_args(&self, name: &str) -> Vec<OsString> {
        let source: Vec<OsString> = match &self.kit {
            Some(kit) => {
                let mut source: Vec<OsString> = vec![self.image.as_str().into()];
                for mixin in &kit.mixins {
                    source.push("--kit".into());
                    source.push(mixin.as_os_str().to_owned());
                }
                for (name, value) in &kit.env {
                    source.push("-e".into());
                    source.push(format!("{name}={value}").into());
                }
                source
            }
            None => vec!["--image-ref".into(), self.image.as_str().into()],
        };
        let mut args: Vec<OsString> = vec!["create".into(), "--name".into(), name.into()];
        args.extend(source);
        args.extend([
            "--platform".into(),
            "linux/arm64".into(),
            "--cpus".into(),
            self.cpus.to_string().into(),
            "--memory".into(),
            format!("{}m", self.memory_mib).into(),
            "--ttl".into(),
            format!("{}s", self.ttl.as_secs()).into(),
            "--on-timeout".into(),
            "delete".into(),
            "--no-progress".into(),
        ]);
        args
    }

    fn validate(&self) -> Result<(), CloudError> {
        if let Some(kit) = &self.kit
            && (!valid_name(&kit.name)
                || kit.mixins.iter().any(|mixin| !mixin.is_absolute())
                || kit.env.iter().any(|(name, value)| {
                    name.is_empty()
                        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                        || value.contains('\0')
                }))
        {
            return Err(CloudError::Invalid(
                "invalid cloud Kit mixin or environment",
            ));
        }
        if self.image.as_str().len() > 4096
            || !self.image.as_str().contains("@sha256:")
            || OciImage::parse(self.image.as_str()).is_err()
        {
            return Err(CloudError::Invalid(
                "cloud image must be a registry-qualified immutable reference",
            ));
        }
        if !matches!(
            (self.cpus, self.memory_mib),
            (1, 2048) | (2, 4096) | (4, 8192) | (8, 16384) | (16, 32768)
        ) {
            return Err(CloudError::Invalid("unsupported cloud CPU/memory shape"));
        }
        if self.ttl.subsec_nanos() != 0 || !(60..=86400).contains(&self.ttl.as_secs()) {
            return Err(CloudError::Invalid(
                "cloud TTL must be whole seconds between one minute and one day",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloudPhase {
    Pending,
    Ready,
    Quarantined,
    Removing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudRecord {
    pub name: String,
    pub id: Option<CloudSandboxId>,
    /// The digest-pinned image (for a Kit sandbox, the Kit reference). Older
    /// records of stock-agent sandboxes held the image observed
    /// when it became ready (empty before).
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub phase: CloudPhase,
    /// A matching ID/name was positively observed in a complete inventory.
    /// An unseen create can be hidden, so its absence cannot release ownership.
    #[serde(default)]
    pub observed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipFile {
    version: u32,
    entries: BTreeMap<String, CloudRecord>,
}

struct State {
    file: OwnershipFile,
    creating: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CloudInventoryVm {
    pub name: String,
    pub id: CloudSandboxId,
    pub status: String,
    pub image: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
}

/// What `sbx kit inspect` reports for a v3 Kit.
#[derive(Clone, Debug)]
pub struct KitManifest {
    pub name: String,
    pub entrypoint: Vec<String>,
}

enum Expected<'a> {
    Image(&'a str),
    Kit(&'a str),
}

/// A complete successful public CLI observation, never a default empty view on
/// errors. Unknown envelopes/fields/duplicate identities are rejected.
pub struct CloudInventory {
    by_name: BTreeMap<String, CloudInventoryVm>,
}

impl CloudInventory {
    /// # Errors
    /// Rejects incomplete/unknown schemas, excessive data and duplicate IDs or
    /// names. Failure must never be interpreted as an empty inventory.
    pub fn decode(bytes: &[u8]) -> Result<Self, CloudError> {
        if bytes.len() > MAX_DOCUMENT {
            return Err(CloudError::Invalid("cloud inventory exceeds byte bound"));
        }
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        let rows = value
            .as_array()
            .ok_or(CloudError::Invalid("unknown cloud inventory envelope"))?;
        if rows.len() > 256 {
            return Err(CloudError::Invalid("cloud inventory exceeds row bound"));
        }
        let mut by_name = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for row in rows {
            let object = row
                .as_object()
                .ok_or(CloudError::Invalid("invalid cloud inventory row"))?;
            if object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "name"
                        | "id"
                        | "status"
                        | "image"
                        | "agent"
                        | "cpus"
                        | "memory_mib"
                        | "template"
                        | "created_at"
                        | "expires_at"
                )
            }) {
                return Err(CloudError::Invalid("unknown cloud inventory row schema"));
            }
            let vm: CloudInventoryVm = serde_json::from_value(row.clone())?;
            if !valid_name(&vm.name)
                || vm.status.is_empty()
                || vm.status.len() > 64
                || vm.status.chars().any(char::is_control)
                || !ids.insert(vm.id.clone())
                || by_name.insert(vm.name.clone(), vm).is_some()
            {
                return Err(CloudError::Invalid("ambiguous cloud inventory"));
            }
        }
        Ok(Self { by_name })
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&CloudInventoryVm> {
        self.by_name.get(name)
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

fn owned_name(name: &str) -> bool {
    if !name.is_ascii() || name.len() < 10 {
        return false;
    }
    let (prefix, tail) = name.split_at(name.len() - 10);
    crate::valid_vm_prefix(prefix)
        && tail.starts_with("s-")
        && tail[2..]
            .bytes()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase())
}

/// Host-side cloud boundary. The control directory must be private and outside
/// guest grants. One adapter holds its ownership lock until drop. Restarted
/// resources are cleanup-only; there is no implicit workload/transport replay.
pub struct CloudSbx {
    executable: PathBuf,
    runner: Arc<dyn CommandRunner>,
    directory: PathBuf,
    directory_identity: (u64, u64),
    _lock: File,
    state: Mutex<State>,
    poisoned: AtomicBool,
}

impl CloudSbx {
    /// # Errors
    /// Rejects unsafe control storage, malformed ownership, competing owners
    /// and persistence failure. Recovered ready resources become cleanup-only.
    pub fn open(
        executable: PathBuf,
        runner: Arc<dyn CommandRunner>,
        directory: PathBuf,
    ) -> Result<Self, CloudError> {
        let md = fs::symlink_metadata(&directory)?;
        if !directory.is_absolute()
            || directory.canonicalize()? != directory
            || !md.is_dir()
            || md.uid() != rustix::process::getuid().as_raw()
            || md.mode() & 0o077 != 0
        {
            return Err(CloudError::Invalid(
                "cloud control directory is not canonical and owner-only",
            ));
        }
        if !executable.is_absolute() {
            return Err(CloudError::Invalid("stock executable must be absolute"));
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(directory.join("cloud-ownership.lock"))?;
        private_file(&lock)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| CloudError::Invalid("cloud ownership is already open"))?;
        let path = directory.join("cloud-ownership.json");
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
            .open(path)
        {
            Ok(mut file) => {
                private_file(&file)?;
                let mut bytes = Vec::new();
                Read::by_ref(&mut file)
                    .take((MAX_DOCUMENT + 1) as u64)
                    .read_to_end(&mut bytes)?;
                if bytes.len() > MAX_DOCUMENT {
                    return Err(CloudError::Invalid("cloud ownership exceeds byte bound"));
                }
                serde_json::from_slice::<OwnershipFile>(&bytes)?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => OwnershipFile {
                version: 1,
                entries: BTreeMap::new(),
            },
            Err(e) => return Err(e.into()),
        };
        if file.version != 1 || file.entries.len() > MAX_OWNED {
            return Err(CloudError::Invalid(
                "invalid cloud ownership version/capacity",
            ));
        }
        let mut ids = BTreeSet::new();
        for (name, entry) in &mut file.entries {
            if !owned_name(name)
                || entry.name != *name
                || (entry.agent.is_none()
                    && (!entry.image.contains("@sha256:")
                        || OciImage::parse(entry.image.clone()).is_err()))
                || entry.image.len() > 4096
                || entry.image.chars().any(char::is_control)
                || entry.id.as_ref().is_some_and(|id| !ids.insert(id.clone()))
                || (entry.id.is_none() && (entry.phase == CloudPhase::Ready || entry.observed))
                || (entry.phase == CloudPhase::Ready && !entry.observed)
            {
                return Err(CloudError::Invalid("invalid cloud ownership record"));
            }
            if entry.phase == CloudPhase::Ready {
                entry.phase = CloudPhase::Quarantined;
            }
        }
        let adapter = Self {
            executable,
            runner,
            directory,
            directory_identity: (md.dev(), md.ino()),
            _lock: lock,
            state: Mutex::new(State {
                file,
                creating: BTreeSet::new(),
            }),
            poisoned: AtomicBool::new(false),
        };
        adapter.update(|_| Ok(()))?;
        Ok(adapter)
    }

    fn check_directory(&self) -> Result<(), CloudError> {
        let md = fs::symlink_metadata(&self.directory)?;
        if self.poisoned.load(Ordering::Acquire)
            || !md.is_dir()
            || (md.dev(), md.ino()) != self.directory_identity
            || md.uid() != rustix::process::getuid().as_raw()
            || md.mode() & 0o077 != 0
        {
            return Err(CloudError::Invalid(
                "cloud ownership directory changed or persistence is uncertain",
            ));
        }
        Ok(())
    }

    fn update<T>(
        &self,
        change: impl FnOnce(&mut OwnershipFile) -> Result<T, CloudError>,
    ) -> Result<T, CloudError> {
        self.check_directory()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| CloudError::Invalid("cloud ownership lock poisoned"))?;
        self.check_directory()?;
        let mut next = state.file.clone();
        let result = change(&mut next)?;
        let write = (|| -> Result<(), CloudError> {
            let mut temp = tempfile::NamedTempFile::new_in(&self.directory)?;
            serde_json::to_writer(&mut temp, &next)?;
            temp.flush()?;
            temp.as_file().sync_all()?;
            self.check_directory()?;
            temp.persist(self.directory.join("cloud-ownership.json"))
                .map_err(|e| e.error)?;
            File::open(&self.directory)?.sync_all()?;
            self.check_directory()?;
            Ok(())
        })();
        if write.is_err() {
            self.poisoned.store(true, Ordering::Release);
        }
        write?;
        state.file = next;
        Ok(result)
    }

    fn invocation(&self, args: impl IntoIterator<Item = OsString>) -> Invocation {
        Invocation {
            program: self.executable.clone(),
            arguments: std::iter::once("--cloud".into()).chain(args).collect(),
            working_directory: None,
            environment: Vec::new(),
        }
    }

    fn control(
        &self,
        args: Vec<OsString>,
        timeout: Duration,
        limit: usize,
    ) -> Result<CommandOutput, CloudError> {
        self.check_directory()?;
        let invocation = self.invocation(args);
        Ok(crate::capped_command::run_stock_command_capped_with_start(
            || self.spawn_admitted(None, &invocation),
            timeout,
            limit,
        )?)
    }

    /// The name and entrypoint a v3 Kit declares (`sbx kit inspect --json`):
    /// the sandbox's agent name, and what a registered command runs there.
    ///
    /// # Errors
    /// Rejects an unreadable inspection or a Kit without a name or entrypoint.
    pub fn kit_manifest(&self, kit: &OciImage) -> Result<KitManifest, CloudError> {
        self.check_directory()?;
        let invocation = Invocation {
            program: self.executable.clone(),
            arguments: vec![
                "kit".into(),
                "inspect".into(),
                "--json".into(),
                kit.as_str().into(),
            ],
            working_directory: None,
            environment: Vec::new(),
        };
        let output = crate::capped_command::run_stock_command_capped_with_start(
            || self.spawn_admitted(None, &invocation),
            CREATE_TIMEOUT,
            MAX_DOCUMENT,
        )?;
        command_ok("kit inspect", &output)?;
        let document: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let entry = document["sandbox"]["entrypoint"]
            .as_array()
            .map(|words| {
                words
                    .iter()
                    .filter_map(|word| word.as_str().map(ToOwned::to_owned))
                    .collect::<Vec<_>>()
            })
            .filter(|words| !words.is_empty() && words.iter().all(|w| !w.contains('\0')))
            .ok_or(CloudError::Invalid(
                "the Kit declares no sandbox entrypoint",
            ))?;
        let name = document["name"]
            .as_str()
            .filter(|name| valid_name(name))
            .ok_or(CloudError::Invalid("the Kit declares no valid name"))?
            .to_owned();
        Ok(KitManifest {
            name,
            entrypoint: entry,
        })
    }

    /// # Errors
    /// Fails on unsafe storage, stock control failure or incomplete inventory.
    pub fn inventory(&self) -> Result<CloudInventory, CloudError> {
        let output = self.control(
            vec!["ls".into(), "--json".into()],
            CONTROL_TIMEOUT,
            MAX_DOCUMENT,
        )?;
        command_ok("inventory", &output)?;
        CloudInventory::decode(&output.stdout)
    }

    /// # Errors
    /// Fails if ownership storage changed or the in-process lock is poisoned.
    pub fn records(&self) -> Result<Vec<CloudRecord>, CloudError> {
        self.check_directory()?;
        Ok(self
            .state
            .lock()
            .map_err(|_| CloudError::Invalid("cloud ownership lock poisoned"))?
            .file
            .entries
            .values()
            .cloned()
            .collect())
    }

    /// Observe/adopt pending creates for cleanup only. An absent intent is never
    /// dropped: a timed-out create may still become visible later.
    ///
    /// # Errors
    /// Fails on invalid inventory, colliding identities or persistence failure.
    pub fn reconcile(&self) -> Result<Vec<CloudRecord>, CloudError> {
        let view = self.inventory()?;
        let creating = self
            .state
            .lock()
            .map_err(|_| CloudError::Invalid("cloud ownership lock poisoned"))?
            .creating
            .clone();
        self.update(|file| {
            let mut ids: BTreeSet<_> = file.entries.values().filter_map(|e| e.id.clone()).collect();
            for entry in file
                .entries
                .values_mut()
                .filter(|e| e.id.is_none() && !creating.contains(&e.name))
            {
                if let Some(vm) = view.get(&entry.name) {
                    if !ids.insert(vm.id.clone()) {
                        return Err(CloudError::Foreign(entry.name.clone()));
                    }
                    entry.id = Some(vm.id.clone());
                    entry.phase = CloudPhase::Quarantined;
                    entry.observed = true;
                }
            }
            Ok(())
        })?;
        self.records()
    }

    /// # Errors
    /// Rejects invalid specs/capacity before creation. Ambiguous creates retain
    /// their persisted intent and are never automatically repeated.
    pub fn create(&self, spec: &CloudImageSpec) -> Result<CloudRecord, CloudError> {
        spec.validate()?;
        let help = self.control(
            vec!["create".into(), "--help".into()],
            CONTROL_TIMEOUT,
            MAX_DOCUMENT,
        )?;
        command_ok("cloud capability probe", &help)?;
        let help = String::from_utf8_lossy(&help.stdout);
        let required: &[&str] = if spec.kit.is_some() {
            &["--platform", "--ttl", "--on-timeout"]
        } else {
            &["--image-ref", "--platform", "--ttl", "--on-timeout"]
        };
        if required.iter().any(|flag| !help.contains(flag)) {
            return Err(CloudError::Invalid(
                "stock SBX lacks required inline cloud image/TTL support",
            ));
        }
        let view = self.inventory()?;
        let name = loop {
            let name = random_vm_name(VmPurpose::Shell);
            if view.get(&name).is_none() && !self.records()?.iter().any(|r| r.name == name) {
                break name;
            }
        };
        self.update(|file| {
            if file.entries.len() >= MAX_OWNED {
                return Err(CloudError::Invalid(
                    "cloud sandbox capacity includes unresolved resources; run `marsh workers reset cloud`",
                ));
            }
            if file.entries.contains_key(&name) {
                return Err(CloudError::Invalid("cloud random name collision"));
            }
            file.entries.insert(
                name.clone(),
                CloudRecord {
                    name: name.clone(),
                    id: None,
                    image: spec.image.as_str().to_owned(),
                    agent: spec.kit.as_ref().map(|kit| kit.name.clone()),
                    phase: CloudPhase::Pending,
                    observed: false,
                },
            );
            Ok(())
        })?;
        self.state
            .lock()
            .map_err(|_| CloudError::Invalid("cloud ownership lock poisoned"))?
            .creating
            .insert(name.clone());
        let _flight = CreationFlight {
            adapter: self,
            name: name.clone(),
        };
        let output = self.control(spec.create_args(&name), CREATE_TIMEOUT, MAX_DOCUMENT);
        let output = output.map_err(|error| CloudError::CreateUncertain {
            name: name.clone(),
            diagnostic: diagnostic(&error.to_string()),
        })?;
        if !output.succeeded() {
            return Err(CloudError::CreateUncertain {
                name,
                diagnostic: diagnostic(&String::from_utf8_lossy(&output.stderr)),
            });
        }
        // Kit creates may print progress lines ahead of the ID.
        let id = String::from_utf8_lossy(&output.stdout)
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .and_then(|line| CloudSandboxId::parse(line.to_owned()).ok())
            .ok_or_else(|| CloudError::CreateUncertain {
                name: name.clone(),
                diagnostic: "stock create returned no valid cloud ID".into(),
            })?;
        self.update(|file| {
            if file
                .entries
                .values()
                .any(|e| e.name != name && e.id.as_ref() == Some(&id))
            {
                return Err(CloudError::Foreign(name.clone()));
            }
            let entry = file
                .entries
                .get_mut(&name)
                .ok_or(CloudError::Invalid("cloud create intent disappeared"))?;
            if entry.id.as_ref().is_some_and(|old| old != &id) {
                return Err(CloudError::Foreign(name.clone()));
            }
            entry.id = Some(id.clone());
            Ok(())
        })?;
        let expected = match &spec.kit {
            Some(kit) => Expected::Kit(&kit.name),
            None => Expected::Image(spec.image.as_str()),
        };
        self.wait_ready(&name, &id, &expected)
    }

    /// Wait until the owned sandbox runs what was asked for: the inline
    /// image, or for a Kit sandbox the Kit's agent (stock `sbx` built the
    /// template from the digest-pinned Kit we named; the record keeps that
    /// reference).
    fn wait_ready(
        &self,
        name: &str,
        id: &CloudSandboxId,
        expected: &Expected<'_>,
    ) -> Result<CloudRecord, CloudError> {
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        loop {
            if let Some(vm) = self.observe_owned(id)?
                && vm.status == "running"
                && match expected {
                    Expected::Image(image) => vm.image.as_deref() == Some(*image),
                    Expected::Kit(kit) => vm.agent.as_deref() == Some(*kit),
                }
            {
                return self.update(|file| {
                    let entry = file
                        .entries
                        .get_mut(name)
                        .ok_or(CloudError::Invalid("cloud create intent disappeared"))?;
                    if entry.phase != CloudPhase::Pending {
                        return Err(CloudError::NotReady(name.to_owned()));
                    }
                    entry.phase = CloudPhase::Ready;
                    Ok(entry.clone())
                });
            }
            if Instant::now() >= deadline {
                self.quarantine(id)?;
                return Err(CloudError::CreateUncertain {
                    name: name.to_owned(),
                    diagnostic: "created sandbox was not observed ready before deadline".into(),
                });
            }
            thread::sleep(OBSERVE_INTERVAL);
        }
    }

    fn record(&self, id: &CloudSandboxId) -> Result<CloudRecord, CloudError> {
        self.records()?
            .into_iter()
            .find(|r| r.id.as_ref() == Some(id))
            .ok_or_else(|| CloudError::NotOwned(id.as_str().to_owned()))
    }

    fn observe_owned(&self, id: &CloudSandboxId) -> Result<Option<CloudInventoryVm>, CloudError> {
        let record = self.record(id)?;
        let view = self.inventory()?;
        if let Some(vm) = view.get(&record.name)
            && vm.id != *id
        {
            return Err(CloudError::Foreign(record.name));
        }
        if let Some(vm) = view.by_name.values().find(|v| v.id == *id) {
            if vm.name != record.name {
                return Err(CloudError::Foreign(record.name));
            }
            if !record.observed {
                self.update(|file| {
                    file.entries
                        .get_mut(&record.name)
                        .ok_or(CloudError::Invalid("cloud record disappeared"))?
                        .observed = true;
                    Ok(())
                })?;
            }
            return Ok(Some(vm.clone()));
        }
        Ok(None)
    }

    fn ready(&self, id: &CloudSandboxId) -> Result<(), CloudError> {
        let record = self.record(id)?;
        if record.phase != CloudPhase::Ready {
            return Err(CloudError::NotReady(record.name));
        }
        match self.observe_owned(id) {
            Ok(Some(vm))
                if vm.status == "running"
                    && match &record.agent {
                        // A Kit sandbox runs a stock-built template: match its Kit.
                        Some(kit) => vm.agent.as_ref() == Some(kit),
                        None => vm.image.as_deref() == Some(record.image.as_str()),
                    } =>
            {
                Ok(())
            }
            Ok(_) => {
                self.quarantine(id)?;
                Err(CloudError::NotReady(record.name))
            }
            Err(error) => {
                self.quarantine(id)?;
                Err(error)
            }
        }
    }

    /// # Errors
    /// Rejects an unowned ID or failed persistence. Quarantine blocks new
    /// submissions; already submitted remote effects still require retirement.
    pub fn quarantine(&self, id: &CloudSandboxId) -> Result<(), CloudError> {
        let record = self.record(id)?;
        self.update(|file| {
            file.entries
                .get_mut(&record.name)
                .ok_or(CloudError::Invalid("cloud record disappeared"))?
                .phase = CloudPhase::Quarantined;
            Ok(())
        })
    }

    /// Trusted bootstrap/control argv is UTF-8. Workload argv bytes travel in
    /// the worker protocol, never through the cloud CLI's JSON command argv.
    /// The transport owner must quarantine the sandbox on ambiguous/lost work;
    /// a local carrier exit is not evidence that a remote command stopped.
    ///
    /// # Errors
    /// Rejects unowned/unready resources, changed identity/image, invalid argv,
    /// and local carrier-start failure.
    pub fn spawn(
        &self,
        id: &CloudSandboxId,
        program: &str,
        args: &[OsString],
    ) -> Result<Attachment, CloudError> {
        self.ready(id)?;
        if args.len() > 256
            || program.len() > 4096
            || args.iter().any(|arg| arg.len() > 65536)
            || args.iter().map(|arg| arg.len()).sum::<usize>() > 128 * 1024
        {
            return Err(CloudError::Invalid("cloud control argv exceeds its bound"));
        }
        let mut command = vec![
            "exec".into(),
            "-i".into(),
            id.as_str().into(),
            program.into(),
        ];
        for arg in args {
            if arg.to_str().is_none_or(|s| s.contains('\0')) {
                return Err(CloudError::Invalid(
                    "cloud control argv must be UTF-8 without NUL",
                ));
            }
            command.push(arg.clone());
        }
        if program.is_empty() || program.contains('\0') {
            return Err(CloudError::Invalid("invalid cloud control program"));
        }
        Ok(self.spawn_ready(id, &self.invocation(command))?)
    }

    fn spawn_ready(
        &self,
        id: &CloudSandboxId,
        invocation: &Invocation,
    ) -> std::io::Result<Attachment> {
        self.spawn_admitted(Some(id), invocation)
    }

    fn spawn_admitted(
        &self,
        ready: Option<&CloudSandboxId>,
        invocation: &Invocation,
    ) -> std::io::Result<Attachment> {
        let state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("cloud ownership lock poisoned"))?;
        // Persistence failures poison while holding this same lock. A check
        // before waiting on it could become stale and admit effects afterward.
        self.check_directory()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if ready.is_some_and(|id| {
            !state
                .file
                .entries
                .values()
                .any(|r| r.id.as_ref() == Some(id) && r.phase == CloudPhase::Ready)
        }) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cloud sandbox no longer ready",
            ));
        }
        self.runner.spawn_attached(invocation)
    }

    fn ready_control(
        &self,
        id: &CloudSandboxId,
        args: Vec<OsString>,
        timeout: Duration,
        limit: usize,
    ) -> Result<CommandOutput, CloudError> {
        let invocation = self.invocation(args);
        Ok(crate::capped_command::run_stock_command_capped_with_start(
            || self.spawn_ready(id, &invocation),
            timeout,
            limit,
        )?)
    }

    /// # Errors
    /// Rejects unsafe/oversized sources and unowned/unready destinations. A
    /// failed submitted upload quarantines the sandbox; bytes may be partial.
    pub fn upload_file(
        &self,
        id: &CloudSandboxId,
        source: &Path,
        remote: &str,
    ) -> Result<(), CloudError> {
        remote_path(remote)?;
        self.ready(id)?;
        let md = fs::symlink_metadata(source)?;
        if !source.is_absolute()
            || source.canonicalize()? != source
            || !md.is_file()
            || md.uid() != rustix::process::getuid().as_raw()
            || md.mode() & 0o022 != 0
            || md.len() > 64 * 1024 * 1024
        {
            return Err(CloudError::Invalid(
                "unsafe or oversized cloud upload source",
            ));
        }
        let output = self.ready_control(
            id,
            vec![
                "cp".into(),
                source.as_os_str().to_owned(),
                format!("{}:{remote}", id.as_str()).into(),
            ],
            CREATE_TIMEOUT,
            MAX_DOCUMENT,
        );
        match output {
            Ok(output) if output.succeeded() => Ok(()),
            Ok(output) => {
                self.quarantine(id)?;
                command_ok("upload", &output)
            }
            Err(error) => {
                self.quarantine(id)?;
                Err(error)
            }
        }
    }

    /// Bounded file bytes, not archive extraction into a host working tree.
    /// The caller must bind the remote path to its admitted workspace view.
    ///
    /// # Errors
    /// Rejects unowned/unready resources, unsafe paths, excess bytes and failed
    /// stock observations. Returned bytes are not an import authorization.
    pub fn read_file(
        &self,
        id: &CloudSandboxId,
        remote: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, CloudError> {
        remote_path(remote)?;
        self.ready(id)?;
        if max_bytes == 0 || max_bytes > MAX_DOCUMENT {
            return Err(CloudError::Invalid("invalid cloud file byte limit"));
        }
        let output = self.ready_control(
            id,
            vec![
                "exec".into(),
                "-i".into(),
                id.as_str().into(),
                "head".into(),
                "-c".into(),
                (max_bytes + 1).to_string().into(),
                "--".into(),
                remote.into(),
            ],
            CONTROL_TIMEOUT,
            max_bytes + 4096,
        )?;
        command_ok("read file", &output)?;
        if output.stdout.len() > max_bytes {
            return Err(CloudError::Invalid("cloud file exceeds byte limit"));
        }
        Ok(output.stdout)
    }

    /// Removal is ID-addressed; uncertain cleanup can be requested again while
    /// the ID remains owned. Only a fresh complete inventory
    /// proving absence releases the persisted record; an ack or timeout cannot.
    ///
    /// # Errors
    /// Refuses foreign identities, retains invisible/unresolved creates, and
    /// reports uncertainty if absence cannot be established before the deadline.
    pub fn retire(&self, id: &CloudSandboxId) -> Result<(), CloudError> {
        let record = self.record(id)?;
        self.observe_owned(id)?;
        self.update(|file| {
            file.entries
                .get_mut(&record.name)
                .ok_or(CloudError::Invalid("cloud record disappeared"))?
                .phase = CloudPhase::Removing;
            Ok(())
        })?;
        let deadline = Instant::now() + REMOVE_TIMEOUT;
        // Even an invisible newly-created ID gets a delete request. It cannot
        // be forgotten merely because propagation has not exposed it yet.
        let _ = self.control(
            vec!["rm".into(), "--force".into(), id.as_str().into()],
            Duration::from_secs(65),
            MAX_DOCUMENT,
        );
        loop {
            // The positive observation must precede this absence query. A
            // concurrent observer cannot retroactively validate an older miss.
            let observed_before_query = self.record(id)?.observed;
            match self.observe_owned(id) {
                Ok(None) if observed_before_query => {
                    return self.update(|file| {
                        file.entries.remove(&record.name);
                        Ok(())
                    });
                }
                Err(CloudError::Foreign(name)) => return Err(CloudError::Foreign(name)),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err(CloudError::CleanupUncertain(id.as_str().to_owned()));
            }
            thread::sleep(OBSERVE_INTERVAL);
        }
    }
}

struct CreationFlight<'a> {
    adapter: &'a CloudSbx,
    name: String,
}
impl Drop for CreationFlight<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.adapter.state.lock() {
            state.creating.remove(&self.name);
        }
    }
}

fn private_file(file: &File) -> Result<(), CloudError> {
    let md = file.metadata()?;
    if !md.is_file()
        || md.uid() != rustix::process::getuid().as_raw()
        || md.mode() & 0o077 != 0
        || md.nlink() != 1
    {
        return Err(CloudError::Invalid(
            "cloud control file is not private and singly linked",
        ));
    }
    Ok(())
}

fn remote_path(value: &str) -> Result<(), CloudError> {
    let path = Path::new(value);
    if value.contains('\0') || value.len() > 4096 || !crate::safe_absolute(path) {
        return Err(CloudError::Invalid("invalid remote cloud path"));
    }
    Ok(())
}

fn command_ok(operation: &'static str, output: &CommandOutput) -> Result<(), CloudError> {
    if output.succeeded() {
        return Ok(());
    }
    Err(CloudError::Command {
        operation,
        code: output.exit_code,
        diagnostic: diagnostic(&String::from_utf8_lossy(&output.stderr)),
    })
}

fn diagnostic(value: &str) -> String {
    value
        .chars()
        .take(2048)
        .flat_map(char::escape_default)
        .collect()
}
