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

impl Daemon {
    /// A job of a session in the project, and its capability token.
    fn job(&self) -> (String, String) {
        let session = self.session();
        let (job, _) = self
            .server
            .store
            .begin_job(NewJob {
                label: None,
                session_id: session.session_id,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        let token = self.server.store.issue_capability_token(&job).unwrap();
        (job, token)
    }

    /// What a job's bridge does for one `cap.sock` channel: take the job's
    /// slot, then connect and send a `SplitCreate` that never sends its
    /// input's end, so the daemon holds the connection.
    fn held_split(
        &self,
        job: &str,
        token: &str,
    ) -> Result<(CapabilityConnection, UnixStream), String> {
        let slot = self.server.store.capability_connection(job)?;
        let mut stream = UnixStream::connect(&self.client().paths.socket).unwrap();
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: token.to_owned(),
                body: PublicRequest::SplitCreate(self.spec(1, self.session())),
            },
        )
        .unwrap();
        write_frame(
            &mut stream,
            &AttachmentFrame::Stdin {
                bytes: b"input".to_vec(),
            },
        )
        .unwrap();
        Ok((slot, stream))
    }
}

/// No reply yet: the daemon is still holding `stream` open.
fn held(stream: &UnixStream) -> bool {
    let mut stream = stream;
    if stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .is_err()
    {
        return false;
    }
    matches!(
        stream.read(&mut [0; 1]),
        Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
    )
}

/// A-03: one job holds at most twice `job-children`, plus 4, daemon
/// connections, jobs together at most three quarters of the cap, and the
/// host's quarter is left: a status and a jobs request still get through.
#[test]
fn jobs_never_take_the_hosts_share_of_daemon_connections() {
    let daemon = Daemon::start(|server| server.with_connection_cap(64));
    let per_job = connection_budget::per_job(daemon.server.store.job_limits().job_children);
    let share = connection_budget::capability_share(64);
    assert_eq!((per_job, share), (36, 48));
    let (first, first_token) = daemon.job();
    let mut holding = (0..per_job)
        .map(|_| daemon.held_split(&first, &first_token).unwrap())
        .collect::<Vec<_>>();
    assert!(
        held(&holding[0].1),
        "the daemon must hold the split's create"
    );
    let refused = daemon.held_split(&first, &first_token).unwrap_err();
    assert!(refused.contains("at most 36 at once"), "{refused}");
    // Other jobs fill the rest of the jobs' share, and no more.
    let refused = 'fill: loop {
        let (job, token) = daemon.job();
        for _ in 0..per_job {
            match daemon.held_split(&job, &token) {
                Ok(more) => holding.push(more),
                Err(message) => break 'fill message,
            }
        }
    };
    assert!(
        refused.contains("jobs hold 48 daemon connections"),
        "{refused}"
    );
    wait_until("the daemon holds every job connection", || {
        holding.iter().all(|(_, stream)| held(stream))
    });
    assert_eq!(holding.len(), share);
    // The host still gets through: a status and a jobs request.
    daemon.answers();
    let client = daemon.client();
    let jobs = returns("a host jobs request", move || {
        client.request(PublicRequest::Jobs).unwrap()
    });
    assert!(matches!(jobs, PublicReply::Jobs(_)), "{jobs:?}");
    // A connection that ends frees its job's slot.
    drop(holding.pop());
    let (job, token) = daemon.job();
    daemon.held_split(&job, &token).unwrap();
    drop(holding);
    wait_until("every slot is released", || {
        daemon.server.store.lock().connections.held_total() == 0
    });
}

