//! Shell VMs: initialization, the retained supervisor, attach, relays, mounts
//! and recovery.
use super::stock_model::verb;
use super::*;

const SHELL: &str = "marsh-shell-cache";

fn ready_shell(model: &Arc<StockModel>) -> (StockSbx, ShellFixture, ReadyShellVm) {
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let vm = adapter.ensure_shell_vm(&shell.spec).unwrap();
    (adapter, shell, vm)
}

#[test]
fn cold_shell_installs_the_shell_and_creates_the_host_identical_user() {
    let model = StockModel::new();
    let (adapter, shell, vm) = ready_shell(&model);
    assert!(vm.cold_started);
    let guest = model.vm(SHELL).unwrap();
    assert_eq!(adapter.ownership.uuid(SHELL), Some(guest.id.clone()));
    assert_eq!(guest.files["/usr/local/bin/marsh"], b"marsh shell");
    assert_eq!(guest.users["alice"], (501, 20));
    let marker =
        serde_json::from_slice::<OwnedWorkerMarker>(&guest.files["/var/lib/marsh/worker.json"])
            .unwrap();
    assert_eq!(
        marker.profile,
        format!("shell:{SHELL}:{}", shell.spec.image.as_str())
    );
    assert!(!guest.files.contains_key(RELAY_PATH));
    assert_eq!(model.retained_spawns("/usr/local/bin/marsh"), 1);
}

#[test]
fn cold_shell_rejects_a_guest_user_with_another_numeric_identity() {
    let model = StockModel::new();
    model.add_template_user("alice", 777, 20);
    let shell = ShellFixture::new();
    assert!(matches!(
        model.adapter().ensure_shell_vm(&shell.spec),
        Err(SbxError::InvalidShellUser)
    ));
    assert_eq!(model.retained_spawns("/usr/local/bin/marsh"), 0);
}

#[test]
fn warm_shell_reuses_validated_initialization_without_stock_calls() {
    let model = StockModel::new();
    let (adapter, shell, _) = ready_shell(&model);
    let calls = model.calls().len();
    assert!(!adapter.ensure_shell_vm(&shell.spec).unwrap().cold_started);
    assert_eq!(model.calls().len(), calls);
    assert_eq!(model.retained_spawns("/usr/local/bin/marsh"), 1);
}

#[test]
fn restarted_daemon_recovers_old_owned_shell_environment_generations() {
    let model = StockModel::new();
    let root = tempfile::tempdir().unwrap();
    let map = root.path().join("ownership.json");
    let first = model.adapter().with_vm_ownership(map.clone()).unwrap();
    let mut shell = ShellFixture::new();
    shell.spec.name = first.vm_name(VmPurpose::Shell, "shell:sbxenv:old").unwrap();
    let old = first.ensure_shell_vm(&shell.spec).unwrap();
    drop(first);
    let restarted = model.adapter().with_vm_ownership(map).unwrap();
    shell.spec.name = restarted
        .vm_name(VmPurpose::Shell, "shell:sbxenv:new")
        .unwrap();
    let recovered = restarted.shell_recovery_specs(&shell.spec);
    assert!(recovered.iter().any(|spec| spec.name == old.name));
    for spec in recovered {
        restarted.reset_shell_vm(&spec).unwrap();
    }
    assert!(model.vm(&old.name).is_none());
}

#[test]
fn shell_vm_is_not_ready_until_its_supervisor_acknowledges_the_generation() {
    let model = StockModel::new();
    model.answer_next_supervisor_with_wrong_generation();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    assert!(matches!(
        adapter.ensure_shell_vm(&shell.spec),
        Err(SbxError::ShellLeaseLost(name)) if name == SHELL
    ));
    // Nothing was cached as ready: the next call starts a new supervisor.
    adapter.ensure_shell_vm(&shell.spec).unwrap();
    assert_eq!(model.retained_spawns("/usr/local/bin/marsh"), 2);
}

