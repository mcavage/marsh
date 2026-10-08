//! Parsing for marsh-owned options around the otherwise unchanged Brush CLI.

use crate::shell_choice::ShellChoice;
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fmt,
    path::PathBuf,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LoadSelection {
    All,
    Kits(Vec<String>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductCommand {
    Shell,
    KitInstall {
        command: String,
        reference: String,
    },
    Status {
        json: bool,
    },
    Results {
        json: bool,
    },
    Result {
        selector: String,
        json: bool,
    },
    ResultsHelp,
    WorkersReset {
        selection: LoadSelection,
    },
    WorkersHelp,
    /// `marsh reset`: remove the selected home's VMs; keep the daemon.
    Reset {
        json: bool,
    },
    /// `marsh stop`: remove the selected home's VMs and stop its daemon.
    Stop {
        json: bool,
    },
}

/// `marsh --dev`: attach this shell with a development grant.
pub static DEV_SESSION: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedCli {
    pub command: ProductCommand,
    pub brush_args: Vec<OsString>,
    pub load: Option<LoadSelection>,
    pub ephemeral_home: bool,
    /// Internal handoff from the host launcher to this shell and its command
    /// children. It is never forwarded to a kit command.
    pub session_id: Option<String>,
    /// Host-approved temporary backing carried into the trusted guest shell.
    /// It is consumed by marsh and never forwarded to Brush or a kit.
    pub ephemeral_home_backing: Option<PathBuf>,
    pub guest: bool,
    /// `--shell NAME` on the host; `--marsh-shell NAME` in the guest handoff.
    /// `None` on the host means "resolve from `MARSH_SHELL` or the home default".
    pub shell: Option<ShellChoice>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliError(pub String);

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

/// # Errors
/// Returns a usage error for malformed marsh-owned options or subcommands.
pub fn parse(args: Vec<String>) -> Result<ParsedCli, CliError> {
    parse_os(args.into_iter().map(OsString::from).collect())
}

/// Preserve ordinary shell input and operands as OS bytes. Only marsh's
/// recognized product protocol fields are checked for UTF-8.
///
/// # Errors
/// Returns usage errors for malformed marsh-owned options or protocol fields.
#[allow(clippy::too_many_lines)] // One scanner for every launch option keeps their ordering rules together.
pub fn parse_os(mut args: Vec<OsString>) -> Result<ParsedCli, CliError> {
    let Some(argv0) = args.first().cloned() else {
        return Err(CliError("missing argv[0]".into()));
    };
    if let Some(parsed) = parse_product_os_command(&args, &argv0)? {
        return Ok(parsed);
    }

    let mut session_id = None;
    let mut guest = false;
    let mut ephemeral_home = false;
    let mut ephemeral_home_backing = None;
    consume_bundled_handoff(
        &mut args,
        &mut guest,
        &mut session_id,
        &mut ephemeral_home,
        &mut ephemeral_home_backing,
    )?;

    let mut brush_args = vec![argv0];
    let mut load = None;
    let mut shell = None;
    let mut index = 1;
    let mut shell_input_started = false;
    while index < args.len() {
        let argument = &args[index];
        if shell_input_started {
            brush_args.push(argument.clone());
            index += 1;
            continue;
        }
        match argument.to_str().unwrap_or("") {
            "--ephemeral-home" => ephemeral_home = true,
            "--dev" => DEV_SESSION.store(true, std::sync::atomic::Ordering::Relaxed),
            "-o" | "+o" | "-O" | "+O" => {
                // Preserve the native scanner's ordering and no-operand
                // listing forms; never invent synthetic long options.
                brush_args.push(argument.clone());
                if let Some(option) = args.get(index + 1) {
                    brush_args.push(option.clone());
                    index += 1;
                }
            }
            "--marsh-guest" => guest = true,
            "--shell" | "--marsh-shell" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| CliError("--shell requires marsh, bash, or zsh".into()))?;
                set_shell(&mut shell, checked_text(value, "--shell")?)?;
            }
            value if value.starts_with("--shell=") => {
                set_shell(&mut shell, &value["--shell=".len()..])?;
            }
            "--marsh-session" => {
                index += 1;
                set_session_id(&mut session_id, args.get(index))?;
            }
            "--marsh-home-backing" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| {
                    CliError("--marsh-home-backing requires an absolute path".into())
                })?;
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err(CliError(
                        "--marsh-home-backing requires an absolute path".into(),
                    ));
                }
                if ephemeral_home_backing.replace(path).is_some() {
                    return Err(CliError(
                        "--marsh-home-backing may be specified only once".into(),
                    ));
                }
            }
            "--load" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| CliError("--load requires KIT[,KIT...] or all".into()))?;
                set_load(&mut load, checked_text(value, "--load")?)?;
            }
            "-c" | "-" | "--" => {
                shell_input_started = true;
                brush_args.push(argument.clone());
            }
            value if value.starts_with("--load=") => {
                set_load(&mut load, &value["--load=".len()..])?;
            }
            value if value.starts_with('+') => brush_args.push(argument.clone()),
            value if !value.starts_with('-') => {
                shell_input_started = true;
                brush_args.push(argument.clone());
            }
            _ => brush_args.push(argument.clone()),
        }
        index += 1;
    }
    Ok(ParsedCli {
        command: ProductCommand::Shell,
        brush_args,
        load,
        ephemeral_home,
        session_id,
        ephemeral_home_backing,
        guest,
        shell,
    })
}

