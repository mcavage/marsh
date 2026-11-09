//! The worker's test break modes for the job endpoint and port forwards (E2).
//! They arrive from the daemon in the job's dedicated `services.breaks` field
//! (DECISIONS 22), never from this process's environment, and are compiled out
//! of release builds: every check is `marsh_contracts::break_mode`, which is a
//! constant `false` without the `test-breaks` feature.

use marsh_contracts::break_mode;

/// Breaks the worker takes: listeners made before the namespace is entered,
/// listeners kept past the attempt, forwards not revoked.
pub(crate) const LISTENER_BEFORE_SETNS: &str = "listener-before-setns";
pub(crate) const LISTENER_RETAIN_ON_END: &str = "listener-retain-on-end";
pub(crate) const PORTS_KEEP_STALE: &str = "ports-keep-stale";
/// Fault: the worker's `setns` fails with `EPERM`.
pub(crate) const FAULT_SETNS_FAIL: &str = "fault-setns-fail";

/// Every mode the daemon hands to the worker.
pub const WORKER_MODES: &[&str] = &[
    LISTENER_BEFORE_SETNS,
    LISTENER_RETAIN_ON_END,
    PORTS_KEEP_STALE,
    FAULT_SETNS_FAIL,
    "hold-netns-attach",
    "docker-env-open",
];

pub(crate) fn on(name: &str) -> bool {
    break_mode(name)
}

/// Test barrier (harness delta 4): with `hold-netns-attach` on, create
/// `<point>.reached` under `/run/marsh-test-holds` and wait up to 120 s for
/// `<point>.release`, or until `stop` says the attempt is ending (shutdown joins
/// the forward manager, which must not sit out the rest of the wait).
pub(crate) fn hold(point: &str, stop: &dyn Fn() -> bool) {
    #[cfg(feature = "test-breaks")]
    {
        use std::time::{Duration, Instant};
        if !on(&format!("hold-{point}")) {
            return;
        }
        let dir = std::path::Path::new("/run/marsh-test-holds");
        let _ = std::fs::create_dir_all(dir);
        let reached = dir.join(format!("{point}.reached"));
        let release = dir.join(format!("{point}.release"));
        let _ = std::fs::write(&reached, b"");
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline && !release.exists() && !stop() {
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(&reached);
        let _ = std::fs::remove_file(&release);
    }
    #[cfg(not(feature = "test-breaks"))]
    let _ = (point, stop);
}
