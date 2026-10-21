//! The per-attempt job capability (`docs/design/processes.md` s3-s4).
//!
//! Every Kit job gets `/run/marsh`, built by the worker in
//! `/run/marsh-cap/<attempt>/` and bound read-only into that container only:
//! `cap.sock`, `context.md`, `job.json` (written by the runtime, which knows
//! the starting environment), a mountpoint for the static artifact, and
//! `bin/` with one two-line stub per link. The socket has no token: each
//! connection is forwarded as attempt-tagged frames over the worker
//! transport, and the daemon admits only this job's requests. Excess
//! connections get a refusal frame. Everything dies with the attempt.

use crate::{WorkerResponse, transport::Publisher};
use marsh_contracts::{
    kit_assets::{KitAssets, KitContext, MAX_CONTEXT_BYTES},
    process::JobCapability,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::Shutdown,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

pub(crate) const ROOT: &str = "/run/marsh-cap";

/// At most this many open connections per attempt (`MaxFan + 4`).
const MAX_CHANNELS: usize = marsh_contracts::process::CONNECTION_LIMIT;
/// Daemon-to-job chunks queued per connection before it is closed.
const QUEUED_CHUNKS: usize = 64;

/// One connection's writer: a bounded queue drained by its own thread, so a
/// job that stops reading never blocks the worker transport.
pub(crate) struct Channel {
    queue: std::sync::mpsc::SyncSender<Vec<u8>>,
    stream: UnixStream,
}

pub(crate) type Streams = Arc<Mutex<BTreeMap<(String, u32), Channel>>>;

pub(crate) struct Capability {
    dir: PathBuf,
    attempt: String,
    stop: Arc<AtomicBool>,
    streams: Streams,
    accept: Option<thread::JoinHandle<()>>,
    socket: PathBuf,
}

/// The refusal an excess connection reads before its EOF: the daemon's
/// `PublicReply::Error` wire frame (the worker does not link the daemon).
fn refusal_frame() -> Vec<u8> {
    let body = serde_json::json!({
        "type": "error",
        "code": "invalid_request",
        "message": marsh_contracts::process::CONNECTION_REFUSAL,
    })
    .to_string();
    let mut frame = u32::try_from(body.len())
        .unwrap_or(0)
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(body.as_bytes());
    frame
}

/// Write the read-only parts of `/run/marsh`: links, context, mountpoint.
fn publish(dir: &Path, capability: &JobCapability) -> std::io::Result<()> {
    let bin = dir.join("bin");
    fs::create_dir(&bin)?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    // `bash` and `sh` are never links: a job's shells are the image's own.
    let mut names = vec!["marsh".to_owned()];
    names.extend(
        capability
            .registered
            .iter()
            .filter(|name| valid_link(name))
            .cloned(),
    );
    for name in names {
        let stub = bin.join(&name);
        fs::write(
            &stub,
            format!("#!{} --link={name}\n", marsh_contracts::process::ARTIFACT),
        )?;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))?;
    }
    let context = dir.join("context.md");
    let mut text = marsh_contracts::process::job_context(&capability.name, &capability.spawn);
    for body in &capability.kit.contexts {
        if let Some(body) = kit_context(body) {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(body.trim_end());
            text.push('\n');
        }
    }
    fs::write(&context, text)?;
    fs::set_permissions(&context, fs::Permissions::from_mode(0o644))?;
    publish_skills(dir, &capability.kit)?;
    // The mountpoint Docker binds the artifact over (the runtime refuses a
    // job when the artifact is missing); unbound, it is not executable.
    let artifact = dir.join("marsh");
    fs::write(&artifact, b"")?;
    fs::set_permissions(&artifact, fs::Permissions::from_mode(0o000))?;
    Ok(())
}

/// One Kit instruction body. Files are read from this container, which runs
/// the job's own image; the worker is root, so only regular, world-readable
/// files qualify (a Kit cannot use this to show a job a root-only file).
fn kit_context(body: &KitContext) -> Option<String> {
    use rustix::fs::{Mode, OFlags};
    let path = match body {
        KitContext::Text(text) => return Some(text.clone()),
        KitContext::File(path) => path,
    };
    let file = fs::File::from(
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .ok()?,
    );
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o004 == 0 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONTEXT_BYTES as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// `skills/<index>/<name>` links to each bundled skill, one directory per
/// skills destination; the runtime binds the deliverable ones read-only.
fn publish_skills(dir: &Path, kit: &KitAssets) -> std::io::Result<()> {
    if kit.skill_destinations.is_empty() || kit.skills.is_empty() {
        return Ok(());
    }
    let root = dir.join("skills");
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
    for index in 0..kit.skill_destinations.len() {
        let destination = root.join(index.to_string());
        fs::create_dir(&destination)?;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755))?;
        for skill in &kit.skills {
            std::os::unix::fs::symlink(&skill.path, destination.join(&skill.name))?;
        }
    }
    Ok(())
}

