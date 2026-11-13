//! The real marshd, started from a project directory a job can write, with a
//! fake stock SBX that resolves an unprefixed Kit reference against its own
//! working directory the way stock does (`os.Stat` before the OCI heuristic).
//! The daemon must leave that directory before any stock call, keep every
//! relative path setting meaning what it meant there (also in an upgrade's
//! successor), never run a program planted there, and hand stock only
//! `oci://` Kit references.
use marsh_daemon::{Client, PublicReply, PublicRequest, SessionAuthority};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const KIT: &str =
    "docker.io/acme/kit@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// What the tag names now: a newer image, so the update check reads its
/// contract label with `MARSH_DOCKER`.
const NEWER: &str =
    "docker.io/acme/kit@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SHELL: &str =
    "docker.io/acme/shell@sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
/// The tag the packaged Kit follows on a release build (`latest`).
const TAG: &str = "docker.io/acme/kit:latest";
const PATIENCE: Duration = Duration::from_secs(60);

/// The launched daemon, and any successor it started (its own process group).
struct Daemon {
    child: Child,
    successors: Vec<u32>,
    /// Once an upgrade is installed the launched daemon exits by design
    /// (before its successor binds), so its exit is no failure.
    handing_over: bool,
    /// The lab's private root: any marshd still running from it (a successor
    /// that never got to serve) is killed too.
    root: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for pid in &self.successors {
            signal(*pid, rustix::process::Signal::KILL);
        }
        let _ = Command::new("pkill")
            .args(["-KILL", "-f"])
            .arg(self.root.join("install/bin/marshd"))
            .status();
    }
}

fn signal(pid: u32, signal: rustix::process::Signal) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid.cast_signed()) {
        let _ = rustix::process::kill_process(pid, signal);
    }
}

/// A scratch tree: the selected home, the control parent, a project with a
/// fake stock, the guest artifacts the daemon is pointed at, and the
/// directories the relative settings name.
struct Lab {
    _temp: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    log: PathBuf,
    local: PathBuf,
}

impl Lab {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        for private in ["selected", "control-parent", "mirror"] {
            let path = root.join(private);
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let project = root.join("project");
        fs::create_dir_all(project.join("tools")).unwrap();
        // A job in the project writes directories named like the Kit
        // references the daemon hands stock, and a program named like the
        // one `MARSH_DOCKER` names.
        for planted in [TAG, KIT, NEWER] {
            fs::create_dir_all(project.join(planted)).unwrap();
        }
        let planted = project.join("sh");
        fs::write(
            &planted,
            format!("#!/bin/sh\n: > '{}'\n", root.join("planted-ran").display()),
        )
        .unwrap();
        fs::set_permissions(&planted, fs::Permissions::from_mode(0o755)).unwrap();
        let guest = root.join("guest");
        fs::create_dir_all(&guest).unwrap();
        fs::write(guest.join("commands.json"), format!(r#"{{"kit":"{KIT}"}}"#)).unwrap();
        fs::write(guest.join("shell-image"), SHELL).unwrap();
        // Recorders for the program settings: they log their cwd and parent.
        for name in ["docker", "opener"] {
            let path = project.join("tools").join(name);
            fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s\\t%s\\t%s\\n' \"$(pwd -P)\" \"$PPID\" \"$*\" >> '{}'\n",
                    root.join(format!("{name}.log")).display()
                ),
            )
            .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let lab = Self {
            _temp: temp,
            log: root.join("calls.log"),
            local: root.join("local-resolutions"),
            root,
            project,
        };
        lab.fake_sbx();
        lab
    }