fn set_shell(slot: &mut Option<ShellChoice>, value: &str) -> Result<(), CliError> {
    let choice = ShellChoice::parse(value).ok_or_else(|| {
        CliError(format!(
            "--shell: unknown shell {value:?} (choose marsh, bash, or zsh)"
        ))
    })?;
    if slot.replace(choice).is_some() {
        return Err(CliError("--shell may be specified only once".into()));
    }
    Ok(())
}

fn parse_product_os_command(
    args: &[OsString],
    argv0: &OsStr,
) -> Result<Option<ParsedCli>, CliError> {
    if !args
        .get(1)
        .and_then(|arg| arg.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "kit" | "stop" | "reset" | "workers" | "status" | "jobs" | "results"
            )
        })
    {
        return Ok(None);
    }
    let mut product = vec![String::new()];
    product.extend(
        args.iter()
            .skip(1)
            .map(|arg| checked_text(arg, "product command").map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?,
    );
    parse_product_command(&product, argv0)
}

fn checked_text<'a>(value: &'a OsStr, field: &str) -> Result<&'a str, CliError> {
    value
        .to_str()
        .ok_or_else(|| CliError(format!("{field} requires valid UTF-8")))
}

fn set_session_id(slot: &mut Option<String>, value: Option<&OsString>) -> Result<(), CliError> {
    let value = checked_text(
        value.ok_or_else(|| CliError("--marsh-session requires an identity".into()))?,
        "--marsh-session",
    )?;
    validate_session_id(value)?;
    if slot.replace(value.to_owned()).is_some() {
        return Err(CliError(
            "--marsh-session may be specified only once".into(),
        ));
    }
    Ok(())
}

