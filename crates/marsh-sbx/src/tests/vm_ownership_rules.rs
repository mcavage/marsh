//! Ownership-map and in-daemon source-overlap rules through the real adapter
//! against the stock model. No real SBX is invoked.
use super::*;

const FOREIGN: &str = "11111111-1111-4111-8111-111111111111";

#[test]
fn ownership_map_refuses_foreign_recreates_absent_and_never_removes_foreign() {
    let control = scratch();
    let map = control.path().join("vm-ownership.json");
    let model = StockModel::new();
    model.seed_vm("unrecorded");
    model.seed_vm("replaced");
    let adapter = StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map.clone())
        .unwrap();
    // A recorded name whose stock UUID differs was replaced by someone else.
    adapter.ownership.record("replaced", FOREIGN).unwrap();

    // Foreign: present but unrecorded, or present with a different UUID.
    assert_eq!(
        adapter.classify_vm("unrecorded", false).unwrap(),
        VmClass::Foreign
    );
    assert_eq!(
        adapter.classify_vm("replaced", false).unwrap(),
        VmClass::Foreign
    );
    // Warm classification reuses the cached view.
    assert_eq!(model.count("ls"), 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(matches!(
        adapter.remove_owned_vm("unrecorded", deadline),
        Err(SbxError::ForeignVm(_))
    ));
    assert!(matches!(
        adapter.remove_owned_vm("replaced", deadline),
        Err(SbxError::ForeignVm(_))
    ));
    assert!(matches!(
        adapter.stop_quarantined_name("unrecorded"),
        Err(SbxError::ForeignVm(_))
    ));
    assert_eq!(model.mutations(), 0, "foreign VMs must never be touched");

    // Absent: a new VM appears (stock create) and its UUID is recorded durably.
    assert_eq!(adapter.classify_vm("mine", true).unwrap(), VmClass::Absent);
    let created = model.seed_vm("mine");
    assert_eq!(adapter.adopt_created("mine").unwrap(), created);
    assert_eq!(
        adapter.classify_vm("mine", false).unwrap(),
        VmClass::Owned {
            uuid: created.clone(),
            running: true
        }
    );
    let reloaded = VmOwnership::persisted(map).unwrap();
    assert_eq!(reloaded.uuid("mine").as_deref(), Some(created.as_str()));

    // Owned removal is fenced to the recorded UUID, addressed by name (local
    // stock SBX rejects UUIDs), then forgets it.
    assert!(adapter.remove_owned_vm("mine", deadline).unwrap());
    assert_eq!(
        model.calls_of("rm")[0].args.last().map(String::as_str),
        Some("mine")
    );
    assert_eq!(adapter.ownership.uuid("mine"), None);
    assert_eq!(model.vm_names(), ["replaced", "unrecorded"]);
}

#[test]
fn source_overlap_is_rejected_within_batch_and_against_active_grants() {
    let root = scratch();
    let project = root.path().join("project");
    let nested = project.join("nested");
    let unrelated = directory(root.path(), "unrelated");
    fs::create_dir_all(&nested).unwrap();
    let model = StockModel::new();
    let adapter = model.adapter();
    model.seed_owned(&adapter, "marsh-kit-fixture");
    let rw = MountAccess::ReadWrite;

    let batch = [
        grant(project.clone(), "/Users/alice/project", rw),
        grant(nested.clone(), "/Users/alice/nested", rw),
    ];
    assert!(matches!(
        adapter.preflight_host_grants(&batch),
        Err(SbxError::HostGrantFence(message)) if message.contains("overlaps")
    ));

    // An active job grant on the project.
    adapter
        .prepare_grants(
            &kit_vm("marsh-kit-fixture"),
            "attempt-active",
            &[grant(project.clone(), "/Users/alice/project", rw)],
        )
        .unwrap();
    let calls = model.calls().len();
    // A nested source under an active grant is refused; the identical source
    // (shared by shell and Kit VMs) and unrelated sources are admitted.
    assert!(matches!(
        adapter.preflight_host_grants(&[grant(nested, "/Users/alice/nested", rw)]),
        Err(SbxError::HostGrantFence(message)) if message.contains("overlaps active grant")
    ));
    adapter
        .preflight_host_grants(&[grant(project, "/Users/alice/project", rw)])
        .unwrap();
    adapter
        .preflight_host_grants(&[grant(unrelated, "/Users/alice/unrelated", rw)])
        .unwrap();
    assert_eq!(
        model.calls().len(),
        calls,
        "admission runs no stock command"
    );
}

#[test]
fn create_intent_persists_before_create_and_restart_adopts_present_or_drops_absent() {
    let control = scratch();
    let map = control.path().join("vm-ownership.json");
    let model = StockModel::new();
    let first = StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map.clone())
        .unwrap();
    let created = first.vm_name(VmPurpose::Kit, "kit-a").unwrap();
    let never = first.vm_name(VmPurpose::Kit, "kit-b").unwrap();
    assert!(created.starts_with("marsh-k-") && created.len() == 16);
    assert_ne!(created, never);
    assert_eq!(first.vm_name(VmPurpose::Kit, "kit-a").unwrap(), created);
    // The intent is durable before any stock call.
    assert!(model.calls().is_empty());
    assert!(fs::read_to_string(&map).unwrap().contains(&created));
    // Crash after `sbx create` succeeded but before the UUID was recorded.
    let uuid = model.seed_vm(&created);
    drop(first);

    let restarted = StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map)
        .unwrap();
    assert_eq!(
        restarted.classify_vm(&created, true).unwrap(),
        VmClass::Owned {
            uuid,
            running: true
        }
    );
    // The absent startup intent was dropped: the Kit gets a fresh name.
    assert_ne!(restarted.vm_name(VmPurpose::Kit, "kit-b").unwrap(), never);
    assert_eq!(restarted.vm_name(VmPurpose::Kit, "kit-a").unwrap(), created);
    // The adopted VM is removable once fenced to its exact UUID.
    assert!(
        restarted
            .remove_owned_vm(&created, Instant::now() + Duration::from_secs(5))
            .unwrap()
    );
    assert!(model.vm_names().is_empty());
}
