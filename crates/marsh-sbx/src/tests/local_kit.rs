//! Local Kit v3 sources: identity, host build, stock build agreement, nested
//! image load, scratch hygiene and the Kit image cache.
use super::stock_model::{HOST_BUILD_DIGEST, STALE_DIGEST, guest_word, host_build, verb};
use super::*;

const KIT: &str = "marsh-kit-local";

fn local_tag(source: &Path) -> String {
    format!(
        "marsh-local-v3:{}",
        &local_source_fingerprint(source).unwrap()[7..]
    )
}

/// `SBX_KIT_REBUILD` of each stock create, in order; every create also pins
/// stock's Kit build to the host builder marsh builds with.
fn create_rebuilds(model: &StockModel) -> Vec<bool> {
    model
        .calls_of("create")
        .iter()
        .map(|create| {
            assert_eq!(create.env_value("SBX_KIT_BUILDER"), Some("host"));
            create.env_value("SBX_KIT_REBUILD") == Some("1")
        })
        .collect()
}

#[test]
fn cold_local_kit_loads_the_host_build_under_its_fingerprint_tag() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let kit = KitFixture::local(KIT);
    let ready = adapter.ensure_kit_vm(&kit.spec).unwrap();
    adapter.prewarm_workload(&ready).unwrap();

    assert!(ready.cold_started);
    assert_eq!(ready.job_image.as_str(), HOST_BUILD_DIGEST);
    let guest = model.vm(KIT).unwrap();
    assert_eq!(guest.images[&local_tag(&kit.source())], HOST_BUILD_DIGEST);
    assert_eq!(guest.image_digest.as_deref(), Some(HOST_BUILD_DIGEST));
    assert_eq!(guest.workspaces, [kit.home()]);
    assert_eq!(guest.files[WORKER_PATH], b"worker");
    assert_eq!(create_rebuilds(&model), [false]);
    let create = &model.calls_of("create")[0];
    assert!(create.args.contains(&kit.source().display().to_string()));
    let build = model
        .calls()
        .into_iter()
        .find(super::stock_model::Call::is_host_build)
        .unwrap();
    assert_eq!(build.value("--platform"), Some("linux/arm64"));
    // Stock exports its own build with OCI media types; digests agree only so.
    assert!(
        build
            .value("--output")
            .unwrap()
            .starts_with("type=docker,oci-mediatypes=true,dest=")
    );
    assert!(kit.home_entries().is_empty(), "{:?}", kit.home_entries());
}

#[test]
fn local_create_and_host_build_run_concurrently() {
    let model = StockModel::new();
    let adapter = Arc::new(model.adapter());
    let kit = KitFixture::local(KIT);
    let creating = model.gate(verb("create"));
    let building = model.gate(host_build());
    let ensuring = {
        let (adapter, spec) = (Arc::clone(&adapter), kit.spec.clone());
        thread::spawn(move || adapter.ensure_kit_vm(&spec).map(drop))
    };
    // Both are inside their stock call at once.
    creating.wait_entered(1);
    building.wait_entered(1);
    creating.release();
    building.release();
    ensuring.join().unwrap().unwrap();
}

#[test]
fn a_source_edit_invalidates_the_ready_image_without_rebuilding_on_revalidation() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let kit = KitFixture::local(KIT);
    let ready = adapter.ensure_kit_vm(&kit.spec).unwrap();
    assert!(adapter.revalidate_cached_kit(&kit.spec, &ready).unwrap());

    fs::write(
        kit.source().join("fixture.dockerfile"),
        "FROM scratch\n# changed\n",
    )
    .unwrap();
    assert!(!adapter.revalidate_cached_kit(&kit.spec, &ready).unwrap());
    assert_eq!(
        model.host_builds(),
        1,
        "revalidation stays a fingerprint check"
    );
}

