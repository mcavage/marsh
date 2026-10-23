use crate::bytes::ToShellText as _;
use crate::{
    ExecutionParameters, error, expansion, extensions,
    shell::Shell,
    sys::{self, users},
};
use std::path::Path;

const VERSION_MAJOR: &str = env!("CARGO_PKG_VERSION_MAJOR");
const VERSION_MINOR: &str = env!("CARGO_PKG_VERSION_MINOR");
const VERSION_PATCH: &str = env!("CARGO_PKG_VERSION_PATCH");

pub(crate) async fn expand_prompt(
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
    spec: &str,
) -> Result<String, error::Error> {
    // Parse the prompt spec into its pieces.
    let prompt_pieces = parse_prompt(spec)?;

    // Now, render each piece.
    let mut formatted_prompt = String::new();
    for piece in prompt_pieces {
        // Pieces that semantically represent a literal char (e.g. user wrote
        // `\$` meaning a literal `$`, not input for pass-2 expansion). We
        // prepend a `\` so pass 2 consumes it and leaves the char alone.
        let semantically_literal =
            matches!(piece, brush_parser::prompt::PromptPiece::DollarOrPound);
        // What an escape stands for is data: as in Bash, pass 2 must not expand
        // a `$(...)` in a directory name or a `$x` a `\D{}` format shows.
        // (An unknown escape, `\\` and `\NNN` are passed on as written: `\\$x`
        // shows `$x`, `\044x` expands `$x`. Pass 2 takes `\"` as double quotes
        // do, but quotes are otherwise literal in it, so that one is resolved
        // here.)
        let quoted_data = !matches!(
            piece,
            brush_parser::prompt::PromptPiece::Literal(_)
                | brush_parser::prompt::PromptPiece::DollarOrPound
                | brush_parser::prompt::PromptPiece::EscapedSequence(_)
                | brush_parser::prompt::PromptPiece::Backslash
                | brush_parser::prompt::PromptPiece::AsciiCharacter(_)
        );
        if shell.options().expand_prompt_strings
            && matches!(&piece, brush_parser::prompt::PromptPiece::EscapedSequence(s) if s == "\\\"")
        {
            formatted_prompt.push('"');
            continue;
        }

        let mut formatted_piece = format_prompt_piece(shell, piece)?;
        if shell.options().expand_prompt_strings && quoted_data {
            formatted_piece = formatted_piece
                .chars()
                .flat_map(|c| {
                    // What pass 2 would take as an expansion or an escape.
                    let escape = matches!(c, '$' | '`' | '\\');
                    escape.then_some('\\').into_iter().chain(std::iter::once(c))
                })
                .collect();
        }

        // Only useful when pass 2 actually consumes a `\` for that leading
        // byte; otherwise the `\` would leak through.
        if shell.options().expand_prompt_strings
            && semantically_literal
            && formatted_piece.starts_with(expansion::DOUBLE_QUOTED_ESCAPE_CHARS)
        {
            formatted_prompt.push('\\');
        }

        formatted_prompt.push_str(&formatted_piece);
    }

    if shell.options().expand_prompt_strings {
        // Then parameters, commands and arithmetic expand, as in a here-document:
        // as in Bash, quotes are literal, and nothing is brace-, tilde- or
        // pathname-expanded.
        formatted_prompt =
            expansion::basic_expand_prompt_word(shell, params, &formatted_prompt).await?;
    }

    Ok(formatted_prompt)
}

#[cached::macros::cached(max_size = 64, key = "String", convert = r#"{ spec.to_owned() }"#)]
fn parse_prompt(
    spec: &str,
) -> Result<Vec<brush_parser::prompt::PromptPiece>, brush_parser::WordParseError> {
    brush_parser::prompt::parse(spec)
}

