use clap::Parser;
use std::{io::Write, ops::ControlFlow};
use uucore::format;

use brush_core::bytes;
use brush_core::{Error, ErrorKind, ExecutionResult, builtins, escape, expansion};

/// Format a string.
#[derive(Parser)]
#[clap(disable_help_flag = true, disable_version_flag = true)]
pub(crate) struct PrintfCommand {
    /// If specified, the output of the command is assigned to this variable.
    #[arg(short = 'v')]
    output_variable: Option<String>,

    /// Format string + arguments to the format string.
    ///
    /// N.B. We intentionally do *not* enable `allow_hyphen_values` here. Doing so would
    /// cause an attached short-option value such as `-va` (i.e. `-v a`) to be misparsed as
    /// a positional argument. With it disabled, a format string that genuinely needs to
    /// start with a hyphen must be preceded by `--`, matching other shells' behavior.
    #[arg(trailing_var_arg = true, required = true)]
    format_and_args: Vec<String>,
}

brush_clap_adapter::impl_parse_args!(PrintfCommand);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for PrintfCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let diagnostics;
        if let Some(variable_name) = &self.output_variable {
            // Format to a u8 vector.
            let mut result: Vec<u8> = vec![];
            diagnostics = format(self.format_and_args.as_slice(), &mut result)?;
            write_diagnostics(&context, &diagnostics.messages)?;
            // A shell value ends at its first NUL, as in Bash.
            if let Some(end) = result.iter().position(|byte| *byte == 0) {
                result.truncate(end);
            }

            // Convert to a string.
            let result_str = bytes::decode(&bytes::encode_output(&result)).into_owned();

            // Assign to the selected variable.
            expansion::assign_to_named_parameter(
                context.shell,
                &context.params,
                variable_name,
                result_str,
            )
            .await?;
        } else {
            // Formatted whole, so raw bytes in the format survive (uucore writes
            // literal text a byte at a time).
            let mut result: Vec<u8> = vec![];
            let formatted = format(self.format_and_args.as_slice(), &mut result);
            let mut written = 0;
            if let Ok(diagnostics) = &formatted {
                for (index, (output_before, _)) in diagnostics.messages.iter().enumerate() {
                    let output = &result[written..*output_before];
                    context.stdout().write_all(&bytes::encode_output(output))?;
                    context.stdout().flush()?;
                    written = *output_before;
                    write_diagnostics(&context, &diagnostics.messages[index..=index])?;
                }
            }
            context
                .stdout()
                .write_all(&bytes::encode_output(&result[written..]))?;
            context.stdout().flush()?;
            diagnostics = formatted?;
        }

        if diagnostics.failed {
            return Ok(ExecutionResult::general_error());
        }
        Ok(ExecutionResult::success())
    }
}

fn write_diagnostics(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    messages: &[(usize, String)],
) -> Result<(), brush_core::Error> {
    for (_, message) in messages {
        let message = std::format!(
            "{}{}: {message}\n",
            context.shell.diagnostic_prefix(),
            context.command_name
        );
        context.stderr().write_all(&bytes::encode(&message))?;
    }
    Ok(())
}

fn format(
    format_and_args: &[String],
    writer: &mut Vec<u8>,
) -> Result<Diagnostics, brush_core::Error> {
    match format_and_args {
        // `%(fmt)T` conversions are rendered here; uucore formats the rest.
        [fmt, args @ ..] if fmt.contains(")T") => {
            let (fmt, args) = render_time_conversions(fmt, args);
            format_via_uucore(&fmt, &args, writer)
        }
        // Handle format string with arguments using uucore.
        [fmt, args @ ..] => format_via_uucore(fmt, args, writer),
        // Handle case with no format string (we shouldn't be able to get here since clap will
        // fail parsing when the format string is missing)
        [] => Err(ErrorKind::PrintfInvalidUsage("missing operand".into()).into()),
    }
}

