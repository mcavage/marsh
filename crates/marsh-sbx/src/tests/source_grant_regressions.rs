//! Source-fencing regressions through the shell mount caller. Stock SBX is
//! the model; a rejected source must cause no stock call at all.
use super::*;

fn fixture() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("source-regression-")
        .tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap())
        .unwrap()
}

/// An adapter whose owned shell VM exists, so only the source can refuse.
fn owned_shell(model: &Arc<StockModel>) -> StockSbx {
    let adapter = model.adapter();
    model.seed_owned(&adapter, "owned-shell");
    adapter
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
    let model = StockModel::new();
    let outcome = owned_shell(&model).prepare_shell_mounts(&shell_vm("owned-shell"), &[admitted]);
    assert!(
        matches!(outcome, Err(SbxError::SourceChanged(_))) && model.mutations() == 0,
        "parent swap admitted: outcome={outcome:?}, effects={:?}",
        model.calls()
    );
}

#[test]
fn rejects_entire_overlapping_batch_before_first_mount() {
    let root = fixture();
    let project = root.path().join("project");
    let home = project.join("home");
    fs::create_dir_all(&home).unwrap();
    let model = StockModel::new();
    let outcome = owned_shell(&model)
        .prepare_shell_mounts(&shell_vm("owned-shell"), &shell_grants(&project, &home));
    assert!(
        matches!(outcome, Err(SbxError::HostGrantFence(_))) && model.mutations() == 0,
        "overlapping project/home admitted: outcome={outcome:?}, effects={:?}",
        model.calls()
    );
}
