use clap::Parser;
use std::io::Write;

use brush_core::traps::TrapSignal;
use brush_core::{ExecutionExitCode, ExecutionResult, builtins, sys};

/// Signal a job or process.
#[derive(Parser)]
pub(crate) struct KillCommand {
    /// Name of the signal to send.
    #[arg(short = 's', value_name = "SIG_NAME")]
    signal_name: Option<String>,

    /// Number of the signal to send.
    #[arg(short = 'n', value_name = "SIG_NUM")]
    signal_number: Option<usize>,

    //
    // TODO(kill): implement -sigspec syntax
    /// List known signal names.
    #[arg(short = 'l', short_alias = 'L')]
    list_signals: bool,

    // Interpretation of these depends on whether -l is present.
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for KillCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // Default signal is SIGTERM, as in Bash.
        let mut trap_signal = TrapSignal::Signal(nix::sys::signal::Signal::SIGTERM);

        // Try parsing the signal name (if specified).
        if let Some(signal_name) = &self.signal_name {
            if let Ok(parsed_trap_signal) = TrapSignal::try_from(signal_name.as_str()) {
                trap_signal = parsed_trap_signal;
            } else {
                writeln!(
                    context.stderr(),
                    "{}: invalid signal name: {}",
                    context.command_name,
                    signal_name
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
        }

        // Try parsing the signal number (if specified).
        if let Some(signal_number) = &self.signal_number {
            #[expect(clippy::cast_possible_truncation)]
            #[expect(clippy::cast_possible_wrap)]
            if let Ok(parsed_trap_signal) = TrapSignal::try_from(*signal_number as i32) {
                trap_signal = parsed_trap_signal;
            } else {
                writeln!(
                    context.stderr(),
                    "{}: invalid signal number: {}",
                    context.command_name,
                    signal_number
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
        }

        // A leading -sigspec (or `--`) may precede the operands; every later
        // argument is a pid, negative process group, or job spec.
        let mut operands = self.args.as_slice();
        if let Some(first) = operands.first() {
            if first == "--" {
                operands = &operands[1..];
            } else if let Some(possible_sigspec) = first.strip_prefix('-') {
                // See if this is -sigspec syntax. The sigspec may be a signal name
                // (e.g., -TERM) or a signal number (e.g., -9).
                if let Ok(parsed_trap_signal) = possible_sigspec.parse::<TrapSignal>() {
                    trap_signal = parsed_trap_signal;
                } else {
                    writeln!(
                        context.stderr(),
                        "{}: {}: invalid signal specification",
                        context.command_name,
                        possible_sigspec
                    )?;
                    return Ok(ExecutionResult::general_error());
                }
                operands = &operands[1..];
                if operands.first().is_some_and(|arg| arg == "--") {
                    operands = &operands[1..];
                }
            }
        }

        if self.list_signals {
            return print_signals(&context, self.args.as_ref());
        }
        if operands.is_empty() {
            writeln!(context.stderr(), "{}: invalid usage", context.command_name)?;
            return Ok(ExecutionExitCode::InvalidUsage.into());
        }

        let mut result = ExecutionResult::success();
        for pid_or_job_spec in operands {
            let sent = if pid_or_job_spec.starts_with('%') {
                // It's a job spec.
                if let Some(job) = context.shell.jobs_mut().resolve_job_spec(pid_or_job_spec) {
                    job.kill(trap_signal)
                } else {
                    writeln!(
                        context.stderr(),
                        "{}: {}: no such job",
                        context.command_name,
                        pid_or_job_spec
                    )?;
                    result = ExecutionResult::general_error();
                    continue;
                }
            } else {
                brush_core::int_utils::parse(pid_or_job_spec.as_str(), 10)
                    .and_then(|pid| sys::signal::kill_process(pid, trap_signal))
            };
            if let Err(error) = sent {
                writeln!(
                    context.stderr(),
                    "{}: {pid_or_job_spec}: {error}",
                    context.command_name
                )?;
                result = ExecutionResult::general_error();
            }
        }
        Ok(result)
    }
}

fn print_signals(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    signals: &[String],
) -> Result<ExecutionResult, brush_core::Error> {
    let mut exit_code = ExecutionResult::success();
    if !signals.is_empty() {
        for s in signals {
            // If the user gives us a code, we print the name; if they give a name, we print its
            // code.
            enum PrintSignal {
                Name(&'static str),
                Num(i32),
            }

            let signal = if let Ok(n) = s.parse::<i32>() {
                // bash compatibility. `SIGHUP` -> `HUP`
                TrapSignal::try_from(n).map(|s| {
                    PrintSignal::Name(s.as_str().strip_prefix("SIG").unwrap_or(s.as_str()))
                })
            } else {
                TrapSignal::try_from(s.as_str()).map(|sig| {
                    i32::try_from(sig).map_or(PrintSignal::Name(sig.as_str()), PrintSignal::Num)
                })
            };

            match signal {
                Ok(PrintSignal::Num(n)) => {
                    writeln!(context.stdout(), "{n}")?;
                }
                Ok(PrintSignal::Name(s)) => {
                    writeln!(context.stdout(), "{s}")?;
                }
                Err(e) => {
                    writeln!(context.stderr(), "{e}")?;
                    exit_code = ExecutionResult::general_error();
                }
            }
        }
    } else {
        return brush_core::traps::format_signals(
            context.stdout(),
            TrapSignal::iterator().filter(|s| !matches!(s, TrapSignal::Exit)),
        )
        .map(|()| ExecutionResult::success());
    }

    Ok(exit_code)
}
