mod execution_cwd;
mod shell_admission;

use super::*;
use std::{
    io::{Cursor, Read, Write},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

#[test]
fn credited_client_preserves_terminal_queued_before_construction_or_failed_input_write() {
    for code in [0, 1, 42] {
        let (client, mut peer) = UnixStream::pair().unwrap();
        write_frame(
            &mut peer,
            &AttachmentFrame::Stdout {
                bytes: b"tail\0\xff".to_vec(),
            },
        )
        .unwrap();
        write_frame(&mut peer, &AttachmentFrame::Exited { code }).unwrap();
        peer.shutdown(Shutdown::Both).unwrap();
        drop(peer);
        let client = ClientExecution::new_shell(client).unwrap();
        assert!(matches!(
            client.send(&AttachmentFrame::StdinEof),
            Err(DaemonError::ShellInputWriteFailed(_))
        ));
        assert_eq!(
            client.receive().unwrap(),
            AttachmentFrame::Stdout {
                bytes: b"tail\0\xff".to_vec()
            }
        );
        assert_eq!(client.receive().unwrap(), AttachmentFrame::Exited { code });
    }
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(&100_u32.to_be_bytes()).unwrap();
    peer.write_all(b"{\"type\":\"exited\"").unwrap();
    drop(peer);
    let client = ClientExecution::new_shell(client).unwrap();
    let _ = client.send(&AttachmentFrame::StdinEof);
    assert!(
        client.receive().is_err(),
        "a missing actual terminal must never be manufactured"
    );
}

#[test]
fn attachment_frame_write_preserves_partial_frames_across_slow_connected_reads() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let attachment = ServerAttachment::new(server).unwrap();
    let reader = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let mut wire = Vec::new();
        let mut bytes = [0_u8; 4096];
        loop {
            match client.read(&mut bytes) {
                Ok(0) => break,
                Err(error) => panic!("{error}"),
                Ok(count) => {
                    wire.extend_from_slice(&bytes[..count]);
                    thread::sleep(Duration::from_millis(2));
                }
            }
        }
        read_frame::<AttachmentFrame>(&mut &wire[..]).unwrap()
    });
    let frame = AttachmentFrame::Failed {
        message: "x".repeat(500 * 1024),
    };
    attachment.send(&frame).unwrap();
    drop(attachment);
    assert_eq!(reader.join().unwrap(), frame);
}

#[test]
fn credit_client_connected_input_pause_does_not_tear_frames() {
    let (client, mut peer) = UnixStream::pair().unwrap();
    let execution = ClientExecution::new_shell(client).unwrap();
    let sender = thread::spawn(move || {
        execution.send(&AttachmentFrame::Stdin {
            bytes: vec![23; 1_000_000],
        })
    });
    thread::sleep(Duration::from_secs(7));
    let mut received = Vec::new();
    let result = loop {
        match read_frame::<AttachmentFrame>(&mut peer) {
            Ok(AttachmentFrame::Stdin { bytes }) => {
                received.extend(bytes);
                if received.len() == 1_000_000 {
                    break Ok(());
                }
            }
            Ok(frame) => panic!("unexpected frame: {frame:?}"),
            Err(error) => break Err(error),
        }
    };
    let sent = sender.join().unwrap();
    assert!(
        sent.is_ok() && result.is_ok(),
        "send={sent:?}; receive={result:?}; bytes={}",
        received.len()
    );
    assert_eq!(received, vec![23; 1_000_000]);
}

#[test]
fn server_attachment_close_input_wakes_receiver_without_losing_final_status() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let attachment = ServerAttachment::new(server).unwrap();
    let receiver = attachment.clone();
    let (send, receive) = mpsc::channel();
    let task = thread::spawn(move || send.send(receiver.receive()).unwrap());
    attachment.close_input();
    assert!(
        receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .is_err()
    );
    task.join().unwrap();
    attachment
        .send(&AttachmentFrame::Exited { code: 42 })
        .unwrap();
    assert_eq!(
        read_frame::<AttachmentFrame>(&mut client).unwrap(),
        AttachmentFrame::Exited { code: 42 }
    );
}

#[test]
fn server_attachment_polling_preserves_partial_frames_across_idle_intervals() {
    let (server, mut client) = UnixStream::pair().unwrap();
    let attachment = ServerAttachment::new(server).unwrap();
    let expected = AttachmentFrame::Signal {
        signal: "TERM".into(),
    };
    let mut wire = Vec::new();
    write_frame(&mut wire, &expected).unwrap();
    let sender = thread::spawn(move || {
        for chunk in wire.chunks(3) {
            client.write_all(chunk).unwrap();
            thread::sleep(Duration::from_millis(30));
        }
    });
    assert_eq!(attachment.receive().unwrap(), expected);
    sender.join().unwrap();
}

#[test]
fn cleanup_uncertain_shell_cannot_be_detached_or_release_session_authority() {
    let home = tempfile::tempdir().unwrap();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let token = store.issue_relay_token(&session).unwrap();
    store.mark_shell_cleanup_uncertain(&session);
    assert!(!store.shell_is_attached(&session));
    assert!(matches!(
        store.detach_shell(&session),
        Err(DaemonError::ShellCleanupUncertain(_))
    ));
    assert!(!store.safe_to_shutdown());
    let state = store.lock();
    assert_eq!(state.shells[&session].state, ShellState::CleanupUncertain);
    assert!(state.session_authorities.contains_key(&session));
    assert!(!state.authentication_tokens.contains_key(&token));
}

#[test]
fn client_attachment_write_has_deadline_when_peer_stops_reading() {
    let (client, _peer) = UnixStream::pair().unwrap();
    let execution =
        ClientExecution::new_with_write_timeout(client, Duration::from_millis(25)).unwrap();
    let started = Instant::now();
    let error = execution
        .send(&AttachmentFrame::Failed {
            message: "x".repeat(900 * 1024),
        })
        .unwrap_err();
    assert!(matches!(error, DaemonError::Io(ref error)
        if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn client_attachment_shutdown_does_not_wait_for_writer_lock() {
    let (client, _peer) = UnixStream::pair().unwrap();
    let execution = ClientExecution::new(client).unwrap();
    let writer = execution.writer.lock().unwrap();
    let (sent, received) = mpsc::channel();
    let shutdown = execution.clone();
    let caller = thread::spawn(move || sent.send(shutdown.shutdown()).unwrap());
    let result = received.recv_timeout(Duration::from_millis(500));
    drop(writer);
    caller.join().unwrap();
    assert!(result.unwrap().is_ok());
}

#[test]
fn shell_stdin_read_failure_reports_error_without_hanging() {
    struct InterruptedThenFail(bool);
    impl Read for InterruptedThenFail {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            if std::mem::replace(&mut self.0, false) {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Err(io::Error::other("synthetic stdin fault"))
            }
        }
    }

    let (_server, client) = UnixStream::pair().unwrap();
    let execution = ClientExecution::new_shell(client).unwrap();
    let (sent, received) = mpsc::channel();
    thread::spawn(move || {
        let mut stderr = Vec::new();
        let result = relay_attachment(
            &execution,
            false,
            InterruptedThenFail(true),
            Vec::new(),
            &mut stderr,
        );
        sent.send((result, stderr)).unwrap();
    });
    let (result, stderr) = received
        .recv_timeout(Duration::from_secs(2))
        .expect("stdin read failure must end the attachment");
    let message = String::from_utf8(stderr).unwrap();
    assert!(message.contains("synthetic stdin fault"), "{message}");
    assert!(message.contains("forwarding 0 bytes"), "{message}");
    assert!(matches!(result, Err(DaemonError::InvalidState(_))));
}

struct SaturatingInput {
    remaining: usize,
    dropped: mpsc::Sender<()>,
}

impl Read for SaturatingInput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        self.remaining -= 1;
        buffer.fill(b'x');
        Ok(buffer.len())
    }
}

impl Drop for SaturatingInput {
    fn drop(&mut self) {
        let _ = self.dropped.send(());
    }
}

#[test]
fn shell_exit_after_blocked_stdin_keeps_its_exit_status() {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    server_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let server = ServerAttachment::new(server_stream).unwrap();
    let execution = ClientExecution::new_shell(client_stream).unwrap();
    let (dropped_send, dropped_receive) = mpsc::channel();
    let (result_send, result_receive) = mpsc::channel();
    thread::spawn(move || {
        let mut stderr = Vec::new();
        let result = relay_attachment(
            &execution,
            false,
            SaturatingInput {
                remaining: SHELL_STDIN_WINDOW + 1,
                dropped: dropped_send,
            },
            Vec::new(),
            &mut stderr,
        );
        result_send.send((result, stderr)).unwrap();
    });
    for _ in 0..SHELL_STDIN_WINDOW {
        assert!(matches!(
            server.receive().unwrap(),
            AttachmentFrame::Stdin { bytes } if bytes == vec![b'x'; SHELL_STDIN_CHUNK]
        ));
    }
    server.send(&AttachmentFrame::StdinClosed).unwrap();
    dropped_receive
        .recv_timeout(Duration::from_secs(2))
        .expect("the blocked input pump must observe stdin closure");
    server.send(&AttachmentFrame::Exited { code: 130 }).unwrap();
    let (result, stderr) = result_receive.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(result.unwrap(), 130);
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
}

#[test]
fn lost_transport_after_remote_stdin_close_reports_transport_cause() {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    server_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let server = ServerAttachment::new(server_stream).unwrap();
    let execution = ClientExecution::new_shell(client_stream).unwrap();
    let (dropped_send, dropped_receive) = mpsc::channel();
    let (result_send, result_receive) = mpsc::channel();
    thread::spawn(move || {
        let mut stderr = Vec::new();
        let result = relay_attachment(
            &execution,
            false,
            SaturatingInput {
                remaining: SHELL_STDIN_WINDOW + 1,
                dropped: dropped_send,
            },
            Vec::new(),
            &mut stderr,
        );
        result_send.send((result, stderr)).unwrap();
    });
    for _ in 0..SHELL_STDIN_WINDOW {
        assert!(matches!(
            server.receive().unwrap(),
            AttachmentFrame::Stdin { .. }
        ));
    }
    server.send(&AttachmentFrame::StdinClosed).unwrap();
    dropped_receive
        .recv_timeout(Duration::from_secs(2))
        .expect("the input pump must observe remote stdin closure");
    drop(server);
    let (result, stderr) = result_receive.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(result, Err(DaemonError::Io(_))));
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
}

#[test]
fn local_signal_write_failure_wakes_blocked_stdin_and_reports_cause() {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    server_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let server = ServerAttachment::new(server_stream).unwrap();
    let mut execution =
        ClientExecution::new_with_write_timeout(client_stream, Duration::from_millis(25)).unwrap();
    execution.stdin_window = Some(Arc::new(StdinWindow::new()));
    let signal_client = execution.clone();
    let (result_send, result_receive) = mpsc::channel();
    thread::spawn(move || {
        let mut stderr = Vec::new();
        let result = relay_attachment(
            &execution,
            false,
            Cursor::new(vec![b'x'; SHELL_STDIN_CHUNK * (SHELL_STDIN_WINDOW + 1)]),
            Vec::new(),
            &mut stderr,
        );
        result_send.send((result, stderr)).unwrap();
    });
    for _ in 0..SHELL_STDIN_WINDOW {
        assert!(matches!(
            server.receive().unwrap(),
            AttachmentFrame::Stdin { .. }
        ));
    }
    // A real disconnect, not a healthy connected consumer's pause. Close the
    // peer descriptor: macOS still accepts writes after a peer's SHUT_RD.
    server.shutdown().unwrap();
    drop(server);
    let write_error = signal_client
        .send(&AttachmentFrame::Signal {
            signal: "interrupt".into(),
        })
        .unwrap_err();
    assert!(matches!(write_error, DaemonError::ShellInputWriteFailed(_)));
    let (result, stderr) = result_receive
        .recv_timeout(Duration::from_secs(2))
        .expect("local signal write failure must end the attachment");
    assert!(matches!(
        result,
        Err(DaemonError::InvalidState(_) | DaemonError::Io(_))
    ));
    let diagnostic = String::from_utf8(stderr).unwrap();
    assert!(
        diagnostic.is_empty() || diagnostic.contains("transport closed"),
        "{diagnostic}"
    );
}

#[test]
fn shell_stdin_window_keeps_signal_sendable_when_guest_stalls() {
    let (server_stream, client_stream) = UnixStream::pair().unwrap();
    server_stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let server = ServerAttachment::new(server_stream).unwrap();
    let client = ClientExecution::new_shell(client_stream).unwrap();
    let input_client = client.clone();
    let (window_sent, window_received) = mpsc::channel();
    let (extra_sent, extra_received) = mpsc::channel();
    let input = thread::spawn(move || {
        for index in 0..=SHELL_STDIN_WINDOW {
            input_client
                .send(&AttachmentFrame::Stdin {
                    bytes: vec![b'x'; SHELL_STDIN_CHUNK],
                })
                .unwrap();
            if index + 1 == SHELL_STDIN_WINDOW {
                window_sent.send(()).unwrap();
            }
        }
        extra_sent.send(()).unwrap();
    });
    for _ in 0..SHELL_STDIN_WINDOW {
        assert!(matches!(
            server.receive().unwrap(),
            AttachmentFrame::Stdin { bytes } if bytes == vec![b'x'; SHELL_STDIN_CHUNK]
        ));
    }
    window_received
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert!(
        extra_received
            .recv_timeout(Duration::from_millis(50))
            .is_err()
    );
    client
        .send(&AttachmentFrame::Signal {
            signal: "interrupt".into(),
        })
        .unwrap();
    assert_eq!(
        server.receive().unwrap(),
        AttachmentFrame::Signal {
            signal: "interrupt".into()
        }
    );
    let output_client = client.clone();
    let output = thread::spawn(move || output_client.receive().unwrap());
    server.send(&AttachmentFrame::StdinCredit).unwrap();
    assert!(matches!(
        server.receive().unwrap(),
        AttachmentFrame::Stdin { bytes } if bytes == vec![b'x'; SHELL_STDIN_CHUNK]
    ));
    extra_received.recv_timeout(Duration::from_secs(2)).unwrap();
    server.send(&AttachmentFrame::Exited { code: 130 }).unwrap();
    assert_eq!(
        output.join().unwrap(),
        AttachmentFrame::Exited { code: 130 }
    );
    input.join().unwrap();
}

fn home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn stock_sbx(contents: &[u8]) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(contents).unwrap();
    file.flush().unwrap();
    file
}

#[test]
fn runtime_identity_binds_the_canonical_sbx_path_and_contents() {
    let executable = std::env::current_exe().unwrap();
    let first = stock_sbx(b"same bytes");
    let second = stock_sbx(b"same bytes");
    let first_identity = runtime_identity(&executable, first.path()).unwrap();

    assert_ne!(
        first_identity,
        runtime_identity(&executable, second.path()).unwrap(),
        "two configured files with identical bytes are still distinct identities"
    );

    let link_directory = tempfile::tempdir().unwrap();
    let link = link_directory.path().join("sbx-link");
    std::os::unix::fs::symlink(first.path(), &link).unwrap();
    assert_eq!(
        first_identity,
        runtime_identity(&executable, &link).unwrap(),
        "a symlink resolves to its canonical configured file"
    );

    std::fs::write(first.path(), b"changed bytes").unwrap();
    assert_ne!(
        first_identity,
        runtime_identity(&executable, first.path()).unwrap(),
        "replacing the configured artifact changes daemon identity"
    );
}

#[test]
fn cleanup_quarantine_removes_worker_from_warm_ready_admission() {
    let home = home();
    let store = DaemonStore::new(home.path());
    store
        .register_worker(WorkerStatus {
            worker_id: "worker-1".into(),
            vm_id: "vm-1".into(),
            scope_id: store.status(None).scope_id,
            kit_ref: "kit:test".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 8,
            active_container_ids: Vec::new(),
        })
        .unwrap();

    store.quarantine_worker("worker-1").unwrap();

    let worker = &store.status(None).workers[0];
    assert_eq!(worker.health, WorkerHealth::Quarantined);
    assert!(!worker.warm);
}

