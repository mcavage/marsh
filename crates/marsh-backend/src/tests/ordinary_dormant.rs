//! H3: authenticated public `OpenShell` -> real `StockDaemonBackend` -> `StockSbx`
//! mount/revoke with one still-live warm VM, real SDK fixture processes. Relay
//! absence is a deliberate pre-effect startup failure AFTER accepted mount work;
//! this exercises the actual backend rollback, not a simplified test backend.
use super::*;

fn descriptors() -> usize {
    fs::read_dir(if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    })
    .unwrap()
    .count()
}

fn open_and_rollback(fixture: &RecoveryFixture, selected: &std::path::Path, index: usize) {
    let project = fixture.root.path().join(format!("project-{index}"));
    fs::create_dir(&project).unwrap();
    let authority = SessionAuthority {
        username: "alice".into(),
        uid: 501,
        gid: 20,
        launch_directory: project,
        guest_home: "/Users/alice".into(),
        home_backing: selected.to_owned(),
        ephemeral_home: false,
    };
    let PublicReply::ShellAttached { session_id } = fixture.request(PublicRequest::AttachShell {
        pid: std::process::id(),
        session: authority.clone(),
    }) else {
        panic!("attach generation {index}")
    };
    let request = marsh_daemon::ShellSpec {
        dev: false,
        session: SessionSpec {
            session_id: session_id.clone(),
            username: authority.username,
            uid: 501,
            gid: 20,
            launch_directory: authority.launch_directory,
            guest_home: authority.guest_home,
            home_backing: authority.home_backing,
            ephemeral_home: false,
            terminal: false,
            terminal_size: None,
        },
        arguments: Vec::new(),
    };
    let attached = fixture.client.start_shell(request).unwrap();
    let mut diagnostic = Vec::new();
    loop {
        match attached.receive().unwrap() {
            marsh_daemon::AttachmentFrame::Stderr { bytes } => diagnostic.extend(bytes),
            marsh_daemon::AttachmentFrame::Exited { code } => {
                assert_eq!(code, 125, "{index}");
                break;
            }
            marsh_daemon::AttachmentFrame::Failed { message } => {
                diagnostic.extend(message.as_bytes());
                break;
            }
            other => panic!("unexpected startup frame {other:?} at {index}"),
        }
    }
    let message = String::from_utf8(diagnostic).unwrap();
    assert!(
        message.contains("relay"),
        "generation {index} failed before its intended rollback: {message}"
    );
    drop(attached);
    let end = Instant::now() + Duration::from_secs(3);
    while fixture.server.store().shell_cleanup_pending() && Instant::now() < end {
        thread::sleep(Duration::from_millis(2));
    }
    assert!(!fixture.server.store().shell_cleanup_pending());
    assert!(matches!(
        fixture.request(PublicRequest::DetachShell { session_id }),
        PublicReply::Detached
    ));
}

#[test]
#[ignore = "owned isolated RLIMIT256 process; 300 public daemon generations"]
fn warm_daemon_three_hundred_dormant_projects_bound_descriptors_and_keep_authority() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let previous = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(256),
            maximum: previous.maximum,
        },
    )
    .unwrap();
    let fixture = RecoveryFixture::new();
    // This owned CLI copy implements just the already-warm shell installation
    // and keepalive carrier. All ls/inspect/mount/umount effects still go through
    // the fixture's real socket server and are checked independently below.
    let script = fixture.root.path().join("stock.py");
    let before = fs::read_to_string(&script).unwrap();
    let needle =
        "        result = call(root / 'control.sock', {'args': sys.argv[1:], 'cwd': os.getcwd()})";
    assert!(before.contains(needle));
    let replacement = "        args = sys.argv[1:]\n        if '--internal-keepalive' in args:\n            print('MARSH-KEEPALIVE/1',flush=True); sys.stdin.buffer.read(); sys.exit(0)\n        if args[0] == 'cp': sys.exit(0)\n        if args[0] == 'exec' and 'id' in args:\n            print('501' if args[-2] == '-u' else '20'); sys.exit(0)\n        if args[0] == 'exec' and ('install' in args or 'sh' in args): sys.exit(0)\n        result = call(root / 'control.sock', {'args': args, 'cwd': os.getcwd()})";
    fs::write(&script, before.replace(needle, replacement)).unwrap();
    fs::write(
        &fixture.spec.shell_binary,
        b"owned fake guest executable; no execution",
    )
    .unwrap();
    fs::set_permissions(
        &fixture.spec.shell_binary,
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let selected = fixture.root.path().join("selected");
    fs::create_dir(&selected).unwrap();
    let mut warm_fds = 0;
    for index in 0..300 {
        open_and_rollback(&fixture, &selected, index);
        if index == 0 {
            warm_fds = descriptors();
        }
        if index % 25 == 0 {
            assert!(
                descriptors() <= warm_fds + 12,
                "FD history at {index}: base{warm_fds} now{}",
                descriptors()
            );
        }
    }
    let now = descriptors();
    assert!(now <= warm_fds + 12, "dormant FD growth {warm_fds}->{now}");
    let snapshot = fixture.control(serde_json::json!({"test":"snapshot"}));
    let requests = snapshot["requests"].as_array().unwrap();
    assert_eq!(requests.iter().filter(|r| r[0] == "mount").count(), 600);
    assert_eq!(requests.iter().filter(|r| r[0] == "umount").count(), 600);
    assert!(snapshot["mounts"].as_array().unwrap().is_empty());
    assert!(
        !requests
            .iter()
            .any(|r| r[0] == "rm" || r[0] == "stop" || r[0] == "create")
    );
    assert!(fixture.canary(&fixture.spec.name));
    assert!(fixture.canary("unrelated-canary"));
    let ledger: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.root.path().join("registry/sources.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(ledger["sources"].as_array().unwrap().len(), 301);
    assert_eq!(ledger["owners"].as_array().unwrap().len(), 301);
    println!(
        "public daemon warm300 RLIMIT256 fds {warm_fds}->{now}, ledger sources301 owners301, mount/umount600/600; both VM members remain alive"
    );
    fixture.adapter.reset_shell_vm(&fixture.spec).unwrap();
    drop(fixture);
    setrlimit(Resource::Nofile, previous).unwrap();
}