fn format_prompt_piece(
    shell: &Shell<impl extensions::ShellExtensions>,
    piece: brush_parser::prompt::PromptPiece,
) -> Result<String, error::Error> {
    let formatted = match piece {
        brush_parser::prompt::PromptPiece::EscapedSequence(s) => s,
        brush_parser::prompt::PromptPiece::Literal(l) => l,
        // `\NNN` (exactly three digits, as in Bash) is a byte; NUL ends nothing
        // and shows nothing.
        #[allow(clippy::cast_possible_truncation)]
        brush_parser::prompt::PromptPiece::AsciiCharacter(c) => match (c & 0xff) as u8 {
            0 => String::new(),
            byte if byte >= 0x80 => crate::bytes::raw_char(byte).to_string(),
            byte => char::from(byte).to_string(),
        },
        brush_parser::prompt::PromptPiece::Backslash => "\\".to_owned(),
        brush_parser::prompt::PromptPiece::BellCharacter => "\x07".to_owned(),
        brush_parser::prompt::PromptPiece::CarriageReturn => "\r".to_owned(),
        brush_parser::prompt::PromptPiece::CurrentCommandNumber => {
            return error::unimp("prompt: current command number");
        }
        brush_parser::prompt::PromptPiece::CurrentHistoryNumber => {
            return error::unimp("prompt: current history number");
        }
        brush_parser::prompt::PromptPiece::CurrentUser => users::get_current_username()?,
        brush_parser::prompt::PromptPiece::CurrentWorkingDirectory {
            tilde_replaced,
            basename,
        } => format_current_working_directory(shell, tilde_replaced, basename),
        brush_parser::prompt::PromptPiece::Date(format) => {
            format_date(&chrono::Local::now(), &format)
        }
        brush_parser::prompt::PromptPiece::DollarOrPound => {
            if users::is_root() {
                "#".to_owned()
            } else {
                "$".to_owned()
            }
        }
        // NOTE: We mimic bash and convert \[ into \001, a.k.a. RL_PROMPT_START_IGNORE.
        // It will need to get removed before it's actually displayed. While present it
        // also has the important (compatible) side effect of ensuring the text on either
        // side of it is not concatenated together, potentially resulting in incompatible
        // variable expansions. Also, we *only* do this if the shell is interactive.
        brush_parser::prompt::PromptPiece::EndNonPrintingSequence => {
            if shell.options().interactive {
                "\x02".to_owned()
            } else {
                String::new()
            }
        }
        brush_parser::prompt::PromptPiece::EscapeCharacter => "\x1b".to_owned(),
        brush_parser::prompt::PromptPiece::Hostname {
            only_up_to_first_dot,
        } => {
            let hn = sys::network::get_hostname()
                .unwrap_or_default()
                .to_shell_text()
                .to_string();
            if only_up_to_first_dot && let Some((first, _)) = hn.split_once('.') {
                return Ok(first.to_owned());
            }
            hn
        }
        brush_parser::prompt::PromptPiece::Newline => "\n".to_owned(),
        brush_parser::prompt::PromptPiece::NumberOfManagedJobs => {
            shell.jobs().jobs.len().to_string()
        }
        brush_parser::prompt::PromptPiece::ShellBaseName => {
            if let Some(shell_name) = shell.current_shell_name() {
                Path::new(shell_name.as_ref())
                    .file_name()
                    .map(|name| name.to_shell_text().to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            }
        }
        brush_parser::prompt::PromptPiece::ShellRelease => {
            std::format!("{VERSION_MAJOR}.{VERSION_MINOR}.{VERSION_PATCH}")
        }
        brush_parser::prompt::PromptPiece::ShellVersion => {
            std::format!("{VERSION_MAJOR}.{VERSION_MINOR}")
        }
        // NOTE: See above note for EndNonPrintingSequence
        brush_parser::prompt::PromptPiece::StartNonPrintingSequence => {
            if shell.options().interactive {
                "\x01".to_owned()
            } else {
                String::new()
            }
        }
        brush_parser::prompt::PromptPiece::TerminalDeviceBaseName => {
            sys::terminal::try_get_terminal_device_path()
                .and_then(|p| p.file_name().map(|s| s.to_shell_text().to_string()))
                .unwrap_or_default()
        }
        brush_parser::prompt::PromptPiece::Time(time_fmt) => {
            format_time(&chrono::Local::now(), &time_fmt)
        }
    };

    Ok(formatted)
}

fn format_current_working_directory(
    shell: &Shell<impl extensions::ShellExtensions>,
    tilde_replaced: bool,
    basename: bool,
) -> String {
    let mut working_dir_str = shell.working_dir().to_shell_text().to_string();

    if tilde_replaced {
        working_dir_str = shell.tilde_shorten(working_dir_str);
    }

    if basename
        && working_dir_str != "/"
        && let Some(filename) = working_dir_str.trim_end_matches('/').rsplit('/').next()
    {
        working_dir_str = filename.to_owned();
    }

    // ast-grep-ignore: platform-cfg-outside-sys
    if cfg!(windows) {
        working_dir_str = working_dir_str.replace('\\', "/");
    }

    visible(&working_dir_str, shell.is_utf8_locale())
}

/// Bash's `sh_strvis` for `\w` and `\W`: a control character is `^X` (`^?`
/// for DEL), and in a locale that is not UTF-8 a byte above 0x7f that is not
/// part of valid UTF-8 is `M-` and the byte without its top bit; tabs and
/// text stay as they are.
fn visible(text: &str, utf8_locale: bool) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        match (c, crate::bytes::raw_byte(c)) {
            (_, Some(byte)) if !utf8_locale => {
                shown.push_str("M-");
                shown.push(char::from(byte & 0x7f));
            }
            ('\x7f', None) => shown.push_str("^?"),
            (c, None) if c.is_ascii_control() && c != '\t' => {
                shown.push('^');
                shown.push(char::from(c as u8 + 0x40));
            }
            (c, _) => shown.push(c),
        }
    }
    shown
}