#[test]
fn prewarm_fails_when_the_nested_image_disappears() {
    let model = StockModel::new();
    let adapter = model.adapter();
    let kit = KitFixture::local(KIT);
    let ready = adapter.ensure_kit_vm(&kit.spec).unwrap();
    model.with_vm(KIT, |vm| vm.images.clear());
    assert!(matches!(
        adapter.prewarm_workload(&ready),
        Err(SbxError::CommandFailed {
            operation: "inspect local v3 kit image",
            ..
        })
    ));
    assert!(kit.home_entries().is_empty());
}

#[test]
fn failed_local_preparation_leaves_no_vm_and_no_scratch() {
    let cases: [(&str, stock_model::Matcher, Fault); 3] = [
        (
            "build local v3 kit job image",
            host_build(),
            Fault::Respond(failure("build failed")),
        ),
        (
            "create local kit VM",
            verb("create"),
            Fault::Respond(failure("create failed")),
        ),
        (
            "nested load",
            guest_word("marsh-load"),
            Fault::SpawnError("nested load transport failed".into()),
        ),
    ];
    for (operation, matcher, fault) in cases {
        let model = StockModel::new();
        model.fault(matcher, fault);
        let adapter = model.adapter();
        let kit = KitFixture::local(KIT);
        let error = adapter.ensure_kit_vm(&kit.spec).unwrap_err();
        match &error {
            SbxError::CommandFailed {
                operation: failed, ..
            } => assert_eq!(*failed, operation),
            SbxError::Io(error) => assert_eq!(error.to_string(), "nested load transport failed"),
            other => panic!("{operation}: {other}"),
        }
        assert!(model.vm_names().is_empty(), "{operation}");
        assert!(
            kit.home_entries().is_empty(),
            "{operation}: {:?}",
            kit.home_entries()
        );
    }
}

#[test]
fn local_create_timeout_is_typed_quarantined_and_released_by_reset() {
    let model = StockModel::new();
    model.fault(verb("create"), Fault::TimedOut { applied: false });
    let adapter = model.adapter();
    let kit = KitFixture::local(KIT);

    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::PreparationTimeout {
            operation: "create local kit VM",
            ..
        })
    ));
    assert!(kit.home_entries().is_empty());
    // An uncertain create quarantines the name: no implicit retry.
    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::QuarantinedVm(_))
    ));
    // Explicit operator retire (`marsh workers reset KIT`) releases it.
    assert!(!adapter.reset_kit_vm(&kit.spec).unwrap());
    assert!(adapter.ensure_kit_vm(&kit.spec).unwrap().cold_started);
    assert_eq!(model.count("create"), 2);
    assert!(kit.home_entries().is_empty());
}

#[test]
fn host_and_stock_builds_that_never_agree_fail_after_one_stock_rebuild() {
    let model = StockModel::new();
    model.set_stock_kit_digests(STALE_DIGEST, STALE_DIGEST);
    let adapter = model.adapter();
    let kit = KitFixture::local(KIT);

    assert!(matches!(
        adapter.ensure_kit_vm(&kit.spec),
        Err(SbxError::LocalKitDigestMismatch(detail)) if detail.contains("after stock SBX rebuilt")
    ));
    assert_eq!(create_rebuilds(&model), [false, true]);
    assert!(model.vm_names().is_empty());
    assert_eq!(model.retained_spawns(WORKER_PATH), 0);
    assert!(kit.home_entries().is_empty());
}

#[test]
fn a_stale_stock_build_is_rebuilt_and_the_vm_recreated() {
    let model = StockModel::new();
    model.set_stock_kit_digests(STALE_DIGEST, HOST_BUILD_DIGEST);
    let adapter = model.adapter();
    let kit = KitFixture::local(KIT);

    let ready = adapter.ensure_kit_vm(&kit.spec).unwrap();
    assert!(ready.cold_started);
    assert_eq!(ready.job_image.as_str(), HOST_BUILD_DIGEST);
    assert_eq!(create_rebuilds(&model), [false, true]);
    let guest = model.vm(KIT).unwrap();
    assert_eq!(guest.image_digest.as_deref(), Some(HOST_BUILD_DIGEST));
    assert_eq!(adapter.ownership.uuid(KIT), Some(guest.id));
    assert!(kit.home_entries().is_empty());
}

