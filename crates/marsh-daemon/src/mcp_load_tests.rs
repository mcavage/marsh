//! Real Unix-socket requests and host CLI. Controlled stock executable and Kit
//! preparation only; no credentials, actual VMs, or claimed stock qualification.
//!
//! The relay journeys need prebuilt host binaries (`MARSH_LOAD_TEST_BIN`
//! names `marsh`; `marsh-mcp` and `marshd` sit beside it), so they are
//! ignored by default and run by `make test-rust` (`MCP_LOAD_CALLERS`).
use super::*;
use crate::test_support::{PATIENCE, fenced, wait_for, wait_until};
use std::sync::atomic::AtomicUsize;

pub(crate) struct Kits {
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
        progress.cold_boot("other-kit", false)?;
        if let Ok(seconds) = fs::read_to_string(self.root.join("long-prepare")) {
            thread::sleep(Duration::from_secs(seconds.trim().parse().unwrap()));
        }
        if self.root.join("hold-prepare").exists() {
            fs::write(self.root.join("prepare-entered"), b"").unwrap();
            wait_until("the test releases preparation", || {
                self.root.join("release-prepare").exists()
            });
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

#[test]
fn publication_transport_distinguishes_pre_dispatch_rejection_and_lost_reply() {
    // A real Unix-socket peer deliberately loses the terminal reply after
    // consuming the entire request. This is transport evidence, not an
    // authorization substitute for the relay journeys below.
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
                let mut stream = wait_for("the publication request", || {
                    match server.lifecycle.listener.accept() {
                        Ok((stream, _)) => Some(stream),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
                        Err(error) => panic!("{error}"),
                    }
                });
                // macOS accepted sockets inherit the listener's O_NONBLOCK; a
                // slow client would otherwise make read_frame fail WouldBlock.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(PATIENCE)).unwrap();
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

/// A controlled stock `sbx`: records every call in `effects` and keeps one
/// MCP registration in `registration.json`. A load's sandbox name selects a
/// scripted behaviour.
const STOCK_SBX: &str = r#"#!/usr/bin/python3
import json,os,pathlib,sys,time
root=pathlib.Path(__file__).parent
p=root/'registration.json'
a=sys.argv[1:]
if a[0]=='mcp' and (root/'stock-delay').exists(): time.sleep(float((root/'stock-delay').read_text()))
def effect(line):
 with (root/'effects').open('a') as f: f.write(line+'\n')
effect(' '.join(a))
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
 while not (root/'release-stock-load').exists(): time.sleep(.01)
 effect('settled held-load')
elif a[1]=='load' and a[-1]=='closed-output-child':
 pid=os.fork()
 if pid==0:
  os.close(1); os.close(2); time.sleep(600)
  (root/'forbidden-late-mutation').touch(); os._exit(0)
 (root/'descendant-pid').write_text(str(pid))
 sys.exit(0)
elif a[1]=='load' and a[-1]=='fail-after-load':
 (root/'loaded-before-failure').write_text(a[2])
 sys.stderr.write('invalid request (peer prose is not a publication outcome)\n'); sys.exit(7)
elif a[1]!='load': sys.exit(64)
"#;

/// The host CLI the daemon executes: normally the exact `marsh` under test.
/// `stdout-only-result` turns it into a peer that lies on stdout and omits
/// its typed result; `mcp load host-loss` runs the real CLI in its own session
/// and kills it on request, as a host whose CLI dies mid-load.
fn host_peer(cli: &Path) -> String {
    format!(
        r"#!/usr/bin/python3
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
 while child.poll() is None:
  if (root/'kill-owned-host-cli').exists():
   child.kill(); child.wait(); (root/'owned-host-cli-killed').touch(); break
  time.sleep(.01)
 sys.exit(child.returncode if child.returncode>=0 else 125)
os.execv(cli,[cli,*sys.argv[1:]])
",
        serde_json::to_string(cli).unwrap()
    )
}

/// One isolated daemon with a published `stable` tool, the controlled stock
/// `sbx`, and three attached shells: the publisher, `other` (whose relay token
/// the guest CLI uses), and `foreign` (another home).
pub(crate) struct LoadFixture {
    temp: tempfile::TempDir,
    cli: PathBuf,
    project: PathBuf,
    home: PathBuf,
    pub(crate) prepared: Arc<AtomicUsize>,
    pub(crate) host: McpHostControl,
    store: DaemonStore,
    id: String,
    other: String,
    foreign: Client,
    foreign_id: String,
    pub(crate) session: SessionSpec,
    relay: Client,
    master: Client,
    guest_token: PathBuf,
    declaration: Vec<u8>,
    declaration_path: PathBuf,
    registration: Vec<u8>,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
}

impl LoadFixture {
    pub(crate) fn new() -> Self {
        Self::with_host(|host| host)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one daemon, host peer, and three shells"
    )]
    fn with_host(configure: impl FnOnce(McpHostControl) -> McpHostControl) -> Self {
        let cli =
            PathBuf::from(std::env::var_os("MARSH_LOAD_TEST_BIN").expect("MARSH_LOAD_TEST_BIN"));
        for name in ["marsh", "marsh-mcp", "marshd"] {
            assert!(
                cli.with_file_name(name).is_file(),
                "{name} must sit beside MARSH_LOAD_TEST_BIN"
            );
        }
        let temp = tempfile::tempdir().unwrap();
        // marsh admits only symlink-free paths; macOS temp dirs live under /var -> /private/var.
        let base = temp.path().canonicalize().unwrap();
        let project = base.join("project");
        let home = base.join("selected");
        let host_home = base.join("host");
        for path in [&project, &home, &host_home, &home.join("home")] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let executable = |name: &str, contents: &str| {
            let path = temp.path().join(name);
            fs::write(&path, contents).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        };
        let sbx = executable("sbx", STOCK_SBX);
        let peer = executable("host-peer", &host_peer(&cli));
        let prepared = Arc::new(AtomicUsize::new(0));
        let mut server = Server::bind(&home).unwrap().with_backend(Arc::new(Kits {
            prepared: Arc::clone(&prepared),
            root: temp.path().into(),
        }));
        let host = configure(McpHostControl::new(
            peer,
            sbx,
            host_home,
            Some(home.clone()),
        ));
        server.mcp_host = Some(host.clone());
        let authority = SessionAuthority {
            username: "fixture".into(),
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            launch_directory: project.clone(),
            guest_home: temp.path().join("guest"),
            home_backing: home.join("home"),
            ephemeral_home: false,
        };
        fs::create_dir(&authority.guest_home).unwrap();
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
        let client = |token: String| Client {
            paths: server.lifecycle.paths.clone(),
            token,
        };
        let foreign = client(store.issue_relay_token(&foreign_id).unwrap());
        let relay = client(store.issue_relay_token(&id).unwrap());
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
        let guest_token = temp.path().join("guest-token");
        fs::write(&guest_token, store.issue_relay_token(&other).unwrap()).unwrap();
        fs::set_permissions(&guest_token, fs::Permissions::from_mode(0o600)).unwrap();
        let master = Client::connect(&home).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let task = thread::spawn(move || {
            server
                .serve_until(|| stopping.load(Ordering::SeqCst))
                .unwrap();
        });
        relay
            .mcp_publish(session.clone(), "stable".into(), None, None, "cat".into())
            .unwrap();
        let declaration_path = host
            .declaration_path(&session, PublicationKind::Mcp, "stable")
            .unwrap();
        Self {
            declaration: fs::read(&declaration_path).unwrap(),
            declaration_path,
            registration: fs::read(temp.path().join("registration.json")).unwrap(),
            temp,
            cli,
            project,
            home,
            prepared,
            host,
            store,
            id,
            other,
            foreign,
            foreign_id,
            session,
            relay,
            master,
            guest_token,
            stop,
            task: Some(task),
        }
    }

    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.temp.path().join(name)
    }

    /// Run the actual guest Brush command as the `other` shell.
    pub(crate) fn guest(&self, command: &str) -> std::process::Output {
        let mut guest = std::process::Command::new(&self.cli);
        guest
            .args([
                "--marsh-guest",
                "--marsh-session",
                &self.other,
                "--noprofile",
                "--norc",
                "-c",
                command,
            ])
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", &self.session.guest_home)
            .env("MARSH_HOME", &self.home)
            .env("USER", &self.session.username)
            .env("PATH", "/usr/bin:/bin")
            .env("MARSH_DAEMON_SOCKET", &self.relay.paths.socket)
            .env("MARSH_DAEMON_TOKEN", &self.guest_token)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        fenced(|| guest.spawn())
            .unwrap()
            .wait_with_output()
            .unwrap()
    }

    pub(crate) fn effects(&self) -> String {
        fs::read_to_string(self.path("effects")).unwrap()
    }

    pub(crate) fn registration(&self) -> Vec<u8> {
        fs::read(self.path("registration.json")).unwrap()
    }

    /// `stable` still carries exactly its original declaration.
    pub(crate) fn published(&self) -> bool {
        fs::read(&self.declaration_path).ok().as_deref() == Some(self.declaration.as_slice())
    }

    pub(crate) fn prepared(&self) -> usize {
        self.prepared.load(Ordering::SeqCst)
    }

    /// The registration with a different generation, as another stock peer
    /// outside marsh's lock would leave it.
    fn stale_registration(&self) -> Vec<u8> {
        let mut stale: serde_json::Value = serde_json::from_slice(&self.registration).unwrap();
        *stale["command"].as_array_mut().unwrap().last_mut().unwrap() =
            serde_json::json!(uuid::Uuid::new_v4().to_string());
        serde_json::to_vec(&stale).unwrap()
    }

    fn hold_preparation(&self) {
        fs::write(self.path("hold-prepare"), b"").unwrap();
    }

    fn wait_for_preparation(&self) {
        wait_until("preparation begins", || {
            self.path("prepare-entered").exists()
        });
    }

    fn release_preparation(&self) {
        fs::write(self.path("release-prepare"), b"").unwrap();
    }
}

fn load_request(session: SessionSpec) -> PublicRequest {
    PublicRequest::McpLoad {
        session,
        name: "stable".into(),
        kit: None,
        sandbox: Some("target".into()),
    }
}

impl Drop for LoadFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Raw authorized callers cannot erase an existing private publication by
/// supplying invalid options, and the guest CLI's invalid `--kit` publishes
/// never boot: every check precedes scope, stock, and Kit effects.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn invalid_publications_are_rejected_before_any_effect() {
    let f = LoadFixture::new();
    let (registration, effects) = (f.registration(), f.effects());
    for (description, kit, sandbox, pipeline) in [
        (None, Some("other-kit"), Some("target"), "cat"),
        (Some("\n"), None, None, "cat"),
        (None, Some("bad.kit"), None, "cat"),
        (None, Some("missing-kit"), None, "cat"),
        (None, None, None, ""),
    ] {
        let reply = f
            .relay
            .request(PublicRequest::McpPublish {
                session: f.session.clone(),
                name: "stable".into(),
                description: description.map(str::to_owned),
                kit: kit.map(str::to_owned),
                sandbox: sandbox.map(str::to_owned),
                pipeline: pipeline.into(),
            })
            .unwrap();
        let reply = serde_json::to_value(reply).unwrap();
        assert_eq!(
            reply["outcome"]["status"], "rejected_before_effect",
            "{reply}"
        );
    }
    for command in [
        "mcp publish bad/name --kit other-kit -- cat",
        "mcp publish invalid --description '' --kit other-kit -- cat",
    ] {
        assert!(!f.guest(command).status.success(), "{command}");
    }
    assert!(f.published(), "invalid publication changed the declaration");
    assert_eq!(f.prepared(), 0);
    assert_eq!(f.registration(), registration);
    assert_eq!(f.effects(), effects);
}

