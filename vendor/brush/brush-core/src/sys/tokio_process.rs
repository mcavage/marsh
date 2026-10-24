//! Process management utilities

use std::process::Stdio;

use crate::{
    ShellFd,
    openfiles::OpenFiles,
    processes::{ChildProcess, LaunchRequest, ProcessGroup},
    sys::commands::{self, CommandExt, CommandFdInjectionExt, CommandFgControlExt},
};

pub(crate) type ProcessId = i32;

/// Starts a native child process.
///
/// # Arguments
///
/// * `command` - The process to start.
pub(crate) fn spawn(command: LaunchRequest) -> std::io::Result<ChildProcess> {
    let process_group = command.process_group;
    // See `CreateOptions::kill_external_commands_on_drop` (false for ordinary shells).
    let kill_on_drop = command.kill_on_drop;

    let (command, blocked) = compose_std_command(command, true)?;
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(kill_on_drop);
    let spawned = command.spawn();
    drop(blocked);
    let child = spawned?;

    #[expect(clippy::cast_possible_wrap)]
    let pid = child.id().map(|id| id as ProcessId);
    if pid.is_none() {
        tracing::warn!("could not retrieve pid for child process");
    }

    let pgid = match process_group {
        ProcessGroup::Inherit => None,
        ProcessGroup::Join(pgid) => Some(pgid),
        ProcessGroup::New { .. } | ProcessGroup::NewSession => pid,
    };

    Ok(ChildProcess::from_output_future(child.wait_with_output()).native_child(pid, pgid))
}

/// Replaces the current process with a new one. Returns only if that fails, with
/// the reason why.
///
/// # Arguments
///
/// * `command` - The process to replace the current one with.
pub(crate) fn exec(command: LaunchRequest) -> std::io::Error {
    match compose_std_command(command, false) {
        Ok((command, _)) => commands::exec(command),
        Err(err) => err,
    }
}

/// Signals stay blocked from just before a fork until the child has default
/// dispositions, so one sent before it execs is not lost to the shell's handlers.
#[cfg(unix)]
type ForkGuard = Option<crate::sys::signal::BlockedForFork>;
#[cfg(not(unix))]
type ForkGuard = ();

/// Composes the `std::process::Command` that starts a process. When `forking`,
/// the returned guard is to be held until the process has started.
///
/// Text in the request is the shell's, in which bytes that aren't UTF-8 stand
/// for themselves (see [`crate::bytes`]); the command has the bytes.
fn compose_std_command(
    command: LaunchRequest,
    forking: bool,
) -> std::io::Result<(std::process::Command, ForkGuard)> {
    use crate::bytes::to_os_string;

    let mut cmd = std::process::Command::new(&command.program);

    // Override argv[0].
    // NOTE: Not supported on all platforms.
    cmd.arg0(to_os_string(&command.argv0));
    cmd.args(command.args.iter().map(|arg| to_os_string(arg)));
    cmd.current_dir(command.working_dir);
    cmd.env_clear();
    cmd.envs(
        command
            .env
            .iter()
            .map(|(name, value)| (to_os_string(name), to_os_string(value))),
    );

    let inherited_fds: Vec<ShellFd> = command.open_files.iter_fds().map(|(fd, _)| fd).collect();

    // A closed standard stream is closed in the process, as bash does (after any
    // descriptors are mapped, so that a saved copy, as in `3>&1 1>&-`, remains).
    let mut open_files = command.open_files;
    let closed = [
        OpenFiles::STDIN_FD,
        OpenFiles::STDOUT_FD,
        OpenFiles::STDERR_FD,
    ]
    .map(|fd| !open_files.contains_fd(fd));
    cmd.stdin(take_stdio(&mut open_files, OpenFiles::STDIN_FD)?);
    cmd.stdout(take_stdio(&mut open_files, OpenFiles::STDOUT_FD)?);
    cmd.stderr(take_stdio(&mut open_files, OpenFiles::STDERR_FD)?);

    cmd.inject_fds(open_files.iter_fds().map(|(fd, file)| (fd, file.clone())))
        .map_err(std::io::Error::other)?;

    if closed.iter().any(|closed| *closed) {
        #[cfg(unix)]
        {
            // SAFETY: Only close(2) and errno inspection run after fork.
            unsafe {
                cmd.pre_exec(move || {
                    for (fd, closed) in (0..=2).zip(closed) {
                        if closed && libc::close(fd) != 0 {
                            let err = std::io::Error::last_os_error();
                            if err.raw_os_error() != Some(libc::EBADF) {
                                return Err(err);
                            }
                        }
                    }
                    Ok(())
                });
            }
        }
    }

    match command.process_group {
        ProcessGroup::Inherit => {}
        ProcessGroup::Join(pgid) => {
            cmd.process_group(pgid);
        }
        ProcessGroup::New { take_foreground } => {
            cmd.process_group(0);
            if take_foreground {
                cmd.take_foreground();
            }
        }
        // setsid() makes a new process group too.
        ProcessGroup::NewSession => cmd.lead_session(),
    }

    #[cfg(unix)]
    let blocked = if forking {
        let blocked = crate::sys::signal::BlockedForFork::new();
        let previous = blocked.previous();
        // SAFETY: only sigaction and pthread_sigmask run after fork.
        unsafe {
            cmd.pre_exec(move || {
                crate::sys::signal::restore_defaults_after_fork(&previous);
                Ok(())
            });
        }
        Some(blocked)
    } else {
        None
    };
    #[cfg(not(unix))]
    let blocked = {
        let _ = forking;
    };

    // Run ENOEXEC files with this shell rather than the platform's /bin/sh.
    // This must follow every other pre-exec step.
    #[cfg(unix)]
    commands::exec_with_script_fallback(
        &mut cmd,
        &to_os_string(&command.argv0),
        command.program.as_os_str(),
        inherited_fds.into_iter(),
        &command.binary_diagnostic,
    )?;
    #[cfg(not(unix))]
    let _ = inherited_fds;

    Ok((cmd, blocked))
}

/// Takes the file for standard stream `fd` out of `open_files`, for the process
/// to have as that stream: the null device if it's closed.
fn take_stdio(open_files: &mut OpenFiles, fd: ShellFd) -> std::io::Result<Stdio> {
    match open_files.remove_fd(fd) {
        Some(file) => commands::stdio_for(file),
        None => Ok(Stdio::null()),
    }
}
