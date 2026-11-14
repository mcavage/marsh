//! How Brush's recorded-signals lock behaves once a panic has poisoned it.
//!
//! This is its own test binary: it installs signal handlers in the test
//! process, and raises signals in it.
#![cfg(unix)]

use brush_core::traps::{TrapHandlerConfig, TrapSignal, signal_records_for_test as hooks};

fn trap_on(traps: &mut TrapHandlerConfig, name: &str) {
    let signal: TrapSignal = name.parse().unwrap();
    traps.register_handler(signal, "echo".into(), brush_core::SourceInfo::default());
}

/// A panic with the lock held (nothing in the shell reaches one now, so the
/// test poisons it directly) must not stop signals being recorded, and a
/// forked child must still forget the arrivals it inherited.
#[test]
fn recording_and_the_post_fork_reset_survive_a_poisoned_lock() {
    use nix::sys::signal::{Signal, raise};

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let mut traps = TrapHandlerConfig::default();

    // The registry panics on these; the trap is kept but nothing is recorded.
    for (name, signal) in [
        ("SEGV", Signal::SIGSEGV),
        ("ILL", Signal::SIGILL),
        ("FPE", Signal::SIGFPE),
    ] {
        trap_on(&mut traps, name);
        assert_eq!(hooks::state(signal as i32), None, "{name}");
    }

    trap_on(&mut traps, "USR1");
    assert_eq!(hooks::state(Signal::SIGUSR1 as i32), Some((true, false)));
    raise(Signal::SIGUSR1).unwrap();
    assert_eq!(hooks::state(Signal::SIGUSR1 as i32), Some((true, true)));

    hooks::poison();

    // Still recorded and consulted: a poisoned lock is not an empty one.
    assert_eq!(hooks::state(Signal::SIGUSR1 as i32), Some((true, true)));
    trap_on(&mut traps, "USR2");
    assert_eq!(hooks::state(Signal::SIGUSR2 as i32), Some((true, false)));
    raise(Signal::SIGUSR2).unwrap();
    assert_eq!(hooks::state(Signal::SIGUSR2 as i32), Some((true, true)));

    // What a forked child does first: both signals are forgotten, not armed.
    hooks::forget_after_fork();
    assert_eq!(hooks::state(Signal::SIGUSR1 as i32), Some((false, false)));
    assert_eq!(hooks::state(Signal::SIGUSR2 as i32), Some((false, false)));
}
