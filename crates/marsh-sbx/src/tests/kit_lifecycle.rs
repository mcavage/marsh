//! Published Kit VMs: creation, ownership, worker lease, reset and quarantine.
use super::stock_model::{marker_write, verb, verb_on};
use super::*;

const KIT: &str = "marsh-kit-fixture";

#[test]
fn cold_kit_vm_is_created_from_the_pinned_workload_and_runs_the_installed_worker() {
    let model = StockModel::new();
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    assert!(ready.cold_started);

    let vm = model.vm(KIT).unwrap();
    assert_eq!(adapter.ownership.uuid(KIT), Some(vm.id.clone()));
    let create = &model.calls_of("create")[0];
    assert_eq!(create.value("--name"), Some(KIT));
    assert_eq!(create.value("--pull"), Some("missing"));
    assert_eq!(
        create.value("-e"),
        Some(format!("MARSH_SELECTED_HOME={}", kit.home().display()).as_str())
    );
    // `oci://` keeps stock from resolving the reference as a local path.
    let workload = format!("oci://{}", kit.spec.workload_kit.identity());
    assert!(create.args.contains(&workload), "{create:?}");
    assert_eq!(vm.workspaces, [kit.home()]);

    assert_eq!(vm.files[WORKER_PATH], b"worker");
    assert_eq!(
        vm.files["/usr/local/libexec/marsh-byte-exec"],
        b"byte-exec helper"
    );
    assert_eq!(vm.files[marsh_runtime::SPLIT_SHIM], b"job artifact");
    assert_eq!(
        vm.files["/var/lib/marsh/worker.json"],
        kit.published_marker()
    );
    assert_eq!(model.retained_spawns(WORKER_PATH), 1);

    let warm = adapter.ensure_kit_vm(&kit.spec).unwrap();
    assert!(!warm.cold_started);
    assert_eq!(model.retained_spawns(WORKER_PATH), 1);
}

#[test]
fn cold_vm_marker_failure_removes_only_the_vm_it_created() {
    let model = StockModel::new();
    model.seed_vm("user-vm");
    model.fault(
        marker_write(),
        Fault::Respond(failure("marker write failed")),
    );
    let adapter = model.adapter();
    let kit = KitFixture::published(KIT);

    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::CommandFailed { .. })
    ));
    assert_eq!(model.vm_names(), ["user-vm"]);
    assert_eq!(adapter.ownership.uuid(KIT), None);
}

#[test]
fn unreadable_or_unknown_inventory_never_creates_a_vm() {
    for listing in [
        failure("Error: cannot connect to the sandboxd daemon"),
        CommandOutput {
            exit_code: Some(0),
            stdout: br#"{"vms":[]}"#.to_vec(),
            stderr: Vec::new(),
        },
    ] {
        let model = StockModel::new();
        model.fault(verb("ls"), Fault::Respond(listing));
        let kit = KitFixture::published(KIT);
        assert!(model.adapter().ensure_kit_vm(&kit.spec).is_err());
        assert_eq!(model.count("create"), 0);
        assert_eq!(model.vm_names(), [] as [std::string::String; 0]);
    }
}

#[test]
fn an_existing_vm_that_is_not_provably_ours_is_never_touched() {
    // Unrecorded name, then a recorded name whose guest marker names another Kit.
    for recorded in [false, true] {
        let model = StockModel::new();
        let adapter = model.adapter();
        let kit = KitFixture::published(KIT);
        if recorded {
            model.seed_owned(&adapter, KIT);
            model.with_vm(KIT, |vm| {
                vm.files.insert(
                    "/var/lib/marsh/worker.json".into(),
                    b"{\"profile\":\"x\"}\n".to_vec(),
                );
            });
        } else {
            model.seed_vm(KIT);
        }

        assert!(matches!(
            adapter.ensure_kit_vm(&kit.spec),
            Err(SbxError::ForeignVm(_))
        ));
        assert_eq!(model.mutations(), 0, "recorded={recorded}");
        assert!(model.vm(KIT).is_some());
    }
}

#[test]
fn cached_kit_revalidation_rejects_another_workload_and_a_lost_worker() {
    let model = StockModel::new();
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    assert!(adapter.revalidate_cached_kit(&kit.spec, &ready).unwrap());
    let mut foreign = ready.clone();
    foreign.kit_ref = format!("registry.example/foreign@sha256:{}", "a".repeat(64));
    assert!(!adapter.revalidate_cached_kit(&kit.spec, &foreign).unwrap());

    model.kill_workers(KIT);
    assert!(!adapter.revalidate_cached_kit(&kit.spec, &ready).unwrap());
    // The next preparation starts a fresh worker instead of reusing the lease.
    adapter.ensure_kit_vm(&kit.spec).unwrap();
    assert_eq!(model.retained_spawns(WORKER_PATH), 2);
}

