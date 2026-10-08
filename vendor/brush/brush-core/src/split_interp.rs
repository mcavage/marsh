//! marsh `split { ... } | join` execution: a thin client of the embedding's
//! split provider (the marsh daemon runs, captures, and keeps every branch).

use std::io::Write;

use brush_parser::ast::{self, CommandPrefixOrSuffixItem};

use crate::interp::{
    CompositionFile, composition_input, execute_composition_segment, setup_redirect,
};
use crate::openfiles::OpenFiles;
use crate::split::SplitBranchRequest;
use crate::{ExecutionExitCode, ExecutionParameters, ExecutionResult, Shell, error, extensions};

/// Where the split and its join sit in a pipeline.
#[derive(Clone, Copy)]
pub(crate) struct SplitShape {
    split: usize,
    join: Option<usize>,
}

/// Recognizes a pipeline with a `split` stage. The parser already rejected
/// mixing it with another composition stage.
pub(crate) fn split_shape(pipeline: &ast::Pipeline) -> Option<SplitShape> {
    let split = pipeline
        .seq
        .iter()
        .position(|command| matches!(command, ast::Command::Split(_)))?;
    let join = pipeline
        .seq
        .get(split + 1)
        .filter(|command| command.is_split_join_stage())
        .map(|_| split + 1);
    Some(SplitShape { split, join })
}

const JOIN_HELP: &str = "join - receive split results\n\nUsage: split { ... } | join [--json] [--keep] [| COMMAND ...]\n\n  --json    the manifest instead of the text rendering\n  --keep    keep the split even when the pipeline succeeds\n\njoin prints the rendering. Later pipeline stages read it on stdin with\nSPLIT_ID, SPLIT_DIR (the out/ directory), SPLIT_MANIFEST and SPLIT_OBJECTS\nexported, from the original directory. Use `command join` for coreutils join.\n";

#[derive(Default)]
struct JoinStage {
    json: bool,
    keep: bool,
    help: bool,
    redirects: Vec<ast::IoRedirect>,
}

/// Reads `join` options and redirections. `join` takes no command: later
/// pipeline stages consume its output (`split { ... } | join | CMD`).
fn parse_join_stage(command: Option<&ast::Command>) -> Result<JoinStage, String> {
    let mut stage = JoinStage::default();
    let Some(ast::Command::Simple(simple)) = command else {
        return Ok(stage);
    };
    for item in simple.suffix.iter().flat_map(|suffix| suffix.0.iter()) {
        match item {
            CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
                stage.redirects.push(redirect.clone());
            }
            CommandPrefixOrSuffixItem::Word(word) => match word.value.as_str() {
                "--json" => stage.json = true,
                "--keep" => stage.keep = true,
                "-h" | "--help" => stage.help = true,
                option if option.starts_with('-') => {
                    return Err(format!(
                        "join: unknown option: {option} (use `command join` for coreutils join)"
                    ));
                }
                word => {
                    return Err(format!(
                        "join: takes no command (got `{word}`); pipe into it: split {{ ... }} | join | {word} ... (use `command join` for coreutils join)"
                    ));
                }
            },
            CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
                return Err(format!(
                    "join: takes no command (got `{}`); pipe into it: split {{ ... }} | join | CMD",
                    word.value
                ));
            }
            CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
                return Err("join: takes no command or process substitution".into());
            }
        }
    }
    Ok(stage)
}

/// Exports the split context into a command-scope environment for the stages
/// after `join`, so every downstream stage (function, builtin, Kit command,
/// or PATH program) sees it. The scope is popped when the pipeline finishes.
fn push_split_environment<SE: extensions::ShellExtensions>(
    shell: &mut Shell<SE>,
    environment: &[(String, String)],
) -> Result<(), error::Error> {
    shell
        .env_mut()
        .push_scope(crate::env::EnvironmentScope::Command);
    for (name, value) in environment {
        let mut variable = crate::ShellVariable::new(value.as_str());
        variable.export();
        shell.env_mut().add(
            name.as_str(),
            variable,
            crate::env::EnvironmentScope::Command,
        )?;
    }
    Ok(())
}

