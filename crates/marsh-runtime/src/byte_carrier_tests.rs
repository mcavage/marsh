//! Fault injection for carrier filesystem ownership and image authority. These
//! tests are not a Docker/stock byte transport oracle; the real driver is required.
use super::*;
use std::os::unix::fs::PermissionsExt;

pub(super) fn inspected(
    spec: &JobSpec,
    entrypoint: serde_json::Value,
    cmd: serde_json::Value,
) -> CommandOutput {
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "amd64"
    };
    let mut document = serde_json::json!({
        "Id": format!("sha256:{}", "d".repeat(64)), "RepoDigests": [spec.image.as_str()],
        "Os": "linux", "Architecture": arch, "Config": {}
    });
    document["Config"]["Entrypoint"] = entrypoint;
    document["Config"]["Cmd"] = cmd;
    success(serde_json::to_vec(&document).unwrap())
}

fn setup(outputs: Vec<CommandOutput>) -> (tempfile::TempDir, DockerCliRuntime<FakeRunner>) {
    let root = tempfile::tempdir().unwrap();
    let helper = root.path().join("helper");
    // Valid ELF header for admission tests only; no process executes this file.
    let mut elf = vec![0_u8; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[18..20].copy_from_slice(
        &(if cfg!(target_arch = "aarch64") {
            183_u16
        } else {
            62_u16
        })
        .to_le_bytes(),
    );
    elf[32..40].copy_from_slice(&64_u64.to_le_bytes());
    elf[54..56].copy_from_slice(&56_u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1_u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1_u32.to_le_bytes());
    fs::write(&helper, elf).unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o555)).unwrap();
    let mut runtime = DockerCliRuntime::new(FakeRunner::with_outputs(outputs), "/trusted/docker");
    runtime.byte_carriers = byte_carrier::CarrierStore::new(root.path().join("carriers"), helper);
    (root, runtime)
}

