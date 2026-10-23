use clap::Parser;
use std::io::Write;

use brush_core::builtins::{Invocation, UsageError};
use brush_core::{ExecutionResult, builtins, escape};

/// Echo text to standard output.
#[derive(Parser)]
#[clap(disable_help_flag = true, disable_version_flag = true)]
pub(crate) struct EchoCommand {
    /// Suppress the trailing newline from the output.
    #[arg(short = 'n')]
    no_trailing_newline: bool,

    /// Interpret backslash escapes in the provided text.
    #[arg(short = 'e')]
    interpret_backslash_escapes: bool,

    /// Do not interpret backslash escapes in the provided text.
    #[arg(short = 'E')]
    no_interpret_backslash_escapes: bool,

    /// Tokens to echo to standard output, options included.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,

    /// How many of `args` are options.
    #[clap(skip)]
    option_words: usize,
}

brush_clap_adapter::impl_parse_args!(EchoCommand, parse = EchoCommand::from_invocation);

impl EchoCommand {
    /// Parses options as Bash's `echo` does: leading words made only of
    /// `-n`, `-e` and `-E` letters, in order (the last of `-e`/`-E` wins);
    /// anything else, `--` included, starts the text. With `xpg_echo` in
    /// posix mode there are no options.
    fn from_invocation(invocation: Invocation<'_>) -> Result<Self, UsageError> {
        let mut this = Self {
            no_trailing_newline: false,
            interpret_backslash_escapes: false,
            no_interpret_backslash_escapes: false,
            args: invocation
                .into_args()
                .into_iter()
                .map(String::from)
                .collect(),
            option_words: 0,
        };
        let mut words = this.args.iter().peekable();
        while let Some(word) = words.next_if(|word| {
            word.len() > 1
                && word.starts_with('-')
                && word[1..].chars().all(|c| matches!(c, 'n' | 'e' | 'E'))
        }) {
            for letter in word[1..].chars() {
                match letter {
                    'n' => this.no_trailing_newline = true,
                    'e' => {
                        this.interpret_backslash_escapes = true;
                        this.no_interpret_backslash_escapes = false;
                    }
                    _ => {
                        this.interpret_backslash_escapes = false;
                        this.no_interpret_backslash_escapes = true;
                    }
                }
            }
            this.option_words += 1;
        }
        Ok(this)
    }
}

impl<SE: brush_core::ShellExtensions> builtins::Builtin<SE> for EchoCommand {
    type Error = brush_core::Error;

    async fn execute(
        self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let xpg_echo = context
            .shell
            .options()
            .echo_builtin_expands_escape_sequences;
        let (text, mut trailing_newline, escapes) =
            if xpg_echo && context.shell.options().posix_mode {
                (self.args.as_slice(), true, true)
            } else {
                (
                    &self.args[self.option_words..],
                    !self.no_trailing_newline,
                    self.interpret_backslash_escapes
                        || (xpg_echo && !self.no_interpret_backslash_escapes),
                )
            };
        let mut s;
        if escapes {
            s = String::new();
            for (i, arg) in text.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }

                let (expanded_arg, keep_going) = escape::expand_backslash_escapes(
                    arg.as_str(),
                    escape::EscapeExpansionMode::EchoBuiltin,
                )?;
                s.push_str(&brush_core::bytes::decode(expanded_arg.as_slice()));

                if !keep_going {
                    trailing_newline = false;
                    break;
                }
            }
        } else {
            s = text.join(" ");
        }

        if trailing_newline {
            s.push('\n');
        }

        write!(context.stdout(), "{s}")?;
        context.stdout().flush()?;

        Ok(ExecutionResult::success())
    }
}
