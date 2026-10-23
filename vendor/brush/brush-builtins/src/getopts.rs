use std::{collections::HashMap, io::Write};

use clap::Parser;

use brush_core::{ExecutionResult, builtins, env, variables};

/// Parse command options.
#[derive(Parser)]
pub(crate) struct GetOptsCommand {
    /// Specification for options
    options_string: String,

    /// Name of variable to receive next option
    variable_name: String,

    /// Arguments to parse
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

/// Where `getopts` is in the arguments, between invocations; kept in the
/// shell, so it follows subshell rules as `OPTIND` does.
#[derive(Clone, Debug, Default)]
struct GetOptsState {
    /// The argument (1-based) and character index it is partway through, if in the
    /// middle of bundled options.
    position: Option<(usize, usize)>,
    /// The `OPTIND` (see [`optind_record`]) it last set, to notice any assignment to it.
    last_optind: Option<String>,
}
const DEFAULT_NEXT_CHAR_INDEX: usize = 1;

/// The result of processing one option from the argument list.
struct GetOptsResult {
    /// The value to assign to the target variable (the option char, `?`, or `:`).
    variable_value: String,
    /// The value for OPTARG, if any (option's argument or, on error, the offending char).
    optarg: Option<String>,
    /// The new value for OPTIND after this call.
    optind: usize,
    /// Exit code: success (0) if an option was found, general error (1) when done.
    exit_code: ExecutionResult,
}

/// Parsed representation of the optstring (e.g., `":a:bc"`).
struct OptionSpec {
    /// Maps each option character to whether it requires an argument.
    defs: HashMap<char, bool>,
    /// True when the optstring has a leading `:`, suppressing error messages
    /// and reporting errors via `?`/`:` in the variable and OPTARG instead.
    silent_errors: bool,
}

/// Parses the optstring into an [`OptionSpec`]. A leading `:` enables silent error
/// mode. Each letter defines an option; a `:` immediately after a letter means it
/// takes an argument. Duplicate letters are ignored (first definition wins).
fn parse_option_spec(spec: &str) -> OptionSpec {
    let mut defs = HashMap::<char, bool>::new();
    let mut silent_errors = false;

    let mut last_char = None;
    for c in spec.chars() {
        if c == ':' {
            if let Some(last_char) = last_char {
                defs.insert(last_char, true);
            } else if defs.is_empty() {
                // First character is ':' — request silent error reporting.
                silent_errors = true;
            }
            continue;
        }

        if let std::collections::hash_map::Entry::Vacant(e) = defs.entry(c) {
            e.insert(false);
            last_char = Some(c);
        } else {
            // Duplicate option char; first definition wins.
            // Clear last_char so a trailing colon doesn't modify the first definition.
            last_char = None;
        }
    }

    OptionSpec {
        defs,
        silent_errors,
    }
}

brush_clap_adapter::impl_parse_args!(GetOptsCommand, keep_double_dash = args);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for GetOptsCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        mut context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // Validate the target variable name.
        if !env::valid_variable_name(&self.variable_name) {
            writeln!(
                context.stderr(),
                "{}: `{}': not a valid identifier",
                context.command_name,
                self.variable_name
            )?;
            return Ok(ExecutionResult::new(1));
        }

        let spec = parse_option_spec(&self.options_string);

        // If unset or non-numeric, assume OPTIND is 1.
        let next_index_signed = context
            .shell
            .env_str("OPTIND")
            .and_then(|s| brush_core::int_utils::parse::<i32>(s.as_ref(), 10).ok())
            .unwrap_or(1);

        // As in Bash, any assignment to OPTIND since the last call (even of the
        // value it had, `OPTIND=1`) restarts parsing at that argument: clear
        // the internal char index so we don't resume mid-cluster.
        let record = optind_record(&context);
        let state = context.state_mut::<GetOptsState>();
        // Only OPTIND=1 (or less) restarts parsing: another value takes effect
        // once the cluster being parsed (`-xyz`) is done, as in Bash.
        if state.last_optind.as_ref() != Some(&record) && next_index_signed <= 1 {
            state.position = None;
        }

        #[allow(clippy::cast_sign_loss)] // .max(1) guarantees positive
        let next_index = next_index_signed.max(1) as usize;

        // Select the arguments to parse. If none were explicitly provided, we
        // default to using the shell's current positional parameters.
        // Clone positional params to avoid borrowing context immutably while we
        // also need it mutably in parse_next_option.
        let owned_args;
        let args_to_parse = if !self.args.is_empty() {
            &self.args
        } else {
            owned_args = context.shell.current_shell_args().to_vec();
            &owned_args
        };