#[test]
fn host_terminal_mode_is_raw_and_restores_exact_flags() {
    let opened = nix::pty::openpty(None, None).unwrap();
    let terminal = File::from(opened.slave);
    let original = rustix::termios::tcgetattr(&terminal).unwrap();
    {
        let _guard = HostTerminalMode::enter(&terminal).unwrap();
        let raw = rustix::termios::tcgetattr(&terminal).unwrap();
        assert!(!raw.local_modes.contains(
            rustix::termios::LocalModes::ECHO
                | rustix::termios::LocalModes::ICANON
                | rustix::termios::LocalModes::ISIG
        ));
    }
    let restored = rustix::termios::tcgetattr(&terminal).unwrap();
    assert_eq!(restored.input_modes, original.input_modes);
    assert_eq!(restored.output_modes, original.output_modes);
    assert_eq!(restored.control_modes, original.control_modes);
    let mut restored_local = restored.local_modes;
    let mut original_local = original.local_modes;
    // macOS may assert the transient PENDIN status bit while applying the
    // saved canonical settings; it is not a configured terminal mode.
    restored_local.remove(rustix::termios::LocalModes::PENDIN);
    original_local.remove(rustix::termios::LocalModes::PENDIN);
    assert_eq!(restored_local, original_local);
}

#[test]
fn initial_terminal_dimensions_are_frozen_before_attachment() {
    let opened = nix::pty::openpty(None, None).unwrap();
    let terminal = File::from(opened.slave);
    rustix::termios::tcsetwinsize(
        &terminal,
        rustix::termios::Winsize {
            ws_row: 51,
            ws_col: 173,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    let home = home();
    let mut request = session("session", home.path());
    request.terminal = true;

    capture_initial_terminal_size_from(&mut request, &terminal).unwrap();

    assert_eq!(
        request.terminal_size,
        Some(TerminalSize {
            rows: 51,
            columns: 173,
        })
    );
}

#[test]
fn zero_initial_terminal_dimensions_fall_back_safely() {
    let opened = nix::pty::openpty(None, None).unwrap();
    let terminal = File::from(opened.slave);
    rustix::termios::tcsetwinsize(
        &terminal,
        rustix::termios::Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .unwrap();
    let home = home();
    let mut request = session("session", home.path());
    request.terminal = true;

    capture_initial_terminal_size_from(&mut request, &terminal).unwrap();

    assert_eq!(
        request.terminal_size,
        Some(TerminalSize {
            rows: 24,
            columns: 80,
        })
    );
}

#[test]
fn resize_frames_ignore_transient_zero_terminal_dimensions() {
    assert!(
        resize_frame(rustix::termios::Winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        })
        .is_none()
    );
    assert!(
        resize_frame(rustix::termios::Winsize {
            ws_row: 24,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        })
        .is_none()
    );
    assert!(matches!(
        resize_frame(rustix::termios::Winsize {
            ws_row: 47,
            ws_col: 123,
            ws_xpixel: 0,
            ws_ypixel: 0,
        }),
        Some(AttachmentFrame::Resize {
            rows: 47,
            columns: 123
        })
    ));
}

fn authority(project: &str, home: &Path) -> SessionAuthority {
    SessionAuthority {
        username: "example".into(),
        uid: 1000,
        gid: 1000,
        launch_directory: project.into(),
        guest_home: "/Users/example".into(),
        home_backing: home.into(),
        ephemeral_home: false,
    }
}

#[test]
fn pinned_project_attachment_rejects_replacement_and_relay_callers() {
    let root = home();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let expected = project_identity(&project).unwrap();
    let server = Arc::new(Server::bind(root.path()).unwrap());
    let host = Client::connect(root.path()).unwrap();
    let request = PublicRequest::AttachPinnedShell {
        pid: 7,
        session: authority(project.to_str().unwrap(), root.path()),
        expected_project_identity: expected,
    };
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_one().unwrap());
    let PublicReply::ShellAttached { session_id } = host.request(request.clone()).unwrap() else {
        panic!("host pinned attachment failed");
    };
    task.join().unwrap();
    assert_eq!(
        server.store.session_project_identity(&session_id),
        Some(expected)
    );
    let relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: server.store.issue_relay_token(&session_id).unwrap(),
    };
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_one().unwrap());
    assert!(matches!(
        relay.request(request.clone()).unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    task.join().unwrap();
    fs::rename(&project, root.path().join("original-project")).unwrap();
    fs::create_dir(&project).unwrap();
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_one().unwrap());
    assert!(matches!(
        host.request(request).unwrap(),
        PublicReply::Error { .. }
    ));
    task.join().unwrap();
    assert_eq!(
        server.store.session_project_identity(&session_id),
        Some(expected)
    );
    assert_eq!(server.store.status(None).shells.len(), 1);
    assert!(
        serde_json::from_value::<PublicRequest>(serde_json::json!({
            "type": "attach_pinned_shell", "pid": 7,
            "session": authority(project.to_str().unwrap(), root.path())
        }))
        .is_err(),
        "missing expected identity must fail deserialization"
    );
}

#[test]
fn mcp_publication_uses_attached_session_and_rejects_replaced_project() {
    let root = home();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let store = DaemonStore::new(root.path());
    let session_id = store.attach_shell(7, authority(project.to_str().unwrap(), root.path()));
    let token = store.issue_relay_token(&session_id).unwrap();
    let mut request = PublicRequest::McpPublish {
        session: SessionSpec {
            session_id: session_id.clone(),
            username: "spoof".into(),
            uid: 0,
            gid: 0,
            launch_directory: root.path().join("other"),
            guest_home: "/root".into(),
            home_backing: root.path().join("other-home"),
            ephemeral_home: true,
            terminal: false,
            terminal_size: None,
        },
        name: "demo".into(),
        description: None,
        sandbox: None,
        kit: None,
        pipeline: "printf hello".into(),
    };
    authorize_request_session(
        &store,
        &AuthenticationScope::Relay(session_id.clone()),
        &token,
        &mut request,
    )
    .unwrap();
    let PublicRequest::McpPublish { session, .. } = request else {
        panic!("wrong request variant")
    };
    assert_eq!(session.launch_directory, project);
    assert_eq!(session.home_backing, root.path());
    assert_eq!(session.uid, 1000);
    assert!(store.verify_publication_project(&session).is_ok());

    let moved = root.path().join("moved");
    fs::rename(&project, &moved).unwrap();
    fs::create_dir(&project).unwrap();
    assert!(store.verify_publication_project(&session).is_err());
}

#[test]
fn mcp_publication_requires_relay_token_not_host_or_kit_master_token() {
    let home = home();
    let server = Arc::new(Server::bind(home.path()).unwrap());
    let client = Client::connect(home.path()).unwrap();
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_one().unwrap());
    let reply = client
        .request(PublicRequest::McpPublish {
            session: session("kit-or-host", home.path()),
            name: "demo".into(),
            description: None,
            sandbox: None,
            kit: None,
            pipeline: "printf hello".into(),
        })
        .unwrap();
    assert!(matches!(
        reply,
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    task.join().unwrap();
}

// A controlled host process speaking the actual private publication protocol.
// The daemon still owns admission, lineage, socket framing and outcome classes.
fn typed_mcp_host_peer(body: &str) -> String {
    let protocol = r"#!/usr/bin/python3
import json,os,pathlib,struct,sys,time
def exact(n):
 data=b''
 while len(data)<n:
  part=os.read(0,n-len(data))
  if not part: raise RuntimeError('publication peer EOF')
  data+=part
 return data
def receive():
 n,=struct.unpack('!I',exact(4))
 if n>1048576: raise RuntimeError('publication frame too large')
 return json.loads(exact(n))
def send(value):
 data=json.dumps(value).encode(); data=struct.pack('!I',len(data))+data
 while data:
  n=os.write(0,data)
  if not n: raise RuntimeError('publication peer write closed')
  data=data[n:]
context=receive()
send({'type':'begin_commit'})
assert receive()=={'Ok':None}
def committed(message):
 send({'type':'complete','data':{'status':'committed','message':message}})
";
    format!("{protocol}\n{body}\n")
}

#[test]
fn attached_relay_waits_for_slow_mcp_host_reply() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = home();
    let selected_home = root.path().join("selected-home");
    let host_home = root.path().join("host-home");
    let project = root.path().join("project");
    fs::create_dir(&selected_home).unwrap();
    fs::create_dir(&host_home).unwrap();
    fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let host_cli = root.path().join("slow-marsh");
    fs::write(
        &host_cli,
        typed_mcp_host_peer("time.sleep(6)\ncommitted('host operation finished')"),
    )
    .unwrap();
    fs::set_permissions(&host_cli, fs::Permissions::from_mode(0o700)).unwrap();
    let mut server = Server::bind(&selected_home).unwrap();
    server.mcp_host = Some(McpHostControl::new(
        host_cli,
        root.path().join("unused-sbx"),
        host_home,
        None,
    ));
    let attached = server.store.attach_shell(
        7,
        SessionAuthority {
            username: "owner".into(),
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            launch_directory: project.clone(),
            guest_home: root.path().join("guest"),
            home_backing: selected_home.clone(),
            ephemeral_home: false,
        },
    );
    let relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: server.store.issue_relay_token(&attached).unwrap(),
    };
    let mut spec = session(&attached, &selected_home);
    spec.launch_directory = project;
    spec.username = "owner".into();
    spec.uid = rustix::process::geteuid().as_raw();
    spec.gid = rustix::process::getegid().as_raw();
    let server = Arc::new(server);
    for publish in [true, false] {
        let task_server = Arc::clone(&server);
        let task = thread::spawn(move || task_server.serve_one().unwrap());
        let started = Instant::now();
        let result = if publish {
            relay.mcp_publish(spec.clone(), "slow".into(), None, None, "cat".into())
        } else {
            relay.mcp_unpublish(spec.clone(), "slow".into())
        };
        assert!(result.unwrap().contains("host operation finished"));
        assert!(started.elapsed() >= Duration::from_secs(6));
        task.join().unwrap();
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "real socket race and direct host overwrite share one publication fixture"
)]
fn concurrent_mcp_publishes_are_serialized_and_invalid_targets_rejected() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = home();
    let selected_home = root.path().join("selected-home");
    let host_home = root.path().join("host-home");
    let project = root.path().join("project");
    for path in [&selected_home, &host_home, &project] {
        fs::create_dir(path).unwrap();
    }
    let project = project.canonicalize().unwrap();
    let mut server = Server::bind(&selected_home).unwrap();
    let session_authority = SessionAuthority {
        username: "owner".into(),
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        launch_directory: project.clone(),
        guest_home: root.path().join("guest"),
        home_backing: selected_home.clone(),
        ephemeral_home: false,
    };
    let public_id = server.store.attach_shell(1, session_authority.clone());
    let private_id = server.store.attach_shell(2, session_authority);
    let spec = |id: &str| SessionSpec {
        session_id: id.into(),
        username: "owner".into(),
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        launch_directory: project.clone(),
        guest_home: root.path().join("guest"),
        home_backing: selected_home.clone(),
        ephemeral_home: false,
        terminal: false,
        terminal_size: None,
    };
    let public_spec = spec(&public_id);
    let public_spec_again = public_spec.clone();
    let private_spec = spec(&private_id);
    let host_cli = root.path().join("fake-marsh");
    let host = McpHostControl::new(
        host_cli.clone(),
        root.path().join("unused-sbx"),
        host_home,
        None,
    );
    let declaration = host
        .declaration_path(&public_spec, PublicationKind::Mcp, "shared")
        .unwrap();
    fs::create_dir_all(declaration.parent().unwrap()).unwrap();
    for directory in declaration.ancestors().skip(1).take(3) {
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let public_started = root.path().join("public-started");
    let private_started = root.path().join("private-started");
    let script = typed_mcp_host_peer(&format!(
        "pipeline=sys.argv[-1]\np=pathlib.Path({declaration})\np.write_bytes(pipeline.encode())\np.chmod(0o600)\nif pipeline=='public':\n pathlib.Path({public_started}).touch()\n time.sleep(1)\nelse:\n pathlib.Path({private_started}).touch()\ncommitted('published '+pipeline)",
        declaration = serde_json::to_string(&declaration).unwrap(),
        public_started = serde_json::to_string(&public_started).unwrap(),
        private_started = serde_json::to_string(&private_started).unwrap(),
    ));
    fs::write(&host_cli, script).unwrap();
    fs::set_permissions(&host_cli, fs::Permissions::from_mode(0o700)).unwrap();
    server.mcp_host = Some(host);
    let public_relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: server.store.issue_relay_token(&public_id).unwrap(),
    };
    let private_relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: server.store.issue_relay_token(&private_id).unwrap(),
    };
    let server = Arc::new(server);
    let public_server = Arc::clone(&server);
    let public_request = thread::spawn(move || public_server.serve_one().unwrap());
    let public = thread::spawn(move || {
        public_relay.mcp_publish(public_spec, "shared".into(), None, None, "public".into())
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !public_started.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        public_started.exists(),
        "first publisher did not reach host CLI: {:?}",
        public.join().unwrap()
    );
    let private_server = Arc::clone(&server);
    let private_request = thread::spawn(move || private_server.serve_one().unwrap());
    let private = thread::spawn(move || {
        private_relay.mcp_publish(private_spec, "shared".into(), None, None, "private".into())
    });
    thread::sleep(Duration::from_millis(150));
    assert!(
        !private_started.exists(),
        "competing publisher reached host CLI before first declaration was bound"
    );
    assert!(public.join().unwrap().unwrap().contains("published public"));
    assert!(
        private
            .join()
            .unwrap()
            .unwrap()
            .contains("published private")
    );
    public_request.join().unwrap();
    private_request.join().unwrap();
    assert_eq!(fs::read(&declaration).unwrap(), b"private");
    // Actual raw relay admission must reject incompatible targets,
    // independent of the CLI's syntactic option parser.
    let invalid_relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: server.store.issue_relay_token(&private_id).unwrap(),
    };
    let mut invalid_spec = public_spec_again.clone();
    invalid_spec.session_id = private_id.clone();
    let invalid_server = Arc::clone(&server);
    let invalid_task = thread::spawn(move || invalid_server.serve_one().unwrap());
    let reply = invalid_relay
        .request(PublicRequest::McpPublish {
            session: invalid_spec,
            name: "shared".into(),
            description: None,
            sandbox: Some("target".into()),
            kit: Some("other-kit".into()),
            pipeline: "private".into(),
        })
        .unwrap();
    assert!(
        matches!(
            reply,
            PublicReply::McpPublication {
                outcome: PublicationOutcome::RejectedBeforeEffect { .. },
            }
        ),
        "{reply:?}"
    );
    invalid_task.join().unwrap();
    assert_eq!(fs::read(&declaration).unwrap(), b"private");
}

#[test]
fn attachment_rejects_guest_mounts_overlapping_daemon_runtime() {
    let root = home();
    let selected_home = root.path().join("selected-home");
    let project = root.path().join("project");
    fs::create_dir(&selected_home).unwrap();
    fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let server = Arc::new(Server::bind(&selected_home).unwrap());
    let runtime = server.lifecycle.paths.runtime_directory.clone();
    let child = runtime.join("mount-child");
    fs::create_dir(&child).unwrap();
    let overlap = [
        runtime.parent().unwrap().to_path_buf(),
        runtime,
        child.clone(),
    ];
    let (stop, task) = start_server(Arc::clone(&server));
    let client = Client::connect(&selected_home).unwrap();
    for mount in overlap {
        for home_mount in [false, true] {
            let mut session = authority(project.to_str().unwrap(), &selected_home);
            if home_mount {
                session.home_backing = mount.clone();
            } else {
                session.launch_directory = mount.clone();
            }
            assert!(matches!(
                client
                    .request(PublicRequest::AttachShell { pid: 7, session })
                    .unwrap(),
                PublicReply::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                }
            ));
        }
    }
    assert!(server.store.status(None).shells.is_empty());
    let session = authority(project.to_str().unwrap(), &selected_home);
    assert!(matches!(
        client
            .request(PublicRequest::AttachShell { pid: 7, session })
            .unwrap(),
        PublicReply::ShellAttached { .. }
    ));
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
    fs::remove_dir(child).unwrap();
}

#[test]
fn lifecycle_is_exclusive_and_owner_only() {
    let home = home();
    let lifecycle = Lifecycle::bind(home.path()).unwrap();
    assert!(!lifecycle.paths().runtime_directory.starts_with(home.path()));
    assert_eq!(
        fs::metadata(&lifecycle.paths().socket).unwrap().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&lifecycle.paths().token).unwrap().mode() & 0o777,
        0o600
    );
    assert!(matches!(
        Lifecycle::bind(home.path()),
        Err(DaemonError::EndpointExists(_))
    ));
    drop(lifecycle);
    Lifecycle::bind(home.path()).unwrap();
}

