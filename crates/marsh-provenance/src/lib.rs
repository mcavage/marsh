//! Build-recorded feature provenance for shipped binaries
//! (`docs/design/test-breaks.md`, "Releases").
//!
//! Every crate that ships an executable has a `build.rs` calling
//! [`build::emit`] and calls [`embed!`] in each of its `main` functions. The
//! build script knows whether the crate's own `test-breaks` feature, or the
//! feature of any `marsh-*` crate it depends on directly, is enabled. When it
//! is, the executable carries the marker `marsh-feature:test-breaks`. The
//! marker comes from the build, not from any code that uses the feature, so it
//! is present even when no test-only code or `marsh-contracts` item is linked
//! into the executable.

/// Evaluates to the build's provenance marker (empty for a normal build) and
/// keeps it live in the executable. A crate without the build script fails to
/// compile here, so an executable cannot ship unwired.
#[macro_export]
macro_rules! embed {
    () => {
        ::std::hint::black_box(env!("MARSH_PROVENANCE_MARKER"))
    };
}

/// For `build.rs` (feature `build`).
#[cfg(feature = "build")]
pub mod build {
    /// Feature variable a dependency's build script exports (`links` metadata):
    /// `DEP_<LINKS>_TEST_BREAKS=1`, for `marsh-*` crates only.
    fn dependency_has_test_breaks(name: &str, value: &str) -> bool {
        name.starts_with("DEP_MARSH_") && name.ends_with("_TEST_BREAKS") && value == "1"
    }

    /// Whether this crate, or a direct `marsh-*` dependency, has `test-breaks`.
    #[must_use]
    pub fn test_breaks_enabled() -> bool {
        std::env::var_os("CARGO_FEATURE_TEST_BREAKS").is_some()
            || std::env::vars().any(|(name, value)| dependency_has_test_breaks(&name, &value))
    }

    /// Sets `MARSH_PROVENANCE_MARKER` for this crate's compilation.
    pub fn emit() {
        let marker = if test_breaks_enabled() {
            "marsh-feature:test-breaks"
        } else {
            ""
        };
        println!("cargo::rerun-if-changed=build.rs");
        println!("cargo::rustc-env=MARSH_PROVENANCE_MARKER={marker}");
    }
}
