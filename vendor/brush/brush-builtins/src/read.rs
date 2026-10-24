use clap::Parser;
use itertools::Itertools;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use brush_core::{ErrorKind, builtins, env, error, variables};

use std::io::{Read, Write};

/// Exit code returned when `read` times out.
/// This is 128 + SIGALRM (14) = 142, matching bash behavior.
const TIMEOUT_EXIT_CODE: u8 = 142;

/// ASCII control character for Ctrl+C (ETX - End of Text).
const CTRL_C: char = '\x03';
/// ASCII control character for Ctrl+D (EOT - End of Transmission).
const CTRL_D: char = '\x04';
/// Backslash character used for escape processing.
const BACKSLASH: char = '\\';
/// Default line delimiter (newline).
const DEFAULT_DELIMITER: char = '\n';
/// NUL character used as delimiter when `-d ''` is specified.
const NUL_DELIMITER: char = '\0';

/// Parse standard input.
#[derive(Parser)]
pub(crate) struct ReadCommand {
    /// Optionally, name of an array variable to receive read words
    /// of input.
    #[clap(short = 'a', value_name = "VAR_NAME")]
    array_variable: Option<String>,

    /// Optionally, a delimiter to use other than a newline character.
    #[clap(short = 'd')]
    delimiter: Option<String>,

    /// Use readline-like input.
    #[clap(short = 'e')]
    use_readline: bool,

    /// Provide text to use as initial input for readline.
    #[clap(short = 'i', value_name = "STR")]
    initial_text: Option<String>,

    /// Read only the first N characters or until a specified
    /// delimiter is reached, whichever happens first.
    #[clap(short = 'n', value_name = "COUNT")]
    return_after_n_chars: Option<usize>,

    /// Read exactly N characters, ignoring any specified delimiter.
    #[clap(short = 'N', value_name = "COUNT")]
    return_after_n_chars_no_delimiter: Option<usize>,

    /// Prompt to display before reading.
    #[clap(short = 'p')]
    prompt: Option<String>,

    /// Read input in raw mode; no escape sequences.
    #[clap(short = 'r')]
    raw_mode: bool,

    /// Do not echo input.
    #[clap(short = 's')]
    silent: bool,

    /// Specify timeout in seconds; fail if the timeout elapses before
    /// input is completed.
    #[clap(short = 't', value_name = "SECONDS", allow_hyphen_values = true)]
    timeout_in_seconds: Option<f64>,

    /// File descriptor to read from instead of stdin.
    #[clap(short = 'u', name = "FD")]
    fd_num_to_read: Option<brush_core::ShellFd>,

    /// Optionally, names of variables to receive read input.
    variable_names: Vec<String>,
}