/// Another shell loads the publication into a Kit: one cold boot with its
/// progress shown, the publication and registration unchanged, and the
/// shell and job IDs copyable from `ps`/`top`.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn guest_load_boots_the_kit_once_and_keeps_the_publication() {
    let f = LoadFixture::new();
    let loaded = f.guest("mcp load stable --kit other-kit");
    assert!(loaded.status.success(), "{}", stderr(&loaded));
    assert!(
        stderr(&loaded).contains("[starting other-kit worker VM…]"),
        "real cold-boot progress lost"
    );
    assert!(String::from_utf8_lossy(&loaded.stdout).contains("exact-ready-kit-vm"));
    assert_eq!(f.prepared(), 1);
    assert!(f.published());
    assert_eq!(f.registration(), f.registration);
    let (job, _) = f
        .store
        .begin_job(NewJob {
            label: None,
            session_id: f.id.clone(),
            command: "other-kit".into(),
            kit_ref: "fixture".into(),
            workload_image: "fixture-image".into(),
            mounts: vec![],
        })
        .unwrap();
    for command in ["ps --marsh", "top --marsh --once"] {
        let view = f.guest(command);
        assert!(view.status.success(), "{}", stderr(&view));
        let table = String::from_utf8(view.stdout).unwrap();
        assert!(table.contains(&f.id), "shell ID is not copyable: {table}");
        assert!(table.contains(&job), "job ID is not copyable: {table}");
    }
    assert_eq!(f.relay.job(job.clone()).unwrap().job_id, job);
}

