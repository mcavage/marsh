//! Private final-boundary bind sources. Drop never unlinks a possibly live
//! container's sources. Only the runtime's verified deletion path releases them.
use crate::{
    RuntimeError,
    byte_exec::{self, Payload},
};
use marsh_contracts::{ContainerId, JobSpec, OciImage, WORKER_CONTAINER_CAPACITY};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::AsFd,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

pub(crate) const TARGET_PREFIX: &str = "/.marsh-bytes-";
const MAX_HELPER_BYTES: u64 = 32 * 1024 * 1024;

fn unsafe_carrier() -> RuntimeError {
    RuntimeError::ByteBridge("unsafe native byte carrier")
}

pub(crate) fn required(spec: &JobSpec) -> bool {
    spec.argv
        .iter()
        .any(|word| std::str::from_utf8(word).is_err())
        || spec
            .exported_environment
            .values()
            .any(|value| std::str::from_utf8(value).is_err())
        || spec.working_directory.to_str().is_none()
}

/// Provenance for the effective command, not a caller-supplied replacement.
pub(crate) struct ImageCommand {
    pub id: String,
    pub platform: &'static str,
    pub argv: Vec<Vec<u8>>,
}

impl ImageCommand {
    pub fn inspect(bytes: &[u8], spec: &JobSpec) -> Result<Self, RuntimeError> {
        let invalid = || RuntimeError::ByteBridge("invalid immutable image command inspection");
        if bytes.len() > 1024 * 1024 {
            return Err(invalid());
        }
        let image: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        let id = image
            .get("Id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid)?;
        if !id.starts_with("sha256:") || OciImage::parse(id).is_err() {
            return Err(invalid());
        }
        if spec.image.as_str().starts_with("sha256:") {
            if spec.image.as_str() != id {
                return Err(invalid());
            }
        } else if !image
            .get("RepoDigests")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|digests| {
                digests.iter().any(|digest| {
                    digest
                        .as_str()
                        .and_then(|value| value.rsplit_once('@').map(|(_, digest)| digest))
                        == spec
                            .image
                            .as_str()
                            .rsplit_once('@')
                            .map(|(_, digest)| digest)
                })
            })
        {
            return Err(invalid());
        }
        let (arch, platform) = native_platform()?;
        if image.get("Os").and_then(serde_json::Value::as_str) != Some("linux")
            || image
                .get("Architecture")
                .and_then(serde_json::Value::as_str)
                != Some(arch)
            || image.get("Variant").is_some_and(|value| {
                value.as_str().is_none_or(|variant| {
                    !(variant.is_empty() || arch == "arm64" && variant == "v8")
                })
            })
        {
            return Err(RuntimeError::ByteBridge(
                "native byte image platform mismatch",
            ));
        }
        let config = image.get("Config").ok_or_else(invalid)?;
        let mut argv = image_words(config.get("Entrypoint"))?;
        // Docker treats the one-empty-word entrypoint as an explicit reset.
        if argv.len() == 1 && argv[0].is_empty() {
            argv.clear();
        }
        let tail = if spec.argv.is_empty() {
            image_words(config.get("Cmd"))?
        } else {
            spec.argv.clone()
        };
        argv.extend(tail);
        let payload = Payload {
            argv,
            environment: spec.exported_environment.clone(),
            working_directory: spec.working_directory.as_os_str().as_bytes().to_vec(),
        };
        payload.validate().map_err(|_| invalid())?;
        Ok(Self {
            id: id.to_owned(),
            platform,
            argv: payload.argv,
        })
    }
}

fn image_words(value: Option<&serde_json::Value>) -> Result<Vec<Vec<u8>>, RuntimeError> {
    let invalid = || RuntimeError::ByteBridge("invalid immutable image command inspection");
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(words)) if words.len() <= byte_exec::MAX_ARGUMENTS => words
            .iter()
            .map(|word| {
                let word = word.as_str().ok_or_else(invalid)?;
                if word.len() > byte_exec::MAX_WORD_BYTES || word.contains('\0') {
                    return Err(invalid());
                }
                Ok(word.as_bytes().to_vec())
            })
            .collect(),
        _ => Err(invalid()),
    }
}

