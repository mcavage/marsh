//! Host grants on Kit VMs: stock mounts, source fencing, retention, rollback
//! and concurrency.
use super::stock_model::{verb, verb_in, verb_on};
use super::*;

const KIT: &str = "marsh-kit-fixture";

/// An owned Kit VM in the model, without a worker (grant paths need none).
fn seeded(model: &Arc<StockModel>) -> (Arc<StockSbx>, ReadyKitVm) {
    let adapter = model.adapter();
    model.seed_owned(&adapter, KIT);
    (Arc::new(adapter), kit_vm(KIT))
}

fn rw(source: &Path, target: &str) -> AdmittedHostGrant {
    grant(source.to_owned(), target, MountAccess::ReadWrite)
}

#[test]
fn grants_mount_each_source_at_an_opaque_vm_path_for_its_natural_target() {
    let root = scratch();
    for project in ["project", "grant:colon,equals=value"] {
        let model = StockModel::new();
        let (adapter, vm) = seeded(&model);
        let project = directory(root.path(), project);
        let home = directory(root.path(), "home");
        let prepared = adapter
            .prepare_grants(
                &vm,
                "attempt_01",
                &[
                    rw(&project, "/Users/alice/project"),
                    rw(&home, "/Users/alice"),
                ],
            )
            .unwrap();

        let mounts = prepared.job_mounts();
        assert_eq!(mounts[0].target, Path::new("/Users/alice/project"));
        assert_eq!(mounts[1].target, Path::new("/Users/alice"));
        assert!(
            mounts
                .iter()
                .all(|mount| mount.access == MountAccess::ReadWrite)
        );
        assert!(
            mounts
                .iter()
                .all(|mount| mount.source.starts_with("/run/marsh/grants/shared"))
        );
        assert_ne!(mounts[0].source, mounts[1].source);
        // Stock mounted exactly these host directories at those VM paths.
        assert_eq!(
            model.vm(KIT).unwrap().mounts,
            BTreeMap::from([
                (mounts[0].source.clone(), (project.clone(), "rw".to_owned())),
                (mounts[1].source.clone(), (home.clone(), "rw".to_owned())),
            ])
        );
    }
}

#[test]
fn admitted_grant_rejects_symlink_and_directory_replacements_before_mount() {
    for replace_with_symlink in [true, false] {
        let root = scratch();
        let source = directory(root.path(), "project");
        let broader = directory(root.path(), "broader");
        let admitted = rw(&source, "/Users/alice/project");
        let admitted_metadata = fs::metadata(&source).unwrap();
        let admitted_identity = (admitted_metadata.dev(), admitted_metadata.ino());
        fs::rename(&source, root.path().join("admitted-project")).unwrap();
        if replace_with_symlink {
            std::os::unix::fs::symlink(&broader, &source).unwrap();
        } else {
            fs::create_dir(&source).unwrap();
        }
        // The grant still pins the object it admitted.
        assert_eq!(admitted.source_identity(), admitted_identity);

        let model = StockModel::new();
        let error = model
            .adapter()
            .prepare_grants(&kit_vm(KIT), "attempt-swapped", &[admitted])
            .unwrap_err();
        assert!(matches!(
            error,
            SbxError::UnsafePath(path) | SbxError::SourceChanged(path) if path == source
        ));
        assert!(model.calls().is_empty());
    }
}

#[test]
fn grant_preparation_never_copies_lifecycle_or_agent_home_into_selected_home() {
    let root = scratch();
    let lifecycle = directory(root.path(), "lifecycle");
    let home = directory(root.path(), "home");
    fs::write(lifecycle.join("credential-file"), b"must-stay-private").unwrap();
    fs::write(lifecycle.join("agent-home-token"), b"must-stay-private").unwrap();
    let model = StockModel::new();
    let (adapter, mut vm) = seeded(&model);
    vm.lifecycle_workspace = lifecycle;

    let prepared = adapter
        .prepare_grants(&vm, "attempt_canary", &[rw(&home, "/Users/alice")])
        .unwrap();

    assert_eq!(prepared.job_mounts().len(), 1);
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    let guest = model.vm(KIT).unwrap();
    assert!(guest.files.is_empty());
    assert_eq!(
        guest.mounts.values().cloned().collect::<Vec<_>>(),
        [(home, "rw".to_owned())]
    );
}

