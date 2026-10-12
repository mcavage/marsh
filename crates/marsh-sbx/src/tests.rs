//! Adapter tests. Stock SBX is the stateful model in `stock_model` (driven by
//! captured CLI output in `tests/data/sbx`); assertions are on outcomes: VM
//! inventory, guest files, mounts, ownership records and errors.
use super::*;
use marsh_contracts::JobSignal;
use marsh_runtime::{AttachedProcess, no_attachment_control};
use std::{
    io::{self, Cursor},
    sync::{
        Barrier, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

mod corpus;
mod grants;
mod kit_lifecycle;
mod local_kit;
mod shell;
mod source_grant_regressions;
mod stock_cli;
mod stock_model;
mod vm_ownership_rules;

use stock_model::{Fault, StockModel, WAIT_LIMIT, failure};

/// Host temp root with symlinked ancestry resolved (macOS `/var` ->
/// `/private/var`): grant sources reject symlinks anywhere in their chain.
fn canonical_temp_dir() -> PathBuf {
    fs::canonicalize(std::env::temp_dir()).unwrap()
}

fn scratch() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("marsh-sbx-test-")
        .tempdir_in(canonical_temp_dir())
        .unwrap()
}

fn directory(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    fs::create_dir_all(&path).unwrap();
    path
}

/// The packaged worker with its required byte-exec and job artifact siblings.
fn worker_artifacts(parent: &Path) -> PathBuf {
    let directory = directory(parent, "artifacts");
    fs::write(
        directory.join("marsh-byte-exec-linux-arm64"),
        b"byte-exec helper",
    )
    .unwrap();
    fs::write(directory.join("marsh-local-linux-arm64"), b"job artifact").unwrap();
    let worker = directory.join("marsh-worker-linux-arm64");
    fs::write(&worker, b"worker").unwrap();
    worker
}

fn write_local_source(source: &Path) {
    fs::create_dir_all(source).unwrap();
    fs::write(
        source.join("fixture.yaml"),
        "# syntax=docker/sandbox-kit:3\nschemaVersion: \"3\"\nkind: workload\ndockerfile: ./fixture.dockerfile\n",
    )
    .unwrap();
    fs::write(source.join("fixture.dockerfile"), "FROM scratch\n").unwrap();
}

fn workload_ref() -> NativeKitRef {
    NativeKitRef::immutable_oci(format!(
        "registry.example/sbx-kit-fixture@sha256:{}",
        "d".repeat(64)
    ))
    .unwrap()
}

/// A Kit spec whose worker artifacts, lifecycle home and (for local Kits)
/// source live in one temporary directory.
struct KitFixture {
    dir: tempfile::TempDir,
    spec: KitVmSpec,
}

impl KitFixture {
    fn published(name: &str) -> Self {
        let dir = scratch();
        let spec = KitVmSpec {
            name: name.into(),
            worker_binary: worker_artifacts(dir.path()),
            workload_kit: workload_ref(),
            lifecycle_workspace: directory(dir.path(), "home"),
        };
        Self { dir, spec }
    }

    fn local(name: &str) -> Self {
        let dir = scratch();
        let source = dir.path().join("source");
        write_local_source(&source);
        let spec = KitVmSpec {
            name: name.into(),
            worker_binary: worker_artifacts(dir.path()),
            workload_kit: NativeKitRef::local_v3_source(source).unwrap(),
            lifecycle_workspace: directory(dir.path(), "home"),
        };
        Self { dir, spec }
    }

    fn source(&self) -> PathBuf {
        self.dir.path().join("source")
    }

    fn home(&self) -> &Path {
        &self.spec.lifecycle_workspace
    }

    fn home_entries(&self) -> Vec<String> {
        fs::read_dir(self.home())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    /// The ownership marker a VM prepared for this spec carries.
    fn published_marker(&self) -> Vec<u8> {
        let lifecycle = selected_home_identity(self.home()).unwrap();
        let mut marker = serde_json::to_vec(&StockSbx::marker(&format!(
            "kit:{}:{lifecycle}",
            self.spec.workload_kit.identity()
        )))
        .unwrap();
        marker.push(b'\n');
        marker
    }
}

/// A cold published Kit VM prepared through the adapter.
fn warm_kit(model: &Arc<StockModel>, name: &str) -> (StockSbx, KitFixture, ReadyKitVm) {
    let adapter = model.adapter();
    let kit = KitFixture::published(name);
    let ready = adapter.ensure_kit_vm(&kit.spec).unwrap();
    (adapter, kit, ready)
}

struct ShellFixture {
    dir: tempfile::TempDir,
    spec: ShellVmSpec,
}

impl ShellFixture {
    fn new() -> Self {
        let dir = scratch();
        let binary = directory(dir.path(), "bin").join("marsh-linux-arm64");
        fs::write(&binary, b"marsh shell").unwrap();
        let spec = ShellVmSpec {
            name: "marsh-shell-cache".into(),
            image: OciImage::parse(format!("dhi.example/shell@sha256:{}", "e".repeat(64))).unwrap(),
            shell_binary: binary,
            user: ShellUser {
                name: "alice".into(),
                uid: 501,
                gid: 20,
                home: "/Users/alice".into(),
            },
        };
        Self { dir, spec }
    }

    /// The packaged relay beside the shell binary.
    fn relay(&self) -> PathBuf {
        let relay = self
            .spec
            .shell_binary
            .with_file_name("marsh-relay-linux-arm64");
        fs::write(&relay, b"relay").unwrap();
        fs::set_permissions(&relay, fs::Permissions::from_mode(0o755)).unwrap();
        relay
    }

    fn project(&self) -> PathBuf {
        directory(self.dir.path(), "project")
    }

    fn selected_home(&self) -> PathBuf {
        directory(self.dir.path(), "selected-home")
    }
}

fn grant(source: PathBuf, target: &str, access: MountAccess) -> AdmittedHostGrant {
    AdmittedHostGrant::open(source, target.into(), access).unwrap()
}

fn shell_grants(project: &Path, selected_home: &Path) -> Vec<AdmittedHostGrant> {
    vec![
        grant(
            project.to_owned(),
            project.to_str().unwrap(),
            MountAccess::ReadWrite,
        ),
        grant(
            selected_home.to_owned(),
            "/Users/alice",
            MountAccess::ReadWrite,
        ),
    ]
}

fn job_spec() -> marsh_contracts::JobSpec {
    marsh_contracts::JobSpec {
        image: marsh_contracts::OciImage::parse(format!(
            "registry.example/job@sha256:{}",
            "d".repeat(64)
        ))
        .unwrap(),
        argv: vec![b"true".to_vec()],
        identity: marsh_contracts::JobIdentity { uid: 501, gid: 20 },
        exported_environment: BTreeMap::new(),
        session_environment: BTreeMap::from([
            ("HOME".into(), "/Users/alice".into()),
            ("LOGNAME".into(), "alice".into()),
            ("MARSH_SELECTED_HOME".into(), "/Users/alice".into()),
            ("USER".into(), "alice".into()),
        ]),
        working_directory: "/Users/alice".into(),
        mounts: Vec::new(),
        resources: marsh_contracts::JobResources {
            cpu_millis: 1000,
            memory_bytes: 1024 * 1024,
            pids: 8,
            writable_bytes: 1024 * 1024,
            output_bytes: 1024 * 1024,
            wall_seconds: 10,
        },
        terminal: false,
        terminal_size: None,
        split_capability: None,
        capability: None,
    }
}

/// Run `work` on a thread; a hung adapter fails the test instead of hanging it.
fn bounded<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let _ = send.send(work());
    });
    receive
        .recv_timeout(WAIT_LIMIT)
        .expect("adapter call did not return")
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT_LIMIT;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        thread::sleep(Duration::from_millis(1));
    }
}

