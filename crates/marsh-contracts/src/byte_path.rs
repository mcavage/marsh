//! Unix working-directory wire bytes, separate from mount/root authority.
//!
//! This representation conveys a bounded absolute path without UTF-8 decoding.
//! Existing request and runtime owners still validate the path against their
//! retained grants. It is not a grant or a replacement for those checks.

use serde::{
    Deserializer, Serializer,
    de::{Error as _, SeqAccess, Visitor},
    ser::Error as _,
};
use std::{
    fmt,
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Component, Path, PathBuf},
};

const MAX_PATH_BYTES: usize = 4096;
const INVALID_PATH: &str = "invalid byte working directory";

fn valid(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_PATH_BYTES
        && !bytes.contains(&0)
        && path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
}

/// Check only the bounded Unix cwd syntax, not filesystem existence or authority.
///
/// # Errors
/// Rejects relative, oversized, NUL-containing or parent-traversing paths.
pub fn validate(path: &Path) -> Result<(), crate::JobSpecError> {
    if valid(path) {
        Ok(())
    } else {
        Err(crate::JobSpecError::InvalidWorkingDirectory)
    }
}

/// Serialize one working directory as its original Unix byte sequence.
///
/// # Errors
/// Rejects relative, oversized, NUL-containing or parent-traversing paths.
pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    if !valid(path) {
        return Err(S::Error::custom(INVALID_PATH));
    }
    serializer.collect_seq(path.as_os_str().as_bytes().iter().copied())
}

/// Decode bounded raw bytes. No filesystem effects or authority changes occur.
///
/// # Errors
/// Rejects malformed byte arrays or paths outside the same lexical bounds as
/// serialization. String-valued paths do not silently select another format.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
    struct BytePathVisitor;
    impl<'de> Visitor<'de> for BytePathVisitor {
        type Value = PathBuf;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded absolute Unix path byte array")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<PathBuf, A::Error> {
            let mut bytes =
                Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(MAX_PATH_BYTES));
            while let Some(byte) = sequence.next_element::<u8>()? {
                if bytes.len() == MAX_PATH_BYTES {
                    return Err(A::Error::custom(INVALID_PATH));
                }
                bytes.push(byte);
            }
            let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
            if !valid(&path) {
                return Err(A::Error::custom(INVALID_PATH));
            }
            Ok(path)
        }
    }
    deserializer.deserialize_seq(BytePathVisitor)
}

/// A missing legacy request field means no independent cwd was supplied.
/// Present fields must use the same bounded byte array: neither JSON strings
/// nor null are an alternate format. Pair this with `serde(default)` and
/// `skip_serializing_if = "Option::is_none"` on the request field.
pub mod optional {
    use super::*;

    /// Serialize a present cwd using the single byte-path representation.
    ///
    /// # Errors
    /// Rejects invalid paths, or `None` when the caller forgot `skip_serializing_if`.
    #[allow(clippy::ref_option)] // serde(with) requires the field's exact reference type.
    pub fn serialize<S: Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let path = path
            .as_deref()
            .ok_or_else(|| S::Error::custom(INVALID_PATH))?;
        super::serialize(path, serializer)
    }

    /// Deserialize a present cwd. The owning field's default handles absence.
    ///
    /// # Errors
    /// Rejects all non-byte-array forms and invalid byte-path syntax.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        super::deserialize(deserializer).map(Some)
    }
}
