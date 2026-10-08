use std::io::Cursor;

use crate::{Parser, ParserOptions, ast::Command};

fn parse(input: &str, enabled: bool) -> Result<crate::ast::Program, crate::ParseError> {
    let options = ParserOptions {
        marsh_extensions: enabled,
        ..ParserOptions::default()
    };
    Parser::new(Cursor::new(input), &options).parse_program()
}

#[test]
fn extension_is_gated_and_parses_branch_pipelines() {
    let ordinary = parse("fanout { claude, codex }\n", false).unwrap();
    assert!(matches!(
        ordinary.complete_commands[0].0[0].0.first.seq[0],
        Command::Simple(_)
    ));

    let program = parse(
        "fanout { first: printf one | sed s/one/1/; second: printf two } | collect --timing\n",
        true,
    )
    .unwrap();
    let pipeline = &program.complete_commands[0].0[0].0.first;
    let Command::Fanout(fanout) = &pipeline.seq[0] else {
        panic!("expected fanout")
    };
    assert_eq!(fanout.branches.len(), 2);
    assert_eq!(fanout.branches[0].label, "first");
    assert_eq!(fanout.branches[0].body.0[0].0.first.seq.len(), 2);
    assert!(matches!(pipeline.seq[1], Command::Collect(_)));
}

#[test]
fn shorthand_labels_are_stable_and_separators_work() {
    let program = parse("fanout {\nclaude,\nclaude;\npi\n} | collect --json\n", true).unwrap();
    let Command::Fanout(fanout) = &program.complete_commands[0].0[0].0.first.seq[0] else {
        panic!("expected fanout")
    };
    assert_eq!(
        fanout
            .branches
            .iter()
            .map(|branch| branch.label.as_str())
            .collect::<Vec<_>>(),
        ["claude", "claude-2", "pi"]
    );
}

#[test]
fn malformed_blocks_fail_closed() {
    for input in [
        "fanout { }",
        "fanout { a: }",
        "fanout { a: true, a: false }",
    ] {
        assert!(parse(input, true).is_err(), "accepted {input:?}");
    }
}

#[test]
fn assignment_prefixed_fanout_remains_an_ordinary_command() {
    let program = parse("MARKER=ordinary fanout { a,b }\n", true).unwrap();
    assert!(matches!(
        program.complete_commands[0].0[0].0.first.seq[0],
        Command::Simple(_)
    ));
}

#[test]
fn ordinary_large_program_does_not_trigger_quadratic_fanout_scans() {
    // This is deliberately large enough that rescanning the complete prefix at
    // every ordinary token makes the regression conspicuous under `cargo test`,
    // while the lexical fast path remains a routine linear parse.
    let mut input = "printf x\n".repeat(16_384);
    input.push_str("fanout { first: printf one; second: printf two } | collect\n");

    let program = parse(&input, true).unwrap();
    assert_eq!(program.complete_commands.len(), 16_385);
    assert!(matches!(
        program.complete_commands.last().unwrap().0[0].0.first.seq[0],
        Command::Fanout(_)
    ));
}

fn first_pipeline(program: &crate::ast::Program) -> &crate::ast::Pipeline {
    &program.complete_commands[0].0[0].0.first
}

#[test]
fn split_is_gated_and_parses_labels_shorthand_and_join() {
    let ordinary = parse("split { a: true }\n", false).unwrap();
    assert!(matches!(
        first_pipeline(&ordinary).seq[0],
        Command::Simple(_)
    ));

    let program = parse(
        "printf in | split { fix: claude -p x, tests: codex exec y; pi } | join --json | claude -p z | cat\n",
        true,
    )
    .unwrap();
    let pipeline = first_pipeline(&program);
    let Command::Split(split) = &pipeline.seq[1] else {
        panic!("expected split")
    };
    assert_eq!(
        split
            .branches
            .iter()
            .map(|b| b.label.as_str())
            .collect::<Vec<_>>(),
        ["fix", "tests"]
    );
    // `; pi` has no label, so it is sequenced inside the `tests` branch.
    assert_eq!(split.branches[1].body.0.len(), 2);
    assert!(pipeline.seq[2].is_split_join_stage());
    assert!(matches!(pipeline.seq[3], Command::Simple(_)));
    assert!(matches!(pipeline.seq[4], Command::Simple(_)));
}

