//! Production (non-test-support) Linux guests retain live FD identity without
//! claiming a durable host filesystem incarnation or opening a host ledger.
#![cfg(not(target_os = "macos"))]
use marsh_contracts::MountAccess;
use marsh_sbx::AdmittedHostGrant;
use std::fs;

#[test]
fn guest_live_descriptors_pin_the_old_object_across_path_replacement() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    let grant =
        AdmittedHostGrant::open(source.clone(), "/project".into(), MountAccess::ReadWrite).unwrap();
    let old = grant.source_identity();
    fs::rename(&source, root.join("original")).unwrap();
    fs::create_dir(&source).unwrap();
    let replacement =
        AdmittedHostGrant::open(source, "/project".into(), MountAccess::ReadWrite).unwrap();
    assert_eq!(grant.source_identity(), old);
    assert_ne!(
        replacement.source_identity(),
        old,
        "a live retained FD did not pin the original inode"
    );
}
