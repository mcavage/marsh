//! Real native exec boundary, without Docker. Stock/Docker/Cloud are separate
//! required gates; these tests deliberately make no transport qualification claim.
use marsh_runtime::byte_exec::Payload;
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

fn payload_file(root: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
    let path = root.join("payload");
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    path
}

#[test]
fn native_exec_preserves_raw_values_stdin_stdout_and_status() {
    let root = tempfile::tempdir().unwrap();
    let raw = (1_u8..=255).collect::<Vec<_>>();
    let payload = Payload {
        argv: vec![b"/usr/bin/printenv".to_vec(), b"PROBE_VALUE".to_vec()],
        environment: BTreeMap::from([("PROBE_VALUE".into(), raw.clone())]),
        working_directory: Vec::new(),
    };
    let path = payload_file(root.path(), &payload.encode().unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
        .arg("--payload")
        .arg(&path)
        .env("STATIC_IMAGE_VALUE", "retained")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout, [raw, b"\n".to_vec()].concat());
    let payload = Payload {
        argv: vec![b"/bin/cat".to_vec()],
        environment: BTreeMap::new(),
        working_directory: Vec::new(),
    };
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&path, payload.encode().unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
    let input = b"\0\xff\xfe\nbyte stdin";
    let mut child = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
        .arg("--payload")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, input);
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_nul_reserved_and_oversized_payloads_never_execute() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("must-not-exist");
    let valid = Payload {
        argv: vec![
            b"/usr/bin/touch".to_vec(),
            marker.as_os_str().as_encoded_bytes().to_vec(),
        ],
        environment: BTreeMap::new(),
        working_directory: Vec::new(),
    }
    .encode()
    .unwrap();
    let mut variants = vec![
        Vec::new(),
        b"secret-malformed".to_vec(),
        valid[..valid.len() - 1].to_vec(),
    ];
    let mut trailing = valid.clone();
    trailing.push(0);
    variants.push(trailing);
    let mut nul = valid.clone();
    nul[16] = 0;
    variants.push(nul);
    let mut count = valid.clone();
    count[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
    variants.push(count);
    for bytes in variants {
        let file = root.path().join("payload");
        let _ = fs::remove_file(&file);
        payload_file(root.path(), &bytes);
        let output = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
            .arg("--payload")
            .arg(file)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125));
        assert_eq!(output.stderr, b"marsh: native byte launch failed\n");
        assert!(output.stdout.is_empty());
        assert!(!marker.exists());
    }
    let bad = Payload {
        argv: vec![b"/usr/bin/touch".to_vec()],
        environment: BTreeMap::from([("DOCKER_HOST".into(), b"secret".to_vec())]),
        working_directory: Vec::new(),
    };
    assert!(bad.encode().is_err());
}

#[test]
fn errno_status_and_payload_failure_are_distinct() {
    let root = tempfile::tempdir().unwrap();
    let denied = root.path().join("denied");
    fs::write(&denied, b"not executable").unwrap();
    for (command, cwd, expected) in [
        (b"/certainly-absent-marsh-command".to_vec(), Vec::new(), 127),
        (b"certainly-absent-marsh-command".to_vec(), Vec::new(), 127),
        (b"/dev/null/child".to_vec(), Vec::new(), 127),
        (
            denied.as_os_str().as_encoded_bytes().to_vec(),
            Vec::new(),
            126,
        ),
        (
            b"/bin/true".to_vec(),
            b"/certainly-absent-marsh-cwd".to_vec(),
            125,
        ),
    ] {
        let payload = Payload {
            argv: vec![command, b"secret-arg-\xff".to_vec()],
            environment: BTreeMap::new(),
            working_directory: cwd,
        };
        let path = root.path().join("payload");
        let _ = fs::remove_file(&path);
        payload_file(root.path(), &payload.encode().unwrap());
        let output = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
            .arg("--payload")
            .arg(path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(expected));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.windows(6).any(|w| w == b"secret"));
    }
}

#[test]
fn closed_stderr_does_not_replace_typed_launch_status_with_a_panic() {
    use std::os::{fd::OwnedFd, unix::net::UnixStream};
    let root = tempfile::tempdir().unwrap();
    let payload = Payload {
        argv: vec![b"/certainly-absent-marsh-command".to_vec()],
        environment: BTreeMap::new(),
        working_directory: Vec::new(),
    }
    .encode()
    .unwrap();
    for (bytes, status) in [(payload.as_slice(), 127), (b"invalid".as_slice(), 125)] {
        let path = root.path().join("payload");
        let _ = fs::remove_file(&path);
        payload_file(root.path(), bytes);
        let (reader, writer) = UnixStream::pair().unwrap();
        drop(reader);
        let fd: OwnedFd = writer.into();
        let output = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
            .arg("--payload")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::from(fd))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(status));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn fifo_symlink_hardlink_and_special_mode_payloads_reject_without_waiting() {
    use std::{
        os::unix::fs::symlink,
        time::{Duration, Instant},
    };
    let root = tempfile::tempdir().unwrap();
    let regular = payload_file(
        root.path(),
        &Payload {
            argv: vec![b"/bin/true".to_vec()],
            environment: BTreeMap::new(),
            working_directory: Vec::new(),
        }
        .encode()
        .unwrap(),
    );
    let fifo = root.path().join("fifo");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let link = root.path().join("symlink");
    symlink(&regular, &link).unwrap();
    let hard = root.path().join("hardlink");
    fs::hard_link(&regular, &hard).unwrap();
    let special = root.path().join("special");
    fs::copy(&regular, &special).unwrap();
    fs::set_permissions(&special, fs::Permissions::from_mode(0o4444)).unwrap();
    for path in [&fifo, &link, &hard, &special] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
            .arg("--payload")
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("payload open blocked");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(status.code(), Some(125));
    }
}

