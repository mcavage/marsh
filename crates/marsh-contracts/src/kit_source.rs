//! Moving Kit tags that `marsh kit install` and `marsh kit upgrade` follow.
//!
//! A tag is never run. It names what to follow; the command registry always
//! holds the digest the tag resolved to when it was installed or upgraded.

use crate::OciImage;
use std::{fmt, ops::RangeInclusive};

/// Image label naming the marsh Kit contract an image implements: the
/// entrypoint conventions marsh relies on (`MARSH_SELECTED_HOME`,
/// `/run/marsh/context.md`, `--acp`, credential mode and Gateway variables).
pub const KIT_CONTRACT_LABEL: &str = "org.marsh.kit.contract";

/// Kit contracts this marsh can run. Bump the image label, and this range, only
/// when that contract changes.
pub const SUPPORTED_KIT_CONTRACTS: RangeInclusive<u32> = 1..=1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KitTagError {
    /// Not `REPOSITORY:TAG`.
    Invalid,
    /// A digest reference was given where a tag was expected.
    Digest,
}

impl fmt::Display for KitTagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Invalid => "expected REPOSITORY:TAG or REPOSITORY@sha256:DIGEST",
            Self::Digest => "a digest reference has no tag to follow",
        })
    }
}

impl std::error::Error for KitTagError {}

/// `REPOSITORY:TAG`, with the repository normalized to its full Docker form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KitTag {
    repository: String,
    tag: String,
}

impl KitTag {
    /// # Errors
    /// Rejects digest references and anything that is not `REPOSITORY:TAG`.
    pub fn parse(value: &str) -> Result<Self, KitTagError> {
        if value.contains('@') {
            return Err(if OciImage::parse(value).is_ok() {
                KitTagError::Digest
            } else {
                KitTagError::Invalid
            });
        }
        let (repository, tag) = value.rsplit_once(':').ok_or(KitTagError::Invalid)?;
        // A colon before the last slash is a registry port, not a tag.
        if repository.is_empty() || tag.contains('/') || !valid_tag(tag) {
            return Err(KitTagError::Invalid);
        }
        let repository = normalize_repository(repository);
        // The digest form of the same repository must be a valid image.
        OciImage::parse(format!("{repository}@sha256:{}", "0".repeat(64)))
            .map_err(|_| KitTagError::Invalid)?;
        Ok(Self {
            repository,
            tag: tag.to_owned(),
        })
    }

    /// The same repository with another tag (a packaged Kit's channel).
    ///
    /// # Errors
    /// Rejects an invalid tag.
    pub fn for_repository(image: &OciImage, tag: &str) -> Result<Self, KitTagError> {
        let repository = image_repository(image).ok_or(KitTagError::Invalid)?;
        Self::parse(&format!("{repository}:{tag}"))
    }

    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }

    #[must_use]
    pub fn tag(&self) -> &str {
        &self.tag
    }

    /// Accepts what the tag resolved to only if it is an immutable reference to
    /// this tag's repository, so a registry answer cannot redirect the name.
    ///
    /// # Errors
    /// Returns a message naming the unexpected resolution.
    pub fn check_resolution(&self, resolved: &str) -> Result<OciImage, String> {
        let image = OciImage::parse(resolved)
            .map_err(|_| format!("{self} did not resolve to a digest (got {resolved:?})"))?;
        if image_repository(&image).as_deref() != Some(self.repository.as_str()) {
            return Err(format!(
                "{self} resolved to another repository ({resolved}); refusing"
            ));
        }
        Ok(image)
    }
}

impl fmt::Display for KitTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.repository, self.tag)
    }
}

/// The normalized repository of a digest reference, or `None` for a bare digest.
#[must_use]
pub fn image_repository(image: &OciImage) -> Option<String> {
    let (repository, _) = image.as_str().rsplit_once("@sha256:")?;
    Some(normalize_repository(repository))
}

/// Whether an image declaring `contract` may run. A packaged Kit must declare
/// a supported contract; another Kit may declare none (marsh cannot know), but
/// one it declares must be supported.
///
/// # Errors
/// Returns a user-facing reason.
pub fn check_contract(contract: Option<u32>, packaged: bool) -> Result<(), String> {
    match contract {
        Some(value) if SUPPORTED_KIT_CONTRACTS.contains(&value) => Ok(()),
        Some(value) => Err(format!(
            "the image needs marsh Kit contract {value}; this marsh supports {}-{}: upgrade marsh first",
            SUPPORTED_KIT_CONTRACTS.start(),
            SUPPORTED_KIT_CONTRACTS.end()
        )),
        None if packaged => Err(format!(
            "the image does not declare a marsh Kit contract ({KIT_CONTRACT_LABEL}); it predates this marsh's upgrade support"
        )),
        None => Ok(()),
    }
}

