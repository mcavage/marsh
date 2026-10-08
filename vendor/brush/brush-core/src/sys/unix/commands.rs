//! Command execution utilities.

pub use std::os::unix::process::CommandExt;
pub use std::os::unix::process::ExitStatusExt;

use command_fds::{CommandFdExt, FdMapping};

use crate::ShellFd;
use crate::error;
use crate::openfiles;

/// Extension trait for injecting file descriptors into commands.
pub trait CommandFdInjectionExt {
    /// Injects the given open files as file descriptors into the command.
    ///
    /// # Arguments
    ///
    /// * `open_files` - A mapping of child file descriptors to open files.
    fn inject_fds(
        &mut self,
        open_files: impl Iterator<Item = (ShellFd, openfiles::OpenFile)>,
    ) -> Result<(), error::Error>;
}

impl CommandFdInjectionExt for std::process::Command {
    fn inject_fds(
        &mut self,
        open_files: impl Iterator<Item = (ShellFd, openfiles::OpenFile)>,
    ) -> Result<(), error::Error> {
        let fd_mappings: Vec<FdMapping> = open_files
            .map(|(child_fd, open_file)| -> Result<FdMapping, error::Error> {
                let parent_fd = open_file.try_clone_to_owned()?;
                Ok(FdMapping {
                    child_fd,
                    parent_fd,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.fd_mappings(fd_mappings)
            .map_err(|_e| error::ErrorKind::ChildCreationFailure)?;

        Ok(())
    }
}

static SCRIPT_INTERPRETER: std::sync::OnceLock<(std::ffi::CString, Vec<std::ffi::CString>)> =
    std::sync::OnceLock::new();

/// Selects the shell executable (and leading arguments) for ENOEXEC files.
///
/// Bash runs such files with itself. Without an interpreter, the platform
/// launcher's own fallback (usually `/bin/sh`) applies.
pub fn set_script_interpreter(
    path: &std::path::Path,
    prefix_args: &[std::ffi::OsString],
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let cstring =
        |s: &std::ffi::OsStr| std::ffi::CString::new(s.as_bytes()).map_err(std::io::Error::other);
    let interpreter = (
        cstring(path.as_os_str())?,
        prefix_args
            .iter()
            .map(|arg| cstring(arg))
            .collect::<std::io::Result<_>>()?,
    );
    let _ = SCRIPT_INTERPRETER.set(interpreter);
    Ok(())
}

/// Runs ENOEXEC files with the configured script interpreter.
///
/// The final pre-exec step becomes an `execve` of the command's own program,
/// so ENOEXEC reaches the shell instead of the platform's `/bin/sh` retry. On
/// ENOEXEC the interpreter is exec'd on `script` with the same arguments,
/// descriptors, and environment. Returns false (leaving the command
/// untouched) when no interpreter is configured.
///
/// Must be called after every other pre-exec setup and the environment are in
/// place. The environment is taken from the command's explicit variables, so
/// it requires the caller to have cleared the inherited environment.
pub fn exec_with_script_fallback(
    command: &mut std::process::Command,
    argv0: &std::ffi::OsStr,
    script: &std::ffi::OsStr,
    inherited_fds: impl Iterator<Item = crate::ShellFd>,
) -> std::io::Result<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;
    let Some((interpreter, prefix)) = SCRIPT_INTERPRETER.get() else {
        return Ok(false);
    };
    let cstring = |s: &std::ffi::OsStr| CString::new(s.as_bytes()).map_err(std::io::Error::other);
    let program = cstring(command.get_program())?;
    let args = command
        .get_args()
        .map(cstring)
        .collect::<std::io::Result<Vec<_>>>()?;
    let env = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                let mut entry = key.as_bytes().to_vec();
                entry.push(b'=');
                entry.extend_from_slice(value.as_bytes());
                CString::new(entry).map_err(std::io::Error::other)
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    // The interpreter only uses descriptors it is told about, and keeps closed
    // standard descriptors closed (the Rust runtime reopens them on /dev/null).
    let inherited_fds: Vec<_> = inherited_fds.collect();
    let inherited = inherited_fds
        .iter()
        .filter(|fd| **fd > 2)
        .map(|fd| format!("--inherit-fd={fd}"))
        .chain(
            (0..=2)
                .filter(|fd| !inherited_fds.contains(fd))
                .map(|fd| format!("--closed-fd={fd}")),
        )
        .map(|arg| CString::new(arg).map_err(std::io::Error::other))
        .collect::<std::io::Result<Vec<_>>>()?;
    let argv0 = cstring(argv0)?;
    let script = cstring(script)?;
    let pointers = |items: Vec<&CString>| {
        items
            .into_iter()
            // Addresses are stored as integers so the closure is Send + Sync.
            .map(|item| item.as_ptr() as usize)
            .chain(std::iter::once(0))
            .collect::<Vec<_>>()
    };
    let argv = pointers(std::iter::once(&argv0).chain(&args).collect());
    let fallback_argv = pointers(
        std::iter::once(interpreter)
            .chain(prefix)
            .chain(&inherited)
            .chain(std::iter::once(&script))
            .chain(&args)
            .collect(),
    );
    let envp = pointers(env.iter().collect());
    // Everything the child touches is allocated here; the closure only makes
    // async-signal-safe calls on immutable, null-terminated pointer arrays.
    let storage = (program, args, env, argv0, script, inherited);
    let pointers = (argv, fallback_argv, envp);
    let exec = move || {
        let (program, ..) = &storage;
        let (argv, fallback_argv, envp) = &pointers;
        // SAFETY: see above. The CStrings backing every pointer move into this
        // closure together with the pointer arrays and live as long as it does.
        unsafe { libc::execve(program.as_ptr(), argv.as_ptr().cast(), envp.as_ptr().cast()) };
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOEXEC) {
            // SAFETY: as above; the interpreter path is a static CString.
            unsafe {
                libc::execve(
                    interpreter.as_ptr(),
                    fallback_argv.as_ptr().cast(),
                    envp.as_ptr().cast(),
                )
            };
        }
        Err(error)
    };
    // SAFETY: the closure only calls execve and reads errno after fork.
    unsafe { command.pre_exec(exec) };
    Ok(true)
}

/// Saved copies of descriptors that an in-place exec rearranges, so a failed
/// exec can put the shell's own descriptors back.
pub struct DescriptorBackup(Vec<(i32, Option<(std::os::fd::OwnedFd, i32)>)>);

impl DescriptorBackup {
    /// Saves descriptors `targets` (and the standard three) above all of them.
    pub fn save(targets: impl IntoIterator<Item = i32>) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd as _;
        let mut targets: Vec<i32> = (0..=2).chain(targets).collect();
        targets.sort_unstable();
        targets.dedup();
        let above = targets.last().copied().unwrap_or(2) + 1;
        let mut saved = Vec::with_capacity(targets.len());
        for target in targets {
            // SAFETY: fcntl only inspects or duplicates this descriptor number.
            let flags = unsafe { libc::fcntl(target, libc::F_GETFD) };
            if flags == -1 {
                saved.push((target, None));
                continue;
            }
            // SAFETY: as above; a successful F_DUPFD_CLOEXEC returns a new fd we own.
            let copy = unsafe { libc::fcntl(target, libc::F_DUPFD_CLOEXEC, above) };
            if copy == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `copy` is a freshly duplicated descriptor owned by nobody else.
            saved.push((
                target,
                Some((unsafe { std::os::fd::OwnedFd::from_raw_fd(copy) }, flags)),
            ));
        }
        Ok(Self(saved))
    }

    /// Restores every saved descriptor, closing those that were absent.
    pub fn restore(self) {
        use std::os::fd::AsRawFd as _;
        for (target, saved) in self.0 {
            match saved {
                Some((copy, flags)) => {
                    // SAFETY: restores the recorded descriptor number from our own copy.
                    unsafe { libc::dup2(copy.as_raw_fd(), target) };
                    // SAFETY: restores that descriptor's recorded flags.
                    unsafe { libc::fcntl(target, libc::F_SETFD, flags) };
                }
                None => {
                    // SAFETY: closes a number that was absent before the exec.
                    unsafe { libc::close(target) };
                }
            }
        }
    }
}

/// Extension trait for arranging for commands to take the foreground.
pub trait CommandFgControlExt {
    /// Arranges for the command to take the foreground when it is executed.
    fn take_foreground(&mut self);
    /// Arranges for the command to become a session leader when it is executed.
    fn lead_session(&mut self);
}

impl CommandFgControlExt for std::process::Command {
    fn take_foreground(&mut self) {
        // SAFETY:
        // This arranges for a provided function to run in the context of
        // the forked process before it exec's the target command. In general,
        // rust can't guarantee safety of code running in such a context.
        unsafe {
            self.pre_exec(pre_exec_take_foreground);
        }
    }

    fn lead_session(&mut self) {
        // SAFETY:
        // This arranges for a provided function to run in the context of
        // the forked process before it exec's the target command. In general,
        // rust can't guarantee safety of code running in such a context.
        unsafe {
            self.pre_exec(pre_exec_lead_session);
        }
    }
}

fn pre_exec_take_foreground() -> Result<(), std::io::Error> {
    use crate::sys;

    sys::terminal::move_self_to_foreground()?;
    Ok(())
}

fn pre_exec_lead_session() -> Result<(), std::io::Error> {
    if let Err(e) = nix::unistd::setsid() {
        return Err(std::io::Error::other(format!(
            "failed to become session leader: {e}"
        )));
    }

    #[cfg(not(target_os = "macos"))]
    let control = libc::TIOCSCTTY;
    #[cfg(target_os = "macos")]
    let control: u64 = libc::TIOCSCTTY.into();

    // SAFETY:
    // This is calling a libc function to set the controlling terminal.
    let result = unsafe { libc::ioctl(0, control, 0) };
    if result != 0 {
        return Err(std::io::Error::other("failed to set controlling terminal"));
    }

    Ok(())
}