#[test]
fn concurrent_cold_shell_initialization_creates_exactly_once() {
    let model = StockModel::new();
    let adapter = Arc::new(model.adapter());
    let shell = ShellFixture::new();
    let starts = [0, 1].map(|_| {
        let (adapter, spec) = (Arc::clone(&adapter), shell.spec.clone());
        thread::spawn(move || adapter.ensure_shell_vm(&spec))
    });
    let [first, second] = starts.map(|start| start.join().unwrap().unwrap());
    assert_ne!(first.cold_started, second.cold_started);
    assert_eq!(model.count("create"), 1);
}

#[test]
fn a_lost_idle_supervisor_is_replaced_after_revalidating_the_vm() {
    let model = StockModel::new();
    let (adapter, shell, _) = ready_shell(&model);
    model.kill_supervisors(SHELL);
    let vm = adapter.ensure_shell_vm(&shell.spec).unwrap();
    assert!(!vm.cold_started);
    assert_eq!(model.count("create"), 1);
    assert_eq!(model.retained_spawns("/usr/local/bin/marsh"), 2);
}

#[test]
fn a_changed_shell_artifact_is_reinstalled() {
    let model = StockModel::new();
    let (adapter, shell, _) = ready_shell(&model);
    fs::write(&shell.spec.shell_binary, b"rebuilt marsh shell").unwrap();
    adapter.ensure_shell_vm(&shell.spec).unwrap();
    assert_eq!(
        model.vm(SHELL).unwrap().files["/usr/local/bin/marsh"],
        b"rebuilt marsh shell"
    );
}

#[test]
fn a_vanished_vm_with_a_dead_idle_supervisor_is_recreated() {
    let model = StockModel::new();
    let (adapter, shell, vm) = ready_shell(&model);
    model.remove_vm(SHELL);
    model.kill_supervisors(SHELL);
    assert!(matches!(
        adapter.attach_shell(&vm, Path::new("/Users/alice/project"), false, None, &[]),
        Err(SbxError::ShellLeaseLost(_))
    ));
    assert!(adapter.ensure_shell_vm(&shell.spec).unwrap().cold_started);
    assert!(model.vm(SHELL).is_some());
}

/// Stock reports a VM missing with `Error: sandbox 'NAME' not found`
/// (captured from v0.47.0). That must drop the ready shell, not leave a
/// cached "warm" VM that no longer exists.
#[test]
fn a_stock_missing_vm_report_invalidates_the_ready_shell() {
    let model = StockModel::new();
    let (adapter, shell, vm) = ready_shell(&model);
    model.remove_vm(SHELL);
    let relay = shell.dir.path().join("relay-binary");
    fs::write(&relay, b"relay").unwrap();
    assert!(matches!(
        adapter.launch_shell_relay(&vm, &relay, "session-1"),
        Err(SbxError::CommandFailed { .. })
    ));
    let recovered = adapter.ensure_shell_vm(&shell.spec).unwrap();
    assert!(recovered.cold_started);
    assert!(model.vm(SHELL).is_some());
}