/// Parses a label value; anything but a plain decimal is treated as undeclared.
#[must_use]
pub fn parse_contract(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn valid_tag(tag: &str) -> bool {
    let bytes = tag.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphanumeric() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// `mcavage/x` and `docker.io/mcavage/x` name one repository; so do `ubuntu`
/// and `docker.io/library/ubuntu`.
fn normalize_repository(repository: &str) -> String {
    let (first, rest) = repository.split_once('/').unwrap_or(("", repository));
    let has_registry =
        !first.is_empty() && (first.contains('.') || first.contains(':') || first == "localhost");
    match (has_registry, first) {
        (true, "docker.io" | "index.docker.io" | "registry-1.docker.io") => normalize_hub(rest),
        (true, _) => repository.to_owned(),
        (false, _) => normalize_hub(repository),
    }
}

fn normalize_hub(path: &str) -> String {
    if path.contains('/') {
        format!("docker.io/{path}")
    } else {
        format!("docker.io/library/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn tags_normalize_and_reject_digests_and_malformed_references() {
        let tag = KitTag::parse("mcavage/marsh-pix:stable").unwrap();
        assert_eq!(tag.to_string(), "docker.io/mcavage/marsh-pix:stable");
        assert_eq!(
            KitTag::parse("docker.io/mcavage/marsh-pix:stable").unwrap(),
            tag
        );
        assert_eq!(
            KitTag::parse("registry.example:5000/team/kit:1.2")
                .unwrap()
                .repository(),
            "registry.example:5000/team/kit"
        );
        assert_eq!(
            KitTag::parse(&format!("mcavage/marsh-pix@{DIGEST}")),
            Err(KitTagError::Digest)
        );
        for invalid in [
            "",
            "mcavage/marsh-pix",
            "registry.example:5000/team/kit",
            "mcavage/marsh-pix:",
            "mcavage/marsh-pix:-x",
            "mcavage/Marsh:tag",
            "mcavage/marsh-pix:a/b",
            &format!("mcavage/marsh-pix:{}", "a".repeat(129)),
        ] {
            assert_eq!(
                KitTag::parse(invalid),
                Err(KitTagError::Invalid),
                "{invalid}"
            );
        }
    }

    #[test]
    fn a_resolution_must_be_a_digest_of_the_same_repository() {
        let tag = KitTag::parse("mcavage/marsh-pix:stable").unwrap();
        assert!(
            tag.check_resolution(&format!("docker.io/mcavage/marsh-pix@{DIGEST}"))
                .is_ok()
        );
        assert!(
            tag.check_resolution(&format!("mcavage/marsh-pix@{DIGEST}"))
                .is_ok()
        );
        for wrong in [
            format!("docker.io/attacker/marsh-pix@{DIGEST}"),
            "docker.io/mcavage/marsh-pix:stable".into(),
            DIGEST.into(),
        ] {
            assert!(tag.check_resolution(&wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn packaged_channel_tags_keep_the_pinned_repository() {
        let pinned = OciImage::parse(format!("docker.io/mcavage/marsh-pi@{DIGEST}")).unwrap();
        assert_eq!(
            KitTag::for_repository(&pinned, "nightly")
                .unwrap()
                .to_string(),
            "docker.io/mcavage/marsh-pi:nightly"
        );
    }

    #[test]
    fn contracts_gate_packaged_kits_strictly_and_other_kits_when_declared() {
        assert!(check_contract(Some(1), true).is_ok());
        assert!(check_contract(Some(1), false).is_ok());
        assert!(check_contract(None, false).is_ok());
        assert!(
            check_contract(None, true)
                .unwrap_err()
                .contains(KIT_CONTRACT_LABEL)
        );
        let newer = SUPPORTED_KIT_CONTRACTS.end() + 1;
        for packaged in [true, false] {
            assert!(
                check_contract(Some(newer), packaged)
                    .unwrap_err()
                    .contains("upgrade marsh first")
            );
        }
        assert_eq!(parse_contract(" 1\n"), Some(1));
        for undeclared in ["", "v1", "-1", "1.0", "999999999999"] {
            assert_eq!(parse_contract(undeclared), None, "{undeclared}");
        }
    }
}

#[cfg(test)]
mod packaged_kit_labels {
    use super::*;
    use std::path::Path;

    /// Every packaged Kit declares the newest contract this marsh supports, so
    /// a contract bump cannot ship on one side only.
    #[test]
    fn packaged_kit_images_declare_the_supported_contract() {
        let kits = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kits");
        let expected = format!(
            "LABEL {KIT_CONTRACT_LABEL}={}",
            SUPPORTED_KIT_CONTRACTS.end()
        );
        let mut checked = 0;
        for kit in std::fs::read_dir(&kits).unwrap() {
            let kit = kit.unwrap().path();
            if !kit.is_dir() {
                continue;
            }
            for file in std::fs::read_dir(&kit).unwrap() {
                let file = file.unwrap().path();
                if file
                    .extension()
                    .is_some_and(|extension| extension == "dockerfile")
                {
                    let text = std::fs::read_to_string(&file).unwrap();
                    assert_eq!(
                        text.lines().filter(|line| line.trim() == expected).count(),
                        1,
                        "{} must declare `{expected}` once",
                        file.display()
                    );
                    checked += 1;
                }
            }
        }
        assert!(
            checked >= 4,
            "found only {checked} packaged Kit Dockerfiles"
        );
    }
}
