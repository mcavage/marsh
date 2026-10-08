//! Actual host-state directory effects and refusal behavior. This policy is
//! shared only by control tree components, not Cloud grant/storage FDs.
use marsh_daemon::host_state_directory::{HostStateDirectoryMode, verify_host_state_directory};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

#[test]
fn host_state_directory_checks_do_not_repair_unsafe_paths_or_follow_leaf_links() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("private");
    assert!(verify_host_state_directory(&missing, HostStateDirectoryMode::Private, false).is_err());
    assert!(!missing.exists());
    verify_host_state_directory(&missing, HostStateDirectoryMode::Private, true).unwrap();
    assert_eq!(
        fs::metadata(&missing).unwrap().permissions().mode() & 0o777,
        0o700
    );
    verify_host_state_directory(&missing, HostStateDirectoryMode::Private, false).unwrap();
    fs::set_permissions(&missing, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(verify_host_state_directory(&missing, HostStateDirectoryMode::Private, true).is_err());
    assert_eq!(
        fs::metadata(&missing).unwrap().permissions().mode() & 0o777,
        0o755
    );
    verify_host_state_directory(&missing, HostStateDirectoryMode::AccountAncestor, false).unwrap();
    fs::set_permissions(&missing, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(
        verify_host_state_directory(&missing, HostStateDirectoryMode::AccountAncestor, false)
            .is_err()
    );
    fs::set_permissions(&missing, fs::Permissions::from_mode(0o700)).unwrap();
    let link = root.path().join("link");
    symlink(&missing, &link).unwrap();
    assert!(verify_host_state_directory(&link, HostStateDirectoryMode::Private, true).is_err());
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let file = root.path().join("file");
    fs::write(&file, b"keep").unwrap();
    assert!(verify_host_state_directory(&file, HostStateDirectoryMode::Private, true).is_err());
    assert_eq!(fs::read(file).unwrap(), b"keep");
}