brush_clap_adapter::impl_parse_args!(ReadCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for ReadCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if self.use_readline {
            return error::unimp("read -e");
        }
        if self.initial_text.is_some() {
            return error::unimp("read -i");
        }

        // Validate timeout value if provided.
        if let Some(result) = self.validate_timeout(&context)? {
            return Ok(result);
        }

        // Find the input stream to use.
        let fd_num = self.fd_num_to_read.map_or(
            brush_core::openfiles::OpenFiles::STDIN_FD,
            brush_core::ShellFd::from,
        );

        // Retrieve the file.
        let Some(input_stream) = context.try_fd(fd_num) else {
            if self.fd_num_to_read.is_none() {
                return Err(ErrorKind::BadFileDescriptor(fd_num).into());
            }
            writeln!(
                context.stderr(),
                "{}read: {fd_num}: invalid file descriptor: Bad file descriptor",
                context.shell.diagnostic_prefix()
            )?;
            return Ok(brush_core::ExecutionResult::general_error());
        };

        // Retrieve effective value of IFS for splitting.
        // We convert to owned String to release the borrow before the mutable borrow
        // needed for variable assignment.
        let ifs = context.shell.ifs().into_owned();

        // Convert timeout to Duration.
        let timeout = self.timeout_in_seconds.map(Duration::from_secs_f64);

        // Perform the read operation (potentially with timeout).
        let mut escaped = vec![];
        let read_result = self.read_line(input_stream, context.stderr(), timeout, &mut escaped)?;

        // `-t 0` only reports whether input is ready; it assigns nothing.
        match read_result {
            ReadResult::InputReady => return Ok(brush_core::ExecutionResult::success()),
            ReadResult::InputNotReady => return Ok(brush_core::ExecutionResult::general_error()),
            _ => (),
        }

        // Determine whether to skip IFS splitting (for -N option).
        let skip_ifs_splitting = self.return_after_n_chars_no_delimiter.is_some();

        // Extract the input line and determine exit code based on result.
        let (input_line, result) = match &read_result {
            ReadResult::Line(line) => (Some(line.clone()), brush_core::ExecutionResult::success()),
            ReadResult::Eof(Some(line)) => (
                Some(line.clone()),
                brush_core::ExecutionResult::general_error(),
            ),
            ReadResult::Eof(None)
            | ReadResult::Interrupted
            | ReadResult::InputNotReady
            | ReadResult::InputReady => (None, brush_core::ExecutionResult::general_error()),
            ReadResult::TimedOut(partial) => (
                partial.clone(),
                brush_core::ExecutionResult::new(TIMEOUT_EXIT_CODE),
            ),
        };
        let input_line = input_line.map(|line| mark_escaped(&line, &escaped));

        // Assign input to variables based on options.
        let readonly = assign_input_to_variables(
            context.shell,
            &context.params,
            input_line.as_deref(),
            &ifs,
            skip_ifs_splitting,
            self.array_variable.as_deref(),
            &self.variable_names,
        )
        .await?;
        if let Some(name) = readonly {
            writeln!(
                context.stderr(),
                "{}{name}: readonly variable",
                context.shell.diagnostic_prefix()
            )?;
            return Ok(brush_core::ExecutionResult::new(2));
        }

        Ok(result)
    }
}

/// Assigns read input to shell variables based on the specified options.
///
/// This handles three modes:
/// - Array mode (`-a`): Split input by IFS and assign to array elements
/// - Named variables: Split input by IFS and assign to each variable, with remainder to last
/// - Default (`REPLY`): Assign entire input line to the `REPLY` variable
async fn assign_input_to_variables(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    params: &brush_core::ExecutionParameters,
    input_line: Option<&[(char, bool)]>,
    ifs: &str,
    skip_ifs_splitting: bool,
    array_variable: Option<&str>,
    variable_names: &[String],
) -> Result<Option<String>, brush_core::Error> {
    if let Some(array_variable) = array_variable {
        let literal_fields = build_array_fields(input_line, ifs, skip_ifs_splitting);
        if !assign(
            shell,
            params,
            array_variable,
            variables::ShellValueLiteral::Array(variables::ArrayLiteral::new(literal_fields)),
        )
        .await?
        {
            return Ok(Some(array_variable.to_string()));
        }
    } else if !variable_names.is_empty() {
        return assign_to_named_variables(
            shell,
            params,
            input_line,
            ifs,
            skip_ifs_splitting,
            variable_names,
        )
        .await;
    } else {
        if !assign(
            shell,
            params,
            "REPLY",
            variables::ShellValueLiteral::Scalar(text(input_line.unwrap_or_default())),
        )
        .await?
        {
            return Ok(Some("REPLY".to_string()));
        }
    }
    Ok(None)
}

/// Assigns split fields to named variables.
///
/// Fields are assigned one per variable, with any remaining fields joined by space
/// and assigned to the last variable. If there are more variables than fields,
/// the extra variables are set to empty strings.
async fn assign_to_named_variables(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    params: &brush_core::ExecutionParameters,
    input_line: Option<&[(char, bool)]>,
    ifs: &str,
    skip_ifs_splitting: bool,
    variable_names: &[String],
) -> Result<Option<String>, brush_core::Error> {
    let mut fields =
        build_variable_fields(input_line, ifs, skip_ifs_splitting, variable_names.len());

    for (i, name) in variable_names.iter().enumerate() {
        let is_last = i == variable_names.len() - 1;

        let value = if fields.is_empty() {
            String::new()
        } else if is_last {
            // Last variable gets all remaining fields joined by space.
            std::mem::take(&mut fields).into_iter().join(" ")
        } else {
            fields.pop_front().unwrap_or_default()
        };

        if !assign(
            shell,
            params,
            name,
            variables::ShellValueLiteral::Scalar(value),
        )
        .await?
        {
            return Ok(Some(name.to_string()));
        }

        if is_last {
            break;
        }
    }
    Ok(None)
}

