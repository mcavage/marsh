//! Stock SBX's shared skills store, through its public `sbx skills` CLI
//! (`docs/design/processes.md` s9). marsh's daemon mirrors the folders
//! `sbx skills import` scans into it.

use super::{SbxError, StockSbx};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The folders `sbx skills import --help` lists (v0.51), used only when the
/// help text names none.
pub const FALLBACK_IMPORT_FOLDERS: [&str; 6] = [
    "~/.agents/skills",
    "~/.claude/skills",
    "~/.config/opencode/skills",
    "~/.copilot/skills",
    "~/.cursor/skills",
    "~/.factory/skills",
];

/// The store's directory and the skill names in it (`sbx skills ls --json`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillsStore {
    pub directory: PathBuf,
    pub skills: Vec<String>,
}

/// The `N. ~/path` lines of `sbx skills import --help`, in order.
#[must_use]
pub fn import_folders_from_help(help: &str) -> Vec<String> {
    help.lines()
        .filter_map(|line| {
            let (number, rest) = line.trim().split_once(". ")?;
            (number.parse::<u8>().is_ok() && rest.starts_with("~/")).then(|| rest.trim().to_owned())
        })
        .collect()
}

impl StockSbx {
    /// The HOME stock SBX runs with: the scanned folders and the store are
    /// relative to it.
    #[must_use]
    pub fn stock_home(&self) -> Option<PathBuf> {
        self.runner
            .stock_control_environment(&self.sbx)
            .map(|(home, _)| home)
    }

    /// The folders `sbx skills import` scans, in its order, as absolute paths
    /// under `home` (the stock HOME when `None`).
    ///
    /// # Errors
    /// Fails when the help cannot be read.
    pub fn skills_import_folders(&self, home: Option<&Path>) -> Result<Vec<PathBuf>, SbxError> {
        let output = self.run_bounded_os(
            "read sbx skills import folders",
            self.preparation_timeouts.sbx,
            ["skills", "import", "--help"],
        )?;
        let mut folders = import_folders_from_help(&String::from_utf8_lossy(&output.stdout));
        if folders.is_empty() {
            folders = FALLBACK_IMPORT_FOLDERS.map(str::to_owned).to_vec();
        }
        let home = home
            .map(Path::to_owned)
            .or_else(|| self.stock_home())
            .unwrap_or_default();
        Ok(folders
            .iter()
            .filter_map(|folder| folder.strip_prefix("~/"))
            .map(|relative| home.join(relative))
            .collect())
    }

    /// `sbx skills ls --json`.
    ///
    /// # Errors
    /// Fails when stock SBX fails or prints something else.
    pub fn skills_store(&self) -> Result<SkillsStore, SbxError> {
        let output = self.run_bounded_os(
            "list sbx skills",
            self.preparation_timeouts.sbx,
            ["skills", "ls", "--json"],
        )?;
        Self::require_success("list sbx skills", output.clone())?;
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| SbxError::KitResolution(format!("sbx skills ls: {error}")))?;
        let directory = value["store"]
            .as_str()
            .filter(|store| Path::new(store).is_absolute())
            .ok_or_else(|| SbxError::KitResolution("sbx skills ls: no store".into()))?;
        let skills = value["skills"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect();
        Ok(SkillsStore {
            directory: PathBuf::from(directory),
            skills,
        })
    }

    /// `sbx skills import --force --no-input` with `home` as HOME: it scans
    /// `home`'s folders, and writes the store `home` leads to.
    ///
    /// # Errors
    /// Fails when the import fails.
    pub fn skills_import(&self, home: &Path) -> Result<(), SbxError> {
        let mut invocation = self.invocation(["skills", "import", "--force", "--no-input"]);
        invocation
            .environment
            .push((OsString::from("HOME"), home.as_os_str().to_owned()));
        let output = self
            .runner
            .run_bounded(&invocation, self.preparation_timeouts.sbx)?;
        Self::require_success("import sbx skills", output)
    }

    /// `sbx skills rm --force --no-input NAME...`.
    ///
    /// # Errors
    /// Fails when the removal fails.
    pub fn skills_remove(&self, names: &[String]) -> Result<(), SbxError> {
        let mut arguments = vec!["skills", "rm", "--force", "--no-input"];
        arguments.extend(names.iter().map(String::as_str));
        let output = self.run_bounded_os(
            "remove sbx skills",
            self.preparation_timeouts.sbx,
            arguments,
        )?;
        Self::require_success("remove sbx skills", output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_import_folders_in_order() {
        let help = "Description:\n  Directories are checked in order:\n\n  1. ~/.agents/skills\n  2. ~/.claude/skills\n\n  The first copy wins.\n";
        assert_eq!(
            import_folders_from_help(help),
            ["~/.agents/skills", "~/.claude/skills"]
        );
        assert_eq!(
            import_folders_from_help("Usage:\n  sbx skills import"),
            [] as [String; 0]
        );
    }
}