fn format_time<Tz: chrono::TimeZone>(
    datetime: &chrono::DateTime<Tz>,
    format: &brush_parser::prompt::PromptTimeFormat,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let formatted = match format {
        brush_parser::prompt::PromptTimeFormat::TwelveHourAM => datetime.format("%I:%M %p"),
        brush_parser::prompt::PromptTimeFormat::TwelveHourHHMMSS => datetime.format("%I:%M:%S"),
        brush_parser::prompt::PromptTimeFormat::TwentyFourHourHHMM => datetime.format("%H:%M"),
        brush_parser::prompt::PromptTimeFormat::TwentyFourHourHHMMSS => datetime.format("%H:%M:%S"),
    };

    formatted.to_string()
}

fn format_date<Tz: chrono::TimeZone>(
    datetime: &chrono::DateTime<Tz>,
    format: &brush_parser::prompt::PromptDateFormat,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match format {
        brush_parser::prompt::PromptDateFormat::WeekdayMonthDate => {
            datetime.format("%a %b %d").to_string()
        }
        #[cfg(unix)]
        brush_parser::prompt::PromptDateFormat::Custom(fmt) => {
            libc_strftime(fmt).unwrap_or_else(|| format_strftime(datetime, fmt))
        }
        #[cfg(not(unix))]
        brush_parser::prompt::PromptDateFormat::Custom(fmt) => format_strftime(datetime, fmt),
    }
}

/// `\D{fmt}` as Bash formats it: the C library's `strftime` of the local
/// time (an empty format is `%X`), which shows a conversion it does not know
/// as written.
#[cfg(unix)]
fn libc_strftime(fmt: &str) -> Option<String> {
    let fmt = if fmt.is_empty() { "%X" } else { fmt };
    let c_fmt = std::ffi::CString::new(crate::bytes::encode(fmt).into_owned()).ok()?;
    // SAFETY: `time` and `localtime_r` fill the provided values; `strftime`
    // writes at most `buffer.len()` bytes and the format is NUL-terminated.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&raw const now, &raw mut tm).is_null() {
            return None;
        }
        let mut size = 256;
        while size <= 65536 {
            let mut buffer = vec![0u8; size];
            let written = libc::strftime(
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                c_fmt.as_ptr(),
                &raw const tm,
            );
            if written > 0 || size >= 65536 {
                return Some(crate::bytes::decode(&buffer[..written]).into_owned());
            }
            size *= 4;
        }
        None
    }
}

/// Formats `fmt` one conversion at a time: one the formatter does not know is
/// shown as written (as `strftime` does), instead of failing the whole prompt.
fn format_strftime<Tz: chrono::TimeZone>(datetime: &chrono::DateTime<Tz>, fmt: &str) -> String
where
    Tz::Offset: std::fmt::Display,
{
    use std::fmt::Write as _;
    let mut out = String::new();
    let mut rest = fmt;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let spec = &rest[start..];
        // `%`, an optional padding flag, and the conversion character.
        let mut end = spec.len();
        for (index, c) in spec.char_indices().skip(1) {
            if !matches!(c, '-' | '_' | '0') || index > 1 {
                end = index + c.len_utf8();
                break;
            }
        }
        let piece = &spec[..end];
        let mut formatted = String::new();
        let items = chrono::format::StrftimeItems::new(piece);
        if write!(formatted, "{}", datetime.format_with_items(items)).is_ok() {
            out.push_str(&formatted);
        } else {
            out.push_str(piece);
        }
        rest = &spec[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_time() {
        // Create a well-known test date/time.
        let dt = chrono::DateTime::parse_from_rfc3339("2024-12-25T13:34:56.789Z").unwrap();

        assert_eq!(
            format_time(&dt, &brush_parser::prompt::PromptTimeFormat::TwelveHourAM),
            "01:34 PM"
        );

        assert_eq!(
            format_time(
                &dt,
                &brush_parser::prompt::PromptTimeFormat::TwentyFourHourHHMMSS
            ),
            "13:34:56"
        );

        assert_eq!(
            format_time(
                &dt,
                &brush_parser::prompt::PromptTimeFormat::TwelveHourHHMMSS
            ),
            "01:34:56"
        );
    }

    #[test]
    fn test_format_date() {
        // Create a well-known test date/time.
        let dt = chrono::DateTime::parse_from_rfc3339("2024-12-25T12:34:56.789Z").unwrap();

        assert_eq!(
            format_date(
                &dt,
                &brush_parser::prompt::PromptDateFormat::WeekdayMonthDate
            ),
            "Wed Dec 25"
        );

        assert_eq!(
            format_date(
                &dt,
                &brush_parser::prompt::PromptDateFormat::Custom(String::from("%Y-%m-%d"))
            ),
            "2024-12-25"
        );

        assert_eq!(
            format_date(
                &dt,
                &brush_parser::prompt::PromptDateFormat::Custom(String::from(
                    "%Y-%m-%d %H:%M:%S.%f"
                ))
            ),
            "2024-12-25 12:34:56.789000000"
        );
    }
}
