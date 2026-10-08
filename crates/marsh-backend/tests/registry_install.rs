//! Real authenticated daemon callers. Rejected install declarations must not
//! invoke stock SBX, rewrite the host overlay, or publish a command.
use marsh_backend::{BackendConfig, RegisteredKit, StockDaemonBackend};
use marsh_contracts::{JobResources, OciImage};
use marsh_daemon::{Client, DaemonBackend, Server};
use marsh_runtime::{Attachment, CommandOutput, CommandRunner, Invocation};
use marsh_sbx::{NativeKitRef, ShellUser, ShellVmSpec, StockSbx};
use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Default)]
struct EffectCounter(AtomicUsize);
impl CommandRunner for EffectCounter {
    fn run(&self, _: &Invocation) -> io::Result<CommandOutput> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("stock probe reached"))
    }
    fn run_bounded(&self, invocation: &Invocation, _: Duration) -> io::Result<CommandOutput> {
        self.run(invocation)
    }
    fn spawn_attached(&self, _: &Invocation) -> io::Result<Attachment> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::other("stock attachment reached"))
    }
}

#[test]
fn invalid_overlay_or_install_has_zero_stock_and_registry_effects() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let control = root.path().join("control");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&control).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&control, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.path().join("worker"), b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(
        root.path().join("worker"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let reference = format!("example/kit@sha256:{}", "a".repeat(64));
    let effects = Arc::new(EffectCounter::default());
    let backend = Arc::new(StockDaemonBackend::new(
        Arc::new(StockSbx::new("unused-stock", effects.clone())),
        BTreeMap::from([(
            "existing".into(),
            RegisteredKit {
                workload: NativeKitRef::immutable_oci(reference.clone()).unwrap(),
            },
        )]),
        BackendConfig {
            worker_binary: root.path().join("worker"),
            relay_binary: root.path().join("relay"),
            daemon_home: home.clone(),
            control_home: control.clone(),
            protected_guest_roots: vec![],
            shell: ShellVmSpec {
                name: "unused-shell".into(),
                image: OciImage::parse(reference.clone()).unwrap(),
                shell_binary: root.path().join("shell"),
                user: ShellUser {
                    name: "fixture".into(),
                    uid: 1000,
                    gid: 1000,
                    home: home.clone(),
                },
            },
            resources: JobResources {
                cpu_millis: 1000,
                memory_bytes: 134_217_728,
                pids: 32,
                writable_bytes: 16_777_216,
                output_bytes: 1_048_576,
                wall_seconds: 10,
            },
            env: marsh_backend::config::EnvironmentConfig::default(),
        },
    ));
    let server = Server::bind(&home).unwrap().with_backend(backend.clone());
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let serving =
        thread::spawn(move || server.serve_until(|| done.load(Ordering::Relaxed)).unwrap());
    let client = Client::connect(&home).unwrap();
    let registry = control.join("commands.json");
    let invalid_documents = [
        format!(r#"{{"foo":"{reference}","foo":"{reference}"}}"#),
        format!(r#"{{"ps":"{reference}"}}"#),
        serde_json::to_string(&BTreeMap::from([("x".repeat(129), reference.clone())])).unwrap(),
        serde_json::to_string(
            &(0..251)
                .map(|n| (format!("cmd{n}"), reference.clone()))
                .collect::<BTreeMap<_, _>>(),
        )
        .unwrap(),
        "{broken".into(),
        r#"{"foo":"missing-source"}"#.into(),
    ];
    // Keep cleanup unconditional even if a caller assertion fails.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for document in invalid_documents {
            fs::write(&registry, &document).unwrap();
            let files_before = fs::read_dir(&control).unwrap().count();
            assert!(client.install_kit("newkit", &reference).is_err());
            assert_eq!(
                effects.0.load(Ordering::SeqCst),
                0,
                "invalid persisted overlay reached stock"
            );
            assert_eq!(fs::read_to_string(&registry).unwrap(), document);
            assert_eq!(fs::read_dir(&control).unwrap().count(), files_before);
            assert_eq!(backend.registered_commands().unwrap(), ["existing"]);
        }
        fs::write(&registry, "{}").unwrap();
        for name in ["ps", "-flag", "existing", &"x".repeat(129)] {
            assert!(client.install_kit(name, &reference).is_err());
            assert_eq!(effects.0.load(Ordering::SeqCst), 0);
            assert_eq!(fs::read(&registry).unwrap(), b"{}");
        }
    }));
    drop(client);
    stop.store(true, Ordering::Relaxed);
    serving.join().unwrap();
    result.unwrap();
}
