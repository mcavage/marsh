//! Opt-in real stock-cloud caller of the production adapter, not a mock gate.
//! Usage: `cloud_lifecycle SBX CONTROL_DIRECTORY IMAGE@sha256:... [--cleanup|--storage-failure]`
use marsh_contracts::OciImage;
use marsh_runtime::{CommandRunner, SystemCommandRunner};
use marsh_sbx::cloud::{CloudError, CloudImageSpec, CloudSandboxId, CloudSbx};
use std::{
    error::Error,
    fs,
    io::{Read, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

fn open(sbx: &Path, directory: &Path) -> Result<CloudSbx, CloudError> {
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemCommandRunner::new(
        std::env::var_os("HOME").ok_or(CloudError::Invalid("HOME is required"))?,
    ));
    CloudSbx::open(sbx.to_owned(), runner, directory.to_owned())
}

fn cleanup(adapter: &CloudSbx) -> Result<(), Box<dyn Error>> {
    let mut failures = Vec::new();
    for record in adapter.reconcile()? {
        if let Some(id) = record.id {
            if let Err(error) = adapter.retire(&id) {
                failures.push(error.to_string());
            }
        } else {
            failures.push(format!(
                "unresolved create intent retained: {}",
                record.name
            ));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; ").into())
    }
}

fn echo(adapter: &CloudSbx, id: &CloudSandboxId) -> Result<(), Box<dyn Error>> {
    let mut attachment = adapter.spawn(
        id,
        "/usr/bin/timeout",
        &[
            "10".into(),
            "/bin/sh".into(),
            "-c".into(),
            "printf 'stderr-only\\n' >&2; cat; exit 17".into(),
        ],
    )?;
    let (send, receive) = mpsc::channel();
    let readers = [attachment.stdout, attachment.stderr]
        .into_iter()
        .enumerate()
        .map(|(n, input)| {
            let send = send.clone();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let result = input.take(65536).read_to_end(&mut bytes).map(|_| bytes);
                let _ = send.send((n, result));
            })
        })
        .collect::<Vec<_>>();
    let payload = (0..=255).cycle().take(32768).collect::<Vec<u8>>();
    attachment.stdin.write_all(&payload)?;
    drop(attachment.stdin);
    let mut output = [None, None];
    let mut code = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    while code.is_none() || output.iter().any(Option::is_none) {
        if let Ok((n, bytes)) = receive.recv_timeout(Duration::from_millis(10)) {
            output[n] = Some(bytes?);
        }
        if code.is_none() {
            code = attachment.process.try_wait()?;
        }
        if Instant::now() >= deadline {
            let _ = attachment.process.cancel_io();
            let _ = attachment.process.terminate();
            let _ = adapter.quarantine(id);
            return Err("cloud exec did not settle; sandbox quarantined".into());
        }
    }
    for reader in readers {
        reader.join().map_err(|_| "cloud reader panicked")?;
    }
    assert_eq!(code, Some(17));
    assert_eq!(output[0].as_deref(), Some(payload.as_slice()));
    assert_eq!(output[1].as_deref(), Some(b"stderr-only\n".as_slice()));
    Ok(())
}

fn lose_client(adapter: &CloudSbx, id: &CloudSandboxId) -> Result<(), Box<dyn Error>> {
    let mut attachment = adapter.spawn(
        id,
        "/usr/bin/timeout",
        &[
            "30".into(),
            "/bin/sh".into(),
            "-c".into(),
            "printf ready; sleep 30".into(),
        ],
    )?;
    let (send, receive) = mpsc::channel();
    let mut stdout = attachment.stdout;
    let reader = thread::spawn(move || {
        let mut ready = [0_u8; 5];
        let result = stdout.read_exact(&mut ready).map(|()| ready);
        let _ = send.send(result);
    });
    let ready = receive.recv_timeout(Duration::from_secs(20));
    if ready.is_err() {
        let _ = attachment.process.cancel_io();
        let _ = attachment.process.terminate();
        adapter.quarantine(id)?;
        return Err("cloud control process never produced its readiness marker".into());
    }
    let ready = ready??;
    reader
        .join()
        .map_err(|_| "cloud readiness reader panicked")?;
    assert_eq!(&ready, b"ready");
    // Local transport teardown is deliberately NOT a remote completion receipt.
    let _ = attachment.process.cancel_io();
    let terminated = attachment.process.terminate();
    adapter.quarantine(id)?;
    terminated?;
    let _ = attachment.process.wait()?;
    Ok(())
}