#[test]
fn shell_attach_preserves_natural_cwd_identity_environment_and_argv() {
    let model = StockModel::new();
    let (adapter, _, vm) = ready_shell(&model);
    let calls = model.calls().len();
    adapter
        .attach_shell(
            &vm,
            Path::new("/Users/alice/project"),
            true,
            Some(TerminalSize {
                rows: 47,
                columns: 123,
            }),
            &[OsString::from("-c"), OsString::from("printf hello")],
        )
        .unwrap();
    // One supervisor frame, no stock process for the session itself.
    assert_eq!(model.calls().len(), calls);
    let [spec] = model.wait_supervisor_starts(1).try_into().unwrap();
    let shell_supervisor::StartKind::Shell {
        record,
        uid,
        terminal,
    } = &spec.kind
    else {
        panic!("shell start expected")
    };
    let record = String::from_utf8(record.clone()).unwrap();
    assert!(record.starts_with("/run/marsh/501/") && record.ends_with("/shell"));
    assert_eq!((*uid, *terminal), (501, Some((47, 123))));
    assert_eq!(spec.program, b"/usr/local/bin/marsh");
    assert_eq!(spec.working_directory, b"/Users/alice/project");
    for (name, value) in [("HOME", "/Users/alice"), ("BASH_ENV", ""), ("ENV", "")] {
        assert!(
            spec.environment
                .contains(&(name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        );
    }
    assert!(spec.arguments.contains(&record.into_bytes()));
    assert!(
        spec.arguments
            .ends_with(&[b"-c".to_vec(), b"printf hello".to_vec()])
    );
}

#[test]
fn shell_attach_exposes_only_relay_paths_and_controls_in_band() {
    let model = StockModel::new();
    let (adapter, _, vm) = ready_shell(&model);
    let calls = model.calls().len();
    let attachment = adapter
        .attach_shell_with_relay(
            &vm,
            Path::new("/Users/alice/project"),
            false,
            None,
            &[],
            Some((
                Path::new("/run/marsh/501/session/s"),
                Path::new("/run/marsh/501/session/t"),
            )),
            &[],
        )
        .unwrap();
    attachment.control.signal(JobSignal::Interrupt).unwrap();
    attachment.control.cleanup_session().unwrap();
    let spec = model.wait_supervisor_starts(1).remove(0);
    assert_eq!(
        spec.kind,
        shell_supervisor::StartKind::Shell {
            record: b"/run/marsh/501/session/shell".to_vec(),
            uid: 501,
            terminal: None,
        }
    );
    for (name, value) in [
        ("MARSH_DAEMON_SOCKET", "/run/marsh/501/session/s"),
        ("MARSH_DAEMON_TOKEN", "/run/marsh/501/session/t"),
    ] {
        assert!(
            spec.environment
                .contains(&(name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        );
    }
    assert!(!format!("{spec:?}").contains("bearer"));
    // The signal is an attempt-keyed frame; cleanup after the reported exit
    // happened in-band; neither starts a stock process.
    let frames = model.supervisor_frames();
    assert!(frames.iter().any(|frame| matches!(
        frame,
        shell_supervisor::Down::Control { op: shell_supervisor::ControlOp::Signal(name), .. }
            if name == "INT"
    )));
    assert!(!frames.iter().any(|frame| matches!(
        frame,
        shell_supervisor::Down::Control {
            op: shell_supervisor::ControlOp::Cleanup,
            ..
        }
    )));
    assert_eq!(model.calls().len(), calls);
}

#[test]
fn lost_supervisor_with_a_live_session_is_uncertain_and_fences_the_vm() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let relay = shell.relay();
    let vm = adapter.ensure_shell_vm(&shell.spec).unwrap();
    let mut session = adapter
        .launch_shell_relay(&vm, &relay, "session-1")
        .unwrap()
        .process;
    assert_eq!(session.process.try_wait().unwrap(), None);
    let calls = model.calls().len();

    // The single retained transport dies with a started attempt.
    model.kill_supervisors(SHELL);
    assert!(matches!(
        adapter.ensure_shell_vm(&shell.spec),
        Err(SbxError::QuarantinedShellVm(name)) if name == SHELL
    ));
    assert!(session.process.try_wait().is_err());
    assert!(session.control.cleanup_session().is_err());
    // Never replayed or replaced underneath the uncertain session.
    assert_eq!(model.calls().len(), calls);
}

#[test]
fn quarantined_shell_rejects_preparation_and_attach_before_stock_effects() {
    let model = StockModel::new();
    let (adapter, shell, vm) = ready_shell(&model);
    let project = shell.project();
    adapter.quarantine_shell(
        &vm,
        vec![grant(project, "/project", MountAccess::ReadWrite)],
    );
    let calls = model.calls().len();
    assert!(matches!(
        adapter.ensure_shell_vm(&shell.spec),
        Err(SbxError::QuarantinedShellVm(_))
    ));
    assert!(matches!(
        adapter.attach_shell(&vm, Path::new("/Users/alice"), false, None, &[]),
        Err(SbxError::QuarantinedShellVm(_))
    ));
    assert_eq!(model.calls().len(), calls);
}

#[test]
fn shell_mounts_project_at_its_natural_path_and_selected_home_at_guest_home() {
    for project_name in ["project", "project:colon,equals=value"] {
        let model = StockModel::new();
        let adapter = model.adapter();
        let shell = ShellFixture::new();
        model.seed_owned(&adapter, SHELL);
        let project = directory(shell.dir.path(), project_name);
        let selected = shell.selected_home();

        adapter
            .prepare_shell_mounts(&shell_vm(SHELL), &shell_grants(&project, &selected))
            .unwrap();
        assert_eq!(
            model.vm(SHELL).unwrap().mounts,
            BTreeMap::from([
                (project.clone(), (project.clone(), "rw".to_owned())),
                (PathBuf::from("/Users/alice"), (selected, "rw".to_owned())),
            ]),
            "{project_name}"
        );
    }
}

#[test]
fn admitted_shell_mount_rejects_a_replaced_source_before_mount() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    model.seed_owned(&adapter, SHELL);
    let project = shell.project();
    let grants = shell_grants(&project, &shell.selected_home());
    fs::rename(&project, shell.dir.path().join("admitted-project")).unwrap();
    fs::create_dir(&project).unwrap();

    assert!(matches!(
        adapter.prepare_shell_mounts(&shell_vm(SHELL), &grants),
        Err(SbxError::SourceChanged(path)) if path == project
    ));
    assert_eq!(model.mutations(), 0);
}

#[test]
fn shell_mounts_are_retained_after_release_until_the_vm_is_reset() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    model.seed_owned(&adapter, SHELL);
    let grants = shell_grants(&shell.project(), &shell.selected_home());
    let vm = shell_vm(SHELL);

    let first = adapter.prepare_shell_mounts(&vm, &grants).unwrap();
    let second = adapter.prepare_shell_mounts(&vm, &grants).unwrap();
    adapter.revoke_shell_mounts(&first).unwrap();
    adapter.revoke_shell_mounts(&second).unwrap();
    assert_eq!(model.vm(SHELL).unwrap().mounts.len(), 2);
    let again = adapter.prepare_shell_mounts(&vm, &grants).unwrap();
    adapter.revoke_shell_mounts(&again).unwrap();
    assert_eq!(model.count("mount"), 2);
    assert_eq!(model.count("umount"), 0);

    assert!(adapter.reset_shell_vm(&shell.spec).unwrap());
    assert!(model.vm(SHELL).is_none());
}

#[test]
fn cold_shell_co_installs_the_packaged_relay() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let relay = shell.relay();
    let vm = adapter.ensure_shell_vm(&shell.spec).unwrap();
    assert_eq!(model.vm(SHELL).unwrap().files[RELAY_PATH], b"relay");
    let calls = model.calls().len();
    adapter
        .launch_shell_relay(&vm, &relay, "session-1")
        .unwrap();
    assert_eq!(model.calls().len(), calls, "relay already installed");
}

