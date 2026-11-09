//! Services VMs: the per-sandbox Docker disk size.
use super::*;

fn spec(shell: &ShellFixture, project: &Path, disk_size: &str) -> ServicesVmSpec {
    ServicesVmSpec {
        project: project.to_owned(),
        image: shell.spec.image.clone(),
        shell_binary: shell.spec.shell_binary.clone(),
        disk_size: disk_size.into(),
    }
}

#[test]
fn the_docker_disk_size_rides_on_the_create_invocation_only() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let project = directory(shell.dir.path(), "project");
    let name = adapter
        .vm_name(VmPurpose::Services, project.to_str().unwrap())
        .unwrap();
    adapter
        .create_services_vm(&spec(&shell, &project, "2G"), &name)
        .unwrap();
    let creates = model.calls_of("create");
    assert_eq!(creates.len(), 1);
    assert_eq!(
        creates[0].env,
        [("DOCKER_SANDBOXES_DOCKER_SIZE".to_owned(), "2g".to_owned())]
    );
    // The host's static MCP set is not attached (SECURITY-DESIGN A12).
    assert_eq!(creates[0].value("--static-mcp"), Some("none"));
    // Nothing else carries it, and the host-global setting is never touched.
    assert!(
        model
            .calls()
            .iter()
            .filter(|call| call.verb() != "create")
            .all(|call| call.env.is_empty())
    );
    assert!(model.calls().iter().all(|call| call.verb() != "settings"));
}

#[test]
fn an_unusable_disk_size_is_refused_before_any_stock_call() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let project = directory(shell.dir.path(), "project");
    for size in ["", "40", "0g", "forty"] {
        assert!(matches!(
            adapter.ensure_services_vm(&spec(&shell, &project, size)),
            Err(SbxError::HostGrantFence(_))
        ));
    }
    assert!(model.calls().is_empty());
}

/// Every operation the stop cutover runs inside a services VM (the freeze, the
/// thaw, the running count) first checks the sandbox now: a name another
/// sandbox holds (the old relay may not know yet), a stopped VM and an absent
/// one are each refused, and nothing is executed. Table: one row per way the
/// recorded VM can stop being the one at the name, crossed with every operation.
#[test]
fn no_exec_reaches_a_services_vm_that_is_not_the_recorded_running_one() {
    type Change = fn(&mut super::stock_model::Vm);
    type Operation = fn(&StockSbx, &str, &[String]) -> Result<(), SbxError>;
    let cases: [(&str, Option<Change>); 4] = [
        ("the owned VM runs", None),
        (
            "another sandbox holds the name",
            Some(|vm| vm.id = "00000000-0000-0000-0000-00000000ffff".into()),
        ),
        ("the VM is stopped", Some(|vm| vm.running = false)),
        (
            "a stopped impostor",
            Some(|vm| {
                vm.id = "00000000-0000-0000-0000-00000000ffff".into();
                vm.running = false;
            }),
        ),
    ];
    let ids = ["b".repeat(64)];
    let operations: [(&str, Operation); 3] = [
        ("quiesce", |a, n, _| a.services_quiesce(n).map(|_| ())),
        ("unquiesce", |a, n, ids| a.services_unquiesce(n, ids)),
        ("running count", |a, n, _| {
            a.services_running_count(n).map(|_| ())
        }),
    ];
    for (case, change) in cases {
        for (operation, run) in &operations {
            let model = StockModel::new();
            let adapter = model.adapter();
            let shell = ShellFixture::new();
            let project = directory(shell.dir.path(), "project");
            let name = adapter
                .vm_name(VmPurpose::Services, project.to_str().unwrap())
                .unwrap();
            model.seed_owned(&adapter, &name);
            adapter
                .services_engine_ids
                .lock()
                .unwrap()
                .insert(name.clone(), "a".repeat(64));
            if let Some(change) = change {
                model.with_vm(&name, change);
            }
            let result = run(&adapter, &name, &ids);
            let execs = model.calls_of("exec").len();
            if change.is_none() {
                assert_eq!(execs, 1, "{operation}: {case}: an owned VM is reached");
            } else {
                assert_eq!(execs, 0, "{operation}: {case}: no exec may reach it");
                assert!(result.is_err(), "{operation}: {case}: refused");
            }
        }
    }
}
