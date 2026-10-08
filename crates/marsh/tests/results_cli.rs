//! Public results CLI against a real authenticated daemon with typed fixture receipts.
//! No SBX backend or VM is involved; this is not stock qualification.

use marsh_daemon::{
    CleanupState, DaemonStore, EndpointPaths, ExitStatus, NewJob, Placement, Server,
    SessionAuthority, TimingReport,
};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

// Keep Rust's workspace-wide unsafe-code prohibition. The public CLI is a
// separately observed, bounded process in its own session; Python is only the
// test process harness, never marsh's shell implementation.
const OWNED_CALLER: &str = r"
import json, os, pathlib, signal, subprocess, sys
p = subprocess.Popen(sys.argv[2:], start_new_session=True)
record = {'pid': p.pid, 'pgid': os.getpgid(p.pid)}
try:
    status = p.wait(timeout=8)
except subprocess.TimeoutExpired:
    os.killpg(p.pid, signal.SIGKILL)
    status = p.wait(timeout=1)
    record['timeout'] = True
record['status'] = status
pathlib.Path(sys.argv[1]).write_text(json.dumps(record))
sys.exit(status)
";

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    endpoint: EndpointPaths,
    store: DaemonStore,
    session_id: String,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let home = root.join("scope");
        fs::create_dir(&home).unwrap();
        let server = Server::bind(&home).unwrap();
        let store = server.store();
        let session_id = store.attach_shell(
            std::process::id(),
            SessionAuthority {
                username: "fixture".into(),
                uid: nix::unistd::Uid::effective().as_raw(),
                gid: nix::unistd::Gid::effective().as_raw(),
                launch_directory: root.clone(),
                guest_home: root.clone(),
                home_backing: home.clone(),
                ephemeral_home: false,
            },
        );
        let endpoint = EndpointPaths::for_home(&home).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let server = thread::spawn(move || {
            server
                .serve_until(|| stopping.load(Ordering::Acquire))
                .unwrap();
        });
        Self {
            _directory: directory,
            root,
            endpoint,
            store,
            session_id,
            stop,
            server: Some(server),
        }
    }

    fn begin(&self, placement: Placement) -> String {
        self.store
            .begin_job_at(
                NewJob {
                    session_id: self.session_id.clone(),
                    command: "receipt-fixture".into(),
                    kit_ref: "fixture-only-no-vm".into(),
                    workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                    mounts: vec![],
                },
                placement,
            )
            .unwrap()
            .0
    }

    fn results(&self, arguments: &[&str]) -> Output {
        let started = Instant::now();
        let identity = tempfile::NamedTempFile::new_in(&self.root).unwrap();
        let mut child = Command::new("/usr/bin/python3")
            .args(["-c", OWNED_CALLER])
            .arg(identity.path())
            .arg(env!("CARGO_BIN_EXE_marsh"))
            .arg("results")
            .args(arguments)
            .current_dir(&self.root)
            .env_clear()
            .env("USER", "fixture")
            .env("HOME", &self.root)
            .env("MARSH_HOME", self.root.join("scope"))
            .env("MARSH_DAEMON_SOCKET", &self.endpoint.socket)
            .env("MARSH_DAEMON_TOKEN", &self.endpoint.token)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                panic!(
                    "results CLI timed out: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        let output = child.wait_with_output().unwrap();
        eprintln!(
            "owned results caller {}",
            fs::read_to_string(identity.path()).unwrap()
        );
        eprintln!(
            "results args={arguments:?} elapsed={:?} status={:?} stdout={:?} stderr={:?}",
            started.elapsed(),
            output.status,
            output.stdout,
            output.stderr
        );
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.server.take().unwrap().join().unwrap();
    }
}

#[test]
fn human_results_preserve_copyable_ids_placement_and_uncertain_cleanup() {
    let fixture = Fixture::new();
    let id = fixture.begin(Placement::Local);
    fixture
        .store
        .finish_job(
            &id,
            ExitStatus {
                code: Some(7),
                cause: "fixture failure".into(),
            },
            true,
            CleanupState::Uncertain,
            TimingReport::default(),
        )
        .unwrap();
    let listed = fixture.results(&[]);
    let text = String::from_utf8(listed.stdout).unwrap();
    let row = text.lines().nth(1).expect("one real daemon receipt");
    let displayed_id = row.split_whitespace().nth(1).expect("JOB column");
    assert_eq!(displayed_id, id, "human IDs must be complete: {text}");
    let shown = fixture.results(&["show", displayed_id, "--json"]);
    let receipt: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(receipt["job_id"], displayed_id);
    // Uncertain cleanup never reports the observed exit as the public code.
    assert_eq!(receipt["exit"]["code"], 125);
    assert_eq!(receipt["cleanup"], "uncertain");
    assert!(row.contains("local") && row.contains("uncertain"), "{text}");
}