        let result = parse_next_option(&mut context, &spec, args_to_parse, next_index)?;

        update_variables(&mut context, &self.variable_name, result).await
    }
}

/// Extracts the next option from `args_to_parse` starting at 1-based position
/// `next_index`. Handles combined flags (e.g., `-abc`), option arguments (both
/// `-pVALUE` and `-p VALUE` forms), and error reporting for unknown options or
/// missing arguments. Tracks position within combined flags via the
/// builtin's state.
fn parse_next_option<SE: brush_core::ShellExtensions>(
    context: &mut brush_core::ExecutionContext<'_, SE>,
    spec: &OptionSpec,
    args_to_parse: &[String],
    mut next_index: usize,
) -> Result<GetOptsResult, brush_core::Error> {
    // See if there are any args left to parse.
    if next_index > args_to_parse.len() {
        return Ok(GetOptsResult {
            variable_value: String::from("?"),
            optarg: None,
            // Normalize OPTIND to one past the last argument.
            optind: args_to_parse.len() + 1,
            exit_code: ExecutionResult::general_error(),
        });
    }

    // Inside a cluster, the argument is the cluster's, even if OPTIND changed.
    let position = context.state_mut::<GetOptsState>().position;
    let arg_index = position.map_or(next_index, |(arg, _)| arg);
    let Some(arg) = args_to_parse
        .get(arg_index.wrapping_sub(1))
        .map(String::as_str)
    else {
        context.state_mut::<GetOptsState>().position = None;
        return Ok(GetOptsResult {
            variable_value: String::from("?"),
            optarg: None,
            optind: args_to_parse.len() + 1,
            exit_code: ExecutionResult::general_error(),
        });
    };

    // See if this is an option.
    if !arg.starts_with('-') || arg == "--" || arg == "-" {
        // Not an option. If it was "--", skip past it.
        return Ok(GetOptsResult {
            variable_value: String::from("?"),
            optarg: None,
            optind: if arg == "--" {
                next_index + 1
            } else {
                next_index
            },
            exit_code: ExecutionResult::general_error(),
        });
    }

    // Figure out how far into this option we are.
    let mut next_char_index = position.map_or(DEFAULT_NEXT_CHAR_INDEX, |(_, char)| char);

    // Find the char. If the index is stale (exceeds this arg's length),
    // reset to the default index, mirroring bash behavior.
    let mut c = arg.chars().nth(next_char_index);
    if c.is_none() {
        next_char_index = DEFAULT_NEXT_CHAR_INDEX;
        c = arg.chars().nth(next_char_index);
    }
    let Some(c) = c else {
        // Arg is too short even at default index.
        return Ok(GetOptsResult {
            variable_value: String::from("?"),
            optarg: None,
            optind: next_index,
            exit_code: ExecutionResult::general_error(),
        });
    };

    let arg_char_count = arg.chars().count();
    let mut is_last_char_in_option = next_char_index == arg_char_count - 1;

    let mut variable_value;
    let optarg;

    // Look up the char in the option spec.
    if let Some(takes_arg) = spec.defs.get(&c) {
        variable_value = String::from(c);

        if *takes_arg {
            (variable_value, optarg, is_last_char_in_option, next_index) = resolve_option_argument(
                context,
                spec,
                c,
                arg,
                args_to_parse,
                next_char_index,
                is_last_char_in_option,
                next_index,
            )?;
        } else {
            optarg = None;
        }
    } else {
        (variable_value, optarg) = report_unknown_option(context, spec, c)?;
    }

    let optind = if is_last_char_in_option {
        // We're done with this argument: forget our place in it, and move
        // OPTIND on.
        context.state_mut::<GetOptsState>().position = None;
        next_index + 1
    } else {
        // We have more to go in this argument: remember our place, and leave
        // OPTIND where it is.
        context.state_mut::<GetOptsState>().position = Some((arg_index, next_char_index + 1));
        next_index
    };

    Ok(GetOptsResult {
        variable_value,
        optarg,
        optind,
        exit_code: ExecutionResult::success(),
    })
}