#[test]
fn a_stale_kit_image_cache_entry_is_rebuilt_on_both_sides() {
    let model = StockModel::new();
    let kit = KitFixture::local(KIT);
    let cache = kit.dir.path().join("kit-images");
    assert!(private_cache_directory(&cache));
    let fingerprint = local_source_fingerprint(&kit.source()).unwrap();
    let archive = kit.dir.path().join("stale.tar");
    fs::write(&archive, b"stale image archive").unwrap();
    retain_local_build(
        &cache,
        &LocalBuild {
            source_fingerprint: fingerprint.clone(),
            local_tag: String::new(),
            manifest_digest: OciImage::parse(STALE_DIGEST.to_owned()).unwrap(),
            docker_archive: archive,
            metadata: kit.dir.path().join("absent.json"),
            retained: false,
            _private_scratch: None,
        },
    )
    .unwrap();
    let adapter = model.adapter().with_kit_image_cache(cache.clone());

    let ready = adapter.ensure_kit_vm(&kit.spec).unwrap();
    assert_eq!(ready.job_image.as_str(), HOST_BUILD_DIGEST);
    assert_eq!(create_rebuilds(&model), [false, true]);
    assert_eq!(model.host_builds(), 1);
    // The fresh, verified build replaced the stale entry.
    let hit = cached_local_build(&cache, &fingerprint).unwrap();
    assert_eq!(hit.manifest_digest.as_str(), HOST_BUILD_DIGEST);
}

#[test]
fn concurrent_existence_checks_never_build() {
    let model = StockModel::new();
    let adapter = Arc::new(model.adapter());
    let kit = KitFixture::local(KIT);
    let checks = [0, 1].map(|_| {
        let (adapter, spec) = (Arc::clone(&adapter), kit.spec.clone());
        thread::spawn(move || adapter.kit_vm_exists(&spec).unwrap())
    });
    assert!(checks.into_iter().all(|check| !check.join().unwrap()));
    assert_eq!(model.host_builds(), 0);
}

#[test]
fn local_source_requires_a_canonical_directory_and_hashes_symlink_text_not_targets() {
    let kit = KitFixture::local(KIT);
    let source = kit.source();
    assert!(matches!(
        NativeKitRef::local_v3_source(PathBuf::from("kits/fixture")),
        Err(SbxError::Metadata { .. } | SbxError::InvalidLocalKitSource(_))
    ));
    let local = NativeKitRef::local_v3_source(source.clone()).unwrap();
    assert_eq!(local.source_dir(), Some(source.as_path()));
    assert!(local.workload_image().is_none());

    let outside = directory(kit.dir.path(), "outside");
    fs::write(outside.join("first"), "one").unwrap();
    fs::write(outside.join("second"), "two").unwrap();
    let link = source.join("ordinary-link");
    std::os::unix::fs::symlink(outside.join("first"), &link).unwrap();
    let first = local_source_fingerprint(&source).unwrap();
    fs::write(outside.join("first"), "changed outside source").unwrap();
    assert_eq!(local_source_fingerprint(&source).unwrap(), first);
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(outside.join("second"), &link).unwrap();
    assert_ne!(local_source_fingerprint(&source).unwrap(), first);
}

#[test]
fn local_source_rejects_special_files() {
    let kit = KitFixture::local(KIT);
    let socket = kit.source().join("local.socket");
    let listener = match std::os::unix::net::UnixListener::bind(&socket) {
        Ok(listener) => listener,
        // Some hermetic macOS test sandboxes deny creation of Unix sockets.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return,
        Err(error) => panic!("create test Unix socket: {error}"),
    };
    assert!(matches!(
        local_source_fingerprint(&kit.source()),
        Err(SbxError::InvalidLocalKitSource(path)) if path == socket
    ));
    drop(listener);
}

