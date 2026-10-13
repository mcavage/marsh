use std::io::{Read, Write};

use clap::Parser;

use brush_core::{ErrorKind, ExecutionExitCode, ExecutionResult, builtins, env, variables};

/// Read lines from standard input into an indexed array variable.
#[derive(Parser)]
pub(crate) struct MapFileCommand {
    /// Delimiter to use (defaults to newline).
    #[arg(short = 'd')]
    delimiter: Option<String>,

    /// Maximum number of entries to read (0 means no limit).
    #[arg(short = 'n', default_value_t = 0)]
    max_count: i64,

    /// Index into array at which to start assignment.
    #[arg(short = 'O', allow_hyphen_values = true)]
    origin: Option<i64>,

    /// Number of initial entries to skip.
    #[arg(short = 's', default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
    skip_count: i64,

    /// Whether or not to remove the delimiter from each read line.
    #[arg(short = 't')]
    remove_delimiter: bool,

    /// File descriptor to read from (defaults to stdin).
    #[arg(short = 'u', default_value_t = 0)]
    fd: brush_core::ShellFd,

    /// Name of function to call for each group of lines.
    #[arg(short = 'C')]
    callback: Option<String>,

    /// Number of lines to pass the callback for each group.
    #[arg(short = 'c', default_value_t = 5000, value_parser = clap::value_parser!(i64).range(1..))]
    callback_group_size: i64,

    /// Name of array to read into.
    #[arg(default_value = "MAPFILE")]
    array_var_name: String,
}

impl builtins::Command for MapFileCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if let Some(origin) = self.origin {
            if origin < 0 {
                writeln!(
                    context.stderr(),
                    "{}: {origin}: invalid array origin",
                    context.command_name
                )?;
                return Ok(ExecutionExitCode::GeneralError.into());
            }
        }

        if let Some((_, var)) = context.shell.env().get(&self.array_var_name) {
            if var.value().is_associative_array() {
                writeln!(
                    context.stderr(),
                    "{}: {}: not an indexed array",
                    context.command_name,
                    self.array_var_name
                )?;
                return Ok(ExecutionExitCode::GeneralError.into());
            }
        }

        let mut input_file = context
            .try_fd(self.fd)
            .ok_or_else(|| ErrorKind::BadFileDescriptor(self.fd))?;

        // Without -O the array starts empty.
        if self.origin.is_none() {
            context.shell.env_mut().update_or_add(
                &self.array_var_name,
                variables::ShellValueLiteral::Array(variables::ArrayLiteral(vec![])),
                |_| Ok(()),
                env::EnvironmentLookup::Anywhere,
                env::EnvironmentScope::Global,
            )?;
        }

        let _term_mode = setup_terminal_settings(&input_file)?;
        let delimiter = match &self.delimiter {
            Some(d) if d.is_empty() => b'\0',
            Some(d) => d.as_bytes().first().copied().unwrap_or(b'\n'),
            None => b'\n',
        };
        let max_count: usize = self.max_count.try_into()?;
        for _ in 0..self.skip_count {
            if read_line(&mut input_file, delimiter)?.is_none() {
                break;
            }
        }

        let mut index = self.origin.unwrap_or(0);
        let mut count = 0;
        while max_count == 0 || count < max_count {
            let Some(mut line) = read_line(&mut input_file, delimiter)? else {
                break;
            };
            count += 1;
            if self.remove_delimiter && line.ends_with(&[delimiter]) {
                line.pop();
            }
            // A shell value ends at its first NUL, as in Bash.
            if let Some(end) = line.iter().position(|byte| *byte == 0) {
                line.truncate(end);
            }
            let line = brush_core::bytes::decode_vec(line);

            // As in Bash, every `-c` lines the callback runs with the index and
            // the line about to be stored; its status is ignored. The input is
            // read a byte at a time, so the callback may read from it too.
            if let Some(callback) = &self.callback
                && i64::try_from(count).is_ok_and(|count| count % self.callback_group_size == 0)
            {
                let command = std::format!(
                    "{callback} {index} {}",
                    brush_core::escape::force_quote(
                        &line,
                        brush_core::escape::QuoteMode::SingleQuote
                    )
                );
                let source_info = context.shell.call_stack().current_pos_as_source_info();
                context
                    .shell
                    .run_string(command, &source_info, &context.params)
                    .await?;
            }

            context.shell.env_mut().update_or_add_array_element(
                &self.array_var_name,
                index.to_string(),
                line,
                |_| Ok(()),
                env::EnvironmentLookup::Anywhere,
                env::EnvironmentScope::Global,
            )?;
            index += 1;
        }

        Ok(ExecutionResult::success())
    }
}

/// Reads one line, delimiter included; `None` at the end of the input.
fn read_line(
    input_file: &mut brush_core::openfiles::OpenFile,
    delimiter: u8,
) -> Result<Option<Vec<u8>>, brush_core::Error> {
    let mut line = vec![];
    let mut buf = [0u8; 1];
    loop {
        match input_file.read(&mut buf)? {
            0 if line.is_empty() => return Ok(None),
            0 => return Ok(Some(line)),
            _ => {
                line.push(buf[0]);
                if buf[0] == delimiter {
                    return Ok(Some(line));
                }
            }
        }
    }
}

fn setup_terminal_settings(
    file: &brush_core::openfiles::OpenFile,
) -> Result<Option<brush_core::terminal::AutoModeGuard>, brush_core::Error> {
    let mode = brush_core::terminal::AutoModeGuard::new(file.to_owned()).ok();
    if let Some(mode) = &mode {
        let config = brush_core::terminal::Settings::builder()
            .line_input(false)
            .interrupt_signals(false)
            .build();

        mode.apply_settings(&config)?;
    }

    Ok(mode)
}
