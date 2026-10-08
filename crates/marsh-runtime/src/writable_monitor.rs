//! Detect-and-kill enforcement of the per-job writable-layer limit.
//!
//! The Kit VM's Docker Engine stores containers with the containerd overlayfs
//! snapshotter, which ignores `--storage-opt size`. The worker therefore
//! measures the running container's overlay upper directory (the job's
//! writable layer, resolved from the container init's own `/proc/PID/mountinfo`)
//! while `docker wait` blocks, and kills the container's cgroup once usage
//! exceeds the limit. Detection is periodic: a fast writer can exceed the limit
//! by what it writes in one sampling window before the kill lands.

use marsh_contracts::ContainerId;
use std::{
    collections::HashSet,
    fs, io,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// Sampling period for a small writable layer. Walking a few dozen entries
/// costs microseconds; a Kit VM disk absorbs gigabytes per second, so the
/// period, not the walk, bounds how far past the limit a writer can get.
pub(crate) const WRITABLE_SAMPLE_INTERVAL: Duration = Duration::from_millis(20);
/// The monitor sleeps at least this many times its last walk's duration, so
/// a very large writable tree costs at most ~1/(1+N) (10%) of one CPU per job.
const WALK_BACKOFF_FACTOR: u32 = 9;
const MAX_MOUNTINFO_BYTES: u64 = 1024 * 1024;

/// One running container's writable-layer enforcement inputs.
pub(crate) struct WritableTarget {
    pub(crate) pid: u32,
    pub(crate) container: ContainerId,
    pub(crate) cgroup: PathBuf,
    pub(crate) upper: PathBuf,
    pub(crate) limit: u64,
}

/// Background sampler joined before the terminal container inspection.
pub(crate) struct WritableMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<bool>>,
}

impl WritableMonitor {
    pub(crate) fn start(target: Option<WritableTarget>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = target.map(|target| {
            let thread_stop = Arc::clone(&stop);
            thread::spawn(move || monitor(&target, &thread_stop))
        });
        Self { stop, handle }
    }

    /// Stop sampling; true when this monitor killed the container for
    /// exceeding its writable limit.
    pub(crate) fn finish(mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        self.handle.take().is_some_and(|handle| {
            handle.thread().unpark();
            handle.join().unwrap_or(false)
        })
    }
}

impl Drop for WritableMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

fn monitor(target: &WritableTarget, stop: &AtomicBool) -> bool {
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let over = writable_usage_exceeds(&target.upper, target.limit);
        // A failed kill (e.g. the container is already exiting) is retried
        // on the next sample until `docker wait` returns and stops us.
        if over && kill_container(target).is_ok() {
            return true;
        }
        let pause = WRITABLE_SAMPLE_INTERVAL.max(started.elapsed() * WALK_BACKOFF_FACTOR);
        if stop.load(Ordering::Acquire) {
            break;
        }
        thread::park_timeout(pause);
    }
    false
}

/// The overlay upper directory backing the container's root filesystem, as
/// seen from the worker's mount namespace.
pub(crate) fn container_upperdir(pid: u32) -> io::Result<Option<PathBuf>> {
    use std::io::Read;
    let mut text = String::new();
    fs::File::open(format!("/proc/{pid}/mountinfo"))?
        .take(MAX_MOUNTINFO_BYTES)
        .read_to_string(&mut text)?;
    let Some(upper) = root_upperdir(&text) else {
        return Ok(None);
    };
    let metadata = fs::symlink_metadata(&upper)?;
    Ok(metadata.is_dir().then_some(upper))
}

/// Parse `upperdir=` from the topmost overlay mounted at `/`. Escaped option
/// text (mountinfo octal escapes, overlay's `\,`) is refused, not decoded.
fn root_upperdir(mountinfo: &str) -> Option<PathBuf> {
    let line = mountinfo
        .lines()
        .rev()
        .find(|line| line.split(' ').nth(4) == Some("/"))?;
    let (_, tail) = line.split_once(" - ")?;
    let mut fields = tail.split(' ');
    if fields.next()? != "overlay" {
        return None;
    }
    let options = fields.nth(1)?;
    if options.contains('\\') {
        return None;
    }
    let upper = options
        .split(',')
        .find_map(|option| option.strip_prefix("upperdir="))?;
    let path = PathBuf::from(upper);
    let normal = path.is_absolute()
        && path
            .components()
            .skip(1)
            .all(|component| matches!(component, Component::Normal(_)));
    normal.then_some(path)
}

