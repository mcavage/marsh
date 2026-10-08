//! Identical caller regressions run against retained pre-repair grant code and
//! the repaired adapter. The stock runner records effects but never calls SBX.
use super::*;

fn fixture() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("source-regression-")
        .tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap())
        .unwrap()
}

fn shell() -> ReadyShellVm {
    ReadyShellVm {
        name: "owned-shell".into(),
        user: shell_spec("/unused".into()).user,
        cold_started: false,
    }
}

#[test]
fn rejects_a_symlink_in_source_ancestry() {
    let root = fixture();
    fs::create_dir_all(root.path().join("real/project")).unwrap();
    std::os::unix::fs::symlink(root.path().join("real"), root.path().join("link")).unwrap();
    assert!(
        AdmittedHostGrant::open(
            root.path().join("link/project"),
            "/project".into(),
            MountAccess::ReadWrite
        )
        .is_err(),
        "a no-follow leaf is insufficient when an ancestor is a symlink"
    );
}

#[test]
fn rejects_changed_parent_even_if_leaf_inode_is_preserved() {
    let root = fixture();
    let parent = root.path().join("parent");
    fs::create_dir_all(parent.join("project")).unwrap();
    let source = parent.join("project");
    let admitted = grant(source.clone(), "/project", MountAccess::ReadWrite);
    fs::rename(&parent, root.path().join("moved")).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::rename(root.path().join("moved/project"), &source).unwrap();
    let leaf = fs::metadata(&source).unwrap();
    assert_eq!(admitted.source_identity(), (leaf.dev(), leaf.ino()));
    let runner = FakeRunner::with_outputs([ok()]);
    let adapter = StockSbx::new("/never-call-real-stock", runner.clone());
    let outcome = adapter.prepare_shell_mounts(&shell(), &[admitted]);
    assert!(
        outcome.is_err() && runner.arguments().is_empty(),
        "parent swap admitted: outcome={outcome:?}, effects={:?}",
        runner.arguments()
    );
}

#[test]
fn rejects_entire_overlapping_batch_before_first_mount() {
    let root = fixture();
    let project = root.path().join("project");
    let home = project.join("home");
    fs::create_dir_all(&home).unwrap();
    let runner = FakeRunner::with_outputs([ok(), ok()]);
    let adapter = StockSbx::new("/never-call-real-stock", runner.clone());
    let outcome = adapter.prepare_shell_mounts(&shell(), &shell_grants(&project, &home));
    assert!(
        outcome.is_err() && runner.arguments().is_empty(),
        "overlapping project/home admitted: outcome={outcome:?}, effects={:?}",
        runner.arguments()
    );
}
