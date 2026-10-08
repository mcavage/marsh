//! Ownership-map and in-daemon source-overlap rules through the real adapter
//! with a recording stock CLI fake. No real SBX is invoked.
use super::*;

const FOREIGN: &str = "11111111-1111-4111-8111-111111111111";
const OTHER: &str = "22222222-2222-4222-8222-222222222222";
const CREATED: &str = "33333333-3333-4333-8333-333333333333";

#[derive(Default)]
struct InventoryRunner {
    vms: Mutex<BTreeMap<String, String>>,
    calls: Mutex<Vec<Vec<String>>>,
}

impl InventoryRunner {
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandRunner for InventoryRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        self.run_bounded(invocation, Duration::from_secs(1))
    }

    fn run_bounded(&self, invocation: &Invocation, _: Duration) -> io::Result<CommandOutput> {
        let args = invocation
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        self.calls.lock().unwrap().push(args.clone());
        let mut vms = self.vms.lock().unwrap();
        let stdout = match args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["ls", "--json"] => serde_json::to_vec(&serde_json::json!({
                "sandboxes": vms.iter().map(|(name, id)| serde_json::json!({
                    "name": name, "id": id, "status": "running", "agent": "shell"
                })).collect::<Vec<_>>()
            }))?,
            ["create", "--name", name] => {
                vms.insert((*name).to_owned(), CREATED.to_owned());
                Vec::new()
            }
            ["rm", "--force", name] => {
                vms.remove(*name);
                Vec::new()
            }
            _ => return Err(io::Error::other(format!("unexpected stock call {args:?}"))),
        };
        Ok(CommandOutput {
            exit_code: Some(0),
            stdout,
            stderr: Vec::new(),
        })
    }

    fn spawn_attached(&self, _: &Invocation) -> io::Result<Attachment> {
        Err(io::Error::other("no attachments in ownership tests"))
    }
}

fn removals(runner: &InventoryRunner) -> Vec<Vec<String>> {
    runner
        .calls()
        .into_iter()
        .filter(|call| {
            call.first()
                .is_some_and(|verb| verb == "rm" || verb == "stop")
        })
        .collect()
}

#[test]
fn ownership_map_refuses_foreign_recreates_absent_and_never_removes_foreign() {
    let control = test_directory("vm-ownership");
    let map = control.join("vm-ownership.json");
    let runner = Arc::new(InventoryRunner::default());
    runner.vms.lock().unwrap().extend([
        ("unrecorded".to_owned(), FOREIGN.to_owned()),
        ("replaced".to_owned(), OTHER.to_owned()),
    ]);
    let adapter = StockSbx::new("sbx", Arc::clone(&runner) as Arc<dyn CommandRunner>)
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
    // Warm classification reuses the cached view: exactly one `ls` so far.
    assert_eq!(
        runner.calls(),
        vec![vec!["ls".to_owned(), "--json".to_owned()]]
    );
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
    assert!(
        removals(&runner).is_empty(),
        "foreign VMs must never be touched"
    );

    // Absent: recreate and record the new UUID durably.
    assert_eq!(adapter.classify_vm("mine", true).unwrap(), VmClass::Absent);
    adapter.run(["create", "--name", "mine"]).unwrap();
    assert_eq!(adapter.adopt_created("mine").unwrap(), CREATED);
    assert_eq!(
        adapter.classify_vm("mine", false).unwrap(),
        VmClass::Owned {
            uuid: CREATED.into(),
            running: true
        }
    );
    let reloaded = VmOwnership::persisted(map.clone()).unwrap();
    assert_eq!(reloaded.uuid("mine").as_deref(), Some(CREATED));

    // Owned removal is fenced to the recorded UUID, addressed by name (local
    // stock SBX rejects UUIDs), then forgets it.
    assert!(adapter.remove_owned_vm("mine", deadline).unwrap());
    assert_eq!(
        removals(&runner),
        vec![vec![
            "rm".to_owned(),
            "--force".to_owned(),
            "mine".to_owned()
        ]]
    );
    assert_eq!(adapter.ownership.uuid("mine"), None);
    assert!(runner.vms.lock().unwrap().contains_key("unrecorded"));
    assert!(runner.vms.lock().unwrap().contains_key("replaced"));
    fs::remove_dir_all(control).unwrap();
}

#[test]
fn source_overlap_is_rejected_within_batch_and_against_active_grants() {
    let root = fs::canonicalize(test_directory("source-overlap")).unwrap();
    let project = root.join("project");
    let nested = project.join("nested");
    let unrelated = root.join("unrelated");
    for path in [&nested, &unrelated] {
        fs::create_dir_all(path).unwrap();
    }
    let runner = Arc::new(InventoryRunner::default());
    let adapter = StockSbx::new("sbx", Arc::clone(&runner) as Arc<dyn CommandRunner>);
    let rw = MountAccess::ReadWrite;

    let batch = [
        grant(project.clone(), "/Users/alice/project", rw),
        grant(nested.clone(), "/Users/alice/nested", rw),
    ];
    assert!(matches!(
        adapter.preflight_host_grants(&batch),
        Err(SbxError::HostGrantFence(message)) if message.contains("overlaps")
    ));

    let active = grant(project.clone(), "/Users/alice/project", rw);
    adapter.grant_mount_references.lock().unwrap().insert(
        (vm().name, project.clone()),
        GrantMountReference {
            identity: active.identity.clone(),
            source_record: source_chain::SourceRecord::new(&active.source, &active.chain),
            access: rw,
            vm_source: shared_grant_path(&project, &active.identity),
            active_jobs: 1,
            pinned_sessions: BTreeMap::new(),
        },
    );
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
    assert!(runner.calls().is_empty(), "admission runs no stock command");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn create_intent_persists_before_create_and_restart_adopts_present_or_drops_absent() {
    let control = test_directory("vm-ownership-intent");
    let map = control.join("vm-ownership.json");
    let runner = Arc::new(InventoryRunner::default());
    let first = StockSbx::new("sbx", Arc::clone(&runner) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map.clone())
        .unwrap();
    let created = first.vm_name(VmPurpose::Kit, "kit-a").unwrap();
    let never = first.vm_name(VmPurpose::Kit, "kit-b").unwrap();
    assert!(created.starts_with("marsh-k-") && created.len() == 16);
    assert_ne!(created, never);
    assert_eq!(first.vm_name(VmPurpose::Kit, "kit-a").unwrap(), created);
    // The intent is durable before any stock call.
    assert!(runner.calls().is_empty());
    assert!(fs::read_to_string(&map).unwrap().contains(&created));
    // Crash after `sbx create` succeeded but before the UUID was recorded.
    runner
        .vms
        .lock()
        .unwrap()
        .insert(created.clone(), CREATED.to_owned());
    drop(first);

    let restarted = StockSbx::new("sbx", Arc::clone(&runner) as Arc<dyn CommandRunner>)
        .with_vm_ownership(map.clone())
        .unwrap();
    assert_eq!(
        restarted.classify_vm(&created, true).unwrap(),
        VmClass::Owned {
            uuid: CREATED.into(),
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
    assert_eq!(
        removals(&runner),
        vec![vec!["rm".to_owned(), "--force".to_owned(), created.clone()]]
    );
    fs::remove_dir_all(control).unwrap();
}
