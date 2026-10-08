//! Actual authenticated socket admission, not a Docker/Cloud execution claim.
use super::*;
use std::{os::unix::ffi::OsStrExt as _, sync::atomic::AtomicUsize};

#[derive(Debug)]
struct CwdObserver(Arc<AtomicUsize>);

impl DaemonBackend for CwdObserver {
    fn open_shell(
        &self,
        _: ShellSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::InvalidState(
            "test backend does not open shells".into(),
        ))
    }

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
        panic!("cwd rejection must precede preparation")
    }

    fn execute(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        // Observe the real decoded request after authentication, without
        // reimplementing admission or touching a caller-selected host path.
        attachment.send(&AttachmentFrame::Stdout {
            bytes: serde_json::to_vec(&request)?,
        })?;
        attachment.send(&AttachmentFrame::Exited { code: 0 })
    }
}

struct CwdServer {
    _root: tempfile::TempDir,
    server: Arc<Server>,
    client: Client,
    session: SessionSpec,
    project: PathBuf,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl CwdServer {
    fn new() -> Self {
        let root = home();
        let selected = root.path().join("selected");
        let project = root.path().join("project");
        fs::create_dir(&selected).unwrap();
        fs::create_dir(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server = Arc::new(
            Server::bind(&selected)
                .unwrap()
                .with_backend(Arc::new(CwdObserver(Arc::clone(&calls)))),
        );
        let (stop, task) = start_server(Arc::clone(&server));
        let owner = Client::connect(&selected).unwrap();
        let mut approved = authority(project.to_str().unwrap(), &selected);
        approved.guest_home = "/selected-guest-home".into();
        let PublicReply::ShellAttached { session_id } = owner
            .request(PublicRequest::AttachShell {
                pid: std::process::id(),
                session: approved,
            })
            .unwrap()
        else {
            panic!("attach failed")
        };
        let mut requested = session(&session_id, &selected);
        // Deliberately forged caller authority must not broaden cwd admission.
        requested.launch_directory = "/forged-project".into();
        requested.guest_home = "/".into();
        let client = Client {
            paths: owner.paths.clone(),
            token: server.store.issue_relay_token(&session_id).unwrap(),
        };
        Self {
            _root: root,
            server,
            client,
            session: requested,
            project,
            calls,
            stop,
            task: Some(task),
        }
    }

    fn request(&self, cwd: Option<serde_json::Value>, placement: &str) -> UnixStream {
        let mut body = serde_json::json!({
            "type": "execute", "command": "fixture", "arguments": [],
            "placement": placement, "environment": {}, "session": self.session,
        });
        if let Some(cwd) = cwd {
            body["working_directory"] = cwd;
        }
        let mut stream = UnixStream::connect(&self.client.paths.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.client.token.clone(),
                body,
            },
        )
        .unwrap();
        stream
    }

    fn accepted(&self, cwd: Option<serde_json::Value>, placement: &str) -> serde_json::Value {
        let mut stream = self.request(cwd, placement);
        assert!(matches!(
            read_frame::<PublicReply>(&mut stream).unwrap(),
            PublicReply::ExecutionAccepted
        ));
        let AttachmentFrame::Stdout { bytes } = read_frame(&mut stream).unwrap() else {
            panic!("request not delivered")
        };
        assert!(matches!(
            read_frame::<AttachmentFrame>(&mut stream).unwrap(),
            AttachmentFrame::Exited { code: 0 }
        ));
        serde_json::from_slice(&bytes).unwrap()
    }
}

impl Drop for CwdServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.client.request(PublicRequest::Ping);
        if let Some(task) = self.task.take() {
            task.join().unwrap();
        }
    }
}

#[test]
fn execution_cwd_keeps_raw_child_path_distinct_from_authenticated_root() {
    let fixture = CwdServer::new();
    let mut raw_child = fixture.project.as_os_str().as_bytes().to_vec();
    raw_child.extend(b"/not-present-on-host-\xff\xfe");
    // No such child is created on the host: admission must be lexical, not
    // speculative host resolution of a guest path.
    {
        let placement = "local";
        for cwd in [
            raw_child.clone(),
            b"/selected-guest-home/child-\xfe".to_vec(),
        ] {
            let request = fixture.accepted(Some(serde_json::json!(cwd)), placement);
            assert_eq!(request["working_directory"], serde_json::json!(cwd));
            assert_eq!(
                request["session"]["launch_directory"],
                fixture.project.to_str().unwrap()
            );
            assert_eq!(request["session"]["guest_home"], "/selected-guest-home");
        }
    }
    let legacy = fixture.accepted(None, "local");
    assert!(legacy.get("working_directory").is_none());
    assert_eq!(
        legacy["session"]["launch_directory"],
        fixture.project.to_str().unwrap()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
    assert!(fixture.server.store.jobs().jobs.is_empty());
}

#[test]
fn execution_cwd_denies_outside_and_malformed_before_acceptance_or_backend() {
    let fixture = CwdServer::new();
    let sibling = [fixture.project.as_os_str().as_bytes(), b"-sibling"].concat();
    {
        let placement = "local";
        for cwd in [
            b"/tmp".to_vec(),
            b"/selected-guest-home-evil/child".to_vec(),
            sibling.clone(),
        ] {
            let mut stream = fixture.request(Some(serde_json::json!(cwd)), placement);
            assert!(matches!(
                read_frame::<PublicReply>(&mut stream).unwrap(),
                PublicReply::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                }
            ));
        }
    }
    for cwd in [
        serde_json::json!("/selected-guest-home/child"),
        serde_json::Value::Null,
        serde_json::json!(b"relative"),
        serde_json::json!(b"/selected-guest-home/../outside"),
        serde_json::json!(b"/selected-guest-home/nul\0x"),
        serde_json::json!(vec![b'/'; 4097]),
    ] {
        let mut stream = fixture.request(Some(cwd), "local");
        let reply = read_frame::<PublicReply>(&mut stream);
        assert!(!matches!(reply, Ok(PublicReply::ExecutionAccepted)));
        assert!(reply.is_err() || matches!(reply, Ok(PublicReply::Error { .. })));
    }
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        0,
        "denial must precede backend effects"
    );
    assert!(fixture.server.store.jobs().jobs.is_empty());
}
