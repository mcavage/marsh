//! Public CLI startup failures read as plain sentences, before any daemon,
//! VM, or SBX work. No SBX backend is involved.

mod common;

use std::{fs, os::unix::fs::PermissionsExt as _};

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
    let output = common::command(env!("CARGO_BIN_EXE_marsh"))
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
    let output = common::command(env!("CARGO_BIN_EXE_marsh"))
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

/// A bare `MARSH_SBX` (and the default lookup) is a program on the absolute
/// entries of PATH, never a file a job planted in the project marsh runs in,
/// even when PATH holds `.` or an empty entry.
#[test]
fn a_bare_marsh_sbx_never_selects_a_program_in_the_working_directory() {
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
    let marker = root.join("planted-ran");
    let planted = project.join("sbx");
    fs::write(
        &planted,
        format!("#!/bin/sh\n: > '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&planted, fs::Permissions::from_mode(0o755)).unwrap();
    let search = format!(".::{}", empty.display());
    for configured in [Some("sbx"), None] {
        let mut command = common::command(env!("CARGO_BIN_EXE_marsh"));
        command
            .args(["-c", "true"])
            .current_dir(&project)
            .env("MARSH_HOME", &home)
            .env("MARSH_CONTROL_HOME", &control)
            .env("PATH", &search);
        match configured {
            Some(value) => command.env("MARSH_SBX", value),
            None => command.env_remove("MARSH_SBX"),
        };
        let output = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{configured:?}: {stderr}");
        let expected = if configured.is_some() {
            "marsh: stock sbx sbx (MARSH_SBX) was not found on the absolute entries of PATH"
        } else {
            "marsh: stock sbx not found on PATH."
        };
        assert!(stderr.starts_with(expected), "{configured:?}: {stderr}");
        assert!(!marker.exists(), "{configured:?}: the planted sbx ran");
        assert_eq!(fs::read_dir(&control).unwrap().count(), 0);
    }
}