/// Only the publishing session's relay can load: the master token, another
/// home's relay, and a relay naming another session are refused before any
/// stock effect.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn load_by_other_callers_is_refused_before_any_effect() {
    let f = LoadFixture::new();
    let effects = f.effects();
    assert!(matches!(
        f.master.request(load_request(f.session.clone())).unwrap(),
        PublicReply::Error {
            code: ErrorCode::Unauthorized,
            ..
        }
    ));
    let mut foreign = f.session.clone();
    foreign.session_id = f.foreign_id.clone();
    assert!(
        f.foreign
            .mcp_load(foreign, "stable".into(), None, Some("target".into()))
            .is_err()
    );
    let mut wrong = f.session.clone();
    wrong.session_id = f.other.clone();
    assert!(matches!(
        f.relay.request(load_request(wrong)),
        Err(_) | Ok(PublicReply::Error { .. })
    ));
    assert_eq!(f.effects(), effects);
    assert_eq!(f.prepared(), 0);
}

/// A load completes on its own connection: with every other public daemon
/// slot occupied, a nested reconnect would need a slot that does not exist.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn load_needs_no_second_daemon_connection() {
    let f = LoadFixture::new();
    // Accepted in order, so these hold their slots before the load arrives.
    let occupied = (0..MAX_DAEMON_CONNECTIONS - 1)
        .map(|_| UnixStream::connect(&f.relay.paths.socket).unwrap())
        .collect::<Vec<_>>();
    f.relay
        .mcp_load(
            f.session.clone(),
            "stable".into(),
            Some("other-kit".into()),
            None,
        )
        .unwrap();
    drop(occupied);
    assert_eq!(f.prepared(), 1);
}

