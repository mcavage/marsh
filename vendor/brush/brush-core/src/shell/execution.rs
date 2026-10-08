//! Execution support for shell.

use std::{io::Read, path::Path};

use crate::{
    ExecutionControlFlow, ExecutionParameters, ExecutionResult, ProcessGroupPolicy, SourceInfo,
    arithmetic::Evaluatable as _, callstack, error, interp::Execute as _, openfiles,
    trace_categories,
};

impl<SE: crate::extensions::ShellExtensions> crate::Shell<SE> {
    /// Returns the default execution parameters for this shell.
    pub fn default_exec_params(&self) -> ExecutionParameters {
        let mut params = ExecutionParameters::default();

        params.process_group_policy = if self.options.enable_job_control {
            ProcessGroupPolicy::NewProcessGroup
        } else {
            ProcessGroupPolicy::SameProcessGroup
        };

        params
    }

    pub(super) async fn source_if_exists(
        &mut self,
        path: impl AsRef<Path>,
        params: &ExecutionParameters,
    ) -> Result<bool, error::Error> {
        let path = path.as_ref();
        if path.exists() {
            self.source_script(path, std::iter::empty::<String>(), params)
                .await?;
            Ok(true)
        } else {
            tracing::debug!("skipping non-existent file: {}", path.display());
            Ok(false)
        }
    }

    /// Source the given file as a shell script, returning the execution result.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file to source.
    /// * `args` - The arguments to pass to the script as positional parameters.
    /// * `params` - Execution parameters.
    pub async fn source_script<S: Into<String>, P: AsRef<Path>, I: Iterator<Item = S>>(
        &mut self,
        path: P,
        args: I,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        self.parse_and_execute_script_file(
            path.as_ref(),
            args,
            params,
            callstack::ScriptCallType::Source,
        )
        .await
    }

    /// Parse and execute the given file as a shell script, returning the execution result.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file to source.
    /// * `args` - The arguments to pass to the script as positional parameters.
    /// * `params` - Execution parameters.
    /// * `call_type` - The type of script call being made.
    async fn parse_and_execute_script_file<
        S: Into<String>,
        P: AsRef<Path>,
        I: Iterator<Item = S>,
    >(
        &mut self,
        path: P,
        args: I,
        params: &ExecutionParameters,
        call_type: callstack::ScriptCallType,
    ) -> Result<ExecutionResult, error::Error> {
        let path = path.as_ref();
        tracing::debug!("sourcing: {}", path.display());

        let mut options = std::fs::File::options();
        options.read(true);

        let mut opened = self.open_file(&options, path, params);
        // Like Bash, a script operand without a slash that is absent from the
        // working directory is read from the first regular file on PATH; $0
        // keeps the operand as given. Execute permission is not required.
        if matches!(call_type, callstack::ScriptCallType::Run)
            && path.components().count() == 1
            && opened
                .as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            let search = self.env_str("PATH").unwrap_or_default().into_owned();
            if let Some(found) = std::env::split_paths(&search)
                .map(|directory| directory.join(path))
                .find(|candidate| candidate.is_file())
            {
                opened = self.open_file(&options, &found, params);
            }
        }
        let opened_file: openfiles::OpenFile =
            opened.map_err(|e| error::ErrorKind::FailedSourcingFile(path.to_owned(), e))?;

        if opened_file.is_dir() {
            return Err(error::ErrorKind::FailedSourcingFile(
                path.to_owned(),
                std::io::Error::from(std::io::ErrorKind::IsADirectory),
            )
            .into());
        }

        let source_info = crate::SourceInfo::from(path.to_owned());

        let mut result = self
            .source_file(opened_file, &source_info, args, params, call_type)
            .await?;

        // Handle control flow at script execution boundary. If execution completed
        // with a `return`, we need to clear it since it's already been "used". All
        // other control flow types are preserved.
        if matches!(
            result.next_control_flow,
            ExecutionControlFlow::ReturnFromFunctionOrScript
        ) {
            result.next_control_flow = ExecutionControlFlow::Normal;
        }