/// A Kit VM handle for a VM the test seeded in the model.
fn kit_vm(name: &str) -> ReadyKitVm {
    ReadyKitVm {
        name: name.into(),
        kit_ref: workload_ref().identity().into(),
        lifecycle_workspace: "/Users/alice/.marsh/kit-lifecycle/fixture".into(),
        cold_started: false,
        job_image: workload_ref().workload_image().unwrap().clone(),
        worker_binary: "/unused/marsh-worker".into(),
    }
}

/// A shell VM handle for a VM the test seeded in the model.
fn shell_vm(name: &str) -> ReadyShellVm {
    ReadyShellVm {
        name: name.into(),
        user: ShellFixture::new().spec.user,
        cold_started: false,
    }
}

/// Assert `receive` has no result yet: a one-sided probe that can only pass
/// spuriously (on a slow machine), never fail spuriously.
fn assert_still_blocked<T: std::fmt::Debug>(receive: &mpsc::Receiver<T>, what: &str) {
    assert!(
        matches!(
            receive.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ),
        "{what} finished while it should still be blocked"
    );
}

#[test]
fn worker_loss_reason_keeps_a_printable_bounded_stderr_tail() {
    let loss = WorkerLoss::default();
    assert_eq!(loss.describe(), None);
    loss.record_stderr(&vec![b'x'; 4096]);
    loss.record_stderr(b"\nthread 'main' panicked at worker.rs:1:\n\x1b[31mboom\x1b[0m\n");
    loss.set("worker transport ended: unexpected EOF (carrier exited 101)".into());
    loss.set("a later reason is ignored".into());
    let described = loss.describe().unwrap();
    assert!(described.starts_with(
        "worker transport ended: unexpected EOF (carrier exited 101); worker stderr: "
    ));
    assert!(
        described.ends_with("panicked at worker.rs:1: [31mboom [0m"),
        "{described}"
    );
    assert!(!described.chars().any(char::is_control));
    assert!(described.len() < 700);
}

#[test]
fn retained_response_budget_drops_only_output_after_limit_and_keeps_terminal() {
    let (send, _receive) = mpsc::channel();
    let mut route = AttemptRoute {
        send,
        queued_bytes: 0,
        byte_limit: 3,
        overflowing: false,
    };
    assert!(route.admit(&WorkerResponse::Stdout {
        attempt: "attempt-1".into(),
        bytes: vec![1, 2],
    }));
    assert!(!route.admit(&WorkerResponse::Stderr {
        attempt: "attempt-1".into(),
        bytes: vec![3, 4],
    }));
    assert!(route.overflowing);
    assert_eq!(route.queued_bytes, 2);
    assert!(route.admit(&WorkerResponse::Terminal {
        attempt: "attempt-1".into(),
        report: marsh_worker::WorkerReport {
            container_id: None,
            execution: marsh_worker::ExecutionOutcome::SupervisionFailed,
            delivery: marsh_worker::DeliveryOutcome::Failed,
            control_errors: 1,
            retained_processes: Vec::new(),
            cleanup: marsh_worker::CleanupOutcome::Verified,
            quarantine: false,
        },
    }));
}

#[test]
fn retained_worker_readiness_wait_is_bounded() {
    let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    // The writer stays open and silent: only the bound can end the wait.
    let result =
        bounded(move || read_worker_ready(Box::new(reader), Duration::from_millis(25)).map(|_| ()));
    drop(writer);
    assert!(matches!(
        result,
        Err(SbxError::WorkerLeaseLost(message)) if message.contains("readiness timed out")
    ));
}
