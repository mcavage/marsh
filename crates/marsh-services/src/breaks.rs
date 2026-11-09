//! The one place E3 asks whether a break mode is on. Every check in `policy`
//! and `pins` calls [`break_mode`].
//!
//! The `test-breaks` feature forwards to `marsh-contracts/test-breaks`, so H2's
//! release guards apply (compile error under `MARSH_RELEASE_PACKAGING=1`, the
//! provenance marker, `scripts/check-no-test-breaks.sh`). `marsh_contracts::
//! break_mode` is consulted first. It is only ever set on the macOS daemon
//! (`init_from_daemon_launch`), so this process, which runs in Linux inside the
//! engine container, also takes the modes the daemon hands it in a dedicated
//! field ([`init`]: the agent's argv, never its environment). When H2 adds an
//! attested guest setter to `marsh_contracts`, [`init`] becomes a call to it.
//! Without the feature this is a constant `false` that reads nothing.
//!
//! Modes (E3): `filter-off`, `filter-no-upgrade`, `filter-case-sensitive`,
//! `filter-lenient-json`, `filter-raw-after-upgrade-header`, `filter-no-resolve`,
//! `filter-forward-original-bytes`, `bind-no-pin`, `bind-repin-unchecked`,
//! `bind-pin-leak`, `protected-subpin-off`, `lane-authority-off`,
//! `scope-reference-check-off`, `reserved-labels-allowed`,
//! `socket-substitute-raw`, `socket-refuse`, `services-resource-limit-off`,
//! `publish-nonloopback`, `capture-api-gate-off`.
//!
//! S3 modes and holds (capture, relay, copy) are checked through the same
//! function; `hold` is the harness barrier.

#[cfg(feature = "test-breaks")]
mod imp {
    use std::sync::OnceLock;
    static MODES: OnceLock<Vec<String>> = OnceLock::new();

    /// Modes arrive from the daemon's argv (never from this process's
    /// environment): `--break a,b`. The dev harness calls this once.
    pub fn init(modes: &[String]) {
        let _ = MODES.set(modes.to_vec());
    }

    /// The agent's own (attested) set, acknowledged as `test_breaks_agent`.
    #[must_use]
    pub fn active() -> Vec<String> {
        MODES.get().cloned().unwrap_or_default()
    }

    #[must_use]
    pub fn break_mode(name: &str) -> bool {
        marsh_contracts::break_mode(name)
            || MODES.get().is_some_and(|m| m.iter().any(|x| x == name))
    }
}

#[cfg(not(feature = "test-breaks"))]
mod imp {
    #[must_use]
    pub const fn break_mode(_name: &str) -> bool {
        false
    }

    #[must_use]
    pub const fn active() -> Vec<String> {
        Vec::new()
    }
}

pub use imp::active;
#[cfg(not(test))]
pub use imp::break_mode;
#[cfg(feature = "test-breaks")]
pub use imp::init;

/// Records the modes the daemon asked for (first call wins). Release builds ignore it.
pub fn install(modes: &[String]) {
    #[cfg(feature = "test-breaks")]
    imp::init(modes);
    #[cfg(not(feature = "test-breaks"))]
    let _ = modes;
}

/// Test barrier (harness delta 4): when the mode `hold-<point>` is active the
/// caller creates `<point>.reached` under `/run/marsh-test-holds` and waits up
/// to 120 s for `<point>.release`. `progress` runs every poll so the caller can
/// keep its heartbeat fresh while it waits.
pub fn hold(point: &str, progress: &mut dyn FnMut()) {
    #[cfg(feature = "test-breaks")]
    {
        use std::time::{Duration, Instant};
        if !break_mode(&format!("hold-{point}")) {
            return;
        }
        let dir = std::path::Path::new("/run/marsh-test-holds");
        let _ = std::fs::create_dir_all(dir);
        let reached = dir.join(format!("{point}.reached"));
        let release = dir.join(format!("{point}.release"));
        let _ = std::fs::write(&reached, b"");
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline && !release.exists() {
            progress();
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(&reached);
        let _ = std::fs::remove_file(&release);
    }
    #[cfg(not(feature = "test-breaks"))]
    {
        let _ = (point, progress);
    }
}

/// The unit-test build (never a shipped one) also takes modes from
/// `E3_TEST_BREAKS=a,b`, so `e3/witness.sh` can run each in-process witness with
/// the defect it guards put back and require it to fail.
#[cfg(test)]
#[must_use]
pub fn break_mode(name: &str) -> bool {
    std::env::var("E3_TEST_BREAKS").is_ok_and(|v| v.split(',').any(|m| m == name))
        || imp::break_mode(name)
}