/// Reclaim on a fresh runtime until `done` accepts the result (bounded).
///
/// A carrier's flock rides on its open file description. A concurrent test's
/// process spawn briefly shares every descriptor of this process with the
/// child (close-on-exec ones close only at its exec), so a carrier released
/// by a dropped runtime can still look locked for that instant. Reclaim then
/// skips it as in use, which is the product contract for a live worker; only
/// this test's assumption that a release is visible at once was wrong.
fn reclaim_until(
    make: impl Fn() -> DockerCliRuntime<FakeRunner>,
    done: impl Fn(&Result<usize, RuntimeError>) -> bool,
) -> (DockerCliRuntime<FakeRunner>, Result<usize, RuntimeError>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let runtime = make();
        let result = runtime.reclaim_byte_carriers();
        if done(&result) || std::time::Instant::now() >= deadline {
            return (runtime, result);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn files(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root.join("carriers"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect()
}

#[test]
fn carrier_derives_image_command_and_retains_until_actual_deletion_observation() {
    let mut spec = job();
    spec.argv = vec![b"\xff\xfe".to_vec()];
    spec.exported_environment
        .insert("PROBE_VALUE".into(), b"secret\xff\xfe".to_vec());
    let (root, runtime) = setup(vec![
        inspected(
            &spec,
            serde_json::json!(["/image-entry", "static-word"]),
            serde_json::json!(["default"]),
        ),
        success(format!("{}\n", "b".repeat(64))),
        success(Vec::new()),
        success(format!("{}\n", "b".repeat(64))),
        success(Vec::new()),
        success(Vec::new()), // fresh complete inventory
        success(Vec::new()), // second inventory unchanged
    ]);
    let id = runtime.create(&spec).unwrap();
    let directory = files(root.path()).pop().unwrap();
    let payload = byte_exec::Payload::read(File::open(directory.join("payload")).unwrap()).unwrap();
    assert_eq!(
        payload.argv,
        [
            b"/image-entry".to_vec(),
            b"static-word".to_vec(),
            b"\xff\xfe".to_vec()
        ]
    );
    assert_eq!(payload.environment, spec.exported_environment);
    let calls = runtime.runner.calls();
    assert!(calls[1].windows(2).any(|pair| pair[0] == "--entrypoint"
        && pair[1].starts_with(byte_carrier::TARGET_PREFIX)
        && pair[1].ends_with("-exec")));
    assert!(
        calls[1]
            .iter()
            .all(|word| !word.contains("secret") && !word.contains('�'))
    );
    assert!(directory.join("helper").exists());
    assert_eq!(
        fs::metadata(directory.join("payload")).unwrap().mode() & 0o777,
        0o444
    );
    runtime.delete(&id).unwrap();
    assert!(!directory.exists());
}

#[test]
fn failed_create_invalid_identity_and_failed_delete_keep_all_sources_even_after_drop() {
    for invalid_identity in [false, true] {
        let mut spec = job();
        spec.argv = vec![b"\xff".to_vec()];
        let output = if invalid_identity {
            success(b"not-an-id".to_vec())
        } else {
            CommandOutput {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: b"opaque failure".to_vec(),
            }
        };
        let (root, runtime) = setup(vec![
            inspected(
                &spec,
                serde_json::json!(["/entry"]),
                serde_json::Value::Null,
            ),
            output,
        ]);
        assert!(matches!(
            runtime.create(&spec),
            Err(RuntimeError::ByteCreateUncertain { .. })
        ));
        let directory = files(root.path()).pop().unwrap();
        drop(runtime);
        for name in ["helper", "payload", "image"] {
            assert!(directory.join(name).exists());
        }
    }
    let mut spec = job();
    spec.argv = vec![b"\xff".to_vec()];
    let (root, runtime) = setup(vec![
        inspected(
            &spec,
            serde_json::json!(["/entry"]),
            serde_json::Value::Null,
        ),
        success("b".repeat(64)),
        CommandOutput {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    ]);
    let id = runtime.create(&spec).unwrap();
    assert!(runtime.delete(&id).is_err());
    assert!(files(root.path())[0].join("payload").exists());
}

#[test]
fn image_cmd_tail_semantics_and_identity_platform_are_independent_authority() {
    let mut spec = job();
    spec.argv.clear();
    let bytes = inspected(
        &spec,
        serde_json::json!(["/entry"]),
        serde_json::json!(["default", ""]),
    )
    .stdout;
    let image = byte_carrier::ImageCommand::inspect(&bytes, &spec).unwrap();
    assert_eq!(
        image.argv,
        [b"/entry".to_vec(), b"default".to_vec(), Vec::new()]
    );
    spec.argv = vec![b"\xff".to_vec()];
    let image = byte_carrier::ImageCommand::inspect(&bytes, &spec).unwrap();
    assert_eq!(image.argv, [b"/entry".to_vec(), b"\xff".to_vec()]);
    for (field, bad) in [
        ("Id", "mutable:tag"),
        ("Os", "windows"),
        ("Architecture", "invalid"),
    ] {
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value[field] = bad.into();
        assert!(
            byte_carrier::ImageCommand::inspect(&serde_json::to_vec(&value).unwrap(), &spec)
                .is_err()
        );
    }
    spec.image = marsh_contracts::OciImage::parse(format!("sha256:{}", "e".repeat(64))).unwrap();
    assert!(byte_carrier::ImageCommand::inspect(&bytes, &spec).is_err());
}

#[test]
fn successful_runtime_absence_does_not_authorize_unlinking_replaced_payload() {
    let mut spec = job();
    spec.argv = vec![b"\xff".to_vec()];
    let (root, runtime) = setup(vec![
        inspected(
            &spec,
            serde_json::json!(["/entry"]),
            serde_json::Value::Null,
        ),
        success("b".repeat(64)),
        success(Vec::new()),
        success(Vec::new()),
        success(Vec::new()),
        success(Vec::new()),
    ]);
    let id = runtime.create(&spec).unwrap();
    let directory = files(root.path()).pop().unwrap();
    let payload = directory.join("payload");
    fs::remove_file(&payload).unwrap();
    fs::write(&payload, b"replacement owner canary").unwrap();
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o444)).unwrap();
    assert!(runtime.delete(&id).is_err());
    assert_eq!(fs::read(&payload).unwrap(), b"replacement owner canary");
    assert!(directory.join("helper").exists());
}

struct BindFailureRunner {
    base: FakeRunner,
    root: PathBuf,
}
impl BindFailureRunner {
    fn inject(&self, invocation: &Invocation) -> io::Result<()> {
        if invocation
            .arguments
            .first()
            .is_some_and(|arg| arg == "create")
        {
            let directory = files(&self.root).pop().unwrap();
            let record = directory.join("container");
            fs::write(&record, b"injected post-create binding collision")?;
            fs::set_permissions(record, fs::Permissions::from_mode(0o400))?;
        }
        Ok(())
    }
}
impl CommandRunner for BindFailureRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        let output = self.base.run(invocation)?;
        self.inject(invocation)?;
        Ok(output)
    }
    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }
    // `create` runs through the cancellable attached path, not `run`.
    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment> {
        let attachment = self.base.spawn_attached(invocation)?;
        self.inject(invocation)?;
        Ok(attachment)
    }
}