/// Assigns `value` to `name`, unless it is readonly.
async fn assign(
    shell: &mut brush_core::Shell<impl brush_core::ShellExtensions>,
    params: &brush_core::ExecutionParameters,
    name: &str,
    value: variables::ShellValueLiteral,
) -> Result<bool, brush_core::Error> {
    // A reference to an array element assigns it, its subscript expanded.
    if let variables::ShellValueLiteral::Scalar(text) = &value
        && shell.env().nameref_element(name).is_some()
    {
        brush_core::expansion::assign_to_named_parameter(shell, params, name, text.clone()).await?;
        return Ok(true);
    }
    if shell
        .env()
        .get(name)
        .is_some_and(|(_, variable)| variable.is_readonly())
    {
        return Ok(false);
    }
    shell.env_mut().update_or_add(
        name,
        value,
        |_| Ok(()),
        env::EnvironmentLookup::Anywhere,
        env::EnvironmentScope::Global,
    )?;
    Ok(true)
}

/// Builds array field values from input, optionally splitting by IFS.
fn build_array_fields(
    input_line: Option<&[(char, bool)]>,
    ifs: &str,
    skip_ifs_splitting: bool,
) -> Vec<(Option<String>, String)> {
    match input_line {
        Some(line) if skip_ifs_splitting => {
            // With -N, don't split - put entire input as single element.
            vec![(None, text(line))]
        }
        Some(line) => split_line(ifs, line, None)
            .into_iter()
            .map(|f| (None, f))
            .collect(),
        None => vec![],
    }
}

/// Builds field values from input for assignment to named variables.
fn build_variable_fields(
    input_line: Option<&[(char, bool)]>,
    ifs: &str,
    skip_ifs_splitting: bool,
    num_variables: usize,
) -> VecDeque<String> {
    match input_line {
        Some(line) if skip_ifs_splitting => {
            // With -N, don't split - put entire input in first variable.
            VecDeque::from([text(line)])
        }
        Some(line) => split_line(ifs, line, Some(num_variables)),
        None => VecDeque::new(),
    }
}

/// The characters of `line`, each marked if a backslash escaped it; `escaped`
/// holds the offsets, in the line's bytes, of those characters.
fn mark_escaped(line: &str, escaped: &[usize]) -> Vec<(char, bool)> {
    let mut offset = 0;
    line.chars()
        .map(|c| {
            let marked = escaped.binary_search(&offset).is_ok();
            offset += encoded_len(c);
            (c, marked)
        })
        .collect()
}

/// How many bytes `c` stands for.
fn encoded_len(c: char) -> usize {
    brush_core::bytes::raw_byte(c).map_or(c.len_utf8(), |_| 1)
}

fn text(line: &[(char, bool)]) -> String {
    line.iter().map(|(c, _)| c).collect()
}