type Scenario = fn(&Arc<StockSbx>, &ReadyKitVm, &AdmittedHostGrant);

/// Every way a grant is acquired and fully released. In each, the one stock
/// mount outlives the last release (no umount while the VM is warm), the next
/// acquisition reuses it without a stock call, and retiring the VM ends it.
#[test]
fn kit_grant_mount_is_retained_after_every_release_until_the_vm_is_retired() {
    let scenarios: [(&str, Scenario); 6] = [
        ("sequential jobs", |adapter, vm, grant| {
            for attempt in ["attempt-1", "attempt-2"] {
                let job = adapter
                    .prepare_grants(vm, attempt, std::slice::from_ref(grant))
                    .unwrap();
                adapter.revoke_grants(&job).unwrap();
            }
        }),
        ("overlapping jobs", |adapter, vm, grant| {
            let first = adapter
                .prepare_grants(vm, "attempt-1", std::slice::from_ref(grant))
                .unwrap();
            let second = adapter
                .prepare_grants(vm, "attempt-2", std::slice::from_ref(grant))
                .unwrap();
            assert_eq!(first.job_mounts(), second.job_mounts());
            adapter.revoke_grants(&first).unwrap();
            adapter.revoke_grants(&second).unwrap();
        }),
        (
            "session pin across sequential jobs",
            |adapter, vm, grant| {
                adapter
                    .prepare_session_grants(vm, "session-one", std::slice::from_ref(grant))
                    .unwrap();
                for attempt in ["attempt-1", "attempt-2"] {
                    let job = adapter
                        .prepare_grants(vm, attempt, std::slice::from_ref(grant))
                        .unwrap();
                    adapter.revoke_grants(&job).unwrap();
                }
                adapter
                    .release_session_grants(&vm.name, "session-one")
                    .unwrap();
            },
        ),
        ("jobs outlive the session pin", |adapter, vm, grant| {
            adapter
                .prepare_session_grants(vm, "session-one", std::slice::from_ref(grant))
                .unwrap();
            let first = adapter
                .prepare_grants(vm, "attempt-1", std::slice::from_ref(grant))
                .unwrap();
            let second = adapter
                .prepare_grants(vm, "attempt-2", std::slice::from_ref(grant))
                .unwrap();
            adapter
                .release_session_grants(&vm.name, "session-one")
                .unwrap();
            adapter.revoke_grants(&first).unwrap();
            adapter.revoke_grants(&second).unwrap();
        }),
        ("two sessions pin one mount", |adapter, vm, grant| {
            for session in ["session-one", "session-two"] {
                adapter
                    .prepare_session_grants(vm, session, std::slice::from_ref(grant))
                    .unwrap();
            }
            for session in ["session-one", "session-two"] {
                adapter.release_session_grants(&vm.name, session).unwrap();
            }
        }),
        ("two sessions pin concurrently", |adapter, vm, grant| {
            let pins = ["session-one", "session-two"].map(|session| {
                let (adapter, vm, grant) = (Arc::clone(adapter), vm.clone(), grant.clone());
                thread::spawn(move || adapter.prepare_session_grants(&vm, session, &[grant]))
            });
            for pin in pins {
                pin.join().unwrap().unwrap();
            }
            adapter.close_session_grants("session-one").unwrap();
            adapter.close_session_grants("session-two").unwrap();
        }),
    ];
    for (name, scenario) in scenarios {
        let root = scratch();
        let source = directory(root.path(), "project");
        let model = StockModel::new();
        let (adapter, vm) = seeded(&model);
        let admitted = rw(&source, "/Users/alice/project");

        scenario(&adapter, &vm, &admitted);
        assert_eq!(model.count("mount"), 1, "{name}");
        assert_eq!(model.count("umount"), 0, "{name}");
        let mounted = model.vm(KIT).unwrap().mounts;
        assert_eq!(mounted.len(), 1, "{name}");
        assert!(mounted.values().all(|(host, _)| host == &source), "{name}");

        let again = adapter
            .prepare_grants(&vm, "attempt-again", std::slice::from_ref(&admitted))
            .unwrap();
        adapter.revoke_grants(&again).unwrap();
        assert_eq!(model.count("mount"), 1, "{name}: reuse needs no stock call");

        let spec = KitFixture::published(KIT).spec;
        assert!(adapter.reset_kit_vm(&spec).unwrap(), "{name}");
        assert!(model.vm(KIT).is_none(), "{name}");
    }
}