fn native_platform() -> Result<(&'static str, &'static str), RuntimeError> {
    match std::env::consts::ARCH {
        "aarch64" => Ok(("arm64", "linux/arm64")),
        "x86_64" => Ok(("amd64", "linux/amd64")),
        _ => Err(RuntimeError::ByteBridge(
            "unsupported native byte helper architecture",
        )),
    }
}

struct Carrier {
    directory: PathBuf,
    // Retained descriptors bind the exact copied files, not a mutable install
    // pathname. The directory remains even if this worker/runtime is dropped.
    helper_fd: File,
    payload_fd: File,
    // Held across create and the entire runtime lifetime, including uncertain
    // outcomes. Another worker must acquire this before observing/reclaiming.
    directory_fd: File,
    container: Option<ContainerId>,
}

pub(crate) struct CarrierStore {
    root: PathBuf,
    helper: PathBuf,
    entries: Mutex<BTreeMap<String, Carrier>>,
    root_identity: Mutex<Option<(u64, u64)>>,
}

impl CarrierStore {
    pub fn new(root: PathBuf, helper: PathBuf) -> Self {
        Self {
            root,
            helper,
            entries: Mutex::new(BTreeMap::new()),
            root_identity: Mutex::new(None),
        }
    }

    pub fn system() -> Self {
        let helper = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(|path| path.join("marsh-byte-exec")))
            .unwrap_or_else(|| PathBuf::from("/usr/local/libexec/marsh-byte-exec"));
        // HOME is the trusted worker's home, never the workload's HOME. No
        // fallback to shared /tmp if it is missing/unsafe.
        let home = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
        Self::new(home.join(".marsh-byte-carriers"), helper)
    }

    /// Make sources and register ownership BEFORE the opaque create call.
    pub fn prepare(
        &self,
        spec: &JobSpec,
        image: &ImageCommand,
    ) -> Result<(String, Vec<OsString>, String), RuntimeError> {
        let bytes = Payload {
            argv: image.argv.clone(),
            environment: spec.exported_environment.clone(),
            working_directory: spec.working_directory.as_os_str().as_bytes().to_vec(),
        }
        .encode()?;
        let helper = read_helper(&self.helper)?;
        let root = self.lock_root()?;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Also count sources retained by an earlier/crashed worker. Never adopt
        // or garbage-collect them merely because the in-memory map is empty.
        let capacity = usize::from(WORKER_CONTAINER_CAPACITY);
        let count = directory_names(&root, capacity)?.len();
        if count >= capacity {
            return Err(RuntimeError::ByteBridge(
                "native byte carrier capacity retained; verify worker cleanup",
            ));
        }
        let mut random = [0_u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let nonce = byte_exec::hex_encode(&random);
        let name = format!("marsh-bytes-{nonce}");
        // Fresh root-level files cannot shadow an image's fixed /run layout.
        // The final decoder also rejects its own inode through PATH/symlinks.
        let target_helper = format!("{TARGET_PREFIX}{nonce}-exec");
        let target_payload = format!("{TARGET_PREFIX}{nonce}-payload");
        let directory = self.root.join(&name);
        rustix::fs::mkdirat(&root, name.as_str(), rustix::fs::Mode::from_raw_mode(0o700))
            .map_err(std::io::Error::from)?;
        let directory_fd = open_directory(&root, Path::new(&name))?;
        lock(&directory_fd)?;
        // Only nonsecret attempt identity is synchronized. Payload/helper data
        // is not a secret-durability promise and must not be fsynced.
        write_readonly(&directory_fd, "attempt", name.as_bytes(), 0o400, true)?;
        directory_fd.sync_all()?;
        root.sync_all()?;
        let staged = (|| {
            let helper_file = write_readonly(&directory_fd, "helper", &helper, 0o555, false)?;
            let payload_file = write_readonly(&directory_fd, "payload", &bytes, 0o444, false)?;
            // Only public image identity/platform, never raw data or its hash.
            write_readonly(
                &directory_fd,
                "image",
                format!(
                    "{}\n{}\n{}\n",
                    spec.image.as_str(),
                    image.id,
                    image.platform
                )
                .as_bytes(),
                0o400,
                false,
            )?;
            let mut arguments = vec![
                "--name".into(),
                name.clone().into(),
                "--platform".into(),
                image.platform.into(),
                "--entrypoint".into(),
                target_helper.clone().into(),
            ];
            for (source, target) in [
                (directory.join("helper"), &target_helper),
                (directory.join("payload"), &target_payload),
            ] {
                arguments.extend([
                    "--mount".into(),
                    format!(
                        "type=bind,{},target={target},readonly",
                        crate::docker_mount_field("source", &source)?
                    )
                    .into(),
                ]);
            }
            verify_path_identity(&self.root, &root)?;
            verify_at_identity(&root, &name, &directory_fd)?;
            // Dispatch intent is part of this same pre-effect transaction:
            // marker I/O failure still rolls back instead of pinning an entry
            // that the caller will report as NotRequired.
            write_readonly(&directory_fd, "dispatched", name.as_bytes(), 0o400, true)?;
            directory_fd.sync_all()?;
            Ok((
                Carrier {
                    directory: directory.clone(),
                    helper_fd: helper_file,
                    payload_fd: payload_file,
                    directory_fd: directory_fd.try_clone()?,
                    container: None,
                },
                arguments,
            ))
        })();
        match staged {
            Ok((carrier, arguments)) => {
                entries.insert(name.clone(), carrier);
                Ok((name, arguments, target_payload))
            }
            Err(error) => {
                // No Docker invocation has occurred: these files have no live
                // references. Failure to remove is still reported, not hidden.
                remove_files(&root, &name, &directory_fd)?;
                Err(error)
            }
        }
    }

    pub fn bind(&self, attempt: &str, container: &ContainerId) -> Result<(), RuntimeError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let carrier = entries.get_mut(attempt).ok_or_else(unsafe_carrier)?;
        write_readonly(
            &carrier.directory_fd,
            "container",
            container.as_str().as_bytes(),
            0o400,
            true,
        )?;
        carrier.directory_fd.sync_all()?;
        carrier.container = Some(container.clone());
        Ok(())
    }

    /// Exact ID deletion alone is insufficient: other containers may still
    /// reference the source. The callback observes fresh actual Docker state.
    pub fn release_verified(
        &self,
        container: &ContainerId,
        observe: impl FnOnce(&[Candidate]) -> Result<Observation, RuntimeError>,
    ) -> Result<(), RuntimeError> {
        if !self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|entry| entry.container.as_ref() == Some(container))
        {
            return Ok(());
        }
        let root = self.lock_root()?;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let name = entries
            .iter()
            .find(|(_, entry)| entry.container.as_ref() == Some(container))
            .map(|(name, _)| name.clone());
        if let Some(name) = name {
            let entry = &entries[&name];
            let candidate = Candidate {
                name: name.clone(),
                directory: entry.directory.clone(),
                container: Some(container.clone()),
                dispatched: true,
            };
            if observe(&[candidate])?.unreferenced != [name.clone()] {
                return Err(RuntimeError::DeletionUncertain);
            }
            verify_path_identity(&self.root, &root)?;
            for (leaf, mode, retained) in [
                ("helper", 0o555, &entry.helper_fd),
                ("payload", 0o444, &entry.payload_fd),
            ] {
                let current =
                    open_owned_file(&entry.directory_fd, leaf, mode)?.ok_or_else(unsafe_carrier)?;
                if !same_identity(&retained.metadata()?, &current.metadata()?) {
                    return Err(unsafe_carrier());
                }
            }
            remove_files(&root, &name, &entry.directory_fd)?;
            entries.remove(&name);
        }
        Ok(())
    }

    fn lock_root(&self) -> Result<File, RuntimeError> {
        let root = private_root(&self.root)?;
        lock(&root)?;
        verify_path_identity(&self.root, &root)?;
        let metadata = root.metadata()?;
        let identity = (metadata.dev(), metadata.ino());
        let mut retained = self
            .root_identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if retained.is_some_and(|old| old != identity) {
            return Err(unsafe_carrier());
        }
        *retained = Some(identity);
        Ok(root)
    }

    /// Bounded restart recovery of our exact on-disk attempts. Active locks,
    /// malformed ownership or incomplete runtime observations never authorize
    /// unlink. This never deletes/adopts a Docker container or replays a job.
    pub fn reclaim(
        &self,
        observe: impl FnOnce(&[Candidate]) -> Result<Observation, RuntimeError>,
    ) -> Result<usize, RuntimeError> {
        let root = self.lock_root()?;
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut candidates = Vec::new();
        let mut pinned = BTreeMap::new();
        for name in directory_names(&root, usize::from(WORKER_CONTAINER_CAPACITY))? {
            if entries.contains_key(&name) {
                continue;
            }
            if !valid_attempt(&name) {
                return Err(unsafe_carrier());
            }
            let directory = open_directory(&root, Path::new(&name))?;
            match rustix::fs::flock(
                &directory,
                rustix::fs::FlockOperation::NonBlockingLockExclusive,
            ) {
                Ok(()) => {}
                Err(rustix::io::Errno::WOULDBLOCK) => continue,
                Err(_) => return Err(unsafe_carrier()),
            }
            let attempt = read_record(&directory, "attempt")?.ok_or_else(unsafe_carrier)?;
            if attempt != name {
                return Err(unsafe_carrier());
            }
            let container = read_record(&directory, "container")?
                .map(|value| ContainerId::parse(value).map_err(|_| unsafe_carrier()))
                .transpose()?;
            let dispatch = read_record(&directory, "dispatched")?;
            if dispatch.as_ref().is_some_and(|value| value != &name) {
                return Err(unsafe_carrier());
            }
            candidates.push(Candidate {
                name: name.clone(),
                directory: self.root.join(&name),
                container,
                dispatched: dispatch.is_some(),
            });
            pinned.insert(name, directory);
        }
        if candidates.is_empty() {
            return Ok(0);
        }
        let observation = observe(&candidates)?;
        verify_path_identity(&self.root, &root)?;
        // A pending opaque create is settled only by a real exact-container
        // observation, not an empty inventory. Persist that identity while it
        // still references our files; a LATER observation may prove absence.
        for (name, container) in observation.bound {
            let directory = pinned.get(&name).ok_or_else(unsafe_carrier)?;
            write_readonly(
                directory,
                "container",
                container.as_str().as_bytes(),
                0o400,
                true,
            )?;
            directory.sync_all()?;
        }
        let mut count = 0;
        for name in observation.unreferenced {
            let directory = pinned.remove(&name).ok_or_else(unsafe_carrier)?;
            remove_files(&root, &name, &directory)?;
            count += 1;
        }
        Ok(count)
    }
}