#[test]
fn captured_generation_reuses_its_fingerprint_but_validates_before_source_use() {
    let kit = KitFixture::local(KIT);
    let source = kit.source();
    let captured = NativeKitRef::local_v3_source(source.clone())
        .unwrap()
        .capture_generation()
        .unwrap();
    let original = captured.expected_local_fingerprint(&source).unwrap();
    fs::write(
        source.join("fixture.dockerfile"),
        "FROM scratch\n# changed\n",
    )
    .unwrap();
    assert_eq!(
        captured.expected_local_fingerprint(&source).unwrap(),
        original
    );
    assert!(matches!(
        captured.validate_captured_source(&source),
        Err(SbxError::SourceChanged(path)) if path == source
    ));
}

#[test]
fn descriptor_detection_ignores_unrelated_yaml_and_directories_and_rejects_legacy_specs() {
    let kit = KitFixture::local(KIT);
    let source = kit.source();
    fs::write(
        source.join("compose.yaml"),
        "services:\n  app:\n    image: example.invalid/app:latest\n",
    )
    .unwrap();
    fs::create_dir(source.join("ignored.yaml")).unwrap();
    assert_eq!(
        local_v3_descriptor(&source).unwrap(),
        source.join("fixture.yaml")
    );
    fs::write(source.join("spec.yaml"), "schemaVersion: \"2\"\n").unwrap();
    assert!(matches!(
        local_v3_descriptor(&source),
        Err(SbxError::InvalidLocalKitSource(path)) if path == source
    ));
}

#[test]
fn descriptor_detection_propagates_candidate_read_errors() {
    let kit = KitFixture::local(KIT);
    let broken = kit.source().join("broken.yaml");
    fs::write(&broken, "schemaVersion: \"3\"\n").unwrap();
    fs::set_permissions(&broken, fs::Permissions::from_mode(0o000)).unwrap();
    assert!(matches!(
        local_v3_descriptor(&kit.source()),
        Err(SbxError::Metadata { path, .. }) if path == broken
    ));
}

#[test]
fn stale_scratch_sweep_is_narrow_and_never_follows_symlinks() {
    let root = scratch();
    let home = directory(root.path(), "home");
    let outside = directory(root.path(), "outside");
    let victim = outside.join("victim");
    fs::write(&victim, "keep").unwrap();
    let stale_archive = home.join(".marsh-kit-123-4.docker.tar");
    let stale_metadata = home.join(".marsh-kit-123-4.metadata.json");
    let fresh_archive = home.join(".marsh-kit-123-5.docker.tar");
    let current_archive = home.join(format!(".marsh-kit-{}-99.docker.tar", std::process::id()));
    let unrelated = home.join("unrelated.docker.tar");
    let malformed = home.join(".marsh-kit-owner-6.docker.tar");
    let disguised_link = home.join(".marsh-kit-123-7.docker.tar");
    for path in [
        &stale_archive,
        &stale_metadata,
        &fresh_archive,
        &current_archive,
        &unrelated,
        &malformed,
    ] {
        fs::write(path, "scratch").unwrap();
    }
    std::os::unix::fs::symlink(&victim, &disguised_link).unwrap();
    let now = SystemTime::now();
    let old = now - LOCAL_SCRATCH_STALE_AFTER - Duration::from_secs(1);
    for path in [
        &stale_archive,
        &stale_metadata,
        &current_archive,
        &unrelated,
        &malformed,
    ] {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
    }

    sweep_stale_local_scratch(&home, now).unwrap();
    assert!(!stale_archive.exists());
    assert!(!stale_metadata.exists());
    assert!(fresh_archive.exists());
    assert!(current_archive.exists());
    assert!(unrelated.exists());
    assert!(malformed.exists());
    assert!(
        fs::symlink_metadata(&disguised_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
}

// ---- nested archive transport ----------------------------------------------

/// Both output pipes must be drained while stdin is still being written.
#[derive(Default)]
struct Drained {
    state: Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

struct DrainedReader {
    drained: Arc<Drained>,
    stdout: bool,
    bytes: Cursor<Vec<u8>>,
}

impl io::Read for DrainedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut state = self.drained.state.lock().unwrap();
        if self.stdout {
            state.0 = true;
        } else {
            state.1 = true;
        }
        self.drained.changed.notify_all();
        drop(state);
        self.bytes.read(buffer)
    }
}

struct PressureWriter {
    drained: Arc<Drained>,
    fail: bool,
}

impl io::Write for PressureWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.fail {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "transfer failed"));
        }
        let state = self.drained.state.lock().unwrap();
        let (_state, timeout) = self
            .drained
            .changed
            .wait_timeout_while(state, WAIT_LIMIT, |(stdout, stderr)| !(*stdout && *stderr))
            .unwrap();
        if timeout.timed_out() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "output pipes were not drained while stdin was written",
            ));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Tracking {
    terminated: AtomicBool,
    reaped: AtomicBool,
}

