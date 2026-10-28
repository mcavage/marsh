//! A host caller killed before it detaches (`client_liveness.rs`), through the
//! authenticated socket with real child processes as the callers.
use super::*;

struct Caller(std::process::Child);

impl Caller {
    fn spawn() -> Self {
        Self(Command::new("/bin/sleep").arg("600").spawn().unwrap())
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// SIGKILL, then reap: its pid no longer exists, as after a caller's
    /// parent collects it.
    fn kill(&mut self) {
        self.0.kill().unwrap();
        self.0.wait().unwrap();
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn project(home: &Path) -> SessionAuthority {
    let project = home.join("project");
    fs::create_dir_all(&project).unwrap();
    authority(project.to_str().unwrap(), home)
}

fn attach(client: &Client, caller: &Caller, home: &Path) -> String {
    let session = project(home);
    match client
        .request(PublicRequest::AttachShell {
            pid: caller.pid(),
            session,
        })
        .unwrap()
    {
        PublicReply::ShellAttached { session_id } => session_id,
        reply => panic!("unexpected attach reply: {reply:?}"),
    }
}

fn shells(client: &Client) -> BTreeMap<String, ShellState> {
    let PublicReply::Status(status) = client
        .request(PublicRequest::Status { session_id: None })
        .unwrap()
    else {
        panic!("expected status")
    };
    status
        .shells
        .into_iter()
        .map(|shell| (shell.session_id, shell.state))
        .collect()
}

fn shutdown(client: &Client) -> PublicReply {
    client.request(PublicRequest::Shutdown).unwrap()
}

#[test]
fn a_killed_host_caller_no_longer_holds_its_session_attached() {
    let home = home();
    let server = Arc::new(Server::bind(home.path()).unwrap());
    let (_stop, task) = start_server(Arc::clone(&server));
    let client = Client::connect(home.path()).unwrap();

    let mut live = Caller::spawn();
    let mut killed = Caller::spawn();
    let live_session = attach(&client, &live, home.path());
    let killed_session = attach(&client, &killed, home.path());
    assert_eq!(shells(&client).len(), 2);

    killed.kill();
    assert_eq!(
        shells(&client),
        BTreeMap::from([(live_session.clone(), ShellState::Attached)]),
        "the killed caller's session is released; the live one is kept"
    );
    assert!(
        !server
            .store
            .lock()
            .session_authorities
            .contains_key(&killed_session),
        "released like its own DetachShell"
    );
    assert!(
        matches!(
            shutdown(&client),
            PublicReply::Error {
                code: ErrorCode::InvalidRequest,
                ..
            }
        ),
        "a live caller still holds the scope"
    );

    live.kill();
    assert_eq!(shutdown(&client), PublicReply::ShuttingDown);
    task.join().unwrap();
}

#[test]
fn a_killed_caller_with_work_in_flight_or_a_bound_shell_vm_is_not_simply_detached() {
    let home = home();
    let server = Arc::new(Server::bind(home.path()).unwrap());
    let (stop, task) = start_server(Arc::clone(&server));
    let client = Client::connect(home.path()).unwrap();

    // A queued job: released only once the job has ended.
    let mut running = Caller::spawn();
    let running_session = attach(&client, &running, home.path());
    let (job, _) = server
        .store
        .begin_job(NewJob {
            session_id: running_session.clone(),
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("example/fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        })
        .unwrap();
    // A shell VM still bound: its cleanup is unproved, so it becomes
    // cleanup-uncertain for `marsh reset`/`marsh stop` to recover.
    let mut bound = Caller::spawn();
    let bound_session = attach(&client, &bound, home.path());
    server
        .store
        .bind_shell_cleanup_vm(&bound_session, "marsh-s-fixture0")
        .unwrap();

    running.kill();
    bound.kill();
    assert_eq!(
        shells(&client),
        BTreeMap::from([
            (running_session.clone(), ShellState::Attached),
            (bound_session.clone(), ShellState::CleanupUncertain),
        ])
    );

    server.store.abort_job(&job, "test").unwrap();
    assert_eq!(
        shells(&client),
        BTreeMap::from([(bound_session, ShellState::CleanupUncertain)])
    );
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}

#[test]
fn a_session_whose_pid_this_daemon_cannot_signal_is_left_to_its_caller() {
    // pid 1 is another user's process: the daemon cannot prove a caller's
    // exit from it, so it never releases the session itself.
    let home = home();
    let server = Arc::new(Server::bind(home.path()).unwrap());
    let (stop, task) = start_server(Arc::clone(&server));
    let client = Client::connect(home.path()).unwrap();
    let session = match client
        .request(PublicRequest::AttachShell {
            pid: 1,
            session: project(home.path()),
        })
        .unwrap()
    {
        PublicReply::ShellAttached { session_id } => session_id,
        reply => panic!("unexpected attach reply: {reply:?}"),
    };
    assert_eq!(
        shells(&client),
        BTreeMap::from([(session.clone(), ShellState::Attached)])
    );
    server.store.detach_shell(&session).unwrap();
    stop.store(true, Ordering::Relaxed);
    task.join().unwrap();
}