    fn fake_sbx(&self) {
        let script = format!(
            r#"#!/bin/sh
printf '%s\t%s\t%s\t%s\n' "$(pwd -P)" "$0" "$PPID" "$*" >> '{log}'
case "$1" in
version) echo 'sbx version: v0.51.0 0123456789abcdef0123456789abcdef01234567' ;;
create) echo '  sandbox kit reference' ;;
kit)
  for word in "$@"; do case "$word" in kit|inspect|--format|json|--json) ;; *) ref=$word ;; esac; done
  case "$ref" in
  oci://*) ;;
  *) [ -e "$ref" ] && printf '%s\n' "$ref" >> '{local}' ;;
  esac
  echo '{{"name":"kit","sandbox":{{"image":"{NEWER}"}}}}' ;;
*) exit 1 ;;
esac
"#,
            log = self.log.display(),
            local = self.local.display(),
        );
        let path = self.project.join("tools/sbx");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn home(&self) -> PathBuf {
        self.root.join("selected")
    }

    /// The scope's control directory, the daemon's fixed working directory.
    fn control(&self) -> PathBuf {
        let entries = fs::read_dir(self.root.join("control-parent/control"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        let [control] = entries.as_slice() else {
            panic!("expected one scope control directory: {entries:?}");
        };
        control.canonicalize().unwrap()
    }

    /// `marshd` started from the project, as a client would start it there,
    /// with every path setting relative to the project.
    fn start(&self, marshd: &Path, environment: &[(&str, &str)]) -> Daemon {
        let account = nix::unistd::User::from_uid(nix::unistd::Uid::current())
            .unwrap()
            .unwrap();
        let mut command = Command::new(marshd);
        command
            .args(["--home", "../selected"])
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", &account.dir)
            .env("USER", &account.name)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            // Always the absolute path of the fake: never a real stock.
            .env("MARSH_SBX", self.project.join("tools/sbx"))
            .env(
                "MARSH_CONTROL_HOME",
                self.root.join("control-parent/control"),
            )
            .env("MARSH_GUEST_ARTIFACTS", "../guest")
            .env("MARSH_KIT_IMAGE_CACHE", "../kit-images")
            .env("MARSH_OWNERSHIP_MIRROR", "../mirror/vm-ownership.json")
            .env("MARSH_DEV_CACHE_ROOT", "../dev")
            .env("MARSH_SKILLS_HOME", self.root.join("skills"))
            // A bare name: the host's `sh` on the fixed host command PATH,
            // never the project's.
            .env("MARSH_DOCKER", "sh")
            .envs(environment.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(run) = std::env::var_os("MARSH_TEST_RUN") {
            command.env("MARSH_TEST_RUN", run);
        }
        Daemon {
            child: command.spawn().unwrap(),
            successors: Vec::new(),
            handing_over: false,
            root: self.root.clone(),
        }
    }

    /// The pid of the daemon serving the selected home, once one answers.
    fn serving(&self) -> Option<u32> {
        let reply = Client::connect(&self.home())
            .ok()?
            .request(PublicRequest::Ping)
            .ok()?;
        match reply {
            PublicReply::Pong { pid, .. } => pid,
            _ => None,
        }
    }

    /// The refusal of a shell launched in `directory`, if it is refused.
    fn attach_refusal(&self, directory: &Path) -> Option<String> {
        let session = SessionAuthority {
            username: "fixture".into(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            launch_directory: directory.into(),
            guest_home: "/home/fixture".into(),
            home_backing: self.project.clone(),
            ephemeral_home: false,
        };
        let reply = Client::connect(&self.home())
            .unwrap()
            .request(PublicRequest::AttachShell {
                pid: std::process::id(),
                session,
            })
            .unwrap();
        match reply {
            PublicReply::Error { message, .. } => Some(message),
            _ => None,
        }
    }

    /// The daemons that ran the `MARSH_DOCKER` recorder.
    fn docker_parents(&self) -> Vec<u32> {
        fs::read_to_string(self.root.join("docker.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split('\t').nth(1)?.parse().ok())
            .collect()
    }

    fn calls(&self) -> Vec<Call> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let [cwd, program, parent, arguments] =
                    line.splitn(4, '\t').collect::<Vec<_>>()[..]
                else {
                    panic!("malformed call record {line:?}");
                };
                Call {
                    cwd: cwd.into(),
                    program: program.into(),
                    parent: parent.parse().unwrap(),
                    arguments: arguments.into(),
                }
            })
            .collect()
    }

    /// Every stock call so far ran in the daemon's directory, by the absolute
    /// path of the configured stock, and saw only `oci://` Kit references.
    fn check_calls(&self, control: &Path) {
        let sbx = self.project.join("tools/sbx");
        for call in self.calls() {
            assert_eq!(
                call.cwd, control,
                "stock ran outside the control dir: {call:?}"
            );
            assert_eq!(call.program, sbx, "stock ran by another path: {call:?}");
            if let Some(rest) = call.arguments.strip_prefix("kit inspect ") {
                assert!(
                    rest.split(' ')
                        .find(|word| !word.starts_with('-'))
                        .is_some_and(|reference| reference.starts_with("oci://")),
                    "an unprefixed Kit reference reached stock: {call:?}"
                );
            }
        }
        assert_eq!(
            fs::read_to_string(&self.local).unwrap_or_default(),
            "",
            "stock resolved a Kit reference as a local path"
        );
        assert!(
            !self.root.join("planted-ran").exists(),
            "the program planted in the launch directory ran"
        );
    }

    /// The relative settings name the directories beside the project.
    fn check_settings_landed(&self) {
        assert!(self.root.join("kit-images").is_dir(), "the Kit image cache");
        assert!(
            self.root.join("mirror/vm-ownership.json").is_file(),
            "the ownership mirror"
        );
    }
}

#[derive(Debug)]
struct Call {
    cwd: PathBuf,
    program: PathBuf,
    parent: u32,
    arguments: String,
}

fn cwd_of(pid: u32) -> PathBuf {
    if cfg!(target_os = "linux") {
        return fs::read_link(format!("/proc/{pid}/cwd")).unwrap();
    }
    let output = Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), "-d", "cwd", "-Fn"])
        .output()
        .unwrap();
    let listing = String::from_utf8(output.stdout).unwrap();
    PathBuf::from(
        listing
            .lines()
            .find_map(|line| line.strip_prefix('n'))
            .unwrap_or_else(|| panic!("no cwd for {pid}: {listing}")),
    )
}