/// Resolves the argument for an option that takes a value. Returns the updated
/// `(variable_value, optarg, is_last_char, next_index)` tuple.
#[allow(clippy::too_many_arguments)]
fn resolve_option_argument<SE: brush_core::ShellExtensions>(
    context: &brush_core::ExecutionContext<'_, SE>,
    spec: &OptionSpec,
    c: char,
    arg: &str,
    args_to_parse: &[String],
    next_char_index: usize,
    is_last_char_in_option: bool,
    mut next_index: usize,
) -> Result<(String, Option<String>, bool, usize), brush_core::Error> {
    // If this is the last character in the token, the argument value comes from
    // the next token. Otherwise, the remainder of the current token is the value.
    if is_last_char_in_option {
        if next_index >= args_to_parse.len() {
            // Missing required argument.
            let (variable_value, optarg) = if spec.silent_errors {
                (String::from(":"), Some(String::from(c)))
            } else {
                if is_opterr_enabled(context) {
                    writeln!(
                        context.stderr(),
                        "getopts: option requires an argument -- {c}"
                    )?;
                }
                (String::from("?"), None)
            };
            Ok((variable_value, optarg, true, next_index))
        } else {
            next_index += 1;
            Ok((
                String::from(c),
                Some(args_to_parse[next_index - 1].clone()),
                true,
                next_index,
            ))
        }
    } else {
        let optarg: String = arg.chars().skip(next_char_index + 1).collect();
        Ok((String::from(c), Some(optarg), true, next_index))
    }
}

/// Handles an unknown option character, reporting an error if appropriate.
fn report_unknown_option<SE: brush_core::ShellExtensions>(
    context: &brush_core::ExecutionContext<'_, SE>,
    spec: &OptionSpec,
    c: char,
) -> Result<(String, Option<String>), brush_core::Error> {
    if !spec.silent_errors && is_opterr_enabled(context) {
        writeln!(context.stderr(), "getopts: illegal option -- {c}")?;
    }

    let optarg = if spec.silent_errors {
        Some(String::from(c))
    } else {
        None
    };

    Ok((String::from("?"), optarg))
}

/// Writes the parsing result back into shell variables (the target variable,
/// OPTARG, OPTIND) and remembers the OPTIND set.
async fn update_variables<SE: brush_core::ShellExtensions>(
    context: &mut brush_core::ExecutionContext<'_, SE>,
    variable_name: &str,
    result: GetOptsResult,
) -> Result<ExecutionResult, brush_core::Error> {
    // Update variable value (through a reference to an array element, with its
    // subscript expanded).
    if context.shell.env().nameref_element(variable_name).is_some() {
        brush_core::expansion::assign_to_named_parameter(
            context.shell,
            &context.params,
            variable_name,
            result.variable_value,
        )
        .await?;
    } else {
        context.shell.env_mut().update_or_add(
            variable_name,
            variables::ShellValueLiteral::Scalar(result.variable_value),
            |_| Ok(()),
            brush_core::env::EnvironmentLookup::Anywhere,
            brush_core::env::EnvironmentScope::Global,
        )?;
    }

    // Update OPTARG
    if let Some(optarg) = result.optarg {
        context.shell.env_mut().update_or_add(
            "OPTARG",
            variables::ShellValueLiteral::Scalar(optarg),
            |_| Ok(()),
            brush_core::env::EnvironmentLookup::Anywhere,
            brush_core::env::EnvironmentScope::Global,
        )?;
    } else {
        context.shell.env_mut().unset("OPTARG")?;
    }

    // Update OPTIND and record it so we can detect external modifications.
    let optind_str = result.optind.to_string();
    context.shell.env_mut().update_or_add(
        "OPTIND",
        variables::ShellValueLiteral::Scalar(optind_str),
        |_| Ok(()),
        brush_core::env::EnvironmentLookup::Anywhere,
        brush_core::env::EnvironmentScope::Global,
    )?;
    let record = optind_record(context);
    context.state_mut::<GetOptsState>().last_optind = Some(record);

    Ok(result.exit_code)
}

/// Returns whether OPTERR is enabled (i.e., getopts should print error messages).
/// OPTERR defaults to 1; any nonzero value means errors are enabled.
fn is_opterr_enabled<SE: brush_core::ShellExtensions>(
    context: &brush_core::ExecutionContext<'_, SE>,
) -> bool {
    context
        .shell
        .env_str("OPTERR")
        .is_none_or(|s| s.parse::<i64>().unwrap_or(1) != 0)
}

/// OPTIND's value and assignment stamp, which change when anything assigns it.
fn optind_record<SE: brush_core::ShellExtensions>(
    context: &brush_core::ExecutionContext<'_, SE>,
) -> String {
    context
        .shell
        .env()
        .get("OPTIND")
        .map_or_else(String::new, |(_, var)| {
            std::format!(
                "{}:{}",
                var.value().to_cow_str(context.shell),
                var.assignment_stamp()
            )
        })
}
