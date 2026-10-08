//! Host registry I/O. The contracts crate owns declaration/overlay/update rules;
//! this adapter alone resolves relative native Kit paths for loading/installing.

use crate::RegisteredKit;
use marsh_contracts::command_registry::{CommandRegistry, rules};
use marsh_sbx::NativeKitRef;
use std::{
    collections::BTreeMap,
    fmt, fs,
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

/// Safe startup diagnostic: identifies the file and stage, never its contents.
#[derive(Debug)]
pub struct RegistryConfigError {
    path: PathBuf,
    reason: &'static str,
}
impl fmt::Display for RegistryConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.reason)
    }
}
impl std::error::Error for RegistryConfigError {}

fn invalid(path: &Path, reason: &'static str) -> RegistryConfigError {
    RegistryConfigError {
        path: path.into(),
        reason,
    }
}

/// Reads an optional, bounded declaration. Only `NotFound` means absent.
///
/// # Errors
/// Rejects unreadable, oversized and invalid documents (including duplicates).
pub fn read_declaration(path: &Path) -> Result<CommandRegistry, RegistryConfigError> {
    let flags = nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK;
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CommandRegistry::default());
        }
        Err(_) => return Err(invalid(path, "cannot read command registry")),
    };
    if !file
        .metadata()
        .map_err(|_| invalid(path, "cannot inspect command registry"))?
        .is_file()
    {
        return Err(invalid(path, "command registry must be a regular file"));
    }
    let limit = rules().max_document_bytes;
    let mut bytes = Vec::new();
    file.take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| invalid(path, "cannot read command registry"))?;
    CommandRegistry::from_json_slice(&bytes).map_err(|_| {
        invalid(
            path,
            "invalid command registry; check names, duplicates, references and capacity",
        )
    })
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RegistryReference {
    ImmutableOci(String),
    LocalV3(PathBuf),
}

pub(crate) fn resolve_registry_reference(
    registry: &Path,
    value: String,
) -> Result<RegistryReference, RegistryConfigError> {
    if NativeKitRef::immutable_oci(value.clone()).is_ok() {
        return Ok(RegistryReference::ImmutableOci(value));
    }
    if value.trim().is_empty() {
        return Err(invalid(registry, "empty local Kit path"));
    }
    let parent = registry
        .parent()
        .ok_or_else(|| invalid(registry, "command registry has no parent"))?;
    let value = PathBuf::from(value);
    let candidate = if value.is_absolute() {
        value
    } else {
        parent.join(value)
    };
    let candidate = candidate
        .canonicalize()
        .map_err(|_| invalid(registry, "cannot resolve local Kit path"))?;
    Ok(RegistryReference::LocalV3(candidate))
}

fn resolve(registry: &Path, reference: String) -> Result<RegisteredKit, RegistryConfigError> {
    let workload = match resolve_registry_reference(registry, reference)? {
        RegistryReference::ImmutableOci(value) => NativeKitRef::immutable_oci(value),
        RegistryReference::LocalV3(directory) => NativeKitRef::local_v3_source(directory),
    }
    .map_err(|_| invalid(registry, "invalid native Kit reference or local source"))?;
    Ok(RegisteredKit { workload })
}

/// Validates a pending control registry before stock calls or persistence.
///
/// # Errors
/// Returns the safe file/stage diagnostic for an invalid Kit reference.
pub fn validate_references(
    path: &Path,
    registry: &CommandRegistry,
) -> Result<(), RegistryConfigError> {
    for (_, reference) in registry.iter() {
        resolve(path, reference.into())?;
    }
    Ok(())
}

/// Loads command overlays. Each declaration and the merged cap are validated
/// before any Kit source is resolved. Relative paths retain their origin file.
///
/// # Errors
/// Rejects invalid declarations, unresolved references or an empty merged set.
pub fn load_commands(
    packaged: &Path,
    user: &Path,
) -> Result<BTreeMap<String, RegisteredKit>, RegistryConfigError> {
    let base = read_declaration(packaged)?;
    let overlay = read_declaration(user)?;
    let merged = base
        .clone()
        .merged(overlay.clone())
        .map_err(|_| invalid(user, "combined command registry exceeds capacity"))?;
    if merged.is_empty() {
        return Err(invalid(
            user,
            "no commands in install or host control registry",
        ));
    }
    // Validate references even in a shadowed declaration; an override must not
    // conceal a broken source registry. Resolve each path relative to its file.
    let mut commands = BTreeMap::new();
    for (origin, registry) in [(packaged, base), (user, overlay)] {
        for (command, reference) in registry.into_entries() {
            commands.insert(command, resolve(origin, reference)?);
        }
    }
    Ok(commands)
}