/// Splits `line` as Bash's `read` does: each field is one word and the
/// delimiter after it (IFS whitespace around at most one other IFS character);
/// a character a backslash escaped is never a delimiter. With `names`, the last
/// of them takes the rest less its trailing IFS whitespace, or, when the rest is
/// a single word, that word without the delimiter after it (`IFS=, read a b <<<
/// 'a,b,'` sets `b` to `b`).
fn split_line(ifs: &str, line: &[(char, bool)], names: Option<usize>) -> VecDeque<String> {
    let delimiter = |at: usize| !line[at].1 && ifs.contains(line[at].0);
    let whitespace = |at: usize| delimiter(at) && matches!(line[at].0, ' ' | '\t' | '\n');
    let skip_whitespace = |mut at: usize| {
        while at < line.len() && whitespace(at) {
            at += 1;
        }
        at
    };
    // One word and the delimiter after it: (word, where the next one starts).
    let next_word = |start: usize| {
        let end = (start..line.len())
            .find(|at| delimiter(*at))
            .unwrap_or(line.len());
        let mut next = skip_whitespace(end);
        if next < line.len() && delimiter(next) && !whitespace(next) {
            next = skip_whitespace(next + 1);
        }
        (text(&line[start..end]), next)
    };

    let mut fields = VecDeque::new();
    let mut at = skip_whitespace(0);
    while names.is_none_or(|names| fields.len() + 1 < names) && at < line.len() {
        let (word, next) = next_word(at);
        fields.push_back(word);
        at = next;
    }
    if at < line.len() {
        let (word, next) = next_word(at);
        fields.push_back(if next == line.len() {
            word
        } else {
            // Bash strips trailing IFS whitespace here even if escaped.
            let mut end = line.len();
            while end > at
                && matches!(line[end - 1].0, ' ' | '\t' | '\n')
                && ifs.contains(line[end - 1].0)
            {
                end -= 1;
            }
            text(&line[at..end])
        });
    }
    fields
}

/// Result of a `read` operation.
///
/// This enum clearly represents all possible outcomes of `read_line()`,
/// making the contract with callers explicit.
enum ReadResult {
    /// Successfully read a complete line (delimiter or char limit reached).
    Line(String),
    /// Reached end of input. Contains any partial content read before EOF.
    Eof(Option<String>),
    /// Input was interrupted (e.g., Ctrl+C). No content is returned.
    Interrupted,
    /// The operation timed out. Contains any partial content read before timeout.
    TimedOut(Option<String>),
    /// For `-t 0`: input is immediately available (exit 0).
    InputReady,
    /// For `-t 0`: no input immediately available (exit 1).
    InputNotReady,
}

/// Helper struct that encapsulates the state for reading input character by character.
///
/// This separates the concerns of character-level I/O with timeout handling from the
/// higher-level logic of line building and escape processing.
struct InputReader {
    /// The input source.
    input: std::io::BufReader<PolledInput>,
    /// Terminal mode guard, restoring the terminal's settings when dropped. It is
    /// present when reading from a terminal, where `^C` and `^D` are keys.
    term_mode: Option<brush_core::terminal::AutoModeGuard>,
    /// Whether `^D` from a terminal ends the input.
    eof_key: bool,
}

/// Events that can occur when reading input.
enum InputEvent {
    /// A regular character was read.
    Char(char),
    /// End of file was reached.
    Eof,
    /// The read operation timed out.
    Timeout,
    /// Ctrl+C was pressed.
    CtrlC,
    /// Ctrl+D was pressed.
    CtrlD,
}

impl InputReader {
    /// Creates a new input reader with optional timeout.
    fn new(
        input: brush_core::openfiles::OpenFile,
        timeout: Option<Duration>,
        term_mode: Option<brush_core::terminal::AutoModeGuard>,
        eof_key: bool,
    ) -> Self {
        Self {
            input: std::io::BufReader::with_capacity(
                1,
                PolledInput {
                    input,
                    deadline: timeout.map(|t| Instant::now() + t),
                },
            ),
            term_mode,
            eof_key,
        }
    }

    /// Checks if input is immediately available (for `-t 0`). Returns `false` if an error
    /// occurs while checking for available input.
    fn check_input_available(&self) -> bool {
        brush_core::sys::poll::poll_for_input(&self.input.get_ref().input, Duration::ZERO)
            .unwrap_or(false)
    }

    /// Reads the next input event, handling timeout and control characters. Input is
    /// read a byte at a time, as Bash does, so the bytes after the line stay unread;
    /// a byte above 0x7f is its raw character until [`finish_line`] decodes the line.
    fn read_event(&mut self) -> Result<InputEvent, brush_core::Error> {
        let mut byte = [0_u8; 1];
        let ch = loop {
            match self.input.read(&mut byte) {
                Ok(0) => return Ok(InputEvent::Eof),
                Ok(_) if byte[0].is_ascii() => break char::from(byte[0]),
                Ok(_) => break brush_core::bytes::raw_char(byte[0]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Ok(InputEvent::Timeout);
                }
                Err(e) => return Err(e.into()),
            }
        };

        // Map control characters to events; only a terminal sends them as keys.
        Ok(match ch {
            CTRL_C if self.term_mode.is_some() => InputEvent::CtrlC,
            CTRL_D if self.term_mode.is_some() && self.eof_key => InputEvent::CtrlD,
            _ => InputEvent::Char(ch),
        })
    }
}

