use clap::Parser;
use std::io::Write;

use brush_core::{
    ExecutionExitCode, ExecutionResult, builtins,
    jobs::{self, JobSpecMatch},
};

const USAGE: &str = "disown [-h] [-ar] [jobspec ... | pid ...]";

const HELP: &str = concat!(
    "    Remove jobs from current shell.\n",
    "    \n",
    "    Removes each JOBSPEC argument from the table of active jobs.  Without\n",
    "    any JOBSPECs, the shell uses its notion of the current job.\n",
    "    \n",
    "    Options:\n",
    "      -a\tremove all jobs if JOBSPEC is not supplied\n",
    "      -h\tmark each JOBSPEC so that SIGHUP is not sent to the job if the\n",
    "    \t\tshell receives a SIGHUP\n",
    "      -r\tremove only running jobs\n",
    "    \n",
    "    Exit Status:\n",
    "    Returns success unless an invalid option or JOBSPEC is given.\n",
);

/// Remove jobs from current shell.
#[derive(Parser)]
#[clap(disable_help_flag = true)]
pub(crate) struct DisownCommand {
    /// Options (-a, -h, -r) and job specs or process IDs, parsed as Bash does.
    #[clap(allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for DisownCommand {
    type Error = brush_core::Error;

    /// Takes the words as given: Bash parses `disown` options itself, and
    /// clap would drop a `--`.
    fn new<I>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = String>,
    {
        Ok(Self {
            args: args.into_iter().skip(1).collect(),
        })
    }

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let prefix = std::format!(
            "{}{}: ",
            context.shell.diagnostic_prefix(),
            context.command_name
        );
        let (mut all, mut nohup, mut running) = (false, false, false);
        let mut operands = self.args.as_slice();
        // Bash's internal_getopt: options end at `--` or the first word that
        // is not `-x...`.
        while let Some(word) = operands.first() {
            if word == "--help" {
                write!(
                    context.stdout(),
                    "{}: {USAGE}\n{HELP}",
                    context.command_name
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
            if word == "--" {
                operands = &operands[1..];
                break;
            }
            let Some(letters) = word.strip_prefix('-').filter(|l| !l.is_empty()) else {
                break;
            };
            for letter in letters.chars() {
                match letter {
                    'a' => all = true,
                    'h' => nohup = true,
                    'r' => running = true,
                    other => {
                        writeln!(context.stderr(), "{prefix}-{other}: invalid option")?;
                        writeln!(context.stderr(), "{}: usage: {USAGE}", context.command_name)?;
                        return Ok(ExecutionExitCode::InvalidUsage.into());
                    }
                }
            }
            operands = &operands[1..];
        }

        let jobs = context.shell.jobs_mut();
        if operands.is_empty() && (all || running) {
            if nohup {
                jobs.set_nohup_all(running);
            } else {
                jobs.disown_all(running);
            }
            return Ok(ExecutionResult::success());
        }

        let mut result = ExecutionResult::success();
        let operands = if operands.is_empty() {
            vec![None]
        } else {
            operands.iter().map(|o| Some(o.as_str())).collect()
        };
        for operand in operands {
            if let Some(word) = operand {
                if !word.is_empty() && !word.starts_with('%') && jobs::parse_pid(word).is_none() {
                    writeln!(
                        context.stderr(),
                        "{prefix}warning: {word}: job specification requires leading `%'"
                    )?;
                }
            }
            let name = operand.unwrap_or("current");
            match context.shell.jobs().find_for_disown(operand) {
                JobSpecMatch::Found(key) if nohup => context.shell.jobs_mut().set_nohup(key),
                JobSpecMatch::Found(key) => {
                    if let Some(job) = context.shell.jobs().get(key) {
                        if matches!(job.state, jobs::JobState::Stopped)
                            && context.shell.depth() == 0
                        {
                            writeln!(
                                context.stderr(),
                                "{}warning: deleting stopped job {} with process group {}",
                                context.shell.diagnostic_prefix(),
                                job.id,
                                job.process_group_id().unwrap_or_default()
                            )?;
                        }
                    }
                    context.shell.jobs_mut().disown(key);
                }
                JobSpecMatch::Ambiguous => {
                    let spec = name.strip_prefix('%').unwrap_or(name);
                    let spec = spec.strip_prefix('?').unwrap_or(spec);
                    writeln!(context.stderr(), "{prefix}{spec}: ambiguous job spec")?;
                    writeln!(context.stderr(), "{prefix}{name}: no such job")?;
                    result = ExecutionResult::general_error();
                }
                JobSpecMatch::NoSuchJob => {
                    writeln!(context.stderr(), "{prefix}{name}: no such job")?;
                    result = ExecutionResult::general_error();
                }
            }
        }
        Ok(result)
    }
}
