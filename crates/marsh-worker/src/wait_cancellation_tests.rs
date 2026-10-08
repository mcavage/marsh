//! Exercises the production Docker wait adapter against an owned CLI carrier.
//! This is not Docker integration or fabricated successful runtime evidence.
use marsh_runtime::{Cancellation, DockerCliRuntime, JobRuntime, SystemCommandRunner};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    thread,
    time::{Duration, Instant},
};

#[test]
fn regular_files_cannot_claim_interruptible_transport_io() {
    let path = std::env::temp_dir().join(format!("marsh-opaque-fd-{}", std::process::id()));
    let file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    let result = marsh_runtime::CancellableFile::from_file(file, &Cancellation::default());
    fs::remove_file(path).unwrap();
    assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::Unsupported));
}

#[test]
fn production_runtime_wait_cancellation_joins_and_reaps_its_cli_carrier() {
    let root = std::env::temp_dir().join(format!(
        "marsh-wait-cancel-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let executable = root.join("owned-cli");
    let pid_file = root.join("pid");
    fs::write(&executable, format!("#!/usr/bin/python3\nimport os,sys,time\nif sys.argv[1] == 'container':\n print('0\\texited')\nelif sys.argv[1] == 'wait':\n open({:?},'w').write(str(os.getpid()))\n os.write(1,b'x'*1048576)\n os.write(2,b'e'*1048576)\n time.sleep(60)\nelse: sys.exit(97)\n", pid_file.to_str().unwrap())).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = DockerCliRuntime::new(SystemCommandRunner::new(&root), executable);
    let cancel = Cancellation::default();
    let waiting_cancel = cancel.clone();
    let task = thread::spawn(move || {
        runtime.wait_cancellable(
            &marsh_contracts::ContainerId::parse("a".repeat(64)).unwrap(),
            &waiting_cancel,
        )
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let pid: u32 = loop {
        if let Ok(pid) = fs::read_to_string(&pid_file)
            && let Ok(pid) = pid.parse()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "wait CLI must really start");
        thread::sleep(Duration::from_millis(10));
    };
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    cancel.cancel();
    assert!(task.join().unwrap().is_err());
    assert!(started.elapsed() < Duration::from_secs(2));
    #[cfg(target_os = "linux")]
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    println!(
        "production_wait pid={pid} cancelled=true wait_and_io_joined=true reaped=true elapsed={:?}",
        started.elapsed()
    );
    fs::remove_dir_all(root).unwrap();
}