fn valid_link(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !matches!(name, "marsh" | "bash" | "sh" | "." | "..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl Capability {
    pub(crate) fn start(
        attempt: &str,
        uid: u32,
        gid: u32,
        output: Publisher,
        streams: Streams,
        capability: &JobCapability,
    ) -> std::io::Result<Self> {
        let dir = Path::new(ROOT).join(attempt);
        fs::create_dir_all(ROOT)?;
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
        publish(&dir, capability)?;
        let stop = Arc::new(AtomicBool::new(false));
        let socket = dir.join("cap.sock");
        let listener = UnixListener::bind(&socket)?;
        std::os::unix::fs::chown(&socket, Some(uid), Some(gid))?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let accept = {
            let stop = Arc::clone(&stop);
            let streams = Arc::clone(&streams);
            let attempt = attempt.to_owned();
            thread::spawn(move || {
                let mut channel = 0_u32;
                // Blocking accept; `Drop` wakes it with one local connection.
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                            let open = streams
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .keys()
                                .filter(|(owner, _)| *owner == attempt)
                                .count();
                            if open >= MAX_CHANNELS {
                                // Never a silent drop: the excess connection
                                // reads one refusal frame, then EOF.
                                let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                                let _ = stream.write_all(&refusal_frame());
                                let _ = stream.shutdown(Shutdown::Both);
                                continue;
                            }
                            channel += 1;
                            let _ = stream.set_nonblocking(false);
                            let (Ok(mut writer), Ok(handle)) =
                                (stream.try_clone(), stream.try_clone())
                            else {
                                continue;
                            };
                            let (queue, chunks) =
                                std::sync::mpsc::sync_channel::<Vec<u8>>(QUEUED_CHUNKS);
                            thread::spawn(move || {
                                for chunk in chunks {
                                    if writer.write_all(&chunk).is_err() {
                                        break;
                                    }
                                }
                                // The daemon closed: deliver EOF after the data.
                                let _ = writer.shutdown(Shutdown::Write);
                            });
                            streams
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .insert(
                                    (attempt.clone(), channel),
                                    Channel {
                                        queue,
                                        stream: handle,
                                    },
                                );
                            let _ = output.control(&WorkerResponse::CapOpen {
                                attempt: attempt.clone(),
                                channel,
                            });
                            pump(stream, attempt.clone(), channel, output.clone());
                        }
                        Err(_) => thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
        };
        Ok(Self {
            dir,
            attempt: attempt.to_owned(),
            stop,
            streams,
            accept: Some(accept),
            socket,
        })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

/// The job's bytes to the daemon. The slot stays counted until the daemon
/// side closes (`close`), never just because the job hung up.
fn pump(mut stream: UnixStream, attempt: String, channel: u32, output: Publisher) {
    thread::spawn(move || {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if output
                        .control(&WorkerResponse::CapData {
                            attempt: attempt.clone(),
                            channel,
                            bytes: buffer[..count].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = output.control(&WorkerResponse::CapClose { attempt, channel });
    });
}

/// Daemon-to-job bytes for one connection; a connection whose job stopped
/// reading (queue full) is closed rather than stalling the transport.
pub(crate) fn deliver(streams: &Streams, attempt: &str, channel: u32, bytes: &[u8]) {
    let mut streams = streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (attempt.to_owned(), channel);
    if let Some(open) = streams.get(&key)
        && open.queue.try_send(bytes.to_vec()).is_err()
        && let Some(open) = streams.remove(&key)
    {
        let _ = open.stream.shutdown(Shutdown::Both);
    }
}

/// The daemon side closed: its queued bytes drain, then the job sees EOF.
pub(crate) fn close(streams: &Streams, attempt: &str, channel: u32) {
    streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(attempt.to_owned(), channel));
}

impl Drop for Capability {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket);
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        self.streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(attempt, _), open| {
                if *attempt == self.attempt {
                    let _ = open.stream.shutdown(Shutdown::Both);
                    false
                } else {
                    true
                }
            });
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marsh_contracts::kit_assets::KitSkill;

    #[test]
    fn publishes_kit_instructions_and_skill_links() {
        let root = std::env::temp_dir().join(format!("marsh-kit-assets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("attempt");
        fs::create_dir_all(&dir).unwrap();
        let readable = root.join("context.md");
        fs::write(&readable, "From the mixin.\n").unwrap();
        fs::set_permissions(&readable, fs::Permissions::from_mode(0o644)).unwrap();
        let private = root.join("private.md");
        fs::write(&private, "root only").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
        let capability = JobCapability {
            name: "pix".into(),
            kit: KitAssets {
                skill_destinations: vec!["/opt/a".into(), "/opt/b".into()],
                skills: vec![KitSkill {
                    name: "hello".into(),
                    path: "/opt/skills/hello".into(),
                }],
                contexts: vec![
                    KitContext::Text("Inline.".into()),
                    KitContext::File(readable),
                    KitContext::File(private),
                    KitContext::File(root.join("missing.md")),
                ],
            },
            ..JobCapability::default()
        };
        publish(&dir, &capability).unwrap();
        let context = fs::read_to_string(dir.join("context.md")).unwrap();
        assert!(context.starts_with(&marsh_contracts::process::job_context("pix", &[])));
        assert!(
            context.ends_with("\n\nInline.\n\nFrom the mixin.\n"),
            "{context}"
        );
        assert!(!context.contains("root only"));
        for index in ["0", "1"] {
            assert_eq!(
                fs::read_link(dir.join("skills").join(index).join("hello")).unwrap(),
                Path::new("/opt/skills/hello")
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_kit_without_assets_publishes_no_skills_directory() {
        let root = std::env::temp_dir().join(format!("marsh-kit-none-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let capability = JobCapability {
            name: "shell".into(),
            ..JobCapability::default()
        };
        publish(&root, &capability).unwrap();
        assert!(!root.join("skills").exists());
        assert_eq!(
            fs::read_to_string(root.join("context.md")).unwrap(),
            marsh_contracts::process::job_context("shell", &[])
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