/// Rewrites each Bash `%[flags][width][.precision](strftime)T` conversion as a
/// `%s` conversion and replaces its argument (an epoch time; empty, missing or
/// `-1` is now, `-2` is shell start, approximated as now) with the formatted
/// local time, keeping argument reuse across format cycles.
fn render_time_conversions(format: &str, args: &[String]) -> (String, Vec<String>) {
    // Each conversion consumes some arguments; `Some(strftime)` marks the time
    // argument of a `%(...)T` conversion.
    let mut consumed: Vec<Option<String>> = Vec::new();
    let mut rewritten = String::with_capacity(format.len());
    let mut rest = format;
    while let Some(percent) = rest.find('%') {
        rewritten.push_str(&rest[..percent]);
        let spec = &rest[percent + 1..];
        if let Some(after) = spec.strip_prefix('%') {
            rewritten.push_str("%%");
            rest = after;
            continue;
        }
        let modifiers = spec
            .find(|c: char| {
                !matches!(c, '-' | '+' | ' ' | '#' | '0' | '\'' | '.' | '*') && !c.is_ascii_digit()
            })
            .unwrap_or(spec.len());
        let (flags, tail) = spec.split_at(modifiers);
        for _ in flags.matches('*') {
            consumed.push(None);
        }
        if let Some(time) = tail.strip_prefix('(')
            && let Some(close) = time.find(")T")
        {
            consumed.push(Some(time[..close].to_owned()));
            rewritten.push('%');
            rewritten.push_str(flags);
            rewritten.push('s');
            rest = &time[close + 2..];
        } else {
            let conversion = tail.chars().next().map_or(0, char::len_utf8);
            if conversion > 0 {
                consumed.push(None);
            }
            rewritten.push('%');
            rewritten.push_str(&spec[..modifiers + conversion]);
            rest = &tail[conversion..];
        }
    }
    rewritten.push_str(rest);

    let per_cycle = consumed.len().max(1);
    let mut args = args.to_vec();
    // A missing time argument means "now", so complete the last cycle up to its
    // final time conversion.
    let partial = args.len() % per_cycle;
    if args.is_empty() || partial != 0 {
        let start = args.len() - partial;
        if let Some(last_time) = consumed.iter().rposition(Option::is_some)
            && start + last_time >= args.len()
        {
            args.resize(start + last_time + 1, String::new());
        }
    }
    for (index, arg) in args.iter_mut().enumerate() {
        if let Some(Some(strftime)) = consumed.get(index % per_cycle) {
            *arg = format_time(strftime, arg);
        }
    }
    (rewritten, args)
}

fn format_time(strftime: &str, arg: &str) -> String {
    use chrono::TimeZone as _;
    use std::fmt::Write as _;
    let seconds = match arg.trim() {
        "" | "-1" | "-2" => chrono::Local::now().timestamp(),
        value => value.parse().unwrap_or(0),
    };
    let strftime = if strftime.is_empty() { "%X" } else { strftime };
    let Some(time) = chrono::Local.timestamp_opt(seconds, 0).single() else {
        return String::new();
    };
    let mut out = String::new();
    if write!(out, "{}", time.format(strftime)).is_err() {
        return strftime.to_owned();
    }
    // The rendered time is passed through `%s`; keep it literal.
    out
}

fn format_via_uucore(
    format_string: &str,
    args: &[String],
    writer: &mut Vec<u8>,
) -> Result<Diagnostics, brush_core::Error> {
    let (format_items, invalid) = parse_format_string(format_string)?;
    let mut printf = Printf {
        args,
        next: 0,
        highest: None,
        offset: 0,
        diagnostics: Diagnostics::default(),
    };

    // Determine whether the format string contains any specifiers that consume arguments. If it
    // doesn't, then we must only run through it once -- even when extra arguments are provided --
    // since otherwise we'd loop forever waiting for arguments that will never be consumed. This
    // matches the behavior of other shells, which print such a format string exactly once.
    let format_consumes_args = format_items
        .iter()
        .any(|(item, ..)| matches!(item, format::FormatItem::Spec(_)));

    // Keep going until we've exhausted all format arguments. Also make sure to run at least once
    // even if there's no format arguments.
    loop {
        for item in &format_items {
            printf.diagnostics.flushed(writer);
            if printf.write_item(item, &mut *writer)?.is_break() {
                // `\c` in a `%b` argument ends all output.
                return Ok(printf.diagnostics);
            }
        }
        if let Some(conversion) = &invalid {
            printf.diagnostics.flushed(writer);
            printf
                .diagnostics
                .error(std::format!("`{conversion}': invalid format character"));
            return Ok(printf.diagnostics);
        }

        // If the format string doesn't consume any arguments, stop now; otherwise we'd reprocess
        // it forever since no arguments will ever be consumed.
        if !format_consumes_args || args.is_empty() {
            break;
        }
        printf.next_batch();
        if printf.offset >= args.len() {
            break;
        }
    }

    Ok(printf.diagnostics)
}

