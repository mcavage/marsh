#![cfg(feature = "test-support")]
//! Actual adapter/CLI boundary for canonical shell-only identity admission.
use marsh_contracts::OciImage;
use marsh_runtime::SystemCommandRunner;
use marsh_sbx::{ShellUser, ShellVmSpec, StockSbx};
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc};

#[test]
fn untagged_local_aliases_and_bare_ids_do_not_reach_stock() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("effect");
    let sdk = root.path().join("sdk.py");
    fs::write(&sdk,format!("#!/usr/bin/python3\nimport pathlib\npathlib.Path({marker:?}).write_text('unexpected SDK effect')\n")).unwrap();
    fs::set_permissions(&sdk, fs::Permissions::from_mode(0o700)).unwrap();
    let adapter = StockSbx::new(sdk, Arc::new(SystemCommandRunner::new(root.path())));
    let digest = "a".repeat(64);
    for repo in [
        "marsh-shell-local",
        "library/marsh-shell-local",
        "docker.io/marsh-shell-local",
        "index.docker.io/library/marsh-shell-local",
        "index.docker.io/marsh-dev-shell",
    ] {
        let spec = ShellVmSpec {
            name: "owned".into(),
            image: OciImage::parse(format!("{repo}@sha256:{digest}")).unwrap(),
            shell_binary: root.path().join("unused"),
            user: user(),
        };
        // Stock SBX resolves local templates only by their import tag; the
        // digest alone is refused before any SDK effect.
        let error = adapter.ensure_shell_vm(&spec).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("local shell templates require repository:tag@sha256"),
            "{repo}: {error}"
        );
        assert!(!marker.exists());
    }
    let spec = ShellVmSpec {
        name: "owned".into(),
        image: OciImage::parse(format!("sha256:{digest}")).unwrap(),
        shell_binary: root.path().join("unused"),
        user: user(),
    };
    assert!(
        adapter
            .ensure_shell_vm(&spec)
            .unwrap_err()
            .to_string()
            .contains("named repository")
    );
    assert!(!marker.exists());
}

fn user() -> ShellUser {
    ShellUser {
        name: "fixture".into(),
        uid: 1000,
        gid: 1000,
        home: "/home/fixture".into(),
    }
}

#[test]
fn ordinary_named_published_template_is_canonicalized_without_local_fallback() {
    let root = tempfile::tempdir().unwrap();
    let sdk = root.path().join("sdk.py");
    fs::write(
        &sdk,
        r"#!/usr/bin/python3
import json,pathlib,sys
args=sys.argv[1:]
if args[0]=='ls':print(json.dumps({'sandboxes':[]}));sys.exit(0)
if args[0]=='inspect':sys.stderr.write('sandbox not found');sys.exit(1)
assert args[0]=='create'
pathlib.Path(__file__).with_name('created').write_text(json.dumps(args));sys.exit(23)
",
    )
    .unwrap();
    fs::set_permissions(&sdk, fs::Permissions::from_mode(0o700)).unwrap();
    let binary = root.path().join("shell");
    fs::write(&binary, b"owned fixture").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let adapter = StockSbx::new(sdk, Arc::new(SystemCommandRunner::new(root.path())));
    let spec = ShellVmSpec {
        name: "owned".into(),
        image: OciImage::parse(format!("alpine:pinned@sha256:{}", "b".repeat(64))).unwrap(),
        shell_binary: binary,
        user: user(),
    };
    let error = adapter.ensure_shell_vm(&spec).unwrap_err();
    let created = fs::read(root.path().join("created"))
        .unwrap_or_else(|read| panic!("stock create not reached ({read}): {error}"));
    let args: Vec<String> = serde_json::from_slice(&created).unwrap();
    assert_eq!(
        args[9],
        format!("docker.io/library/alpine@sha256:{}", "b".repeat(64))
    );
    assert_eq!(args[5], "missing");
}