#[test]
fn split_rejects_case_insensitive_duplicates_and_bad_shapes() {
    for input in [
        "split { }",
        "split { a: }",
        "split { a: true, A: false }",
        "split { a: true } |& join",
        "split { a: true } | join |& cat",
        "split { a: true } | collect",
        "fanout { a: true } | split { b: true }",
        "split { a: true } | cat | split { b: true }",
    ] {
        assert!(parse(input, true).is_err(), "accepted {input:?}");
    }
    let many = (0..17)
        .map(|i| format!("b{i}: true"))
        .collect::<Vec<_>>()
        .join("; ");
    assert!(parse(&format!("split {{ {many} }}"), true).is_err());
}

#[test]
fn coreutils_split_and_join_stay_ordinary_commands() {
    for input in [
        "split -l 1 f\n",
        "command split { a\n",
        "\\split { a\n",
        "x | join a b\n",
        "join a b\n",
        "MARKER=1 split { a: true }\n",
    ] {
        let program = parse(input, true).unwrap();
        assert!(
            first_pipeline(&program)
                .seq
                .iter()
                .all(|command| matches!(command, Command::Simple(_))),
            "{input:?} parsed as a composition"
        );
    }
    let program = parse("split { a: true } | command join a b\n", true).unwrap();
    assert!(!first_pipeline(&program).seq[1].is_split_join_stage());
    let program = parse("split { a: true } | \\join a b\n", true).unwrap();
    assert!(!first_pipeline(&program).seq[1].is_split_join_stage());
    let program = parse("split { a: true } | X=1 join a b\n", true).unwrap();
    assert!(!first_pipeline(&program).seq[1].is_split_join_stage());
}

fn branches(input: &str) -> Vec<(String, String)> {
    let program = parse(input, true).unwrap_or_else(|e| panic!("{input:?}: {e}"));
    let (Command::Fanout(command) | Command::Split(command)) = &first_pipeline(&program).seq[0]
    else {
        panic!("{input:?} is not a fanout or split")
    };
    command
        .branches
        .iter()
        .map(|branch| (branch.label.clone(), branch.body.to_string()))
        .collect()
}

fn labels(input: &str) -> Vec<String> {
    branches(input)
        .into_iter()
        .map(|(label, _)| label)
        .collect()
}