/// Reads the input file one byte at a time, enforcing the deadline on every byte.
///
/// Wrapped in a 1-byte `BufReader`, which gives us both the pushback a byte that turns
/// out not to belong to the current UTF-8 sequence needs, and the guarantee that `read`
/// never consumes more of the descriptor than it asked for -- whatever runs next may
/// want the rest.
struct PolledInput {
    /// The input source.
    input: brush_core::openfiles::OpenFile,
    /// Optional deadline for timeout.
    deadline: Option<Instant>,
}

impl Read for PolledInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if let Some(deadline) = self.deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero()
                || !brush_core::sys::poll::poll_for_input(&self.input, remaining)?
            {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
        }
        self.input.read(buf)
    }
}

/// Decodes a line read byte by byte (see [`InputReader::read_event`]) as shell text.
fn finish_line(line: &str) -> String {
    brush_core::bytes::decode(&brush_core::bytes::encode(line)).into_owned()
}

/// Configuration for line reading behavior.
struct LineReaderConfig {
    /// Character that terminates input (None for -N mode).
    delimiter: Option<char>,
    /// Maximum characters to read (for -n or -N).
    char_limit: Option<usize>,
    /// Whether to process backslash escapes (false for -r mode).
    process_escapes: bool,
}

/// Reads a complete line of input using the given reader and configuration.
///
/// Returns a `ReadResult` indicating success, EOF, timeout, or interruption.
///
/// Note on character counting for `-n` limit:
/// Bash counts OUTPUT characters (after escape processing) toward the limit.
/// For example, with `-n 3` and input `a\bc` (4 bytes):
/// - Bash processes: 'a' (output 1), '\b' → 'b' (output 2), 'c' (output 3) → "abc"
/// - The backslash is consumed but doesn't count toward the limit
fn read_line_with_reader(
    reader: &mut InputReader,
    config: &LineReaderConfig,
    escaped: &mut Vec<usize>,
) -> Result<ReadResult, brush_core::Error> {
    let mut line = String::new();
    // The line's length in bytes, where an escaped character's offset is noted.
    let mut length = 0;
    // Tracked alongside `line`, which is a `String`: the limit counts characters, and
    // recounting a growing string on every one of them is quadratic.
    let mut output_chars = 0usize;
    let mut pending_backslash = false;

    // `-n 0` and `-N 0` read nothing.
    if config.char_limit == Some(0) {
        return Ok(ReadResult::Line(line));
    }

    loop {
        let event = reader.read_event()?;

        match event {
            InputEvent::Eof => {
                // Bash discards pending backslash on EOF.
                return Ok(ReadResult::Eof(if line.is_empty() {
                    None
                } else {
                    Some(line)
                }));
            }

            InputEvent::Timeout => {
                // Include pending backslash on timeout (different from EOF).
                if pending_backslash {
                    line.push(BACKSLASH);
                }
                return Ok(ReadResult::TimedOut(if line.is_empty() {
                    None
                } else {
                    Some(line)
                }));
            }

            InputEvent::CtrlC => {
                return Ok(ReadResult::Interrupted);
            }

            InputEvent::CtrlD => {
                // At line start = EOF, mid-input = flush current input.
                // Bash discards pending backslash here too.
                return Ok(if line.is_empty() && !pending_backslash {
                    ReadResult::Eof(None)
                } else {
                    ReadResult::Line(line)
                });
            }

            InputEvent::Char(ch) => {
                // Handle backslash escape processing (when enabled).
                if config.process_escapes {
                    if pending_backslash {
                        pending_backslash = false;

                        // Backslash-delimiter is line continuation.
                        if let Some(delim) = config.delimiter
                            && ch == delim
                        {
                            continue; // Line continuation.
                        }

                        // For other chars, add char literally (backslash consumed).
                        escaped.push(length);
                        length += encoded_len(ch);
                        line.push(ch);
                        output_chars += 1;

                        // Check character limit (based on output length).
                        if let Some(limit) = config.char_limit
                            && output_chars >= limit
                        {
                            return Ok(ReadResult::Line(line));
                        }
                        continue;
                    }

                    if ch == BACKSLASH {
                        pending_backslash = true;
                        continue;
                    }
                }

                // Check for delimiter.
                if let Some(delim) = config.delimiter
                    && ch == delim
                {
                    return Ok(ReadResult::Line(line));
                }

                // Bash drops NUL bytes; other control characters are data.
                if ch == '\0' {
                    continue;
                }

                length += encoded_len(ch);
                line.push(ch);
                output_chars += 1;

                // Check character limit (based on output length).
                if let Some(limit) = config.char_limit
                    && output_chars >= limit
                {
                    return Ok(ReadResult::Line(line));
                }
            }
        }
    }
}

