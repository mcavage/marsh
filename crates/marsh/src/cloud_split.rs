//! `split { ... } | join` inside a cloud block: forks, branches, capture and
//! rendering in the sandbox itself (`marsh_daemon::split::LocalSplit`). The
//! cloud placement node is the admitted job; its branches are its processes,
//! killed with it. Decision of 2026-10-17 in `docs/design/cloud-placement.md`.

use brush_shell::bundled::split::{SplitBranchRequest, SplitHandle, SplitRun, SplitWorkspace};
use marsh_daemon::split::LocalSplit;
use std::fs::File;
use std::io::Write as _;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The cloud workspace root, set by the helper for the workload.
pub const ROOT_VARIABLE: &str = "MARSH_CLOUD_ROOT";
const CANCEL_GRACE: Duration = Duration::from_secs(10);

pub struct CloudSplit {
    pub root: PathBuf,
}

struct Running {
    split: LocalSplit,
    children: Arc<Mutex<Vec<Option<Child>>>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    json: bool,
}

fn signal_groups(children: &Mutex<Vec<Option<Child>>>, signal: nix::sys::signal::Signal) {
    let children = children
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for child in children.iter().flatten() {
        if let Ok(pid) = i32::try_from(child.id()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
        }
    }
}

impl SplitWorkspace for CloudSplit {
    fn start(
        &self,
        cwd: &Path,
        branches: &[SplitBranchRequest],
        environment: Vec<(String, String)>,
        input: Vec<u8>,
        json: bool,
    ) -> Result<Box<dyn SplitHandle>, String> {
        let labels = branches
            .iter()
            .map(|branch| branch.label.clone())
            .collect::<Vec<_>>();
        let split = LocalSplit::create(&self.root, cwd, &labels)?;
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut children = Vec::new();
        for (index, branch) in branches.iter().enumerate() {
            let out = split.out(index);
            let stdout = File::create(out.join("stdout")).map_err(|e| e.to_string())?;
            let stderr = File::create(out.join("stderr")).map_err(|e| e.to_string())?;
            let mut command = Command::new(&executable);
            command
                .arg0("marsh")
                .args(["-c", &branch.source])
                .current_dir(split.cwd(index))
                .envs(
                    environment
                        .iter()
                        .filter(|(name, _)| marsh_daemon::split::forwardable(name))
                        .map(|(name, value)| (name, value)),
                )
                .env("SPLIT_BRANCH", &branch.label)
                .stdin(Stdio::piped())
                .stdout(stdout)
                .stderr(stderr)
                .process_group(0);
            match command.spawn() {
                Ok(mut child) => {
                    if let Some(mut stdin) = child.stdin.take() {
                        let input = input.clone();
                        std::thread::spawn(move || {
                            let _ = stdin.write_all(&input);
                        });
                    }
                    children.push(Some(child));
                }
                Err(error) => {
                    let _ = writeln!(
                        File::options()
                            .append(true)
                            .open(out.join("stderr"))
                            .map_err(|e| e.to_string())?,
                        "split: {error}"
                    );
                    children.push(None);
                }
            }
        }
        Ok(Box::new(Running {
            split,
            children: Arc::new(Mutex::new(children)),
            cancelled: Arc::default(),
            json,
        }))
    }
}

impl SplitHandle for Running {
    fn canceller(&self) -> Box<dyn Fn() + Send + Sync> {
        let children = Arc::clone(&self.children);
        let cancelled = Arc::clone(&self.cancelled);
        Box::new(move || {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
            signal_groups(&children, nix::sys::signal::Signal::SIGINT);
            let children = Arc::clone(&children);
            std::thread::spawn(move || {
                std::thread::sleep(CANCEL_GRACE);
                signal_groups(&children, nix::sys::signal::Signal::SIGKILL);
            });
        })
    }

    fn wait(mut self: Box<Self>) -> Result<SplitRun, String> {
        let count = self
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        let mut codes = Vec::with_capacity(count);
        for index in 0..count {
            let child = self
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[index]
                .take();
            let code = child.and_then(|mut child| {
                let status = child.wait().ok();
                // The branch's leftover processes must not write after capture.
                if let Ok(pid) = i32::try_from(child.id()) {
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(pid),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                status.map(|status| {
                    status
                        .code()
                        .or_else(|| status.signal().map(|signal| 128 + signal))
                        .unwrap_or(125)
                })
            });
            codes.push(code.or(Some(127)));
        }
        let cancelled = self.cancelled.load(std::sync::atomic::Ordering::Acquire);
        for (index, code) in codes.into_iter().enumerate() {
            self.split
                .finish(index, if cancelled { None } else { code });
        }
        let status = self.split.settle(cancelled);
        if cancelled {
            let _ = self.split.remove();
            return Ok(SplitRun {
                status: 130,
                cancelled: true,
                input: Vec::new(),
                environment: Vec::new(),
                release: Box::new(|_, _| Vec::new()),
            });
        }
        let input = if self.json {
            format!("{}\n", self.split.manifest()).into_bytes()
        } else {
            self.split.render()
        };
        let environment = self.split.environment();
        let split = self.split;
        Ok(SplitRun {
            status: u8::try_from(status).unwrap_or(1),
            cancelled: false,
            input,
            environment,
            release: Box::new(move |last, keep| {
                if last == 0 && !keep {
                    match split.remove() {
                        Ok(()) => Vec::new(),
                        Err(error) => vec![format!("split {}: not removed: {error}", split.id())],
                    }
                } else {
                    vec![format!(
                        "kept split {} at {} (in the cloud sandbox; it is deleted with the sandbox)",
                        split.id(),
                        split.dir().display()
                    )]
                }
            }),
        })
    }
}
