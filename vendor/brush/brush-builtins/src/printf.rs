use clap::Parser;
use std::{ffi::OsString, io::Write, ops::ControlFlow};
use uucore::format;

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

impl builtins::Command for PrintfCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if let Some(variable_name) = &self.output_variable {
            // Format to a u8 vector.
            let mut result: Vec<u8> = vec![];
            format(self.format_and_args.as_slice(), &mut result)?;

            // Convert to a string.
            let result_str = String::from_utf8(result).map_err(|_| {
                brush_core::ErrorKind::PrintfInvalidUsage("invalid UTF-8 output".into())
            })?;

            // Assign to the selected variable.
            expansion::assign_to_named_parameter(
                context.shell,
                &context.params,
                variable_name,
                result_str,
            )
            .await?;
        } else {
            format(self.format_and_args.as_slice(), context.stdout())?;
            context.stdout().flush()?;
        }

        Ok(ExecutionResult::success())
    }
}

fn format(format_and_args: &[String], writer: impl Write) -> Result<(), brush_core::Error> {
    match format_and_args {
        // `%(fmt)T` conversions are rendered here; uucore formats the rest.
        [fmt, args @ ..] if fmt.contains(")T") => {
            let (fmt, args) = render_time_conversions(fmt, args);
            format_via_uucore(&fmt, args.iter(), writer)
        }
        // Handle format string with arguments using uucore
        [fmt, args @ ..] => format_via_uucore(fmt, args.iter(), writer),
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
    args: impl Iterator<Item = impl Into<OsString>>,
    mut writer: impl Write,
) -> Result<(), brush_core::Error> {
    // Convert string arguments to FormatArgument::Unparsed
    let format_args: Vec<_> = args
        .map(|s| format::FormatArgument::Unparsed(s.into()))
        .collect();

    // Parse format string once.
    let format_items = parse_format_string(format_string)?;

    // Wrap the format arguments.
    let mut format_args_wrapper = format::FormatArguments::new(&format_args);

    // Determine whether the format string contains any specifiers that consume arguments. If it
    // doesn't, then we must only run through it once -- even when extra arguments are provided --
    // since otherwise we'd loop forever waiting for arguments that will never be consumed. This
    // matches the behavior of other shells, which print such a format string exactly once.
    let format_consumes_args = format_items
        .iter()
        .any(|(item, _)| matches!(item, format::FormatItem::Spec(_)));

    // Keep going until we've exhausted all format arguments. Also make sure to run at least once
    // even if there's no format arguments.
    while format_args.is_empty() || !format_args_wrapper.is_exhausted() {
        // Process all format items, in order. We'll bail when we're told to stop.
        for (item, backslash_quote) in &format_items {
            if let (format::FormatItem::Spec(format::Spec::QuotedString { position }), true) =
                (item, *backslash_quote)
            {
                let arg = format_args_wrapper.next_string(*position).to_string_lossy();
                let quoted = quote_printf_q(&arg);
                write!(writer, "{quoted}")?;
                continue;
            }

            let control_flow = item
                .write(&mut writer, &mut format_args_wrapper)
                .map_err(|e| match e {
                    // Propagate I/O errors directly so they can be handled appropriately
                    format::FormatError::IoError(io_err) => Error::from(io_err),
                    // Wrap other format errors
                    other => Error::from(ErrorKind::PrintfInvalidUsage(std::format!(
                        "printf formatting error: {other}"
                    ))),
                })?;

            if control_flow == ControlFlow::Break(()) {
                break;
            }
        }

        // If the format string doesn't consume any arguments, stop now; otherwise we'd reprocess
        // it forever since no arguments will ever be consumed.
        if !format_consumes_args {
            break;
        }

        // Start next batch if not exhausted
        if !format_args_wrapper.is_exhausted() {
            format_args_wrapper.start_next_batch();
        }

        if format_args.is_empty() {
            break;
        }
    }

    Ok(())
}

fn quote_printf_q(s: &str) -> String {
    let quoted = escape::quote_if_needed(s, escape::QuoteMode::BackslashEscape);
    if quoted.starts_with("$'") {
        return quoted.into_owned();
    }

    let mut quoted = quoted.replace(":~", ":\\~").replace("=~", "=\\~");
    if matches!(quoted.as_bytes().first(), Some(b'~' | b'#')) {
        quoted.insert(0, '\\');
    }
    quoted
}

type ParsedFormatItem = (format::FormatItem<format::EscapedChar>, bool);

fn parse_format_string(format_string: &str) -> Result<Vec<ParsedFormatItem>, brush_core::Error> {
    let format_items: Result<Vec<_>, _> = format::parse_spec_and_escape(format_string.as_bytes())
        .map(|result| match result {
            Ok(item @ format::FormatItem::Spec(format::Spec::QuotedString { .. })) => {
                Ok((item, true))
            }
            Ok(item) => Ok((item, false)),
            // Fixed q/Q modifiers are deliberately ignored; dynamic modifiers remain unsupported
            // until uucore exposes quoted-string metadata.
            Err(format::FormatError::SpecError(spec))
                if matches!(spec.last(), Some(b'q' | b'Q'))
                    && !spec.contains(&b'*')
                    && !spec.contains(&b'$') =>
            {
                let mut bare_q: &[u8] = b"q";
                let item = format::Spec::parse(&mut bare_q)
                    .map(format::FormatItem::Spec)
                    .map_err(|spec| format::FormatError::SpecError(spec.to_vec()))?;
                Ok((item, !spec.contains(&b'#')))
            }
            Err(error) => Err(error),
        })
        .collect();

    // Observe any errors we encountered along the way.
    let format_items = format_items
        .map_err(|e| ErrorKind::PrintfInvalidUsage(format!("printf parsing error: {e}")))?;

    Ok(format_items)
}

#[cfg(test)]
#[expect(clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn sprintf_via_uucore(
        format_string: &str,
        args: impl Iterator<Item = impl Into<OsString>>,
    ) -> Result<String> {
        let mut result = vec![];
        format_via_uucore(format_string, args, &mut result)?;

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
