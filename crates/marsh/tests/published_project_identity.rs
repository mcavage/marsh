//! Real host-launcher checks for replacement of a published pipeline project.
use std::{
    fs,
    io::{BufRead as _, BufReader, Write as _},
    os::unix::fs::MetadataExt as _,
    process::{Command, Stdio},
};

#[test]
fn published_project_replaced_after_child_chdir_is_rejected_before_daemon_start() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let project = root.join("project");
    let host_home = root.join("host-home");
    let scope = root.join("scope");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&host_home).unwrap();
    let expected = fs::metadata(&project).unwrap();
    // This trusted launcher deliberately pauses after the exporter's chdir,
    // making the otherwise narrow rename window reproducible without a hook
    // in the production launcher or any VM/external-service effects.
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            "printf 'ready\\n'; read -r resume; exec \"$1\" -c 'printf should-not-run'",
            "launcher",
            env!("CARGO_BIN_EXE_marsh"),
        ])
        .current_dir(&project)
        .env_clear()
        .env("USER", "fixture")
        .env("HOME", &host_home)
        .env("MARSH_HOME", &scope)
        .env("MARSH_MCP_EXPECTED_PROJECT_PATH", &project)
        .env("MARSH_MCP_EXPECTED_PROJECT_DEV", expected.dev().to_string())
        .env("MARSH_MCP_EXPECTED_PROJECT_INO", expected.ino().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    output.read_line(&mut ready).unwrap();
    assert_eq!(ready, "ready\n");
    fs::rename(&project, root.join("original-project")).unwrap();
    fs::create_dir(&project).unwrap();
    child.stdin.take().unwrap().write_all(b"resume\n").unwrap();
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("attached project identity changed"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !scope.exists(),
        "project rejection must precede daemon startup"
    );
    let mut remaining = String::new();
    output.read_line(&mut remaining).unwrap();
    assert!(remaining.is_empty(), "pipeline must never execute");
}

#[test]
fn published_project_missing_identity_is_rejected_before_daemon_start() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_marsh"))
        .args(["-c", "printf should-not-run"])
        .current_dir(&root)
        .env_clear()
        .env("USER", "fixture")
        .env("HOME", &root)
        .env("MARSH_HOME", root.join("scope"))
        .env("MARSH_MCP_EXPECTED_PROJECT_PATH", &root)
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("incomplete attached project identity")
    );
    assert!(!root.join("scope").exists());
    assert!(result.stdout.is_empty());
}