/// A different publication name still contends for the same project/home
/// scope. While a load holds it in cold preparation, a competing publish is
/// refused before any effect once the admission wait runs out.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn competing_publication_is_refused_after_the_scope_admission_wait() {
    let wait = Duration::from_secs(1);
    let f = LoadFixture::with_host(|host| host.with_scope_admission_wait(wait));
    f.hold_preparation();
    thread::scope(|threads| {
        let load = threads.spawn(|| f.guest("mcp load stable --kit other-kit"));
        f.wait_for_preparation();
        let effects = f.effects();
        let started = Instant::now();
        let rejected = f.guest("mcp publish another-name -- cat");
        assert!(!rejected.status.success());
        assert!(stderr(&rejected).contains("rejected before effect"));
        assert!(stderr(&rejected).contains("scope is busy"));
        assert!(started.elapsed() >= wait);
        assert_eq!(f.effects(), effects);
        assert!(f.published());
        f.release_preparation();
        let load = load.join().unwrap();
        assert!(load.status.success(), "{}", stderr(&load));
    });
    assert_eq!(f.prepared(), 1);
}

/// Public classification follows the admitted effect, never peer prose: a
/// stock peer that changes its load state and then prints a misleading error,
/// and a host peer that claims success on stdout without a typed result, are
/// both uncertain, and neither touches the publication.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn misleading_peer_output_is_uncertain_not_a_rejection_or_commit() {
    let f = LoadFixture::new();
    let load = |sandbox: &str| {
        f.relay
            .mcp_load(
                f.session.clone(),
                "stable".into(),
                None,
                Some(sandbox.into()),
            )
            .unwrap_err()
    };
    match load("fail-after-load") {
        DaemonError::Publication(PublicationOutcome::Uncertain { message }) => {
            assert!(message.contains("invalid request"));
        }
        other => panic!("stock effect failure collapsed into an argument error: {other:?}"),
    }
    assert!(f.path("loaded-before-failure").exists());
    assert!(f.published());
    let effects = f.effects();
    fs::write(f.path("stdout-only-result"), b"").unwrap();
    match load("target") {
        DaemonError::Publication(PublicationOutcome::Uncertain { .. }) => {}
        other => panic!("stdout/exit-zero was mistaken for a typed commit: {other:?}"),
    }
    assert_eq!(f.effects(), effects);
    assert!(f.published());
}