#[test]
fn endpoint_rejects_unsafe_home() {
    let home = home();
    fs::set_permissions(home.path(), fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        EndpointPaths::for_home(home.path()),
        Err(DaemonError::UnsafeHome(_))
    ));
}

fn endpoint_paths(runtime_directory: PathBuf) -> EndpointPaths {
    EndpointPaths {
        socket: runtime_directory.join("s"),
        lock: runtime_directory.join("l"),
        token: runtime_directory.join("t"),
        runtime_directory,
    }
}

#[test]
fn stale_owned_endpoint_is_reclaimed_but_live_owner_is_preserved() {
    let root = home();
    let stale = endpoint_paths(root.path().join("stale"));
    fs::create_dir(&stale.runtime_directory).unwrap();
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    fs::write(&stale.lock, format!("{pid}\n")).unwrap();
    assert_eq!(
        reclaim_stale_endpoint(&stale).unwrap(),
        ReclaimOutcome::Reclaimed
    );
    assert!(!stale.runtime_directory.exists());

    let live = endpoint_paths(root.path().join("live"));
    fs::create_dir(&live.runtime_directory).unwrap();
    fs::write(&live.lock, format!("{}\n", std::process::id())).unwrap();
    assert_eq!(reclaim_stale_endpoint(&live).unwrap(), ReclaimOutcome::Live);
    assert!(live.lock.exists());
}

#[test]
fn stale_endpoint_with_unknown_content_fails_closed() {
    let root = home();
    let paths = endpoint_paths(root.path().join("unknown"));
    fs::create_dir(&paths.runtime_directory).unwrap();
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    fs::write(&paths.lock, format!("{pid}\n")).unwrap();
    fs::write(paths.runtime_directory.join("foreign"), b"leave me").unwrap();
    assert!(reclaim_stale_endpoint(&paths).is_err());
    assert!(paths.runtime_directory.join("foreign").exists());
}

#[test]
fn framing_is_bounded_and_round_trips() {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &PublicRequest::Ping).unwrap();
    assert_eq!(
        read_frame::<PublicRequest>(&mut Cursor::new(bytes)).unwrap(),
        PublicRequest::Ping
    );
    let size = u32::try_from(MAX_FRAME_BYTES).unwrap() + 1;
    let mut oversized = Cursor::new(size.to_be_bytes());
    assert!(matches!(
        read_frame::<PublicRequest>(&mut oversized),
        Err(DaemonError::FrameTooLarge(_))
    ));
}

#[test]
fn store_exposes_safe_versioned_documents() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let scope = store.status(Some(session.clone())).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker-1".into(),
            vm_id: "vm-1".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 3,
            active_container_ids: vec![],
        })
        .unwrap();
    let (job, attempt) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("example/fixture@sha256:{}", "a".repeat(64)),
            mounts: vec![PublicMount {
                target: "/Users/example/project".into(),
                access: "read_write".into(),
            }],
        })
        .unwrap();
    store.reserve_worker(&job, "worker-1", "vm-1").unwrap();
    store
        .mark_running(&job, "worker-1", "vm-1", &"b".repeat(64))
        .unwrap();
    let receipt = store.job(&job).unwrap();
    assert_eq!(receipt.attempt_id, attempt);
    assert_eq!(receipt.schema, "marsh.job/v1");
    assert_eq!(store.jobs().schema, "marsh.jobs/v1");
    let status = store.status(None);
    assert_eq!(status.schema, "marsh.status/v1");
    assert!(
        status
            .features
            .iter()
            .any(|feature| feature == "durable-results-v1")
    );
    let json = serde_json::to_string(&receipt).unwrap();
    assert!(!json.contains("/run/marsh/grants"));
    store
        .finish_job(
            &job,
            ExitStatus {
                code: Some(0),
                cause: "exited".into(),
            },
            true,
            CleanupState::Verified,
            TimingReport::default(),
        )
        .unwrap();
    let status = store.status(None);
    assert!(status.workers[0].active_container_ids.is_empty());
    assert_eq!(store.job(&job).unwrap().state, JobState::Finished);
}

#[test]
fn uncertain_cleanup_quarantines_worker_and_releases_capacity() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker-1".into(),
            vm_id: "vm-1".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 1,
            active_container_ids: vec![],
        })
        .unwrap();
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("example/fixture@sha256:{}", "a".repeat(64)),
            mounts: vec![],
        })
        .unwrap();
    store.reserve_worker(&job, "worker-1", "vm-1").unwrap();
    store
        .mark_running(&job, "worker-1", "vm-1", &"c".repeat(64))
        .unwrap();
    store
        .finish_job(
            &job,
            ExitStatus {
                code: None,
                cause: "controller_lost".into(),
            },
            false,
            CleanupState::Uncertain,
            TimingReport::default(),
        )
        .unwrap();
    let status = store.status(None);
    assert_eq!(status.workers[0].health, WorkerHealth::Quarantined);
    assert!(status.workers[0].active_container_ids.is_empty());
    let receipt = store.job(&job).unwrap();
    assert_eq!(receipt.state, JobState::Unknown);
    assert_eq!(receipt.exit.unwrap().code, Some(125));
    assert_eq!(
        store.job(&job).unwrap().exit.unwrap().cause,
        "controller_lost; cleanup_uncertain"
    );
    assert!(store.worker_reuse_blocked("vm-1"));
    drop(store);
    let reopened = DaemonStore::new(home.path());
    assert!(reopened.status(None).workers.is_empty());
    assert!(
        !reopened.worker_reuse_blocked("vm-1"),
        "a historical receipt must not block a physically recreated VM"
    );
}

#[test]
fn reserved_attempt_failure_remains_auditable_without_blocking_fresh_vm() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker-reserved".into(),
            vm_id: "vm-reserved".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 1,
            active_container_ids: vec![],
        })
        .unwrap();
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("example/fixture@sha256:{}", "a".repeat(64)),
            mounts: vec![],
        })
        .unwrap();
    store
        .reserve_worker(&job, "worker-reserved", "vm-reserved")
        .unwrap();
    assert_eq!(
        store.job(&job).unwrap().vm_id.as_deref(),
        Some("vm-reserved")
    );
    store.abort_job(&job, "backend_failed").unwrap();
    drop(store);

    let reopened = DaemonStore::new(home.path());
    assert!(!reopened.worker_reuse_blocked("vm-reserved"));
}

#[test]
fn verified_pre_runtime_cleanup_does_not_tombstone_vm() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker-clean".into(),
            vm_id: "vm-clean".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 1,
            active_container_ids: vec![],
        })
        .unwrap();
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("example/fixture@sha256:{}", "a".repeat(64)),
            mounts: vec![],
        })
        .unwrap();
    store
        .reserve_worker(&job, "worker-clean", "vm-clean")
        .unwrap();
    store
        .finish_job(
            &job,
            ExitStatus {
                code: Some(125),
                cause: "worker_start_failed".into(),
            },
            false,
            CleanupState::Verified,
            TimingReport::default(),
        )
        .unwrap();
    drop(store);

    let reopened = DaemonStore::new(home.path());
    assert!(!reopened.worker_reuse_blocked("vm-clean"));
}

#[test]
fn uncertain_grant_rollback_preserves_cause_without_blocking_fresh_vm() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker-grant".into(),
            vm_id: "vm-grant".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 1,
            active_container_ids: vec![],
        })
        .unwrap();
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("example/fixture@sha256:{}", "a".repeat(64)),
            mounts: vec![],
        })
        .unwrap();
    store
        .reserve_worker(&job, "worker-grant", "vm-grant")
        .unwrap();
    store
        .finish_job(
            &job,
            ExitStatus {
                code: Some(125),
                cause: "grant_prepare_failed: second mount rejected".into(),
            },
            false,
            CleanupState::Uncertain,
            TimingReport::default(),
        )
        .unwrap();
    assert_eq!(
        store.job(&job).unwrap().exit.unwrap().cause,
        "grant_prepare_failed: second mount rejected; cleanup_uncertain"
    );
    drop(store);

    let reopened = DaemonStore::new(home.path());
    assert!(!reopened.worker_reuse_blocked("vm-grant"));
}

#[test]
fn detaching_a_shell_revokes_its_relay_tokens() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let token = store.issue_relay_token(&session).unwrap();

    assert!(store.authentication_scope(&token).is_some());
    store.detach_shell(&session).unwrap();
    assert!(store.authentication_scope(&token).is_none());
}

#[test]
fn relay_token_restores_host_authority_and_rejects_other_sessions() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let first = store.attach_shell(1, authority("/Users/example/approved", home.path()));
    let second = store.attach_shell(2, authority("/Users/example/other", home.path()));
    let token = store.issue_relay_token(&first).unwrap();
    let forged = SessionSpec {
        session_id: first.clone(),
        username: "root".into(),
        uid: 0,
        gid: 0,
        launch_directory: "/private".into(),
        guest_home: "/root".into(),
        home_backing: "/".into(),
        ephemeral_home: true,
        terminal: true,
        terminal_size: Some(TerminalSize {
            rows: 31,
            columns: 119,
        }),
    };

    let authorized = store.authorize_session(&token, &forged).unwrap();
    assert_eq!(authorized.username, "example");
    assert_eq!(authorized.uid, 1000);
    assert_eq!(
        authorized.launch_directory,
        Path::new("/Users/example/approved")
    );
    assert_eq!(authorized.home_backing, home.path());
    assert!(!authorized.ephemeral_home);
    assert!(authorized.terminal);

    let mut cross_session = forged;
    cross_session.session_id = second;
    assert!(store.authorize_session(&token, &cross_session).is_err());
}

#[test]
fn job_capability_admits_only_its_own_requests() {
    // A job's capability socket must not become a general daemon channel:
    // ProcessRun, ProcessShow, its own jobs, SplitCreate, and SplitJoin only.
    let root = tempfile::tempdir().unwrap();
    let server = Arc::new(Server::bind(root.path()).unwrap());
    let capability = Client {
        paths: server.lifecycle.paths.clone(),
        token: server.store.issue_capability_token("job-1").unwrap(),
    };
    for request in [
        PublicRequest::Status { session_id: None },
        PublicRequest::RegisteredCommands,
        PublicRequest::SplitShow { split: None },
        PublicRequest::SplitRemove { split: "x".into() },
        PublicRequest::ResetWorkers {
            selection: LoadSelection::All,
        },
    ] {
        let task_server = Arc::clone(&server);
        let task = thread::spawn(move || task_server.serve_one().unwrap());
        assert!(
            matches!(
                capability.request(request.clone()).unwrap(),
                PublicReply::Error {
                    code: ErrorCode::Unauthorized,
                    ..
                }
            ),
            "{request:?}"
        );
        task.join().unwrap();
    }
    // A revoked capability authenticates nothing.
    server.store.revoke_capability_token(&capability.token);
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_one().unwrap());
    assert!(matches!(
        capability
            .request(PublicRequest::SplitJoin { split: "x".into() })
            .unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    task.join().unwrap();
}

#[test]
fn relay_token_cannot_detach_another_session() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let first = store.attach_shell(1, authority("/Users/example/first", home.path()));
    let second = store.attach_shell(2, authority("/Users/example/second", home.path()));
    let token = store.issue_relay_token(&first).unwrap();
    let scope = store.authentication_scope(&token).unwrap();
    let mut request = PublicRequest::DetachShell {
        session_id: second.clone(),
    };

    assert!(authorize_request_session(&store, &scope, &token, &mut request).is_err());
    assert!(
        store
            .status(Some(second.clone()))
            .shells
            .iter()
            .any(|shell| shell.session_id == second && shell.state == ShellState::Attached)
    );

    let mut own_request = PublicRequest::DetachShell {
        session_id: first.clone(),
    };
    authorize_request_session(&store, &scope, &token, &mut own_request).unwrap();
    assert_eq!(
        dispatch_request(&store, &EchoBackend, "sha256:test", own_request),
        PublicReply::Detached
    );
    assert!(
        store
            .status(Some(second.clone()))
            .shells
            .iter()
            .any(|shell| shell.session_id == second && shell.state == ShellState::Attached)
    );
}

#[test]
fn capacity_is_reserved_before_container_identity_and_abort_releases_it() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker".into(),
            vm_id: "vm".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 1,
            active_container_ids: Vec::new(),
        })
        .unwrap();
    let new_job = || NewJob {
        session_id: session.clone(),
        command: "fixture".into(),
        kit_ref: "fixture".into(),
        workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
        mounts: Vec::new(),
    };
    let (first, _) = store.begin_job(new_job()).unwrap();
    let (second, _) = store.begin_job(new_job()).unwrap();

    store.reserve_worker(&first, "worker", "vm").unwrap();
    assert!(store.reserve_worker(&second, "worker", "vm").is_err());
    store.cancel_job(&first, "setup_failed").unwrap();
    store.reserve_worker(&second, "worker", "vm").unwrap();
    assert_eq!(store.job(&first).unwrap().state, JobState::Cancelled);
}

#[test]
fn worker_repair_claim_is_atomic_with_admission_and_live_work() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker".into(),
            vm_id: "vm".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 2,
            active_container_ids: Vec::new(),
        })
        .unwrap();
    let new_job = || NewJob {
        session_id: session.clone(),
        command: "fixture".into(),
        kit_ref: "fixture".into(),
        workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
        mounts: Vec::new(),
    };
    let (admitted, _) = store.begin_job(new_job()).unwrap();
    store.reserve_worker(&admitted, "worker", "vm").unwrap();
    assert!(store.begin_worker_repair("worker", "vm").is_err());

    store.cancel_job(&admitted, "test_release").unwrap();
    assert!(store.begin_worker_repair("worker", "vm").unwrap());
    assert_eq!(
        store.status(None).workers[0].health,
        WorkerHealth::Repairing
    );
    let (blocked, _) = store.begin_job(new_job()).unwrap();
    assert!(store.reserve_worker(&blocked, "worker", "vm").is_err());
    store.finish_worker_repair("worker", true).unwrap();
    store.reserve_worker(&blocked, "worker", "vm").unwrap();
    store
        .mark_running(&blocked, "worker", "vm", &"d".repeat(64))
        .unwrap();
    assert!(store.begin_worker_repair("worker", "vm").is_err());
}

#[test]
fn failed_worker_repair_quarantines_the_generation() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker".into(),
            vm_id: "vm".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 1,
            active_container_ids: Vec::new(),
        })
        .unwrap();

    assert!(store.begin_worker_repair("worker", "vm").unwrap());
    store.finish_worker_repair("worker", false).unwrap();
    let worker = &store.status(None).workers[0];
    assert_eq!(worker.health, WorkerHealth::Quarantined);
    assert!(!worker.warm);
    assert!(store.worker_reuse_blocked("vm"));
}

#[test]
fn worker_reset_refuses_active_selection_atomically_and_preserves_other_state() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    for worker in ["first", "second"] {
        store
            .register_worker(WorkerStatus {
                worker_id: worker.into(),
                vm_id: worker.into(),
                scope_id: scope.clone(),
                kit_ref: worker.into(),
                kits: Vec::new(),
                warm: true,
                health: WorkerHealth::Ready,
                container_capacity: 1,
                active_container_ids: Vec::new(),
            })
            .unwrap();
    }
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session.clone(),
            command: "first".into(),
            kit_ref: "first".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    store.reserve_worker(&job, "first", "first").unwrap();
    let selected = vec!["first".to_string(), "second".to_string()];
    assert!(store.begin_workers_reset(&selected).is_err());
    assert!(
        store
            .status(None)
            .workers
            .iter()
            .all(|worker| worker.health == WorkerHealth::Ready && worker.warm)
    );

    store.cancel_job(&job, "test_complete").unwrap();
    store.begin_workers_reset(&selected).unwrap();
    store.finish_worker_reset("first", true).unwrap();
    store.finish_worker_reset("second", true).unwrap();
    let status = store.status(None);
    assert!(status.workers.is_empty());
    assert_eq!(status.shells.len(), 1);
    assert_eq!(store.job(&job).unwrap().state, JobState::Cancelled);
}

#[test]
fn abort_receipt_is_explicitly_failed() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    store.abort_job(&job, "transport_failed").unwrap();
    let receipt = store.job(&job).unwrap();
    assert_eq!(receipt.state, JobState::Failed);
    let exit = receipt.exit.unwrap();
    assert_eq!(exit.code, Some(125));
    assert_eq!(exit.cause, "transport_failed; cleanup_uncertain");
    assert!(!receipt.output_complete);
}

