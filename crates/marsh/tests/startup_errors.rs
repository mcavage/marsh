//! Public CLI startup failures read as plain sentences, before any daemon,
//! VM, or SBX work. No SBX backend is involved.

use std::{fs, os::unix::fs::PermissionsExt as _, process::Command};

#[test]
fn a_bad_marsh_sbx_names_the_path_and_the_fix() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (home, control, project) = (
        root.join("home"),
        root.join("control"),
        root.join("project"),
    );
    for directory in [&home, &control, &project] {
        fs::create_dir(directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_marsh"))
        .args(["-c", "true"])
        .current_dir(&project)
        .env("MARSH_HOME", &home)
        .env("MARSH_CONTROL_HOME", &control)
        .env("MARSH_SBX", "/nonexistent")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.starts_with(
            "marsh: stock sbx not found at /nonexistent (MARSH_SBX); fix the path, or unset MARSH_SBX. \
             marsh needs Docker Sandboxes: brew install docker/tap/sbx; then `sbx login`"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("os error"), "{stderr}");
    assert!(!stderr.contains("Error {"), "{stderr}");
    // Nothing was written to the isolated homes.
    assert_eq!(fs::read_dir(&control).unwrap().count(), 0);
}

#[test]
fn a_missing_sbx_prints_the_exact_install_command() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (home, control, project, empty) = (
        root.join("home"),
        root.join("control"),
        root.join("project"),
        root.join("empty-path"),
    );
    for directory in [&home, &control, &project, &empty] {
        fs::create_dir(directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_marsh"))
        .args(["-c", "true"])
        .current_dir(&project)
        .env("MARSH_HOME", &home)
        .env("MARSH_CONTROL_HOME", &control)
        .env_remove("MARSH_SBX")
        .env("PATH", &empty)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    // Same text as the Homebrew formula's caveats and docs/install.md.
    assert!(
        stderr.contains(
            "marsh needs Docker Sandboxes: brew install docker/tap/sbx; then `sbx login`"
        ),
        "{stderr}"
    );
    assert_eq!(fs::read_dir(&control).unwrap().count(), 0);
}
