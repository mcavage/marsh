//! `write_request` and `after_failed_write` (the client's recovery of a
//! refusal after a failed request write), and that every client request
//! write uses them.

use super::*;

/// A connection whose peer has said `said` and closed: the client's request
/// write fails with a broken pipe (or reset), with `said` queued to read.
fn closed_peer(said: &[u8]) -> UnixStream {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(said).unwrap();
    drop(peer);
    client
}

fn refusal_bytes(message: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_frame(
        &mut bytes,
        &PublicReply::Error {
            code: ErrorCode::InvalidRequest,
            message: message.to_owned(),
        },
    )
    .unwrap();
    bytes
}

fn failed_write(said: &[u8]) -> DaemonError {
    let mut client = closed_peer(said);
    write_request(&mut client, &"a request").unwrap_err()
}

fn is_transport_failure(error: &DaemonError) -> bool {
    matches!(error, DaemonError::Io(io)
        if matches!(io.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset))
}

#[test]
fn a_failed_write_reads_the_refusal_the_peer_queued() {
    let error = failed_write(&refusal_bytes("marsh: busy; try again"));
    assert!(matches!(&error, DaemonError::Remote(message) if message == "marsh: busy; try again"));
}

#[test]
fn a_failed_write_keeps_its_own_error_when_the_peer_left_no_refusal() {
    // Nothing queued: a daemon that simply went away.
    assert!(is_transport_failure(&failed_write(b"")));
    // A frame that stops short (a header, then part of a body).
    let whole = refusal_bytes("marsh: busy; try again");
    assert!(is_transport_failure(&failed_write(&whole[..2])));
    assert!(is_transport_failure(&failed_write(
        &whole[..whole.len() - 1]
    )));
    // A header that promises more than any frame may be.
    assert!(is_transport_failure(&failed_write(&u32::MAX.to_be_bytes())));
    // Bytes that are not a frame of JSON.
    let mut garbage = 5_u32.to_be_bytes().to_vec();
    garbage.extend_from_slice(b"not j");
    assert!(is_transport_failure(&failed_write(&garbage)));
    // A reply that is not a refusal is not a reason for a failed write.
    let mut accepted = Vec::new();
    write_frame(&mut accepted, &PublicReply::ExecutionAccepted).unwrap();
    assert!(is_transport_failure(&failed_write(&accepted)));
}

