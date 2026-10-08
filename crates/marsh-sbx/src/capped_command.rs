//! Bounded stock control-plane capture, shared by source observation and brokers.
use crate::SbxError;
use marsh_runtime::{CommandOutput, CommandRunner, Invocation, retain_for_reaping};
use std::{
    io::{self, Read},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(5);
pub const STOCK_COMMAND_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

fn capture(
    mut input: Box<dyn Read + Send>,
    total: &AtomicUsize,
    exceeded: &AtomicBool,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = match input.read(&mut buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(output);
        }
        if total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |previous| {
                previous.checked_add(count).filter(|next| *next <= limit)
            })
            .is_err()
        {
            exceeded.store(true, Ordering::Release);
            return Err(io::Error::other(
                "stock control output exceeded its combined capture limit",
            ));
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

/// Execute one trusted stock control command with a hard combined stdout/stderr payload
/// limit and a process-plus-EOF deadline. Cancellation wakes both readers; native
/// process identity remains unreaped until its owned process group is signaled.
/// This bounds the local control carrier; remote effects and complete descendant
/// containment require separate runtime observations.
///
/// # Errors
/// Rejects non-cancellable runners, excess output, deadline, I/O, unknown exit,
/// or unverified cleanup. Teardown has a separate maximum two-second grace.
/// The native macOS termination observation (at most 500ms) uses that grace.
#[allow(clippy::too_many_lines)] // Retain process and both readers through one cleanup transaction.
pub fn run_stock_command_capped(
    runner: &dyn CommandRunner,
    invocation: &Invocation,
    timeout: Duration,
    limit: usize,
) -> Result<CommandOutput, SbxError> {
    if timeout.is_zero() || limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "stock control deadline and capture limit must be nonzero",
        )
        .into());
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::other("stock control deadline overflow"))?;
    let mut attachment = runner.spawn_attached(invocation)?;
    drop(attachment.stdin);
    if !attachment.process.supports_io_cancellation() {
        let _ = attachment.process.terminate();
        retain_for_reaping(attachment.process);
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bounded stock observation requires cancellable native I/O",
        )
        .into());
    }
    let total = Arc::new(AtomicUsize::new(0));
    let exceeded = Arc::new(AtomicBool::new(false));
    let mut readers = [attachment.stdout, attachment.stderr].map(|input| {
        let total = Arc::clone(&total);
        let exceeded = Arc::clone(&exceeded);
        Some(thread::spawn(move || {
            capture(input, &total, &exceeded, limit)
        }))
    });
    let mut outputs = [None, None];
    let mut code = None;
    let mut identity_uncertain = false;
    let result = (|| -> io::Result<()> {
        loop {
            for (reader, output) in readers.iter_mut().zip(&mut outputs) {
                if reader.as_ref().is_some_and(thread::JoinHandle::is_finished)
                    && let Some(completed) = reader.take()
                {
                    *output = Some(
                        completed
                            .join()
                            .map_err(|_| io::Error::other("stock output reader panicked"))??,
                    );
                }
            }
            if exceeded.load(Ordering::Acquire) {
                return Err(io::Error::other(
                    "stock control output exceeded its combined capture limit",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "stock process or output EOF deadline exceeded",
                ));
            }
            if code.is_none() {
                code = match attachment.process.try_wait_unreaped() {
                    Ok(code) => code,
                    Err(error) => {
                        identity_uncertain = true;
                        return Err(error);
                    }
                };
            }
            if code.is_some() && outputs.iter().all(Option::is_some) {
                return Ok(());
            }
            thread::sleep(POLL);
        }
    })();
    // No reader join before wakeup. Even an unrelated holder of stdout cannot
    // keep a native cancellation-aware reader asleep after preparation failed.
    // Closed stdio and terminal leader status do not close descendants' effect
    // authority. Signal the owned group while WNOWAIT still reserves its leader
    // identity on success as well as failure. Darwin zombie-only EPERM is handled
    // by the native process's exact singleton observation, never by EOF alone.
    let cleanup_deadline = Instant::now() + STOCK_COMMAND_CLEANUP_TIMEOUT;
    let cancellation = if result.is_err() {
        attachment.process.cancel_io()
    } else {
        Ok(())
    };
    let termination = if identity_uncertain {
        Err(io::Error::other(
            "termination withheld after uncertain child identity observation",
        ))
    } else {
        attachment.process.terminate()
    };
    let mut reaped = false;
    let mut wait_error = None;
    while Instant::now() < cleanup_deadline {
        match attachment.process.try_wait() {
            Ok(Some(actual)) => {
                code = Some(actual);
                reaped = true;
                break;
            }
            Ok(None) => {}
            Err(error) => {
                wait_error = Some(error.to_string());
                break;
            }
        }
        thread::sleep(POLL);
    }
    if !reaped {
        retain_for_reaping(attachment.process);
    }
    let mut joined = true;
    for reader in readers.into_iter().flatten() {
        while !reader.is_finished() && Instant::now() < cleanup_deadline {
            thread::sleep(POLL);
        }
        if reader.is_finished() {
            let _ = reader.join();
        } else {
            joined = false;
        }
    }
    if cancellation.is_err() || termination.is_err() || !reaped || !joined {
        return Err(SbxError::StockControlCleanupUncertain {
            control_error: result.as_ref().err().map(ToString::to_string),
            cancel_error: cancellation.err().map(|error| error.to_string()),
            terminate_error: termination.err().map(|error| error.to_string()),
            wait_error,
            reaped,
            readers_joined: joined,
        });
    }
    result?;
    let [Some(stdout), Some(stderr)] = outputs else {
        return Err(io::Error::other("stock control capture incomplete").into());
    };
    Ok(CommandOutput {
        exit_code: code,
        stdout,
        stderr,
    })
}