#[test]
fn relays_run_as_supervisor_children_on_private_per_session_paths() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    let relay = shell.relay();
    let vm = adapter.ensure_shell_vm(&shell.spec).unwrap();
    let calls = model.calls().len();
    let launches = ["session-1", "session-2"]
        .map(|session| adapter.launch_shell_relay(&vm, &relay, session).unwrap());
    assert_eq!(
        model.calls().len(),
        calls,
        "warm relay launch adds no stock call"
    );

    let starts = model.wait_supervisor_starts(2);
    for ((launch, spec), session) in launches.iter().zip(&starts).zip(["session-1", "session-2"]) {
        let socket = format!("/run/marsh/501/{session}/s");
        let token = format!("/run/marsh/501/{session}/t");
        assert_eq!(launch.socket_path, Path::new(&socket));
        assert_eq!(launch.token_path, Path::new(&token));
        assert_eq!(
            spec.kind,
            shell_supervisor::StartKind::Child { uid: 501, gid: 20 }
        );
        assert_eq!(spec.program, RELAY_PATH.as_bytes());
        // Paths only: the token itself never crosses as an argument.
        assert_eq!(spec.arguments, [socket.into_bytes(), token.into_bytes()]);
        assert_eq!(
            spec.environment,
            [] as [(std::vec::Vec<u8>, std::vec::Vec<u8>); 0]
        );
    }
}