#[test]
fn grant_rollback_is_complete_only_when_every_partial_mount_is_revoked() {
    for umount_succeeds in [true, false] {
        let root = scratch();
        let first = directory(root.path(), "first");
        let second = directory(root.path(), "second");
        let model = StockModel::new();
        let (adapter, vm) = seeded(&model);
        // The second mount lands in stock but reports failure.
        model.fault(
            verb_in("mount", second.clone()),
            Fault::FailAfterEffect(failure("second mount rejected")),
        );
        if !umount_succeeds {
            model.fault(
                verb("umount"),
                Fault::Respond(failure("mount remains busy")),
            );
        }

        let error = adapter
            .prepare_grants(
                &vm,
                "attempt-partial",
                &[
                    rw(&first, "/Users/alice/first"),
                    rw(&second, "/Users/alice/second"),
                ],
            )
            .unwrap_err();

        assert_eq!(error.grant_rollback_complete(), Some(umount_succeeds));
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("second mount rejected"), "{diagnostic}");
        assert!(diagnostic.contains(&format!("rollback complete: {umount_succeeds}")));
        let mounted = model.vm(KIT).unwrap().mounts;
        // The first mount is retained; the second is gone only if revoked.
        assert!(mounted.values().any(|(host, _)| host == &first));
        assert_eq!(
            mounted.values().any(|(host, _)| host == &second),
            !umount_succeeds
        );
    }
}

/// A mount stock refused before any effect leaves nothing to roll back. The
/// rollback's `umount` answers "nothing to unmount", which proves the target
/// is not bound: the rollback is complete, and the VM is neither quarantined
/// nor stopped. A rollback `umount` that fails otherwise stays uncertain
/// (`failed_session_pin_rollback_quarantines_and_stops_the_exact_vm`).
#[test]
fn a_mount_refused_before_effect_rolls_back_completely_without_quarantine() {
    let root = scratch();
    let first = directory(root.path(), "first");
    let refused = directory(root.path(), "refused");
    let later = directory(root.path(), "later");
    let model = StockModel::new();
    let (adapter, vm) = seeded(&model);
    for (grants, retained) in [
        (vec![rw(&refused, "/Users/alice/refused")], false),
        // The refused target's host is unexposed; another one stays bound.
        (
            vec![
                rw(&first, "/Users/alice/first"),
                rw(&refused, "/Users/alice/refused"),
            ],
            true,
        ),
    ] {
        model.fault(
            verb_in("mount", refused.clone()),
            Fault::Respond(failure("Error: mount policy denied")),
        );
        let error = adapter
            .prepare_grants(&vm, "attempt-refused", &grants)
            .unwrap_err();
        assert_eq!(error.grant_rollback_complete(), Some(true), "{error}");
        assert!(error.to_string().contains("mount policy denied"), "{error}");
        let mounts = model.vm(KIT).unwrap().mounts;
        assert_eq!(mounts.values().any(|(host, _)| host == &first), retained);
        assert!(!mounts.values().any(|(host, _)| host == &refused));
    }
    assert_eq!(model.count("umount"), 2);
    assert!(model.vm(KIT).unwrap().running);
    adapter
        .prepare_session_grants(&vm, "session-later", &[rw(&later, "/Users/alice/later")])
        .unwrap();
}

