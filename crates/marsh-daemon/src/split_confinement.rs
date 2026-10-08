//! Kit confinement for split branches (`docs/design/workspaces.md` section 8).
//!
//! A Kit job whose cwd lies in `<root>/.marsh/split/<id>/<label>` (an argv
//! branch, or a Kit command started from a shell branch) mounts only its fork
//! and its fork's Git metadata instead of the whole project: the fork, the
//! admin `objects/` and `index` read-write; the gitfile, `config`, `HEAD`,
//! `info/`, and `packed-refs` read-only binds; the split's `store.git` and the
//! user's `.git/objects` read-only. Every component must be a real directory.

use std::fs;
use std::path::{Component, Path, PathBuf};

/// What a split branch's Kit job may mount, as natural (host = guest) paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchConfinement {
    /// The fork, mounted read-write.
    pub workspace: PathBuf,
    /// Split identity and branch label (receipt lineage).
    pub split: String,
    pub label: String,
    /// `(target, read_write)` binds besides the fork, for a Git split.
    pub binds: Vec<(PathBuf, bool)>,
}

/// Split ids and labels are single safe path components.
#[must_use]
pub fn split_id(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// `[A-Za-z_][A-Za-z0-9_-]{0,63}`, not a name used by the split layout.
#[must_use]
pub fn split_label(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        && !["out", "base", "owner"]
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn real_directories(project: &Path, path: &Path) -> Result<(), String> {
    let relative = path
        .strip_prefix(project)
        .map_err(|_| format!("{} is outside the project", path.display()))?;
    let mut current = project.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("{}: {error}", current.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!("{} is not a plain directory", current.display()));
        }
    }
    Ok(())
}

/// Resolves the split branch confinement for one job: `Ok(None)` for an
/// ordinary job whose cwd is outside `<project>/.marsh`.
///
/// # Errors
/// Returns a one-line reason when the job runs inside `.marsh` but not
/// inside a well-formed fork.
pub fn branch_confinement(project: &Path, cwd: &Path) -> Result<Option<BranchConfinement>, String> {
    let Ok(relative) = cwd.strip_prefix(project) else {
        return Ok(None);
    };
    if relative.components().next().map(Component::as_os_str) != Some(".marsh".as_ref()) {
        return Ok(None);
    }
    let parts = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .ok_or("split workspace is not a plain path")?;
    let (Some(&"split"), Some(id), Some(label)) = (parts.get(1), parts.get(2), parts.get(3)) else {
        return Err("a Kit job inside .marsh must run in a split fork".into());
    };
    if !split_id(id) || !split_label(label) {
        return Err(format!("{} is not a split fork", cwd.display()));
    }
    let split_dir = project.join(".marsh/split").join(id);
    let workspace = split_dir.join(label);
    real_directories(project, &workspace)?;
    let admin = split_dir.join(".admin").join(label);
    let mut binds = Vec::new();
    if fs::symlink_metadata(&admin).is_ok() {
        real_directories(project, &admin)?;
        // The admin directory is writable (index.lock, objects, refs, so a
        // branch can `git add` and `git commit`); the files a host `git`
        // would act on are read-only binds, and capture verification checks
        // them and the admin directory's entries (`AdminReadOnly`).
        binds.extend([
            (admin.clone(), true),
            (admin.join("config"), false),
            (admin.join("HEAD"), false),
            (admin.join("info"), false),
            (admin.join("packed-refs"), false),
            (workspace.join(".git"), false),
            (split_dir.join("store.git"), false),
        ]);
        if fs::symlink_metadata(project.join(".git")).is_ok_and(|m| m.is_dir()) {
            binds.push((project.join(".git/objects"), false));
        }
    }
    Ok(Some(BranchConfinement {
        workspace,
        split: (*id).to_owned(),
        label: (*label).to_owned(),
        binds,
    }))
}

#[cfg(test)]
mod tests {
    //! The bind list is the Kit job's whole view of the user's tree.
    use super::*;

    #[test]
    fn fork_jobs_see_only_their_fork_and_read_only_git_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(temp.path()).unwrap();
        let split = project.join(".marsh/split/abc123");
        fs::create_dir_all(project.join(".git/objects")).unwrap();
        fs::create_dir_all(split.join("fix/src")).unwrap();
        fs::create_dir_all(split.join(".admin/fix")).unwrap();
        let confinement = branch_confinement(&project, &split.join("fix/src"))
            .unwrap()
            .unwrap();
        assert_eq!(confinement.workspace, split.join("fix"));
        let writable = confinement
            .binds
            .iter()
            .filter(|(_, write)| *write)
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        assert_eq!(writable, [split.join(".admin/fix")]);
        for read_only in ["config", "HEAD", "info", "packed-refs"] {
            assert!(
                confinement
                    .binds
                    .contains(&(split.join(".admin/fix").join(read_only), false))
            );
        }
        assert!(confinement.binds.contains(&(split.join("fix/.git"), false)));
        assert!(
            confinement
                .binds
                .contains(&(split.join("store.git"), false))
        );
        assert!(
            confinement
                .binds
                .contains(&(project.join(".git/objects"), false))
        );
        assert!(
            confinement
                .binds
                .iter()
                .all(|(path, _)| path.starts_with(&split) || path == &project.join(".git/objects"))
        );
        // Ordinary jobs are not confined; a job elsewhere in .marsh is refused.
        assert_eq!(branch_confinement(&project, &project), Ok(None));
        assert!(branch_confinement(&project, &project.join(".marsh/split")).is_err());
        // A symlinked fork is never bound.
        std::os::unix::fs::symlink(&project, split.join("evil")).unwrap();
        assert!(branch_confinement(&project, &split.join("evil")).is_err());
    }
}