#[test]
fn incomplete_output_makes_terminal_receipt_failed() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker".into(),
            vm_id: "vm".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 1,
            active_container_ids: Vec::new(),
        })
        .unwrap();
    let new_job = || NewJob {
        session_id: session.clone(),
        command: "fixture".into(),
        kit_ref: "fixture".into(),
        workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
        mounts: Vec::new(),
    };
    let (job, _) = store.begin_job(new_job()).unwrap();
    store.reserve_worker(&job, "worker", "vm").unwrap();
    store
        .finish_job(
            &job,
            ExitStatus {
                code: Some(0),
                cause: "exited; output_incomplete".into(),
            },
            false,
            CleanupState::Verified,
            TimingReport::default(),
        )
        .unwrap();
    assert_eq!(store.job(&job).unwrap().state, JobState::Failed);

    let (next, _) = store.begin_job(new_job()).unwrap();
    store.reserve_worker(&next, "worker", "vm").unwrap();
    assert!(!store.worker_reuse_blocked("vm"));
}

#[test]
fn receipts_survive_restart_and_active_work_becomes_unknown() {
    let home = home();
    let (finished, active) = {
        let store = DaemonStore::open(home.path()).unwrap();
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        let create = |command: &str| NewJob {
            session_id: session.clone(),
            command: command.into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        };
        let (finished, _) = store.begin_job(create("finished")).unwrap();
        store
            .finish_job(
                &finished,
                ExitStatus {
                    code: Some(0),
                    cause: "exited".into(),
                },
                true,
                CleanupState::Verified,
                TimingReport {
                    wall_ms: 42,
                    ..TimingReport::default()
                },
            )
            .unwrap();
        let (active, _) = store.begin_job(create("active")).unwrap();
        (finished, active)
    };

    let reopened = DaemonStore::open(home.path()).unwrap();
    assert_eq!(reopened.job(&finished).unwrap().state, JobState::Finished);
    let interrupted = reopened.job(&active).unwrap();
    assert_eq!(interrupted.state, JobState::Unknown);
    assert_eq!(interrupted.exit.unwrap().cause, "daemon_restarted");
    assert_eq!(reopened.jobs().jobs[0].job_id, active);
}

#[test]
fn result_selectors_accept_cursor_full_id_and_unique_prefix() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    let receipt = store.job(&job).unwrap();
    assert_eq!(store.job(&receipt.cursor.to_string()).unwrap().job_id, job);
    assert_eq!(store.job(&job).unwrap().job_id, job);
    assert_eq!(store.job(&job[..8]).unwrap().job_id, job);
    assert!(matches!(
        store.job("not-a-job"),
        Err(DaemonError::NotFound(_))
    ));

    let mut first = receipt.clone();
    first.cursor += 1;
    first.job_id = "aaaaaaaa-0000-0000-0000-000000000001".into();
    let mut second = receipt;
    second.cursor += 2;
    second.job_id = "aaaaaaaa-0000-0000-0000-000000000002".into();
    let mut state = store.lock();
    state.jobs.insert(first.job_id.clone(), first);
    state.jobs.insert(second.job_id.clone(), second);
    drop(state);
    assert!(matches!(
        store.job("aaaaaaaa"),
        Err(DaemonError::InvalidState(message)) if message.contains("ambiguous")
    ));
}

#[test]
fn receipt_journal_is_owner_only_and_contains_no_captured_content() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    store
        .finish_job(
            &job,
            ExitStatus {
                code: Some(0),
                cause: "exited".into(),
            },
            true,
            CleanupState::Verified,
            TimingReport::default(),
        )
        .unwrap();
    let state = home.path().join("state");
    let journal = receipt_journal::journal_path(home.path());
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&journal).unwrap().mode() & 0o777, 0o600);
    let bytes = fs::read(journal).unwrap();
    assert!(
        !bytes
            .windows(b"raw prompt secret".len())
            .any(|part| part == b"raw prompt secret")
    );
    assert!(
        !bytes
            .windows(b"stdout secret".len())
            .any(|part| part == b"stdout secret")
    );
    assert!(
        !bytes
            .windows(b"stderr secret".len())
            .any(|part| part == b"stderr secret")
    );
    for forbidden_key in [b"\"prompt\"".as_slice(), b"\"stdout\"", b"\"stderr\""] {
        assert!(
            !bytes
                .windows(forbidden_key.len())
                .any(|part| part == forbidden_key)
        );
    }
}

#[test]
fn receipt_journal_rejects_permissive_state_modes() {
    let state_home = home();
    fs::create_dir(state_home.path().join("state")).unwrap();
    fs::set_permissions(
        state_home.path().join("state"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(
        DaemonStore::open(state_home.path()),
        Err(DaemonError::UnsafeHome(_))
    ));

    let file_home = home();
    let store = DaemonStore::new(file_home.path());
    drop(store);
    let journal = receipt_journal::journal_path(file_home.path());
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        DaemonStore::open(file_home.path()),
        Err(DaemonError::UnsafeHome(_))
    ));
}