#[test]
fn post_create_binding_failure_stays_uncertain_and_retains_sources() {
    let mut spec = job();
    spec.argv = vec![b"\xff".to_vec()];
    let (root, old) = setup(vec![
        inspected(
            &spec,
            serde_json::json!(["/entry"]),
            serde_json::Value::Null,
        ),
        success("b".repeat(64)),
    ]);
    let mut runtime = DockerCliRuntime::new(
        BindFailureRunner {
            base: old.runner,
            root: root.path().to_owned(),
        },
        "/trusted/docker",
    );
    runtime.byte_carriers = old.byte_carriers;
    let result = runtime.create(&spec);
    let Err(RuntimeError::ByteCreateUncertain { stderr, .. }) = result else {
        panic!("post-create failure must remain uncertain: {result:?}");
    };
    assert!(stderr.contains("binding failed"));
    let directory = files(root.path()).pop().unwrap();
    assert!(directory.join("payload").exists());
    assert!(directory.join("helper").exists());
}

#[test]
fn complete_reclaim_observation_includes_volume_sources_and_rejects_unknown_mount_kinds() {
    let candidate = byte_carrier::Candidate {
        name: format!("marsh-bytes-{}", "1".repeat(32)),
        directory: "/owner-private/attempt".into(),
        container: Some(ContainerId::parse("a".repeat(64)).unwrap()),
        dispatched: true,
    };
    let id = "f".repeat(64);
    for kind in ["bind", "volume"] {
        let document = serde_json::json!({"Id": id, "Name": "/foreign", "Mounts": [{"Type": kind, "Source": candidate.directory.join("payload")}]});
        let observation = byte_carrier::Observation::from_inspection(
            std::slice::from_ref(&candidate),
            std::slice::from_ref(&id),
            &serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        assert!(observation.unreferenced.is_empty());
    }
    for mount in [
        serde_json::json!({"Type":"unknown", "Source":""}),
        serde_json::json!({"Type":"volume"}),
        serde_json::json!({"Type":"tmpfs", "Source":"/unexpected-file-source"}),
    ] {
        let document = serde_json::json!({"Id":id, "Name":"/foreign", "Mounts":[mount]});
        assert!(
            byte_carrier::Observation::from_inspection(
                std::slice::from_ref(&candidate),
                std::slice::from_ref(&id),
                &serde_json::to_vec(&document).unwrap()
            )
            .is_err()
        );
    }
}

#[test]
fn both_encodings_reject_foreign_platform_without_create_or_carrier() {
    for raw in [false, true] {
        let mut spec = job();
        if raw {
            spec.argv = vec![b"\xff".to_vec()];
        }
        let mut value: serde_json::Value = serde_json::from_slice(
            &inspected(
                &spec,
                serde_json::json!(["/entry"]),
                serde_json::Value::Null,
            )
            .stdout,
        )
        .unwrap();
        value["Architecture"] = "foreign".into();
        let (root, runtime) = setup(vec![success(serde_json::to_vec(&value).unwrap())]);
        assert!(matches!(
            runtime.create(&spec),
            Err(RuntimeError::ByteBridge(_))
        ));
        assert_eq!(runtime.runner.calls().len(), 1);
        assert!(!root.path().join("carriers").exists());
    }
}

#[test]
fn unsafe_or_missing_helper_is_typed_pre_effect_and_stderr_is_preserved_after_effect() {
    let mut spec = job();
    spec.argv = vec![b"\xff".to_vec()];
    let inspection = inspected(
        &spec,
        serde_json::json!(["/entry"]),
        serde_json::Value::Null,
    );
    let (root, runtime) = setup(vec![inspection.clone()]);
    fs::remove_file(root.path().join("helper")).unwrap();
    assert!(matches!(
        runtime.create(&spec),
        Err(RuntimeError::ByteBridge(_))
    ));
    assert_eq!(runtime.runner.calls().len(), 1);
    let (_root, runtime) = setup(vec![
        inspection,
        CommandOutput {
            exit_code: Some(1),
            stdout: Vec::new(),
            stderr: b"bind source path does not exist\0".to_vec(),
        },
    ]);
    let Err(RuntimeError::ByteCreateUncertain { stderr, .. }) = runtime.create(&spec) else {
        panic!("must retain uncertain create");
    };
    assert!(stderr.starts_with("bind source path does not exist"));
    assert!(!stderr.contains('\0'));
}

#[test]
fn restart_reclaims_only_after_complete_fresh_runtime_absence() {
    let mut spec = job();
    spec.argv = vec![b"\xff".to_vec()];
    let (root, runtime) = setup(vec![
        inspected(
            &spec,
            serde_json::json!(["/entry"]),
            serde_json::Value::Null,
        ),
        success("b".repeat(64)),
    ]);
    runtime.create(&spec).unwrap();
    let directory = files(root.path()).pop().unwrap();
    let attempt = directory.file_name().unwrap().to_str().unwrap().to_owned();
    assert_eq!(
        fs::read_to_string(directory.join("attempt")).unwrap(),
        attempt
    );
    // Empty session map on a different runtime is NOT absence; the active
    // per-attempt flock excludes it before any Docker observation.
    let make = |outputs| {
        let mut runtime =
            DockerCliRuntime::new(FakeRunner::with_outputs(outputs), "/trusted/docker");
        runtime.byte_carriers = byte_carrier::CarrierStore::new(
            root.path().join("carriers"),
            root.path().join("helper"),
        );
        runtime
    };
    let other = make(vec![]);
    assert_eq!(other.reclaim_byte_carriers().unwrap(), 0);
    assert!(directory.join("payload").exists());
    drop(runtime);
    // An unrelated same-prefix container is neither removed nor an obstacle.
    let foreign = "f".repeat(64);
    let inventory = success(format!("{foreign}\n"));
    let inspect = |source: &Path| {
        success(serde_json::to_vec(&serde_json::json!({
        "Id": foreign, "Name": "/marsh-bytes-foreign-canary", "Mounts": [{"Type": "bind", "Source": source}]
    })).unwrap())
    };
    let blocked = make(vec![
        inventory.clone(),
        inspect(&directory.join("payload")),
        inventory.clone(),
    ]);
    assert_eq!(blocked.reclaim_byte_carriers().unwrap(), 0);
    assert!(directory.join("payload").exists());
    let (_, incomplete) = reclaim_until(
        || make(vec![inventory.clone(), success(Vec::new())]),
        Result::is_err,
    );
    assert!(incomplete.is_err());
    assert!(directory.join("payload").exists());
    let (recovered, reclaimed) = reclaim_until(
        || {
            make(vec![
                inventory.clone(),
                inspect(Path::new("/unrelated")),
                inventory.clone(),
            ])
        },
        |result| matches!(result, Ok(1)),
    );
    assert_eq!(reclaimed.unwrap(), 1);
    assert!(!directory.exists());
    assert!(
        recovered
            .runner
            .calls()
            .iter()
            .all(|call| !call.iter().any(|word| word == "rm"))
    );
}

#[test]
fn pending_create_absence_is_not_settlement_but_observed_identity_can_be_reclaimed_later() {
    let mut spec = job();
    spec.argv = vec![b"\xff".to_vec()];
    let (root, runtime) = setup(vec![
        inspected(
            &spec,
            serde_json::json!(["/entry"]),
            serde_json::Value::Null,
        ),
        success(b"lost-id".to_vec()),
    ]);
    assert!(matches!(
        runtime.create(&spec),
        Err(RuntimeError::ByteCreateUncertain { .. })
    ));
    let directory = files(root.path()).pop().unwrap();
    let name = directory.file_name().unwrap().to_str().unwrap().to_owned();
    drop(runtime);
    let make = |outputs| {
        let mut runtime =
            DockerCliRuntime::new(FakeRunner::with_outputs(outputs), "/trusted/docker");
        runtime.byte_carriers = byte_carrier::CarrierStore::new(
            root.path().join("carriers"),
            root.path().join("helper"),
        );
        runtime
    };
    let absent = make(vec![success(Vec::new()), success(Vec::new())]);
    assert_eq!(absent.reclaim_byte_carriers().unwrap(), 0);
    assert!(directory.join("payload").exists());
    let id = "c".repeat(64);
    let inventory = success(format!("{id}\n"));
    let actual = serde_json::json!({"Id": id, "Name": format!("/{name}"), "Mounts": [
        {"Type": "bind", "Source": directory.join("helper")},
        {"Type": "bind", "Source": directory.join("payload")}
    ]});
    let (_, observed) = reclaim_until(
        || {
            make(vec![
                inventory.clone(),
                success(serde_json::to_vec(&actual).unwrap()),
                inventory.clone(),
            ])
        },
        |_| directory.join("container").exists(),
    );
    assert_eq!(observed.unwrap(), 0);
    assert_eq!(fs::read_to_string(directory.join("container")).unwrap(), id);
    let (_, absent) = reclaim_until(
        || make(vec![success(Vec::new()), success(Vec::new())]),
        |result| matches!(result, Ok(1)),
    );
    assert_eq!(absent.unwrap(), 1);
    assert!(!directory.exists());
}

#[test]
fn special_bits_and_root_replacement_are_rejected_without_create() {
    let mut spec = job();
    spec.argv = vec![b"\xff".to_vec()];
    let inspection = inspected(
        &spec,
        serde_json::json!(["/entry"]),
        serde_json::Value::Null,
    );
    let (root, runtime) = setup(vec![inspection.clone(), inspection]);
    assert_eq!(runtime.reclaim_byte_carriers().unwrap(), 0);
    let directory = root.path().join("carriers");
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o1700)).unwrap();
    assert!(matches!(
        runtime.create(&spec),
        Err(RuntimeError::ByteBridge(_))
    ));
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(&directory, root.path().join("retained-old-root")).unwrap();
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        runtime.create(&spec),
        Err(RuntimeError::ByteBridge(_))
    ));
    assert!(runtime.runner.calls().iter().all(|call| call[0] == "image"));
}