        Ok(result)
    }

    /// Source the given file as a shell script, returning the execution result.
    ///
    /// # Arguments
    ///
    /// * `file` - The file to source.
    /// * `source_info` - Information about the source of the script.
    /// * `args` - The arguments to pass to the script as positional parameters.
    /// * `params` - Execution parameters.
    /// * `call_type` - The type of script call being made.
    async fn source_file<F: Read, S: Into<String>, I: Iterator<Item = S>>(
        &mut self,
        file: F,
        source_info: &crate::SourceInfo,
        args: I,
        params: &ExecutionParameters,
        call_type: callstack::ScriptCallType,
    ) -> Result<ExecutionResult, error::Error> {
        let mut file = file;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        tracing::debug!(target: trace_categories::PARSE, "Parsing sourced file: {}", source_info.source);
        let parse_result = self.parse(bytes.as_slice());

        let script_positional_args = args.map(Into::into);

        self.call_stack
            .push_script(call_type, source_info, script_positional_args);

        let result = match String::from_utf8(bytes) {
            Ok(text) => {
                self.run_source_text(&text, parse_result, source_info, params)
                    .await
            }
            Err(_) => {
                self.run_parsed_result(parse_result, source_info, params)
                    .await
            }
        };

        self.call_stack.pop();

        result
    }

    /// Executes the given string as a shell program, returning the resulting exit status.
    ///
    /// # Arguments
    ///
    /// * `command` - The command to execute.
    /// * `source_info` - Information about the source of the command text.
    /// * `params` - Execution parameters.
    pub async fn run_string<S: Into<String>>(
        &mut self,
        command: S,
        source_info: &crate::SourceInfo,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        let command: String = command.into();
        let parse_result = self.parse_string(command.as_str());
        self.run_source_text(&command, parse_result, source_info, params)
            .await
    }

    /// Runs shell source text as Bash reads it. When `text` parses as a whole
    /// (`parsed`), it runs as one program. Otherwise each read unit (a line,
    /// extended until its commands are complete) runs before the next is
    /// parsed, so commands that precede a lexical error still execute.
    async fn run_source_text(
        &mut self,
        text: &str,
        parsed: Result<brush_parser::ast::Program, brush_parser::ParseError>,
        source_info: &crate::SourceInfo,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        if let Ok(program) = parsed {
            return self
                .run_program_reading_aliases(text, program, source_info, params)
                .await;
        }

        let mut result = ExecutionResult::success();
        let mut unit = String::new();
        let mut lines = text.split_inclusive('\n').enumerate().peekable();
        while let Some((line_index, line)) = lines.next() {
            if unit.is_empty() {
                // Leading newlines keep reported line numbers relative to `text`.
                unit = "\n".repeat(line_index);
            }
            unit.push_str(line);
            let parsed = self.parse(unit.as_bytes());
            if lines.peek().is_some() && !is_complete_read_unit(&unit, &parsed) {
                continue;
            }
            unit.clear();
            let failed = parsed.is_err();
            result = self.run_parsed_result(parsed, source_info, params).await?;
            if failed || !result.is_normal_flow() {
                break;
            }
        }
        Ok(result)
    }

    /// Executes the given command, provided to a shell executable on the command
    /// line (i.e., via `-c`).
    ///
    /// It is expected that the shell will not be used for any further execution
    /// after this command; this function will perform any necessary shell exit
    /// handling.
    ///
    /// # Arguments
    ///
    /// * `command` - The command to execute.
    pub async fn run_dash_c_command<S: Into<String>>(
        &mut self,
        command: S,
    ) -> Result<ExecutionResult, error::Error> {
        self.start_command_string_mode();

        // Execute the command string.
        let params = self.default_exec_params();
        let source_info = SourceInfo::from("-c");
        let result = self.run_string(command, &source_info, &params).await?;

        self.end_command_string_mode()?;

        // Give the shell a chance to run on-exit tasks, but ignore the result.
        let _ = self.on_exit().await;

        Ok(result)
    }

    /// Executes the given script file, returning the resulting exit status.
    ///
    /// It is expected that the shell will not be used for any further execution
    /// after this command; this function will perform any necessary shell exit
    /// handling.
    ///
    /// # Arguments
    ///
    /// * `script_path` - The path to the script file to execute.
    /// * `args` - The arguments to pass to the script as positional parameters.
    pub async fn run_script<S: Into<String>, P: AsRef<Path>, I: Iterator<Item = S>>(
        &mut self,
        script_path: P,
        args: I,
    ) -> Result<ExecutionResult, error::Error> {
        let params = self.default_exec_params();
        let result = self
            .parse_and_execute_script_file(
                script_path.as_ref(),
                args,
                &params,
                callstack::ScriptCallType::Run,
            )
            .await?;

        // Give the shell a chance to run on-exit tasks, but ignore the result.
        let _ = self.on_exit().await;

        Ok(result)
    }

    pub(crate) async fn run_parsed_result(
        &mut self,
        parse_result: Result<brush_parser::ast::Program, brush_parser::ParseError>,
        source_info: &crate::SourceInfo,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        // If parsing succeeded, run the program. If there's a parse error, it's fatal (per spec).
        let result = match parse_result {
            Ok(prog) => self.run_program(prog, params).await,
            Err(parse_err) => Err(error::Error::from(error::ErrorKind::ParseError(
                parse_err,
                source_info.clone(),
            ))
            .into_fatal()),
        };

        // Report any errors.
        match result {
            Ok(result) => Ok(result),
            Err(err) => {
                let _ = self.display_error(&mut params.stderr(self), &err);

                let result = err.into_result(self);
                self.set_last_exit_status(result.exit_code.into());

                Ok(result)
            }
        }
    }

    /// Runs a program parsed from `text` one complete command at a time. If a
    /// command changes the aliases that parsing depends on, the commands on
    /// later lines are parsed again, as Bash would only read them now.
    async fn run_program_reading_aliases(
        &mut self,
        text: &str,
        program: brush_parser::ast::Program,
        source_info: &crate::SourceInfo,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        use brush_parser::ast::SourceLocation as _;

        let alias_state = self.alias_parse_state();
        let mut result = ExecutionResult::success();
        let mut commands = program.complete_commands.iter().peekable();
        while let Some(command) = commands.next() {
            result = crate::interp::execute_complete_command(self, command, params).await;
            if !result.is_normal_flow() {
                break;
            }
            if self.alias_parse_state() == alias_state {
                continue;
            }
            let Some(next_line) = commands
                .peek()
                .and_then(|next| next.location())
                .map(|location| location.start.line)
            else {
                continue;
            };
            // Leading newlines keep reported line numbers relative to `text`.
            let mut rest = "\n".repeat(next_line.saturating_sub(1));
            rest.extend(text.split_inclusive('\n').skip(next_line.saturating_sub(1)));
            let parsed = self.parse(rest.as_bytes());
            return Box::pin(self.run_source_text(&rest, parsed, source_info, params)).await;
        }
        Ok(result)
    }

    /// Executes the given parsed shell program, returning the resulting exit status.
    ///
    /// # Arguments
    ///
    /// * `program` - The program to execute.
    /// * `params` - Execution parameters.
    pub async fn run_program(
        &mut self,
        program: brush_parser::ast::Program,
        params: &ExecutionParameters,
    ) -> Result<ExecutionResult, error::Error> {
        program.execute(self, params).await
    }

    /// Evaluate the given arithmetic expression, returning the result.
    pub fn eval_arithmetic(
        &mut self,
        expr: &brush_parser::ast::ArithmeticExpr,
    ) -> Result<i64, error::Error> {
        Ok(expr.eval(self)?)
    }
}

/// Whether `unit` needs no further lines: it parsed (without a trailing line
/// continuation) or has an error that more input cannot repair.
fn is_complete_read_unit(
    unit: &str,
    parsed: &Result<brush_parser::ast::Program, brush_parser::ParseError>,
) -> bool {
    match parsed {
        Ok(_) => {
            let line = unit.strip_suffix('\n').unwrap_or(unit);
            (line.len() - line.trim_end_matches('\\').len()).is_multiple_of(2)
        }
        Err(brush_parser::ParseError::Tokenizing { inner, .. }) => !inner.is_incomplete(),
        Err(brush_parser::ParseError::ParsingAtEndOfInput(_)) => false,
        Err(brush_parser::ParseError::ParsingNear { .. }) => true,
    }
}