impl ReadCommand {
    /// Reads a line of input, optionally with a timeout.
    ///
    /// Handles backslash escape processing:
    /// - Without `-r`: backslash-newline is line continuation, other backslashes escape the next
    ///   char
    /// - With `-r`: backslash is treated as a literal character
    fn read_line(
        &self,
        input_file: brush_core::openfiles::OpenFile,
        mut stderr_file: impl std::io::Write,
        timeout: Option<Duration>,
        escaped: &mut Vec<usize>,
    ) -> Result<ReadResult, brush_core::Error> {
        let term_mode = self.setup_terminal_settings(&input_file)?;

        // Display prompt on stderr, but only if input is from a terminal (per bash behavior).
        if let Some(prompt) = &self.prompt {
            if input_file.is_terminal() {
                write!(stderr_file, "{prompt}")?;
                stderr_file.flush()?;
            }
        }

        // Determine delimiter based on options.
        let delimiter = if self.return_after_n_chars_no_delimiter.is_some() {
            None
        } else if let Some(delimiter_str) = &self.delimiter {
            // Bash's delimiter is the first byte of the argument.
            match brush_core::bytes::encode(delimiter_str).first() {
                None => Some(NUL_DELIMITER),
                Some(byte) if byte.is_ascii() => Some(char::from(*byte)),
                Some(byte) => Some(brush_core::bytes::raw_char(*byte)),
            }
        } else {
            Some(DEFAULT_DELIMITER)
        };

        let char_limit = self
            .return_after_n_chars_no_delimiter
            .or(self.return_after_n_chars);

        // Create the input reader.
        // With `-n` or `-N`, Bash reads a terminal character by character, so
        // `^D` is data, not the end of the input.
        let eof_key = char_limit.is_none();
        let mut reader = InputReader::new(input_file, timeout, term_mode, eof_key);

        // Handle -t 0 special case: just check if input is available without reading.
        if timeout == Some(Duration::ZERO) {
            return Ok(if reader.check_input_available() {
                ReadResult::InputReady
            } else {
                ReadResult::InputNotReady
            });
        }

        // Configure and perform the read.
        let config = LineReaderConfig {
            delimiter,
            char_limit,
            process_escapes: !self.raw_mode,
        };

        Ok(
            match read_line_with_reader(&mut reader, &config, escaped)? {
                ReadResult::Line(line) => ReadResult::Line(finish_line(&line)),
                ReadResult::Eof(line) => ReadResult::Eof(line.as_deref().map(finish_line)),
                ReadResult::TimedOut(line) => {
                    ReadResult::TimedOut(line.as_deref().map(finish_line))
                }
                other => other,
            },
        )
    }

    fn setup_terminal_settings(
        &self,
        file: &brush_core::openfiles::OpenFile,
    ) -> Result<Option<brush_core::terminal::AutoModeGuard>, brush_core::Error> {
        let mode = brush_core::terminal::AutoModeGuard::new(file.to_owned()).ok();
        if let Some(mode) = &mode {
            let config = brush_core::terminal::Settings::builder()
                .line_input(false)
                .interrupt_signals(false)
                .echo_input(!self.silent)
                .build();

            mode.apply_settings(&config)?;
        }

        Ok(mode)
    }

