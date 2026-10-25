//! `marsh init`: create the guest home and copy the git identity into it,
//! before any daemon, VM, or SBX work.

mod common;

use std::{fs, os::unix::fs::PermissionsExt as _, path::Path, process::Output};

fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn init(host_home: &Path, scope: &Path, extra: &[&str]) -> Output {
    common::command(env!("CARGO_BIN_EXE_marsh"))
        .arg("init")
        .args(extra)
        .current_dir(host_home)
        .env("HOME", host_home)
        .env("MARSH_HOME", scope)
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn init_creates_the_home_and_copies_only_the_name_and_email() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (host_home, scope) = (root.join("mac-home"), root.join("scope"));
    private_dir(&host_home);
    private_dir(&scope);
    let config = scope.join("home/.gitconfig");

    // No identity on the Mac: the home is made, nothing is seeded, and it says what to do.
    let first = init(&host_home, &scope, &[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(text(&first).contains("(created)"), "{}", text(&first));
    assert!(text(&first).contains("none found"), "{}", text(&first));
    assert!(!config.exists());

    // The Mac has an identity, plus settings that must not travel.
    fs::write(
        host_home.join(".gitconfig"),
        "[user]\n\tname = Ada Lovelace\n\temail = ada@example.com\n\tsigningkey = SECRETKEY\n\
         [credential]\n\thelper = store\n[commit]\n\tgpgsign = true\n",
    )
    .unwrap();
    let second = init(&host_home, &scope, &[]);
    assert!(second.status.success());
    assert!(
        text(&second).contains("(already there)"),
        "{}",
        text(&second)
    );
    assert!(
        text(&second).contains("copied your name and email"),
        "{}",
        text(&second)
    );
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "[user]\n\tname = \"Ada Lovelace\"\n\temail = \"ada@example.com\"\n"
    );

    // An existing file is never changed, even if the Mac's identity differs.
    fs::write(&config, "[user]\n\tname = Edited in marsh\n").unwrap();
    let third = init(&host_home, &scope, &[]);
    assert!(
        text(&third).contains("already exists; left as it is"),
        "{}",
        text(&third)
    );
    assert_eq!(
        fs::read_to_string(&config).unwrap(),
        "[user]\n\tname = Edited in marsh\n"
    );
}

#[test]
fn init_has_help_and_refuses_arguments() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let (host_home, scope) = (root.join("mac-home"), root.join("scope"));
    private_dir(&host_home);
    private_dir(&scope);
    let help = init(&host_home, &scope, &["--help"]);
    assert!(help.status.success());
    assert!(
        text(&help).starts_with("Usage: marsh init"),
        "{}",
        text(&help)
    );
    assert!(
        !scope.join("home").exists(),
        "--help must not create anything"
    );
    let extra = init(&host_home, &scope, &["now"]);
    assert_eq!(extra.status.code(), Some(2));
    assert!(!scope.join("home").exists());
}