/// A registration whose generation no longer matches is refused before any
/// preparation; a failed preparation is uncertain and leaves the shell usable.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn stale_registration_and_failed_preparation_leave_the_shell_usable() {
    let f = LoadFixture::new();
    fs::write(f.path("registration.json"), f.stale_registration()).unwrap();
    let stale = f.guest("mcp load stable --kit other-kit");
    assert!(!stale.status.success());
    assert!(stderr(&stale).contains("Republish"), "{}", stderr(&stale));
    assert!(!stderr(&stale).contains("host MCP registration exited"));
    assert_eq!(f.prepared(), 0);
    fs::write(f.path("registration.json"), &f.registration).unwrap();

    fs::write(f.path("fail-prepare"), b"").unwrap();
    let failed = f.guest("mcp load stable --kit other-kit");
    assert!(!failed.status.success());
    assert!(stderr(&failed).contains("publication outcome uncertain"));
    match f
        .relay
        .mcp_load(
            f.session.clone(),
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
    fs::remove_file(f.path("fail-prepare")).unwrap();
    assert!(f.guest("ps --marsh").status.success());
    let reload = f.guest("mcp load stable --sandbox still-attached");
    assert!(reload.status.success(), "{}", stderr(&reload));
}

/// Stock peers outside marsh's lock cannot change the generation during
/// preparation unnoticed: the load is refused and never reaches stock load.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn registration_changed_during_preparation_is_refused() {
    let f = LoadFixture::new();
    f.hold_preparation();
    thread::scope(|threads| {
        let load = threads.spawn(|| f.guest("mcp load stable --kit other-kit"));
        f.wait_for_preparation();
        fs::write(f.path("registration.json"), f.stale_registration()).unwrap();
        let before = f.effects();
        f.release_preparation();
        let denied = load.join().unwrap();
        assert!(!denied.status.success());
        assert!(stderr(&denied).contains("Republish"), "{}", stderr(&denied));
        assert!(
            !f.effects()[before.len()..]
                .lines()
                .any(|line| line.starts_with("mcp load"))
        );
    });
}