/// A child the daemon runs for a job's `ProcessRun`, held until released:
/// what a job's bridge does for one `cap.sock` channel, then the request.
fn held_run(
    daemon: &Daemon,
    job: &str,
    token: &str,
) -> Result<(CapabilityConnection, UnixStream), String> {
    let slot = daemon.server.store.capability_connection(job)?;
    let mut stream = UnixStream::connect(&daemon.client().paths.socket).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let session = daemon.server.store.job(job).unwrap().session_id;
    let authority = authority(daemon.project.to_str().unwrap(), &daemon.home);
    let spec = ExecuteSpec {
        model: None,
        usage: None,
        label: None,
        kit: None,
        acp: false,
        working_directory: None,
        command: "fixture".into(),
        arguments: vec![b"hold".to_vec()],
        placement: Placement::Local,
        environment: BTreeMap::new(),
        session: split::session_spec(&session, &authority),
        process: None,
    };
    write_frame(
        &mut stream,
        &Envelope {
            protocol: PROTOCOL.into(),
            token: token.to_owned(),
            body: PublicRequest::ProcessRun(spec),
        },
    )
    .unwrap();
    match read_frame::<PublicReply>(&mut stream) {
        Ok(PublicReply::ExecutionAccepted) => Ok((slot, stream)),
        other => Err(format!("{other:?}")),
    }
}

/// A-03: the daemon's own connection that starts a job's `ProcessRun` child
/// takes one of the job's slots, so children within the jobs' share never
/// fill the host's quarter (each child costs two of the 48 slots here).
#[test]
fn process_runs_within_the_jobs_share_leave_the_host_its_quarter() {
    let release = Arc::new(AtomicBool::new(false));
    let backend = Arc::new(HoldOne(Arc::clone(&release)));
    let daemon = Daemon::start(|server| server.with_connection_cap(64).with_backend(backend));
    let mut holding = Vec::new();
    let refused = 'fill: loop {
        let (job, token) = daemon.job();
        for _ in 0..16 {
            match held_run(&daemon, &job, &token) {
                Ok(run) => holding.push(run),
                Err(message) => break 'fill message,
            }
        }
    };
    assert!(
        refused.contains("jobs hold 48 daemon connections"),
        "{refused}"
    );
    assert_eq!(holding.len(), 24);
    assert_eq!(daemon.server.store.lock().connections.held_total(), 48);
    daemon.answers();
    release.store(true, Ordering::Release);
    drop(holding);
    wait_until("every slot is released", || {
        daemon.server.store.lock().connections.held_total() == 0
    });
}

/// A-03: each argv branch of a job's split is a daemon connection on the
/// job's behalf and takes one of its slots; branches past the jobs' share
/// are refused, and the host is still served.
#[test]
fn a_jobs_split_branches_take_the_jobs_slots() {
    let release = Arc::new(AtomicBool::new(false));
    let backend = Arc::new(HoldOne(Arc::clone(&release)));
    let daemon = Daemon::start(|server| server.with_connection_cap(16).with_backend(backend));
    let (job, token) = daemon.job();
    let slot = daemon.server.store.capability_connection(&job).unwrap();
    let mut spec = daemon.spec(16, daemon.session());
    for branch in &mut spec.branches {
        branch.argv = Some(vec![b"fixture".to_vec(), b"hold".to_vec()]);
    }
    let mut stream = UnixStream::connect(&daemon.client().paths.socket).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    write_frame(
        &mut stream,
        &Envelope {
            protocol: PROTOCOL.into(),
            token,
            body: PublicRequest::SplitCreate(spec),
        },
    )
    .unwrap();
    write_frame(&mut stream, &AttachmentFrame::StdinEof).unwrap();
    let started = read_frame::<PublicReply>(&mut stream).unwrap();
    assert!(
        matches!(started, PublicReply::SplitStarted { .. }),
        "{started:?}"
    );
    wait_until("the jobs' share is held", || {
        daemon.server.store.lock().connections.held_total() == 12
    });
    daemon.answers();
    release.store(true, Ordering::Release);
    let finished = read_frame::<PublicReply>(&mut stream).unwrap();
    assert!(
        matches!(finished, PublicReply::SplitFinished { .. }),
        "{finished:?}"
    );
    drop((slot, stream));
    wait_until("every slot is released", || {
        daemon.server.store.lock().connections.held_total() == 0
    });
}