#[test]
fn failed_session_pin_rollback_quarantines_and_stops_the_exact_vm() {
    let root = scratch();
    let first = directory(root.path(), "first");
    let second = directory(root.path(), "second");
    let model = StockModel::new();
    let (adapter, vm) = seeded(&model);
    model.seed_vm("user-vm");
    model.fault(
        verb_in("mount", second.clone()),
        Fault::FailAfterEffect(failure("second mount failed")),
    );
    model.fault(
        verb("umount"),
        Fault::Respond(failure("rollback unmount failed")),
    );

    let error = adapter
        .prepare_session_grants(
            &vm,
            "session-one",
            &[
                rw(&first, "/Users/alice/first"),
                rw(&second, "/Users/alice/second"),
            ],
        )
        .unwrap_err();

    assert!(matches!(
        error,
        SbxError::GrantPreparationFailed {
            rollback_complete: false,
            ..
        }
    ));
    assert!(!model.vm(KIT).unwrap().running);
    assert!(model.vm("user-vm").unwrap().running);
    assert!(matches!(
        adapter.prepare_session_grants(&vm, "session-two", &[rw(&first, "/Users/alice/first")]),
        Err(SbxError::QuarantinedVm(_))
    ));
}

#[test]
fn a_live_grant_rejects_a_replaced_source_or_another_access_mode_without_stock_effects() {
    let root = scratch();
    let source = directory(root.path(), "project");
    let model = StockModel::new();
    let (adapter, vm) = seeded(&model);
    adapter
        .prepare_session_grants(&vm, "session-one", &[rw(&source, "/Users/alice/project")])
        .unwrap();

    let read_only = grant(
        source.clone(),
        "/Users/alice/project",
        MountAccess::ReadOnly,
    );
    assert!(matches!(
        adapter.prepare_grants(&vm, "attempt-read", &[read_only]),
        Err(SbxError::GrantPreparationFailed {
            rollback_complete: true,
            ..
        })
    ));
    fs::rename(&source, root.path().join("displaced")).unwrap();
    fs::create_dir(&source).unwrap();
    let replaced = adapter
        .prepare_session_grants(&vm, "session-two", &[rw(&source, "/Users/alice/project")])
        .unwrap_err();
    assert!(
        replaced.to_string().contains("source changed"),
        "{replaced}"
    );
    assert_eq!(model.count("mount"), 1);
    assert_eq!(model.count("umount"), 0);
}

/// A daemon restart finds its previous generation's mount still saved in the
/// VM. v0.47.0 re-mounts idempotently; a stock that reports the target as
/// already mounted (unverified wording) is reclaimed exactly once.
#[test]
fn a_mount_left_by_a_previous_daemon_is_reused_or_reclaimed_once() {
    for conflicts in [false, true] {
        let root = scratch();
        let source = directory(root.path(), "project");
        let model = StockModel::new();
        let (previous, vm) = seeded(&model);
        previous
            .prepare_session_grants(&vm, "session-old", &[rw(&source, "/Users/alice/project")])
            .unwrap();
        drop(previous);
        if conflicts {
            model.set_remount_conflicts();
        }

        let restarted = model.adapter();
        restarted
            .prepare_session_grants(&vm, "session-new", &[rw(&source, "/Users/alice/project")])
            .unwrap();
        let mounted = model.vm(KIT).unwrap().mounts;
        assert_eq!(mounted.len(), 1);
        assert!(mounted.values().all(|(host, _)| host == &source));
        assert_eq!(model.count("umount"), usize::from(conflicts));
        assert_eq!(model.count("mount"), if conflicts { 3 } else { 2 });
    }
}