struct RestoreMap {
    path: PathBuf,
    backup: PathBuf,
    active: bool,
}

impl RestoreMap {
    fn replace_with_directory(path: PathBuf) -> Result<Self, Box<dyn Error>> {
        let backup = path.with_extension("failure-backup");
        if backup.exists() {
            return Err("storage failure backup already exists".into());
        }
        fs::rename(&path, &backup)?;
        let guard = Self {
            path,
            backup,
            active: true,
        };
        fs::DirBuilder::new().mode(0o700).create(&guard.path)?;
        Ok(guard)
    }

    fn restore(&mut self) -> Result<(), Box<dyn Error>> {
        match fs::symlink_metadata(&self.path) {
            Ok(md) if md.is_dir() => fs::remove_dir(&self.path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => {
                return Err(
                    "unexpected ownership path during storage restoration; backup retained".into(),
                );
            }
        }
        fs::rename(&self.backup, &self.path)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RestoreMap {
    fn drop(&mut self) {
        if self.active {
            let _ = self.restore();
        }
    }
}

fn storage_failure(sbx: &Path, directory: &Path, image: OciImage) -> Result<(), Box<dyn Error>> {
    let adapter = open(sbx, directory)?;
    if !adapter.records()?.is_empty() {
        return Err("use a fresh qualification directory".into());
    }
    let mut created = None;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<(), Box<dyn Error>> {
            let record = adapter.create(&CloudImageSpec {
                image,
                kit: None,
                cpus: 2,
                memory_mib: 4096,
                ttl: Duration::from_mins(30),
            })?;
            let id = record.id.ok_or("created sandbox has no ID")?;
            created = Some(id.clone());
            // A real filesystem write failure, not a mocked persistence result.
            let mut restore =
                RestoreMap::replace_with_directory(directory.join("cloud-ownership.json"))?;
            assert!(adapter.quarantine(&id).is_err());
            assert!(
                adapter
                    .spawn(&id, "touch", &["/tmp/must-not-run".into()])
                    .is_err()
            );
            assert!(
                adapter.inventory().is_err(),
                "poison must block every control submission"
            );
            restore.restore()?;
            assert!(
                adapter.spawn(&id, "true", &[]).is_err(),
                "restoring the file does not unpoison the live owner"
            );
            Ok(())
        },
    ));
    drop(adapter);
    let recovered = open(sbx, directory)?;
    let blocked = created.as_ref().is_some_and(|id| {
        matches!(
            recovered.spawn(id, "true", &[]),
            Err(CloudError::NotReady(_))
        )
    });
    let retired = cleanup(&recovered);
    outcome.map_err(|_| "storage failure qualification assertion failed")??;
    retired?;
    assert!(blocked);
    println!(
        "{}",
        serde_json::json!({"schema":"marsh.cloud-storage-failure-qualification/v1", "id":created.map(|id|id.as_str().to_owned()),
        "failed_write_poisoned":true,"submissions_blocked":true,"restart_quarantined":true,"absence_verified":true})
    );
    Ok(())
}

#[allow(clippy::too_many_lines)] // Keep the real-caller journey and unconditional cleanup together.
fn run() -> Result<(), Box<dyn Error>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if !(3..=4).contains(&args.len()) {
        return Err("usage: cloud_lifecycle SBX CONTROL_DIRECTORY IMAGE [--cleanup]".into());
    }
    let sbx = PathBuf::from(&args[0]).canonicalize()?;
    let base = PathBuf::from(&args[1]).canonicalize()?;
    let image = OciImage::parse(args[2].to_str().ok_or("image is not UTF-8")?.to_owned())?;
    if args
        .get(3)
        .is_some_and(|x| x != "--cleanup" && x != "--storage-failure")
    {
        return Err("unknown cloud lifecycle qualification mode".into());
    }
    let primary = base.join("primary");
    let foreign = base.join("other-client");
    for directory in [&primary, &foreign] {
        if !directory.exists() {
            fs::DirBuilder::new().mode(0o700).create(directory)?;
        }
    }
    if args.get(3).is_some_and(|x| x == "--storage-failure") {
        return storage_failure(&sbx, &primary, image);
    }
    let mut a = Some(open(&sbx, &primary)?);
    let b = open(&sbx, &foreign)?;
    if args.get(3).is_some_and(|x| x == "--cleanup") {
        cleanup(a.as_ref().unwrap())?;
        cleanup(&b)?;
        return Ok(());
    }
    if !a.as_ref().unwrap().records()?.is_empty() || !b.records()?.is_empty() {
        return Err("ownership is not empty; use --cleanup, never replay a qualification".into());
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
        || -> Result<(), Box<dyn Error>> {
            let spec = CloudImageSpec {
                image,
                kit: None,
                cpus: 2,
                memory_mib: 4096,
                ttl: Duration::from_mins(30),
            };
            let own = a.as_ref().unwrap().create(&spec)?;
            let other = b.create(&spec)?;
            let id = own.id.as_ref().ok_or("created sandbox has no ID")?;
            let other_id = other.id.as_ref().ok_or("other sandbox has no ID")?;
            assert!(matches!(
                a.as_ref().unwrap().spawn(other_id, "true", &[]),
                Err(CloudError::NotOwned(_))
            ));
            assert!(matches!(
                a.as_ref().unwrap().retire(other_id),
                Err(CloudError::NotOwned(_))
            ));
            assert!(
                open(&sbx, &primary).is_err(),
                "second owner must not share the map lock"
            );
            echo(a.as_ref().unwrap(), id)?;
            echo(&b, other_id)?;
            let mut file = tempfile::NamedTempFile::new_in(&base)?;
            file.write_all(b"file\0\xffroundtrip")?;
            file.flush()?;
            a.as_ref().unwrap().upload_file(
                id,
                &file.path().canonicalize()?,
                "/tmp/marsh-roundtrip",
            )?;
            assert_eq!(
                a.as_ref()
                    .unwrap()
                    .read_file(id, "/tmp/marsh-roundtrip", 1024)?,
                b"file\0\xffroundtrip"
            );
            assert!(
                a.as_ref()
                    .unwrap()
                    .read_file(id, "/tmp/marsh-roundtrip", 2)
                    .is_err()
            );
            lose_client(&b, other_id)?;
            assert!(matches!(
                b.spawn(other_id, "true", &[]),
                Err(CloudError::NotReady(_))
            ));
            // A is still Ready on disk: reopening must quarantine it,
            // independently of the explicit quarantine exercised on B.
            drop(a.take());
            a = Some(open(&sbx, &primary)?);
            assert!(matches!(
                a.as_ref().unwrap().spawn(id, "true", &[]),
                Err(CloudError::NotReady(_))
            ));
            a.as_ref().unwrap().retire(id)?;
            b.retire(other_id)?;
            assert!(a.as_ref().unwrap().records()?.is_empty());
            assert!(b.records()?.is_empty());
            println!(
                "{}",
                serde_json::json!({"schema":"marsh.cloud-lifecycle-qualification/v1", "own":id.as_str(), "other":other_id.as_str(),
                        "streams":true,"file_roundtrip":true,"bounded_read":true,"foreign_refused":true,"client_loss_quarantined":true,"restart_cleanup":true,"absence_verified":true})
            );
            Ok(())
        },
    ));
    let result = result.unwrap_or_else(|_| {
        Err("cloud qualification assertion failed; cleanup attempted, maps retained".into())
    });
    // Persistent maps remain on every failure. Best-effort cleanup cannot turn
    // the failed journey green or hide unresolved resources.
    let a_cleanup = if let Some(adapter) = &a {
        cleanup(adapter)
    } else {
        open(&sbx, &primary)
            .map_err(Into::into)
            .and_then(|adapter| cleanup(&adapter))
    };
    let b_cleanup = cleanup(&b);
    result?;
    a_cleanup?;
    b_cleanup?;
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("cloud lifecycle qualification: {error}");
        std::process::exit(1);
    }
}