/// Unpublish fences a slow preparation: the load is refused as revocation
/// pending, never reaches stock load, and the tool cannot be loaded after.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn unpublish_fences_a_slow_preparation() {
    let f = LoadFixture::new();
    let loads = |effects: &str| {
        effects
            .lines()
            .filter(|line| line.starts_with("mcp load"))
            .count()
    };
    f.hold_preparation();
    thread::scope(|threads| {
        let load = threads.spawn(|| f.guest("mcp load stable --kit other-kit"));
        f.wait_for_preparation();
        let revoke = threads.spawn(|| f.guest("mcp unpublish stable"));
        wait_until("revocation is marked pending", || {
            f.declaration_path.with_extension("revoke").exists()
        });
        let before = loads(&f.effects());
        f.release_preparation();
        let denied = load.join().unwrap();
        assert!(!denied.status.success());
        assert!(stderr(&denied).contains("revocation is pending"));
        let revoked = revoke.join().unwrap();
        assert!(revoked.status.success(), "{}", stderr(&revoked));
        assert_eq!(loads(&f.effects()), before);
    });
    let effects = f.effects();
    assert!(!f.guest("mcp load stable --kit other-kit").status.success());
    assert_eq!(f.effects(), effects);
    assert_eq!(f.prepared(), 1);
}

/// `publish --kit` prepares its target through the same path as load.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn publish_with_a_kit_target_boots_it_through_preparation() {
    let f = LoadFixture::new();
    let published = f.guest("mcp publish new-tool --kit other-kit -- cat");
    assert!(published.status.success(), "{}", stderr(&published));
    assert!(String::from_utf8_lossy(&published.stdout).contains("exact-ready-kit-vm"));
    assert!(stderr(&published).contains("[starting other-kit worker VM…]"));
    assert_eq!(f.prepared(), 1);
    assert!(f.guest("mcp unpublish new-tool").status.success());
}

/// The stock load carrier belongs to the daemon, not the host CLI that asked
/// for it: after that CLI dies, unpublish waits for the carrier to settle
/// before removing the registration.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn unpublish_waits_for_a_stock_load_whose_host_cli_died() {
    let f = LoadFixture::new();
    assert!(f.guest("mcp publish host-loss -- cat").status.success());
    let declaration = f
        .host
        .declaration_path(&f.session, PublicationKind::Mcp, "host-loss")
        .unwrap();
    thread::scope(|threads| {
        let load = threads.spawn(|| f.guest("mcp load host-loss --sandbox held-load"));
        wait_until("stock load starts", || {
            f.path("stock-load-entered").exists()
        });
        fs::write(f.path("kill-owned-host-cli"), b"").unwrap();
        wait_until("host CLI is killed", || {
            f.path("owned-host-cli-killed").exists()
        });
        let unpublish = threads.spawn(|| f.guest("mcp unpublish host-loss"));
        wait_until("revocation is marked pending", || {
            declaration.with_extension("revoke").exists()
        });
        fs::write(f.path("release-stock-load"), b"").unwrap();
        let result = load.join().unwrap();
        assert!(!result.status.success());
        assert!(stderr(&result).contains("uncertain"));
        let unpublished = unpublish.join().unwrap();
        assert!(unpublished.status.success(), "{}", stderr(&unpublished));
    });
    let effects = f.effects();
    let settled = effects.find("settled held-load").unwrap();
    let removed = effects.rfind("mcp rm").expect(&effects);
    assert!(
        settled < removed,
        "unpublish raced an unsettled stock carrier: {effects}"
    );
    assert!(!f.path("registration.json").exists());
}