pub(crate) struct Candidate {
    pub name: String,
    pub directory: PathBuf,
    pub container: Option<ContainerId>,
    pub dispatched: bool,
}

pub(crate) struct Observation {
    pub unreferenced: Vec<String>,
    pub bound: BTreeMap<String, ContainerId>,
}

impl Observation {
    pub fn from_inspection(
        candidates: &[Candidate],
        expected: &[String],
        bytes: &[u8],
    ) -> Result<Self, RuntimeError> {
        let text = std::str::from_utf8(bytes).map_err(|_| RuntimeError::DeletionUncertain)?;
        let mut referenced = std::collections::BTreeSet::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut bound = BTreeMap::new();
        for line in text.lines() {
            let value: serde_json::Value =
                serde_json::from_str(line).map_err(|_| RuntimeError::DeletionUncertain)?;
            let id = value["Id"]
                .as_str()
                .ok_or(RuntimeError::DeletionUncertain)?;
            let name = value["Name"]
                .as_str()
                .filter(|name| name.starts_with('/'))
                .ok_or(RuntimeError::DeletionUncertain)?;
            if expected.binary_search(&id.to_owned()).is_err() || !seen.insert(id.to_owned()) {
                return Err(RuntimeError::DeletionUncertain);
            }
            let mounts = value["Mounts"]
                .as_array()
                .ok_or(RuntimeError::DeletionUncertain)?;
            let sources = mount_sources(mounts)?;
            for candidate in candidates {
                if candidate.container.is_none()
                    && candidate.dispatched
                    && name.strip_prefix('/') == Some(candidate.name.as_str())
                    && ["helper", "payload"].iter().all(|leaf| {
                        mounts.iter().any(|mount| {
                            mount["Type"] == "bind"
                                && mount["Source"].as_str().map(Path::new)
                                    == Some(candidate.directory.join(leaf).as_path())
                        })
                    })
                {
                    let id = ContainerId::parse(id.to_owned())
                        .map_err(|_| RuntimeError::DeletionUncertain)?;
                    if bound.insert(candidate.name.clone(), id).is_some() {
                        return Err(RuntimeError::DeletionUncertain);
                    }
                }
                if name.strip_prefix('/') == Some(candidate.name.as_str())
                    || candidate
                        .container
                        .as_ref()
                        .is_some_and(|container| container.as_str() == id)
                    || sources.iter().any(|source| {
                        source.starts_with(&candidate.directory)
                            || candidate.directory.starts_with(source)
                    })
                {
                    referenced.insert(candidate.name.clone());
                }
            }
        }
        if seen.into_iter().collect::<Vec<_>>() != expected {
            return Err(RuntimeError::DeletionUncertain);
        }
        Ok(Self {
            unreferenced: candidates
                .iter()
                .filter(|candidate| {
                    (!candidate.dispatched || candidate.container.is_some())
                        && !referenced.contains(&candidate.name)
                })
                .map(|candidate| candidate.name.clone())
                .collect(),
            bound,
        })
    }
}