#[test]
fn reset_removes_only_the_exact_owned_kit_vm_and_releases_its_quarantine() {
    let model = StockModel::new();
    model.seed_vm("user-vm");
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    adapter.stop_quarantined(&ready).unwrap();
    assert!(!model.vm(KIT).unwrap().running);
    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::QuarantinedVm(_))
    ));

    assert!(adapter.reset_kit_vm(&kit.spec).unwrap());
    assert_eq!(model.vm_names(), ["user-vm"]);
    assert_eq!(adapter.ownership.uuid(KIT), None);
    // `marsh workers reset KIT` retires the quarantined VM; the Kit is usable.
    assert!(adapter.ensure_kit_vm(&kit.spec).unwrap().cold_started);
}

#[test]
fn reset_refuses_a_foreign_vm_and_a_vm_with_a_pinned_session_grant() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let foreign = KitFixture::published("marsh-kit-foreign");
    model.seed_vm("marsh-kit-foreign");
    assert!(matches!(
        adapter.reset_kit_vm(&foreign.spec),
        Err(SbxError::ForeignVm(_))
    ));
    assert!(model.vm("marsh-kit-foreign").is_some());

    let (adapter, kit, ready) = warm_kit(&model, KIT);
    let source = directory(kit.dir.path(), "project");
    adapter
        .prepare_session_grants(
            &ready,
            "session-one",
            &[grant(
                source,
                "/Users/alice/project",
                MountAccess::ReadWrite,
            )],
        )
        .unwrap();
    assert!(matches!(
        adapter.reset_kit_vm(&kit.spec),
        Err(SbxError::ActiveWorkerState(_))
    ));
    assert!(model.vm(KIT).is_some());
    adapter.release_session_grants(KIT, "session-one").unwrap();
    assert!(adapter.reset_kit_vm(&kit.spec).unwrap());
    assert_eq!(model.count("rm"), 1);
}

#[test]
fn reset_waits_for_an_inflight_mount_and_then_observes_its_reference() {
    let model = StockModel::new();
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    let adapter = Arc::new(adapter);
    let mounting = model.gate(verb_on("mount", KIT.into()));
    let source = directory(kit.dir.path(), "project");
    let preparing = {
        let adapter = Arc::clone(&adapter);
        let admitted = grant(source, "/Users/alice/project", MountAccess::ReadWrite);
        thread::spawn(move || adapter.prepare_session_grants(&ready, "session-one", &[admitted]))
    };
    mounting.wait_entered(1);
    let (reset_send, reset_receive) = mpsc::channel();
    {
        let adapter = Arc::clone(&adapter);
        let spec = kit.spec.clone();
        thread::spawn(move || reset_send.send(adapter.reset_kit_vm(&spec)));
    }
    assert_still_blocked(&reset_receive, "reset");
    mounting.release();

    preparing.join().unwrap().unwrap();
    assert!(matches!(
        reset_receive.recv_timeout(WAIT_LIMIT).unwrap(),
        Err(SbxError::ActiveWorkerState(_))
    ));
    assert!(model.vm(KIT).is_some());
}

#[test]
fn scope_reset_removes_every_recorded_kit_vm_and_never_touches_unrecorded() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let stale = adapter.vm_name(VmPurpose::Kit, "old-kit").unwrap();
    assert!(stale.starts_with("marsh-k-") && stale.len() == "marsh-k-".len() + 8);
    model.seed_vm(&stale);
    // An in-process intent whose create never ran survives inventory.
    let intent = adapter.vm_name(VmPurpose::Kit, "never-created").unwrap();
    model.seed_vm("user-unrelated");
    let shell = adapter.vm_name(VmPurpose::Shell, "shell").unwrap();
    model.seed_vm(&shell);

    assert_eq!(adapter.reset_stale_scope_kit_vms().unwrap(), 1);
    let mut expected = vec![shell, "user-unrelated".to_owned()];
    expected.sort();
    assert_eq!(model.vm_names(), expected);
    assert_eq!(adapter.ownership.uuid(&stale), None);
    // A later use of the same Kit gets a fresh random name.
    assert_ne!(adapter.vm_name(VmPurpose::Kit, "old-kit").unwrap(), stale);
    assert_eq!(
        adapter.vm_name(VmPurpose::Kit, "never-created").unwrap(),
        intent
    );
}

