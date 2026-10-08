//! Real Unixsocket requests and host CLI. Controlled stock executable and Kit
//! preparation only; no credentials, actual VMs, or claimed stock qualification.
use super::*;
use std::sync::atomic::AtomicUsize;

struct Kits {
    prepared: Arc<AtomicUsize>,
    root: PathBuf,
}
impl DaemonBackend for Kits {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(vec!["other-kit".into()])
    }
    fn registered_kits(&self) -> Result<BTreeMap<String, String>, DaemonError> {
        Ok(BTreeMap::from([("other-kit".into(), "fixture".into())]))
    }
    fn prepare(
        &self,
        _: &LoadSelection,
        _: &SessionSpec,
        progress: PreparationProgress,
        _: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        progress.cold_boot("other-kit")?;
        if self.root.join("long-prepare").exists() {
            thread::sleep(Duration::from_secs(305));
        }
        if self.root.join("hold-prepare").exists() {
            fs::write(self.root.join("prepare-entered"), b"").unwrap();
            let deadline = Instant::now() + Duration::from_secs(45);
            while !self.root.join("release-prepare").exists() {
                assert!(
                    Instant::now() < deadline,
                    "preparation fixture release missing"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        if self.root.join("fail-prepare").exists() {
            return Err(DaemonError::InvalidState(
                "fixture preparation failed".into(),
            ));
        }
        Ok(PreparationResult {
            cold_kits: vec![],
            sandboxes: BTreeMap::from([("other-kit".into(), "exact-ready-kit-vm".into())]),
        })
    }
    fn execute(
        &self,
        _: ExecuteSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }
    fn open_shell(
        &self,
        _: ShellSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }
}

struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn publication_transport_distinguishes_pre_dispatch_rejection_and_lost_reply() {
    // A real Unix-socket peer deliberately loses the terminal reply after
    // consuming the entire request. This is transport evidence, not an
    // authorization substitute for the full relay journey below.
    let root = tempfile::tempdir().unwrap();
    let server = Server::bind(root.path()).unwrap();
    server.lifecycle.listener.set_nonblocking(true).unwrap();
    let client = Client::connect(root.path()).unwrap();
    let session = SessionSpec {
        session_id: "transport-peer".into(),
        username: "fixture".into(),
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        launch_directory: root.path().into(),
        guest_home: root.path().into(),
        home_backing: root.path().into(),
        ephemeral_home: false,
        terminal: false,
        terminal_size: None,
    };
    thread::scope(|threads| {
        let peer = threads.spawn(|| {
            for wrong_protocol in [false, true] {
                // Generous: under full-workspace load the client thread can be
                // starved for seconds before it connects.
                let deadline = Instant::now() + Duration::from_mins(2);
                let mut stream = loop {
                    match server.lifecycle.listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                        other => panic!("publication peer did not receive request: {other:?}"),
                    }
                };
                // macOS accepted sockets inherit the listener's O_NONBLOCK; a
                // slow client would otherwise make read_frame fail WouldBlock.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_mins(2)))
                    .unwrap();
                let request: Envelope<PublicRequest> = read_frame(&mut stream).unwrap();
                assert!(matches!(request.body, PublicRequest::McpLoad { .. }));
                if wrong_protocol {
                    write_frame(
                        &mut stream,
                        &PublicReply::AcpPublication {
                            outcome: PublicationOutcome::Committed(PublicationCommit {
                                message: "wrong protocol".into(),
                            }),
                        },
                    )
                    .unwrap();
                }
            }
        });
        for _ in 0..2 {
            assert!(matches!(
                client.mcp_load(session.clone(), "tool".into(), None, Some("target".into())),
                Err(DaemonError::Publication(
                    PublicationOutcome::Uncertain { .. }
                ))
            ));
        }
        peer.join().unwrap();
    });
    drop(server);
    assert!(matches!(
        client.mcp_load(session, "tool".into(), None, Some("target".into())),
        Err(DaemonError::Publication(
            PublicationOutcome::RejectedBeforeEffect { .. }
        ))
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // Stateful real CLI/Unixsocket journey owns one isolated daemon.
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn load_via_relay_prepares_only_after_validation_and_preserves_publication() {
    use std::os::unix::fs::PermissionsExt as _;
    let cli = PathBuf::from(std::env::var_os("MARSH_LOAD_TEST_BIN").expect("MARSH_LOAD_TEST_BIN"));
    for name in ["marsh", "marsh-mcp", "marshd"] {
        let path = cli.with_file_name(name);
        let digest = format!(
            "{:x}",
            Sha256::digest(fs::read(&path).expect("all three host binaries required"))
        );
        eprintln!("MCP relay binary {} sha256={digest}", path.display());
        if let Some(evidence) = std::env::var_os("MARSH_MCP_TEST_EVIDENCE") {
            fs::create_dir_all(&evidence).unwrap();
            fs::write(
                PathBuf::from(evidence).join(format!("relay-{name}.sha256")),
                format!("{digest}  {}\n", path.display()),
            )
            .unwrap();
        }
    }
    let temp = tempfile::tempdir().unwrap();
    // marsh admits only symlink-free paths; macOS temp dirs live under /var -> /private/var.
    let base = temp.path().canonicalize().unwrap();
    let project = base.join("project");
    let home = base.join("selected");
    let host = base.join("host");
    for path in [&project, &home, &host, &home.join("home")] {
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let sbx = temp.path().join("sbx");
    fs::write(&sbx, r#"#!/usr/bin/python3
import json,os,pathlib,sys,time
root=pathlib.Path(__file__).parent
p=root/'registration.json'
a=sys.argv[1:]
if a[0]=='mcp' and (root/'stock-delay').exists(): time.sleep(float((root/'stock-delay').read_text()))
with (root/'effects').open('a') as f: f.write(' '.join(a)+'\n')
if a[:2]==['inspect','--json']:
 print(json.dumps({'name':a[2],'state':'running'}))
elif a[1]=='inspect':
 if p.exists() and json.loads(p.read_text())['name']==a[2]: print(p.read_text())
 else:
  sys.stderr.write(f'error: mcp server "{a[2]}" not found: mcp server not found\n  try: sbx mcp ls\n'); sys.exit(1)
elif a[1]=='add':
 c=a[a.index('--command')+1]; args=a[a.index('--args')+1].split(',')
 p.write_text(json.dumps({'name':a[2],'type':'local','resolved_command':c,'command':[c,*args]}))
elif a[1]=='rm': p.unlink()
elif a[1]=='load' and a[-1]=='rollback-slow':
 sys.stderr.write('forced load failure for reserved rollback proof\n'); sys.exit(7)
elif a[1]=='load' and a[-1]=='held-load':
 (root/'stock-load-entered').touch()
 for _ in range(1000):
  if (root/'release-stock-load').exists(): break
  time.sleep(.02)
 else: sys.exit(8)
 (root/'settled-stock-load').write_text('load finished before unpublish')
elif a[1]=='load' and a[-1]=='closed-output-child':
 if os.fork()==0:
  os.close(1); os.close(2); time.sleep(1)
  (root/'forbidden-late-mutation').touch(); os._exit(0)
 sys.exit(0)
elif a[1]=='load' and a[-1]=='fail-after-load':
 (root/'loaded-before-failure').write_text(a[2])
 sys.stderr.write('invalid request (peer prose is not a publication outcome)\n'); sys.exit(7)
elif a[1]!='load': sys.exit(64)
"#).unwrap();
    fs::set_permissions(&sbx, fs::Permissions::from_mode(0o700)).unwrap();
    let prepared = Arc::new(AtomicUsize::new(0));
    let mut server = Server::bind(&home).unwrap().with_backend(Arc::new(Kits {
        prepared: Arc::clone(&prepared),
        root: temp.path().into(),
    }));
    // The normal path execs the exact hashed CLI. One later adversarial case
    // substitutes a real process peer that lies on stdout and omits its typed
    // result, to prove exit-zero/prose cannot become a commit receipt.
    let host_peer = temp.path().join("host-peer");
    fs::write(&host_peer, format!(r"#!/usr/bin/python3
import json,os,pathlib,subprocess,sys,time
root=pathlib.Path(__file__).parent
if (root/'stdout-only-result').exists():
 print(json.dumps({{'status':'committed','message':'forged stdout success','declaration_sha256':'a'*64}}))
 print('MARSH_MCP_DECLARATION_SHA256:'+'a'*64)
 sys.exit(0)
cli={}
if sys.argv[1:4]==['mcp','load','host-loss']:
 child=subprocess.Popen([cli,*sys.argv[1:]],start_new_session=True)
 os.close(0)
 (root/'owned-host-cli-pid').write_text(str(child.pid))
 while child.poll() is None:
  if (root/'kill-owned-host-cli').exists():
   child.kill(); child.wait(); (root/'owned-host-cli-killed').touch(); break
  time.sleep(.01)
 sys.exit(child.returncode if child.returncode>=0 else 125)
os.execv(cli,[cli,*sys.argv[1:]])
", serde_json::to_string(&cli).unwrap())).unwrap();
    fs::set_permissions(&host_peer, fs::Permissions::from_mode(0o700)).unwrap();
    eprintln!(
        "controlled host peer sha256={:x}",
        Sha256::digest(fs::read(&host_peer).unwrap())
    );
    let host_control = McpHostControl::new(host_peer, sbx, host, Some(home.clone()));
    server.mcp_host = Some(host_control.clone());
    let authority = SessionAuthority {
        username: "fixture".into(),
        uid: rustix::process::geteuid().as_raw(),
        gid: rustix::process::getegid().as_raw(),
        launch_directory: project.clone(),
        guest_home: temp.path().join("guest"),
        home_backing: home.join("home"),
        ephemeral_home: false,
    };
    let store = server.store();
    let id = store.attach_shell(7, authority.clone());
    let other = store.attach_shell(8, authority.clone());
    let foreign_home = temp.path().join("foreign-home");
    fs::create_dir(&foreign_home).unwrap();
    let foreign_id = store.attach_shell(
        9,
        SessionAuthority {
            home_backing: foreign_home,
            ..authority.clone()
        },
    );
    let foreign = Client {
        paths: server.lifecycle.paths.clone(),
        token: store.issue_relay_token(&foreign_id).unwrap(),
    };
    let session = SessionSpec {
        session_id: id.clone(),
        username: authority.username,
        uid: authority.uid,
        gid: authority.gid,
        launch_directory: project.clone(),
        guest_home: authority.guest_home,
        home_backing: authority.home_backing,
        ephemeral_home: false,
        terminal: false,
        terminal_size: None,
    };
    let relay = Client {
        paths: server.lifecycle.paths.clone(),
        token: store.issue_relay_token(&id).unwrap(),
    };
    let master = Client::connect(&home).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_guard = Stop(Arc::clone(&stop));
    let thread = thread::spawn(move || server.serve_until(|| stop.load(Ordering::SeqCst)).unwrap());
    relay
        .mcp_publish(session.clone(), "stable".into(), None, None, "cat".into())
        .unwrap();
    let declaration_path = host_control
        .declaration_path(&session, PublicationKind::Mcp, "stable")
        .unwrap();
    let declaration = fs::read(&declaration_path).unwrap();
    let published = || fs::read(&declaration_path).ok().as_deref() == Some(declaration.as_slice());
    let registration = fs::read(temp.path().join("registration.json")).unwrap();
    // Raw authorized callers cannot erase an existing private publication by
    // supplying invalid options. All checks precede scope and Kit effects.
    let rejected_effects = fs::read(temp.path().join("effects")).unwrap();
    for (description, kit, sandbox, pipeline) in [
        (None, Some("other-kit"), Some("target"), "cat"),
        (Some("\n"), None, None, "cat"),
        (None, Some("bad.kit"), None, "cat"),
        (None, Some("missing-kit"), None, "cat"),
        (None, None, None, ""),
    ] {
        let reply = relay
            .request(PublicRequest::McpPublish {
                session: session.clone(),
                name: "stable".into(),
                description: description.map(str::to_owned),
                kit: kit.map(str::to_owned),
                sandbox: sandbox.map(str::to_owned),
                pipeline: pipeline.into(),
            })
            .unwrap();
        let reply = serde_json::to_value(reply).unwrap();
        eprintln!("invalid publication reply={reply}");
        assert_eq!(reply["outcome"]["status"], "rejected_before_effect");
        assert!(published(), "invalid publication changed the declaration");
        assert_eq!(prepared.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read(temp.path().join("registration.json")).unwrap(),
            registration
        );
        assert_eq!(
            fs::read(temp.path().join("effects")).unwrap(),
            rejected_effects
        );
    }
    // Run the actual guest Brush command with its existing session relay token.
    fs::create_dir(&session.guest_home).unwrap();
    let guest_token = temp.path().join("guest-token");
    // A different attached shell loads the publication into another Kit.
    fs::write(&guest_token, store.issue_relay_token(&other).unwrap()).unwrap();
    fs::set_permissions(&guest_token, fs::Permissions::from_mode(0o600)).unwrap();
    let guest = |command: &str| {
        std::process::Command::new(&cli)
            .args([
                "--marsh-guest",
                "--marsh-session",
                &other,
                "--noprofile",
                "--norc",
                "-c",
                command,
            ])
            .current_dir(&project)
            .env_clear()
            .env("HOME", &session.guest_home)
            .env("MARSH_HOME", &home)
            .env("USER", &session.username)
            .env("PATH", "/usr/bin:/bin")
            .env("MARSH_DAEMON_SOCKET", &relay.paths.socket)
            .env("MARSH_DAEMON_TOKEN", &guest_token)
            .output()
            .unwrap()
    };
    // Optional slow real-caller evidence uses the production five-minute host
    // budget unchanged, not a test-only timeout or helper substitute.
    let long_prepare = std::env::var_os("MARSH_MCP_LONG_PREPARE").is_some();
    if long_prepare {
        fs::write(temp.path().join("long-prepare"), b"").unwrap();
    }
    let load_started = Instant::now();
    let loaded = guest("mcp load stable --kit other-kit");
    if long_prepare {
        assert!(load_started.elapsed() > Duration::from_mins(5));
        fs::remove_file(temp.path().join("long-prepare")).unwrap();
        eprintln!(
            "MCP cold preparation completed across unchanged five-minute host budget in {:?}",
            load_started.elapsed()
        );
    }
    assert!(
        loaded.status.success(),
        "{}",
        String::from_utf8_lossy(&loaded.stderr)
    );
    assert!(
        String::from_utf8_lossy(&loaded.stderr).contains("[starting other-kit worker VM…]"),
        "real cold-boot progress lost"
    );
    let message = String::from_utf8(loaded.stdout).unwrap();
    assert!(message.contains("exact-ready-kit-vm"));
    assert_eq!(prepared.load(Ordering::SeqCst), 1);
    assert!(published());
    assert_eq!(
        fs::read(temp.path().join("registration.json")).unwrap(),
        registration
    );
    let request = |session| PublicRequest::McpLoad {
        session,
        name: "stable".into(),
        kit: None,
        sandbox: Some("target".into()),
    };
    let (job, _) = store
        .begin_job(NewJob {
            session_id: id.clone(),
            command: "other-kit".into(),
            kit_ref: "fixture".into(),
            workload_image: "fixture-image".into(),
            mounts: vec![],
        })
        .unwrap();
    for command in ["ps --marsh", "top --marsh --once"] {
        let view = guest(command);
        assert!(view.status.success());
        let table = String::from_utf8(view.stdout).unwrap();
        assert!(table.contains(&id), "shell ID is not copyable: {table}");
        assert!(table.contains(&job), "job ID is not copyable: {table}");
    }
    assert_eq!(relay.job(job.clone()).unwrap().job_id, job);
    let denied_effects = fs::read(temp.path().join("effects")).unwrap();
    assert!(matches!(
        master.request(request(session.clone())).unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    let mut foreign_session = session.clone();
    foreign_session.session_id = foreign_id;
    assert!(
        foreign
            .mcp_load(
                foreign_session,
                "stable".into(),
                None,
                Some("target".into())
            )
            .is_err()
    );
    let mut wrong = session.clone();
    wrong.session_id = other.clone();
    assert!(matches!(
        relay.request(request(wrong)),
        Err(_) | Ok(PublicReply::Error { .. })
    ));
    assert_eq!(
        fs::read(temp.path().join("effects")).unwrap(),
        denied_effects
    );
    // Leave exactly one public daemon connection slot for the authenticated
    // load request. A nested master reconnect would need a forbidden 33rd slot.
    let occupied = (0..MAX_DAEMON_CONNECTIONS - 1)
        .map(|_| UnixStream::connect(&relay.paths.socket).unwrap())
        .collect::<Vec<_>>();
    thread::sleep(Duration::from_millis(200));
    relay
        .mcp_load(
            session.clone(),
            "stable".into(),
            Some("other-kit".into()),
            None,
        )
        .unwrap();
    drop(occupied);
    assert_eq!(prepared.load(Ordering::SeqCst), 2);
    // A different publication name still contends for the same physical
    // project/home scope. Admission must fail before any host or privacy effect,
    // not after another shell has spent 30 seconds waiting behind cold prep.
    fs::write(temp.path().join("hold-prepare"), b"").unwrap();
    thread::scope(|threads| {
        let load = threads.spawn(|| guest("mcp load stable --kit other-kit"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !temp.path().join("prepare-entered").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        let effects = fs::read(temp.path().join("effects")).unwrap();
        let started = Instant::now();
        let rejected = guest("mcp publish another-name -- cat");
        assert!(!rejected.status.success());
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("rejected before effect"));
        assert!(String::from_utf8_lossy(&rejected.stderr).contains("scope is busy"));
        assert!(started.elapsed() >= Duration::from_secs(30));
        assert_eq!(fs::read(temp.path().join("effects")).unwrap(), effects);
        assert!(published());
        fs::write(temp.path().join("release-prepare"), b"").unwrap();
        assert!(load.join().unwrap().status.success());
    });
    for name in ["hold-prepare", "prepare-entered", "release-prepare"] {
        fs::remove_file(temp.path().join(name)).unwrap();
    }
    assert_eq!(prepared.load(Ordering::SeqCst), 3);
    // A stock peer changes its load state and then returns misleading error
    // prose. Public classification follows the admitted effect, not stderr.
    match relay
        .mcp_load(
            session.clone(),
            "stable".into(),
            None,
            Some("fail-after-load".into()),
        )
        .unwrap_err()
    {
        DaemonError::Publication(PublicationOutcome::Uncertain { message }) => {
            assert!(message.contains("invalid request"));
        }
        other => panic!("stock effect failure collapsed into an argument error: {other:?}"),
    }
    assert!(temp.path().join("loaded-before-failure").exists());
    assert!(
        published(),
        "load must not change the publication even on uncertain reply"
    );
    let before_bogus = fs::read(temp.path().join("effects")).unwrap();
    fs::write(temp.path().join("stdout-only-result"), b"").unwrap();
    match relay
        .mcp_load(
            session.clone(),
            "stable".into(),
            None,
            Some("target".into()),
        )
        .unwrap_err()
    {
        DaemonError::Publication(PublicationOutcome::Uncertain { .. }) => {}
        other => panic!("stdout/exit-zero was mistaken for a typed commit: {other:?}"),
    }
    fs::remove_file(temp.path().join("stdout-only-result")).unwrap();
    assert_eq!(fs::read(temp.path().join("effects")).unwrap(), before_bogus);
    assert!(published());
    // Invalid publish --kit must not boot first. These use the real guest CLI,
    // daemon Unix socket, host CLI and controlled stock process, not a helper mirror.
    assert!(
        !guest("mcp publish bad/name --kit other-kit -- cat")
            .status
            .success()
    );
    assert!(
        !guest("mcp publish invalid --description '' --kit other-kit -- cat")
            .status
            .success()
    );
    assert_eq!(prepared.load(Ordering::SeqCst), 3);
    // A registered generation mismatch also fails before prepare.
    let mut stale: serde_json::Value = serde_json::from_slice(&registration).unwrap();
    *stale["command"].as_array_mut().unwrap().last_mut().unwrap() =
        serde_json::json!(uuid::Uuid::new_v4().to_string());
    fs::write(
        temp.path().join("registration.json"),
        serde_json::to_vec(&stale).unwrap(),
    )
    .unwrap();
    let stale_load = guest("mcp load stable --kit other-kit");
    assert!(!stale_load.status.success());
    assert!(String::from_utf8_lossy(&stale_load.stderr).contains("Republish"));
    assert!(!String::from_utf8_lossy(&stale_load.stderr).contains("host MCP registration exited"));
    assert_eq!(prepared.load(Ordering::SeqCst), 3);
    fs::write(temp.path().join("registration.json"), &registration).unwrap();
    // Failed backend preparation must leave the attached shell usable.
    fs::write(temp.path().join("fail-prepare"), b"").unwrap();
    let failed_prepare = guest("mcp load stable --kit other-kit");
    assert!(!failed_prepare.status.success());
    assert!(
        String::from_utf8_lossy(&failed_prepare.stderr).contains("publication outcome uncertain")
    );
    match relay
        .mcp_load(
            session.clone(),
            "stable".into(),
            Some("other-kit".into()),
            None,
        )
        .unwrap_err()
    {
        DaemonError::Publication(PublicationOutcome::Uncertain { message }) => {
            assert!(message.contains("fixture preparation failed"));
        }
        other => panic!("post-prepare failure lost its typed uncertainty: {other:?}"),
    }
    fs::remove_file(temp.path().join("fail-prepare")).unwrap();
    assert!(guest("ps --marsh").status.success());
    assert!(
        guest("mcp load stable --sandbox still-attached")
            .status
            .success()
    );
    // Stock peers outside marsh's lock cannot change the generation during
    // preparation unnoticed. Drive a real load, alter only the controlled SBX
    // peer's registration, then release its backend preparation.
    fs::write(temp.path().join("hold-prepare"), b"").unwrap();
    thread::scope(|threads| {
        let load = threads.spawn(|| guest("mcp load stable --kit other-kit"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !temp.path().join("prepare-entered").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        fs::write(
            temp.path().join("registration.json"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        let before = fs::read(temp.path().join("effects")).unwrap();
        fs::write(temp.path().join("release-prepare"), b"").unwrap();
        let denied = load.join().unwrap();
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("Republish"));
        let after = fs::read(temp.path().join("effects")).unwrap();
        assert!(
            !String::from_utf8_lossy(&after[before.len()..])
                .lines()
                .any(|line| line.starts_with("mcp load"))
        );
    });
    fs::write(temp.path().join("registration.json"), &registration).unwrap();
    fs::remove_file(temp.path().join("prepare-entered")).unwrap();
    fs::remove_file(temp.path().join("release-prepare")).unwrap();
    // Unpublish fences a slow prepare while its host transaction lock is held.
    let declaration = host_control
        .declaration_path(&session, PublicationKind::Mcp, "stable")
        .unwrap();
    thread::scope(|threads| {
        let load = threads.spawn(|| guest("mcp load stable --kit other-kit"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !temp.path().join("prepare-entered").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        let revoke = threads.spawn(|| guest("mcp unpublish stable"));
        while !declaration.with_extension("revoke").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        let before = fs::read_to_string(temp.path().join("effects")).unwrap();
        let loads = before
            .lines()
            .filter(|line| line.starts_with("mcp load"))
            .count();
        fs::write(temp.path().join("release-prepare"), b"").unwrap();
        let denied = load.join().unwrap();
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("revocation is pending"));
        let revoked = revoke.join().unwrap();
        assert!(
            revoked.status.success(),
            "{}",
            String::from_utf8_lossy(&revoked.stderr)
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("effects"))
                .unwrap()
                .lines()
                .filter(|line| line.starts_with("mcp load"))
                .count(),
            loads
        );
    });
    let effects = fs::read(temp.path().join("effects")).unwrap();
    assert!(!guest("mcp load stable --kit other-kit").status.success());
    assert_eq!(prepared.load(Ordering::SeqCst), 7);
    assert_eq!(fs::read(temp.path().join("effects")).unwrap(), effects);
    // The same shared helper handles an admitted publish target; lineage is a
    // normal publish transition here, not silently reminted by load.
    fs::remove_file(temp.path().join("hold-prepare")).unwrap();
    let published = guest("mcp publish new-tool --kit other-kit -- cat");
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    assert!(String::from_utf8_lossy(&published.stdout).contains("exact-ready-kit-vm"));
    assert!(String::from_utf8_lossy(&published.stderr).contains("[starting other-kit worker VM…]"));
    assert_eq!(prepared.load(Ordering::SeqCst), 8);
    assert!(guest("mcp unpublish new-tool").status.success());
    assert!(guest("mcp publish host-loss -- cat").status.success());
    thread::scope(|threads| {
        let load = threads.spawn(|| guest("mcp load host-loss --sandbox held-load"));
        let deadline = Instant::now() + Duration::from_secs(15);
        while !temp.path().join("stock-load-entered").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        // The Python creator signals only its tracked new-session CLI. The
        // stock process belongs to the daemon, not this killable host.
        fs::write(temp.path().join("kill-owned-host-cli"), b"").unwrap();
        while !temp.path().join("owned-host-cli-killed").exists() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(20));
        }
        let unpublish = threads.spawn(|| guest("mcp unpublish host-loss"));
        thread::sleep(Duration::from_millis(250));
        assert!(
            !unpublish.is_finished(),
            "unpublish raced an unsettled stock carrier after host death"
        );
        assert!(temp.path().join("registration.json").exists());
        fs::write(temp.path().join("release-stock-load"), b"").unwrap();
        let result = load.join().unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("uncertain"));
        assert!(unpublish.join().unwrap().status.success());
        assert!(temp.path().join("settled-stock-load").exists());
        assert!(!temp.path().join("registration.json").exists());
    });
    assert!(guest("mcp publish no-orphan -- cat").status.success());
    assert!(
        guest("mcp load no-orphan --sandbox closed-output-child")
            .status
            .success()
    );
    assert!(guest("mcp unpublish no-orphan").status.success());
    thread::sleep(Duration::from_millis(1200));
    assert!(
        !temp.path().join("forbidden-late-mutation").exists(),
        "stock descendant mutated after its carrier/unpublish completed"
    );
    if std::env::var_os("MARSH_MCP_LONG_ROLLBACK").is_some() {
        assert!(guest("mcp publish long-rollback -- cat").status.success());
        let declaration = host_control
            .declaration_path(&session, PublicationKind::Mcp, "long-rollback")
            .unwrap();
        let prior = fs::read(&declaration).unwrap();
        let prior_registration = fs::read(temp.path().join("registration.json")).unwrap();
        fs::write(temp.path().join("stock-delay"), b"35").unwrap();
        let started = Instant::now();
        let failed = guest("mcp publish long-rollback --sandbox rollback-slow -- 'printf changed'");
        fs::remove_file(temp.path().join("stock-delay")).unwrap();
        assert!(!failed.status.success());
        assert!(String::from_utf8_lossy(&failed.stderr).contains("uncertain"));
        assert!(String::from_utf8_lossy(&failed.stderr).contains("forced load failure"));
        assert!(started.elapsed() > Duration::from_mins(5));
        assert_eq!(fs::read(&declaration).unwrap(), prior);
        assert_eq!(
            fs::read(temp.path().join("registration.json")).unwrap(),
            prior_registration
        );
        eprintln!(
            "production stock/rollback allowance completed after {:?}",
            started.elapsed()
        );
        assert!(guest("mcp unpublish long-rollback").status.success());
        thread::sleep(Duration::from_millis(250));
        assert!(!temp.path().join("registration.json").exists());
    }
    stop_guard.0.store(true, Ordering::SeqCst);
    thread.join().unwrap();
}