fn mount_sources(mounts: &[serde_json::Value]) -> Result<Vec<&Path>, RuntimeError> {
    let mut sources = Vec::new();
    for mount in mounts {
        match mount["Type"].as_str() {
            Some("bind" | "volume") => {
                let source = Path::new(
                    mount["Source"]
                        .as_str()
                        .ok_or(RuntimeError::DeletionUncertain)?,
                );
                if !source.is_absolute()
                    || source
                        .components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                {
                    return Err(RuntimeError::DeletionUncertain);
                }
                sources.push(source);
            }
            Some("tmpfs")
                if mount
                    .get("Source")
                    .is_none_or(|source| source.is_null() || source.as_str() == Some("")) => {}
            // A new/opaque kind is not proof that no file source exists.
            _ => return Err(RuntimeError::DeletionUncertain),
        }
    }
    Ok(sources)
}

fn private_root(path: &Path) -> Result<File, RuntimeError> {
    if !path.is_absolute() {
        return Err(unsafe_carrier());
    }
    let parent = path.parent().ok_or_else(unsafe_carrier)?;
    let parent_file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(parent)?;
    let parent_metadata = parent_file.metadata()?;
    if parent_metadata.uid() != rustix::process::geteuid().as_raw()
        || parent_metadata.mode() & 0o7022 != 0
    {
        return Err(unsafe_carrier());
    }
    let leaf = path.file_name().ok_or_else(unsafe_carrier)?;
    match rustix::fs::mkdirat(&parent_file, leaf, rustix::fs::Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(std::io::Error::from(error).into()),
    }
    let root = open_directory(&parent_file, Path::new(leaf))?;
    if !same_identity(&parent_metadata, &fs::symlink_metadata(parent)?) {
        return Err(unsafe_carrier());
    }
    let metadata = root.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o700
    {
        return Err(unsafe_carrier());
    }
    Ok(root)
}