/// Allocated bytes in the writable tree (`st_blocks`, each hard-linked inode
/// once, one filesystem), stopping as soon as `limit` is exceeded.
pub(crate) fn writable_usage_exceeds(root: &Path, limit: u64) -> bool {
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return false;
    };
    let device = metadata.dev();
    let mut total = allocated(&metadata);
    let mut linked = HashSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        // Entries can vanish while the job runs; count what remains.
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.dev() != device {
                continue;
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.nlink() > 1 && !linked.insert(metadata.ino()) {
                continue;
            }
            total = total.saturating_add(allocated(&metadata));
            if total > limit {
                return true;
            }
        }
    }
    total > limit
}

fn allocated(metadata: &fs::Metadata) -> u64 {
    metadata.blocks().saturating_mul(512)
}

fn kill_container(target: &WritableTarget) -> io::Result<()> {
    let by_cgroup = fs::OpenOptions::new()
        .write(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(target.cgroup.join("cgroup.kill"))
        .and_then(|mut file| file.write_all(b"1"));
    if by_cgroup.is_ok() {
        return by_cgroup;
    }
    // Kernels without cgroup.kill: SIGKILL the container's init (PID 1 of
    // its namespace, which takes every descendant with it), only while that
    // PID still belongs to this container's cgroup.
    let cgroup = fs::read_to_string(format!("/proc/{}/cgroup", target.pid))?;
    if !cgroup.contains(target.container.as_str()) {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    let pid = i32::try_from(target.pid).map_err(io::Error::other)?;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_upperdir_reads_the_topmost_root_overlay() {
        let mountinfo = "\
105 87 0:47 / / rw - overlay overlay rw,lowerdir=/a/1/fs,upperdir=/old/fs,workdir=/old/work
106 87 0:48 / / rw,relatime - overlay overlay rw,lowerdir=/s/9/fs:/s/8/fs,upperdir=/s/10/fs,workdir=/s/10/work,index=off
107 106 0:49 / /usr/sbin/docker-init rw - overlay overlay rw,lowerdir=/x,upperdir=/y,workdir=/z
108 106 0:50 / /proc rw - proc proc rw
";
        assert_eq!(root_upperdir(mountinfo), Some(PathBuf::from("/s/10/fs")));
    }

    #[test]
    fn root_upperdir_refuses_non_overlay_escaped_or_relative_roots() {
        for mountinfo in [
            "1 0 0:1 / / rw - ext4 /dev/vda rw\n",
            "1 0 0:1 / / rw - overlay overlay rw,upperdir=/s/a\\054b/fs,workdir=/w\n",
            "1 0 0:1 / / rw - overlay overlay rw,upperdir=s/10/fs,workdir=/w\n",
            "1 0 0:1 / / rw - overlay overlay rw,upperdir=/s/../etc,workdir=/w\n",
            "1 0 0:1 / / rw - overlay overlay rw,lowerdir=/s/1/fs\n",
            "1 0 0:1 / /proc rw - overlay overlay rw,upperdir=/s/10/fs\n",
        ] {
            assert_eq!(root_upperdir(mountinfo), None, "{mountinfo}");
        }
    }

    #[test]
    fn usage_counts_allocated_bytes_once_per_inode_and_stops_at_the_limit() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("tmp/deep");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("data");
        let mut writer = fs::File::create(&file).unwrap();
        writer.write_all(&vec![b'w'; 4 * 1024 * 1024]).unwrap();
        writer.sync_all().unwrap();
        fs::hard_link(&file, root.path().join("again")).unwrap();
        // A sparse file allocates (almost) nothing.
        fs::File::create(root.path().join("sparse"))
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        assert!(writable_usage_exceeds(root.path(), 3 * 1024 * 1024));
        assert!(!writable_usage_exceeds(root.path(), 6 * 1024 * 1024));
        assert!(!writable_usage_exceeds(&root.path().join("absent"), 0));
    }
}