#[test]
fn production_receipts_ignore_guest_written_legacy_journal() {
    let selected = home();
    let control = home();
    let control_home = control.path().canonicalize().unwrap();
    fs::set_permissions(&control_home, fs::Permissions::from_mode(0o700)).unwrap();
    let sbx = stock_sbx(b"stock-sbx");

    let legacy = DaemonStore::new(selected.path());
    let legacy_session =
        legacy.attach_shell(1, authority("/Users/example/project", selected.path()));
    legacy
        .begin_job(NewJob {
            session_id: legacy_session,
            command: "guest-forged".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    let legacy_bytes = fs::read(receipt_journal::journal_path(selected.path())).unwrap();
    let scope_id = legacy.status(None).scope_id;
    drop(legacy);

    let server =
        Server::bind_with_stock_sbx_and_control_home(selected.path(), sbx.path(), &control_home)
            .unwrap();
    assert_eq!(server.store.status(None).scope_id, scope_id);
    assert!(server.store.jobs().jobs.is_empty());
    let session = server
        .store
        .attach_shell(2, authority("/Users/example/project", selected.path()));
    server
        .store
        .begin_job(NewJob {
            session_id: session,
            command: "host-owned".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    assert_eq!(receipt_journal::record_count(&control_home), 1);
    assert_eq!(
        fs::read(receipt_journal::journal_path(selected.path())).unwrap(),
        legacy_bytes
    );
    drop(server);

    let resumed =
        Server::bind_with_stock_sbx_and_control_home(selected.path(), sbx.path(), &control_home)
            .unwrap();
    assert_eq!(resumed.store.status(None).scope_id, scope_id);
    assert_eq!(resumed.store.jobs().jobs.len(), 1);
    assert_eq!(resumed.store.jobs().jobs[0].command, "host-owned");
}

#[test]
fn daemon_endpoint_is_shared_across_different_tmpdirs() {
    let selected = home();
    let control = home();
    fs::set_permissions(control.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let control_home = control.path().canonicalize().unwrap();
    let sbx = stock_sbx(b"stock-sbx");
    let first_tmp = tempfile::Builder::new()
        .prefix("j-a-")
        .tempdir_in("/tmp")
        .unwrap();
    let second_tmp = tempfile::Builder::new()
        .prefix("j-b-")
        .tempdir_in("/tmp")
        .unwrap();
    let runner = |role: &str, tmpdir: &Path| {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::daemon_endpoint_cross_tmpdir_child",
                "--nocapture",
            ])
            .env("MARSH_TEST_JOURNAL_OWNER_ROLE", role)
            .env("MARSH_TEST_JOURNAL_SELECTED", selected.path())
            .env("MARSH_TEST_JOURNAL_CONTROL", &control_home)
            .env("MARSH_TEST_JOURNAL_SBX", sbx.path())
            .env("TMPDIR", tmpdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let owner = runner("owner", first_tmp.path());
    let ready = control_home.join("ready");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if !ready.exists() {
        fs::write(control_home.join("release"), b"go").unwrap();
        let output = owner.wait_with_output().unwrap();
        panic!(
            "owner did not bind its first endpoint: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let contender = runner("contender", second_tmp.path())
        .wait_with_output()
        .unwrap();
    fs::write(control_home.join("release"), b"go").unwrap();
    let owner_output = owner.wait_with_output().unwrap();
    assert!(
        contender.status.success(),
        "second daemon result: {}{}",
        String::from_utf8_lossy(&contender.stdout),
        String::from_utf8_lossy(&contender.stderr)
    );
    assert!(
        owner_output.status.success(),
        "first daemon result: {}{}",
        String::from_utf8_lossy(&owner_output.stdout),
        String::from_utf8_lossy(&owner_output.stderr)
    );
    let reopened =
        Server::bind_with_stock_sbx_and_control_home(selected.path(), sbx.path(), &control_home)
            .unwrap();
    assert_eq!(reopened.store.jobs().jobs.len(), 1);
}

#[test]
fn daemon_endpoint_cross_tmpdir_child() {
    let Ok(role) = std::env::var("MARSH_TEST_JOURNAL_OWNER_ROLE") else {
        return;
    };
    let selected = PathBuf::from(std::env::var_os("MARSH_TEST_JOURNAL_SELECTED").unwrap());
    let control = PathBuf::from(std::env::var_os("MARSH_TEST_JOURNAL_CONTROL").unwrap());
    let sbx = PathBuf::from(std::env::var_os("MARSH_TEST_JOURNAL_SBX").unwrap());
    match role.as_str() {
        "owner" => {
            let server =
                Server::bind_with_stock_sbx_and_control_home(&selected, &sbx, &control).unwrap();
            let session = server
                .store
                .attach_shell(1, authority("/Users/example/project", &selected));
            server
                .store
                .begin_job(NewJob {
                    session_id: session,
                    command: "retained".into(),
                    kit_ref: "fixture".into(),
                    workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                    mounts: Vec::new(),
                })
                .unwrap();
            fs::write(
                control.join("ready"),
                server
                    .lifecycle
                    .paths()
                    .runtime_directory
                    .as_os_str()
                    .as_encoded_bytes(),
            )
            .unwrap();
            server
                .serve_until(|| control.join("release").exists())
                .unwrap();
        }
        "contender" => {
            let owner_runtime = PathBuf::from(fs::read_to_string(control.join("ready")).unwrap());
            let contender_runtime = EndpointPaths::for_home(&selected)
                .unwrap()
                .runtime_directory;
            assert_eq!(owner_runtime, contender_runtime);
            let client = Client::connect(&selected).unwrap();
            assert!(matches!(
                client.request(PublicRequest::Ping).unwrap(),
                PublicReply::Pong { .. }
            ));
            let error = Server::bind_with_stock_sbx_and_control_home(&selected, &sbx, &control)
                .err()
                .expect("a second daemon must not claim the same endpoint");
            assert!(matches!(error, DaemonError::EndpointExists(_)));
            assert!(contender_runtime.join("l").exists());
        }
        other => panic!("unknown child role: {other}"),
    }
}

#[test]
fn journal_owner_lock_rejects_loose_permissions_and_symlink() {
    let home = home();
    drop(DaemonStore::new(home.path()));
    let lock = home.path().join("state/owner.lock");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        DaemonStore::open(home.path()),
        Err(DaemonError::UnsafeHome(_))
    ));
    fs::remove_file(&lock).unwrap();
    let target = home.path().join("target");
    fs::write(&target, b"sentinel").unwrap();
    std::os::unix::fs::symlink(&target, &lock).unwrap();
    assert!(matches!(
        DaemonStore::open(home.path()),
        Err(DaemonError::UnsafeHome(_))
    ));
    assert_eq!(fs::read(&target).unwrap(), b"sentinel");
}

#[test]
fn journal_owner_lock_excludes_a_second_direct_open() {
    let home = home();
    let store = DaemonStore::new(home.path());
    assert!(matches!(
        receipt_journal::ReceiptJournal::open(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("already owned")
    ));
    drop(store);
    assert!(receipt_journal::ReceiptJournal::open(home.path()).is_ok());
}

#[test]
fn ordinary_terminal_transition_appends_without_compacting() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    assert_eq!(receipt_journal::record_count(home.path()), 1);
    store
        .finish_job(
            &job,
            ExitStatus {
                code: Some(0),
                cause: "exited".into(),
            },
            true,
            CleanupState::Verified,
            TimingReport::default(),
        )
        .unwrap();
    assert_eq!(receipt_journal::record_count(home.path()), 2);
}

#[test]
fn receipt_retention_compacts_with_hysteresis() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    let template = store.job(&job).unwrap();
    let mut receipts = BTreeMap::new();
    for cursor in 1..=receipt_journal::MAX_RETAINED_RECEIPTS as u64 + 1 {
        let mut receipt = template.clone();
        receipt.cursor = cursor;
        receipt.job_id = format!("job-{cursor}");
        receipt.state = JobState::Finished;
        receipts.insert(receipt.job_id.clone(), receipt);
    }
    assert!(receipt_journal::trim_receipts(&mut receipts));
    assert_eq!(receipts.len(), 900);
    assert_eq!(receipts.values().map(|value| value.cursor).min(), Some(102));
}

#[test]
fn receipt_retention_compacts_an_overfull_journal_on_restart() {
    let home = home();
    let template = {
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        let mut receipt = store.job(&job).unwrap();
        receipt.state = JobState::Finished;
        receipt.exit = Some(ExitStatus {
            code: Some(0),
            cause: "exited".into(),
        });
        receipt.cleanup = CleanupState::Verified;
        receipt.output_complete = true;
        receipt.finished_unix_ms = Some(receipt.created_unix_ms + 1);
        receipt
    };
    let receipts: Vec<_> = (1..=receipt_journal::MAX_RETAINED_RECEIPTS as u64 + 1)
        .map(|cursor| {
            let mut receipt = template.clone();
            receipt.cursor = cursor;
            receipt.job_id = Uuid::new_v4().to_string();
            receipt.attempt_id = Uuid::new_v4().to_string();
            receipt
        })
        .collect();
    receipt_journal::replace_uncompacted(home.path(), &receipts);

    let reopened = DaemonStore::open(home.path()).unwrap();
    assert_eq!(reopened.jobs().jobs.len(), 900);
    assert_eq!(receipt_journal::record_count(home.path()), 900);
}

#[test]
fn receipt_journal_compacts_and_replays_a_reserved_queued_receipt() {
    let home = home();
    let store = DaemonStore::new(home.path());
    let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let scope = store.status(None).scope_id;
    store
        .register_worker(WorkerStatus {
            worker_id: "worker-reserved-compact".into(),
            vm_id: "vm-reserved-compact".into(),
            scope_id: scope,
            kit_ref: "fixture".into(),
            kits: Vec::new(),
            warm: true,
            health: WorkerHealth::Ready,
            container_capacity: 1,
            active_container_ids: vec![],
        })
        .unwrap();
    let (job, _) = store
        .begin_job(NewJob {
            session_id: session,
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    store
        .reserve_worker(&job, "worker-reserved-compact", "vm-reserved-compact")
        .unwrap();
    {
        let mut state = store.lock();
        let receipts = state.jobs.clone();
        state.journal.compact(&receipts).unwrap();
    }
    drop(store);

    let reopened = DaemonStore::open(home.path()).unwrap();
    let receipt = reopened.job(&job).unwrap();
    assert_eq!(receipt.state, JobState::Unknown);
    assert_eq!(
        receipt.worker_id.as_deref(),
        Some("worker-reserved-compact")
    );
    assert_eq!(receipt.vm_id.as_deref(), Some("vm-reserved-compact"));
    assert_eq!(receipt.container_id, None);
    assert!(!reopened.worker_reuse_blocked("vm-reserved-compact"));
    assert_eq!(receipt_journal::record_count(home.path()), 1);
}

#[test]
fn receipt_journal_rejects_a_half_assigned_queued_receipt() {
    let home = home();
    let mut receipt = {
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        store.job(&job).unwrap()
    };
    receipt.worker_id = Some("worker-without-vm".into());
    receipt_journal::replace_uncompacted(home.path(), &[receipt]);

    assert!(matches!(
        DaemonStore::open(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("state evidence")
    ));
}

#[test]
fn receipt_journal_truncates_only_a_corrupt_tail() {
    let home = home();
    let job = {
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap()
            .0
    };
    let journal = receipt_journal::journal_path(home.path());
    let good_length = fs::metadata(&journal).unwrap().len();
    OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"\0\0")
        .unwrap();

    let reopened = DaemonStore::open(home.path()).unwrap();
    assert_eq!(reopened.job(&job).unwrap().state, JobState::Unknown);
    assert!(fs::metadata(journal).unwrap().len() >= good_length);
}

#[test]
fn receipt_journal_fails_closed_on_interior_corruption() {
    let home = home();
    {
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        for command in ["first", "second"] {
            store
                .begin_job(NewJob {
                    session_id: session.clone(),
                    command: command.into(),
                    kit_ref: "fixture".into(),
                    workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                    mounts: Vec::new(),
                })
                .unwrap();
        }
    }
    let journal = receipt_journal::journal_path(home.path());
    let mut bytes = fs::read(&journal).unwrap();
    bytes[8] ^= 1;
    fs::write(&journal, bytes).unwrap();
    assert!(matches!(
        DaemonStore::open(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("corrupt")
    ));
}

#[test]
fn receipt_journal_fails_closed_on_complete_corrupt_final_record() {
    let home = home();
    {
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        store
            .begin_job(NewJob {
                session_id: session,
                command: "only-record".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
    }
    let journal = receipt_journal::journal_path(home.path());
    let mut bytes = fs::read(&journal).unwrap();
    let last = bytes.last_mut().expect("record checksum");
    *last ^= 1;
    fs::write(&journal, bytes).unwrap();

    assert!(matches!(
        DaemonStore::open(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("checksum mismatch")
    ));
}

#[test]
fn receipt_journal_rejects_inconsistent_terminal_state_evidence() {
    let home = home();
    let mut receipt = {
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        store.job(&job).unwrap()
    };
    receipt.state = JobState::Finished;
    receipt_journal::replace_uncompacted(home.path(), &[receipt]);

    assert!(matches!(
        DaemonStore::open(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("state evidence")
    ));
}

#[test]
fn receipt_journal_accepts_coherent_terminal_placement_and_cleanup_states() {
    for (worker_id, vm_id, container_id, cleanup, exit_code) in [
        (None, None, None, CleanupState::NotRequired, Some(0)),
        (
            Some("worker-reserved"),
            Some("vm-reserved"),
            None,
            CleanupState::Verified,
            Some(0),
        ),
        (
            Some("worker-started"),
            Some("vm-started"),
            Some("container-started"),
            CleanupState::Verified,
            Some(0),
        ),
        (
            Some("worker-uncertain"),
            Some("vm-uncertain"),
            Some("container-uncertain"),
            CleanupState::Uncertain,
            Some(125),
        ),
        (None, None, None, CleanupState::Uncertain, None),
    ] {
        let home = home();
        let mut receipt = {
            let store = DaemonStore::new(home.path());
            let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
            let (job, _) = store
                .begin_job(NewJob {
                    session_id: session,
                    command: "fixture".into(),
                    kit_ref: "fixture".into(),
                    workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                    mounts: Vec::new(),
                })
                .unwrap();
            store.job(&job).unwrap()
        };
        receipt.state = if cleanup == CleanupState::Uncertain {
            JobState::Unknown
        } else {
            JobState::Finished
        };
        receipt.worker_id = worker_id.map(str::to_owned);
        receipt.vm_id = vm_id.map(str::to_owned);
        receipt.container_id = container_id.map(str::to_owned);
        receipt.exit = Some(ExitStatus {
            code: exit_code,
            cause: if cleanup == CleanupState::Uncertain {
                "cleanup_uncertain".into()
            } else {
                "exited".into()
            },
        });
        receipt.output_complete = true;
        receipt.cleanup = cleanup;
        receipt.finished_unix_ms = Some(receipt.created_unix_ms + 1);
        receipt_journal::replace_uncompacted(home.path(), &[receipt.clone()]);

        let reopened = DaemonStore::open(home.path()).unwrap();
        assert_eq!(reopened.job(&receipt.job_id).unwrap(), receipt);
    }
}

#[test]
fn receipt_journal_rejects_terminal_container_without_worker_placement() {
    let home = home();
    let mut receipt = {
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        store.job(&job).unwrap()
    };
    receipt.state = JobState::Finished;
    receipt.container_id = Some("container-without-placement".into());
    receipt.exit = Some(ExitStatus {
        code: Some(0),
        cause: "exited".into(),
    });
    receipt.output_complete = true;
    receipt.cleanup = CleanupState::Verified;
    receipt.finished_unix_ms = Some(receipt.created_unix_ms + 1);
    receipt_journal::replace_uncompacted(home.path(), &[receipt]);

    assert!(matches!(
        DaemonStore::open(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("state evidence")
    ));
}

#[test]
fn receipt_journal_rejects_success_with_uncertain_cleanup() {
    let home = home();
    let mut receipt = {
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        store.job(&job).unwrap()
    };
    receipt.state = JobState::Unknown;
    receipt.exit = Some(ExitStatus {
        code: Some(0),
        cause: "cleanup_uncertain".into(),
    });
    receipt.output_complete = true;
    receipt.cleanup = CleanupState::Uncertain;
    receipt.finished_unix_ms = Some(receipt.created_unix_ms + 1);
    receipt_journal::replace_uncompacted(home.path(), &[receipt]);

    assert!(matches!(
        DaemonStore::open(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("state evidence")
    ));
}

#[test]
fn attachment_copy_failure_is_terminal() {
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    assert!(write_attachment_bytes(&mut FailingWriter, b"output").is_err());
}

#[test]
fn public_client_authenticates_and_shares_daemon() {
    let home = home();
    let one = home.path().join("one");
    let two = home.path().join("two");
    fs::create_dir(&one).unwrap();
    fs::create_dir(&two).unwrap();
    let server = Arc::new(Server::bind(home.path()).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let first = match client
        .request(PublicRequest::AttachShell {
            pid: 1,
            session: authority(one.to_str().unwrap(), home.path()),
        })
        .unwrap()
    {
        PublicReply::ShellAttached { session_id } => session_id,
        reply => panic!("unexpected reply: {reply:?}"),
    };
    let second = match client
        .request(PublicRequest::AttachShell {
            pid: 2,
            session: authority(two.to_str().unwrap(), home.path()),
        })
        .unwrap()
    {
        PublicReply::ShellAttached { session_id } => session_id,
        reply => panic!("unexpected reply: {reply:?}"),
    };
    let status = match client
        .request(PublicRequest::Status {
            session_id: Some(first.clone()),
        })
        .unwrap()
    {
        PublicReply::Status(status) => status,
        reply => panic!("unexpected reply: {reply:?}"),
    };
    assert_ne!(first, second);
    assert_eq!(status.shells.len(), 2);
    assert!(
        status
            .shells
            .iter()
            .all(|shell| shell.daemon_id == status.daemon_id)
    );
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[test]
fn matching_installed_daemon_build_is_reused() {
    let home = home();
    let sbx = stock_sbx(b"stock-sbx-a");
    let server = Arc::new(Server::bind_with_stock_sbx(home.path(), sbx.path()).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });

    let client =
        Client::ensure_running(home.path(), &std::env::current_exe().unwrap(), sbx.path()).unwrap();
    assert!(matches!(
        client.request(PublicRequest::Ping).unwrap(),
        PublicReply::Pong {
            build_identity: Some(_),
            pid: Some(_),
            ..
        }
    ));
    assert!(!server.shutdown_requested.load(Ordering::Acquire));
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[test]
fn changed_stock_sbx_replaces_an_idle_daemon_and_reuses_the_new_identity() {
    let home = home();
    let old_sbx = stock_sbx(b"stock-sbx-old");
    let new_sbx = stock_sbx(b"stock-sbx-new");
    let old_server = Arc::new(Server::bind_with_stock_sbx(home.path(), old_sbx.path()).unwrap());
    let task_server = Arc::clone(&old_server);
    let old_task = thread::spawn(move || task_server.serve_until(|| false).unwrap());
    drop(old_server);
    let executable = std::env::current_exe().unwrap();
    let expected = runtime_identity(&executable, new_sbx.path()).unwrap();
    let paths = EndpointPaths::for_home(home.path()).unwrap();

    assert!(
        Client::reconcile_resident(home.path(), &paths, &expected)
            .unwrap()
            .is_none()
    );
    old_task.join().unwrap();

    let new_server = Arc::new(Server::bind_with_stock_sbx(home.path(), new_sbx.path()).unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&new_server);
    let task_stop = Arc::clone(&stop);
    let new_task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::reconcile_resident(home.path(), &paths, &expected)
        .unwrap()
        .expect("new matching daemon must be reused");
    let PublicReply::Pong { build_identity, .. } = client.request(PublicRequest::Ping).unwrap()
    else {
        panic!("expected pong")
    };
    assert_eq!(build_identity.as_deref(), Some(expected.as_str()));
    stop.store(true, Ordering::Relaxed);
    new_task.join().unwrap();
}

#[test]
fn resident_replacement_retries_the_bounded_post_probe_admission_race() {
    let home = home();
    let server =
        Arc::new(Server::bind_with_identity(home.path(), "sha256:resident-old".into()).unwrap());
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_until(|| false).unwrap());
    let client = Client::connect(home.path()).unwrap();
    assert!(matches!(
        client.request(PublicRequest::Ping).unwrap(),
        PublicReply::Pong { .. }
    ));

    // Model the narrow tail where Ping has written its reply but its ordinary
    // admission guard has not dropped yet.
    let admission = server.store.begin_ordinary_request().unwrap();
    let (send, receive) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = send.send(client.shutdown_resident_after_probe());
    });
    assert!(matches!(
        receive.recv_timeout(Duration::from_millis(40)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert!(!server.shutdown_requested.load(Ordering::Acquire));

    drop(admission);
    receive
        .recv_timeout(Duration::from_secs(1))
        .expect("bounded retry must observe released Ping admission")
        .unwrap();
    task.join().unwrap();
}

#[test]
fn changed_stock_sbx_refuses_to_replace_a_daemon_with_an_attached_shell() {
    let home = home();
    let old_sbx = stock_sbx(b"stock-sbx-old");
    let new_sbx = stock_sbx(b"stock-sbx-new");
    let server = Arc::new(Server::bind_with_stock_sbx(home.path(), old_sbx.path()).unwrap());
    let session = server
        .store
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let expected = runtime_identity(&std::env::current_exe().unwrap(), new_sbx.path()).unwrap();
    let paths = EndpointPaths::for_home(home.path()).unwrap();

    let started = Instant::now();
    let error = Client::reconcile_resident(home.path(), &paths, &expected).unwrap_err();
    assert!(matches!(
        error,
        DaemonError::Remote(ref message)
            if message == BUSY_LIFECYCLE_MESSAGE
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!server.shutdown_requested.load(Ordering::Acquire));

    server.store.detach_shell(&session).unwrap();
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[test]
fn mismatched_authenticated_build_accepts_safe_replacement_shutdown() {
    let home = home();
    let server =
        Arc::new(Server::bind_with_identity(home.path(), "sha256:resident-old".into()).unwrap());
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_until(|| false).unwrap());
    let client = Client::connect(home.path()).unwrap();
    let expected = artifact_identity(&std::env::current_exe().unwrap()).unwrap();
    let PublicReply::Pong { build_identity, .. } = client.request(PublicRequest::Ping).unwrap()
    else {
        panic!("expected pong")
    };
    assert_ne!(build_identity.as_deref(), Some(expected.as_str()));
    assert_eq!(
        client.request(PublicRequest::Shutdown).unwrap(),
        PublicReply::ShuttingDown
    );
    task.join().unwrap();
    assert!(server.shutdown_requested.load(Ordering::Acquire));
}

#[test]
fn relay_and_unauthenticated_clients_cannot_perform_host_lifecycle_operations() {
    let home = home();
    let server = Arc::new(Server::bind(home.path()).unwrap());
    let attached = server
        .store
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let relay_token = server.store.issue_relay_token(&attached).unwrap();
    let master = Client::connect(home.path()).unwrap();
    let task_server = Arc::clone(&server);
    let busy_task = thread::spawn(move || task_server.serve_one().unwrap());
    assert!(matches!(
        master.request(PublicRequest::Shutdown).unwrap(),
        PublicReply::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    busy_task.join().unwrap();

    let relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: relay_token,
    };
    let task_server = Arc::clone(&server);
    let relay_task = thread::spawn(move || task_server.serve_one().unwrap());
    assert!(matches!(
        relay.request(PublicRequest::Shutdown).unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    relay_task.join().unwrap();

    let relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: server.store.issue_relay_token(&attached).unwrap(),
    };
    let task_server = Arc::clone(&server);
    let relay_task = thread::spawn(move || task_server.serve_one().unwrap());
    assert!(matches!(
        relay
            .request(PublicRequest::ResetWorkers {
                selection: LoadSelection::All,
            })
            .unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    relay_task.join().unwrap();

    let relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: server.store.issue_relay_token(&attached).unwrap(),
    };
    let task_server = Arc::clone(&server);
    let relay_task = thread::spawn(move || task_server.serve_one().unwrap());
    assert!(matches!(
        relay.request(PublicRequest::ResetScope).unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    relay_task.join().unwrap();

    let task_server = Arc::clone(&server);
    let unauthenticated_task = thread::spawn(move || task_server.serve_one().unwrap());
    let mut stream = UnixStream::connect(&server.lifecycle.paths.socket).unwrap();
    write_frame(
        &mut stream,
        &Envelope {
            protocol: PROTOCOL.into(),
            token: "0".repeat(64),
            body: PublicRequest::Shutdown,
        },
    )
    .unwrap();
    assert!(matches!(
        read_frame::<PublicReply>(&mut stream).unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    unauthenticated_task.join().unwrap();
    assert!(!server.shutdown_requested.load(Ordering::Acquire));
}

#[test]
fn legacy_pong_is_compatible_but_ambiguous_owner_is_never_signalable() {
    let old: PublicReply = serde_json::from_str(r#"{"type":"pong","daemon_id":"old"}"#).unwrap();
    assert_eq!(
        old,
        PublicReply::Pong {
            daemon_id: "old".into(),
            build_identity: None,
            pid: None,
        }
    );

    let home = home();
    let server = Arc::new(Server::bind(home.path()).unwrap());
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_until(|| false).unwrap());
    let client = Client::connect(home.path()).unwrap();
    assert!(matches!(
        verified_legacy_owner(&client, &server.lifecycle.paths, "different-daemon"),
        Err(DaemonError::EndpointInconsistent(_))
    ));
    server.shutdown_requested.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
fn wrong_token_fails_without_dispatch() {
    let home = home();
    let server = Arc::new(Server::bind(home.path()).unwrap());
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_one().unwrap());
    let mut stream = UnixStream::connect(&server.lifecycle.paths.socket).unwrap();
    write_frame(
        &mut stream,
        &Envelope {
            protocol: PROTOCOL.into(),
            token: "0".repeat(64),
            body: PublicRequest::AttachShell {
                pid: 4,
                session: authority("/tmp", home.path()),
            },
        },
    )
    .unwrap();
    assert!(matches!(
        read_frame::<PublicReply>(&mut stream).unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    task.join().unwrap();
    assert!(server.store.status(None).shells.is_empty());
}

#[derive(Debug)]
struct EchoBackend;

impl DaemonBackend for EchoBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(vec!["zeta".into(), "fixture".into(), "fixture".into()])
    }

    fn prepare(
        &self,
        selection: &LoadSelection,
        _session: &SessionSpec,
        progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        progress.cold_boot("fixture")?;
        Ok(PreparationResult {
            cold_kits: match selection {
                LoadSelection::All => vec!["fixture".into()],
                LoadSelection::Kits(kits) => kits.clone(),
            },
            sandboxes: BTreeMap::from([("fixture".into(), "fixture-vm".into())]),
        })
    }

    fn execute(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        assert_eq!(request.command, "fixture");
        loop {
            match attachment.receive()? {
                AttachmentFrame::Stdin { bytes } => {
                    attachment.send(&AttachmentFrame::Stdout { bytes })?;
                }
                AttachmentFrame::StdinEof => {
                    attachment.send(&AttachmentFrame::Stderr {
                        bytes: b"finished".to_vec(),
                    })?;
                    attachment.send(&AttachmentFrame::Exited { code: 23 })?;
                    return Ok(());
                }
                other => return Err(DaemonError::InvalidState(format!("unexpected {other:?}"))),
            }
        }
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        attachment.send(&AttachmentFrame::ShellReady)?;
        attachment.send(&AttachmentFrame::Exited { code: 0 })
    }
}

#[derive(Debug)]
struct FailingShellBackend;

impl DaemonBackend for FailingShellBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(Vec::new())
    }

    fn prepare(
        &self,
        _selection: &LoadSelection,
        _session: &SessionSpec,
        _progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        Err(DaemonError::InvalidState("preparation failed".into()))
    }

    fn execute(
        &self,
        _request: ExecuteSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::InvalidState("shell failed".into()))
    }
}

fn session(session_id: &str, home: &Path) -> SessionSpec {
    SessionSpec {
        session_id: session_id.into(),
        username: "example".into(),
        uid: 1000,
        gid: 1000,
        launch_directory: "/Users/example/project".into(),
        guest_home: "/Users/example".into(),
        home_backing: home.into(),
        ephemeral_home: false,
        terminal: false,
        terminal_size: None,
    }
}

#[test]
fn failed_shell_setup_and_preparation_detach_their_sessions() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FailingShellBackend)),
    );
    let shell_session = server
        .store()
        .attach_shell(41, authority("/Users/example/shell", home.path()));
    let prepare_session = server
        .store()
        .attach_shell(42, authority("/Users/example/prepare", home.path()));
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();

    assert!(
        client
            .open_shell(ShellSpec {
                dev: false,
                session: session(&shell_session, home.path()),
                arguments: Vec::new(),
            })
            .is_err()
    );
    assert!(
        client
            .prepare(
                LoadSelection::Kits(Vec::new()),
                session(&prepare_session, home.path()),
            )
            .is_err()
    );
    let status = server.store().status(None);
    assert!(
        status
            .shells
            .iter()
            .filter(|shell| [shell_session.as_str(), prepare_session.as_str()]
                .contains(&shell.session_id.as_str()))
            .all(|shell| shell.state == ShellState::Detached)
    );

    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[test]
fn execution_attachment_preserves_bytes_and_exit_status() {
    let home = home();
    let project = home.path().join("project");
    fs::create_dir(&project).unwrap();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(EchoBackend)),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    assert_eq!(
        client.registered_commands().unwrap(),
        vec!["fixture".to_string(), "zeta".to_string()]
    );
    let session_id = match client
        .request(PublicRequest::AttachShell {
            pid: 7,
            session: authority(project.to_str().unwrap(), home.path()),
        })
        .unwrap()
    {
        PublicReply::ShellAttached { session_id } => session_id,
        reply => panic!("unexpected reply: {reply:?}"),
    };
    let selected = session(&session_id, home.path());
    let mut progress = Vec::new();
    assert_eq!(
        client
            .prepare_with_progress(
                LoadSelection::Kits(vec!["fixture".into()]),
                selected.clone(),
                false,
                &mut progress,
            )
            .unwrap(),
        PreparationResult {
            cold_kits: vec!["fixture".to_string()],
            sandboxes: BTreeMap::from([("fixture".into(), "fixture-vm".into())]),
        }
    );
    assert_eq!(progress, b"[starting fixture worker VM\xe2\x80\xa6]\n");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = client
        .execute_with_io(
            ExecuteSpec {
                command: "fixture".into(),
                arguments: vec![b"invalid:\xff".to_vec()],
                working_directory: None,
                placement: Placement::Local,
                environment: BTreeMap::new(),
                session: selected.clone(),
                process: None,
            },
            Cursor::new(b"input\x00\xff".to_vec()),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
    assert_eq!(status, 23);
    assert_eq!(stdout, b"input\x00\xff");
    assert_eq!(stderr, b"finished");
    assert_eq!(
        client
            .open_shell_with_io(
                ShellSpec {
                    dev: false,
                    arguments: vec![b"-c".to_vec(), b"printf ok".to_vec()],
                    session: selected,
                },
                File::open("/dev/null").unwrap(),
                Vec::new(),
                Vec::new(),
            )
            .unwrap(),
        0
    );
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[derive(Debug)]
struct LifecycleBackend {
    complete: bool,
    calls: Arc<Mutex<Vec<ScopeLifecycleAction>>>,
}

impl DaemonBackend for LifecycleBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(Vec::new())
    }

    fn teardown_scope(
        &self,
        action: ScopeLifecycleAction,
        _store: DaemonStore,
        _deadline: Instant,
    ) -> ScopeLifecycleReport {
        self.calls.lock().unwrap().push(action);
        ScopeLifecycleReport {
            action,
            cleanup_complete: self.complete,
            components: vec![ScopeCleanupComponent {
                kind: "shell".into(),
                label: "project".into(),
                state: if self.complete {
                    ScopeCleanupState::Removed
                } else {
                    ScopeCleanupState::CleanupUncertain
                },
                vm: None,
                detail: (!self.complete).then(|| "cleanup could not be verified".into()),
            }],
        }
    }

    fn prepare(
        &self,
        _selection: &LoadSelection,
        _session: &SessionSpec,
        _progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        unreachable!()
    }

    fn execute(
        &self,
        _request: ExecuteSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }
}

#[test]
fn scope_reset_is_host_authenticated_refuses_active_state_and_keeps_daemon_resident() {
    let home = home();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(Server::bind(home.path()).unwrap().with_backend(Arc::new(
        LifecycleBackend {
            complete: true,
            calls: Arc::clone(&calls),
        },
    )));
    let session = server
        .store
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let client = Client::connect(home.path()).unwrap();
    let task_server = Arc::clone(&server);
    let busy = thread::spawn(move || task_server.serve_one().unwrap());
    assert!(matches!(
        client.request(PublicRequest::ResetScope).unwrap(),
        PublicReply::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    busy.join().unwrap();
    assert!(calls.lock().unwrap().is_empty());

    server.store.detach_shell(&session).unwrap();
    let task_server = Arc::clone(&server);
    let reset = thread::spawn(move || task_server.serve_one().unwrap());
    let report = client.reset_scope().unwrap();
    reset.join().unwrap();
    assert!(report.cleanup_complete);
    assert_eq!(*calls.lock().unwrap(), vec![ScopeLifecycleAction::Reset]);
    assert!(!server.shutdown_requested.load(Ordering::Acquire));
}

#[test]
fn scope_stop_only_terminates_daemon_after_proven_complete_cleanup() {
    for complete in [false, true] {
        let home = home();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let server = Arc::new(Server::bind(home.path()).unwrap().with_backend(Arc::new(
            LifecycleBackend {
                complete,
                calls: Arc::clone(&calls),
            },
        )));
        let client = Client::connect(home.path()).unwrap();
        let task_server = Arc::clone(&server);
        let task = thread::spawn(move || task_server.serve_one().unwrap());
        let PublicReply::ScopeLifecycle(report) = client.request(PublicRequest::StopScope).unwrap()
        else {
            panic!("expected typed scope lifecycle reply");
        };
        task.join().unwrap();
        assert_eq!(report.cleanup_complete, complete);
        assert_eq!(*calls.lock().unwrap(), vec![ScopeLifecycleAction::Stop]);
        assert_eq!(server.shutdown_requested.load(Ordering::Acquire), complete);
    }
}

#[test]
fn connect_if_running_distinguishes_clean_absence_from_inconsistent_endpoint() {
    let home = home();
    assert!(Client::connect_if_running(home.path()).unwrap().is_none());

    let paths = EndpointPaths::for_home(home.path()).unwrap();
    fs::create_dir_all(&paths.runtime_directory).unwrap();
    fs::set_permissions(&paths.runtime_directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&paths.lock, format!("{}\n", std::process::id())).unwrap();
    assert!(matches!(
        Client::connect_if_running(home.path()),
        Err(DaemonError::EndpointInconsistent(path)) if path == paths.runtime_directory
    ));
}

#[test]
fn stop_scope_client_returns_complete_only_after_endpoint_owner_release() {
    let home = home();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let server = Server::bind(home.path())
        .unwrap()
        .with_backend(Arc::new(LifecycleBackend {
            complete: true,
            calls: Arc::clone(&calls),
        }));
    let task = thread::spawn(move || server.serve_until(|| false).unwrap());
    let client = Client::connect(home.path()).unwrap();

    let report = client.stop_scope().unwrap();

    assert!(report.cleanup_complete);
    task.join().unwrap();
    assert!(Client::connect_if_running(home.path()).unwrap().is_none());
    assert_eq!(*calls.lock().unwrap(), vec![ScopeLifecycleAction::Stop]);
}

#[derive(Debug)]
struct BlockingConnectionBackend;

impl DaemonBackend for BlockingConnectionBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(vec!["fixture".into()])
    }

    fn prepare(
        &self,
        _selection: &LoadSelection,
        _session: &SessionSpec,
        _progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        Ok(PreparationResult::default())
    }

    fn execute(
        &self,
        _request: ExecuteSpec,
        attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        hold_attachment(&attachment)
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        hold_attachment(&attachment)
    }
}

fn hold_attachment(attachment: &ServerAttachment) -> Result<(), DaemonError> {
    loop {
        match attachment.receive()? {
            AttachmentFrame::StdinEof => {
                return attachment.send(&AttachmentFrame::Exited { code: 0 });
            }
            AttachmentFrame::Stdin { .. } | AttachmentFrame::Resize { .. } => {}
            frame => {
                return Err(DaemonError::InvalidState(format!(
                    "unexpected blocking test frame: {frame:?}"
                )));
            }
        }
    }
}

fn assert_lifecycle_refused_and_status_responsive(client: &Client, request: PublicRequest) {
    let started = Instant::now();
    assert!(matches!(
        client.request(request).unwrap(),
        PublicReply::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    assert!(started.elapsed() < Duration::from_secs(1));

    let started = Instant::now();
    assert!(matches!(
        client
            .request(PublicRequest::Status { session_id: None })
            .unwrap(),
        PublicReply::Status(_)
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn attached_open_shell_refuses_lifecycle_immediately_without_blocking_status() {
    let home = home();
    let project = home.path().join("project");
    fs::create_dir(&project).unwrap();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(BlockingConnectionBackend)),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let session_id = match client
        .request(PublicRequest::AttachShell {
            pid: 17,
            session: authority(project.to_str().unwrap(), home.path()),
        })
        .unwrap()
    {
        PublicReply::ShellAttached { session_id } => session_id,
        reply => panic!("unexpected attach reply: {reply:?}"),
    };
    let shell = client
        .start_shell(ShellSpec {
            dev: false,
            arguments: Vec::new(),
            session: session(&session_id, home.path()),
        })
        .unwrap();

    for request in [
        PublicRequest::ResetScope,
        PublicRequest::StopScope,
        PublicRequest::Shutdown,
    ] {
        assert_lifecycle_refused_and_status_responsive(&client, request);
    }

    shell.send(&AttachmentFrame::StdinEof).unwrap();
    assert_eq!(
        shell.receive().unwrap(),
        AttachmentFrame::Exited { code: 0 }
    );
    assert!(matches!(
        client
            .request(PublicRequest::DetachShell { session_id })
            .unwrap(),
        PublicReply::Detached
    ));
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[test]
fn active_execute_refuses_lifecycle_immediately_without_blocking_status() {
    let home = home();
    let project = home.path().join("project");
    fs::create_dir(&project).unwrap();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(BlockingConnectionBackend)),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let session_id = match client
        .request(PublicRequest::AttachShell {
            pid: 18,
            session: authority(project.to_str().unwrap(), home.path()),
        })
        .unwrap()
    {
        PublicReply::ShellAttached { session_id } => session_id,
        reply => panic!("unexpected attach reply: {reply:?}"),
    };
    let execution = client
        .start_execution(ExecuteSpec {
            working_directory: None,
            command: "fixture".into(),
            arguments: Vec::new(),
            placement: Placement::Local,
            environment: BTreeMap::new(),
            session: session(&session_id, home.path()),
            process: None,
        })
        .unwrap();
    assert!(matches!(
        client
            .request(PublicRequest::DetachShell { session_id })
            .unwrap(),
        PublicReply::Detached
    ));
    let admission_deadline = Instant::now() + Duration::from_secs(1);
    while server.store.lock().ordinary_requests != 1 && Instant::now() < admission_deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.store.lock().ordinary_requests, 1);

    for request in [
        PublicRequest::ResetScope,
        PublicRequest::StopScope,
        PublicRequest::Shutdown,
    ] {
        assert_lifecycle_refused_and_status_responsive(&client, request);
    }

    execution.send(&AttachmentFrame::StdinEof).unwrap();
    assert_eq!(
        execution.receive().unwrap(),
        AttachmentFrame::Exited { code: 0 }
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    while server.store.lock().ordinary_requests != 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(server.store.lock().ordinary_requests, 0);
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[derive(Debug)]
struct BlockingLifecycleBackend {
    gate: Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
}

#[derive(Debug)]
struct DeadlineAwareMultiComponentBackend;

impl DaemonBackend for DeadlineAwareMultiComponentBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(Vec::new())
    }

    fn teardown_scope(
        &self,
        action: ScopeLifecycleAction,
        _store: DaemonStore,
        deadline: Instant,
    ) -> ScopeLifecycleReport {
        // Model a first stock-SBX component that consumes the whole remaining
        // budget. Later components must be reported uncertain without effects.
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        ScopeLifecycleReport {
            action,
            cleanup_complete: false,
            components: ["kit-a", "kit-b", "shell"]
                .into_iter()
                .map(|label| ScopeCleanupComponent {
                    kind: if label == "shell" { "shell" } else { "kit" }.into(),
                    label: label.into(),
                    state: ScopeCleanupState::CleanupUncertain,
                    vm: None,
                    detail: Some("scope lifecycle deadline expired".into()),
                })
                .collect(),
        }
    }

    fn prepare(
        &self,
        _selection: &LoadSelection,
        _session: &SessionSpec,
        _progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        unreachable!()
    }

    fn execute(
        &self,
        _request: ExecuteSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }
}

#[test]
fn aggregate_lifecycle_deadline_returns_coherent_report_and_releases_gate() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(DeadlineAwareMultiComponentBackend))
            .with_scope_lifecycle_timeout(Duration::from_millis(35)),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let started = Instant::now();
    let report = client.reset_scope().unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!report.cleanup_complete);
    assert_eq!(report.components.len(), 3);
    assert!(report.components.iter().all(|component| {
        component.state == ScopeCleanupState::CleanupUncertain
            && component.detail.as_deref() == Some("scope lifecycle deadline expired")
    }));
    assert!(matches!(
        client
            .request(PublicRequest::Status { session_id: None })
            .unwrap(),
        PublicReply::Status(_)
    ));
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

impl DaemonBackend for BlockingLifecycleBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(Vec::new())
    }

    fn teardown_scope(
        &self,
        action: ScopeLifecycleAction,
        _store: DaemonStore,
        _deadline: Instant,
    ) -> ScopeLifecycleReport {
        let (state, changed) = &*self.gate;
        let mut state = state.lock().unwrap();
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).unwrap();
        }
        ScopeLifecycleReport {
            action,
            cleanup_complete: true,
            components: Vec::new(),
        }
    }

    fn prepare(
        &self,
        _selection: &LoadSelection,
        _session: &SessionSpec,
        _progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        unreachable!()
    }

    fn execute(
        &self,
        _request: ExecuteSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }
}

#[test]
fn admitted_lifecycle_blocks_new_work_only_until_teardown_finishes() {
    let home = home();
    let gate = Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
    let server = Arc::new(Server::bind(home.path()).unwrap().with_backend(Arc::new(
        BlockingLifecycleBackend {
            gate: Arc::clone(&gate),
        },
    )));
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let reset_client = client.clone();
    let reset = thread::spawn(move || reset_client.request(PublicRequest::ResetScope).unwrap());
    {
        let (state, changed) = &*gate;
        let mut state = state.lock().unwrap();
        while !state.0 {
            state = changed.wait(state).unwrap();
        }
    }

    let started = Instant::now();
    assert!(matches!(
        client
            .request(PublicRequest::Status { session_id: None })
            .unwrap(),
        PublicReply::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    assert!(started.elapsed() < Duration::from_secs(1));

    {
        let (state, changed) = &*gate;
        let mut state = state.lock().unwrap();
        state.1 = true;
        changed.notify_all();
    }
    assert!(matches!(
        reset.join().unwrap(),
        PublicReply::ScopeLifecycle(_)
    ));
    assert!(matches!(
        client
            .request(PublicRequest::Status { session_id: None })
            .unwrap(),
        PublicReply::Status(_)
    ));
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[test]
fn scope_lifecycle_client_has_an_absolute_deadline_and_server_recovers_after_teardown() {
    let home = home();
    let gate = Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
    let server = Arc::new(Server::bind(home.path()).unwrap().with_backend(Arc::new(
        BlockingLifecycleBackend {
            gate: Arc::clone(&gate),
        },
    )));
    let stop = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();

    let started = Instant::now();
    let error = client
        .scope_lifecycle_with_timeout(PublicRequest::ResetScope, Duration::from_millis(25))
        .unwrap_err();
    assert!(matches!(
        error,
        DaemonError::Io(ref error) if error.kind() == io::ErrorKind::TimedOut
    ));
    assert!(started.elapsed() < Duration::from_secs(1));

    {
        let (state, changed) = &*gate;
        let mut state = state.lock().unwrap();
        assert!(
            state.0,
            "teardown must have begun before the client timed out"
        );
        state.1 = true;
        changed.notify_all();
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match client.request(PublicRequest::Status { session_id: None }) {
            Ok(PublicReply::Status(_)) => break,
            Ok(PublicReply::Error { .. }) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            result => panic!("daemon did not recover after bounded client timeout: {result:?}"),
        }
    }
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}
#[test]
fn internal_attachment_pair_keeps_kit_bytes_and_control_separate() {
    let (server, client) = ServerAttachment::pair().unwrap();
    client
        .send(&AttachmentFrame::Stdin {
            bytes: b"{\"jsonrpc\":\"2.0\"}\n".to_vec(),
        })
        .unwrap();
    assert!(matches!(
        server.receive().unwrap(),
        AttachmentFrame::Stdin { bytes } if bytes == b"{\"jsonrpc\":\"2.0\"}\n"
    ));
    server
        .send(&AttachmentFrame::Stdout {
            bytes: b"response\n".to_vec(),
        })
        .unwrap();
    assert!(matches!(
        client.receive().unwrap(),
        AttachmentFrame::Stdout { bytes } if bytes == b"response\n"
    ));
}

#[derive(Debug)]
struct FakeAcpKit;

impl DaemonBackend for FakeAcpKit {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(vec!["agent-kit".into()])
    }

    fn resolve_acp_agent(
        &self,
        name: &str,
    ) -> Result<marsh_acp::AgentAdapterDeclaration, DaemonError> {
        if name != "agent-session" {
            return Err(DaemonError::NotFound(name.into()));
        }
        Ok(marsh_acp::AgentAdapterDeclaration {
            schema_version: 1,
            name: name.into(),
            protocol: marsh_acp::AgentProtocol::AcpV1,
            command: "agent-kit".into(),
            workload_digest: "test-kit-identity".into(),
            required_capabilities: vec![],
            arguments: vec![],
            description: None,
        })
    }

    fn prepare(
        &self,
        _selection: &LoadSelection,
        _session: &SessionSpec,
        _progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        Ok(PreparationResult::default())
    }

    fn execute(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError> {
        assert_eq!(request.command, "agent-kit");
        let (job_id, _) = store.begin_job(NewJob {
            session_id: request.session.session_id,
            command: request.command,
            kit_ref: "kit:fake-acp".into(),
            workload_image: "fake-acp@sha256:1".into(),
            mounts: Vec::new(),
        })?;
        attachment.send(&AttachmentFrame::JobStarted {
            job_id: job_id.clone(),
        })?;
        let mut pending = Vec::new();
        let mut held = None;
        let mut permission_prompt = None;
        loop {
            match attachment.receive()? {
                AttachmentFrame::Stdin { bytes } => {
                    pending.extend(bytes);
                    while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = pending.drain(..=end).collect();
                        let request: serde_json::Value = serde_json::from_slice(&line)?;
                        let method = request["method"].as_str().unwrap_or_default();
                        let id = request["id"].clone();
                        let response = match method {
                            "initialize" => Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}})),
                            "session/new" => Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"remote-session"}})),
                            "session/prompt" => {
                                let update = serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"remote-session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}}});
                                let mut bytes = serde_json::to_vec(&update)?;
                                bytes.push(b'\n');
                                attachment.send(&AttachmentFrame::Stdout { bytes })?;
                                if request["params"]["prompt"][0]["text"] == "hold" {
                                    held = Some(id);
                                    None
                                } else if request["params"]["prompt"][0]["text"] == "permission" {
                                    permission_prompt = Some(id);
                                    let mut bytes = serde_json::to_vec(&serde_json::json!({
                                        "jsonrpc":"2.0", "id":999, "method":"session/request_permission",
                                        "params": {
                                            "sessionId":"remote-session",
                                            "toolCall": {"toolCallId":"tool-1", "title":"Run command", "rawInput":{"command":"echo permission"}},
                                            "options": [
                                                {"optionId":"once", "name":"Allow once", "kind":"allow_once"},
                                                {"optionId":"always", "name":"Allow always", "kind":"allow_always"},
                                                {"optionId":"deny", "name":"Deny once", "kind":"reject_once"}
                                            ]
                                        }
                                    }))?;
                                    bytes.push(b'\n');
                                    attachment.send(&AttachmentFrame::Stdout { bytes })?;
                                    None
                                } else {
                                    if let Some(path) = request["params"]["prompt"][0]["text"].as_str()
                                        .and_then(|text| text.strip_prefix("finish-on-file:"))
                                    {
                                        let deadline = Instant::now() + Duration::from_secs(3);
                                        while !std::path::Path::new(path).exists() {
                                            assert!(Instant::now() < deadline, "test did not release natural completion");
                                            thread::sleep(Duration::from_millis(5));
                                        }
                                    }
                                    Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}))
                                }
                            }
                            "session/cancel" => held.take().map(|id| serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"cancelled"}})),
                            "" if id == 999 => permission_prompt.take().map(|prompt_id| {
                                let allowed = request["result"]["outcome"]["outcome"] == "selected"
                                    && request["result"]["outcome"]["optionId"] == "once";
                                let reason = if allowed { "end_turn" } else { "refusal" };
                                serde_json::json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":reason}})
                            }),
                            _ => None,
                        };
                        if let Some(response) = response {
                            let mut bytes = serde_json::to_vec(&response)?;
                            bytes.push(b'\n');
                            attachment.send(&AttachmentFrame::Stdout { bytes })?;
                        }
                    }
                }
                AttachmentFrame::Signal { signal } if signal == "terminate" || signal == "kill" => {
                    store.finish_job(
                        &job_id,
                        ExitStatus {
                            code: Some(0),
                            cause: "terminated".into(),
                        },
                        true,
                        CleanupState::Verified,
                        TimingReport::default(),
                    )?;
                    attachment.send(&AttachmentFrame::Exited { code: 0 })?;
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        Ok(())
    }
}

fn acp_handoff_during_admission(
    server: &Arc<Server>,
    id: &str,
    first: &SessionSpec,
    second: &SessionSpec,
    action: impl FnOnce(Box<dyn FnOnce() + '_>),
) -> (Result<(), DaemonError>, Result<(), DaemonError>) {
    thread::scope(|scope| {
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let acp = Arc::clone(&server.acp);
        let handoff_id = id.to_owned();
        let handoff_first = first.clone();
        let handoff_second = second.clone();
        let handoff = scope.spawn(move || {
            start_rx.recv().unwrap();
            attempt_tx.send(()).unwrap();
            let release = acp.release(&handoff_id, &handoff_first);
            let attach = acp.attach(&handoff_id, &handoff_second);
            done_tx.send((release, attach)).unwrap();
        });
        action(Box::new(|| {
            start_tx.send(()).unwrap();
            attempt_rx.recv().unwrap();
            assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        }));
        let outcome = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        handoff.join().unwrap();
        outcome
    })
}

#[test]
fn acp_prompt_retry_after_lost_reply_keeps_one_provider_turn() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let shell_id = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let shell = session(&shell_id, home.path());
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let (id, _, lease) = client
        .acp_start("agent-session".into(), shell.clone())
        .unwrap();
    let key = Uuid::new_v4().to_string();
    let mut lost_reply = UnixStream::connect(&client.paths.socket).unwrap();
    write_frame(
        &mut lost_reply,
        &Envelope {
            protocol: PROTOCOL.into(),
            token: client.token.clone(),
            body: PublicRequest::AcpPrompt {
                agent_session_id: id.clone(),
                session: shell.clone(),
                operation_id: key.clone(),
                text: "once".into(),
            },
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let initial = loop {
        let status = client.acp_status(id.clone(), shell.clone(), 0).unwrap();
        if status.last_stop_reason == Some(marsh_acp::StopReason::EndTurn) {
            break status;
        }
        assert!(Instant::now() < deadline, "lost-reply turn did not finish");
        thread::sleep(Duration::from_millis(5));
    };
    // The daemon has admitted and finished the turn. Discard the reply
    // without reading it, as a caller whose acknowledgment was lost would.
    drop(lost_reply);
    assert_eq!(initial.updates.len(), 1);
    let first_retry = client
        .acp_prompt_with_key(id.clone(), shell.clone(), key.clone(), "once".into())
        .unwrap();
    let second_retry = client
        .acp_prompt_with_key(id.clone(), shell.clone(), key.clone(), "once".into())
        .unwrap();
    assert_eq!(first_retry, second_retry);
    assert!(
        client
            .acp_prompt_with_key(id.clone(), shell.clone(), key, "changed".into())
            .is_err(),
        "one key cannot authorize different prompt content"
    );
    let after = client.acp_status(id.clone(), shell.clone(), 0).unwrap();
    assert_eq!(
        after.updates, initial.updates,
        "retry sent a second ACP turn"
    );
    client.acp_stop(id, shell).unwrap();
    drop(lease);
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
#[allow(clippy::too_many_lines)] // One real socket journey proves grant, lease, retry, and revocation together.
fn acp_published_grant_survives_parent_lease_and_revokes_cached_call() {
    let home = home();
    let project = home.path().join("project");
    fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let owner_id = server
        .store()
        .attach_shell(1, authority(project.to_str().unwrap(), home.path()));
    let observer_id = server
        .store()
        .attach_shell(2, authority(project.to_str().unwrap(), home.path()));
    let owner = SessionSpec {
        launch_directory: project.clone(),
        ..session(&owner_id, home.path())
    };
    let observer = SessionSpec {
        launch_directory: project,
        ..session(&observer_id, home.path())
    };
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let (agent_id, _, lease) = client
        .acp_start("agent-session".into(), owner.clone())
        .unwrap();
    let generation = server.acp.publish(&agent_id, &owner, "shared").unwrap();
    assert!(server.acp.unpublish("shared", &observer, None).is_err());
    assert!(
        client
            .acp_prompt(agent_id.clone(), owner.clone(), "steal".into())
            .is_err()
    );
    let mut relay = client.clone();
    relay.token = server.store.issue_relay_token(&owner_id).unwrap();
    assert!(
        relay
            .acp_published_status(agent_id.clone(), generation.clone(), 0)
            .is_err()
    );
    let key = Uuid::new_v4().to_string();
    let turn_id = client
        .acp_published_prompt(
            agent_id.clone(),
            generation.clone(),
            key.clone(),
            "hello".into(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = client
            .acp_published_status(agent_id.clone(), generation.clone(), 0)
            .unwrap();
        if status.last_stop_reason == Some(marsh_acp::StopReason::EndTurn) {
            assert_eq!(status.updates.len(), 1);
            assert!(status.controller_shell_session_id.is_none());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "published ACP turn did not finish"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        client
            .acp_published_prompt(
                agent_id.clone(),
                generation.clone(),
                key.clone(),
                "hello".into()
            )
            .unwrap(),
        turn_id
    );
    assert!(
        client
            .acp_published_prompt(agent_id.clone(), generation.clone(), key, "changed".into())
            .is_err()
    );
    drop(lease);
    thread::sleep(Duration::from_millis(50));
    assert!(
        !client
            .acp_published_status(agent_id.clone(), generation.clone(), 0)
            .unwrap()
            .stopping
    );
    server
        .acp
        .unpublish("shared", &owner, Some(&generation))
        .unwrap();
    assert!(
        client
            .acp_published_status(agent_id.clone(), generation.clone(), 0)
            .is_err()
    );
    assert!(
        client
            .acp_published_prompt(
                agent_id.clone(),
                generation.clone(),
                Uuid::new_v4().to_string(),
                "stale".into()
            )
            .is_err()
    );
    let newer = server.acp.publish(&agent_id, &owner, "shared").unwrap();
    assert_ne!(newer, generation);
    server
        .acp
        .rollback_publication(&agent_id, &generation, &owner.session_id)
        .unwrap();
    assert!(
        client
            .acp_published_status(agent_id.clone(), newer.clone(), 0)
            .is_ok(),
        "late rollback revoked a newer grant"
    );
    assert!(
        client
            .acp_published_status(agent_id.clone(), generation, 0)
            .is_err()
    );
    server
        .acp
        .unpublish("shared", &owner, Some(&newer))
        .unwrap();
    client.acp_stop(agent_id, owner).unwrap();
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
fn acp_cancel_racing_natural_completion_keeps_session_usable() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let shell_id = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let shell = session(&shell_id, home.path());
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let (id, _, lease) = client
        .acp_start("agent-session".into(), shell.clone())
        .unwrap();
    let finish = home.path().join("allow-natural-completion");
    client
        .acp_prompt(
            id.clone(),
            shell.clone(),
            format!("finish-on-file:{}", finish.display()),
        )
        .unwrap();
    let phase = server.acp.cancel_after_reservation(&id, &shell, || {
        // No timing window: the fixture cannot finish before this cancel has
        // reserved the turn, then it completes before dispatch is attempted.
        fs::write(&finish, b"").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = client.acp_status(id.clone(), shell.clone(), 0).unwrap();
            if !status.turn_active {
                assert_eq!(
                    status.last_stop_reason,
                    Some(marsh_acp::StopReason::EndTurn)
                );
                break;
            }
            assert!(Instant::now() < deadline, "natural completion stalled");
            thread::sleep(Duration::from_millis(5));
        }
    });
    client
        .acp_prompt(id.clone(), shell.clone(), "same session next turn".into())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = client.acp_status(id.clone(), shell.clone(), 0).unwrap();
        if !status.turn_active {
            assert_eq!(
                status.last_stop_reason,
                Some(marsh_acp::StopReason::EndTurn)
            );
            assert_eq!(status.turns.len(), 2);
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    client.acp_stop(id, shell).unwrap();
    drop(lease);
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
    assert_eq!(phase.unwrap(), "AlreadyFinished");
}

#[test]
fn acp_handoff_waits_for_release_and_prompt_admission() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let first = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let second = server
        .store()
        .attach_shell(2, authority("/Users/example/project", home.path()));
    let first = session(&first, home.path());
    let second = session(&second, home.path());
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let (id, _, lease) = client
        .acp_start("agent-session".into(), first.clone())
        .unwrap();

    // A second release and handoff may begin after the first authorization,
    // but they cannot finish before that release clears its own lease.
    let (stale_release, attach) =
        acp_handoff_during_admission(&server, &id, &first, &second, |hook| {
            server
                .acp
                .release_after_authorization(&id, &first, hook)
                .unwrap();
        });
    assert!(stale_release.is_err());
    attach.unwrap();
    assert_eq!(
        server
            .acp
            .status(&id, &second, 0, &server.store())
            .unwrap()
            .controller_shell_session_id
            .as_deref(),
        Some(second.session_id.as_str()),
    );

    server.acp.release(&id, &second).unwrap();
    server.acp.attach(&id, &first).unwrap();
    // The prompt is admitted under the first controller before the second
    // shell can take over. A stale check followed by an unlocked enqueue
    // would let the handoff finish inside the hook.
    let (release, attach) = acp_handoff_during_admission(&server, &id, &first, &second, |hook| {
        server
            .acp
            .prompt_after_authorization(
                &id,
                &first,
                &Uuid::new_v4().to_string(),
                "admitted".into(),
                hook,
            )
            .unwrap();
    });
    release.unwrap();
    attach.unwrap();
    let status = server.acp.status(&id, &second, 0, &server.store()).unwrap();
    assert!(status.turn_active || status.last_stop_reason.is_some());
    assert_eq!(
        status.controller_shell_session_id.as_deref(),
        Some(second.session_id.as_str()),
    );

    server.acp.stop(&id, &second).unwrap();
    drop(lease);
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
#[allow(clippy::too_many_lines)] // Exercises the complete daemon caller lifecycle in one fixture.
fn acp_daemon_caller_has_distinct_identity_controller_and_stop() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let first = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let second = server
        .store()
        .attach_shell(2, authority("/Users/example/project", home.path()));
    let other = server
        .store()
        .attach_shell(3, authority("/Users/example/other", home.path()));
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let (agent_id, job_id, lease) = client
        .acp_start("agent-session".into(), session(&first, home.path()))
        .unwrap();
    assert_ne!(agent_id, first);
    assert_ne!(agent_id, "remote-session");
    assert_ne!(job_id, agent_id);
    assert_eq!(
        client.acp_list(session(&first, home.path())).unwrap().len(),
        1
    );
    assert!(
        client
            .acp_status(agent_id.clone(), session(&other, home.path()), 0)
            .is_err()
    );
    assert!(
        client
            .acp_status("forged-agent-id".into(), session(&first, home.path()), 0)
            .is_err()
    );
    assert!(
        client
            .acp_attach(agent_id.clone(), session(&second, home.path()))
            .is_err()
    );
    client
        .acp_prompt(
            agent_id.clone(),
            session(&first, home.path()),
            "hello".into(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while client
        .acp_status(agent_id.clone(), session(&first, home.path()), 0)
        .unwrap()
        .last_stop_reason
        .is_none()
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let first_turn = client
        .acp_status(agent_id.clone(), session(&first, home.path()), 0)
        .unwrap();
    assert!(
        !first_turn.updates.is_empty(),
        "completed turn must expose every queued update before its stop reason"
    );
    let cursor = first_turn.next_cursor;
    client
        .acp_prompt(
            agent_id.clone(),
            session(&first, home.path()),
            "hold".into(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while client
        .acp_status(agent_id.clone(), session(&first, home.path()), cursor)
        .unwrap()
        .updates
        .is_empty()
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        client
            .acp_cancel(agent_id.clone(), session(&first, home.path()))
            .unwrap(),
        "Dispatched"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while client
        .acp_status(agent_id.clone(), session(&first, home.path()), cursor)
        .unwrap()
        .last_stop_reason
        != Some(marsh_acp::StopReason::Cancelled)
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    client
        .acp_release(agent_id.clone(), session(&first, home.path()))
        .unwrap();
    client
        .acp_attach(agent_id.clone(), session(&second, home.path()))
        .unwrap();
    client
        .acp_stop(agent_id.clone(), session(&second, home.path()))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while client
        .acp_status(agent_id.clone(), session(&second, home.path()), 0)
        .unwrap()
        .attachment
        .terminal
        .is_none()
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    drop(lease);
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "One real daemon caller permission lifecycle"
)]
fn acp_permission_requires_controller_and_offered_one_time_choice() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let first = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let second = server
        .store()
        .attach_shell(2, authority("/Users/example/project", home.path()));
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let (id, _, lease) = client
        .acp_start("agent-session".into(), session(&first, home.path()))
        .unwrap();
    client
        .acp_prompt(
            id.clone(),
            session(&first, home.path()),
            "permission".into(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let request = loop {
        let status = client
            .acp_status(id.clone(), session(&first, home.path()), 0)
            .unwrap();
        if let Some(permission) = status.permissions.first() {
            break permission.clone();
        }
        assert!(
            Instant::now() < deadline,
            "permission request was not surfaced"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(request.tool_call_id, "tool-1");
    assert_eq!(
        request.options.len(),
        2,
        "persistent grant must not be offered"
    );
    assert!(
        client
            .acp_status(id.clone(), session(&second, home.path()), 0)
            .unwrap()
            .permissions
            .is_empty(),
        "another same-project shell cannot inspect pending permission input"
    );
    assert!(
        client
            .acp_respond(
                id.clone(),
                session(&second, home.path()),
                request.request_id.clone(),
                "once".into(),
            )
            .is_err(),
        "another shell cannot approve"
    );
    assert!(
        client
            .acp_respond(
                id.clone(),
                session(&first, home.path()),
                request.request_id.clone(),
                "always".into(),
            )
            .is_err(),
        "persistent option is not allowed"
    );
    client
        .acp_respond(
            id.clone(),
            session(&first, home.path()),
            request.request_id,
            "once".into(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = client
            .acp_status(id.clone(), session(&first, home.path()), 0)
            .unwrap();
        if status.last_stop_reason == Some(marsh_acp::StopReason::EndTurn) {
            assert!(status.permissions.is_empty());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "approved ACP turn did not finish"
        );
        thread::sleep(Duration::from_millis(5));
    }
    client.acp_stop(id, session(&first, home.path())).unwrap();
    drop(lease);
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
fn acp_run_lease_child() {
    let Ok(home) = std::env::var("MARSH_TEST_ACP_LEASE_HOME") else {
        return;
    };
    let shell = std::env::var("MARSH_TEST_ACP_LEASE_SHELL").unwrap();
    let marker = std::env::var("MARSH_TEST_ACP_LEASE_MARKER").unwrap();
    if let Ok(ready) = std::env::var("MARSH_TEST_ACP_LEASE_READY") {
        fs::write(ready, b"starting").unwrap();
    }
    let client = Client::connect(Path::new(&home)).unwrap();
    let (id, _, _lease) = client
        .acp_start("agent-session".into(), session(&shell, Path::new(&home)))
        .unwrap();
    fs::write(marker, id).unwrap();
    loop {
        thread::park();
    }
}

#[derive(Debug)]
struct StalledAcpKit {
    launched: Arc<AtomicBool>,
    terminated: Arc<AtomicBool>,
}

impl DaemonBackend for StalledAcpKit {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        FakeAcpKit.registered_commands()
    }
    fn resolve_acp_agent(
        &self,
        name: &str,
    ) -> Result<marsh_acp::AgentAdapterDeclaration, DaemonError> {
        FakeAcpKit.resolve_acp_agent(name)
    }
    fn prepare(
        &self,
        _: &LoadSelection,
        _: &SessionSpec,
        _: PreparationProgress,
        _: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        Ok(PreparationResult::default())
    }
    fn execute(
        &self,
        _: ExecuteSpec,
        attachment: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        attachment.send(&AttachmentFrame::JobStarted {
            job_id: "stalled-job".into(),
        })?;
        self.launched.store(true, Ordering::Release);
        loop {
            match attachment.receive()? {
                AttachmentFrame::Signal { signal } if signal == "terminate" || signal == "kill" => {
                    self.terminated.store(true, Ordering::Release);
                    attachment.send(&AttachmentFrame::Exited { code: 125 })?;
                    return Ok(());
                }
                _ => {}
            }
        }
    }
    fn open_shell(
        &self,
        _: ShellSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        Ok(())
    }
}

#[test]
fn killed_run_process_during_acp_handshake_terminates_kit_promptly() {
    let home = home();
    let launched = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    let server = Arc::new(Server::bind(home.path()).unwrap().with_backend(Arc::new(
        StalledAcpKit {
            launched: Arc::clone(&launched),
            terminated: Arc::clone(&terminated),
        },
    )));
    let shell = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let marker = home.path().join("startup-marker");
    let ready = home.path().join("startup-ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::acp_run_lease_child"])
        .env("MARSH_TEST_ACP_LEASE_HOME", home.path())
        .env("MARSH_TEST_ACP_LEASE_SHELL", &shell)
        .env("MARSH_TEST_ACP_LEASE_MARKER", &marker)
        .env("MARSH_TEST_ACP_LEASE_READY", &ready)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !launched.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "ACP Kit did not start");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(ready.exists());
    assert!(!marker.exists(), "ACP handshake unexpectedly completed");
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !terminated.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "orphaned ACP startup Kit");
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        Client::connect(home.path())
            .unwrap()
            .acp_list(session(&shell, home.path()))
            .unwrap()
            .iter()
            .all(|entry| entry.terminal && entry.state.as_deref() == Some("failed"))
    );
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
fn killed_or_interrupted_run_process_stops_kit_and_releases_controller() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let first = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let second = server
        .store()
        .attach_shell(2, authority("/Users/example/project", home.path()));
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    for signal in [rustix::process::Signal::KILL, rustix::process::Signal::INT] {
        let marker = home.path().join(format!("lease-{signal:?}"));
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::acp_run_lease_child"])
            .env("MARSH_TEST_ACP_LEASE_HOME", home.path())
            .env("MARSH_TEST_ACP_LEASE_SHELL", &first)
            .env("MARSH_TEST_ACP_LEASE_MARKER", &marker)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let id = loop {
            if let Ok(id) = fs::read_to_string(&marker)
                && !id.is_empty()
            {
                break id;
            }
            assert!(Instant::now() < deadline, "ACP child did not start");
            thread::sleep(Duration::from_millis(5));
        };
        let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "ACP child survived {signal:?}");
            thread::sleep(Duration::from_millis(5));
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = client
                .acp_status(id.clone(), session(&first, home.path()), 0)
                .unwrap();
            if status.controller_shell_session_id.is_none() && status.attachment.terminal.is_some()
            {
                break;
            }
            assert!(Instant::now() < deadline, "ACP Kit survived {signal:?}");
            thread::sleep(Duration::from_millis(5));
        }
        client
            .acp_attach(id.clone(), session(&second, home.path()))
            .unwrap();
        assert!(
            client
                .acp_prompt(id, session(&second, home.path()), "late".into())
                .is_err()
        );
    }
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
fn ended_shell_releases_acp_controller_without_explicit_detach() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let first = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let second = server
        .store()
        .attach_shell(2, authority("/Users/example/project", home.path()));
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let (id, _, lease) = client
        .acp_start("agent-session".into(), session(&first, home.path()))
        .unwrap();
    assert!(matches!(
        client.request(PublicRequest::OpenShell(ShellSpec {
            dev: false,
            arguments: vec![],
            session: session(&first, home.path()),
        })),
        Ok(PublicReply::ShellAccepted)
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if client
            .acp_attach(id.clone(), session(&second, home.path()))
            .is_ok()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "dead shell retained ACP controller"
        );
        thread::sleep(Duration::from_millis(5));
    }
    drop(lease);
    assert!(
        client
            .acp_status(id.clone(), session(&second, home.path()), 0)
            .unwrap()
            .attachment
            .terminal
            .is_none(),
        "explicit handoff keeps the Kit running"
    );
    client.acp_stop(id, session(&second, home.path())).unwrap();
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
fn handed_off_kit_stops_when_last_controller_detaches_after_run_lease_loss() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let first_id = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let second_id = server
        .store()
        .attach_shell(2, authority("/Users/example/project", home.path()));
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let first = session(&first_id, home.path());
    let second = session(&second_id, home.path());
    let (id, _, lease) = client
        .acp_start("agent-session".into(), first.clone())
        .unwrap();
    client.acp_release(id.clone(), first.clone()).unwrap();
    client.acp_attach(id.clone(), second.clone()).unwrap();
    drop(lease);
    // Fence the lease callback before B detaches; the socket handler may
    // independently observe the same close, which is intentionally idempotent.
    server.acp.run_disconnected(&id, &first_id, &server.store());
    let status = client.acp_status(id.clone(), second.clone(), 0).unwrap();
    assert_eq!(status.controller_shell_session_id, Some(second_id.clone()));
    assert!(!status.stopping);
    client
        .acp_prompt(id.clone(), second.clone(), "hello".into())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = client.acp_status(id.clone(), second.clone(), 0).unwrap();
        if status.last_stop_reason == Some(marsh_acp::StopReason::EndTurn) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "handoff controller lost live Kit"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(
        client.request(PublicRequest::DetachShell {
            session_id: second_id
        }),
        Ok(PublicReply::Detached)
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = client.acp_status(id.clone(), first.clone(), 0).unwrap();
        if status.attachment.terminal.is_some() {
            assert!(status.stopping);
            assert!(status.controller_shell_session_id.is_none());
            assert_eq!(status.receipt.unwrap().cleanup, CleanupState::Verified);
            break;
        }
        assert!(Instant::now() < deadline, "orphaned Kit was not stopped");
        thread::sleep(Duration::from_millis(5));
    }
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[test]
fn handed_off_controller_can_detach_while_run_lease_is_live() {
    let home = home();
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(Arc::new(FakeAcpKit)),
    );
    let first_id = server
        .store()
        .attach_shell(1, authority("/Users/example/project", home.path()));
    let second_id = server
        .store()
        .attach_shell(2, authority("/Users/example/project", home.path()));
    let stopping = Arc::new(AtomicBool::new(false));
    let task_server = Arc::clone(&server);
    let task_stopping = Arc::clone(&stopping);
    let task = thread::spawn(move || {
        task_server
            .serve_until(|| task_stopping.load(Ordering::Acquire))
            .unwrap();
    });
    let client = Client::connect(home.path()).unwrap();
    let first = session(&first_id, home.path());
    let second = session(&second_id, home.path());
    let (id, _, lease) = client
        .acp_start("agent-session".into(), first.clone())
        .unwrap();
    client.acp_release(id.clone(), first.clone()).unwrap();
    client.acp_attach(id.clone(), second).unwrap();
    assert!(matches!(
        client.request(PublicRequest::DetachShell {
            session_id: second_id
        }),
        Ok(PublicReply::Detached)
    ));
    let status = client.acp_status(id.clone(), first.clone(), 0).unwrap();
    assert!(status.controller_shell_session_id.is_none());
    assert!(!status.stopping);
    assert!(status.attachment.terminal.is_none());
    client.acp_attach(id.clone(), first.clone()).unwrap();
    client
        .acp_prompt(id.clone(), first.clone(), "hello".into())
        .unwrap();
    client.acp_stop(id, first).unwrap();
    drop(lease);
    stopping.store(true, Ordering::Release);
    task.join().unwrap();
}

#[derive(Debug)]
struct StdinEchoBackend;

impl DaemonBackend for StdinEchoBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(vec!["fixture".into()])
    }
    fn prepare(
        &self,
        _: &LoadSelection,
        _: &SessionSpec,
        _: PreparationProgress,
        _: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        Ok(PreparationResult::default())
    }
    fn execute(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError> {
        let (job_id, _) = store.begin_job(NewJob {
            session_id: request.session.session_id,
            command: request.command,
            kit_ref: "kit:fixture".into(),
            workload_image: "fixture@sha256:1".into(),
            mounts: Vec::new(),
        })?;
        attachment.send(&AttachmentFrame::JobStarted {
            job_id: job_id.clone(),
        })?;
        while let AttachmentFrame::Stdin { bytes } = attachment.receive()? {
            attachment.send(&AttachmentFrame::Stdout { bytes })?;
        }
        attachment.send(&AttachmentFrame::Stderr {
            bytes: b"diagnostic\0\xff".to_vec(),
        })?;
        store.finish_job(
            &job_id,
            ExitStatus {
                code: Some(23),
                cause: "fixture".into(),
            },
            true,
            CleanupState::NotRequired,
            TimingReport::default(),
        )?;
        attachment.send(&AttachmentFrame::Exited { code: 23 })
    }
    fn open_shell(
        &self,
        _: ShellSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }
}

fn start_server(server: Arc<Server>) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let task = thread::spawn(move || {
        server
            .serve_until(|| task_stop.load(Ordering::Relaxed))
            .unwrap();
    });
    (stop, task)
}

#[test]
fn daemon_handshake_deadline_survives_trickled_frame_bytes() {
    let (mut writer, mut reader) = UnixStream::pair().unwrap();
    let sender = thread::spawn(move || {
        for byte in [0_u8; 16] {
            if writer.write_all(&[byte]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    let start = Instant::now();
    let result = read_frame::<PublicRequest>(&mut DeadlineRead {
        stream: &mut reader,
        deadline: start + Duration::from_millis(65),
    });
    assert!(result.is_err());
    assert!(start.elapsed() < Duration::from_millis(180));
    drop(reader);
    sender.join().unwrap();
}

/// A daemon job's structural receipt carries its exit status.
#[test]
fn echo_job_keeps_structural_receipt() {
    let parent = home();
    let selected_home = parent.path().join("selected-home");
    let project = parent.path().join("project");
    fs::create_dir(&selected_home).unwrap();
    fs::create_dir(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let server = Arc::new(
        Server::bind(&selected_home)
            .unwrap()
            .with_backend(Arc::new(StdinEchoBackend)),
    );
    let (stop, task) = start_server(server);
    let client = Client::connect(&selected_home).unwrap();
    let session_id = match client
        .request(PublicRequest::AttachShell {
            pid: 7,
            session: authority(project.to_str().unwrap(), &selected_home),
        })
        .unwrap()
    {
        PublicReply::ShellAttached { session_id } => session_id,
        reply => panic!("unexpected reply: {reply:?}"),
    };
    let mut selected = session(&session_id, &selected_home);
    selected.launch_directory = project;
    let code = client
        .execute_with_io(
            ExecuteSpec {
                placement: Placement::Local,
                environment: std::collections::BTreeMap::new(),
                command: "fixture".into(),
                arguments: Vec::new(),
                session: selected.clone(),
                process: None,
                working_directory: None,
            },
            Cursor::new(b"private\0\xff".to_vec()),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
    assert_eq!(code, 23);
    assert_eq!(client.jobs().unwrap().jobs[0].exit_code, Some(23));
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}
