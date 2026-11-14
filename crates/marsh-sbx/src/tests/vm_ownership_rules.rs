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
        adapter.stop_quarantined_name("unrecorded", "test".into()),
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
fn admission_preflight_refuses_live_grants_but_not_idle_retained_ones() {
    let root = scratch();
    let project = root.path().join("project");
    let nested = project.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let model = StockModel::new();
    let adapter = model.adapter();
    model.seed_owned(&adapter, "marsh-kit-fixture");
    let vm = kit_vm("marsh-kit-fixture");
    let rw = MountAccess::ReadWrite;

    let live = adapter
        .prepare_grants(
            &vm,
            "attempt-active",
            &[grant(project.clone(), "/Users/alice/project", rw)],
        )
        .unwrap();
    // A job holds the parent: the nested source is refused at admission too.
    assert!(matches!(
        adapter.preflight_host_grants_evicting_idle(&[grant(
            nested.clone(),
            "/Users/alice/nested",
            rw
        )]),
        Err(SbxError::HostGrantFence(message)) if message.contains("overlaps active grant")
    ));

    // The job ends; the mount stays retained while the VM is warm, but idle.
    adapter.revoke_grants(&live).unwrap();
    let calls = model.calls().len();
    adapter
        .preflight_host_grants_evicting_idle(&[grant(nested.clone(), "/Users/alice/nested", rw)])
        .unwrap();
    assert_eq!(
        model.calls().len(),
        calls,
        "admission runs no stock command"
    );
    // The mount step itself still evicts the idle parent before it re-checks.
    adapter
        .prepare_grants(
            &vm,
            "attempt-nested",
            &[grant(nested, "/Users/alice/nested", rw)],
        )
        .unwrap();
    assert_eq!(model.count("umount"), 1);
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
    assert_eq!(model.vm_names(), [] as [std::string::String; 0]);
}

#[test]
fn ownership_mirror_holds_every_intent_before_create_and_tracks_removal() {
    let control = scratch();
    let registry = scratch();
    let map = control.path().join("vm-ownership.json");
    fs::set_permissions(registry.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mirror = registry.path().join("vm-ownership.json");
    let model = StockModel::new();
    let first = StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map.clone())
        .unwrap();
    let kept = first.vm_name(VmPurpose::Kit, "kit-a").unwrap();
    drop(first);
    // A restarted daemon given a mirror copies the existing map at once.
    let adapter = StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map.clone())
        .unwrap()
        .with_vm_ownership_mirror(mirror.clone())
        .unwrap();
    assert!(fs::read_to_string(&mirror).unwrap().contains(&kept));
    // A new intent reaches the mirror before any stock call.
    let name = adapter.vm_name(VmPurpose::Kit, "kit-b").unwrap();
    assert!(model.calls().is_empty());
    assert!(fs::read_to_string(&mirror).unwrap().contains(&name));
    let uuid = model.seed_vm(&name);
    adapter.adopt_created(&name).unwrap();
    assert!(fs::read_to_string(&mirror).unwrap().contains(&uuid));
    assert_eq!(fs::read(&mirror).unwrap(), fs::read(&map).unwrap());
    assert!(
        adapter
            .remove_owned_vm(&name, Instant::now() + Duration::from_secs(5))
            .unwrap()
    );
    assert!(!fs::read_to_string(&mirror).unwrap().contains(&name));
    assert_eq!(fs::read(&mirror).unwrap(), fs::read(&map).unwrap());
    let mode = fs::metadata(&mirror).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // A mirror in a group- or world-accessible directory is refused.
    let open = registry.path().join("open");
    fs::create_dir(&open).unwrap();
    fs::set_permissions(&open, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
            .with_vm_ownership(map)
            .unwrap()
            .with_vm_ownership_mirror(open.join("vm-ownership.json")),
        Err(SbxError::HostGrantFence(_))
    ));
}

#[test]
fn a_malformed_inventory_row_never_makes_an_owned_vm_absent() {
    let model = StockModel::new();
    let home = tempfile::tempdir().unwrap();
    let adapter = model
        .adapter()
        .with_vm_ownership(home.path().join("vm-ownership.json"))
        .unwrap();
    let created = model.seed_vm("mine");
    adapter.adopt_created("mine").unwrap();
    model.drop_name_from_inventory("mine");
    adapter.ownership.invalidate();
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(adapter.remove_owned_vm("mine", deadline).is_err());
    assert!(adapter.classify_vm("mine", true).is_err());
    // Ownership is kept, and nothing was removed.
    assert_eq!(
        adapter.ownership.uuid("mine").as_deref(),
        Some(created.as_str())
    );
    assert_eq!(model.mutations(), 0);
}

