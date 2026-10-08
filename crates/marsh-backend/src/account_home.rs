//! Resolve host-only daemon state against the login account, not a caller's HOME.

use std::{ffi::OsStr, fs, path::PathBuf};

/// Return the canonical login home only when HOME names that same directory.
/// A mismatched HOME must not hide the account's host-only control
/// tree from sandbox mount admission.
///
/// # Errors
///
/// Fails if the process identity or login account cannot be resolved, or if
/// HOME is missing, relative, or resolves to a different directory.
pub fn trusted_account_home(home: Option<&OsStr>) -> Result<PathBuf, String> {
    let uid = nix::unistd::Uid::current();
    if uid != nix::unistd::Uid::effective() {
        return Err("marshd requires matching real and effective user IDs".into());
    }
    let account = nix::unistd::User::from_uid(uid)
        .map_err(|error| format!("cannot resolve account home for UID {uid}: {error}"))?
        .ok_or_else(|| format!("UID {uid} has no account home"))?;
    let trusted = fs::canonicalize(&account.dir)
        .map_err(|error| format!("cannot resolve account home: {error}"))?;
    let supplied = home.ok_or("HOME is required")?;
    let supplied = PathBuf::from(supplied);
    if !supplied.is_absolute() {
        return Err("HOME must be an absolute path to the login account home".into());
    }
    let supplied =
        fs::canonicalize(&supplied).map_err(|error| format!("cannot resolve HOME: {error}"))?;
    if supplied != trusted {
        return Err("HOME differs from the login account home".into());
    }
    Ok(trusted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_login_account_home() {
        let uid = nix::unistd::Uid::current();
        let account = nix::unistd::User::from_uid(uid).unwrap().unwrap();
        let expected = account.dir.canonicalize().unwrap();
        assert_eq!(
            trusted_account_home(Some(account.dir.as_os_str())),
            Ok(expected)
        );
        assert!(trusted_account_home(None).is_err());
        assert!(trusted_account_home(Some(OsStr::new("relative/home"))).is_err());
        let fake = tempfile::tempdir().unwrap();
        assert_eq!(
            trusted_account_home(Some(fake.path().as_os_str())).unwrap_err(),
            "HOME differs from the login account home"
        );
    }
}
