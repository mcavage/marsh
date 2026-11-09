//! The services agent: runs as root inside a project's services VM, in the
//! `marsh-svc-engine` container, next to the production Docker engine.
//! Entered as `marsh --internal-services <command>`.
//!
//! Design: `sdlc/ENG-PLAN.md` D3 and `sdlc/SECURITY-DESIGN.md` A1-A4, A8, B1-B2
//! in the Narwhal Fork spike repository. Each module is owned by one task.

pub mod breaks; // E3: break modes, through marsh_contracts::break_mode (feature test-breaks) and the agent's argv
pub mod capture; // S3: pause, reflink, unpause; opaque snapshot ids
pub mod engine; // S2: readiness generation as seen from inside the engine
pub mod engine_api; // S3: the agent's own client for the production engine
pub mod firewall; // S2b: engine egress firewall for the sbx gateway (A12)
pub mod frame; // S3: typed, size-capped relay frames
#[cfg(target_os = "linux")]
pub mod freeze; // S3: freeze journal, thaw confirmation, watchdog
#[cfg(target_os = "linux")]
pub mod fsops; // S3: descriptor-relative snapshot filesystem operations
pub mod host; // E1: the daemon side of the relay (typed channels, bounds, validation)
pub mod internet; // E5: the services Internet gate (A5)
pub mod ledger; // E3: workspace scope ledger (A4)
pub mod locks; // S3: per-workspace operation lock and API gate (shared with E3)
pub mod pins; // E3: bind-source pinning (A3)
pub mod policy; // E3: strict HTTP parse, rebuild, route and schema allowlists (B1-B2)
pub mod ports; // S3: published ports from the engine's event stream
pub mod reap; // E4: stop and remove an attempt's containers at its end (DECISIONS 11)
pub mod relay; // S3/E1: stdio channel server (typed frames, bounds)
pub mod restore; // S4: restore through the filtered socket

/// Entry point for `marsh --internal-services ARGS...`. Returns the exit code.
#[must_use]
pub fn run(arguments: &[std::ffi::OsString]) -> i32 {
    let words: Vec<String> = arguments
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match words.first().map(String::as_str) {
        Some("engine-up") => engine::run_engine_up(&words[1..]),
        Some("relay") => relay::run(&words[1..]),
        Some("internet") => internet::run(&words[1..]),
        #[cfg(target_os = "linux")]
        Some("capture-watchdog") => freeze::watchdog_main(&words[1..]),
        Some("restore-client") => restore::client_main(&words[1..]),
        _ => {
            eprintln!(
                "marsh: internal services command not implemented: {}",
                words.join(" ")
            );
            2
        }
    }
}