#[test]
fn grant_transitions_on_unrelated_vms_progress_while_another_mount_blocks() {
    let root = scratch();
    let model = StockModel::new();
    let adapter = Arc::new(model.adapter());
    model.seed_owned(&adapter, "marsh-kit-first");
    model.seed_owned(&adapter, "marsh-kit-second");
    let blocked = model.gate(verb_on("mount", "marsh-kit-first".into()));
    let first = {
        let adapter = Arc::clone(&adapter);
        let admitted = rw(&directory(root.path(), "first"), "/Users/alice/project");
        thread::spawn(move || {
            adapter.prepare_session_grants(&kit_vm("marsh-kit-first"), "session-one", &[admitted])
        })
    };
    blocked.wait_entered(1);

    let second = {
        let adapter = Arc::clone(&adapter);
        let admitted = rw(&directory(root.path(), "second"), "/Users/alice/project");
        bounded(move || {
            adapter.prepare_session_grants(&kit_vm("marsh-kit-second"), "session-two", &[admitted])
        })
    };
    second.unwrap();
    blocked.release();
    first.join().unwrap().unwrap();
}

#[test]
fn same_vm_stock_mounts_are_serialized_without_serializing_reference_hits() {
    let root = scratch();
    let model = StockModel::new();
    let (adapter, vm) = seeded(&model);
    let cached = rw(&directory(root.path(), "cached"), "/Users/alice/cached");
    adapter
        .prepare_session_grants(&vm, "session-pinned", std::slice::from_ref(&cached))
        .unwrap();
    let mounting = model.gate(verb_on("mount", KIT.into()));

    let spawn = |session: &'static str, name: &str| {
        let adapter = Arc::clone(&adapter);
        let vm = vm.clone();
        let admitted = rw(
            &directory(root.path(), name),
            &format!("/Users/alice/{name}"),
        );
        let (send, receive) = mpsc::channel();
        thread::spawn(move || send.send(adapter.prepare_session_grants(&vm, session, &[admitted])));
        receive
    };
    let first = spawn("session-first", "first");
    mounting.wait_entered(1);
    let second = spawn("session-second", "second");
    assert_still_blocked(&second, "a second stock mount on the same VM");
    assert_eq!(mounting.entered(), 1);

    // A reference hit needs no stock call and does not wait for the mount.
    let hit = {
        let adapter = Arc::clone(&adapter);
        let vm = vm.clone();
        bounded(move || adapter.prepare_session_grants(&vm, "session-hit", &[cached]))
    };
    hit.unwrap();

    mounting.release();
    first.recv_timeout(WAIT_LIMIT).unwrap().unwrap();
    second.recv_timeout(WAIT_LIMIT).unwrap().unwrap();
    assert_eq!(model.max_concurrent("mount", KIT), 1);
    assert_eq!(model.count("mount"), 3);
}

#[test]
fn session_close_waits_for_an_inflight_pin_and_rejects_late_lazy_preparation() {
    let root = scratch();
    let source = directory(root.path(), "project");
    let model = StockModel::new();
    let (adapter, vm) = seeded(&model);
    let mounting = model.gate(verb("mount"));
    let preparing = {
        let (adapter, vm) = (Arc::clone(&adapter), vm.clone());
        let admitted = rw(&source, "/Users/alice/project");
        thread::spawn(move || adapter.prepare_session_grants(&vm, "session-racing", &[admitted]))
    };
    mounting.wait_entered(1);
    let (close_send, close_receive) = mpsc::channel();
    {
        let adapter = Arc::clone(&adapter);
        thread::spawn(move || close_send.send(adapter.close_session_grants("session-racing")));
    }
    assert_still_blocked(&close_receive, "session close");
    mounting.release();

    preparing.join().unwrap().unwrap();
    close_receive.recv_timeout(WAIT_LIMIT).unwrap().unwrap();
    assert!(adapter.pinned_session_vms("session-racing").is_empty());
    assert!(matches!(
        adapter.prepare_session_grants(&vm, "session-racing", &[rw(&source, "/Users/alice/project")]),
        Err(SbxError::ClosedSession(session)) if session == "session-racing"
    ));
    assert_eq!(model.count("mount"), 1);
}

