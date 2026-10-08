//! Pure command registry declaration, overlay and update rules.
//!
//! This module does not resolve paths, validate native Kits, write files or call
//! stock SBX. Callers must validate the complete prospective registry before
//! those effects. MCP publication/tool names have different semantics and are
//! deliberately not `CommandName`s.

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, Visitor},
};
use std::{collections::BTreeMap, fmt, sync::OnceLock};

/// Declarative source shared with the repository's Python publisher.
pub const RULES_JSON: &str = include_str!("command_registry_rules.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRegistryRules {
    pub max_commands: usize,
    pub max_name_bytes: usize,
    pub max_document_bytes: usize,
    pub allowed_name_characters: String,
    pub forbidden_name_prefixes: Vec<String>,
    pub reserved_names: Vec<String>,
}

/// Returns the checked-in rules, not user-supplied configuration.
///
/// # Panics
/// Panics if the embedded declarative source is invalid (a build-source defect).
#[must_use]
pub fn rules() -> &'static CommandRegistryRules {
    static RULES: OnceLock<CommandRegistryRules> = OnceLock::new();
    RULES.get_or_init(|| {
        serde_json::from_str(RULES_JSON).expect("checked-in command registry rules")
    })
}

/// A shell dispatch name, not a public MCP tool/publication name or a Kit identity.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommandName(String);

impl CommandName {
    /// Validates without normalizing, truncating or changing the user's name.
    ///
    /// # Errors
    /// Returns an error for a reserved, empty, oversized or unsafe name.
    pub fn parse(name: impl Into<String>) -> Result<Self, RegistryError> {
        let name = name.into();
        let rules = rules();
        if name.is_empty()
            || name.len() > rules.max_name_bytes
            || !name
                .bytes()
                .all(|byte| rules.allowed_name_characters.as_bytes().contains(&byte))
            || rules
                .forbidden_name_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix))
        {
            return Err(RegistryError::InvalidName);
        }
        if rules.reserved_names.contains(&name) {
            return Err(RegistryError::ReservedName);
        }
        Ok(Self(name))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommandName {
    type Error = RegistryError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<CommandName> for String {
    fn from(value: CommandName) -> Self {
        value.0
    }
}

/// Errors intentionally contain no raw registry contents, paths or references.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryError {
    InvalidName,
    ReservedName,
    DuplicateName,
    EmptyReference,
    Capacity,
    DocumentTooLarge,
    InvalidDocument,
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidName => "invalid Kit command name",
            Self::ReservedName => "reserved Kit command name",
            Self::DuplicateName => "duplicate command registry key",
            Self::EmptyReference => "empty Kit reference",
            Self::Capacity => "Kit command registry is full",
            Self::DocumentTooLarge => "command registry document is too large",
            Self::InvalidDocument => {
                "invalid command registry (check names, duplicate keys, references and capacity)"
            }
        })
    }
}
impl std::error::Error for RegistryError {}

/// Validated string references. Native Kit identity/path resolution stays at the
/// backend; duplicate keys cannot disappear during deserialization.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CommandRegistry(BTreeMap<String, String>);

impl CommandRegistry {
    /// Parses a bounded JSON declaration, rejecting duplicate keys before reduction.
    ///
    /// # Errors
    /// Returns an error for an invalid document, name, reference, or bound.
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, RegistryError> {
        if bytes.len() > rules().max_document_bytes {
            return Err(RegistryError::DocumentTooLarge);
        }
        serde_json::from_slice(bytes).map_err(|_| RegistryError::InvalidDocument)
    }

    /// Overlays explicit entries by name. An empty overlay does not disable
    /// packaged commands. Validate each declaration before this operation.
    ///
    /// # Errors
    /// Returns an error if the combined registry exceeds the command cap.
    pub fn merged(mut self, overlay: Self) -> Result<Self, RegistryError> {
        self.0.extend(overlay.0);
        validate_count(self.0.len())?;
        Ok(self)
    }

    /// Adds one new mapping. Unlike an overlay, installation cannot overwrite an
    /// existing mapping. The original registry is unchanged on rejection.
    ///
    /// # Errors
    /// Returns an error for invalid names/references, duplicates or capacity.
    pub fn with_added(&self, command: &str, reference: &str) -> Result<Self, RegistryError> {
        CommandName::parse(command)?;
        if reference.trim().is_empty() {
            return Err(RegistryError::EmptyReference);
        }
        if self.0.contains_key(command) {
            return Err(RegistryError::DuplicateName);
        }
        validate_count(self.0.len().saturating_add(1))?;
        let mut updated = self.clone();
        updated.0.insert(command.to_owned(), reference.to_owned());
        // Installation must not write a document its own loader cannot read.
        updated.to_json_pretty()?;
        Ok(updated)
    }

    /// Encodes the canonical on-disk form, including its final newline.
    ///
    /// # Errors
    /// Rejects a serialized document exceeding the loader's byte bound.
    pub fn to_json_pretty(&self) -> Result<Vec<u8>, RegistryError> {
        let mut bytes =
            serde_json::to_vec_pretty(self).map_err(|_| RegistryError::InvalidDocument)?;
        bytes.push(b'\n');
        if bytes.len() > rules().max_document_bytes {
            return Err(RegistryError::DocumentTooLarge);
        }
        Ok(bytes)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, reference)| (name.as_str(), reference.as_str()))
    }

    #[must_use]
    pub fn into_entries(self) -> BTreeMap<String, String> {
        self.0
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Shared cap check for already validated runtime registries.
///
/// # Errors
/// Returns an error if `count` exceeds the declared maximum.
pub fn validate_count(count: usize) -> Result<(), RegistryError> {
    if count > rules().max_commands {
        Err(RegistryError::Capacity)
    } else {
        Ok(())
    }
}

impl<'de> Deserialize<'de> for CommandRegistry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RegistryVisitor;
        impl<'de> Visitor<'de> for RegistryVisitor {
            type Value = CommandRegistry;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object mapping Kit command names to string references")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = BTreeMap::new();
                while let Some((command, reference)) = map.next_entry::<String, String>()? {
                    CommandName::parse(command.clone()).map_err(serde::de::Error::custom)?;
                    if reference.trim().is_empty() {
                        return Err(serde::de::Error::custom(RegistryError::EmptyReference));
                    }
                    if entries.insert(command, reference).is_some() {
                        return Err(serde::de::Error::custom(RegistryError::DuplicateName));
                    }
                    validate_count(entries.len()).map_err(serde::de::Error::custom)?;
                }
                Ok(CommandRegistry(entries))
            }
        }
        deserializer.deserialize_map(RegistryVisitor)
    }
}