struct TrackingProcess {
    tracking: Arc<Tracking>,
    complete_immediately: bool,
}

impl AttachedProcess for TrackingProcess {
    fn wait(&mut self) -> io::Result<i32> {
        self.tracking.reaped.store(true, Ordering::SeqCst);
        Ok(0)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if self.complete_immediately || self.tracking.terminated.load(Ordering::SeqCst) {
            self.tracking.reaped.store(true, Ordering::SeqCst);
            Ok(Some(0))
        } else {
            Ok(None)
        }
    }

    fn terminate(&mut self) -> io::Result<()> {
        self.tracking.terminated.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Blocks reads and writes until the process is terminated.
struct UntilTerminated(Arc<Tracking>);

impl UntilTerminated {
    fn block(&self) {
        while !self.0.terminated.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl io::Read for UntilTerminated {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        self.block();
        Ok(0)
    }
}

impl io::Write for UntilTerminated {
    fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
        self.block();
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "terminated"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An attached `docker image load` carrier with scripted pipe behaviour.
struct LoadCarrier {
    tracking: Arc<Tracking>,
    mode: LoadMode,
}

#[derive(Clone, Copy)]
enum LoadMode {
    Pressure { fail_write: bool },
    Blocked,
}

impl CommandRunner for LoadCarrier {
    fn run(&self, _invocation: &Invocation) -> io::Result<CommandOutput> {
        Err(io::Error::other("unexpected synchronous command"))
    }

    fn run_bounded(&self, invocation: &Invocation, _: Duration) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, _invocation: &Invocation) -> io::Result<Attachment> {
        let tracking = Arc::clone(&self.tracking);
        Ok(match self.mode {
            LoadMode::Pressure { fail_write } => {
                let drained = Arc::new(Drained::default());
                let reader = |stdout| DrainedReader {
                    drained: Arc::clone(&drained),
                    stdout,
                    bytes: Cursor::new(vec![b'o'; 256 * 1024]),
                };
                Attachment {
                    stdin: Box::new(PressureWriter {
                        drained: Arc::clone(&drained),
                        fail: fail_write,
                    }),
                    stdout: Box::new(reader(true)),
                    stderr: Box::new(reader(false)),
                    process: Box::new(TrackingProcess {
                        tracking,
                        complete_immediately: true,
                    }),
                    control: no_attachment_control(),
                }
            }
            LoadMode::Blocked => Attachment {
                stdin: Box::new(UntilTerminated(Arc::clone(&tracking))),
                stdout: Box::new(UntilTerminated(Arc::clone(&tracking))),
                stderr: Box::new(UntilTerminated(Arc::clone(&tracking))),
                process: Box::new(TrackingProcess {
                    tracking,
                    complete_immediately: false,
                }),
                control: no_attachment_control(),
            },
        })
    }
}

fn load_archive(mode: LoadMode) -> (Arc<Tracking>, Result<(), SbxError>) {
    let tracking = Arc::new(Tracking::default());
    let runner = Arc::new(LoadCarrier {
        tracking: Arc::clone(&tracking),
        mode,
    });
    let timeout = Duration::from_millis(40);
    let adapter = StockSbx::new("sbx", runner).with_preparation_timeouts(PreparationTimeouts {
        load: timeout,
        ..PreparationTimeouts::default()
    });
    let root = scratch();
    let archive = root.path().join("image.tar");
    fs::write(&archive, vec![b'i'; 1024 * 1024]).unwrap();
    // A load that never ends would hang here; `bounded` fails it instead.
    let result = bounded(move || {
        let result = adapter.load_nested_archive(KIT, &archive);
        drop(root);
        result
    });
    (tracking, result)
}

#[test]
fn nested_archive_load_drains_both_output_pipes_while_writing() {
    let (tracking, result) = load_archive(LoadMode::Pressure { fail_write: false });
    result.unwrap();
    assert!(!tracking.terminated.load(Ordering::SeqCst));
    assert!(tracking.reaped.load(Ordering::SeqCst));
}

#[test]
fn nested_archive_transfer_failure_terminates_and_reaps_the_carrier() {
    let (tracking, result) = load_archive(LoadMode::Pressure { fail_write: true });
    assert!(matches!(
        result,
        Err(SbxError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe
    ));
    assert!(tracking.terminated.load(Ordering::SeqCst));
    assert!(tracking.reaped.load(Ordering::SeqCst));
}

#[test]
fn nested_archive_deadline_terminates_reaps_and_joins_all_streams() {
    let (tracking, result) = load_archive(LoadMode::Blocked);
    assert!(matches!(
        result,
        Err(SbxError::PreparationTimeout {
            operation: "load local v3 kit image into worker VM",
            ..
        })
    ));
    assert!(tracking.terminated.load(Ordering::SeqCst));
    assert!(tracking.reaped.load(Ordering::SeqCst));
}

// ---- Kit image cache ---------------------------------------------------------

struct Cache {
    dir: tempfile::TempDir,
    path: PathBuf,
}

impl Cache {
    fn new() -> Self {
        let dir = scratch();
        let path = dir.path().join("kit-images");
        assert!(private_cache_directory(&path));
        Self { dir, path }
    }

    fn build(&self, fingerprint: &str, manifest: &str) -> LocalBuild {
        let archive = self.dir.path().join(format!("{}.tar", &fingerprint[7..15]));
        fs::write(&archive, manifest).unwrap();
        LocalBuild {
            source_fingerprint: fingerprint.to_owned(),
            local_tag: String::new(),
            manifest_digest: OciImage::parse(manifest.to_owned()).unwrap(),
            docker_archive: archive,
            metadata: self.dir.path().join("absent.json"),
            retained: false,
            _private_scratch: None,
        }
    }
}

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

#[test]
fn kit_image_cache_returns_the_retained_archive_for_its_fingerprint_only() {
    let cache = Cache::new();
    let fingerprint = digest('a');
    assert!(cached_local_build(&cache.path, &fingerprint).is_none());
    retain_local_build(&cache.path, &cache.build(&fingerprint, &digest('1'))).unwrap();
    let hit = cached_local_build(&cache.path, &fingerprint).unwrap();
    assert!(hit.retained);
    assert_eq!(hit.manifest_digest.as_str(), digest('1'));
    assert_eq!(
        fs::read_to_string(&hit.docker_archive).unwrap(),
        digest('1')
    );
    assert!(cached_local_build(&cache.path, &digest('b')).is_none());
}

#[test]
fn kit_image_cache_replaces_a_record_and_archive_as_a_unit() {
    let cache = Cache::new();
    let fingerprint = digest('a');
    retain_local_build(&cache.path, &cache.build(&fingerprint, &digest('1'))).unwrap();
    retain_local_build(&cache.path, &cache.build(&fingerprint, &digest('2'))).unwrap();
    let hit = cached_local_build(&cache.path, &fingerprint).unwrap();
    assert_eq!(hit.manifest_digest.as_str(), digest('2'));
    assert_eq!(
        fs::read_to_string(&hit.docker_archive).unwrap(),
        digest('2')
    );
}

#[test]
fn a_group_accessible_kit_image_cache_is_untrusted_until_made_private() {
    let cache = Cache::new();
    let fingerprint = digest('a');
    retain_local_build(&cache.path, &cache.build(&fingerprint, &digest('1'))).unwrap();
    fs::set_permissions(&cache.path, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(cached_local_build(&cache.path, &fingerprint).is_none());
    assert!(private_cache_directory(&cache.path));
    assert!(cached_local_build(&cache.path, &fingerprint).is_some());
}

#[test]
fn kit_image_cache_eviction_drops_the_entry() {
    let cache = Cache::new();
    let fingerprint = digest('a');
    retain_local_build(&cache.path, &cache.build(&fingerprint, &digest('1'))).unwrap();
    let hit = cached_local_build(&cache.path, &fingerprint).unwrap();
    evict_kit_image_cache(&cache.path, &hit);
    assert!(cached_local_build(&cache.path, &fingerprint).is_none());
}

#[test]
fn kit_image_cache_pruning_keeps_the_newest_records_with_their_archives() {
    let cache = Cache::new();
    for index in 0..KIT_IMAGE_CACHE_ENTRIES + 2 {
        let fingerprint = format!("sha256:{index:064x}");
        retain_local_build(&cache.path, &cache.build(&fingerprint, &digest('1'))).unwrap();
    }
    prune_kit_image_cache(&cache.path, SystemTime::now() + Duration::from_hours(2));
    let names = fs::read_dir(&cache.path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let records = names
        .iter()
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "json")
        })
        .count();
    assert_eq!(records, KIT_IMAGE_CACHE_ENTRIES);
    assert_eq!(names.len() - records, KIT_IMAGE_CACHE_ENTRIES);
}

#[test]
fn published_kit_images_share_the_cache_by_full_reference() {
    let cache = Cache::new();
    let pinned = digest('c');
    let reference = OciImage::parse(format!("docker.io/example/kit@{pinned}")).unwrap();
    assert_eq!(
        published_cache_tag(&reference).unwrap(),
        format!("docker.io/example/kit:marsh-{}", "c".repeat(64))
    );
    let tagged = OciImage::parse(format!("localhost:5000/kit:v1@{pinned}")).unwrap();
    assert_eq!(
        published_cache_tag(&tagged).unwrap(),
        format!("localhost:5000/kit:marsh-{}", "c".repeat(64))
    );
    assert!(published_image_digest(&OciImage::parse(pinned.clone()).unwrap()).is_none());
    assert!(cached_published_image(&cache.path, &reference).is_none());

    // A real child process stands in for the in-VM `docker image save`.
    let saved = cache.dir.path().join("saved");
    fs::write(&saved, "oci-archive").unwrap();
    store_published_image(
        &marsh_runtime::SystemCommandRunner::new(cache.dir.path()),
        &Invocation {
            program: "/bin/cat".into(),
            arguments: vec![saved.into_os_string()],
            working_directory: None,
            environment: Vec::new(),
        },
        &cache.path,
        &reference,
        WAIT_LIMIT,
    )
    .unwrap();
    let archive = cached_published_image(&cache.path, &reference).unwrap();
    assert_eq!(fs::read_to_string(&archive).unwrap(), "oci-archive");
    // Another repository with the same digest, or a local-build lookup of
    // that key, never uses the entry.
    let other = OciImage::parse(format!("docker.io/other/kit@{pinned}")).unwrap();
    assert!(cached_published_image(&cache.path, &other).is_none());
    assert!(cached_local_build(&cache.path, &pinned).is_none());
    // Pruning keeps the pair; a failed use evicts both files.
    prune_kit_image_cache(&cache.path, SystemTime::now() + Duration::from_hours(2));
    assert!(cached_published_image(&cache.path, &reference).is_some());
    evict_published_image(&cache.path, &reference);
    assert!(cached_published_image(&cache.path, &reference).is_none());
    assert_eq!(fs::read_dir(&cache.path).unwrap().count(), 0);
}