/// Write `value` as an owner-only ownership map, as a daemon would.
fn write_map(path: &Path, value: &serde_json::Value) {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(&serde_json::to_vec_pretty(value).unwrap())
        .unwrap();
}

/// A grant as an older `marsh --dev` daemon persisted it.
fn legacy_grant(names: &serde_json::Value, uncertain: Option<bool>) -> serde_json::Value {
    let mut grant = serde_json::json!({
        "session": "session-1",
        "prefix": "marsh-xab12c-",
        "roots": ["/Users/example/marsh"],
        "scratch": "/Users/example/Library/Caches/marsh/dev/0123456789abcdef",
        "max_vms": 4,
        "revoked": true,
        "names": names,
    });
    if let Some(uncertain) = uncertain {
        grant["cleanup_uncertain"] = uncertain.into();
    }
    grant
}

/// The retired entries in the map file at `path`, and whether it still has
/// a `grants` section.
fn retired_on_disk(path: &Path) -> (Vec<String>, bool) {
    let written: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let retired = written["vms"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, entry)| entry["purpose"] == "retired")
        .map(|(name, _)| name.clone())
        .collect();
    (retired, written.get("grants").is_some())
}

/// `marsh --dev` was removed. A map an older daemon left with development
/// grants (every historical field, `cleanup_uncertain` present or not, and
/// one it never had) and a dev shell VM still loads. Loading alone, with no
/// mirror (as every installed daemon runs), rewrites the map at once with
/// what `--dev` left retired; a mirror added later gets the same. The
/// retired-VM pass removes it by recorded UUID. A child name now held by
/// another VM is forgotten and that VM is never named in any stock call; one
/// whose VM is gone is forgotten; the ordinary shell stays.
#[test]
fn an_older_maps_dev_grants_and_dev_shell_are_retired_and_removed_by_uuid() {
    let control = scratch();
    let map = control.path().join("vm-ownership.json");
    let mirror_dir = control.path().join("mirror");
    fs::create_dir(&mirror_dir).unwrap();
    fs::set_permissions(&mirror_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let mirror = mirror_dir.join("vm-ownership.json");
    let model = StockModel::new();
    let shell = model.seed_vm("marsh-s-shell001");
    let dev_shell = model.seed_vm("marsh-s-devshl01");
    let child = model.seed_vm("marsh-xab12c-k-child001");
    model.seed_vm("marsh-xab12c-k-intent01");
    model.seed_vm("marsh-xab12c-s-replaced");
    let mut uncertain = legacy_grant(
        &serde_json::json!({
            "marsh-xab12c-s-replaced": FOREIGN,
            "marsh-xab12c-k-gone0001": "22222222-2222-4222-8222-222222222222",
        }),
        Some(true),
    );
    uncertain["depth"] = 2.into();
    write_map(
        &map,
        &serde_json::json!({
            "version": 2,
            "vms": {
                "marsh-s-shell001": {"purpose": "shell", "key": "shell", "uuid": shell},
                "marsh-s-devshl01": {"purpose": "shell", "key": "dev", "uuid": dev_shell},
            },
            "grants": {
                "grant-a": legacy_grant(
                    &serde_json::json!({
                        "marsh-xab12c-k-child001": child,
                        "marsh-xab12c-k-intent01": null,
                    }),
                    None,
                ),
                "grant-b": uncertain,
            },
        }),
    );

    let retired = [
        "marsh-s-devshl01",
        "marsh-xab12c-k-child001",
        "marsh-xab12c-k-gone0001",
        "marsh-xab12c-k-intent01",
        "marsh-xab12c-s-replaced",
    ];
    // The load itself saves, before anything else uses the map.
    drop(VmOwnership::persisted(map.clone()).unwrap());
    assert_eq!(
        retired_on_disk(&map),
        (retired.map(String::from).to_vec(), false)
    );

    let adapter = StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map.clone())
        .unwrap()
        .with_vm_ownership_mirror(mirror.clone())
        .unwrap();
    assert_eq!(
        retired_on_disk(&mirror),
        (retired.map(String::from).to_vec(), false)
    );
    assert_eq!(model.mutations(), 0, "loading touches no VM");

    let (removed, failed) = adapter.remove_retired_kit_vms();
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(removed, retired);
    assert_eq!(
        model.vm_names(),
        ["marsh-s-shell001", "marsh-xab12c-s-replaced"]
    );
    let named: Vec<_> = model
        .calls()
        .into_iter()
        .filter(|call| {
            call.args
                .iter()
                .any(|arg| arg.contains("marsh-xab12c-s-replaced"))
        })
        .map(|call| call.args)
        .collect();
    assert!(named.is_empty(), "the other VM was named: {named:?}");
    let (removed, failed) = adapter.remove_retired_kit_vms();
    assert!(
        removed.is_empty() && failed.is_empty(),
        "{removed:?} {failed:?}"
    );
    let reloaded = VmOwnership::persisted(map).unwrap();
    assert_eq!(
        reloaded.host_names().into_iter().collect::<Vec<_>>(),
        ["marsh-s-shell001"]
    );
    assert_eq!(reloaded.uuid("marsh-s-shell001"), Some(shell));
}

