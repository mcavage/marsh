use brush_parser::ast;
use std::path::Path;

use crate::{
    ExecutionParameters, Shell, ShellFd, arithmetic, env, error, escape, expansion, extensions,
    namedoptions, patterns,
    sys::{
        fs::{MetadataExt, PathExt},
        users,
    },
    variables::{self, ArrayLiteral},
};

/// Evaluates `[[ ... ]]` to its status: 0 or 1, or 2 for an invalid regular
/// expression, which `&&` and `||` pass on as Bash's `execute_cond_node` does.
#[async_recursion::async_recursion]
pub(crate) async fn eval_extended_test_expr(
    expr: &ast::ExtendedTestExpr,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<u8, error::Error> {
    let status = |result: bool| if result { 0 } else { 1 };
    match expr {
        ast::ExtendedTestExpr::UnaryTest(op, operand) => Ok(status(
            apply_unary_predicate(op, operand, shell, params).await?,
        )),
        ast::ExtendedTestExpr::BinaryTest(
            ast::BinaryPredicate::StringMatchesRegex
            | ast::BinaryPredicate::StringContainsSubstring,
            left,
            right,
        ) => match_regex(left, right, shell, params).await,
        ast::ExtendedTestExpr::BinaryTest(op, left, right) => Ok(status(
            apply_binary_predicate(op, left, right, shell, params).await?,
        )),
        ast::ExtendedTestExpr::And(left, right) => {
            match eval_extended_test_expr(left, shell, params).await? {
                0 => eval_extended_test_expr(right, shell, params).await,
                failed => Ok(failed),
            }
        }
        ast::ExtendedTestExpr::Or(left, right) => {
            match eval_extended_test_expr(left, shell, params).await? {
                0 => Ok(0),
                _ => eval_extended_test_expr(right, shell, params).await,
            }
        }
        ast::ExtendedTestExpr::Not(expr) => Ok(status(
            eval_extended_test_expr(expr, shell, params).await? != 0,
        )),
        ast::ExtendedTestExpr::Parenthesized(expr) => {
            eval_extended_test_expr(expr, shell, params).await
        }
    }
}

async fn apply_unary_predicate(
    op: &ast::UnaryPredicate,
    operand: &ast::Word,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<bool, error::Error> {
    let expanded_operand = expansion::basic_expand_word(shell, params, operand).await?;

    if shell.options().print_commands_and_arguments {
        shell
            .trace_command(
                params,
                std::format!(
                    "[[ {op} {} ]]",
                    escape::quote_if_needed(&expanded_operand, escape::QuoteMode::SingleQuote)
                ),
            )
            .await;
    }

    apply_unary_predicate_to_str(op, expanded_operand.as_str(), shell, params)
}

#[expect(clippy::too_many_lines)]
pub(crate) fn apply_unary_predicate_to_str(
    op: &ast::UnaryPredicate,
    operand: &str,
    shell: &Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<bool, error::Error> {
    match op {
        ast::UnaryPredicate::StringHasNonZeroLength => Ok(!operand.is_empty()),
        ast::UnaryPredicate::StringHasZeroLength => Ok(operand.is_empty()),
        ast::UnaryPredicate::FileExists => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.exists())
        }
        ast::UnaryPredicate::FileExistsAndIsBlockSpecialFile => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.exists_and_is_block_device())
        }
        ast::UnaryPredicate::FileExistsAndIsCharSpecialFile => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.exists_and_is_char_device())
        }
        ast::UnaryPredicate::FileExistsAndIsDir => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.is_dir())
        }
        ast::UnaryPredicate::FileExistsAndIsRegularFile => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.is_file())
        }
        ast::UnaryPredicate::FileExistsAndIsSetgid => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.exists_and_is_setgid())
        }
        ast::UnaryPredicate::FileExistsAndIsSymlink => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.is_symlink())
        }
        ast::UnaryPredicate::FileExistsAndHasStickyBit => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.exists_and_is_sticky_bit())
        }
        ast::UnaryPredicate::FileExistsAndIsFifo => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.exists_and_is_fifo())
        }
        ast::UnaryPredicate::FileExistsAndIsReadable => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.readable())
        }
        ast::UnaryPredicate::FileExistsAndIsNotZeroLength => {
            let path = shell.absolute_path(Path::new(operand));
            if let Ok(metadata) = path.metadata() {
                Ok(metadata.len() > 0)
            } else {
                Ok(false)
            }
        }
        ast::UnaryPredicate::FdIsOpenTerminal => {
            // Trim whitespace before parsing, matching bash behavior.
            if let Ok(fd) = operand.trim().parse::<ShellFd>() {
                if let Some(open_file) = params.try_fd(shell, fd) {
                    Ok(open_file.is_terminal())
                } else {
                    Ok(false)
                }
            } else {
                Ok(false)
            }
        }
        ast::UnaryPredicate::FileExistsAndIsSetuid => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.exists_and_is_setuid())
        }
        ast::UnaryPredicate::FileExistsAndIsWritable => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.writable())
        }
        ast::UnaryPredicate::FileExistsAndIsExecutable => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.executable())
        }
        ast::UnaryPredicate::FileExistsAndOwnedByEffectiveGroupId => {
            let path = shell.absolute_path(Path::new(operand));
            if !path.exists() {
                return Ok(false);
            }

            let md = path.metadata()?;
            Ok(md.gid() == users::get_effective_gid()?)
        }
        ast::UnaryPredicate::FileExistsAndModifiedSinceLastRead => {
            error::unimp("unary extended test predicate: FileExistsAndModifiedSinceLastRead")
        }
        ast::UnaryPredicate::FileExistsAndOwnedByEffectiveUserId => {
            let path = shell.absolute_path(Path::new(operand));
            if !path.exists() {
                return Ok(false);
            }

            let md = path.metadata()?;
            Ok(md.uid() == users::get_effective_uid()?)
        }
        ast::UnaryPredicate::FileExistsAndIsSocket => {
            let path = shell.absolute_path(Path::new(operand));
            Ok(path.exists_and_is_socket())
        }
        ast::UnaryPredicate::ShellOptionEnabled => {
            let shopt_name = operand;
            if let Some(option) =
                namedoptions::options(namedoptions::ShellOptionKind::SetO).get(shopt_name)
            {
                Ok(option.get(shell.options()))
            } else {
                Ok(false)
            }
        }
        ast::UnaryPredicate::ShellVariableIsSetAndAssigned => Ok(shell.env().is_set(operand)),
        ast::UnaryPredicate::ShellVariableIsSetAndNameRef => match shell.env().get_raw(operand) {
            Some((_, reffed)) => Ok(reffed.value().is_set() && reffed.is_treated_as_nameref()),
            None => Ok(false),
        },
    }
}

