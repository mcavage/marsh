use clap::Parser;
use std::{borrow::Cow, io::Write as _, os::unix::process::CommandExt};

use brush_core::{ExecutionControlFlow, ExecutionExitCode, ExecutionResult, builtins, commands};

/// Exec the provided command.
#[derive(Parser)]
pub(crate) struct ExecCommand {
    /// Pass given name as zeroth argument to command.
    #[arg(short = 'a', value_name = "NAME")]
    name_for_argv0: Option<String>,

    /// Exec command with an empty environment.
    #[arg(short = 'c')]
    empty_environment: bool,

    /// Exec command as a login shell.
    #[arg(short = 'l')]
    exec_as_login: bool,

    /// Command and args.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for ExecCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if self.args.is_empty() {
            // When no arguments are present, then there's nothing for us to execute -- but we need
            // to ensure that any redirections setup for this builtin get applied to the calling
            // shell instance.
            #[allow(clippy::needless_collect)]
            let fds: Vec<_> = context.iter_fds().collect();

            context.shell.replace_open_files(fds.into_iter());
            return Ok(ExecutionResult::success());
        }

        // If we know we're already running in a subshell, then `exec`ing is actually
        // unsafe, since it would also replace the *parent* shell instance. We instead
        // delegate to the `command` builtin to perform the execution, with an expectation
        // of returning.
        if context.shell.is_subshell() && !context.shell.owns_process() {
            if self.empty_environment || self.exec_as_login || self.name_for_argv0.is_some() {
                return brush_core::error::unimp("exec with options in subshell not yet supported");
            }

            let cmd_cmd = crate::command::CommandCommand {
                command_and_args: self.args.clone(),
                ..Default::default()
            };

            return cmd_cmd.execute(context).await;
        }

        let mut argv0 = Cow::Borrowed(self.name_for_argv0.as_ref().unwrap_or(&self.args[0]));

        if self.exec_as_login {
            argv0 = Cow::Owned(std::format!("-{argv0}"));
        }

        // Resolve PATH here so the shell (not the platform launcher) decides
        // how a found file is run, including ENOEXEC files.
        let name = &self.args[0];
        let path = if name.contains('/') {
            Some(std::path::PathBuf::from(name))
        } else {
            context.shell.find_first_executable_in_path(name)
        };

        let error = if let Some(path) = path {
            let path = path.to_string_lossy().into_owned();
            let mut cmd = commands::compose_std_command(
                &context,
                &path,
                argv0.as_str(),
                &self.args[1..],
                self.empty_environment,
            )?;
            #[cfg(unix)]
            brush_core::sys::commands::exec_with_script_fallback(
                &mut cmd,
                argv0.as_ref().as_ref(),
                path.as_ref(),
                context.iter_fds().map(|(fd, _)| fd),
            )?;

            // A failed exec must leave the shell's own descriptors in place.
            let backup = brush_core::sys::commands::DescriptorBackup::save(
                context.iter_fds().map(|(fd, _)| fd),
            )?;
            let error = cmd.exec();
            backup.restore();
            let missing = error.kind() == std::io::ErrorKind::NotFound
                && !context.shell.absolute_path(&path).exists();
            (error.to_string(), missing)
        } else {
            ("not found".to_owned(), true)
        };

        let (message, missing) = error;
        let _ = writeln!(context.stderr(), "exec: {name}: {message}");
        let mut result: ExecutionResult = if missing {
            ExecutionExitCode::NotFound.into()
        } else {
            ExecutionExitCode::CannotExecute.into()
        };
        // Like Bash, a noninteractive shell exits unless `execfail` is set.
        if !context.shell.options().exit_on_exec_fail && !context.shell.options().interactive {
            result.next_control_flow = ExecutionControlFlow::ExitShell;
        }
        Ok(result)
    }
}