fn running(pid: i32) -> bool {
    // A zombie is dead; an unreaped one still answers kill(0) on Linux.
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        let state = stat.rsplit(')').next().unwrap_or("").trim_start();
        return !(state.starts_with('Z') || state.starts_with('X'));
    }
    rustix::process::Pid::from_raw(pid)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

/// A stock carrier's descendant (here one that closed its output) does not
/// outlive the carrier to mutate state later.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn stock_descendants_do_not_outlive_their_carrier() {
    let f = LoadFixture::new();
    assert!(f.guest("mcp publish no-orphan -- cat").status.success());
    let loaded = f.guest("mcp load no-orphan --sandbox closed-output-child");
    assert!(loaded.status.success(), "{}", stderr(&loaded));
    assert!(f.guest("mcp unpublish no-orphan").status.success());
    let pid: i32 = fs::read_to_string(f.path("descendant-pid"))
        .unwrap()
        .parse()
        .unwrap();
    wait_until("the stock descendant ends", || !running(pid));
    assert!(!f.path("forbidden-late-mutation").exists());
}

/// A host budget short enough to outlast in seconds; real host work outside
/// preparation and stock calls takes well under it.
const SHORT_HOST_BUDGET: Duration = Duration::from_secs(10);

/// Kit preparation is backend-owned: a cold preparation longer than the
/// host's command budget still completes the load.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn cold_preparation_outlasts_the_host_budget() {
    let f = LoadFixture::with_host(|host| host.with_host_command_budget(SHORT_HOST_BUDGET));
    let preparation = SHORT_HOST_BUDGET + Duration::from_secs(3);
    fs::write(f.path("long-prepare"), preparation.as_secs().to_string()).unwrap();
    let loaded = f.guest("mcp load stable --kit other-kit");
    assert!(loaded.status.success(), "{}", stderr(&loaded));
    assert_eq!(f.prepared(), 1);
    assert!(f.published());
}

/// Supervised stock calls in a failed republish, rollback included, do not
/// count against the host budget: their sum outlasts it, and the prior
/// declaration and registration survive.
#[test]
#[ignore = "requires prebuilt MARSH_LOAD_TEST_BIN and its sibling host binaries"]
fn slow_stock_rollback_keeps_the_prior_publication() {
    let f = LoadFixture::with_host(|host| host.with_host_command_budget(SHORT_HOST_BUDGET));
    assert!(f.guest("mcp publish slow-rollback -- cat").status.success());
    let declaration = f
        .host
        .declaration_path(&f.session, PublicationKind::Mcp, "slow-rollback")
        .unwrap();
    let prior = fs::read(&declaration).unwrap();
    let prior_registration = f.registration();
    let calls_before = f.effects().lines().count();
    // Each stock `mcp` call (the controlled sbx delays only those) takes this.
    let delay = Duration::from_millis(1500);
    fs::write(f.path("stock-delay"), delay.as_secs_f64().to_string()).unwrap();
    let failed = f.guest("mcp publish slow-rollback --sandbox rollback-slow -- 'printf changed'");
    fs::remove_file(f.path("stock-delay")).unwrap();
    assert!(!failed.status.success());
    assert!(stderr(&failed).contains("uncertain"), "{}", stderr(&failed));
    assert!(
        stderr(&failed).contains("forced load failure"),
        "{}",
        stderr(&failed)
    );
    let slow_calls = f
        .effects()
        .lines()
        .skip(calls_before)
        .filter(|call| call.starts_with("mcp "))
        .count();
    assert!(
        delay * u32::try_from(slow_calls).unwrap() > SHORT_HOST_BUDGET,
        "{slow_calls} slow stock calls do not outlast the host budget"
    );
    assert_eq!(fs::read(&declaration).unwrap(), prior);
    assert_eq!(f.registration(), prior_registration);
    assert!(f.guest("mcp unpublish slow-rollback").status.success());
    assert!(!f.path("registration.json").exists());
}