#[test]
fn quarantine_stop_waits_for_an_inflight_retained_start() {
    let model = StockModel::new();
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    let adapter = Arc::new(adapter);
    let source = directory(kit.dir.path(), "project");
    let grants = adapter
        .prepare_job_grants(
            &ready,
            "session-one",
            "attempt-start-fence",
            &[grant(
                source,
                "/Users/alice/project",
                MountAccess::ReadWrite,
            )],
        )
        .unwrap();
    let input = model.gate_worker_input();
    let launching = {
        let adapter = Arc::clone(&adapter);
        let ready = ready.clone();
        thread::spawn(move || {
            adapter
                .launch_worker(&ready, &grants, "attempt-start-fence", job_spec())
                .map(drop)
        })
    };
    input.wait_entered(1);
    let (stop_send, stop_receive) = mpsc::channel();
    {
        let adapter = Arc::clone(&adapter);
        let ready = ready.clone();
        thread::spawn(move || stop_send.send(adapter.stop_quarantined(&ready)));
    }
    assert_still_blocked(&stop_receive, "quarantine stop");
    input.release();

    // Stopping under the write would have broken the Start frame.
    launching.join().unwrap().unwrap();
    stop_receive.recv_timeout(WAIT_LIMIT).unwrap().unwrap();
    assert_eq!(model.written_worker_starts(KIT), ["attempt-start-fence"]);
    assert!(!model.vm(KIT).unwrap().running);
    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::QuarantinedVm(_))
    ));
}

#[test]
fn ambiguous_retained_start_write_quarantines_and_stops_the_exact_vm() {
    let model = StockModel::new();
    model.seed_vm("user-vm");
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    let source = directory(kit.dir.path(), "project");
    let grants = adapter
        .prepare_job_grants(
            &ready,
            "session-one",
            "attempt-ambiguous-start",
            &[grant(
                source,
                "/Users/alice/project",
                MountAccess::ReadWrite,
            )],
        )
        .unwrap();
    model.fail_next_worker_flush();

    let Err(error) = adapter.launch_worker(&ready, &grants, "attempt-ambiguous-start", job_spec())
    else {
        panic!("an ambiguous Start completion must fail closed");
    };
    assert!(matches!(
        error,
        SbxError::AmbiguousWorkerStart { ref vm, ref detail }
            if vm == KIT && detail.contains("flush completion unknown")
    ));
    // The whole Start reached the transport: a container may exist.
    assert_eq!(
        model.written_worker_starts(KIT),
        ["attempt-ambiguous-start"]
    );
    assert!(!model.vm(KIT).unwrap().running);
    assert!(model.vm("user-vm").unwrap().running);
    let before = model.mutations();
    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::QuarantinedVm(_))
    ));
    assert_eq!(model.mutations(), before);
}

#[test]
fn quarantine_stop_drains_worker_initialization_before_the_vm_is_published_ready() {
    let model = StockModel::new();
    let adapter = Arc::new(model.adapter());
    let kit = KitFixture::published(KIT);
    let ready_gate = model.gate_worker_ready();
    let ensuring = {
        let adapter = Arc::clone(&adapter);
        let spec = kit.spec.clone();
        thread::spawn(move || adapter.ensure_kit_vm(&spec).map(drop))
    };
    ready_gate.wait_entered(1);

    let (stop_send, stop_receive) = mpsc::channel();
    {
        let adapter = Arc::clone(&adapter);
        thread::spawn(move || stop_send.send(adapter.stop_quarantined(&kit_vm(KIT))));
    }
    let probe = kit_vm(KIT);
    wait_until("stop publishes quarantine", || {
        matches!(
            adapter.revalidate_cached_kit(&kit.spec, &probe),
            Err(SbxError::QuarantinedVm(_))
        )
    });
    assert_still_blocked(&stop_receive, "quarantine stop");
    ready_gate.release();

    assert!(matches!(
        ensuring.join().unwrap(),
        Err(SbxError::QuarantinedVm(_))
    ));
    stop_receive.recv_timeout(WAIT_LIMIT).unwrap().unwrap();
    assert!(!model.vm(KIT).unwrap().running);
    let before = model.mutations();
    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::QuarantinedVm(_))
    ));
    assert_eq!(model.mutations(), before);
}

#[test]
fn prewarm_pulls_the_exact_digest_and_verifies_the_pulled_image_id() {
    let image =
        OciImage::parse(format!("example.invalid/probe@sha256:{}", "a".repeat(64))).unwrap();
    let valid = format!("sha256:{}", "e".repeat(64));
    for (published, expected) in [
        (Some(valid.as_str()), Ok(())),
        (Some("sha256:not-an-image-id"), Err("image verification")),
        (None, Err("pull")),
    ] {
        let model = StockModel::new();
        if let Some(id) = published {
            model.publish(image.as_str(), id);
        }
        let (adapter, _kit, ready) = warm_kit(&model, KIT);
        let result = adapter.prewarm_image(&ready, &image);
        match expected {
            Ok(()) => {
                result.unwrap();
                assert_eq!(model.vm(KIT).unwrap().images[image.as_str()], valid);
            }
            Err("pull") => assert!(matches!(
                result,
                Err(SbxError::CommandFailed {
                    operation: "pull immutable job image",
                    ..
                })
            )),
            Err(_) => assert!(matches!(result, Err(SbxError::ImageVerificationFailed))),
        }
    }
}