#[test]
fn a_semicolon_starts_a_branch_only_before_a_label() {
    // Labeled `;` separation.
    assert_eq!(
        labels("fanout { ok: true; bad: false } | collect\n"),
        ["ok", "bad"]
    );
    let split = branches("split { a: cd x; make; b: cd y; make } | join\n");
    assert_eq!(
        split
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    for (_, body) in &split {
        assert!(body.contains("cd") && body.contains("make"), "{body:?}");
    }
    // Labels may use hyphens and underscores, and may start with `_`.
    assert_eq!(
        labels("split { fix-lint: true; run_tests: true; _x: true; A-1_b: true } | join\n"),
        ["fix-lint", "run_tests", "_x", "A-1_b"]
    );
    // Newline and `,` still start branches without a label.
    assert_eq!(
        labels("fanout {\necho one; echo two\nprintf x, true\n}\n"),
        ["echo", "printf", "true"]
    );
}

#[test]
fn an_unlabeled_semicolon_is_bash_sequencing() {
    let one = branches("fanout { echo one; echo two } | collect\n");
    assert_eq!(one.len(), 1, "{one:?}");
    assert_eq!(one[0].0, "echo");
    let program = parse("fanout { echo one; echo two } | collect\n", true).unwrap();
    let Command::Fanout(fanout) = &first_pipeline(&program).seq[0] else {
        panic!("expected fanout")
    };
    assert_eq!(fanout.branches[0].body.0.len(), 2);

    // `a: cmd; cmd` keeps both commands in `a`.
    assert_eq!(labels("split { a: true; false; true } | join\n"), ["a"]);
}

#[test]
fn groups_subshells_and_lists_stay_inside_one_branch() {
    for (input, expected) in [
        ("fanout { a: { echo 1; echo 2; } } | collect\n", vec!["a"]),
        (
            "fanout { a: { echo 1; echo 2; }; b: true } | collect\n",
            vec!["a", "b"],
        ),
        (
            "fanout { a: (cd x; make); b: true } | collect\n",
            vec!["a", "b"],
        ),
        (
            "fanout { a: true && echo x; false || echo y; b: true } | collect\n",
            vec!["a", "b"],
        ),
        (
            "split { a: make && make test || echo failed } | join\n",
            vec!["a"],
        ),
        (
            "fanout { a: case $x in y) echo y;; *) echo z;; esac; echo after; b: true } | collect\n",
            vec!["a", "b"],
        ),
    ] {
        assert_eq!(labels(input), expected, "{input:?}");
    }
    let group = branches("fanout { a: { echo 1; echo 2; }; b: true } | collect\n");
    assert!(
        group[0].1.contains("echo 1") && group[0].1.contains("echo 2"),
        "{group:?}"
    );
    let case = branches(
        "fanout { a: case $x in y) echo y;; *) echo z;; esac; echo after; b: true } | collect\n",
    );
    assert!(
        case[0].1.contains("esac") && case[0].1.contains("after"),
        "{case:?}"
    );
}

#[test]
fn only_a_bare_label_word_right_after_a_semicolon_starts_a_branch() {
    // `a:` mid-command is an argument, not a label.
    let parsed = branches("fanout { x: echo a: b; echo c: d } | collect\n");
    assert_eq!(parsed.len(), 1, "{parsed:?}");
    assert!(
        parsed[0].1.contains("a: b") && parsed[0].1.contains("c: d"),
        "{parsed:?}"
    );
    // Quoted, expanded, or otherwise non-label words do not start a branch.
    for input in [
        "fanout { x: true; 'b:' true } | collect\n",
        "fanout { x: true; \"b:\" true } | collect\n",
        "fanout { x: true; $b: true } | collect\n",
        "fanout { x: true; 1b: true } | collect\n",
    ] {
        assert_eq!(labels(input), ["x"], "{input:?}");
    }
    // `b-:` is a valid label (hyphens are allowed after the first character).
    assert_eq!(
        labels("fanout { x: true; b-: true } | collect\n"),
        ["x", "b-"]
    );
    // The documented ambiguity: a command named `echo:` after `;` is a label.
    assert_eq!(
        labels("fanout { x: true; echo: hi } | collect\n"),
        ["x", "echo"]
    );
}

#[test]
fn a_fanout_nested_in_a_branch_is_normalized_after_its_label() {
    // Commas inside a nested `fanout { }` belong to it, in one-line,
    // multi-line, and grouped forms: the outer block keeps two branches and
    // the nested fanout keeps its own two.
    for (input, outer, nested) in [
        (
            "split { fix: true, review: fanout { races: true, leaks: true } | collect } | join\n",
            ["fix", "review"],
            1,
        ),
        (
            "split {\n  fix: true\n  review: fanout { races: true, leaks: true } | collect\n} | join\n",
            ["fix", "review"],
            1,
        ),
        (
            "fanout { a: fanout { races: true, leaks: true } | collect, d: true } | collect\n",
            ["a", "d"],
            0,
        ),
    ] {
        let parsed = branches(input);
        assert_eq!(
            parsed
                .iter()
                .map(|(label, _)| label.as_str())
                .collect::<Vec<_>>(),
            outer,
            "{input:?}"
        );
        let body = format!("{}\n", parsed[nested].1.trim());
        assert_eq!(labels(&body), ["races", "leaks"], "{input:?}: {body:?}");
    }
    let grouped = labels(
        "split { fix: true, review: { fanout { races: true, leaks: true } | collect; } } | join\n",
    );
    assert_eq!(grouped, ["fix", "review"]);
    // `{ fanout {` at the top level is in command position too.
    assert!(parse("{ fanout { a: true, b: true } | collect; }\n", true).is_ok());
}
