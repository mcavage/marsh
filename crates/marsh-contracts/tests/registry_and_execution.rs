//! Boundary checks: packaged declarations and historical worker JSON, not a
//! second implementation of execution or registry policy.
use marsh_contracts::{
    ExecutionOutcome, ResourceLimit, SetupStage,
    command_registry::{CommandName, CommandRegistry, rules},
};

#[test]
fn shipped_registries_and_an_ordinary_custom_command_are_valid() {
    for document in [
        include_bytes!("../../../packaging/commands.json").as_slice(),
        include_bytes!("../../../packaging/drive-commands.json").as_slice(),
        include_bytes!("../../../tests/acceptance/fixture/commands.json").as_slice(),
    ] {
        assert!(
            !CommandRegistry::from_json_slice(document)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(CommandName::parse("foo").unwrap().as_str(), "foo");
    assert!(CommandName::parse("agent-tool_2").is_ok());
    // Dots belong to MCP/publication semantics, not shell Kit command names.
    assert!(CommandName::parse("tools.foo").is_err());
}

#[test]
fn invalid_declarations_cannot_be_hidden_by_overlay_or_update() {
    let base = CommandRegistry::from_json_slice(br#"{"foo":"/source/foo","keep":"/source/keep"}"#)
        .unwrap();
    let override_foo = CommandRegistry::from_json_slice(br#"{"foo":"/source/new"}"#).unwrap();
    let merged = base.clone().merged(override_foo).unwrap().into_entries();
    assert_eq!(merged["foo"], "/source/new");
    assert_eq!(merged["keep"], "/source/keep");
    assert_eq!(
        base.clone().merged(CommandRegistry::default()).unwrap(),
        base
    );
    assert!(base.with_added("foo", "/another").is_err());
    assert!(base.with_added("bar", " ").is_err());
    assert!(
        base.with_added("bar", &"x".repeat(rules().max_document_bytes))
            .is_err()
    );
    let encoded = base.to_json_pretty().unwrap();
    assert_eq!(CommandRegistry::from_json_slice(&encoded).unwrap(), base);
    assert_eq!(base.iter().count(), 2);

    for name in rules().reserved_names.iter().cloned().chain([
        "-option".into(),
        "with space".into(),
        "path/like".into(),
        String::new(),
        "x".repeat(rules().max_name_bytes + 1),
    ]) {
        let json = serde_json::to_vec(&serde_json::json!({name.as_str(): "/source"})).unwrap();
        assert!(CommandRegistry::from_json_slice(&json).is_err());
        assert!(base.with_added(&name, "/source").is_err());
    }
    for json in [
        br#"{"foo":"a","foo":"b"}"#.as_slice(),
        br#"{"foo":"a","\u0066oo":"b"}"#.as_slice(),
        br#"{"foo":false}"#.as_slice(),
        br#"{"foo":""}"#.as_slice(),
        b"[]".as_slice(),
        b"{} trailing".as_slice(),
    ] {
        assert!(CommandRegistry::from_json_slice(json).is_err());
    }
    let entries = (0..rules().max_commands)
        .map(|n| (format!("command{n}"), "source"))
        .collect::<std::collections::BTreeMap<_, _>>();
    let full = CommandRegistry::from_json_slice(&serde_json::to_vec(&entries).unwrap()).unwrap();
    assert!(full.with_added("overflow", "source").is_err());
    assert!(full.merged(base).is_err());
    assert!(CommandRegistry::from_json_slice(&vec![b' '; rules().max_document_bytes + 1]).is_err());
}

#[test]
fn worker_wire_forms_survive_the_move_and_legacy_missing_facts_are_unknown() {
    let observations = [
        (
            r#"{"status":"exited","code":7}"#,
            ExecutionOutcome::Exited { code: 7 },
        ),
        (
            r#"{"status":"limit_exceeded","resource":"output"}"#,
            ExecutionOutcome::LimitExceeded {
                resource: ResourceLimit::Output,
            },
        ),
        (
            r#"{"status":"limit_exceeded","resource":"pids"}"#,
            ExecutionOutcome::LimitExceeded {
                resource: ResourceLimit::Pids,
            },
        ),
        (
            r#"{"status":"limit_exceeded","resource":"wall"}"#,
            ExecutionOutcome::LimitExceeded {
                resource: ResourceLimit::Wall,
            },
        ),
        (
            r#"{"status":"setup_failed","stage":"attach"}"#,
            ExecutionOutcome::SetupFailed {
                stage: SetupStage::Attach,
            },
        ),
        (
            r#"{"status":"supervision_failed"}"#,
            ExecutionOutcome::SupervisionFailed,
        ),
    ];
    for (wire, observed) in observations {
        assert_eq!(
            serde_json::from_str::<ExecutionOutcome>(wire).unwrap(),
            observed
        );
        assert_eq!(
            serde_json::to_value(observed).unwrap(),
            serde_json::from_str::<serde_json::Value>(wire).unwrap()
        );
    }
    assert_eq!(ExecutionOutcome::default(), ExecutionOutcome::Unknown);
    assert!(
        serde_json::from_str::<ExecutionOutcome>(
            r#"{"status":"setup_failed","stage":"cloud_capture"}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<ExecutionOutcome>(
            r#"{"status":"limit_exceeded","resource":"exit7"}"#
        )
        .is_err()
    );
}