    /// Validates the timeout value and returns an error result if invalid.
    ///
    /// Returns `Ok(Some(result))` if the timeout is invalid (caller should return early),
    /// `Ok(None)` if the timeout is valid or not specified.
    ///
    /// TODO(read): Bash uses $TMOUT as a default timeout for `read` when -t is not specified.
    fn validate_timeout(
        &self,
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    ) -> Result<Option<brush_core::ExecutionResult>, brush_core::Error> {
        if let Some(timeout) = self.timeout_in_seconds {
            if timeout < 0.0 {
                writeln!(
                    context.stderr(),
                    "{}: -t: invalid timeout specification",
                    context.command_name
                )?;
                return Ok(Some(brush_core::ExecutionResult::general_error()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use itertools::assert_equal;

    use super::*;

    // ==================== UTF-8 decoding tests ====================

    // Decoding is otherwise covered end-to-end by the compat suite; what can't be
    // expressed there is a partial sequence that never completes, since it needs a
    // writer held open while `read` gives up on it.
    //
    // `-t` needs `poll_for_input`, which only the unix backend implements; elsewhere it
    // reports `Unsupported`, so there's no timeout to observe.
    #[cfg(unix)]
    #[test]
    fn test_read_times_out_mid_sequence_without_losing_the_partial_bytes() {
        let (rx, mut tx) = std::io::pipe().unwrap();
        tx.write_all(b"\xc3").unwrap();

        let mut reader = InputReader::new(rx.into(), Some(Duration::from_millis(100)), None, true);
        let config = LineReaderConfig {
            delimiter: Some(DEFAULT_DELIMITER),
            char_limit: None,
            process_escapes: false,
        };

        // Holding `tx` means the continuation byte never arrives, so the deadline has to
        // be enforced on it and not just on the byte that started the sequence.
        let result = read_line_with_reader(&mut reader, &config, &mut vec![]).unwrap();
        drop(tx);

        assert!(
            matches!(result, ReadResult::TimedOut(Some(line)) if line == brush_core::bytes::raw_char(0xc3).to_string())
        );
    }

    // ==================== build_array_fields tests ====================

    #[test]
    fn test_build_array_fields_basic() {
        let result = build_array_fields(Some(&mark_escaped("a b c", &[])), " ", false);
        assert_eq!(
            result,
            vec![
                (None, "a".to_string()),
                (None, "b".to_string()),
                (None, "c".to_string())
            ]
        );
    }

    #[test]
    fn test_build_array_fields_skip_splitting() {
        // With -N option, entire input goes as single element.
        let result = build_array_fields(Some(&mark_escaped("a b c", &[])), " ", true);
        assert_eq!(result, vec![(None, "a b c".to_string())]);
    }

    #[test]
    fn test_build_array_fields_none_input() {
        let result = build_array_fields(None, " ", false);
        assert_eq!(result, Vec::<(Option<String>, String)>::new());
    }

    // ==================== build_variable_fields tests ====================

    #[test]
    fn test_build_variable_fields_basic() {
        let result = build_variable_fields(Some(&mark_escaped("a b c", &[])), " ", false, 3);
        assert_equal(result, VecDeque::from(vec!["a", "b", "c"]));
    }

    #[test]
    fn test_build_variable_fields_fewer_vars_than_fields() {
        // Last variable gets remainder.
        let result = build_variable_fields(Some(&mark_escaped("a b c d", &[])), " ", false, 2);
        assert_equal(result, VecDeque::from(vec!["a", "b c d"]));
    }

    #[test]
    fn test_build_variable_fields_skip_splitting() {
        // With -N option, entire input goes to first variable.
        let result = build_variable_fields(Some(&mark_escaped("a b c", &[])), " ", true, 3);
        assert_equal(result, VecDeque::from(vec!["a b c"]));
    }

    #[test]
    fn test_build_variable_fields_none_input() {
        let result = build_variable_fields(None, " ", false, 3);
        assert_eq!(result, VecDeque::<String>::new());
    }
}