/// `[[ left =~ right ]]`: 0 for a match, 1 for none, 2 for an invalid regular
/// expression. (The parser gives a pattern that starts with a quote its own
/// predicate, but it is a regex whose quoted parts match literally.) A match
/// sets `BASH_REMATCH`, no match empties it, and an invalid one leaves it.
async fn match_regex(
    left: &ast::Word,
    right: &ast::Word,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<u8, error::Error> {
    let s = expansion::basic_expand_word(shell, params, left).await?;
    let regex = expansion::basic_expand_regex(shell, params, right)
        .await?
        .set_multiline(true)
        .set_case_insensitive(shell.options().case_insensitive_conditionals);
    let pattern = regex.bash_pattern();

    if shell.options().print_commands_and_arguments {
        shell
            .trace_command(params, std::format!("[[ {s} =~ {pattern} ]]"))
            .await;
    }

    #[cfg(unix)]
    let captures = match regex.posix_matches(
        &s,
        &shell.locale_category("LC_CTYPE"),
        &shell.locale_category("LC_COLLATE"),
    ) {
        crate::regex::PosixMatch::Matched(captures) => captures,
        crate::regex::PosixMatch::NoMatch => vec![],
        crate::regex::PosixMatch::Invalid(reason) => {
            let message = std::format!(
                "{}[[: invalid regular expression `{pattern}': {reason}\n",
                shell.diagnostic_prefix()
            );
            let _ = std::io::Write::write_all(
                &mut params.stderr(shell),
                &crate::bytes::encode(&message),
            );
            return Ok(2);
        }
    };
    #[cfg(not(unix))]
    let captures = match regex.matches(s.as_str()) {
        Ok(captures) => captures.unwrap_or_default(),
        Err(e) => {
            tracing::warn!("error using regex: {}", e);
            return Ok(2);
        }
    };

    let matched = !captures.is_empty();
    let captures_value = variables::ShellValueLiteral::Array(ArrayLiteral(
        captures
            .into_iter()
            .map(|c| (None, c.unwrap_or_default()))
            .collect(),
    ));
    shell.env_mut().update_or_add(
        "BASH_REMATCH",
        captures_value,
        |_| Ok(()),
        env::EnvironmentLookup::Anywhere,
        env::EnvironmentScope::Global,
    )?;
    Ok(if matched { 0 } else { 1 })
}

#[expect(clippy::too_many_lines)]
async fn apply_binary_predicate(
    op: &ast::BinaryPredicate,
    left: &ast::Word,
    right: &ast::Word,
    shell: &mut Shell<impl extensions::ShellExtensions>,
    params: &ExecutionParameters,
) -> Result<bool, error::Error> {
    match op {
        ast::BinaryPredicate::StringMatchesRegex
        | ast::BinaryPredicate::StringContainsSubstring => {
            Ok(match_regex(left, right, shell, params).await? == 0)
        }
        ast::BinaryPredicate::StringExactlyMatchesString => {
            let left = expansion::basic_expand_word(shell, params, left).await?;
            let right = expansion::basic_expand_word(shell, params, right).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            Ok(left == right)
        }
        ast::BinaryPredicate::StringDoesNotExactlyMatchString => {
            let left = expansion::basic_expand_word(shell, params, left).await?;
            let right = expansion::basic_expand_word(shell, params, right).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            Ok(left != right)
        }
        ast::BinaryPredicate::FilesReferToSameDeviceAndInodeNumbers => {
            let left = expansion::basic_expand_word(shell, params, left).await?;
            let right = expansion::basic_expand_word(shell, params, right).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            files_refer_to_same_device_and_inode_numbers(shell, left, right)
        }
        ast::BinaryPredicate::LeftFileIsNewerOrExistsWhenRightDoesNot => {
            let left = expansion::basic_expand_word(shell, params, left).await?;
            let right = expansion::basic_expand_word(shell, params, right).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            left_file_is_newer_or_exists_when_right_does_not(shell, left, right)
        }
        ast::BinaryPredicate::LeftFileIsOlderOrDoesNotExistWhenRightDoes => {
            let left = expansion::basic_expand_word(shell, params, left).await?;
            let right = expansion::basic_expand_word(shell, params, right).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            left_file_is_older_or_does_not_exist_when_right_does(shell, left, right)
        }
        ast::BinaryPredicate::LeftSortsBeforeRight => {
            let left = expansion::basic_expand_word(shell, params, left).await?;
            let right = expansion::basic_expand_word(shell, params, right).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            // TODO(test): According to docs, should be lexicographical order of the current locale.
            Ok(left < right)
        }
        ast::BinaryPredicate::LeftSortsAfterRight => {
            let left = expansion::basic_expand_word(shell, params, left).await?;
            let right = expansion::basic_expand_word(shell, params, right).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            // TODO(test): According to docs, should be lexicographical order of the current locale.
            Ok(left > right)
        }
        ast::BinaryPredicate::ArithmeticEqualTo => {
            let left =
                arithmetic::expand_and_eval(shell, params, left.value.as_str(), false).await?;
            let right =
                arithmetic::expand_and_eval(shell, params, right.value.as_str(), false).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            Ok(left == right)
        }
        ast::BinaryPredicate::ArithmeticNotEqualTo => {
            let left =
                arithmetic::expand_and_eval(shell, params, left.value.as_str(), false).await?;
            let right =
                arithmetic::expand_and_eval(shell, params, right.value.as_str(), false).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            Ok(left != right)
        }
        ast::BinaryPredicate::ArithmeticLessThan => {
            let left =
                arithmetic::expand_and_eval(shell, params, left.value.as_str(), false).await?;
            let right =
                arithmetic::expand_and_eval(shell, params, right.value.as_str(), false).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            Ok(left < right)
        }
        ast::BinaryPredicate::ArithmeticLessThanOrEqualTo => {
            let left =
                arithmetic::expand_and_eval(shell, params, left.value.as_str(), false).await?;
            let right =
                arithmetic::expand_and_eval(shell, params, right.value.as_str(), false).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            Ok(left <= right)
        }
        ast::BinaryPredicate::ArithmeticGreaterThan => {
            let left =
                arithmetic::expand_and_eval(shell, params, left.value.as_str(), false).await?;
            let right =
                arithmetic::expand_and_eval(shell, params, right.value.as_str(), false).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            Ok(left > right)
        }
        ast::BinaryPredicate::ArithmeticGreaterThanOrEqualTo => {
            let left =
                arithmetic::expand_and_eval(shell, params, left.value.as_str(), false).await?;
            let right =
                arithmetic::expand_and_eval(shell, params, right.value.as_str(), false).await?;

            if shell.options().print_commands_and_arguments {
                shell
                    .trace_command(params, std::format!("[[ {left} {op} {right} ]]"))
                    .await;
            }

            Ok(left >= right)
        }
        // N.B. The "=", "==", and "!=" operators don't compare 2 strings; they check
        // for whether the lefthand operand (a string) is matched by the righthand
        // operand (treated as a shell pattern).
        // TODO(test): implement case-insensitive matching if relevant via shopt options
        // (nocasematch).
        // As in Bash, `[[ == ]]` and `[[ != ]]` patterns are extended globs
        // whether or not `extglob` is set.
        ast::BinaryPredicate::StringExactlyMatchesPattern => {
            let s = expansion::basic_expand_word(shell, params, left).await?;
            let pattern = expansion::basic_expand_pattern(shell, params, right)
                .await?
                .set_extended_globbing(true)
                .set_case_insensitive(shell.options().case_insensitive_conditionals);

            if shell.options().print_commands_and_arguments {
                let expanded_right = expansion::basic_expand_word(shell, params, right).await?;
                let escaped_right = escape::quote_if_needed(
                    expanded_right.as_str(),
                    escape::QuoteMode::BackslashEscape,
                );
                shell
                    .trace_command(params, std::format!("[[ {s} {op} {escaped_right} ]]"))
                    .await;
            }

            pattern.exactly_matches(s.as_str())
        }
        ast::BinaryPredicate::StringDoesNotExactlyMatchPattern => {
            let s = expansion::basic_expand_word(shell, params, left).await?;
            let pattern = expansion::basic_expand_pattern(shell, params, right)
                .await?
                .set_extended_globbing(true)
                .set_case_insensitive(shell.options().case_insensitive_conditionals);

            if shell.options().print_commands_and_arguments {
                let expanded_right = expansion::basic_expand_word(shell, params, right).await?;
                let escaped_right = escape::quote_if_needed(
                    expanded_right.as_str(),
                    escape::QuoteMode::BackslashEscape,
                );
                shell
                    .trace_command(params, std::format!("[[ {s} {op} {escaped_right} ]]"))
                    .await;
            }

            let eq = pattern.exactly_matches(s.as_str())?;
            Ok(!eq)
        }
    }
}

pub(crate) fn apply_binary_predicate_to_strs(
    op: &ast::BinaryPredicate,
    left: &str,
    right: &str,
    shell: &Shell<impl extensions::ShellExtensions>,
) -> Result<bool, error::Error> {
    match op {
        ast::BinaryPredicate::FilesReferToSameDeviceAndInodeNumbers => {
            files_refer_to_same_device_and_inode_numbers(shell, left, right)
        }
        ast::BinaryPredicate::LeftFileIsNewerOrExistsWhenRightDoesNot => {
            left_file_is_newer_or_exists_when_right_does_not(shell, left, right)
        }
        ast::BinaryPredicate::LeftFileIsOlderOrDoesNotExistWhenRightDoes => {
            left_file_is_older_or_does_not_exist_when_right_does(shell, left, right)
        }
        ast::BinaryPredicate::LeftSortsBeforeRight => {
            // TODO(test): According to docs, should be lexicographical order of the current locale.
            Ok(left < right)
        }
        ast::BinaryPredicate::LeftSortsAfterRight => {
            // TODO(test): According to docs, should be lexicographical order of the current locale.
            Ok(left > right)
        }
        ast::BinaryPredicate::ArithmeticEqualTo => Ok(apply_test_binary_arithmetic_predicate(
            left,
            right,
            |left, right| left == right,
        )),
        ast::BinaryPredicate::ArithmeticNotEqualTo => Ok(apply_test_binary_arithmetic_predicate(
            left,
            right,
            |left, right| left != right,
        )),
        ast::BinaryPredicate::ArithmeticLessThan => Ok(apply_test_binary_arithmetic_predicate(
            left,
            right,
            |left, right| left < right,
        )),
        ast::BinaryPredicate::ArithmeticLessThanOrEqualTo => Ok(
            apply_test_binary_arithmetic_predicate(left, right, |left, right| left <= right),
        ),
        ast::BinaryPredicate::ArithmeticGreaterThan => Ok(apply_test_binary_arithmetic_predicate(
            left,
            right,
            |left, right| left > right,
        )),
        ast::BinaryPredicate::ArithmeticGreaterThanOrEqualTo => Ok(
            apply_test_binary_arithmetic_predicate(left, right, |left, right| left >= right),
        ),
        ast::BinaryPredicate::StringExactlyMatchesPattern => {
            let pattern = patterns::Pattern::from(right)
                .set_extended_globbing(shell.options().extended_globbing)
                .set_case_insensitive(shell.options().case_insensitive_conditionals);

            pattern.exactly_matches(left)
        }
        ast::BinaryPredicate::StringDoesNotExactlyMatchPattern => {
            let pattern = patterns::Pattern::from(right)
                .set_extended_globbing(shell.options().extended_globbing)
                .set_case_insensitive(shell.options().case_insensitive_conditionals);

            let eq = pattern.exactly_matches(left)?;
            Ok(!eq)
        }
        ast::BinaryPredicate::StringExactlyMatchesString => Ok(left == right),
        ast::BinaryPredicate::StringDoesNotExactlyMatchString => Ok(left != right),
        _ => error::unimp("unsupported test binary predicate"),
    }
}

fn apply_test_binary_arithmetic_predicate(
    left: &str,
    right: &str,
    op: fn(i64, i64) -> bool,
) -> bool {
    // We trim leading/trailing whitespace (including newlines) before parsing integers.
    let left: Result<i64, _> = left.trim().parse();
    let right: Result<i64, _> = right.trim().parse();

    if let (Ok(left), Ok(right)) = (left, right) {
        op(left, right)
    } else {
        false
    }
}

fn left_file_is_older_or_does_not_exist_when_right_does(
    shell: &Shell<impl extensions::ShellExtensions>,
    left: impl AsRef<str>,
    right: impl AsRef<str>,
) -> Result<bool, error::Error> {
    let (l_path, r_path) = (
        shell.absolute_path(Path::new(left.as_ref())),
        shell.absolute_path(Path::new(right.as_ref())),
    );

    match (l_path.metadata(), r_path.metadata()) {
        (Ok(m1), Ok(m2)) => Ok(m1.modified()? < m2.modified()?),
        (Err(_), Ok(_)) => Ok(true),
        _ => Ok(false),
    }
}

fn left_file_is_newer_or_exists_when_right_does_not(
    shell: &Shell<impl extensions::ShellExtensions>,
    left: impl AsRef<str>,
    right: impl AsRef<str>,
) -> Result<bool, error::Error> {
    let (l_path, r_path) = (
        shell.absolute_path(Path::new(left.as_ref())),
        shell.absolute_path(Path::new(right.as_ref())),
    );

    match (l_path.metadata(), r_path.metadata()) {
        (Ok(m1), Ok(m2)) => Ok(m1.modified()? > m2.modified()?),
        (Ok(_), Err(_)) => Ok(true),
        _ => Ok(false),
    }
}

fn files_refer_to_same_device_and_inode_numbers(
    shell: &Shell<impl extensions::ShellExtensions>,
    left: impl AsRef<str>,
    right: impl AsRef<str>,
) -> Result<bool, error::Error> {
    let (l_path, r_path) = (
        shell.absolute_path(Path::new(left.as_ref())),
        shell.absolute_path(Path::new(right.as_ref())),
    );

    if !l_path.readable() || !r_path.readable() {
        return Ok(false);
    }

    Ok(l_path.get_device_and_inode()? == r_path.get_device_and_inode()?)
}