#[test]
fn busy_shell_session_close_fails_fast_without_closing_the_session() {
    let root = scratch();
    let source = directory(root.path(), "project");
    let model = StockModel::new();
    let (adapter, vm) = seeded(&model);
    let mounting = model.gate(verb("mount"));
    let preparing = {
        let (adapter, vm) = (Arc::clone(&adapter), vm.clone());
        let admitted = rw(&source, "/Users/alice/project");
        thread::spawn(move || adapter.prepare_session_grants(&vm, "session-busy", &[admitted]))
    };
    mounting.wait_entered(1);

    let close = {
        let adapter = Arc::clone(&adapter);
        bounded(move || adapter.close_shell_session_grants("session-busy"))
    };
    assert!(matches!(close, Err(SbxError::LifecycleDeadline { .. })));
    mounting.release();
    preparing.join().unwrap().unwrap();
    // The refused close did not end the session: its pin completed.
    assert_eq!(adapter.pinned_session_vms("session-busy"), [KIT]);
    adapter.close_shell_session_grants("session-busy").unwrap();
    assert!(adapter.pinned_session_vms("session-busy").is_empty());
}

#[test]
fn shell_session_close_gives_up_after_its_bounded_wait_for_a_vm_transition() {
    let root = scratch();
    let source = directory(root.path(), "project");
    let model = StockModel::new();
    let wait = Duration::from_millis(100);
    let adapter = Arc::new(
        StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
            .with_shell_close_transition_wait(wait),
    );
    model.seed_owned(&adapter, KIT);
    adapter
        .prepare_session_grants(
            &kit_vm(KIT),
            "session-one",
            &[rw(&source, "/Users/alice/project")],
        )
        .unwrap();
    // A quarantine stop holds the VM's transition fence across its stock call.
    let stopping = model.gate(verb("stop"));
    let stop = {
        let adapter = Arc::clone(&adapter);
        thread::spawn(move || adapter.stop_quarantined(&kit_vm(KIT)))
    };
    stopping.wait_entered(1);

    let started = Instant::now();
    let close = {
        let adapter = Arc::clone(&adapter);
        bounded(move || adapter.close_shell_session_grants("session-one"))
    };
    assert!(matches!(close, Err(SbxError::LifecycleDeadline { .. })));
    assert!(started.elapsed() >= wait);
    stopping.release();
    stop.join().unwrap().unwrap();
}

#[test]
fn three_pinned_warm_preparations_and_starts_share_one_vm_without_stock_effects() {
    let model = StockModel::new();
    let (adapter, kit, ready) = warm_kit(&model, KIT);
    let adapter = Arc::new(adapter);
    let admitted = rw(
        &directory(kit.dir.path(), "project"),
        "/Users/alice/project",
    );
    adapter
        .prepare_session_grants(&ready, "session-warm", std::slice::from_ref(&admitted))
        .unwrap();
    let before = model.mutations();
    let barrier = Arc::new(Barrier::new(3));
    let tasks = (0..3)
        .map(|sequence| {
            let (adapter, ready, admitted) =
                (Arc::clone(&adapter), ready.clone(), admitted.clone());
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let attempt = format!("attempt-warm-{sequence}");
                barrier.wait();
                let grants = adapter
                    .prepare_job_grants(&ready, "session-warm", &attempt, &[admitted])
                    .unwrap();
                let channel = adapter
                    .launch_worker(&ready, &grants, &attempt, job_spec())
                    .unwrap();
                (grants, channel)
            })
        })
        .collect::<Vec<_>>();
    let active = tasks
        .into_iter()
        .map(|task| task.join().unwrap())
        .collect::<Vec<_>>();

    model.wait_worker_starts(KIT, 3);
    assert_eq!(model.mutations(), before);
    for (grants, channel) in active {
        drop(channel);
        adapter.revoke_grants(&grants).unwrap();
    }
    adapter.release_session_grants(KIT, "session-warm").unwrap();
    assert_eq!(model.mutations(), before);
    assert_eq!(model.retained_spawns(WORKER_PATH), 1);
}
