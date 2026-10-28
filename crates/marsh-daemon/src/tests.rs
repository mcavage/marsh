mod client_liveness;
mod execution_cwd;
mod shell_admission;

use super::*;
use crate::test_support::{
    PATIENCE, fenced, recv, reopen, returns, socket_pair, wait_for, wait_until,
};
use std::{
    io::{Cursor, Read, Write},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
};

#[test]
fn credited_client_preserves_terminal_queued_before_construction_or_failed_input_write() {
    for code in [0, 1, 42] {
        let (client, mut peer) = socket_pair();
        write_frame(
            &mut peer,
            &AttachmentFrame::Stdout {
                bytes: b"tail\0\xff".to_vec(),
            },
        )
        .unwrap();
        write_frame(&mut peer, &AttachmentFrame::Exited { code }).unwrap();
        // Fully closed (macOS still accepts writes after a peer's shutdown),
        // and with no spawn in flight that could hold a duplicate open.
        fenced(|| drop(peer));
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
    let (client, mut peer) = socket_pair();
    peer.write_all(&100_u32.to_be_bytes()).unwrap();
    peer.write_all(b"{\"type\":\"exited\"").unwrap();
    fenced(|| drop(peer));
    let client = ClientExecution::new_shell(client).unwrap();
    let _ = client.send(&AttachmentFrame::StdinEof);
    assert!(
        client.receive().is_err(),
        "a missing actual terminal must never be manufactured"
    );
}

#[test]
fn attachment_frame_write_preserves_partial_frames_across_slow_connected_reads() {
    let (server, mut client) = socket_pair();
    let attachment = ServerAttachment::new(server).unwrap();
    let reader = thread::spawn(move || {
        // A consumer that stalls past several of the writer's 20 ms polls.
        thread::sleep(Duration::from_millis(100));
        read_frame::<AttachmentFrame>(&mut client).unwrap()
    });
    let frame = AttachmentFrame::Failed {
        message: "x".repeat(500 * 1024),
    };
    attachment.send(&frame).unwrap();
    assert_eq!(reader.join().unwrap(), frame);
}

/// A paused but connected guest must never turn credited shell input into a
/// timed-out, torn stream: the socket carries no send deadline at all.
#[test]
fn shell_input_has_no_send_deadline_and_arrives_whole() {
    let (client, mut peer) = socket_pair();
    let socket = client.try_clone().unwrap();
    let execution = ClientExecution::new_shell(client).unwrap();
    assert_eq!(socket.write_timeout().unwrap(), None);
    let sender = thread::spawn(move || {
        execution.send(&AttachmentFrame::Stdin {
            bytes: vec![23; 1_000_000],
        })
    });
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
    let (server, mut client) = socket_pair();
    let attachment = ServerAttachment::new(server).unwrap();
    let receiver = attachment.clone();
    let (send, receive) = mpsc::channel();
    let task = thread::spawn(move || send.send(receiver.receive()).unwrap());
    attachment.close_input();
    assert!(recv(&receive, "closed input wakes the receiver").is_err());
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
    let (server, mut client) = socket_pair();
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
    let store = reopen(home.path()).unwrap();
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let token = store.issue_relay_token(&session).unwrap();
    store.mark_shell_cleanup_uncertain(&session);
    assert!(!store.shell_is_attached(&session));
    assert!(matches!(
        store.detach_shell(&session),
        Err(DaemonError::ShellCleanupUncertain(_))
    ));
    assert!(!store.safe_to_shutdown());
    assert!(
        store
            .status(None)
            .shells
            .iter()
            .any(|shell| shell.session_id == session
                && shell.state == ShellState::CleanupUncertain)
    );
    assert!(store.authentication_scope(&token).is_none());
    // The session authority stays: a fresh relay still resolves to it.
    let fresh = store.issue_relay_token(&session).unwrap();
    let authorized = store
        .authorize_session(&fresh, &self::session(&session, home.path()))
        .unwrap();
    assert_eq!(
        authorized.launch_directory,
        Path::new("/Users/example/project")
    );
}

#[test]
fn client_attachment_write_has_deadline_when_peer_stops_reading() {
    let (client, _peer) = socket_pair();
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
fn client_shutdown_wakes_a_sender_blocked_mid_frame() {
    let (client, mut peer) = socket_pair();
    // No deadline of its own: only shutdown can end this send.
    let execution = ClientExecution::new_with_write_timeout(client, PATIENCE * 10).unwrap();
    let sender = execution.clone();
    let (sent, outcome) = mpsc::channel();
    thread::spawn(move || {
        let _ = sent.send(sender.send(&AttachmentFrame::Failed {
            message: "x".repeat(900 * 1024),
        }));
    });
    // The frame header has arrived and the peer stops reading: the sender is
    // now inside a frame far larger than the socket buffer.
    let mut header = [0_u8; 4];
    peer.read_exact(&mut header).unwrap();
    execution.shutdown().unwrap();
    assert!(recv(&outcome, "shutdown wakes the blocked sender").is_err());
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

    let (_server, client) = socket_pair();
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
    let (result, stderr) = recv(&received, "stdin read failure ends the attachment");
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

type AttachmentResult = (Result<i32, DaemonError>, Vec<u8>);

/// A shell attachment whose input pump has spent the whole credit window and
/// is blocked on the next chunk.
fn blocked_shell_input() -> (
    ServerAttachment,
    mpsc::Receiver<()>,
    mpsc::Receiver<AttachmentResult>,
) {
    let (server_stream, client_stream) = socket_pair();
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
    (server, dropped_receive, result_receive)
}

#[test]
fn shell_exit_after_blocked_stdin_keeps_its_exit_status() {
    let (server, dropped, result) = blocked_shell_input();
    server.send(&AttachmentFrame::StdinClosed).unwrap();
    recv(&dropped, "the blocked input pump observes stdin closure");
    server.send(&AttachmentFrame::Exited { code: 130 }).unwrap();
    let (result, stderr) = recv(&result, "attachment result");
    assert_eq!(result.unwrap(), 130);
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
}

#[test]
fn lost_transport_after_remote_stdin_close_reports_transport_cause() {
    let (server, dropped, result) = blocked_shell_input();
    server.send(&AttachmentFrame::StdinClosed).unwrap();
    recv(&dropped, "the input pump observes remote stdin closure");
    // Shut the socket itself down: a descriptor briefly inherited by another
    // test's child process would keep a merely dropped peer open.
    server.shutdown().unwrap();
    drop(server);
    let (result, stderr) = recv(&result, "attachment result");
    assert!(matches!(result, Err(DaemonError::Io(_))));
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
}

/// A control frame (a forwarded signal) that cannot be written fences the
/// transport: stdin blocked on credit wakes with the cause, and the terminal
/// status the guest already queued is still delivered.
#[test]
fn failed_control_write_wakes_stdin_blocked_on_credit() {
    let (client, mut peer) = socket_pair();
    let local = client.try_clone().unwrap();
    let execution = ClientExecution::new_shell(client).unwrap();
    let input = execution.clone();
    let (input_send, input_result) = mpsc::channel();
    thread::spawn(move || {
        let _ = input_send.send(input.send(&AttachmentFrame::Stdin {
            bytes: vec![b'x'; SHELL_STDIN_CHUNK * (SHELL_STDIN_WINDOW + 1)],
        }));
    });
    for _ in 0..SHELL_STDIN_WINDOW {
        assert!(matches!(
            read_frame::<AttachmentFrame>(&mut peer).unwrap(),
            AttachmentFrame::Stdin { .. }
        ));
    }
    write_frame(&mut peer, &AttachmentFrame::Exited { code: 7 }).unwrap();
    // The write direction fails without the peer (or the receiver) noticing.
    local.shutdown(Shutdown::Write).unwrap();
    assert!(matches!(
        execution.send(&AttachmentFrame::Signal {
            signal: "interrupt".into(),
        }),
        Err(DaemonError::ShellInputWriteFailed(_))
    ));
    assert!(matches!(
        recv(&input_result, "blocked stdin wakes"),
        Err(DaemonError::ShellInputTransportClosed(_))
    ));
    assert_eq!(
        execution.receive().unwrap(),
        AttachmentFrame::Exited { code: 7 }
    );
}

#[test]
fn shell_stdin_window_keeps_signal_sendable_when_guest_stalls() {
    let (server_stream, client_stream) = socket_pair();
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
    recv(&window_received, "input spent the window");
    // Without credit the next chunk stays queued behind the signal.
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
    recv(&extra_received, "credited chunk sent");
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
    let store = reopen(home.path()).unwrap();
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
fn initial_terminal_dimensions_are_frozen_before_attachment_with_a_safe_fallback() {
    for ((rows, columns), expected) in [((51, 173), (51, 173)), ((0, 0), (24, 80))] {
        let opened = nix::pty::openpty(None, None).unwrap();
        let terminal = File::from(opened.slave);
        rustix::termios::tcsetwinsize(
            &terminal,
            rustix::termios::Winsize {
                ws_row: rows,
                ws_col: columns,
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
                rows: expected.0,
                columns: expected.1,
            })
        );
    }
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
    let store = reopen(root.path()).unwrap();
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
    // Longer than the 5 s default client request timeout: the relay must wait
    // on the publication reply budget, not the ordinary request budget.
    let server = Arc::new(server);
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_one().unwrap());
    let started = Instant::now();
    let result = relay.mcp_publish(spec, "slow".into(), None, None, "cat".into());
    assert!(result.unwrap().contains("host operation finished"));
    assert!(started.elapsed() >= Duration::from_secs(6));
    task.join().unwrap();
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
    let release = root.path().join("release-public");
    let log = root.path().join("host-log");
    // The first publisher holds the host step until released, then records
    // whether the declaration it bound was overwritten meanwhile.
    let script = typed_mcp_host_peer(&format!(
        "pipeline=sys.argv[-1]\np=pathlib.Path({declaration})\nlog=pathlib.Path({log})\nwith log.open('a') as f: f.write('start '+pipeline+'\\n')\np.write_bytes(pipeline.encode())\np.chmod(0o600)\nif pipeline=='public':\n pathlib.Path({public_started}).touch()\n while not pathlib.Path({release}).exists(): time.sleep(.01)\n assert p.read_bytes()==b'public'\nwith log.open('a') as f: f.write('end '+pipeline+'\\n')\ncommitted('published '+pipeline)",
        declaration = serde_json::to_string(&declaration).unwrap(),
        log = serde_json::to_string(&log).unwrap(),
        public_started = serde_json::to_string(&public_started).unwrap(),
        release = serde_json::to_string(&release).unwrap(),
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
    wait_until("first publisher reaches the host CLI", || {
        public_started.exists() || public.is_finished()
    });
    assert!(public_started.exists(), "{:?}", public.join().unwrap());
    let private_server = Arc::clone(&server);
    let private_request = thread::spawn(move || private_server.serve_one().unwrap());
    let private = thread::spawn(move || {
        private_relay.mcp_publish(private_spec, "shared".into(), None, None, "private".into())
    });
    fs::write(&release, b"").unwrap();
    assert!(public.join().unwrap().unwrap().contains("published public"));
    assert!(
        private
            .join()
            .unwrap()
            .unwrap()
            .contains("published private")
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "start public\nend public\nstart private\nend private\n",
        "a competing publisher reached the host CLI before the first finished"
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
    assert!(
        server.store.status(None).shells.is_empty(),
        "{:?}",
        server.store.status(None).shells
    );
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
    fenced(|| Lifecycle::bind(home.path())).unwrap();
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
    let mut child = fenced(|| Command::new("/usr/bin/true").spawn()).unwrap();
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
    let mut child = fenced(|| Command::new("/usr/bin/true").spawn()).unwrap();
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
    let store = reopen(home.path()).unwrap();
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
    assert!(
        status.workers[0].active_container_ids.is_empty(),
        "{:?}",
        status.workers[0].active_container_ids
    );
    assert_eq!(store.job(&job).unwrap().state, JobState::Finished);
}

#[test]
fn uncertain_cleanup_quarantines_worker_and_releases_capacity() {
    let home = home();
    let store = reopen(home.path()).unwrap();
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
    assert!(
        status.workers[0].active_container_ids.is_empty(),
        "{:?}",
        status.workers[0].active_container_ids
    );
    let receipt = store.job(&job).unwrap();
    assert_eq!(receipt.state, JobState::Unknown);
    assert_eq!(receipt.exit.unwrap().code, Some(125));
    assert_eq!(
        store.job(&job).unwrap().exit.unwrap().cause,
        "controller_lost; cleanup_uncertain"
    );
    assert!(store.worker_reuse_blocked("vm-1"));
    drop(store);
    let reopened = reopen(home.path()).unwrap();
    assert!(
        reopened.status(None).workers.is_empty(),
        "{:?}",
        reopened.status(None).workers
    );
    assert!(
        !reopened.worker_reuse_blocked("vm-1"),
        "a historical receipt must not block a physically recreated VM"
    );
}

/// A failure before the container ran (an abort, a verified pre-runtime
/// cleanup, an uncertain grant rollback) keeps its cause on the receipt but
/// never blocks a freshly created VM of the same name after a restart.
#[test]
fn pre_runtime_failures_stay_auditable_without_blocking_a_fresh_vm() {
    enum End {
        Abort,
        Finish(&'static str, CleanupState),
    }
    for (end, cause) in [
        (End::Abort, "backend_failed; cleanup_uncertain"),
        (
            End::Finish("worker_start_failed", CleanupState::Verified),
            "worker_start_failed",
        ),
        (
            End::Finish(
                "grant_prepare_failed: second mount rejected",
                CleanupState::Uncertain,
            ),
            "grant_prepare_failed: second mount rejected; cleanup_uncertain",
        ),
    ] {
        let home = home();
        let store = reopen(home.path()).unwrap();
        let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
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
        store.reserve_worker(&job, "worker", "vm").unwrap();
        assert_eq!(store.job(&job).unwrap().vm_id.as_deref(), Some("vm"));
        match end {
            End::Abort => store.abort_job(&job, "backend_failed").unwrap(),
            End::Finish(cause, cleanup) => store
                .finish_job(
                    &job,
                    ExitStatus {
                        code: Some(125),
                        cause: cause.into(),
                    },
                    false,
                    cleanup,
                    TimingReport::default(),
                )
                .unwrap(),
        }
        assert_eq!(store.job(&job).unwrap().exit.unwrap().cause, cause);
        drop(store);

        let reopened = reopen(home.path()).unwrap();
        assert_eq!(reopened.job(&job).unwrap().exit.unwrap().cause, cause);
        assert!(!reopened.worker_reuse_blocked("vm"), "{cause}");
    }
}

#[test]
fn detaching_a_shell_revokes_its_relay_tokens() {
    let home = home();
    let store = reopen(home.path()).unwrap();
    let session = store.attach_shell(42, authority("/Users/example/project", home.path()));
    let token = store.issue_relay_token(&session).unwrap();

    assert!(store.authentication_scope(&token).is_some());
    store.detach_shell(&session).unwrap();
    assert!(store.authentication_scope(&token).is_none());
}

#[test]
fn relay_token_restores_host_authority_and_rejects_other_sessions() {
    let home = home();
    let store = reopen(home.path()).unwrap();
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
    let store = reopen(home.path()).unwrap();
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
    let store = reopen(home.path()).unwrap();
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
    let store = reopen(home.path()).unwrap();
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
    let store = reopen(home.path()).unwrap();
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
    let store = reopen(home.path()).unwrap();
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
    assert!(status.workers.is_empty(), "{:?}", status.workers);
    assert_eq!(status.shells.len(), 1);
    assert_eq!(store.job(&job).unwrap().state, JobState::Cancelled);
}

#[test]
fn abort_receipt_is_explicitly_failed() {
    let home = home();
    let store = reopen(home.path()).unwrap();
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
    let store = reopen(home.path()).unwrap();
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
        let store = reopen(home.path()).unwrap();
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

    let reopened = reopen(home.path()).unwrap();
    assert_eq!(reopened.job(&finished).unwrap().state, JobState::Finished);
    let interrupted = reopened.job(&active).unwrap();
    assert_eq!(interrupted.state, JobState::Unknown);
    assert_eq!(interrupted.exit.unwrap().cause, "daemon_restarted");
    assert_eq!(reopened.jobs().jobs[0].job_id, active);
}

#[test]
fn a_detached_cloud_job_survives_restart_outside_the_pool_and_settles_once() {
    let home = home();
    let job = {
        let store = reopen(home.path()).unwrap();
        let session = store.attach_shell(1, authority("/Users/example/project", home.path()));
        let link = process::ProcessLink::default();
        let (job, _, _) = store
            .begin_cloud_job(
                NewJob {
                    session_id: session,
                    command: "cloud".into(),
                    kit_ref: "cloud-shell".into(),
                    workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                    mounts: Vec::new(),
                },
                &link,
                &[],
            )
            .unwrap();
        store.mark_cloud_running(&job, "sbx_abc123").unwrap();
        assert_eq!(store.process_counts().running, 1);
        store
            .detach_cloud_job(&job, TimingReport::default())
            .unwrap();
        // Out of the local pool once detached.
        assert_eq!(store.process_counts().running, 0);
        job
    };
    // A restart does not make it uncertain: collection settles it later.
    let reopened = reopen(home.path()).unwrap();
    let receipt = reopened.job(&job).unwrap();
    assert_eq!(receipt.state, JobState::Detached);
    assert_eq!(receipt.detached.as_deref(), Some("running"));
    reopened
        .finish_detached_job(
            &job,
            ExecutionOutcome::Exited { code: 0 },
            ExitStatus {
                code: Some(0),
                cause: "kept-as-patch".into(),
            },
            true,
            CleanupState::Verified,
            TimingReport::default(),
            "kept-as-patch",
            None,
        )
        .unwrap();
    // Applying the kept patch later relabels it; the journal still replays.
    reopened.relabel_settled_detached(&job, "collected");
    drop(reopened);
    let settled = reopen(home.path()).unwrap().job(&job).unwrap();
    assert_eq!(settled.state, JobState::Finished);
    assert_eq!(settled.detached.as_deref(), Some("collected"));
    assert_eq!(settled.exit.unwrap().code, Some(0));
}

#[test]
fn result_selectors_accept_cursor_full_id_and_unique_prefix() {
    let home = home();
    let store = reopen(home.path()).unwrap();
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
fn receipt_journal_rejects_permissive_state_modes() {
    let state_home = home();
    fs::create_dir(state_home.path().join("state")).unwrap();
    fs::set_permissions(
        state_home.path().join("state"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(
        reopen(state_home.path()),
        Err(DaemonError::UnsafeHome(_))
    ));

    let file_home = home();
    let store = reopen(file_home.path()).unwrap();
    drop(store);
    let journal = receipt_journal::journal_path(file_home.path());
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        reopen(file_home.path()),
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

    let legacy = reopen(selected.path()).unwrap();
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
    assert!(
        server.store.jobs().jobs.is_empty(),
        "{:?}",
        server.store.jobs().jobs
    );
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

    let resumed = fenced(|| {
        Server::bind_with_stock_sbx_and_control_home(selected.path(), sbx.path(), &control_home)
    })
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
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
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
            .stderr(Stdio::piped());
        fenced(|| command.spawn()).unwrap()
    };
    let mut owner = runner("owner", first_tmp.path());
    let ready = control_home.join("ready");
    let bound = wait_for("owner binds its endpoint or exits", || {
        if ready.exists() {
            Some(true)
        } else {
            owner.try_wait().unwrap().map(|_| false)
        }
    });
    if !bound {
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
    let reopened = fenced(|| {
        Server::bind_with_stock_sbx_and_control_home(selected.path(), sbx.path(), &control_home)
    })
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
            // Publish atomically: the parent starts the contender as soon
            // as `ready` exists.
            fs::write(
                control.join("ready.partial"),
                server
                    .lifecycle
                    .paths()
                    .runtime_directory
                    .as_os_str()
                    .as_encoded_bytes(),
            )
            .unwrap();
            fs::rename(control.join("ready.partial"), control.join("ready")).unwrap();
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
    drop(reopen(home.path()).unwrap());
    let lock = home.path().join("state/owner.lock");
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        reopen(home.path()),
        Err(DaemonError::UnsafeHome(_))
    ));
    fs::remove_file(&lock).unwrap();
    let target = home.path().join("target");
    fs::write(&target, b"sentinel").unwrap();
    std::os::unix::fs::symlink(&target, &lock).unwrap();
    assert!(matches!(
        reopen(home.path()),
        Err(DaemonError::UnsafeHome(_))
    ));
    assert_eq!(fs::read(&target).unwrap(), b"sentinel");
}

#[test]
fn journal_owner_lock_excludes_a_second_direct_open() {
    let home = home();
    let store = reopen(home.path()).unwrap();
    assert!(matches!(
        receipt_journal::ReceiptJournal::open(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("already owned")
    ));
    drop(store);
    assert!(fenced(|| receipt_journal::ReceiptJournal::open(home.path())).is_ok());
}

#[test]
fn ordinary_terminal_transition_appends_without_compacting() {
    let home = home();
    let store = reopen(home.path()).unwrap();
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
fn receipt_retention_compacts_an_overfull_journal_on_restart() {
    let home = home();
    let template = {
        let store = reopen(home.path()).unwrap();
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

    // Over the limit, the oldest terminal receipts go, with hysteresis: down
    // to 900, not to the limit, so the next job does not compact again.
    let reopened = reopen(home.path()).unwrap();
    let mut retained = reopened
        .jobs()
        .jobs
        .iter()
        .map(|job| reopened.job(&job.job_id).unwrap().cursor)
        .collect::<Vec<_>>();
    retained.sort_unstable();
    let total = receipt_journal::MAX_RETAINED_RECEIPTS as u64 + 1;
    assert_eq!(retained, (total - 899..=total).collect::<Vec<_>>());
    assert_eq!(receipt_journal::record_count(home.path()), 900);
}

#[test]
fn receipt_journal_compacts_and_replays_a_reserved_queued_receipt() {
    let home = home();
    let store = reopen(home.path()).unwrap();
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

    let reopened = reopen(home.path()).unwrap();
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
        let store = reopen(home.path()).unwrap();
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
        reopen(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("state evidence")
    ));
}

#[test]
fn receipt_journal_truncates_only_a_corrupt_tail() {
    let home = home();
    let job = {
        let store = reopen(home.path()).unwrap();
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

    let reopened = reopen(home.path()).unwrap();
    assert_eq!(reopened.job(&job).unwrap().state, JobState::Unknown);
    assert!(fs::metadata(journal).unwrap().len() >= good_length);
}

#[test]
fn receipt_journal_fails_closed_on_interior_corruption() {
    let home = home();
    {
        let store = reopen(home.path()).unwrap();
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
        reopen(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("corrupt")
    ));
}

#[test]
fn receipt_journal_fails_closed_on_complete_corrupt_final_record() {
    let home = home();
    {
        let store = reopen(home.path()).unwrap();
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
        reopen(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("checksum mismatch")
    ));
}

#[test]
fn receipt_journal_rejects_inconsistent_terminal_state_evidence() {
    let home = home();
    let mut receipt = {
        let store = reopen(home.path()).unwrap();
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
        reopen(home.path()),
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
            let store = reopen(home.path()).unwrap();
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

        let reopened = reopen(home.path()).unwrap();
        assert_eq!(reopened.job(&receipt.job_id).unwrap(), receipt);
    }
}

#[test]
fn receipt_journal_rejects_terminal_container_without_worker_placement() {
    let home = home();
    let mut receipt = {
        let store = reopen(home.path()).unwrap();
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
        reopen(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("state evidence")
    ));
}

#[test]
fn receipt_journal_rejects_success_with_uncertain_cleanup() {
    let home = home();
    let mut receipt = {
        let store = reopen(home.path()).unwrap();
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
        reopen(home.path()),
        Err(DaemonError::InvalidState(message)) if message.contains("state evidence")
    ));
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
    // admission guard has not dropped yet. The retry is bounded: with the
    // admission held throughout, the caller gives up with the busy refusal.
    let admission = server.store.begin_ordinary_request().unwrap();
    assert!(matches!(
        client.shutdown_resident_after_probe_within(Duration::ZERO),
        Err(DaemonError::Remote(message)) if message == BUSY_LIFECYCLE_MESSAGE
    ));
    assert!(!server.shutdown_requested.load(Ordering::Acquire));

    // Within its budget the caller retries past the released admission.
    let retrying = thread::spawn(move || client.shutdown_resident_after_probe_within(PATIENCE));
    drop(admission);
    retrying.join().unwrap().unwrap();
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

    let error = Client::reconcile_resident(home.path(), &paths, &expected).unwrap_err();
    assert!(matches!(
        error,
        DaemonError::Remote(ref message)
            if message == BUSY_LIFECYCLE_MESSAGE
    ));
    assert!(!server.shutdown_requested.load(Ordering::Acquire));

    server.store.detach_shell(&session).unwrap();
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[test]
fn a_daemon_whose_stock_sbx_was_removed_can_be_replaced_unless_work_is_attached() {
    let home = home();
    let old_sbx = stock_sbx(b"stock-sbx-old");
    let new_sbx = stock_sbx(b"stock-sbx-new");
    let server = Arc::new(Server::bind_with_stock_sbx(home.path(), old_sbx.path()).unwrap());
    // Unresolved cleanup that only a working SBX could settle.
    let uncertain = server
        .store
        .attach_shell(1, authority("/Users/example/project", home.path()));
    server.store.mark_shell_cleanup_uncertain(&uncertain);
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_until(|| false).unwrap());
    // Only the serving thread holds the server, so it releases its endpoint
    // when it shuts down.
    let store = server.store.clone();
    drop(server);
    let expected = runtime_identity(&std::env::current_exe().unwrap(), new_sbx.path()).unwrap();
    let paths = EndpointPaths::for_home(home.path()).unwrap();

    // While its SBX exists, the old daemon keeps the record and refuses.
    assert!(matches!(
        Client::reconcile_resident(home.path(), &paths, &expected),
        Err(DaemonError::Remote(ref message)) if message == BUSY_LIFECYCLE_MESSAGE
    ));
    // An attached shell is live work: still refused after SBX is removed,
    // and the refusal says why.
    let attached = store.attach_shell(2, authority("/Users/example/project", home.path()));
    let old_path = old_sbx.path().to_owned();
    old_sbx.close().unwrap();
    assert!(!old_path.exists());
    let removed = stock_sbx_removed_message(&old_path);
    let refused = Client::reconcile_resident(home.path(), &paths, &expected);
    assert!(
        matches!(&refused, Err(DaemonError::Remote(message)) if *message == removed),
        "{refused:?}"
    );
    // Once nothing is attached, a daemon that cannot reach SBX steps aside.
    store.detach_shell(&attached).unwrap();
    assert!(
        Client::reconcile_resident(home.path(), &paths, &expected)
            .unwrap()
            .is_none()
    );
    task.join().unwrap();
}

#[test]
fn a_daemon_whose_stock_sbx_was_removed_reports_it_and_stops_once_nothing_is_attached() {
    let home = home();
    let sbx = stock_sbx(b"stock-sbx-old");
    let server = Arc::new(
        Server::bind_with_stock_sbx(home.path(), sbx.path())
            .unwrap()
            .with_stock_sbx_probe_interval(Duration::from_millis(10)),
    );
    let store = server.store.clone();
    let shell = store.attach_shell(1, authority("/Users/example/project", home.path()));
    let task_server = Arc::clone(&server);
    let task = thread::spawn(move || task_server.serve_until(|| false).unwrap());
    drop(server);
    let status = || {
        let PublicReply::Status(status) = Client::connect(home.path())
            .unwrap()
            .request(PublicRequest::Status { session_id: None })
            .unwrap()
        else {
            panic!("expected status")
        };
        status
    };
    assert_eq!(status().stock_sbx_removed, None);

    let path = sbx.path().to_owned();
    sbx.close().unwrap();
    // The attached shell keeps working through its running SBX process, so
    // the daemon stays and says why.
    thread::sleep(Duration::from_millis(200));
    assert!(!task.is_finished());
    assert_eq!(status().stock_sbx_removed, Some(path));

    store.detach_shell(&shell).unwrap();
    let deadline = Instant::now() + PATIENCE;
    while !task.is_finished() {
        assert!(Instant::now() < deadline, "the daemon did not stop");
        thread::sleep(Duration::from_millis(10));
    }
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
    assert!(
        server.store.status(None).shells.is_empty(),
        "{:?}",
        server.store.status(None).shells
    );
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
        progress.cold_boot("fixture", false)?;
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
                terminal_environment: std::collections::BTreeMap::new(),
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
                    terminal_environment: std::collections::BTreeMap::new(),
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

/// The owner unlinks its lock while the stopping client may be between the
/// existence check and reading it. That is a release, not an incomplete stop.
#[test]
fn owner_release_tolerates_the_lock_vanishing_mid_check_but_not_other_errors() {
    let paths = EndpointPaths {
        runtime_directory: PathBuf::from("/fixture"),
        socket: PathBuf::from("/fixture/s"),
        lock: tempfile::NamedTempFile::new()
            .unwrap()
            .into_temp_path()
            .keep()
            .unwrap(),
        token: PathBuf::from("/fixture/t"),
    };
    // The probe observes the lock, then the owner removes it before the read.
    let vanished = wait_for_release(&paths, Duration::from_secs(2), |paths| {
        fs::remove_file(&paths.lock).unwrap();
        Err(DaemonError::Io(io::ErrorKind::NotFound.into()))
    });
    assert!(vanished.is_ok(), "{vanished:?}");

    fs::write(&paths.lock, "1\n").unwrap();
    let failing = wait_for_release(&paths, Duration::from_secs(2), |paths| {
        Err(DaemonError::EndpointInconsistent(paths.lock.clone()))
    });
    assert!(
        failing.is_err(),
        "an error with the lock still present is not a release"
    );

    let live = wait_for_release(&paths, Duration::from_millis(100), |_| {
        Ok(ReclaimOutcome::Live)
    });
    assert!(
        live.is_err(),
        "a live owner past the deadline is not a release"
    );
    fs::remove_file(&paths.lock).unwrap();
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

/// Refused at once and without blocking `Status`: neither call may wait for
/// the live shell or job to end (it never ends on its own here).
fn assert_lifecycle_refused_and_status_responsive(client: &Client, request: PublicRequest) {
    let refusing = client.clone();
    assert!(matches!(
        returns("lifecycle refusal", move || refusing
            .request(request)
            .unwrap()),
        PublicReply::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));
    let status = client.clone();
    assert!(matches!(
        returns("status during live work", move || {
            status
                .request(PublicRequest::Status { session_id: None })
                .unwrap()
        }),
        PublicReply::Status(_)
    ));
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
            terminal_environment: std::collections::BTreeMap::new(),
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
    // The accepted execution holds its connection's admission until it ends.
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
    wait_until("lifecycle is admitted once the execution ends", || {
        matches!(
            client.request(PublicRequest::ResetScope),
            Ok(PublicReply::ScopeLifecycle(_))
        )
    });
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[derive(Debug)]
struct BlockingLifecycleBackend {
    gate: Arc<(Mutex<(bool, bool)>, std::sync::Condvar)>,
}

/// Records the teardown deadline the daemon hands it and reports every
/// component uncertain.
#[derive(Debug, Default)]
struct DeadlineRecordingBackend {
    deadline: Mutex<Option<Instant>>,
}

impl DaemonBackend for DeadlineRecordingBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(Vec::new())
    }

    fn teardown_scope(
        &self,
        action: ScopeLifecycleAction,
        _store: DaemonStore,
        deadline: Instant,
    ) -> ScopeLifecycleReport {
        *self.deadline.lock().unwrap() = Some(deadline);
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
                    detail: Some(format!("{label} not verified")),
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

/// The daemon gives teardown one absolute deadline (its configured lifecycle
/// budget from admission), returns the backend's per-component report
/// unchanged, and releases the lifecycle gate afterwards.
#[test]
fn scope_teardown_gets_the_configured_deadline_and_releases_the_gate() {
    let home = home();
    let backend = Arc::new(DeadlineRecordingBackend::default());
    let budget = Duration::from_hours(1);
    let server = Arc::new(
        Server::bind(home.path())
            .unwrap()
            .with_backend(backend.clone())
            .with_scope_lifecycle_timeout(budget),
    );
    let (stop, task) = start_server(Arc::clone(&server));
    let client = Client::connect(home.path()).unwrap();
    let sent = Instant::now();
    let report = client.reset_scope().unwrap();
    let received = Instant::now();
    let deadline = backend.deadline.lock().unwrap().unwrap();
    assert!(sent + budget <= deadline && deadline <= received + budget);
    assert!(!report.cleanup_complete);
    assert_eq!(
        report
            .components
            .iter()
            .map(|component| (component.label.as_str(), component.detail.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("kit-a", Some("kit-a not verified")),
            ("kit-b", Some("kit-b not verified")),
            ("shell", Some("shell not verified")),
        ]
    );
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

    let status = client.clone();
    assert!(matches!(
        returns("status refused during teardown", move || {
            status
                .request(PublicRequest::Status { session_id: None })
                .unwrap()
        }),
        PublicReply::Error {
            code: ErrorCode::InvalidRequest,
            ..
        }
    ));

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

/// A socket read timeout is an idle timeout; the lifecycle client instead
/// imposes one absolute deadline over the whole framed reply, so a daemon
/// that sends part of a reply and stalls cannot hold it past that deadline.
#[test]
fn scope_lifecycle_client_deadline_covers_a_stalled_partial_reply() {
    let root = home();
    let paths = endpoint_paths(root.path().join("runtime"));
    fs::create_dir(&paths.runtime_directory).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&paths.socket).unwrap();
    let daemon = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _: Envelope<PublicRequest> = read_frame(&mut stream).unwrap();
        stream.write_all(&100_u32.to_be_bytes()).unwrap();
        stream.write_all(b"{\"type\":").unwrap();
        // Hold the connection open until the caller gives up.
        let _ = stream.read(&mut [0_u8; 1]);
    });
    let client = Client {
        paths,
        token: "0".repeat(64),
    };
    let timeout = Duration::from_millis(50);
    let started = Instant::now();
    let error = client
        .scope_lifecycle_with_timeout(PublicRequest::ResetScope, timeout)
        .unwrap_err();
    assert!(
        matches!(error, DaemonError::Io(ref error) if error.kind() == io::ErrorKind::TimedOut),
        "{error:?}"
    );
    assert!(started.elapsed() >= timeout);
    daemon.join().unwrap();
}

/// A lifecycle caller that vanishes mid-teardown does not wedge the daemon:
/// once teardown finishes, its gate is released and ordinary work resumes.
#[test]
fn daemon_recovers_after_teardown_for_a_vanished_caller() {
    let home = home();
    let gate = Arc::new((Mutex::new((false, false)), std::sync::Condvar::new()));
    let server = Arc::new(Server::bind(home.path()).unwrap().with_backend(Arc::new(
        BlockingLifecycleBackend {
            gate: Arc::clone(&gate),
        },
    )));
    let (stop, task) = start_server(Arc::clone(&server));
    let client = Client::connect(home.path()).unwrap();
    let mut caller = UnixStream::connect(&client.paths.socket).unwrap();
    write_frame(
        &mut caller,
        &Envelope {
            protocol: PROTOCOL.into(),
            token: client.token.clone(),
            body: PublicRequest::ResetScope,
        },
    )
    .unwrap();
    {
        let (state, changed) = &*gate;
        let state = state.lock().unwrap();
        let (_state, waited) = changed
            .wait_timeout_while(state, PATIENCE, |state| !state.0)
            .unwrap();
        assert!(!waited.timed_out(), "teardown never began");
    }
    caller.shutdown(Shutdown::Both).unwrap();
    drop(caller);
    {
        let (state, changed) = &*gate;
        state.lock().unwrap().1 = true;
        changed.notify_all();
    }
    wait_until("the daemon serves Status after teardown", || {
        matches!(
            client.request(PublicRequest::Status { session_id: None }),
            Ok(PublicReply::Status(_))
        )
    });
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
                                        wait_until("the test releases natural completion", || {
                                            std::path::Path::new(path).exists()
                                        });
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
        let outcome = recv(&done_rx, "handoff finishes after the hook");
        handoff.join().unwrap();
        outcome
    })
}

/// A daemon serving an ACP Kit, with two shells attached to one project.
struct AcpFixture {
    home: tempfile::TempDir,
    server: Arc<Server>,
    client: Client,
    first_id: String,
    second_id: String,
    first: SessionSpec,
    second: SessionSpec,
    stopping: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl AcpFixture {
    fn new() -> Self {
        Self::with_backend(Arc::new(FakeAcpKit), None)
    }

    /// `project`: attach the shells to a real directory under the fixture
    /// home (publication binds to its identity) instead of a nominal path.
    fn with_backend(backend: Arc<dyn DaemonBackend>, project: Option<&str>) -> Self {
        let home = home();
        let launch = match project {
            Some(name) => {
                let project = home.path().join(name);
                fs::create_dir(&project).unwrap();
                project.canonicalize().unwrap()
            }
            None => PathBuf::from("/Users/example/project"),
        };
        let server = Arc::new(Server::bind(home.path()).unwrap().with_backend(backend));
        let first_id = server
            .store()
            .attach_shell(1, authority(launch.to_str().unwrap(), home.path()));
        let second_id = server
            .store()
            .attach_shell(2, authority(launch.to_str().unwrap(), home.path()));
        let spec = |id: &str| SessionSpec {
            launch_directory: launch.clone(),
            ..session(id, home.path())
        };
        let (first, second) = (spec(&first_id), spec(&second_id));
        let stopping = Arc::new(AtomicBool::new(false));
        let task_server = Arc::clone(&server);
        let task_stopping = Arc::clone(&stopping);
        let task = thread::spawn(move || {
            task_server
                .serve_until(|| task_stopping.load(Ordering::Acquire))
                .unwrap();
        });
        let client = Client::connect(home.path()).unwrap();
        Self {
            home,
            server,
            client,
            first_id,
            second_id,
            first,
            second,
            stopping,
            task: Some(task),
        }
    }

    fn wait_status(
        &self,
        id: &str,
        shell: &SessionSpec,
        what: &str,
        ready: impl Fn(&AcpSessionStatus) -> bool,
    ) -> AcpSessionStatus {
        wait_for(what, || {
            let status = self
                .client
                .acp_status(id.to_owned(), shell.clone(), 0)
                .unwrap();
            ready(&status).then_some(status)
        })
    }

    fn turn_ended(&self, id: &str, shell: &SessionSpec, what: &str) -> AcpSessionStatus {
        self.wait_status(id, shell, what, |status| {
            status.last_stop_reason == Some(marsh_acp::StopReason::EndTurn)
        })
    }

    /// A child test process holding an `acp run` lease for `shell`.
    fn lease_child(&self, shell: &str, marker: &Path, ready: Option<&Path>) -> std::process::Child {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "tests::acp_run_lease_child"])
            .env("MARSH_TEST_ACP_LEASE_HOME", self.home.path())
            .env("MARSH_TEST_ACP_LEASE_SHELL", shell)
            .env("MARSH_TEST_ACP_LEASE_MARKER", marker)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(ready) = ready {
            command.env("MARSH_TEST_ACP_LEASE_READY", ready);
        }
        fenced(|| command.spawn()).unwrap()
    }
}

impl Drop for AcpFixture {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn signal_child(child: &std::process::Child, signal: rustix::process::Signal) {
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, signal).unwrap();
}

#[test]
fn acp_prompt_retry_after_lost_reply_keeps_one_provider_turn() {
    let acp = AcpFixture::new();
    let (client, shell) = (&acp.client, acp.first.clone());
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
    let initial = acp.turn_ended(&id, &shell, "lost-reply turn ends");
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
}

#[test]
fn acp_published_grant_survives_parent_lease_and_revokes_cached_call() {
    let acp = AcpFixture::with_backend(Arc::new(FakeAcpKit), Some("project"));
    let (server, client) = (&acp.server, &acp.client);
    let (owner, observer) = (acp.first.clone(), acp.second.clone());
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
    relay.token = server.store.issue_relay_token(&acp.first_id).unwrap();
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
    let status = wait_for("published ACP turn ends", || {
        let status = client
            .acp_published_status(agent_id.clone(), generation.clone(), 0)
            .unwrap();
        (status.last_stop_reason == Some(marsh_acp::StopReason::EndTurn)).then_some(status)
    });
    assert_eq!(status.updates.len(), 1);
    assert!(status.controller_shell_session_id.is_none());
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
    // Fence the lease-loss callback (idempotent with the socket handler's).
    server
        .acp
        .run_disconnected(&agent_id, &acp.first_id, &server.store());
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
    client.acp_stop(agent_id, owner).unwrap();
}

#[test]
fn acp_unpublish_revokes_its_grant_and_a_late_rollback_spares_a_newer_one() {
    let acp = AcpFixture::with_backend(Arc::new(FakeAcpKit), Some("project"));
    let (server, client, owner) = (&acp.server, &acp.client, acp.first.clone());
    let (agent_id, _, lease) = client
        .acp_start("agent-session".into(), owner.clone())
        .unwrap();
    let generation = server.acp.publish(&agent_id, &owner, "shared").unwrap();
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
    drop(lease);
}

#[test]
fn acp_cancel_racing_natural_completion_keeps_session_usable() {
    let acp = AcpFixture::new();
    let (client, shell) = (&acp.client, acp.first.clone());
    let (id, _, lease) = client
        .acp_start("agent-session".into(), shell.clone())
        .unwrap();
    let finish = acp.home.path().join("allow-natural-completion");
    client
        .acp_prompt(
            id.clone(),
            shell.clone(),
            format!("finish-on-file:{}", finish.display()),
        )
        .unwrap();
    let phase = acp.server.acp.cancel_after_reservation(&id, &shell, || {
        // No timing window: the fixture cannot finish before this cancel has
        // reserved the turn, then it completes before dispatch is attempted.
        fs::write(&finish, b"").unwrap();
        let status = acp.wait_status(&id, &shell, "natural completion", |status| {
            !status.turn_active
        });
        assert_eq!(
            status.last_stop_reason,
            Some(marsh_acp::StopReason::EndTurn)
        );
    });
    client
        .acp_prompt(id.clone(), shell.clone(), "same session next turn".into())
        .unwrap();
    let status = acp.wait_status(&id, &shell, "next turn ends", |status| {
        !status.turn_active && status.turns.len() == 2
    });
    assert_eq!(
        status.last_stop_reason,
        Some(marsh_acp::StopReason::EndTurn)
    );
    client.acp_stop(id, shell).unwrap();
    drop(lease);
    assert_eq!(phase.unwrap(), "AlreadyFinished");
}

#[test]
fn acp_handoff_waits_for_release_and_prompt_admission() {
    let acp = AcpFixture::new();
    let (server, first, second) = (&acp.server, acp.first.clone(), acp.second.clone());
    let (id, _, lease) = acp
        .client
        .acp_start("agent-session".into(), first.clone())
        .unwrap();

    // A second release and handoff may begin after the first authorization,
    // but they cannot finish before that release clears its own lease.
    let (stale_release, attach) =
        acp_handoff_during_admission(server, &id, &first, &second, |hook| {
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
    let (release, attach) = acp_handoff_during_admission(server, &id, &first, &second, |hook| {
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
}

/// The daemon can be dropped while a turn still waits on its Kit: the Kit has
/// not answered, or its exit has not yet reached the transport (macOS can
/// delay that EOF by losing the bridge's shutdown wakeup). The drop must not
/// wait for the turn to end.
#[test]
fn acp_daemon_drop_does_not_wait_for_an_active_turn() {
    let acp = AcpFixture::new();
    let (server, shell) = (Arc::clone(&acp.server), acp.first.clone());
    let (id, _) = server
        .acp
        .start_reserved(
            Arc::clone(&server.backend),
            &server.store(),
            "agent-session",
            shell.clone(),
            &Arc::new(AcpStartupAbort::default()),
            None,
        )
        .unwrap();
    server
        .acp
        .prompt_with_key(&id, &shell, &Uuid::new_v4().to_string(), "hold".into())
        .unwrap();
    wait_until("the held turn records its first update", || {
        let status = server.acp.status(&id, &shell, 0, &server.store()).unwrap();
        status.turn_active && !status.updates.is_empty()
    });
    // The fixture's drop is then the manager's last owner.
    drop(server);
    wait_until("connection handlers release the ACP manager", || {
        Arc::strong_count(&acp.server.acp) == 1
    });
    returns("the daemon drops with a turn in flight", move || drop(acp));
}

#[test]
fn acp_daemon_caller_has_distinct_identity_controller_and_stop() {
    let acp = AcpFixture::new();
    let (client, first, second) = (&acp.client, acp.first.clone(), acp.second.clone());
    let other = acp
        .server
        .store()
        .attach_shell(3, authority("/Users/example/other", acp.home.path()));
    let other = session(&other, acp.home.path());
    let (agent_id, job_id, lease) = client
        .acp_start("agent-session".into(), first.clone())
        .unwrap();
    assert_ne!(agent_id, acp.first_id);
    assert_ne!(agent_id, "remote-session");
    assert_ne!(job_id, agent_id);
    assert_eq!(client.acp_list(first.clone()).unwrap().len(), 1);
    assert!(client.acp_status(agent_id.clone(), other, 0).is_err());
    assert!(
        client
            .acp_status("forged-agent-id".into(), first.clone(), 0)
            .is_err()
    );
    assert!(client.acp_attach(agent_id.clone(), second.clone()).is_err());
    client
        .acp_prompt(agent_id.clone(), first.clone(), "hello".into())
        .unwrap();
    let first_turn = acp.wait_status(&agent_id, &first, "first turn ends", |status| {
        status.last_stop_reason.is_some()
    });
    assert!(
        !first_turn.updates.is_empty(),
        "completed turn must expose every queued update before its stop reason"
    );
    let cursor = first_turn.next_cursor;
    client
        .acp_prompt(agent_id.clone(), first.clone(), "hold".into())
        .unwrap();
    wait_until("held turn reports progress", || {
        !client
            .acp_status(agent_id.clone(), first.clone(), cursor)
            .unwrap()
            .updates
            .is_empty()
    });
    // The agent may confirm the cancel before the call returns: either phase is
    // a delivered cancel, and the turn's stop reason below is the contract.
    let phase = client.acp_cancel(agent_id.clone(), first.clone()).unwrap();
    assert!(
        matches!(phase.as_str(), "Dispatched" | "Confirmed"),
        "{phase}"
    );
    acp.wait_status(&agent_id, &first, "held turn is cancelled", |status| {
        status.last_stop_reason == Some(marsh_acp::StopReason::Cancelled)
    });
    client.acp_release(agent_id.clone(), first).unwrap();
    client.acp_attach(agent_id.clone(), second.clone()).unwrap();
    client.acp_stop(agent_id.clone(), second.clone()).unwrap();
    acp.wait_status(&agent_id, &second, "stopped Kit ends", |status| {
        status.attachment.terminal.is_some()
    });
    drop(lease);
}

#[test]
fn acp_permission_requires_controller_and_offered_one_time_choice() {
    let acp = AcpFixture::new();
    let (client, first, second) = (&acp.client, acp.first.clone(), acp.second.clone());
    let (id, _, lease) = client
        .acp_start("agent-session".into(), first.clone())
        .unwrap();
    client
        .acp_prompt(id.clone(), first.clone(), "permission".into())
        .unwrap();
    let request = acp
        .wait_status(&id, &first, "permission request surfaces", |status| {
            !status.permissions.is_empty()
        })
        .permissions[0]
        .clone();
    assert_eq!(request.tool_call_id, "tool-1");
    assert_eq!(
        request.options.len(),
        2,
        "persistent grant must not be offered"
    );
    assert!(
        client
            .acp_status(id.clone(), second.clone(), 0)
            .unwrap()
            .permissions
            .is_empty(),
        "another same-project shell cannot inspect pending permission input"
    );
    assert!(
        client
            .acp_respond(
                id.clone(),
                second,
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
                first.clone(),
                request.request_id.clone(),
                "always".into(),
            )
            .is_err(),
        "persistent option is not allowed"
    );
    client
        .acp_respond(id.clone(), first.clone(), request.request_id, "once".into())
        .unwrap();
    let status = acp.turn_ended(&id, &first, "approved turn ends");
    assert!(status.permissions.is_empty(), "{:?}", status.permissions);
    client.acp_stop(id, first).unwrap();
    drop(lease);
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
    let launched = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    let acp = AcpFixture::with_backend(
        Arc::new(StalledAcpKit {
            launched: Arc::clone(&launched),
            terminated: Arc::clone(&terminated),
        }),
        None,
    );
    let marker = acp.home.path().join("startup-marker");
    let ready = acp.home.path().join("startup-ready");
    let mut child = acp.lease_child(&acp.first_id, &marker, Some(&ready));
    wait_until("ACP Kit starts", || launched.load(Ordering::Acquire));
    assert!(ready.exists());
    assert!(!marker.exists(), "ACP handshake unexpectedly completed");
    signal_child(&child, rustix::process::Signal::KILL);
    child.wait().unwrap();
    wait_until("orphaned ACP startup Kit is terminated", || {
        terminated.load(Ordering::Acquire)
    });
    assert!(
        acp.client
            .acp_list(acp.first.clone())
            .unwrap()
            .iter()
            .all(|entry| entry.terminal && entry.state.as_deref() == Some("failed"))
    );
}

#[test]
fn killed_or_interrupted_run_process_stops_kit_and_releases_controller() {
    let acp = AcpFixture::new();
    for signal in [rustix::process::Signal::KILL, rustix::process::Signal::INT] {
        let marker = acp.home.path().join(format!("lease-{signal:?}"));
        let child = acp.lease_child(&acp.first_id, &marker, None);
        let id = wait_for("ACP child starts its session", || {
            fs::read_to_string(&marker).ok().filter(|id| !id.is_empty())
        });
        signal_child(&child, signal);
        let mut child = child;
        returns("ACP child exits", move || child.wait().unwrap());
        acp.wait_status(
            &id,
            &acp.first,
            "Kit stops and releases its controller",
            |status| {
                status.controller_shell_session_id.is_none() && status.attachment.terminal.is_some()
            },
        );
        acp.client
            .acp_attach(id.clone(), acp.second.clone())
            .unwrap();
        assert!(
            acp.client
                .acp_prompt(id, acp.second.clone(), "late".into())
                .is_err()
        );
    }
}

#[test]
fn ended_shell_releases_acp_controller_without_explicit_detach() {
    let acp = AcpFixture::new();
    let client = &acp.client;
    let (id, _, lease) = client
        .acp_start("agent-session".into(), acp.first.clone())
        .unwrap();
    assert!(matches!(
        client.request(PublicRequest::OpenShell(ShellSpec {
            dev: false,
            arguments: vec![],
            session: acp.first.clone(),
            terminal_environment: std::collections::BTreeMap::new(),
        })),
        Ok(PublicReply::ShellAccepted)
    ));
    wait_until("the ended shell releases its ACP controller", || {
        client.acp_attach(id.clone(), acp.second.clone()).is_ok()
    });
    drop(lease);
    assert!(
        client
            .acp_status(id.clone(), acp.second.clone(), 0)
            .unwrap()
            .attachment
            .terminal
            .is_none(),
        "explicit handoff keeps the Kit running"
    );
    client.acp_stop(id, acp.second.clone()).unwrap();
}

#[test]
fn handed_off_kit_stops_when_last_controller_detaches_after_run_lease_loss() {
    let acp = AcpFixture::new();
    let (client, first, second) = (&acp.client, acp.first.clone(), acp.second.clone());
    let (id, _, lease) = client
        .acp_start("agent-session".into(), first.clone())
        .unwrap();
    client.acp_release(id.clone(), first.clone()).unwrap();
    client.acp_attach(id.clone(), second.clone()).unwrap();
    drop(lease);
    // Fence the lease callback before B detaches; the socket handler may
    // independently observe the same close, which is intentionally idempotent.
    acp.server
        .acp
        .run_disconnected(&id, &acp.first_id, &acp.server.store());
    let status = client.acp_status(id.clone(), second.clone(), 0).unwrap();
    assert_eq!(
        status.controller_shell_session_id,
        Some(acp.second_id.clone())
    );
    assert!(!status.stopping);
    client
        .acp_prompt(id.clone(), second.clone(), "hello".into())
        .unwrap();
    acp.turn_ended(&id, &second, "handed-off controller's turn ends");
    assert!(matches!(
        client.request(PublicRequest::DetachShell {
            session_id: acp.second_id.clone()
        }),
        Ok(PublicReply::Detached)
    ));
    let status = acp.wait_status(&id, &first, "orphaned Kit is stopped", |status| {
        status.attachment.terminal.is_some()
    });
    assert!(status.stopping);
    assert!(status.controller_shell_session_id.is_none());
    assert_eq!(status.receipt.unwrap().cleanup, CleanupState::Verified);
}

#[test]
fn handed_off_controller_can_detach_while_run_lease_is_live() {
    let acp = AcpFixture::new();
    let (client, first, second) = (&acp.client, acp.first.clone(), acp.second.clone());
    let (id, _, lease) = client
        .acp_start("agent-session".into(), first.clone())
        .unwrap();
    client.acp_release(id.clone(), first.clone()).unwrap();
    client.acp_attach(id.clone(), second).unwrap();
    assert!(matches!(
        client.request(PublicRequest::DetachShell {
            session_id: acp.second_id.clone()
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
            bytes: b"stderr secret".to_vec(),
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

/// The handshake deadline is absolute: a peer trickling one byte at a time
/// (each well inside any idle timeout) still times out at the deadline.
#[test]
fn daemon_handshake_deadline_survives_trickled_frame_bytes() {
    let (mut writer, mut reader) = socket_pair();
    let deadline = Instant::now() + Duration::from_millis(100);
    let sender = thread::spawn(move || {
        // A 64-byte frame, one byte per 10 ms: complete only long after the
        // deadline, and then not valid JSON.
        writer.write_all(&64_u32.to_be_bytes()).unwrap();
        for _ in 0..64 {
            thread::sleep(Duration::from_millis(10));
            if writer.write_all(b" ").is_err() {
                break;
            }
        }
    });
    let result = read_frame::<PublicRequest>(&mut DeadlineRead {
        stream: &mut reader,
        deadline,
    });
    assert!(
        matches!(&result, Err(DaemonError::Io(error))
            if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)),
        "{result:?}"
    );
    drop(reader);
    sender.join().unwrap();
}

/// Job input/output and agent prompts flow through the daemon but never into
/// its durable receipt journal, which is owner-only.
#[test]
fn receipt_journal_is_owner_only_and_keeps_no_job_or_prompt_content() {
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
    let (stop, task) = start_server(Arc::clone(&server));
    let client = Client::connect(&selected_home).unwrap();
    let PublicReply::ShellAttached { session_id } = client
        .request(PublicRequest::AttachShell {
            pid: 7,
            session: authority(project.to_str().unwrap(), &selected_home),
        })
        .unwrap()
    else {
        panic!("attach failed");
    };
    let mut selected = session(&session_id, &selected_home);
    selected.launch_directory = project;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = client
        .execute_with_io(
            ExecuteSpec {
                placement: Placement::Local,
                environment: BTreeMap::new(),
                command: "fixture".into(),
                arguments: Vec::new(),
                session: selected,
                process: None,
                working_directory: None,
            },
            Cursor::new(b"stdout secret".to_vec()),
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
    assert_eq!(code, 23);
    assert_eq!(stdout, b"stdout secret");
    assert_eq!(stderr, b"stderr secret");
    assert_eq!(client.jobs().unwrap().jobs[0].exit_code, Some(23));
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();

    let acp = AcpFixture::new();
    let (id, _, lease) = acp
        .client
        .acp_start("agent-session".into(), acp.first.clone())
        .unwrap();
    acp.client
        .acp_prompt(id.clone(), acp.first.clone(), "raw prompt secret".into())
        .unwrap();
    acp.wait_status(&id, &acp.first, "prompt turn ends", |status| {
        status.last_stop_reason == Some(marsh_acp::StopReason::EndTurn)
    });
    acp.client.acp_stop(id, acp.first.clone()).unwrap();
    drop(lease);

    for home in [selected_home.as_path(), acp.home.path()] {
        assert_eq!(
            fs::metadata(home.join("state")).unwrap().mode() & 0o777,
            0o700
        );
        let journal = receipt_journal::journal_path(home);
        assert_eq!(fs::metadata(&journal).unwrap().mode() & 0o777, 0o600);
        let bytes = fs::read(journal).unwrap();
        assert!(receipt_journal::record_count(home) > 0);
        for forbidden in [
            b"stdout secret".as_slice(),
            b"stderr secret",
            b"raw prompt secret",
            b"\"prompt\"",
            b"\"stdout\"",
            b"\"stderr\"",
        ] {
            assert!(
                !bytes.windows(forbidden.len()).any(|part| part == forbidden),
                "journal holds {}",
                String::from_utf8_lossy(forbidden)
            );
        }
    }
}