#[test]
fn quarantine_stop_stops_only_its_vm_and_rejects_reuse_before_any_effect() {
    let model = StockModel::new();
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    let other = KitFixture::published("marsh-kit-other");
    adapter.ensure_kit_vm(&other.spec).unwrap();

    adapter.stop_quarantined(&ready).unwrap();
    assert!(!model.vm(KIT).unwrap().running);
    assert!(model.vm("marsh-kit-other").unwrap().running);
    let before = model.mutations();
    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::QuarantinedVm(_))
    ));
    assert_eq!(model.mutations(), before);
}

#[test]
fn published_kit_create_timeout_is_typed_and_quarantines_the_name() {
    let model = StockModel::new();
    model.fault(verb("create"), Fault::TimedOut { applied: false });
    let adapter = model.adapter();
    let kit = KitFixture::published(KIT);

    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::PreparationTimeout {
            operation: "create kit VM",
            ..
        })
    ));
    // The create may still land: never retry under it implicitly.
    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::QuarantinedVm(_))
    ));
    assert_eq!(model.count("create"), 1);
}

#[test]
fn retained_worker_teardown_is_bounded_when_termination_wedges() {
    let model = StockModel::new();
    let wedge = model.wedge_worker_termination();
    let adapter = Arc::new(
        StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
            .with_retained_process_teardown_timeout(Duration::from_millis(30)),
    );
    let kit = KitFixture::published(KIT);
    let ready = adapter.ensure_kit_vm(&kit.spec).unwrap();

    let reset = {
        let adapter = Arc::clone(&adapter);
        let spec = kit.spec.clone();
        bounded(move || adapter.reset_kit_vm(&spec))
    };
    assert!(matches!(
        reset,
        Err(SbxError::Io(ref error)) if error.kind() == io::ErrorKind::TimedOut
    ));
    assert!(wedge.entered() >= 1);
    // The lease whose teardown is uncertain is never handed out again.
    assert!(!adapter.revalidate_cached_kit(&kit.spec, &ready).unwrap());
    wedge.release();
}

/// A Kit VM keeps a grant mount for every project it worked in. Once one is
/// deleted, stock refuses to start the stopped VM; the next daemon (an
/// upgrade, a restart) drops that saved mount and reuses the VM, instead of
/// calling it foreign until a reset. A lost mount marsh did not make stays.
#[test]
fn a_stopped_kit_vm_whose_project_was_deleted_starts_again_for_the_next_daemon() {
    let model = StockModel::new();
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    let root = scratch();
    let kept = directory(root.path(), "kept");
    let gone = directory(root.path(), "gone");
    for (attempt, source) in [("a1", &kept), ("a2", &gone)] {
        let job = adapter
            .prepare_grants(
                &ready,
                attempt,
                &[grant(
                    source.clone(),
                    "/Users/alice/project",
                    MountAccess::ReadWrite,
                )],
            )
            .unwrap();
        adapter.revoke_grants(&job).unwrap();
    }
    drop(adapter);
    fs::remove_dir_all(&gone).unwrap();
    model.with_vm(KIT, |vm| vm.running = false);
    let id = model.vm(KIT).unwrap().id;

    let next = model.adapter();
    next.ownership.record(KIT, &id).unwrap();
    let warm = next.ensure_kit_vm(&kit.spec).unwrap();
    assert!(!warm.cold_started);
    assert_eq!(model.vm(KIT).unwrap().id, id);
    let mounts = model.vm(KIT).unwrap().mounts;
    assert_eq!(mounts.len(), 1, "{mounts:?}");
    assert!(mounts.values().all(|(host, _)| host == &kept));

    // Control: a lost mount outside marsh's grant root is never removed.
    drop(next);
    let other = directory(root.path(), "other");
    model.with_vm(KIT, |vm| {
        vm.mounts
            .insert("/opt/user-mount".into(), (other.clone(), "rw".into()));
        vm.running = false;
    });
    fs::remove_dir_all(&other).unwrap();
    let last = model.adapter();
    last.ownership.record(KIT, &id).unwrap();
    assert!(matches!(
        last.ensure_kit_vm(&kit.spec),
        Err(SbxError::ForeignVm(_))
    ));
    assert!(
        model
            .vm(KIT)
            .unwrap()
            .mounts
            .contains_key(Path::new("/opt/user-mount"))
    );
}
