use std::{
    fs,
    io::Write as _,
    os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Prepare the persistent guest backing directory, never the host control root.
///
/// # Errors
/// Refuses symlinks, foreign ownership and group/other-writable directories.
pub fn prepare_persistent_guest_home(path: &Path, uid: u32) -> Result<PathBuf, &'static str> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .map_err(|_| "cannot create selected guest home")?;
        }
        Err(_) => return Err("cannot inspect selected guest home"),
        Ok(_) => {}
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| "cannot inspect selected guest home")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("selected guest home must be a directory");
    }
    if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err("selected guest home ownership or mode is unsafe");
    }
    path.canonicalize()
        .map_err(|_| "cannot resolve selected guest home")
}

/// What [`seed_git_identity`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitSeed {
    /// `.gitconfig` was written with the identity.
    Seeded,
    /// A `.gitconfig` (or anything else at that path) is already there; it is never touched.
    AlreadyPresent,
    /// There was no usable identity to copy.
    NoIdentity,
}

const GIT_IDENTITY_LIMIT: usize = 256;

fn identity_value(value: &str) -> Option<String> {
    let value = value.trim();
    let valid = !value.is_empty()
        && value.len() <= GIT_IDENTITY_LIMIT
        && !value.chars().any(char::is_control);
    valid.then(|| value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Write `$HOME/.gitconfig` in the guest home with a git identity, only if no
/// file is there. Jobs and shells share this home, so one file gives every
/// Kit and the shell a name and email to commit with. Only the name and email
/// are written: never a credential helper, signing key, or include.
///
/// # Errors
/// Reports a failed write; a missing or invalid identity is `NoIdentity`.
pub fn seed_git_identity(home: &Path, identity: Option<(&str, &str)>) -> std::io::Result<GitSeed> {
    let target = home.join(".gitconfig");
    if fs::symlink_metadata(&target).is_ok() {
        return Ok(GitSeed::AlreadyPresent);
    }
    let Some((name, email)) = identity else {
        return Ok(GitSeed::NoIdentity);
    };
    let (Some(name), Some(email)) = (identity_value(name), identity_value(email)) else {
        return Ok(GitSeed::NoIdentity);
    };
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&target)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(GitSeed::AlreadyPresent);
        }
        Err(error) => return Err(error),
    };
    file.write_all(format!("[user]\n\tname = \"{name}\"\n\temail = \"{email}\"\n").as_bytes())?;
    Ok(GitSeed::Seeded)
}

fn host_git_value(key: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["config", "--global", "--get", key])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .map(|value| value.trim().to_owned())
}

/// The git name and email configured on this machine, if both are.
#[must_use]
pub fn host_git_identity() -> Option<(String, String)> {
    Some((host_git_value("user.name")?, host_git_value("user.email")?))
}

/// [`seed_git_identity`] from this machine's git configuration. Best effort:
/// it asks git only when the guest home has no `.gitconfig`, and a failure
/// never stops a session from starting.
#[must_use]
pub fn seed_git_identity_from_host(home: &Path) -> GitSeed {
    if fs::symlink_metadata(home.join(".gitconfig")).is_ok() {
        return GitSeed::AlreadyPresent;
    }
    let identity = host_git_identity();
    let identity = identity.as_ref().map(|(name, email)| (name.as_str(), email.as_str()));
    seed_git_identity(home, identity).unwrap_or(GitSeed::NoIdentity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(home: &Path) -> String {
        fs::read_to_string(home.join(".gitconfig")).unwrap()
    }

    #[test]
    fn seeds_the_name_and_email_and_nothing_else() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            seed_git_identity(home.path(), Some(("Mark Cavage", "mark@example.com"))).unwrap(),
            GitSeed::Seeded
        );
        assert_eq!(read(home.path()), "[user]\n\tname = \"Mark Cavage\"\n\temail = \"mark@example.com\"\n");
        assert_eq!(fs::metadata(home.path().join(".gitconfig")).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn never_replaces_an_existing_file_or_link() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join(".gitconfig"), "[user]\n\tname = Mine\n").unwrap();
        assert_eq!(
            seed_git_identity(home.path(), Some(("Other", "o@example.com"))).unwrap(),
            GitSeed::AlreadyPresent
        );
        assert_eq!(read(home.path()), "[user]\n\tname = Mine\n");

        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/nonexistent", linked.path().join(".gitconfig")).unwrap();
        assert_eq!(
            seed_git_identity(linked.path(), Some(("Other", "o@example.com"))).unwrap(),
            GitSeed::AlreadyPresent
        );
    }

    #[test]
    fn a_missing_or_unusable_identity_writes_nothing() {
        for identity in [
            None,
            Some(("", "a@b")),
            Some(("A", "  ")),
            Some(("A\nB", "a@b")),
            Some(("A", "a@b\0")),
        ] {
            let home = tempfile::tempdir().unwrap();
            assert_eq!(seed_git_identity(home.path(), identity).unwrap(), GitSeed::NoIdentity, "{identity:?}");
            assert!(!home.path().join(".gitconfig").exists());
        }
        let home = tempfile::tempdir().unwrap();
        let long = "x".repeat(GIT_IDENTITY_LIMIT + 1);
        assert_eq!(seed_git_identity(home.path(), Some((&long, "a@b"))).unwrap(), GitSeed::NoIdentity);
    }

    #[test]
    fn quotes_and_backslashes_are_escaped_so_git_reads_the_same_value() {
        let home = tempfile::tempdir().unwrap();
        seed_git_identity(home.path(), Some((r#"Ann "Ace" O\Neil"#, "a@b.c"))).unwrap();
        let out = std::process::Command::new("git")
            .args(["config", "--file"])
            .arg(home.path().join(".gitconfig"))
            .args(["--get", "user.name"])
            .output();
        if let Ok(out) = out {
            // Where git is installed, it must parse back exactly what was given.
            assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), r#"Ann "Ace" O\Neil"#);
        }
    }
}