fn read_helper(path: &Path) -> Result<Vec<u8>, RuntimeError> {
    let mut helper = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
        .open(path)?;
    let metadata = helper.metadata()?;
    if !metadata.is_file()
        || ![0, rustix::process::geteuid().as_raw()].contains(&metadata.uid())
        || metadata.mode() & 0o7022 != 0
        || metadata.nlink() != 1
        || metadata.mode() & 0o111 == 0
        || metadata.len() > MAX_HELPER_BYTES
    {
        return Err(unsafe_carrier());
    }
    let mut bytes = Vec::new();
    (&mut helper)
        .take(MAX_HELPER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(unsafe_carrier());
    }
    validate_static_elf(&bytes)?;
    Ok(bytes)
}

fn validate_static_elf(bytes: &[u8]) -> Result<(), RuntimeError> {
    if bytes.len() < 64 || &bytes[..7] != b"\x7fELF\x02\x01\x01" {
        return Err(unsafe_carrier());
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    if !matches!(
        (std::env::consts::ARCH, machine),
        ("aarch64", 183) | ("x86_64", 62)
    ) {
        return Err(RuntimeError::ByteBridge(
            "native byte helper architecture mismatch",
        ));
    }
    let offset = usize::try_from(u64::from_le_bytes(
        bytes[32..40].try_into().map_err(|_| unsafe_carrier())?,
    ))
    .map_err(|_| unsafe_carrier())?;
    let size = usize::from(u16::from_le_bytes([bytes[54], bytes[55]]));
    let count = usize::from(u16::from_le_bytes([bytes[56], bytes[57]]));
    if size != 56 || count == 0 || count > 128 {
        return Err(unsafe_carrier());
    }
    let end = offset
        .checked_add(size * count)
        .ok_or_else(unsafe_carrier)?;
    let headers = bytes.get(offset..end).ok_or_else(unsafe_carrier)?;
    for header in headers.chunks_exact(size) {
        if header[..4] == 3_u32.to_le_bytes() {
            return Err(RuntimeError::ByteBridge(
                "native byte helper must be statically linked",
            ));
        }
        if header[..4] == 2_u32.to_le_bytes() {
            let offset = elf_size(&header[8..16])?;
            let length = elf_size(&header[32..40])?;
            if length > 64 * 1024 || length % 16 != 0 {
                return Err(unsafe_carrier());
            }
            let end = offset.checked_add(length).ok_or_else(unsafe_carrier)?;
            let dynamic = bytes.get(offset..end).ok_or_else(unsafe_carrier)?;
            if dynamic
                .as_chunks::<16>()
                .0
                .iter()
                .any(|entry| entry[..8] == 1_u64.to_le_bytes())
            {
                return Err(RuntimeError::ByteBridge(
                    "native byte helper must not need shared libraries",
                ));
            }
        }
    }
    Ok(())
}

fn elf_size(bytes: &[u8]) -> Result<usize, RuntimeError> {
    usize::try_from(u64::from_le_bytes(
        bytes.try_into().map_err(|_| unsafe_carrier())?,
    ))
    .map_err(|_| unsafe_carrier())
}

fn write_readonly(
    directory: &File,
    name: &str,
    bytes: &[u8],
    mode: u32,
    durable: bool,
) -> Result<File, RuntimeError> {
    use rustix::fs::{Mode, OFlags, openat};
    let mut file: File = openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(std::io::Error::from)?
    .into();
    file.write_all(bytes)?;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    if durable {
        file.sync_all()?;
    }
    let identity = file.metadata()?;
    drop(file); // executable must not retain a writable open (ETXTBSY)
    let file = open_owned_file(directory, name, mode)?.ok_or_else(unsafe_carrier)?;
    if !same_identity(&identity, &file.metadata()?) {
        return Err(unsafe_carrier());
    }
    Ok(file)
}

fn lock(file: &File) -> Result<(), RuntimeError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                return Err(RuntimeError::ByteBridge(
                    "native byte carrier admission busy",
                ));
            }
        }
    }
}