#[test]
fn parallel_relay_launches_install_one_complete_artifact() {
    let model = StockModel::new();
    let (adapter, shell, vm) = ready_shell(&model);
    let adapter = Arc::new(adapter);
    let relay = shell.dir.path().join("relay-binary");
    fs::write(&relay, b"relay").unwrap();
    let copies = model.count("cp");
    let copying = model.gate(verb("cp"));
    let start = Arc::new(Barrier::new(8));
    let launches = (0..8)
        .map(|index| {
            let (adapter, relay, vm, start) = (
                Arc::clone(&adapter),
                relay.clone(),
                vm.clone(),
                Arc::clone(&start),
            );
            thread::spawn(move || {
                start.wait();
                adapter
                    .launch_shell_relay(&vm, &relay, &format!("session-{index}"))
                    .map(drop)
            })
        })
        .collect::<Vec<_>>();
    copying.wait_entered(1);
    copying.release();
    for launch in launches {
        launch.join().unwrap().unwrap();
    }
    assert_eq!(model.count("cp"), copies + 1);
    assert_eq!(model.vm(SHELL).unwrap().files[RELAY_PATH], b"relay");
}

#[test]
fn shell_reset_removes_only_the_exact_owned_vm() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    model.seed_owned(&adapter, SHELL);
    model.seed_vm("marsh-shell-other");

    assert!(adapter.reset_shell_vm(&shell.spec).unwrap());
    assert_eq!(model.vm_names(), ["marsh-shell-other"]);
    assert_eq!(adapter.ownership.uuid(SHELL), None);
}

#[test]
fn shell_reset_refuses_a_foreign_vm_and_an_expired_deadline_before_any_effect() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    assert!(matches!(
        adapter.reset_shell_vm_before(&shell.spec, Instant::now()),
        Err(SbxError::LifecycleDeadline {
            operation: "shell reset admission"
        })
    ));
    assert!(model.calls().is_empty());

    model.seed_vm(SHELL);
    assert!(matches!(
        adapter.reset_shell_vm(&shell.spec),
        Err(SbxError::ForeignVm(_))
    ));
    assert_eq!(model.mutations(), 0);
    assert!(model.vm(SHELL).is_some());
}

#[test]
fn shell_create_timeout_is_typed_and_a_later_open_retries() {
    let model = StockModel::new();
    model.fault(verb("create"), Fault::TimedOut { applied: false });
    let adapter = model.adapter();
    let shell = ShellFixture::new();
    assert!(matches!(
        adapter.ensure_shell_vm(&shell.spec),
        Err(SbxError::PreparationTimeout {
            operation: "create shell VM",
            ..
        })
    ));
    assert!(adapter.ensure_shell_vm(&shell.spec).unwrap().cold_started);
    assert_eq!(model.count("create"), 2);
}

/// Stock resolves a locally loaded template only by tag: creation selects the
/// import tag and the created VM must report the pinned digest.
#[test]
fn local_shell_template_is_created_by_tag_and_a_digest_mismatch_is_removed() {
    let model = StockModel::new();
    let tag = format!("sha256-{}", "c".repeat(64));
    let reference = format!("docker.io/library/marsh-shell-local:{tag}");
    model.add_template(&reference, &format!("sha256:{}", "b".repeat(64)));
    let adapter = model.adapter();
    let mut shell = ShellFixture::new();
    shell.spec.image = OciImage::parse(format!("{reference}@sha256:{}", "a".repeat(64))).unwrap();

    let error = adapter.ensure_shell_vm(&shell.spec).unwrap_err();
    assert!(error.to_string().contains("did not resolve"), "{error}");
    let create = &model.calls_of("create")[0];
    assert_eq!(create.value("--template"), Some(reference.as_str()));
    assert_eq!(create.value("--pull"), Some("never"));
    assert!(model.vm(SHELL).is_none());
}

#[test]
fn shell_setup_script_is_valid_shell_and_adds_the_proxy_ca_without_blocking_startup() {
    let syntax = std::process::Command::new("sh")
        .args(["-n", "-c", SHELL_SETUP_SCRIPT])
        .status()
        .unwrap();
    assert!(syntax.success(), "the shell setup script must parse");
    assert!(
        SHELL_SETUP_SCRIPT.contains("/usr/local/share/ca-certificates/docker-sandboxes-proxy.crt")
    );
    assert!(SHELL_SETUP_SCRIPT.contains("command -v update-ca-certificates"));
    // A failure to add the CA is reported and never fails the setup (it runs under `set -eu`).
    assert!(SHELL_SETUP_SCRIPT.contains("could not add the sandbox proxy CA' >&2; fi;"));
}
