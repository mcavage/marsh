//! Services VMs: the per-sandbox Docker disk size.
use super::*;

fn spec(shell: &ShellFixture, project: &Path, disk_size: &str) -> ServicesVmSpec {
    ServicesVmSpec {
        project: project.to_owned(),
        image: shell.spec.image.clone(),
        shell_binary: shell.spec.shell_binary.clone(),
        disk_size: disk_size.into(),
        pool_bytes: None,
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

const IMPOSTOR: &str = "00000000-0000-0000-0000-00000000ffff";

/// A services VM recorded for a project, running in the model.
fn owned_services_vm(
    model: &StockModel,
    adapter: &StockSbx,
    shell: &ShellFixture,
    project: &str,
) -> (PathBuf, String) {
    let project = directory(shell.dir.path(), project);
    let name = adapter
        .vm_name(VmPurpose::Services, project.to_str().unwrap())
        .unwrap();
    model.seed_owned(adapter, &name);
    (project, name)
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

/// `marsh docker stop` keeps the VM and its data; `rm` removes exactly the
/// recorded VM, once, and forgets the record.
#[test]
fn stop_keeps_the_vm_and_remove_deletes_the_recorded_one() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let (project, name) = owned_services_vm(&model, &adapter, &shell, "project");
    let (_, bystander) = owned_services_vm(&model, &adapter, &shell, "bystander");

    assert!(adapter.stop_services_vm(&project, soon()).unwrap());
    assert_eq!(model.calls_of("stop").len(), 1);
    assert!(!model.vm(&name).expect("stop keeps the VM").running);
    assert!(
        !adapter.stop_services_vm(&project, soon()).unwrap(),
        "already stopped"
    );
    assert_eq!(model.calls_of("stop").len(), 1);
    assert_eq!(model.count("rm"), 0, "a stop never removes");

    assert!(adapter.remove_services_vm(&project, soon()).unwrap());
    assert!(model.vm(&name).is_none());
    assert!(adapter.recorded_services_vm(&project).is_none());
    assert!(!adapter.remove_services_vm(&project, soon()).unwrap());
    assert_eq!(model.count("rm"), 1);
    assert!(model.vm(&bystander).is_some(), "another project's VM stays");
    assert_eq!(adapter.owned_services_vms().len(), 1);
}

/// A VM that is not the recorded one holds the name: removal forgets the
/// record and never touches the VM; stop refuses.
#[test]
fn a_foreign_vm_at_the_recorded_name_is_never_stopped_or_removed() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let (project, name) = owned_services_vm(&model, &adapter, &shell, "project");
    model.with_vm(&name, |vm| vm.id = IMPOSTOR.into());

    assert!(matches!(
        adapter.stop_services_vm(&project, soon()),
        Err(SbxError::ForeignVm(_))
    ));
    assert_eq!(model.calls_of("stop").len(), 0);
    assert!(!adapter.remove_services_vm(&project, soon()).unwrap());
    assert_eq!(model.count("rm"), 0, "the impostor must not be removed");
    assert_eq!(model.vm(&name).unwrap().id, IMPOSTOR);
    assert!(adapter.recorded_services_vm(&project).is_none());
    assert_eq!(model.calls_of("exec").len(), 0);
}

/// `reset --include-services` removes the VMs of the confirmed projects only,
/// by recorded UUID: an unconfirmed project is kept (reported as such), and a
/// confirmed project whose name now holds a foreign VM removes nothing.
#[test]
fn reset_removes_only_confirmed_projects_and_never_a_foreign_vm() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let (confirmed, confirmed_name) = owned_services_vm(&model, &adapter, &shell, "confirmed");
    let (kept, kept_name) = owned_services_vm(&model, &adapter, &shell, "kept");
    let (replaced, replaced_name) = owned_services_vm(&model, &adapter, &shell, "replaced");
    model.with_vm(&replaced_name, |vm| vm.id = IMPOSTOR.into());

    let key = |path: &Path| path.to_str().unwrap().to_owned();
    let keys: BTreeSet<String> = [key(&confirmed), key(&replaced)].into();
    let results = adapter.remove_confirmed_services_vms_before(&keys, soon());
    let outcome = |name: &str| {
        let entry = results.iter().find(|(n, _, _)| n == name).unwrap();
        entry
            .2
            .as_ref()
            .map(|removed| *removed)
            .map_err(ToString::to_string)
    };
    assert_eq!(results.len(), 3, "one entry per owned VM");
    assert_eq!(outcome(&confirmed_name), Ok(Some(true)));
    assert_eq!(outcome(&kept_name), Ok(None), "not confirmed");
    assert_eq!(
        outcome(&replaced_name),
        Ok(Some(false)),
        "foreign: nothing removed"
    );
    assert!(model.vm(&confirmed_name).is_none());
    assert!(model.vm(&kept_name).is_some());
    assert_eq!(model.vm(&replaced_name).unwrap().id, IMPOSTOR);
    assert_eq!(model.count("rm"), 1);
    assert!(adapter.recorded_services_vm(&kept).is_some());
    assert!(adapter.recorded_services_vm(&replaced).is_none());
}

/// A key that no owned VM has (a project that never had one, or a
/// look-alike path) removes nothing at all.
#[test]
fn reset_with_an_unknown_project_key_touches_nothing() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let (project, name) = owned_services_vm(&model, &adapter, &shell, "project");
    let lookalike = format!("{}/", project.display());
    let keys: BTreeSet<String> = [lookalike, "/nowhere".to_owned()].into();
    let results = adapter.remove_confirmed_services_vms_before(&keys, soon());
    assert!(
        results
            .iter()
            .all(|(_, _, result)| matches!(result, Ok(None)))
    );
    assert_eq!(model.count("rm"), 0);
    assert!(model.vm(&name).is_some());
}

/// A new VM whose configured disk cannot hold the engine's pool and reserve is
/// refused before a name is recorded or any stock call is made, with the
/// engine's own hint; a disk that can hold it, or no pool to hold, creates.
#[test]
fn a_disk_too_small_for_the_pool_is_refused_before_a_vm_is_created() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let project = directory(shell.dir.path(), "project");
    let pool = 24_u64 << 30;
    let with_pool = |size: &str, pool| ServicesVmSpec {
        pool_bytes: pool,
        ..spec(&shell, &project, size)
    };

    for size in ["5g", "24g"] {
        let refused = adapter
            .ensure_services_vm(&with_pool(size, Some(pool)))
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(&format!("the Docker disk is set to {size}"))
                && refused
                    .contains("a 24576 MiB pool needs 25088 MiB (including a 512 MiB reserve)")
                && refused.contains("marsh config services-disk-size"),
            "{refused}"
        );
        assert!(model.calls().is_empty(), "{size}: {:?}", model.calls());
        assert_eq!(adapter.recorded_services_vm(&project), None);
    }

    // 25g holds the pool and the reserve; a 5g disk holds a small test pool.
    for (size, pool) in [("25g", Some(pool)), ("5g", Some(4_u64 << 30)), ("5g", None)] {
        let model = StockModel::new();
        let adapter = model.adapter();
        let _ = adapter.ensure_services_vm(&with_pool(size, pool));
        assert_eq!(model.count("create"), 1, "{size} {pool:?}");
    }
}
