//! Kits marsh knows by name: `marsh kit install NAME` without a reference
//! installs NAME from the tag listed here, and follows that tag like any Kit
//! installed from a tag. `marsh kit install NAME REF` always wins.

/// `(name, REPOSITORY:TAG)`, sorted by name.
pub const BUILTIN_KITS: &[(&str, &str)] = &[
    ("hermes", "docker.io/mcavage/marsh-hermes:stable"),
    ("pix", "docker.io/mcavage/marsh-pix:stable"),
];

/// The tag a known Kit name installs from.
#[must_use]
pub fn reference(name: &str) -> Option<&'static str> {
    BUILTIN_KITS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, reference)| *reference)
}

/// `hermes, pix`, for messages.
#[must_use]
pub fn names() -> String {
    BUILTIN_KITS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{command_registry::CommandName, kit_source::KitTag};

    #[test]
    fn every_known_kit_is_a_valid_name_and_tag_in_order() {
        for (name, reference) in BUILTIN_KITS {
            CommandName::parse((*name).to_owned()).unwrap();
            assert_eq!(KitTag::parse(reference).unwrap().to_string(), *reference);
        }
        assert!(BUILTIN_KITS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(reference("pix"), Some("docker.io/mcavage/marsh-pix:stable"));
        assert_eq!(reference("nope"), None);
        assert_eq!(names(), "hermes, pix");
    }
}