/// Take slots for distinct jobs (each up to its per-job limit) until the
/// jobs' share refuses; how many were taken.
fn fill_jobs_share(daemon: &Daemon) -> (Vec<CapabilityConnection>, String) {
    let mut slots = Vec::new();
    loop {
        let (job, _) = daemon.job();
        loop {
            match daemon.server.store.capability_connection(&job) {
                Ok(slot) => slots.push(slot),
                Err(message) if message.starts_with("marsh: jobs hold") => {
                    return (slots, message);
                }
                Err(_) => break,
            }
        }
    }
}

fn set_daemon_jobs(daemon: &Daemon, daemon_jobs: usize) {
    daemon.server.store.lock().job_defaults = Some(crate::JobDefaults {
        daemon_jobs,
        ..crate::JobDefaults::default()
    });
}

/// A-03: the connection cap grows with `daemon-jobs`, so a full pool of
/// nested children (two connections each) fits in the jobs' share, and the
/// host keeps its quarter: 1024 (share 768) at the default 64, 1371 (share
/// 1029, room for 510 children) at the most, 512.
#[test]
fn the_connection_cap_grows_with_the_job_pool() {
    assert_eq!(connection_budget::connection_cap_for(64), 1024);
    assert_eq!(connection_budget::connection_cap_for(512), 1371);
    let daemon = Daemon::start(|server| server);
    assert_eq!(daemon.server.store.connection_cap(), 1024);
    let (slots, refused) = fill_jobs_share(&daemon);
    assert_eq!(slots.len(), 768, "{refused}");
    drop(slots);
    set_daemon_jobs(&daemon, 512);
    assert_eq!(daemon.server.store.connection_cap(), 1371);
    let (slots, refused) = fill_jobs_share(&daemon);
    assert_eq!(slots.len(), 1029, "{refused}");
    assert!(slots.len() >= 2 * 510);
    drop(slots);
    daemon.answers();
}

/// A-03: the accept loop serves the grown cap, not 1024: at `daemon-jobs`
/// 512, with 1100 idle connections held, a host request (accepted after
/// them) is still served.
#[test]
fn the_accept_loop_serves_past_1024_connections_for_a_large_pool() {
    let daemon = Daemon::start(|server| server);
    set_daemon_jobs(&daemon, 512);
    let socket = daemon.client().paths.socket;
    let deadline = std::time::Instant::now() + PATIENCE;
    let idle = (0..1100)
        .map(|_| {
            loop {
                // Past the listen backlog a connect is refused until the accept
                // loop catches up.
                match UnixStream::connect(&socket) {
                    Ok(stream) => break stream,
                    Err(error)
                        if error.kind() == io::ErrorKind::ConnectionRefused
                            && std::time::Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        })
        .collect::<Vec<_>>();
    daemon.answers();
    // Each was still held (not timed out) while the host was served.
    for stream in &idle {
        stream.set_nonblocking(true).unwrap();
        let read = (&*stream).read(&mut [0; 1]);
        assert!(
            matches!(&read, Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "{read:?}"
        );
    }
}

/// A-03: at `daemon-jobs` 512, more `ProcessRun` children run at once than
/// a fixed 1024 cap allowed (384), and the host is still served.
#[test]
fn a_large_job_pool_runs_its_children_and_the_host_is_served() {
    let release = Arc::new(AtomicBool::new(false));
    let backend = Arc::new(HoldOne(Arc::clone(&release)));
    let daemon = Daemon::start(|server| server.with_backend(backend));
    set_daemon_jobs(&daemon, 512);
    let mut holding = Vec::new();
    while holding.len() < 400 {
        let (job, token) = daemon.job();
        for _ in 0..16 {
            holding.push(held_run(&daemon, &job, &token).unwrap());
        }
    }
    assert!(daemon.server.store.lock().connections.held_total() >= 800);
    daemon.answers();
    release.store(true, Ordering::Release);
    drop(holding);
    wait_until("every slot is released", || {
        daemon.server.store.lock().connections.held_total() == 0
    });
}
