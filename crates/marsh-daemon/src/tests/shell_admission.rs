//! Authenticated public sockets with a real, exactly owned child process.
//! Admission/receipt tests, not stock VM/containment qualification.
use super::*;
use std::{
    process::Stdio,
    sync::atomic::{AtomicI32, AtomicUsize},
    time::Instant,
};

#[derive(Default)]
struct Backend {
    starts: AtomicUsize,
    actual_exit: AtomicI32,
    uncertain: bool,
}

impl DaemonBackend for Backend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(Vec::new())
    }
    fn prepare(
        &self,
        _: &LoadSelection,
        _: &SessionSpec,
        _: PreparationProgress,
        _: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        unreachable!()
    }
    fn execute(
        &self,
        _: ExecuteSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }
    fn open_shell(
        &self,
        _: ShellSpec,
        attachment: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        use std::os::unix::process::CommandExt as _;
        let mut child = Command::new("/usr/bin/python3")
            .args(["-c", "import sys; sys.stdin.buffer.read(); sys.exit(42)"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        self.starts.fetch_add(1, Ordering::Release);
        let delivery = (|| {
            attachment.send(&AttachmentFrame::ShellReady)?;
            loop {
                match attachment.receive()? {
                    AttachmentFrame::StdinEof => return Ok::<(), DaemonError>(()),
                    AttachmentFrame::Signal { .. } => return Ok(()),
                    _ => {}
                }
            }
        })();
        drop(child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill()?;
                break child.wait()?;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let actual = status.code().unwrap_or(125);
        self.actual_exit.store(actual, Ordering::Release);
        if self.uncertain {
            return Err(DaemonError::ShellCleanupUncertain(format!(
                "fixture remote cleanup unknown; actual exit {actual}"
            )));
        }
        delivery?;
        attachment.send(&AttachmentFrame::Exited { code: actual })
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    server: Arc<Server>,
    backend: Arc<Backend>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
    client: Client,
    spec: ShellSpec,
}

impl Fixture {
    fn new(uncertain: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        fs::create_dir(&project).unwrap();
        let backend = Arc::new(Backend {
            uncertain,
            ..Backend::default()
        });
        let server = Arc::new(
            Server::bind(home.path())
                .unwrap()
                .with_backend(backend.clone()),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let serving = Arc::clone(&server);
        let stopping = Arc::clone(&stop);
        let task = thread::spawn(move || {
            serving
                .serve_until(|| stopping.load(Ordering::Acquire))
                .unwrap();
        });
        let client = Client::connect(home.path()).unwrap();
        let authority = authority(project.to_str().unwrap(), home.path());
        let PublicReply::ShellAttached { session_id } = client
            .request(PublicRequest::AttachShell {
                pid: std::process::id(),
                session: authority.clone(),
            })
            .unwrap()
        else {
            panic!("not attached")
        };
        let spec = ShellSpec {
            dev: false,
            arguments: Vec::new(),
            session: SessionSpec {
                session_id,
                username: authority.username,
                uid: authority.uid,
                gid: authority.gid,
                launch_directory: authority.launch_directory,
                guest_home: authority.guest_home,
                home_backing: authority.home_backing,
                ephemeral_home: false,
                terminal: false,
                terminal_size: None,
            },
        };
        Self {
            _home: home,
            server,
            backend,
            stop,
            task: Some(task),
            client,
            spec,
        }
    }
    fn state(&self) -> ShellState {
        self.client
            .status(None)
            .unwrap()
            .shells
            .into_iter()
            .find(|row| row.session_id == self.spec.session.session_id)
            // An ended shell (detached, authority released) is not listed.
            .map_or(ShellState::Detached, |row| row.state)
    }
    fn wait_state(&self, expected: ShellState) {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let state = self.state();
            if state == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "session remained {state:?}, expected {expected:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

#[test]
fn duplicate_open_and_public_detach_cannot_release_a_live_generation() {
    let f = Fixture::new(false);
    let first = f.client.start_shell(f.spec.clone()).unwrap();
    assert_eq!(first.receive().unwrap(), AttachmentFrame::ShellReady);
    let token = f
        .server
        .store()
        .issue_relay_token(&f.spec.session.session_id)
        .unwrap();
    // Preserve results until original owner has finished, even in a RED run.
    let duplicate = f.client.start_shell(f.spec.clone());
    if let Ok(second) = &duplicate {
        second.shutdown().unwrap();
    }
    let detach = f
        .client
        .request(PublicRequest::DetachShell {
            session_id: f.spec.session.session_id.clone(),
        })
        .unwrap();
    let retained = f.server.store().authentication_scope(&token).is_some();
    first.send(&AttachmentFrame::StdinEof).unwrap();
    assert_eq!(
        first.receive().unwrap(),
        AttachmentFrame::Exited { code: 42 }
    );
    f.wait_state(ShellState::Detached);
    assert!(
        duplicate.is_err(),
        "second OpenShell launched another process for one session"
    );
    assert!(
        matches!(detach, PublicReply::Error { .. }),
        "premature detach was accepted: {detach:?}"
    );
    assert!(retained, "rejected detach erased live relay authority");
    assert_eq!(f.backend.starts.load(Ordering::Acquire), 1);
    assert_eq!(f.backend.actual_exit.load(Ordering::Acquire), 42);
}

#[test]
fn acknowledgement_disconnect_has_no_orphaned_registration_or_running_child() {
    let f = Fixture::new(false);
    let mut raw = UnixStream::connect(&f.client.paths.socket).unwrap();
    write_frame(
        &mut raw,
        &Envelope {
            protocol: PROTOCOL.into(),
            token: f.client.token.clone(),
            body: PublicRequest::OpenShell(f.spec.clone()),
        },
    )
    .unwrap();
    raw.shutdown(Shutdown::Both).unwrap();
    drop(raw);
    // Linux reads the buffered request and admits it, then releases it as
    // Detached. macOS rejects socket options on a closed peer, so the daemon
    // drops the request before admission. Either way nothing is orphaned.
    let deadline = Instant::now() + Duration::from_secs(2);
    let detached = loop {
        if f.state() == ShellState::Detached {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let starts = f.backend.starts.load(Ordering::Acquire);
    assert!(starts <= 1);
    if detached {
        if starts == 1 {
            assert_eq!(f.backend.actual_exit.load(Ordering::Acquire), 42);
        }
    } else {
        // Dropped before admission: no child ran and the session is still
        // admissible (no orphaned attachment registration).
        assert_eq!(starts, 0);
        let retry = f.client.start_shell(f.spec.clone()).unwrap();
        assert_eq!(retry.receive().unwrap(), AttachmentFrame::ShellReady);
        retry.send(&AttachmentFrame::StdinEof).unwrap();
        assert_eq!(
            retry.receive().unwrap(),
            AttachmentFrame::Exited { code: 42 }
        );
        f.wait_state(ShellState::Detached);
    }
}

#[test]
fn uncertain_completion_retains_authority_and_refuses_reopen() {
    let f = Fixture::new(true);
    let attachment = f.client.start_shell(f.spec.clone()).unwrap();
    assert_eq!(attachment.receive().unwrap(), AttachmentFrame::ShellReady);
    attachment.send(&AttachmentFrame::StdinEof).unwrap();
    assert!(
        matches!(attachment.receive().unwrap(), AttachmentFrame::Failed { message } if message.contains("actual exit 42"))
    );
    f.wait_state(ShellState::CleanupUncertain);
    assert_eq!(f.backend.actual_exit.load(Ordering::Acquire), 42);
    assert!(matches!(
        f.client
            .request(PublicRequest::DetachShell {
                session_id: f.spec.session.session_id.clone()
            })
            .unwrap(),
        PublicReply::Error { .. }
    ));
    assert!(f.client.start_shell(f.spec.clone()).is_err());
    assert_eq!(f.backend.starts.load(Ordering::Acquire), 1);
}
