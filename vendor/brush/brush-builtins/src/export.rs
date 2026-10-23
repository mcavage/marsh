use clap::Parser;
use itertools::Itertools;
use std::io::Write;

use brush_core::{
    ExecutionResult, builtins,
    env::{EnvironmentLookup, EnvironmentScope},
    parser::ast,
    variables,
};

/// Add or update exported shell variables.
#[derive(Parser)]
pub(crate) struct ExportCommand {
    /// Names are treated as function names.
    #[arg(short = 'f')]
    names_are_functions: bool,

    /// Un-export the names.
    #[arg(short = 'n')]
    unexport: bool,

    /// Display all exported names.
    #[arg(short = 'p')]
    display_exported_names: bool,

    //
    // Declarations
    //
    // N.B. These are skipped by clap, but filled in by `impl_parse_args!`.
    #[clap(skip)]
    declarations: Vec<brush_core::CommandArg>,
}

brush_clap_adapter::impl_parse_args!(ExportCommand, declarations = declarations);

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for ExportCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        mut context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        if self.declarations.is_empty() {
            display_all_exported_vars(&context)?;
            return Ok(ExecutionResult::success());
        }

        let mut result = ExecutionResult::success();
        for decl in &self.declarations {
            let current_result = self.process_decl(&mut context, decl)?;
            if !current_result.is_success() {
                result = current_result;
            }
        }

        Ok(result)
    }
}

impl ExportCommand {
    fn process_decl(
        &self,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        decl: &brush_core::CommandArg,
    ) -> Result<ExecutionResult, brush_core::Error> {
        match decl {
            // A word that only became `name=value` when expanded (`export "$1"`)
            // is still an assignment, as in Bash.
            brush_core::CommandArg::String(s) if !self.names_are_functions && s.contains('=') => {
                let (name, value) = s.split_once('=').unwrap_or_default();
                let (name, append) = match name.strip_suffix('+') {
                    Some(name) => (name, true),
                    None => (name, false),
                };
                if !brush_core::env::valid_variable_name(name) {
                    // Bash names a subscripted reference without its value.
                    let element = name.split_once('[').is_some_and(|(array, rest)| {
                        brush_core::env::valid_variable_name(array) && rest.ends_with(']')
                    });
                    return self.invalid_identifier(context, if element { name } else { s });
                }
                self.assign(
                    context,
                    name,
                    variables::ShellValueLiteral::Scalar(value.to_owned()),
                    append,
                )
            }
            brush_core::CommandArg::String(s) => {
                // See if this is supposed to be a function name.
                if self.names_are_functions {
                    // Try to find the function already present; if we find it, then mark it
                    // exported.
                    if let Some(func) = context.shell.func_mut(s) {
                        if self.unexport {
                            func.unexport();
                        } else {
                            func.export();
                        }
                    } else {
                        writeln!(
                            context.stderr(),
                            "{}{}: {s}: not a function",
                            context.shell.diagnostic_prefix(),
                            context.command_name
                        )?;
                        return Ok(ExecutionResult::general_error());
                    }
                } else if !brush_core::env::valid_variable_name(s) {
                    return self.invalid_identifier(context, s);
                }
                // Try to find the variable already present; if we find it, then mark it
                // exported.
                else if let Some((_, variable)) = context.shell.env_mut().get_mut(s) {
                    if self.unexport {
                        variable.unexport();
                    } else {
                        variable.export();
                    }
                }
                Ok(ExecutionResult::success())
            }
            brush_core::CommandArg::Assignment(assignment) => {
                let name = match &assignment.name {
                    ast::AssignmentName::VariableName(name) => name,
                    ast::AssignmentName::ArrayElementName(name, index) => {
                        return self.invalid_identifier(context, &std::format!("{name}[{index}]"));
                    }
                };

                let value = match &assignment.value {
                    ast::AssignmentValue::Scalar(s) => {
                        variables::ShellValueLiteral::Scalar(s.flatten())
                    }
                    ast::AssignmentValue::Array(a) => {
                        variables::ShellValueLiteral::Array(variables::ArrayLiteral::new(
                            a.iter()
                                .map(|(k, v)| (k.as_ref().map(|k| k.flatten()), v.flatten()))
                                .collect(),
                        ))
                    }
                };
                self.assign(context, name, value, assignment.append)
            }
        }
    }

    /// Bash's message for a word that names no variable; `export` goes on.
    fn invalid_identifier(
        &self,
        context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        word: &str,
    ) -> Result<ExecutionResult, brush_core::Error> {
        writeln!(
            context.stderr(),
            "{}{}: `{word}': not a valid identifier",
            context.shell.diagnostic_prefix(),
            context.command_name
        )?;
        Ok(ExecutionResult::general_error())
    }

    fn assign(
        &self,
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        name: &str,
        value: variables::ShellValueLiteral,
        append: bool,
    ) -> Result<ExecutionResult, brush_core::Error> {
        // Like Bash, a reference to an array element assigns the element, which
        // cannot be exported.
        if let Some((array, index)) = context.shell.env().nameref_element(name) {
            context.shell.env_mut().update_or_add(
                name,
                value,
                |_| Ok(()),
                EnvironmentLookup::Anywhere,
                EnvironmentScope::Global,
            )?;
            writeln!(
                context.stderr(),
                "{}{}: `{array}[{index}]': not a valid identifier",
                context.shell.diagnostic_prefix(),
                context.command_name
            )?;
            return Ok(ExecutionResult::success());
        }
        if context
            .shell
            .env()
            .get(name)
            .is_some_and(|(_, variable)| variable.is_readonly())
        {
            writeln!(
                context.stderr(),
                "{}{name}: readonly variable",
                context.shell.diagnostic_prefix()
            )?;
            return Ok(ExecutionResult::general_error());
        }

        // `export name+=value` appends to the existing value, exactly like a
        // bare `name+=value`. update_or_add always replaces, so when the
        // variable already exists honor the append here. A missing variable
        // falls through: appending to nothing is a plain assignment.
        if append && let Some((_, variable)) = context.shell.env_mut().get_mut(name) {
            variable.assign(value, true)?;
            if self.unexport {
                variable.unexport();
            } else {
                variable.export();
            }
            return Ok(ExecutionResult::success());
        }

        // Update the variable with the provided value and then mark it exported.
        context.shell.env_mut().update_or_add(
            name,
            value,
            |var| {
                if self.unexport {
                    var.unexport();
                } else {
                    var.export();
                }
                Ok(())
            },
            EnvironmentLookup::Anywhere,
            EnvironmentScope::Global,
        )?;
        Ok(ExecutionResult::success())
    }
}

fn display_all_exported_vars(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
) -> Result<(), brush_core::Error> {
    // Enumerate variables, sorted by key.
    for (name, variable) in context.shell.env().iter().sorted_by_key(|v| v.0) {
        if variable.is_exported() {
            let value = variable.value().try_get_cow_str(context.shell);
            if let Some(value) = value {
                writeln!(context.stdout(), "declare -x {name}=\"{value}\"")?;
            } else {
                writeln!(context.stdout(), "declare -x {name}")?;
            }
        }
    }

    Ok(())
}
