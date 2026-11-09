//! Live driver for the services engine lifecycle (S2): starts a generation in an
//! already-admitted services VM (a hand-made `nfx-s2-*` sandbox until S1's
//! `ensure_services_vm` exists), prints every readiness transition and a
//! channel-gate probe as JSON lines, and with `--follow` starts a new
//! generation whenever the VM is running again after a loss.
//!
//! ```text
//! services_engine_drive --vm NAME --project DIR --artifacts DIR
//!     [--pool-bytes 4g] [--follow] [--probe-ms 100] [--detach-after-ms N] [--test-breaks-from-env]
//!     [--internet on|off | --grants-home DIR]
//! ```
//!
//! The Internet gate starts closed. `--internet on` opens it for this start;
//! `--grants-home DIR` reads the project's grant from that daemon scope's
//! `services-internet.json` instead, the way the daemon's engine start will
//! (E5, SECURITY-DESIGN A5).
//!
//! Break modes are selected the way the daemon's are: `MARSH_TEST_BREAK=a,b` in
//! this process's environment together with `--test-breaks-from-env`, read by
//! `marsh_contracts::break_mode` in a build with `test-breaks` (and ignored in
//! any other).
//!
//! Reads nothing from stdin; ends when the engine is lost (or never, with
//! `--follow`) or on SIGTERM. Not a product entry point.

use marsh_runtime::{CommandRunner, Invocation, SystemCommandRunner};
use marsh_sbx::services_engine::{
    EngineArtifacts, EngineHandle, EngineSettings, EngineState, ServicesEngine, ServicesVmRef,
    parse_size,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let mut vm = None;
    let mut project = None;
    let mut artifacts = None;
    let mut raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    marsh_contracts::test_breaks::init_from_daemon_launch(&mut raw);
    let mut settings = EngineSettings::from_env().expect("settings");
    let mut follow = false;
    let mut probe_ms = 100_u64;
    let mut detach_after = None;
    let mut grants_home: Option<PathBuf> = None;
    let mut arguments = raw
        .into_iter()
        .map(|argument| argument.to_string_lossy().into_owned());
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().expect("missing value");
        match argument.as_str() {
            "--vm" => vm = Some(value()),
            "--project" => project = Some(PathBuf::from(value())),
            "--artifacts" => artifacts = Some(PathBuf::from(value())),
            "--pool-bytes" => settings.pool_bytes = parse_size(&value()).expect("size"),
            "--probe-ms" => probe_ms = value().parse().expect("ms"),
            "--follow" => follow = true,
            "--internet" => settings.internet_open = value() == "on",
            "--grants-home" => grants_home = Some(PathBuf::from(value())),
            "--detach-after-ms" => detach_after = Some(value().parse::<u64>().expect("ms")),
            other => panic!("unknown argument {other}"),
        }
    }
    let vm = vm.expect("--vm");
    let sbx = PathBuf::from("sbx");
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemCommandRunner::new(
        std::env::var_os("HOME").map(PathBuf::from).expect("HOME"),
    ));
    let engine = ServicesEngine::new(sbx.clone(), Arc::clone(&runner));
    let artifacts = EngineArtifacts::from_guest_dir(&artifacts.expect("--artifacts"))
        .unwrap_or_else(|error| panic!("{error}"));
    let vm_ref = ServicesVmRef {
        name: vm.clone(),
        project_root: project.expect("--project"),
    };
    let origin = Instant::now();
    loop {
        let mut settings = settings.clone();
        if let Some(home) = &grants_home {
            // Read at every start, as the daemon's engine start will: a new
            // generation takes the grant as it is now.
            settings.internet_open =
                marsh_sbx::InternetGrants::new(home).is_on(&vm_ref.project_root.to_string_lossy());
        }
        let handle = engine.start(vm_ref.clone(), artifacts.clone(), settings);
        if let Some(after) = detach_after {
            std::thread::sleep(Duration::from_millis(after));
            let state = describe(&handle.state());
            let before = Instant::now();
            handle.detach();
            line(
                origin,
                &serde_json::json!({"detached_in_ms": before.elapsed().as_millis(), "state_before": state,
                    "state_after": describe(&handle.state())}),
            );
            return;
        }
        report(&handle, probe_ms, origin);
        let waited = Instant::now();
        while handle.diagnostic().is_none() && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(note) = handle.diagnostic() {
            line(origin, &serde_json::json!({"diagnostic": note}));
        }
        if !follow {
            return;
        }
        // Wait until the VM runs again (never start it ourselves: `sbx exec`
        // would).
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let listing = runner
                .run(&Invocation {
                    program: sbx.clone(),
                    arguments: vec!["ls".into()],
                    working_directory: None,
                    environment: Vec::new(),
                })
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default();
            if listing.lines().any(|line| {
                line.split_whitespace().next() == Some(vm.as_str()) && line.contains("running")
            }) {
                break;
            }
        }
    }
}

fn line(origin: Instant, value: &serde_json::Value) {
    let mut value = value.clone();
    value["t_ms"] = serde_json::json!(origin.elapsed().as_millis());
    println!("{value}");
}

fn describe(state: &EngineState) -> serde_json::Value {
    match state {
        EngineState::Starting => serde_json::json!({"state": "starting"}),
        EngineState::Ready(generation) => serde_json::json!({
            "state": "ready", "boot_id": generation.boot_id, "engine_id": generation.engine_id}),
        EngineState::Unavailable(reason) => {
            serde_json::json!({"state": "unavailable", "reason": reason})
        }
    }
}

fn report(handle: &EngineHandle, probe_ms: u64, origin: Instant) {
    line(origin, &describe(&EngineState::Starting));
    handle.subscribe(move |transition| {
        let mut value = describe(&transition.state);
        value["since_start_ms"] = serde_json::json!(transition.elapsed_ms);
        line(origin, &value);
    });
    // The channel gate: what E1 will call before opening any channel.
    let mut last = None;
    loop {
        let open = handle.ready_generation().is_some();
        if last != Some(open) {
            line(
                origin,
                &serde_json::json!({"probe": "channel-gate", "open": open}),
            );
            if open {
                line(
                    origin,
                    &serde_json::json!({"probe": "firewall", "report": handle.firewall()}),
                );
                let target = handle.relay_target();
                line(
                    origin,
                    &serde_json::json!({"probe": "relay-target", "container": target.as_ref().ok().and_then(|t| t.container.clone()),
                        "socket": target.as_ref().ok().map(|t| t.socket.clone())}),
                );
            }
            last = Some(open);
        }
        if matches!(handle.state(), EngineState::Unavailable(_)) {
            if last != Some(false) {
                line(
                    origin,
                    &serde_json::json!({"probe": "channel-gate", "open": false}),
                );
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(probe_ms));
    }
}