fn consume_bundled_handoff(
    args: &mut Vec<OsString>,
    guest: &mut bool,
    session_id: &mut Option<String>,
    ephemeral_home: &mut bool,
    ephemeral_home_backing: &mut Option<PathBuf>,
) -> Result<(), CliError> {
    if args.get(1).and_then(|arg| arg.to_str()) != Some("--invoke-bundled") {
        return Ok(());
    }
    while let Some(argument) = args.get(3).and_then(|arg| arg.to_str()) {
        match argument {
            "--marsh-guest" => {
                *guest = true;
                args.remove(3);
            }
            "--marsh-session" => {
                let value = args
                    .get(4)
                    .ok_or_else(|| CliError("--marsh-session requires an identity".into()))?;
                let value = checked_text(value, "--marsh-session")?;
                validate_session_id(value)?;
                *session_id = Some(value.to_owned());
                args.drain(3..=4);
            }
            "--ephemeral-home" => {
                *ephemeral_home = true;
                args.remove(3);
            }
            "--marsh-home-backing" => {
                let value = args.get(4).ok_or_else(|| {
                    CliError("--marsh-home-backing requires an absolute path".into())
                })?;
                let path = PathBuf::from(value);
                if !path.is_absolute() {
                    return Err(CliError(
                        "--marsh-home-backing requires an absolute path".into(),
                    ));
                }
                *ephemeral_home_backing = Some(path);
                args.drain(3..=4);
            }
            _ => break,
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep the public subcommand grammar in one match.
fn parse_product_command(args: &[String], argv0: &OsStr) -> Result<Option<ParsedCli>, CliError> {
    let tail = &args[1..];
    let command = match tail {
        [kit, install, command, from, reference]
            if kit == "kit" && install == "install" && from == "--from" =>
        {
            ProductCommand::KitInstall {
                command: command.clone(),
                reference: reference.clone(),
            }
        }
        [name] if name == "reset" => ProductCommand::Reset { json: false },
        [name] if name == "stop" => ProductCommand::Stop { json: false },
        [name, json] if name == "reset" && json == "--json" => ProductCommand::Reset { json: true },
        [name, json] if name == "stop" && json == "--json" => ProductCommand::Stop { json: true },
        [name, help] if name == "workers" && matches!(help.as_str(), "-h" | "--help") => {
            ProductCommand::WorkersHelp
        }
        [name, reset, selection] if name == "workers" && reset == "reset" => {
            ProductCommand::WorkersReset {
                selection: parse_load_selection(selection, "worker reset")?,
            }
        }
        [name] if name == "status" => ProductCommand::Status { json: false },
        [name, json] if name == "status" && json == "--json" => {
            ProductCommand::Status { json: true }
        }
        [name] if name == "jobs" => ProductCommand::Results { json: false },
        [name, json] if name == "jobs" && json == "--json" => {
            ProductCommand::Results { json: true }
        }
        [name, show, selector]
            if name == "jobs" && show == "show" && !selector.is_empty() && selector != "--json" =>
        {
            ProductCommand::Result {
                selector: selector.clone(),
                json: false,
            }
        }
        [name, show, selector, json] if name == "jobs" && show == "show" && json == "--json" => {
            if selector.is_empty() || selector == "--json" {
                return Err(CliError("job selector cannot be empty".into()));
            }
            ProductCommand::Result {
                selector: selector.clone(),
                json: true,
            }
        }
        [name] if name == "results" => ProductCommand::Results { json: false },
        [name, help] if name == "results" && matches!(help.as_str(), "-h" | "--help") => {
            ProductCommand::ResultsHelp
        }
        [name, json] if name == "results" && json == "--json" => {
            ProductCommand::Results { json: true }
        }
        [name, show, selector]
            if name == "results"
                && show == "show"
                && !selector.is_empty()
                && selector != "--json" =>
        {
            ProductCommand::Result {
                selector: selector.clone(),
                json: false,
            }
        }
        [name, show, selector, json] if name == "results" && show == "show" && json == "--json" => {
            if selector.is_empty() || selector == "--json" {
                return Err(CliError("result selector cannot be empty".into()));
            }
            ProductCommand::Result {
                selector: selector.clone(),
                json: true,
            }
        }
        [name, ..] if name == "status" => {
            return Err(CliError("usage: marsh status [--json]".into()));
        }
        [name, ..] if name == "jobs" => {
            return Err(CliError(format!("usage: {}", crate::job::JOBS_USAGE)));
        }
        [name, ..] if name == "results" => {
            return Err(CliError(
                "usage: marsh results [--json] | marsh results show RESULT [--json]".into(),
            ));
        }
        [name, ..] if name == "stop" || name == "reset" => {
            return Err(CliError(format!("usage: marsh {name} [--json]")));
        }
        [name, ..] if name == "workers" => {
            return Err(CliError(
                "usage: marsh workers reset KIT[,KIT...]|all".into(),
            ));
        }
        [name, ..] if name == "kit" => {
            return Err(CliError(
                "usage: marsh kit install NAME --from REPOSITORY@sha256:DIGEST".into(),
            ));
        }
        _ => return Ok(None),
    };
    Ok(Some(ParsedCli {
        command,
        brush_args: vec![argv0.into()],
        load: None,
        ephemeral_home: false,
        session_id: None,
        ephemeral_home_backing: None,
        guest: false,
        shell: None,
    }))
}

fn validate_session_id(value: &str) -> Result<(), CliError> {
    if value.len() != 36
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(CliError("invalid internal session identity".into()));
    }
    Ok(())
}

fn set_load(slot: &mut Option<LoadSelection>, value: &str) -> Result<(), CliError> {
    if slot.is_some() {
        return Err(CliError("--load may be specified only once".into()));
    }
    *slot = Some(parse_load_selection(value, "--load")?);
    Ok(())
}

fn parse_load_selection(value: &str, source: &str) -> Result<LoadSelection, CliError> {
    if value == "all" {
        return Ok(LoadSelection::All);
    }
    let kits: Vec<_> = value.split(',').map(str::to_owned).collect();
    if kits.is_empty()
        || kits.iter().any(|kit| {
            kit.is_empty()
                || kit.len() > 64
                || !kit
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(CliError(format!("{source} contains an invalid kit name")));
    }
    Ok(LoadSelection::Kits(kits))
}

#[cfg(test)]
mod tests {
    use super::{parse_os as parse, *};

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn raw_shell_operands_are_not_product_protocol_strings() {
        use std::os::unix::ffi::OsStringExt as _;
        for bytes in [
            vec![
                b"marsh".to_vec(),
                b"-c".to_vec(),
                b"printf '\\377'".to_vec(),
                b"arg\xff".to_vec(),
            ],
            vec![b"marsh".to_vec(), b"cloud".to_vec(), b"arg\xfe".to_vec()],
            vec![b"marsh".to_vec(), b"file\xff".to_vec(), b"--load".to_vec()],
        ] {
            let arguments: Vec<_> = bytes.into_iter().map(OsString::from_vec).collect();
            assert_eq!(parse_os(arguments.clone()).unwrap().brush_args, arguments);
        }
        let args = vec![
            OsString::from("marsh"),
            OsString::from("kit"),
            OsString::from_vec(b"\xff".to_vec()),
        ];
        assert!(parse_os(args).is_err());
    }

    #[test]
    fn product_options_are_removed_before_brush() {
        let parsed = parse(strings(&[
            "marsh",
            "--ephemeral-home",
            "--load",
            "claude,pi",
            "-c",
            "printf ok",
        ]))
        .unwrap();
        assert!(parsed.ephemeral_home);
        assert_eq!(parsed.session_id, None);
        assert_eq!(parsed.ephemeral_home_backing, None);
        assert!(!parsed.guest);
        assert_eq!(
            parsed.load,
            Some(LoadSelection::Kits(vec!["claude".into(), "pi".into()]))
        );
        assert_eq!(parsed.brush_args, strings(&["marsh", "-c", "printf ok"]));
    }

    #[test]
    fn shell_choice_is_a_launch_option_before_shell_input_only() {
        let parsed = parse(strings(&[
            "marsh",
            "--shell",
            "zsh",
            "-c",
            "echo --shell bash",
        ]))
        .unwrap();
        assert_eq!(parsed.shell, Some(ShellChoice::Zsh));
        assert_eq!(
            parsed.brush_args,
            strings(&["marsh", "-c", "echo --shell bash"])
        );
        let parsed = parse(strings(&["marsh", "--shell=bash", "script", "--shell"])).unwrap();
        assert_eq!(parsed.shell, Some(ShellChoice::Bash));
        assert_eq!(parsed.brush_args, strings(&["marsh", "script", "--shell"]));
        let guest = parse(strings(&[
            "marsh",
            "--marsh-guest",
            "--marsh-session",
            "00000000-0000-4000-8000-000000000000",
            "--marsh-shell",
            "bash",
        ]))
        .unwrap();
        assert_eq!(guest.shell, Some(ShellChoice::Bash));
        assert!(parse(strings(&["marsh", "--shell", "fish"])).is_err());
        assert!(parse(strings(&["marsh", "--shell"])).is_err());
        assert!(parse(strings(&["marsh", "--shell", "zsh", "--shell", "bash"])).is_err());
        assert_eq!(
            parse(strings(&["marsh", "-c", "true"])).unwrap().shell,
            None
        );
    }

    #[test]
    fn native_no_operand_option_listings_are_preserved() {
        for option in ["-o", "+o", "-O", "+O"] {
            let arguments = strings(&["marsh", option]);
            let parsed = parse(arguments.clone()).unwrap();
            assert_eq!(parsed.brush_args, arguments);
        }
    }

    #[test]
    fn other_bash_plus_options_do_not_become_script_paths() {
        let parsed = parse(strings(&["marsh", "+o", "posix", "-c", "printf ok"])).unwrap();
        assert_eq!(
            parsed.brush_args,
            strings(&["marsh", "+o", "posix", "-c", "printf ok"])
        );
        let arguments = strings(&["marsh", "+x", "-c", "printf ok"]);
        assert_eq!(parse(arguments.clone()).unwrap().brush_args, arguments);
    }

    #[test]
    fn history_option_after_command_is_positional() {
        let arguments = strings(&["marsh", "-c", "printf '%s' \"$1\"", "name", "+o", "history"]);
        let parsed = parse(arguments.clone()).unwrap();
        assert_eq!(parsed.brush_args, arguments);
    }

    #[test]
    fn command_child_session_handoff_is_consumed() {
        let parsed = parse(strings(&[
            "marsh",
            "--invoke-bundled",
            "fixture",
            "--marsh-guest",
            "--marsh-session",
            "01234567-89ab-cdef-0123-456789abcdef",
            "--ephemeral-home",
            "--marsh-home-backing",
            "/private/tmp/session-home",
            "user-argument",
        ]))
        .unwrap();
        assert_eq!(
            parsed.session_id.as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
        assert!(parsed.guest);
        assert!(parsed.ephemeral_home);
        assert_eq!(
            parsed.ephemeral_home_backing,
            Some(PathBuf::from("/private/tmp/session-home"))
        );
        assert_eq!(
            parsed.brush_args,
            strings(&["marsh", "--invoke-bundled", "fixture", "user-argument"])
        );
    }

    #[test]
    fn script_arguments_are_never_reinterpreted() {
        let parsed = parse(strings(&[
            "marsh",
            "script",
            "--ephemeral-home",
            "--no-record",
            "--load",
            "all",
        ]))
        .unwrap();
        assert!(!parsed.ephemeral_home);
        assert_eq!(parsed.load, None);
        assert_eq!(
            parsed.brush_args,
            strings(&[
                "marsh",
                "script",
                "--ephemeral-home",
                "--no-record",
                "--load",
                "all"
            ])
        );
    }

    #[test]
    fn help_tokens_after_shell_input_starts_are_brush_arguments() {
        for arguments in [
            strings(&["marsh", "-c", "printf ok", "--help"]),
            strings(&["marsh", "--", "--help"]),
            strings(&["marsh", "-", "--load", "positional"]),
        ] {
            let parsed = parse(arguments.clone()).unwrap();
            assert_eq!(parsed.brush_args, arguments);
            assert_eq!(parsed.command, ProductCommand::Shell);
        }
    }

    #[test]
    fn public_queries_are_unambiguous() {
        assert_eq!(
            parse(strings(&["marsh", "jobs", "show", "abc", "--json"]))
                .unwrap()
                .command,
            ProductCommand::Result {
                selector: "abc".into(),
                json: true
            }
        );
        assert_eq!(
            parse(strings(&["marsh", "status"])).unwrap().command,
            ProductCommand::Status { json: false }
        );
        assert_eq!(
            parse(strings(&["marsh", "results"])).unwrap().command,
            ProductCommand::Results { json: false }
        );
        assert_eq!(
            parse(strings(&["marsh", "results", "show", "17", "--json"]))
                .unwrap()
                .command,
            ProductCommand::Result {
                selector: "17".into(),
                json: true,
            }
        );
        assert!(parse(strings(&["marsh", "results", "show"])).is_err());
        assert_eq!(
            parse(strings(&["marsh", "workers", "reset", "claude,pi"]))
                .unwrap()
                .command,
            ProductCommand::WorkersReset {
                selection: LoadSelection::Kits(vec!["claude".into(), "pi".into()]),
            }
        );
        assert_eq!(
            parse(strings(&["marsh", "workers", "reset", "all"]))
                .unwrap()
                .command,
            ProductCommand::WorkersReset {
                selection: LoadSelection::All,
            }
        );
        assert!(parse(strings(&["marsh", "workers", "reset", "unknown/name"])).is_err());
        assert_eq!(
            parse(strings(&["marsh", "workers", "--help"]))
                .unwrap()
                .command,
            ProductCommand::WorkersHelp
        );
        for (word, json) in [
            ("reset", false),
            ("reset", true),
            ("stop", false),
            ("stop", true),
        ] {
            let mut argv = vec!["marsh", word];
            if json {
                argv.push("--json");
            }
            let command = parse(strings(&argv)).unwrap().command;
            let expected = if word == "reset" {
                ProductCommand::Reset { json }
            } else {
                ProductCommand::Stop { json }
            };
            assert_eq!(command, expected);
        }
        assert!(parse(strings(&["marsh", "stop", "now"])).is_err());
        assert!(parse(strings(&["marsh", "reset", "--all"])).is_err());
        // Like `status`, a script with a lifecycle name runs by path. The
        // retired `scope` group is an ordinary script operand, not an alias.
        for argv in [["marsh", "./stop", "x"], ["marsh", "scope", "stop"]] {
            let parsed = parse(strings(&argv)).unwrap();
            assert_eq!(parsed.command, ProductCommand::Shell);
            assert_eq!(parsed.brush_args, strings(&argv));
        }
        let reference = format!("docker.io/example/alternate@sha256:{}", "a".repeat(64));
        assert_eq!(
            parse(vec![
                "marsh".into(),
                "kit".into(),
                "install".into(),
                "alternate".into(),
                "--from".into(),
                reference.clone().into(),
            ])
            .unwrap()
            .command,
            ProductCommand::KitInstall {
                command: "alternate".into(),
                reference,
            }
        );
        assert!(parse(strings(&["marsh", "kit", "install"])).is_err());
    }
}