/// Messages from a `printf`, each with how much output comes before it, and
/// whether one of them makes its status 1.
#[derive(Default)]
struct Diagnostics {
    messages: Vec<(usize, String)>,
    output_before: usize,
    failed: bool,
}

impl Diagnostics {
    /// Bash's standard output is line-buffered: a diagnostic comes after the
    /// output's complete lines so far.
    fn flushed(&mut self, output: &[u8]) {
        self.output_before = output
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |at| at + 1);
    }

    fn warn(&mut self, message: String) {
        self.messages.push((self.output_before, message));
    }

    fn error(&mut self, message: String) {
        self.warn(message);
        self.failed = true;
    }
}

/// What a conversion reads its argument as.
#[derive(Clone, Copy)]
enum ArgKind {
    Text,
    Signed,
    Unsigned,
    Float,
}

/// Bash's `printf`: the arguments each conversion takes (in uucore's order,
/// so its formatting can be used), converted as Bash converts them.
struct Printf<'a> {
    args: &'a [String],
    next: usize,
    highest: Option<usize>,
    offset: usize,
    diagnostics: Diagnostics,
}

impl<'a> Printf<'a> {
    /// The argument at `position` (`N$`, from 1), or else the next one.
    fn take(&mut self, position: Option<usize>) -> Option<&'a str> {
        let index = match position {
            None => {
                self.next += 1;
                self.next - 1
            }
            Some(position) => {
                let index = (position - 1).saturating_add(self.offset);
                self.highest = Some(self.highest.map_or(index, |highest| highest.max(index)));
                index
            }
        };
        self.args.get(index).map(String::as_str)
    }

    fn next_batch(&mut self) {
        self.offset = self
            .next
            .max(self.highest.map_or(0, |highest| highest.saturating_add(1)));
        self.next = self.offset;
    }

    fn write_item(
        &mut self,
        (item, quoting, taken): &ParsedFormatItem,
        mut writer: impl Write,
    ) -> Result<ControlFlow<()>, brush_core::Error> {
        let quoting = *quoting;
        let spec = match item {
            // Bash's format strings have no `\c`.
            format::FormatItem::Char(format::EscapedChar::End) => {
                writer.write_all(b"\\c")?;
                return Ok(ControlFlow::Continue(()));
            }
            format::FormatItem::Char(_) => {
                // Only `\c` (handled above) stops.
                let _ = item
                    .write(&mut writer, &mut format::FormatArguments::new(&[]))
                    .map_err(format_error)?;
                return Ok(ControlFlow::Continue(()));
            }
            format::FormatItem::Spec(spec) => spec,
        };
        let value_position = taken.last().and_then(|(position, _)| *position);
        match (spec, quoting) {
            (format::Spec::QuotedString { .. }, Some(alternate)) => {
                let arg = self.take(value_position).unwrap_or_default();
                let quoted = if alternate {
                    escape::quote_if_needed(arg, escape::QuoteMode::SingleQuote).into_owned()
                } else {
                    quote_printf_q(arg)
                };
                write!(writer, "{quoted}")?;
            }
            (format::Spec::EscapedString { .. }, _) => {
                let arg = self.take(value_position).unwrap_or_default();
                let (expanded, stop) = self.expand_b(arg);
                writer.write_all(&expanded)?;
                if stop {
                    return Ok(ControlFlow::Break(()));
                }
            }
            // The conversion gets its arguments, converted, where it looks
            // for them in a fresh argument list: the next ones in order, and
            // `N$` ones at N.
            _ => {
                let mut args = vec![];
                let mut next = 0;
                for (position, kind) in taken {
                    let index = position.map_or_else(
                        || {
                            next += 1;
                            next - 1
                        },
                        |position| position - 1,
                    );
                    if args.len() <= index {
                        args.resize(index + 1, format::FormatArgument::Unparsed("".into()));
                    }
                    args[index] = self.argument(*position, *kind);
                }
                let _ = item
                    .write(&mut writer, &mut format::FormatArguments::new(&args))
                    .map_err(format_error)?;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn argument(&mut self, position: Option<usize>, kind: ArgKind) -> format::FormatArgument {
        let arg = self.take(position);
        match (kind, arg) {
            (ArgKind::Text, arg) => {
                format::FormatArgument::Unparsed(bytes::to_os_string(arg.unwrap_or_default()))
            }
            (ArgKind::Signed, None) => format::FormatArgument::SignedInt(0),
            (ArgKind::Unsigned, None) => format::FormatArgument::UnsignedInt(0),
            (ArgKind::Float, None) => format::FormatArgument::Unparsed("0".into()),
            (kind, Some(arg)) => {
                if let Some(code) = character_code(arg) {
                    return match kind {
                        ArgKind::Unsigned => format::FormatArgument::UnsignedInt(code.into()),
                        ArgKind::Float => format::FormatArgument::Unparsed(code.to_string().into()),
                        _ => format::FormatArgument::SignedInt(code.into()),
                    };
                }
                let (value, end) = match kind {
                    ArgKind::Float => {
                        let end = float_prefix(arg);
                        let number = arg[..end].trim_start();
                        let number = if number.is_empty() { "0" } else { number };
                        (format::FormatArgument::Unparsed(number.into()), end)
                    }
                    _ => {
                        let (magnitude, negative, end) = integer_prefix(arg);
                        let unsigned = matches!(kind, ArgKind::Unsigned);
                        let limit: u128 = match (unsigned, negative) {
                            (true, _) => u64::MAX.into(),
                            (false, false) => i64::MAX.unsigned_abs().into(),
                            (false, true) => i64::MIN.unsigned_abs().into(),
                        };
                        if magnitude > limit && end == arg.len() && end > 0 {
                            self.diagnostics
                                .warn(std::format!("{arg}: {}", range_error()));
                        }
                        let magnitude = magnitude.min(limit);
                        let value = if unsigned {
                            #[allow(clippy::cast_possible_truncation)]
                            let value = magnitude as u64;
                            format::FormatArgument::UnsignedInt(if negative {
                                value.wrapping_neg()
                            } else {
                                value
                            })
                        } else {
                            #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
                            let value = if negative {
                                (magnitude as i128).wrapping_neg() as i64
                            } else {
                                magnitude as i64
                            };
                            format::FormatArgument::SignedInt(value)
                        };
                        (value, end)
                    }
                };
                if end == 0 || end != arg.len() {
                    self.diagnostics
                        .error(std::format!("{arg}: invalid number"));
                }
                value
            }
        }
    }

    /// `%b`: Bash's `bexpand`. Returns the bytes, and whether a `\c` ended them.
    fn expand_b(&mut self, arg: &str) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(arg.len());
        let mut chars = arg.chars().peekable();
        let mut buffer = [0; 4];
        while let Some(c) = chars.next() {
            let Some(&escape) = chars.peek().filter(|_| c == '\\') else {
                out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                continue;
            };
            let byte = match escape {
                'a' => Some(0x07),
                'b' => Some(0x08),
                'e' | 'E' => Some(0x1b),
                'f' => Some(0x0c),
                'n' => Some(b'\n'),
                'r' => Some(b'\r'),
                't' => Some(b'\t'),
                'v' => Some(0x0b),
                '\\' => Some(b'\\'),
                _ => None,
            };
            if let Some(byte) = byte {
                chars.next();
                out.push(byte);
                continue;
            }
            match escape {
                '0'..='7' => {
                    chars.next();
                    let mut value = escape.to_digit(8).unwrap_or_default();
                    let more = if value == 0 { 3 } else { 2 };
                    for _ in 0..more {
                        let Some(digit) = chars.peek().and_then(|c| c.to_digit(8)) else {
                            break;
                        };
                        chars.next();
                        value = value * 8 + digit;
                    }
                    #[allow(clippy::cast_possible_truncation)]
                    out.push((value & 0xff) as u8);
                }
                'x' | 'u' | 'U' => {
                    chars.next();
                    let most = match escape {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let mut value = 0;
                    let mut digits = 0;
                    while digits < most
                        && let Some(digit) = chars.peek().and_then(|c| c.to_digit(16))
                    {
                        chars.next();
                        value = value * 16 + digit;
                        digits += 1;
                    }
                    if digits == 0 {
                        let kind = if escape == 'x' { "hex" } else { "unicode" };
                        self.diagnostics
                            .warn(std::format!("missing {kind} digit for \\{escape}"));
                        out.push(b'\\');
                        out.extend_from_slice(escape.encode_utf8(&mut buffer).as_bytes());
                    } else if escape == 'x' {
                        #[allow(clippy::cast_possible_truncation)]
                        out.push(value as u8);
                    } else if let Some(c) = char::from_u32(value) {
                        out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
                    }
                }
                'c' => return (out, true),
                // Anything else, `\"` included, keeps its backslash.
                _ => out.push(b'\\'),
            }
        }
        (out, false)
    }
}

fn format_error(error: format::FormatError) -> Error {
    match error {
        // Propagate I/O errors directly so they can be handled appropriately
        format::FormatError::IoError(io_err) => Error::from(io_err),
        // Wrap other format errors
        other => Error::from(ErrorKind::PrintfInvalidUsage(std::format!(
            "printf formatting error: {other}"
        ))),
    }
}

/// The arguments `spec` (whose text, after `%`, is `text`) reads, in the order
/// uucore reads them: `*` width, `*` precision, then the value; each with its
/// `N$` position, if it has one.
fn spec_arguments(text: &[u8], spec: &format::Spec) -> Vec<(Option<usize>, ArgKind)> {
    // `N$` at the start of `text`, and the text after it.
    fn position(text: &[u8]) -> (Option<usize>, &[u8]) {
        let digits = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
        match text.get(digits) {
            Some(b'$') if digits > 0 => (
                std::str::from_utf8(&text[..digits])
                    .ok()
                    .and_then(|n| n.parse().ok()),
                &text[digits + 1..],
            ),
            _ => (None, text),
        }
    }
    let (value, mut rest) = position(text);
    let flags = rest
        .iter()
        .take_while(|byte| matches!(byte, b'-' | b'+' | b' ' | b'#' | b'0' | b'\''))
        .count();
    rest = &rest[flags..];
    let mut stars = vec![];
    if let Some(after) = rest.strip_prefix(b"*") {
        let (star, after) = position(after);
        stars.push(star);
        rest = after;
    }
    rest = &rest[rest.iter().take_while(|byte| byte.is_ascii_digit()).count()..];
    if let Some(after) = rest.strip_prefix(b".*") {
        stars.push(position(after).0);
    }
    let (kind, stars) = match spec {
        format::Spec::Char { .. } => (ArgKind::Text, &stars[..stars.len().min(1)]),
        format::Spec::String { .. } => (ArgKind::Text, &stars[..]),
        format::Spec::EscapedString { .. } | format::Spec::QuotedString { .. } => {
            (ArgKind::Text, &[][..])
        }
        format::Spec::SignedInt { .. } => (ArgKind::Signed, &stars[..]),
        format::Spec::UnsignedInt { .. } => (ArgKind::Unsigned, &stars[..]),
        format::Spec::Float { .. } => (ArgKind::Float, &stars[..]),
    };
    let mut arguments: Vec<_> = stars.iter().map(|star| (*star, ArgKind::Signed)).collect();
    arguments.push((value, kind));
    arguments
}

/// A `'c` or `"c` argument's character code (0 for a lone quote).
fn character_code(arg: &str) -> Option<u32> {
    let rest = arg.strip_prefix(['\'', '"'])?;
    Some(rest.chars().next().map_or(0, |c| {
        bytes::raw_byte(c).map_or_else(|| u32::from(c), u32::from)
    }))
}

/// `strtoimax(arg, &end, 0)`: the magnitude (saturated), its sign, and where
/// the number ends (0 when there is none).
fn integer_prefix(arg: &str) -> (u128, bool, usize) {
    let text = arg.as_bytes();
    let mut at = text
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .unwrap_or(text.len());
    let negative = text.get(at) == Some(&b'-');
    if matches!(text.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let (radix, start) = match (text.get(at), text.get(at + 1), text.get(at + 2)) {
        (Some(b'0'), Some(b'x' | b'X'), Some(digit)) if digit.is_ascii_hexdigit() => (16, at + 2),
        (Some(b'0'), _, _) => (8, at),
        _ => (10, at),
    };
    let mut magnitude: u128 = 0;
    let mut end = start;
    while let Some(digit) = text
        .get(end)
        .and_then(|byte| char::from(*byte).to_digit(radix))
    {
        magnitude = magnitude
            .saturating_mul(radix.into())
            .saturating_add(digit.into())
            .min(u128::from(u64::MAX) + 1);
        end += 1;
    }
    if end == start {
        return (0, false, 0);
    }
    (magnitude, negative, end)
}

/// Where `strtod(arg, &end)` stops (0 when there is no number).
fn float_prefix(arg: &str) -> usize {
    let text = arg.as_bytes();
    let mut at = text
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .unwrap_or(text.len());
    if matches!(text.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let rest = arg[at..].to_ascii_lowercase();
    for word in ["infinity", "inf", "nan"] {
        if rest.starts_with(word) {
            return at + word.len();
        }
    }
    let hex = rest.starts_with("0x")
        && rest[2..].starts_with(|c: char| c.is_ascii_hexdigit() || c == '.');
    let (digits, exponent): (fn(&u8) -> bool, u8) = if hex {
        at += 2;
        (u8::is_ascii_hexdigit, b'p')
    } else {
        (u8::is_ascii_digit, b'e')
    };
    let mut end = at;
    let mut seen = false;
    while text.get(end).is_some_and(digits) {
        end += 1;
        seen = true;
    }
    if text.get(end) == Some(&b'.') {
        end += 1;
        while text.get(end).is_some_and(digits) {
            end += 1;
            seen = true;
        }
    }
    if !seen {
        return if hex { at - 1 } else { 0 };
    }
    if text.get(end).map(u8::to_ascii_lowercase) == Some(exponent) {
        let mut exp = end + 1;
        if matches!(text.get(exp), Some(b'-' | b'+')) {
            exp += 1;
        }
        if text.get(exp).is_some_and(u8::is_ascii_digit) {
            while text.get(exp).is_some_and(u8::is_ascii_digit) {
                exp += 1;
            }
            end = exp;
        }
    }
    end
}

/// The C library's message for `ERANGE`.
fn range_error() -> String {
    #[cfg(unix)]
    let message = std::io::Error::from_raw_os_error(nix::errno::Errno::ERANGE as i32).to_string();
    #[cfg(not(unix))]
    let message = String::from("Result too large");
    message
        .split(" (os error")
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn quote_printf_q(s: &str) -> String {
    // Like bash's, this escapes commas too, so the result is safe even inside a brace
    // expansion.
    let options = escape::QuoteOptions::builder()
        .preferred_mode(escape::QuoteMode::BackslashEscape)
        .quote_commas(true)
        .build();
    escape::quote(s, &options).into_owned()
}

/// A format item; for a `%q` one whether it is `%#q` (quote with single
/// quotes); and the arguments it reads (see [`spec_arguments`]).
type ParsedFormatItem = (
    format::FormatItem<format::EscapedChar>,
    Option<bool>,
    Vec<(Option<usize>, ArgKind)>,
);

/// The format's items up to the first invalid conversion, and that conversion's
/// character (Bash prints what comes before it, then fails).
fn parse_format_string(
    format_string: &str,
) -> Result<(Vec<ParsedFormatItem>, Option<String>), brush_core::Error> {
    let parse_error = |e: format::FormatError| {
        ErrorKind::PrintfInvalidUsage(format!("printf parsing error: {e}"))
    };
    // Each conversion's text, in order, as uucore's parser finds them: after
    // `%` (not `%%`, and not `\%`, an escape), `Spec::parse` takes its text.
    let mut texts = vec![];
    let mut rest = format_string.as_bytes();
    while let Some(at) = rest.iter().position(|byte| matches!(byte, b'%' | b'\\')) {
        match &rest[at..] {
            [b'\\', _, after @ ..] | [b'%', b'%', after @ ..] => rest = after,
            [b'%', after @ ..] => {
                let mut spec = after;
                let length = match format::Spec::parse(&mut spec) {
                    Ok(_) => after.len() - spec.len(),
                    Err(error) => error.len(),
                };
                texts.push(&after[..length]);
                rest = &after[length..];
            }
            _ => break,
        }
    }
    let mut texts = texts.into_iter();

    let mut items = vec![];
    for result in format::parse_spec_and_escape(format_string.as_bytes()) {
        match result {
            Ok(format::FormatItem::Spec(spec)) => {
                let arguments = spec_arguments(texts.next().unwrap_or_default(), &spec);
                let quoting = matches!(spec, format::Spec::QuotedString { .. }).then_some(false);
                items.push((format::FormatItem::Spec(spec), quoting, arguments));
            }
            Ok(item) => items.push((item, None, vec![])),
            // Fixed q/Q modifiers are deliberately ignored; dynamic modifiers remain unsupported
            // until uucore exposes quoted-string metadata.
            Err(format::FormatError::SpecError(spec, span))
                if matches!(spec.last(), Some(b'q' | b'Q'))
                    && !spec.contains(&b'*')
                    && !spec.contains(&b'$') =>
            {
                texts.next();
                let mut bare_q: &[u8] = b"q";
                let item = format::Spec::parse(&mut bare_q)
                    .map(format::FormatItem::Spec)
                    .map_err(|spec| {
                        parse_error(format::FormatError::SpecError(spec.to_vec(), span))
                    })?;
                items.push((
                    item,
                    Some(spec.contains(&b'#')),
                    vec![(None, ArgKind::Text)],
                ));
            }
            Err(format::FormatError::SpecError(spec, _)) => {
                return Ok((items, Some(invalid_conversion(format_string, &spec))));
            }
            Err(e) => return Err(parse_error(e).into()),
        }
    }
    Ok((items, None))
}

/// The character that ends the invalid conversion `spec` in `format_string`.
fn invalid_conversion(format_string: &str, spec: &[u8]) -> String {
    let start = format_string
        .as_bytes()
        .windows(spec.len())
        .position(|window| window == spec)
        .map_or(format_string.len(), |at| at + spec.len().saturating_sub(1));
    format_string
        .get(start..)
        .and_then(|rest| rest.chars().next())
        .map(String::from)
        .unwrap_or_default()
}

#[cfg(test)]
#[expect(clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn sprintf_via_uucore<'a>(
        format_string: &str,
        args: impl Iterator<Item = &'a &'a str>,
    ) -> Result<String> {
        let args: Vec<String> = args.map(|arg| (*arg).to_owned()).collect();
        let mut result = vec![];
        format_via_uucore(format_string, &args, &mut result)?;

        Ok(String::from_utf8(result)?)
    }

    #[test]
    fn test_basic_sprintf() -> Result<()> {
        assert_eq!(sprintf_via_uucore("%s", std::iter::once(&"xyz"))?, "xyz");
        assert_eq!(sprintf_via_uucore(r"%d\n", std::iter::once(&"1"))?, "1\n");

        Ok(())
    }

    #[test]
    fn test_sprintf_without_args() -> Result<()> {
        let empty: [&str; 0] = [];

        assert_eq!(sprintf_via_uucore("xyz", empty.iter())?, "xyz");
        assert_eq!(sprintf_via_uucore("%s|", empty.iter())?, "|");

        Ok(())
    }

    #[test]
    fn test_sprintf_with_cycles() -> Result<()> {
        assert_eq!(sprintf_via_uucore("%s|", ["x", "y"].iter())?, "x|y|");

        Ok(())
    }
}
