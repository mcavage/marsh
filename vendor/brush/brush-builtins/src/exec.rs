use clap::Parser;
use std::{borrow::Cow, io::Write as _};

use brush_core::{
    ExecutionControlFlow, ExecutionExitCode, ExecutionResult, builtins, commands,
    extensions::ProcessLauncher as _,
};

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

brush_clap_adapter::impl_parse_args!(ExecCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for ExecCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
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

        // Resolve the program as running it as a command would.
        let name = &self.args[0];
        let (message, missing) = if let Some(program) = context.shell.resolve_external_command(name)
        {
            // Bash execs the file by its full path, which is also what an ENOEXEC
            // script sees as `$0` and what diagnostics name.
            let program = brush_core::bytes::shell_path(&program).into_owned();
            let full_path = if program.is_absolute() {
                program.clone()
            } else {
                context
                    .shell
                    .working_dir()
                    .join(program.strip_prefix("./").unwrap_or(&program))
            };
            let request = commands::compose_launch_request(
                &context,
                &full_path,
                argv0.as_str(),
                &self.args[1..],
                if self.empty_environment {
                    commands::ProgramEnvironment::Empty
                } else {
                    commands::ProgramEnvironment::Exported
                },
            );

            // A failed exec must leave the shell's own descriptors in place.
            #[cfg(unix)]
            let backup = brush_core::sys::commands::DescriptorBackup::save(
                context.iter_fds().map(|(fd, _)| fd),
            )?;
            brush_core::traps::ignore_trap_ignored_signals_for_exec();
            let error = context.shell.process_launcher().exec(request);
            #[cfg(unix)]
            backup.restore();

            let missing = error.kind() == std::io::ErrorKind::NotFound
                && !context.shell.absolute_path(&program).exists();
            (error.to_string(), missing)
        } else {
            ("not found".to_owned(), true)
        };

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
