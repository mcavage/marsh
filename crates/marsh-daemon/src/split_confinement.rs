//! Kit confinement for split branches (`docs/design/workspaces.md` section 8).
//!
//! A Kit job whose cwd lies in `<root>/.marsh/split/<id>/<label>` (an argv
//! branch, or a Kit command started from a shell branch) mounts only its fork
//! and its fork's Git metadata instead of the whole project: the fork, the
//! admin `objects/` and `index` read-write; the gitfile, `config`, `HEAD`,
//! `info/`, `packed-refs`, `commondir` (`./`) and empty `hooks/`, `modules/`
//! and `worktrees/` read-only binds; the split's `store.git` and the user's `.git/objects` read-only.
//! Every component must be a real directory.

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

/// Whether the admin dir holds the daemon's placeholders: `commondir` as
/// written, and each of `ADMIN_EMPTY_DIRS` an empty real directory.
fn placeholders_in_place(admin: &Path) -> bool {
    let commondir = fs::symlink_metadata(admin.join("commondir"))
        .is_ok_and(|metadata| metadata.is_file())
        && fs::read(admin.join("commondir"))
            .is_ok_and(|bytes| bytes == crate::split_fs::ADMIN_COMMONDIR);
    commondir
        && crate::split_fs::ADMIN_EMPTY_DIRS.iter().all(|name| {
            let path = admin.join(name);
            fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_dir())
                && fs::read_dir(&path).is_ok_and(|mut entries| entries.next().is_none())
        })
}

/// The project whose fork holds `path` (`<root>/.marsh/split/<id>/<label>/...`),
/// so a command started in a lane runs with that project as its root.
#[must_use]
pub fn fork_project(path: &Path) -> Option<PathBuf> {
    let parts = path.components().collect::<Vec<_>>();
    let at = parts.windows(4).position(|window| match window {
        [
            Component::Normal(marsh),
            Component::Normal(split),
            Component::Normal(id),
            Component::Normal(label),
        ] => {
            *marsh == ".marsh"
                && *split == "split"
                && id.to_str().is_some_and(split_id)
                && label.to_str().is_some_and(split_label)
        }
        _ => false,
    })?;
    Some(parts[..at].iter().collect())
}

/// Resolves the split branch confinement for one job: `Ok(None)` for an
/// ordinary job whose cwd is outside `<project>/.marsh`.
///
/// # Errors
/// Returns a one-line reason when the job runs inside `.marsh` but not
/// inside a well-formed fork.
pub fn branch_confinement(project: &Path, cwd: &Path) -> Result<Option<BranchConfinement>, String> {
    // A project root inside `.marsh` (a shell opened in a fork) would mount
    // the fork without its Git metadata and with a writable gitfile.
    if project
        .components()
        .any(|component| component == Component::Normal(".marsh".as_ref()))
    {
        return Err(format!(
            "{} is inside .marsh; run marsh from the project root",
            project.display()
        ));
    }
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
        // A fork made before these placeholders existed (a lane kept open
        // across an upgrade) would admit a job that plants them: refused.
        if !placeholders_in_place(&admin) {
            return Err(format!(
                "split {id} was made by an older marsh and lacks Git safeguards; close it (marsh split close {id}) and open a new one"
            ));
        }
        binds.push((admin.join("commondir"), false));
        for name in crate::split_fs::ADMIN_EMPTY_DIRS {
            binds.push((admin.join(name), false));
        }
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
        for dir in crate::split_fs::ADMIN_EMPTY_DIRS {
            fs::create_dir_all(split.join(".admin/fix").join(dir)).unwrap();
        }
        fs::write(split.join(".admin/fix/commondir"), "./\n").unwrap();
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
        for read_only in [
            "config",
            "HEAD",
            "info",
            "packed-refs",
            "commondir",
            "hooks",
            "modules",
            "worktrees",
        ] {
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
        // A fork from before the placeholders, or one whose placeholders a
        // job filled, admits no job.
        fs::write(split.join(".admin/fix/hooks/post-checkout"), "x").unwrap();
        assert!(branch_confinement(&project, &split.join("fix")).is_err());
        fs::remove_file(split.join(".admin/fix/hooks/post-checkout")).unwrap();
        fs::remove_file(split.join(".admin/fix/commondir")).unwrap();
        let refused = branch_confinement(&project, &split.join("fix")).unwrap_err();
        assert!(refused.contains("older marsh"), "{refused}");
    }

    /// The admin dir the daemon writes for a fork is what confinement binds:
    /// its placeholders exist, so a job of the fork is admitted with them
    /// read-only.
    #[test]
    fn a_written_admin_dir_admits_its_jobs_with_placeholders_read_only() {
        use crate::split_fs::{Dir, GitSnapshot, admin_contents, write_admin};
        let temp = tempfile::tempdir().unwrap();
        let project = fs::canonicalize(temp.path()).unwrap();
        let split = project.join(".marsh/split/abc123");
        fs::create_dir_all(split.join("fix")).unwrap();
        fs::create_dir_all(split.join(".admin/fix")).unwrap();
        let git = GitSnapshot::default();
        let root = Dir::open_root(&split).unwrap();
        write_admin(
            &git,
            &admin_contents(&git, &split.join("store.git")),
            &root.open(".admin").unwrap().open("fix").unwrap(),
            &split.join(".admin/fix"),
            &root.open("fix").unwrap(),
        )
        .unwrap();
        let confinement = branch_confinement(&project, &split.join("fix"))
            .unwrap()
            .unwrap();
        assert!(
            confinement
                .binds
                .contains(&(split.join(".admin/fix/commondir"), false))
        );
    }
}