fn wait_for<T>(what: &str, daemon: &mut Daemon, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(value) = ready() {
            return value;
        }
        if !daemon.handing_over
            && let Some(status) = daemon.child.try_wait().unwrap()
        {
            panic!("marshd exited ({status}) before {what}");
        }
        assert!(Instant::now() < deadline, "no {what} within {PATIENCE:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

fn stop(daemon: &mut Daemon, pid: u32) {
    signal(pid, rustix::process::Signal::TERM);
    let deadline = Instant::now() + Duration::from_secs(20);
    while rustix::process::test_kill_process(
        rustix::process::Pid::from_raw(pid.cast_signed()).unwrap(),
    )
    .is_ok()
    {
        let _ = daemon.child.try_wait();
        assert!(
            Instant::now() < deadline,
            "marshd {pid} did not stop on SIGTERM"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_daemon_leaves_its_launch_directory_before_any_stock_call() {
    let lab = Lab::new();
    let mut daemon = lab.start(Path::new(env!("CARGO_BIN_EXE_marshd")), &[]);
    let pid = daemon.child.id();
    // The start-up Kit update check resolves the packaged Kit's tag to a
    // newer image and reads its contract with MARSH_DOCKER.
    let record = wait_for("a finished Kit update check", &mut daemon, || {
        let control = fs::read_dir(lab.root.join("control-parent/control"))
            .ok()?
            .next()?
            .ok()?
            .path();
        let record = fs::read_to_string(control.join("kit-updates.json")).ok()?;
        record.contains("last_check").then_some(record)
    });
    assert_eq!(
        lab.serving(),
        Some(pid),
        "the daemon serves the selected home"
    );
    let control = lab.control();
    assert_eq!(cwd_of(pid), control, "the daemon's cwd");
    assert!(lab.calls().iter().any(|call| call.arguments.contains(TAG)));
    lab.check_calls(&control);
    // The host's `sh` read Docker's arguments as a script name.
    assert!(record.contains("buildx"), "{record}");
    lab.check_settings_landed();
    // The guest artifacts the relative setting names stay private host
    // state: no shell may mount them.
    let refusal = lab
        .attach_refusal(&lab.root.join("guest"))
        .unwrap_or_default();
    assert!(
        refusal.contains(&format!(
            "private host state at {}",
            lab.root.join("guest").display()
        )),
        "{refusal}"
    );
    stop(&mut daemon, pid);
}

#[test]
fn an_upgrade_successor_keeps_the_launch_settings() {
    let lab = Lab::new();
    // An install tree: bin/marshd and the libexec/marsh it checks itself
    // against (`--check-config`).
    let bin = lab.root.join("install/bin");
    let libexec = lab.root.join("install/libexec/marsh");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&libexec).unwrap();
    for file in ["commands.json", "shell-image"] {
        fs::copy(lab.root.join("guest").join(file), libexec.join(file)).unwrap();
    }
    let marshd = bin.join("marshd");
    fs::copy(env!("CARGO_BIN_EXE_marshd"), &marshd).unwrap();
    // Program settings given as paths relative to the project: each daemon's
    // start-up Kit update check runs MARSH_DOCKER for the newer image.
    let mut daemon = lab.start(
        &marshd,
        &[
            ("MARSH_DOCKER", "tools/docker"),
            ("MARSH_OPEN_COMMAND", "tools/opener"),
        ],
    );
    let first = daemon.child.id();
    wait_for("the daemon to serve", &mut daemon, || lab.serving());
    wait_for("the daemon's Docker read", &mut daemon, || {
        lab.docker_parents().contains(&first).then_some(())
    });
    lab.check_settings_landed();
    // The successor must make these again, where the settings name them.
    fs::remove_dir(lab.root.join("kit-images")).unwrap();
    fs::remove_file(lab.root.join("mirror/vm-ownership.json")).unwrap();

    // Install a different build of the same version over it.
    let staged = bin.join("marshd.new");
    fs::copy(&marshd, &staged).unwrap();
    let mut bytes = fs::read(&staged).unwrap();
    bytes.push(0);
    fs::write(&staged, bytes).unwrap();
    fs::rename(&staged, &marshd).unwrap();
    daemon.handing_over = true;

    let successor = wait_for("the successor to serve", &mut daemon, || {
        lab.serving().filter(|pid| *pid != first)
    });
    daemon.successors.push(successor);
    assert!(
        daemon.child.wait().unwrap().success(),
        "the replaced daemon exits"
    );
    let control = lab.control();
    assert_eq!(cwd_of(successor), control, "the successor's cwd");
    lab.check_settings_landed();
    assert!(
        lab.calls().iter().any(|call| call.parent == successor),
        "the successor ran its stock checks"
    );
    lab.check_calls(&control);
    // The successor started in `/` and still ran the Docker the relative
    // setting named, so it was handed the resolved program.
    wait_for("the successor's Docker read", &mut daemon, || {
        lab.docker_parents().contains(&successor).then_some(())
    });
    if cfg!(target_os = "linux") {
        let environment = fs::read(format!("/proc/{successor}/environ")).unwrap();
        let expected = format!(
            "MARSH_OPEN_COMMAND={}",
            lab.project.join("tools/opener").display()
        );
        assert!(
            environment
                .split(|byte| *byte == 0)
                .any(|entry| entry == expected.as_bytes()),
            "the successor's opener setting"
        );
    }
    stop(&mut daemon, successor);
}