#[test]
fn only_a_closed_peer_makes_a_write_error_read_for_a_refusal() {
    let mut client = closed_peer(&refusal_bytes("marsh: busy; try again"));
    let other = DaemonError::InvalidState("not a transport failure".into());
    // The queued refusal is left alone: this error was not the peer closing.
    assert!(matches!(
        after_failed_write(&mut client, other),
        DaemonError::InvalidState(_)
    ));
    // And a successful write never reads.
    let (mut client, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(&refusal_bytes("marsh: busy; try again"))
        .unwrap();
    write_request(&mut client, &"hello").unwrap();
    assert!(matches!(
        read_frame::<PublicReply>(&mut client),
        Ok(PublicReply::Error { .. })
    ));
}

/// Production source of a file, without its test module.
fn production(source: &str) -> &str {
    source
        .split("\n#[cfg(test)]\nmod tests")
        .next()
        .unwrap_or(source)
}

/// The methods of the source's `impl` blocks (four-space indentation), each
/// up to its closing brace.
fn methods(source: &str) -> Vec<&str> {
    let mut starts = source
        .match_indices("\n    pub fn ")
        .chain(source.match_indices("\n    fn "))
        .chain(source.match_indices("\n    pub(crate) fn "))
        .map(|(at, _)| at)
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts
        .into_iter()
        .map(|start| {
            let rest = &source[start..];
            &rest[..rest.find("\n    }\n").map_or(rest.len(), |end| end + 7)]
        })
        .collect()
}

#[test]
fn every_client_request_write_goes_through_the_refusal_recovery() {
    let sources = [
        ("lib.rs", include_str!("lib.rs")),
        ("acp_session.rs", include_str!("acp_session.rs")),
    ];
    let mut clients = 0;
    for (file, source) in sources {
        for method in methods(production(source)) {
            if !method.contains("connect_unix(&self.paths.socket)") {
                continue;
            }
            clients += 1;
            let name = method.lines().nth(1).unwrap_or_default().trim();
            // The request is written by `write_request`, or by a writer of its
            // own that hands a failure to `after_failed_write`; never by a
            // bare `write_frame` or `write_all`.
            let recovers =
                method.contains("write_request(") || method.contains("after_failed_write(");
            assert!(
                recovers,
                "{file}: `{name}` writes its request without the refusal recovery"
            );
            if !method.contains("after_failed_write(") {
                assert!(
                    !method.contains(".write_all(")
                        && !method.contains("write_frame(&mut stream, &Envelope"),
                    "{file}: `{name}` writes its request with a bare write"
                );
            }
        }
        // Every request envelope is built by a method counted above.
        for method in methods(production(source)) {
            if method.contains("Envelope {") {
                assert!(
                    method.contains("write_request(")
                        || method.contains("serde_json::to_vec(&envelope)"),
                    "{file}: an Envelope outside a recovering writer: {}",
                    method.lines().nth(1).unwrap_or_default()
                );
            }
        }
    }
    assert!(
        clients >= 14,
        "the scan found only {clients} client request writers"
    );
}

/// What a process run reports for one scripted daemon connection.
fn run_against(
    reply: impl FnOnce(&mut UnixStream) + Send + 'static,
) -> Result<i32, ProcessRunError> {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let peer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _: serde_json::Value = read_frame(&mut stream).unwrap();
        reply(&mut stream);
    });
    let spec = ExecuteSpec {
        model: None,
        usage: None,
        label: None,
        kit: None,
        command: "fixture".into(),
        arguments: vec![],
        placement: Placement::Local,
        environment: marsh_contracts::ExportedEnvironment::default(),
        working_directory: None,
        session: SessionSpec {
            session_id: "s".into(),
            username: "u".into(),
            uid: 1000,
            gid: 1000,
            launch_directory: "/p".into(),
            guest_home: "/h".into(),
            home_backing: "/b".into(),
            ephemeral_home: false,
            terminal: false,
            terminal_size: None,
        },
        process: None,
        acp: false,
    };
    let result = Client::capability(&socket).run_process_with_io_attempt(
        spec,
        std::io::empty(),
        std::io::sink(),
        std::io::sink(),
        &mut |_| {},
    );
    peer.join().unwrap();
    result
}

#[test]
fn a_process_run_says_whether_the_daemon_may_have_admitted_it() {
    // A refusal in place of the admission: not admitted.
    let refused = run_against(|stream| {
        write_frame(
            stream,
            &PublicReply::Error {
                code: ErrorCode::InvalidRequest,
                message: "marsh: busy; try again".into(),
            },
        )
        .unwrap();
    });
    assert!(matches!(
        refused,
        Err(ProcessRunError::NotAdmitted(DaemonError::Remote(_)))
    ));
    // The reply lost after a whole request, or not an admission at all: the
    // daemon may have admitted it.
    let lost = run_against(|_| {});
    assert!(
        matches!(lost, Err(ProcessRunError::MaybeAdmitted(_))),
        "{lost:?}"
    );
    let odd = run_against(|stream| write_frame(stream, &PublicReply::ShuttingDown).unwrap());
    assert!(
        matches!(odd, Err(ProcessRunError::MaybeAdmitted(_))),
        "{odd:?}"
    );
    // Any failure after the admission, whatever its text.
    let failed = run_against(|stream| {
        write_frame(stream, &PublicReply::ExecutionAccepted).unwrap();
        let _: AttachmentFrame = read_frame(stream).unwrap();
        write_frame(
            stream,
            &AttachmentFrame::Failed {
                message: "marsh: busy; try again".into(),
            },
        )
        .unwrap();
    });
    assert!(
        matches!(&failed, Err(ProcessRunError::MaybeAdmitted(DaemonError::Remote(m))) if m == "marsh: busy; try again"),
        "{failed:?}"
    );
    assert!(![refused, lost, odd, failed].iter().any(|result| {
        result
            .as_ref()
            .is_err_and(|e| matches!(e, ProcessRunError::MaybeAdmitted(_)) && e.is_retryable())
    }));
}