#[test]
fn shell_hex_transport_is_not_an_admission_interface() {
    let payload = Payload {
        argv: vec![
            b"--internal-record-session".to_vec(),
            b"/run/marsh/1000/session/shell".to_vec(),
            b"1000".to_vec(),
            b"1000".to_vec(),
        ],
        environment: BTreeMap::new(),
        working_directory: Vec::new(),
    };
    let hex: String = payload
        .encode()
        .unwrap()
        .iter()
        .flat_map(|byte| {
            let hex = b"0123456789abcdef";
            [
                char::from(hex[(byte >> 4) as usize]),
                char::from(hex[(byte & 15) as usize]),
            ]
        })
        .collect();
    assert!(marsh_runtime::byte_exec::decode_shell_launch(&[hex.into()]).is_err());
    marsh_runtime::byte_exec::validate_shell_payload(&payload).unwrap();
    let mut alias = payload;
    alias.argv[2] = b"0001000".to_vec();
    assert!(marsh_runtime::byte_exec::validate_shell_payload(&alias).is_err());
}

#[test]
fn enoexec_never_falls_back_to_a_shell() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("not-an-executable");
    let marker = root.path().join("would-be-shell-effect");
    fs::write(&executable, format!("touch {}\n", marker.display())).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let payload = Payload {
        argv: vec![executable.as_os_str().as_encoded_bytes().to_vec()],
        environment: BTreeMap::new(),
        working_directory: Vec::new(),
    };
    let file = payload_file(root.path(), &payload.encode().unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
        .arg("--payload")
        .arg(file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126));
    assert!(!marker.exists());
}

#[test]
fn native_exec_preserves_sigpipe_on_an_early_closed_output() {
    use std::os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::ExitStatusExt},
    };
    let root = tempfile::tempdir().unwrap();
    let payload = Payload {
        argv: vec![b"/usr/bin/yes".to_vec()],
        environment: BTreeMap::new(),
        working_directory: Vec::new(),
    };
    let file = payload_file(root.path(), &payload.encode().unwrap());
    let (reader, writer) = UnixStream::pair().unwrap();
    drop(reader);
    let fd: OwnedFd = writer.into();
    let output = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
        .arg("--payload")
        .arg(file)
        .stdout(Stdio::from(fd))
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(
        output.status.signal(),
        Some(13),
        "native command must receive SIGPIPE, not inherited Rust SIG_IGN: {:?}",
        output.stderr
    );
}

#[test]
fn injected_helper_cannot_become_the_image_command_through_a_symlink() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("must-not-execute");
    let nested = root.path().join("nested");
    let nested_payload = Payload {
        argv: vec![
            b"/usr/bin/touch".to_vec(),
            marker.as_os_str().as_encoded_bytes().to_vec(),
        ],
        environment: BTreeMap::new(),
        working_directory: Vec::new(),
    };
    fs::write(&nested, nested_payload.encode().unwrap()).unwrap();
    fs::set_permissions(&nested, fs::Permissions::from_mode(0o444)).unwrap();
    let alias = root.path().join("alias");
    symlink(env!("CARGO_BIN_EXE_marsh-byte-exec"), &alias).unwrap();
    let payload = Payload {
        argv: vec![
            alias.as_os_str().as_encoded_bytes().to_vec(),
            b"--payload".to_vec(),
            nested.as_os_str().as_encoded_bytes().to_vec(),
        ],
        environment: BTreeMap::new(),
        working_directory: Vec::new(),
    };
    let file = payload_file(root.path(), &payload.encode().unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_marsh-byte-exec"))
        .arg("--payload")
        .arg(file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(126));
    assert!(!marker.exists());
}

#[test]
fn binary_envelope_roundtrips_bytes_and_rejects_trailing_data() {
    let raw = (1_u8..=255).cycle().take(16000).collect::<Vec<_>>();
    let payload = Payload {
        argv: vec![
            b"--internal-record-session".to_vec(),
            raw.clone(),
            raw.clone(),
        ],
        environment: BTreeMap::new(),
        working_directory: b"/workspace/\xff\xfe".to_vec(),
    };
    let bytes = payload.encode().unwrap();
    let decoded = Payload::decode(&bytes).unwrap();
    assert_eq!(decoded.argv, payload.argv);
    assert_eq!(decoded.working_directory, payload.working_directory);
    assert!(Payload::decode(b"xx").is_err());
    let mut trailing = bytes;
    trailing.push(0);
    assert!(Payload::decode(&trailing).is_err());
}
