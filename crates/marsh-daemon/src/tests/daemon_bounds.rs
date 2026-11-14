//! Daemon resource bounds, through the real socket: each test drives one
//! path to its bound, then checks that a host request still gets its reply.
use super::*;

/// Argv branches that end at once with status 0; no job runs.
#[derive(Debug)]
struct Instant0;

impl DaemonBackend for Instant0 {
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
        _: ExecuteSpec,
        attachment: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        attachment.send(&AttachmentFrame::Exited { code: 0 })
    }

    fn open_shell(
        &self,
        _: ShellSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::InvalidState("no shells here".into()))
    }
}

struct Daemon {
    _root: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    server: Arc<Server>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl Daemon {
    fn start(configure: impl FnOnce(Server) -> Server) -> Self {
        let root = home();
        let home = root.path().join("scope");
        let project = root.path().join("project");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&project).unwrap();
        fs::write(project.join("file"), "x\n").unwrap();
        let project = project.canonicalize().unwrap();
        let server = Arc::new(configure(
            Server::bind(&home)
                .unwrap()
                .with_backend(Arc::new(Instant0)),
        ));
        let (stop, task) = start_server(Arc::clone(&server));
        Self {
            _root: root,
            home,
            project,
            server,
            stop,
            task: Some(task),
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.home).unwrap()
    }

    /// A host request answers within a short budget.
    fn answers(&self) {
        let client = self.client();
        returns("a host status request", move || {
            client.status(None).unwrap()
        });
    }

    /// A host CLI's session, which lasts one split.
    fn session(&self) -> SessionSpec {
        let authority = authority(self.project.to_str().unwrap(), &self.home);
        let session_id = self
            .server
            .store
            .attach_shell(std::process::id(), authority.clone());
        split::session_spec(&session_id, &authority)
    }

    fn spec(&self, branches: usize, session: SessionSpec) -> split::SplitCreateSpec {
        split::SplitCreateSpec {
            session,
            cwd: self.project.clone(),
            branches: (0..branches)
                .map(|index| split::BranchSpec {
                    label: format!("b{index}"),
                    shell: None,
                    argv: Some(vec![b"fixture".to_vec()]),
                })
                .collect(),
            environment: marsh_contracts::ExportedEnvironment::default(),
            start_env: BTreeMap::new(),
            detach: false,
            open: false,
            from: None,
            label: None,
            model: None,
            usage: None,
            spawn: None,
        }
    }

    /// One split of `branches` argv branches, run to its end and joined.
    fn split(&self, branches: usize) {
        let spec = self.spec(branches, self.session());
        let mut stream = self
            .client()
            .open_request(PublicRequest::SplitCreate(spec))
            .unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        write_frame(&mut stream, &AttachmentFrame::StdinEof).unwrap();
        let PublicReply::SplitStarted { id } = read_frame(&mut stream).unwrap() else {
            panic!("split refused")
        };
        let finished = read_frame::<PublicReply>(&mut stream).unwrap();
        assert!(
            matches!(finished, PublicReply::SplitFinished { status: 0, .. }),
            "{finished:?}"
        );
        drop(stream);
        let mut join = self
            .client()
            .open_request(PublicRequest::SplitJoin { split: id })
            .unwrap();
        join.set_read_timeout(Some(PATIENCE)).unwrap();
        let joined = read_frame::<PublicReply>(&mut join).unwrap();
        assert!(
            matches!(joined, PublicReply::SplitJoined { .. }),
            "{joined:?}"
        );
        write_frame(
            &mut join,
            &split::SplitRelease {
                status: 0,
                keep: false,
                consumer: None,
            },
        )
        .unwrap();
        let done = read_frame::<PublicReply>(&mut join).unwrap();
        assert!(matches!(done, PublicReply::SplitDone { .. }), "{done:?}");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            task.join().unwrap();
        }
    }
}

fn open_descriptors() -> usize {
    fs::read_dir("/dev/fd").unwrap().count()
}

/// A-02: a finished split keeps neither its branches' daemon connections nor
/// its runtime; the descriptor count returns to where it was.
#[test]
fn finished_splits_release_their_branch_connections_and_runtime() {
    let daemon = Daemon::start(|server| server);
    // The first split warms whatever is opened once (journals, threads).
    daemon.split(4);
    wait_until("the first split's runtime is released", || {
        daemon.server.splits.runtimes() == 0
    });
    let baseline = open_descriptors();
    for _ in 0..25 {
        daemon.split(4);
    }
    // 100 branches: a leak of three descriptors each would be 300.
    wait_until("split runtimes are released", || {
        daemon.server.splits.runtimes() == 0
    });
    wait_until("branch descriptors are closed", || {
        open_descriptors() <= baseline + 8
    });
    daemon.answers();
}

/// Argv branches that end at once, except one whose argument is `hold`,
/// which runs until released.
#[derive(Debug)]
struct HoldOne(Arc<AtomicBool>);

impl DaemonBackend for HoldOne {
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
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        if request.arguments.iter().any(|argument| argument == b"hold") {
            while !self.0.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
        }
        attachment.send(&AttachmentFrame::Exited { code: 0 })
    }

    fn open_shell(
        &self,
        _: ShellSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::InvalidState("no shells here".into()))
    }
}

/// A-02: a branch's daemon connection goes when the branch ends, while its
/// siblings still run and while the creator still holds its create stream.
#[test]
fn an_ended_branch_releases_its_connection_while_the_split_runs_on() {
    let release = Arc::new(AtomicBool::new(false));
    let backend = Arc::new(HoldOne(Arc::clone(&release)));
    let daemon = Daemon::start(|server| server.with_backend(backend));
    let mut spec = daemon.spec(4, daemon.session());
    spec.branches[0].argv = Some(vec![b"fixture".to_vec(), b"hold".to_vec()]);
    let mut stream = daemon
        .client()
        .open_request(PublicRequest::SplitCreate(spec))
        .unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    write_frame(&mut stream, &AttachmentFrame::StdinEof).unwrap();
    let PublicReply::SplitStarted { .. } = read_frame(&mut stream).unwrap() else {
        panic!("split refused")
    };
    wait_until("the three quick branches release their connections", || {
        daemon.server.splits.listed_executions() == 1
    });
    release.store(true, Ordering::Release);
    let finished = read_frame::<PublicReply>(&mut stream).unwrap();
    assert!(
        matches!(finished, PublicReply::SplitFinished { status: 0, .. }),
        "{finished:?}"
    );
    // The create stream is still open, so the runtime stays; its branches'
    // connections do not.
    assert_eq!(daemon.server.splits.runtimes(), 1);
    assert_eq!(daemon.server.splits.listed_executions(), 0);
    daemon.answers();
}

/// A-02: a client that goes away before it reads `SplitStarted` leaves no
/// runtime and no running split behind.
#[test]
fn a_split_whose_client_left_before_it_started_leaves_nothing_running() {
    let daemon = Daemon::start(|server| server);
    let spec = daemon.spec(1, daemon.session());
    let stream = daemon
        .client()
        .open_request(PublicRequest::SplitCreate(spec))
        .unwrap();
    stream.shutdown(std::net::Shutdown::Read).unwrap();
    let mut writer = &stream;
    write_frame(&mut writer, &AttachmentFrame::StdinEof).unwrap();
    drop(stream);
    wait_until("the abandoned split's runtime is released", || {
        daemon.server.splits.runtimes() == 0
    });
    thread::sleep(Duration::from_millis(1500));
    assert_eq!(daemon.server.splits.runtimes(), 0);
    assert_eq!(daemon.server.splits.counts().active, 0);
    daemon.answers();
}