/// A retired name held by another UUID is forgotten only when the same
/// listing has no VM with the recorded UUID. If the recorded UUID is still
/// listed (under another name), the record and the failure stay, and neither
/// VM is named in a stock call.
#[test]
fn a_retired_name_held_by_another_vm_keeps_a_recorded_uuid_that_is_still_listed() {
    let control = scratch();
    let map = control.path().join("vm-ownership.json");
    let model = StockModel::new();
    let ours = model.seed_vm("marsh-k-renamed1");
    model.seed_vm("marsh-k-retired1");
    write_map(
        &map,
        &serde_json::json!({
            "version": 2,
            "vms": {"marsh-k-retired1": {"purpose": "retired", "key": "fixture", "uuid": ours}},
        }),
    );
    let adapter = StockSbx::new("sbx", Arc::clone(&model) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map.clone())
        .unwrap();

    let (removed, failed) = adapter.remove_retired_kit_vms();
    assert!(removed.is_empty(), "{removed:?}");
    assert!(
        matches!(failed.as_slice(), [(name, SbxError::ForeignVm(_))] if name == "marsh-k-retired1"),
        "{failed:?}"
    );
    let named: Vec<_> = model
        .calls()
        .into_iter()
        .filter(|call| call.args.iter().any(|arg| arg.starts_with("marsh-k-re")))
        .map(|call| call.args)
        .collect();
    assert!(named.is_empty(), "a VM was named: {named:?}");
    assert_eq!(model.vm_names(), ["marsh-k-renamed1", "marsh-k-retired1"]);
    let reloaded = VmOwnership::persisted(map).unwrap();
    assert_eq!(reloaded.uuid("marsh-k-retired1"), Some(ours));
    assert!(reloaded.is_retired("marsh-k-retired1"));
}

/// A grant whose only names are already host entries still counts as
/// something `marsh --dev` left: the load drops `grants` from the map at
/// once, with no mirror, and the host entry keeps its purpose, key and UUID.
#[test]
fn a_grant_name_that_is_a_host_entry_keeps_the_host_entry() {
    let control = scratch();
    let map = control.path().join("vm-ownership.json");
    write_map(
        &map,
        &serde_json::json!({
            "version": 2,
            "vms": {
                "marsh-k-fixture1": {"purpose": "kit", "key": "fixture", "uuid": FOREIGN},
            },
            "grants": {
                "grant-a": legacy_grant(&serde_json::json!({"marsh-k-fixture1": null}), None),
            },
        }),
    );
    let loaded = VmOwnership::persisted(map.clone()).unwrap();
    assert_eq!(retired_on_disk(&map), (Vec::new(), false));
    assert!(!loaded.is_retired("marsh-k-fixture1"));
    assert_eq!(loaded.uuid("marsh-k-fixture1").as_deref(), Some(FOREIGN));
    assert_eq!(
        loaded.lookup(VmPurpose::Kit, "fixture").as_deref(),
        Some("marsh-k-fixture1")
    );
}

/// A map with nothing `marsh --dev` left loads without being rewritten.
#[test]
fn a_map_without_dev_grants_is_not_rewritten_at_load() {
    let control = scratch();
    let map = control.path().join("vm-ownership.json");
    write_map(
        &map,
        &serde_json::json!({
            "version": 2,
            "vms": {
                "marsh-s-shell001": {"purpose": "shell", "key": "shell", "uuid": FOREIGN},
                "marsh-k-intent01": {"purpose": "kit", "key": "fixture"},
            },
        }),
    );
    let before = (fs::read(&map).unwrap(), fs::metadata(&map).unwrap().ino());
    let loaded = VmOwnership::persisted(map.clone()).unwrap();
    // Every write replaces the file, so the same inode means none happened.
    assert_eq!(
        (fs::read(&map).unwrap(), fs::metadata(&map).unwrap().ino()),
        before
    );
    assert_eq!(loaded.uuid("marsh-s-shell001").as_deref(), Some(FOREIGN));
    assert!(!loaded.is_retired("marsh-k-intent01"));
}