fn settle<SE: extensions::ShellExtensions>(
    shell: &mut Shell<SE>,
    pipeline: &ast::Pipeline,
    statuses: Vec<u8>,
    status: u8,
) -> ExecutionResult {
    let mut result = ExecutionResult::new(status);
    if pipeline.bang {
        result.exit_code = ExecutionExitCode::from(u8::from(status == 0));
    }
    *shell.last_pipeline_statuses_mut() = statuses;
    shell.set_last_exit_status(result.exit_code.into());
    result
}

/// Status for every stage that did not run (split, join if present, and the
/// stages after them), so `PIPESTATUS` keeps one entry per pipeline stage.
fn fail_stages(statuses: &mut Vec<u8>, pipeline: &ast::Pipeline, status: u8) {
    statuses.resize(statuses.len().max(pipeline.seq.len()), status);
}

fn write_lines<SE: extensions::ShellExtensions>(
    shell: &Shell<SE>,
    params: &ExecutionParameters,
    lines: &[String],
) -> Result<(), error::Error> {
    let mut stderr = params.stderr(shell);
    for line in lines {
        writeln!(stderr, "split: {line}")?;
    }
    Ok(())
}

pub(crate) async fn execute_split_pipeline<SE: extensions::ShellExtensions>(
    pipeline: &ast::Pipeline,
    shape: SplitShape,
    shell: &mut Shell<SE>,
    params: &ExecutionParameters,
) -> Result<ExecutionResult, error::Error> {
    let mut statuses = Vec::new();
    let join = match parse_join_stage(shape.join.map(|index| &pipeline.seq[index])) {
        Ok(join) => join,
        Err(message) => {
            writeln!(params.stderr(shell), "{message}")?;
            fail_stages(&mut statuses, pipeline, 2);
            return Ok(settle(shell, pipeline, statuses, 2));
        }
    };
    if join.help {
        let suffix = &pipeline.seq[shape.join.unwrap_or(shape.split) + 1..];
        if suffix.is_empty() {
            params.stdout(shell).write_all(JOIN_HELP.as_bytes())?;
            return Ok(settle(shell, pipeline, vec![0], 0));
        }
        let help = CompositionFile::create("join-help")?;
        help.write()?.write_all(JOIN_HELP.as_bytes())?;
        let mut suffix_params = params.clone();
        suffix_params.set_fd(OpenFiles::STDIN_FD, help.read()?.into());
        let result = execute_composition_segment(suffix, shell, &suffix_params).await?;
        let mut statuses = vec![0];
        statuses.extend_from_slice(shell.last_pipeline_statuses());
        return Ok(settle(shell, pipeline, statuses, result.exit_code.into()));
    }
    let ast::Command::Split(split) = &pipeline.seq[shape.split] else {
        unreachable!()
    };

    let Some((prefix_status, input)) =
        composition_input(&pipeline.seq[..shape.split], shell, params, &mut statuses).await?
    else {
        fail_stages(&mut statuses, pipeline, 2);
        return Ok(settle(shell, pipeline, statuses, 2));
    };
    if prefix_status != 0 {
        fail_stages(&mut statuses, pipeline, prefix_status);
        return Ok(settle(shell, pipeline, statuses, prefix_status));
    }

    let Some(workspace) = crate::split::workspace() else {
        writeln!(
            params.stderr(shell),
            "split: workspaces are unavailable here"
        )?;
        fail_stages(&mut statuses, pipeline, 2);
        return Ok(settle(shell, pipeline, statuses, 2));
    };
    let branches = split
        .branches
        .iter()
        .map(|branch| SplitBranchRequest {
            label: branch.label.clone(),
            source: branch.body.to_string(),
        })
        .collect::<Vec<_>>();
    let environment = shell
        .env()
        .iter_exported()
        .filter(|(_, value)| value.value().is_set())
        .map(|(name, value)| (name.clone(), value.value().to_cow_str(shell).into_owned()))
        .collect();
    // Ctrl-C cancels the daemon-side split; the wait then reports it.
    let started = workspace.start(
        shell.working_dir(),
        &branches,
        environment,
        input,
        join.json,
    );
    let run = match started {
        Ok(handle) => {
            let cancel = handle.canceller();
            let waiter = tokio::task::spawn_blocking(move || handle.wait());
            tokio::pin!(waiter);
            loop {
                tokio::select! {
                    result = &mut waiter => {
                        break result.map_err(|error| error.to_string()).and_then(|run| run);
                    }
                    _ = crate::sys::signal::await_ctrl_c() => cancel(),
                }
            }
        }
        Err(message) => Err(message),
    };
    let run = match run {
        Ok(run) => run,
        Err(message) => {
            writeln!(params.stderr(shell), "split: {message}")?;
            fail_stages(&mut statuses, pipeline, 2);
            return Ok(settle(shell, pipeline, statuses, 2));
        }
    };
    if run.cancelled {
        fail_stages(&mut statuses, pipeline, 130);
        let mut result = settle(shell, pipeline, statuses, 130);
        // Like a foreground job killed by Ctrl-C, an interactive shell
        // abandons the rest of the command line.
        if shell.options().interactive {
            result.next_control_flow = crate::results::ExecutionControlFlow::Interrupted;
        }
        return Ok(result);
    }
    let branch_failure = run.status;
    statuses.push(branch_failure);

    let typed_end = shape.join.unwrap_or(shape.split) + 1;
    let suffix = &pipeline.seq[typed_end..];
    // `join` (explicit or implied) writes the rendering, or the manifest with
    // `--json`, to its stdout; its stage status is the first failed branch's.
    let mut join_params = params.clone();
    let rendered = (!suffix.is_empty())
        .then(|| CompositionFile::create("join-output"))
        .transpose()?;
    if let Some(rendered) = &rendered {
        join_params.set_fd(OpenFiles::STDOUT_FD, rendered.write()?.into());
    }
    for redirect in &join.redirects {
        setup_redirect(shell, &mut join_params, redirect).await?;
    }
    join_params.stdout(shell).write_all(&run.input)?;
    drop(join_params);
    if shape.join.is_some() {
        statuses.push(branch_failure);
    }
    // The stages after `join` see SPLIT_ID, SPLIT_DIR, SPLIT_MANIFEST and
    // SPLIT_OBJECTS; the split is released after the last stage.
    let (last_status, mut status) = if let Some(rendered) = &rendered {
        let mut suffix_params = params.clone();
        suffix_params.set_fd(OpenFiles::STDIN_FD, rendered.read()?.into());
        push_split_environment(shell, &run.environment)?;
        let result = execute_composition_segment(suffix, shell, &suffix_params).await;
        shell
            .env_mut()
            .pop_scope(crate::env::EnvironmentScope::Command)?;
        let result = result?;
        let segment_statuses = shell.last_pipeline_statuses().to_vec();
        let last = segment_statuses
            .last()
            .copied()
            .unwrap_or_else(|| result.exit_code.into());
        statuses.extend_from_slice(&segment_statuses);
        (last, u8::from(result.exit_code))
    } else {
        (branch_failure, branch_failure)
    };
    if shell.options().return_last_failure_from_pipeline
        && let Some(failure) = statuses.iter().rev().find(|status| **status != 0)
    {
        status = *failure;
    }
    // Retention follows the last stage's own status (not pipefail).
    let lines = (run.release)(last_status, join.keep);
    write_lines(shell, params, &lines)?;
    Ok(settle(shell, pipeline, statuses, status))
}