fn open_directory(parent: impl AsFd, path: &Path) -> Result<File, RuntimeError> {
    use rustix::fs::{Mode, OFlags, openat};
    let file: File = openat(
        parent,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?
    .into();
    let metadata = file.metadata()?;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o7777 != 0o700 {
        return Err(unsafe_carrier());
    }
    Ok(file)
}

fn directory_names(directory: &File, limit: usize) -> Result<Vec<String>, RuntimeError> {
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(directory).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name().to_str().map_err(|_| unsafe_carrier())?;
        if name == "." || name == ".." {
            continue;
        }
        if names.len() == limit {
            return Err(RuntimeError::ByteBridge(
                "native byte carrier capacity retained; verify worker cleanup",
            ));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

fn valid_attempt(name: &str) -> bool {
    name.strip_prefix("marsh-bytes-").is_some_and(|nonce| {
        nonce.len() == 32
            && nonce
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn same_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    (left.dev(), left.ino()) == (right.dev(), right.ino())
}

fn verify_path_identity(path: &Path, file: &File) -> Result<(), RuntimeError> {
    let observed = fs::symlink_metadata(path)?;
    if !observed.is_dir()
        || !same_identity(&observed, &file.metadata()?)
        || observed.uid() != rustix::process::geteuid().as_raw()
        || observed.mode() & 0o7777 != 0o700
    {
        return Err(unsafe_carrier());
    }
    Ok(())
}

fn verify_at_identity(parent: &File, name: &str, file: &File) -> Result<(), RuntimeError> {
    let observed = open_directory(parent, Path::new(name))?;
    if !same_identity(&observed.metadata()?, &file.metadata()?) {
        return Err(unsafe_carrier());
    }
    Ok(())
}

fn open_owned_file(directory: &File, name: &str, mode: u32) -> Result<Option<File>, RuntimeError> {
    use rustix::fs::{Mode, OFlags, openat};
    let file: File = match openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd.into(),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(std::io::Error::from(error).into()),
    };
    let metadata = file.metadata()?;
    let limit = match name {
        "helper" => MAX_HELPER_BYTES,
        "payload" => byte_exec::MAX_PAYLOAD_BYTES as u64,
        _ => 4096,
    };
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != mode
        || metadata.len() > limit
    {
        return Err(unsafe_carrier());
    }
    Ok(Some(file))
}

fn read_record(directory: &File, name: &str) -> Result<Option<String>, RuntimeError> {
    let Some(file) = open_owned_file(directory, name, 0o400)? else {
        return Ok(None);
    };
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text)?;
    if text.len() > 4096 {
        return Err(unsafe_carrier());
    }
    Ok(Some(text))
}

fn remove_files(root: &File, name: &str, directory: &File) -> Result<(), RuntimeError> {
    use rustix::fs::{AtFlags, unlinkat};
    verify_at_identity(root, name, directory)?;
    let modes = [
        ("helper", 0o555),
        ("payload", 0o444),
        ("image", 0o400),
        ("container", 0o400),
        ("dispatched", 0o400),
        ("attempt", 0o400),
    ];
    // Validate ALL leaves first; never partially delete a foreign/replaced set.
    if directory_names(directory, modes.len())?
        .iter()
        .any(|name| !modes.iter().any(|(known, _)| known == name))
    {
        return Err(unsafe_carrier());
    }
    let files = modes
        .iter()
        .map(|(name, mode)| Ok((*name, *mode, open_owned_file(directory, name, *mode)?)))
        .collect::<Result<Vec<_>, RuntimeError>>()?;
    for (leaf, mode, retained) in files {
        if let Some(file) = retained {
            let current = open_owned_file(directory, leaf, mode)?.ok_or_else(unsafe_carrier)?;
            if !same_identity(&file.metadata()?, &current.metadata()?) {
                return Err(unsafe_carrier());
            }
            verify_at_identity(root, name, directory)?;
            unlinkat(directory, leaf, AtFlags::empty()).map_err(std::io::Error::from)?;
        }
    }
    verify_at_identity(root, name, directory)?;
    unlinkat(root, name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
    root.sync_all()?;
    Ok(())
}
