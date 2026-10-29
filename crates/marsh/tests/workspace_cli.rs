//! Checkpoints, open lanes, storage, and events through the public CLI
//! against a real authenticated daemon. No SBX backend or VM is involved: a
//! lane's edits are made directly in its fork, as a lane job would make them,
//! and a lane job's admission is observed at the daemon boundary.

use marsh_daemon::{
    CleanupState, EndpointPaths, ExitStatus, NewJob, Placement, PublicReply, PublicRequest, Server,
    SessionAuthority, TimingReport,
};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    _directory: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    endpoint: EndpointPaths,
    token: PathBuf,
    store: Option<marsh_daemon::DaemonStore>,
    session: String,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_retention(Duration::from_hours(7 * 24))
    }

    /// A daemon that removes kept splits `retention` after they were kept.
    fn with_retention(retention: Duration) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let home = root.join("scope");
        let project = root.join("project");
        fs::create_dir(&home).unwrap();
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join("src/lib.rs"), "fn a() {}\n").unwrap();
        fs::write(project.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir(project.join("target")).unwrap();
        fs::write(project.join("target/big"), "ignored").unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(&project, &["add", "."]);
        git(&project, &["commit", "-q", "-m", "init"]);
        let server = Server::bind(&home)
            .unwrap()
            .with_kept_split_retention(retention);
        let store = server.store();
        let session = store.attach_shell(
            std::process::id(),
            SessionAuthority {
                username: "fixture".into(),
                uid: nix::unistd::Uid::effective().as_raw(),
                gid: nix::unistd::Gid::effective().as_raw(),
                launch_directory: project.clone(),
                guest_home: root.clone(),
                home_backing: home.join("home"),
                ephemeral_home: false,
            },
        );
        let endpoint = EndpointPaths::for_home(&home).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let server = thread::spawn(move || {
            server
                .serve_until(|| stopping.load(Ordering::Acquire))
                .unwrap();
        });
        Self {
            token: endpoint.token.clone(),
            store: Some(store),
            _directory: directory,
            home,
            project,
            endpoint,
            session,
            stop,
            server: Some(server),
        }
    }

    /// Stop the daemon and start another on the same home, as a crash or an
    /// upgrade does: only what reached the journal on disk carries over.
    fn restart(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.server.take().unwrap().join().unwrap();
        // The journal is locked by every handle on the old daemon's store.
        self.store = None;
        let server = Server::bind(&self.home).unwrap();
        self.store = Some(server.store());
        self.session = self.store().attach_shell(
            std::process::id(),
            SessionAuthority {
                username: "fixture".into(),
                uid: nix::unistd::Uid::effective().as_raw(),
                gid: nix::unistd::Gid::effective().as_raw(),
                launch_directory: self.project.clone(),
                guest_home: self.home.parent().unwrap().to_path_buf(),
                home_backing: self.home.join("home"),
                ephemeral_home: false,
            },
        );
        self.endpoint = EndpointPaths::for_home(&self.home).unwrap();
        self.token = self.endpoint.token.clone();
        self.stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&self.stop);
        self.server = Some(thread::spawn(move || {
            server
                .serve_until(|| stopping.load(Ordering::Acquire))
                .unwrap();
        }));
    }

    fn store(&self) -> &marsh_daemon::DaemonStore {
        self.store.as_ref().unwrap()
    }

    fn command(&self, cwd: &Path, arguments: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_marsh"));
        command
            .args(arguments)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("USER", "fixture")
            .env("HOME", self.home.parent().unwrap())
            .env("MARSH_HOME", &self.home)
            .env("MARSH_DAEMON_SOCKET", &self.endpoint.socket)
            .env("MARSH_DAEMON_TOKEN", &self.token)
            .env("MARSH_EXTERNAL_SESSION", self.context(&self.session))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// A session's guest context, as a shell exports it to its commands.
    fn context(&self, session: &str) -> String {
        serde_json::json!({
            "session_id": session,
            "username": "fixture",
            "uid": nix::unistd::Uid::effective().as_raw(),
            "gid": nix::unistd::Gid::effective().as_raw(),
            "launch_directory": self.project,
            "guest_home": self.home.parent().unwrap(),
            "home_backing": self.home.join("home"),
            "ephemeral_home": false,
        })
        .to_string()
    }

    fn run_in(&self, cwd: &Path, arguments: &[&str]) -> Output {
        let output = self.command(cwd, arguments).output().unwrap();
        eprintln!("marsh {arguments:?} -> {output:?}");
        output
    }

    fn json(&self, arguments: &[&str]) -> serde_json::Value {
        let output = self.run_in(&self.project, arguments);
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn client(&self) -> marsh_daemon::Client {
        marsh_daemon::Client::connect_relay(&self.endpoint.socket, &self.token).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.server.take().unwrap().join().unwrap();
    }
}

fn git(dir: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@x",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(arguments)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {arguments:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

/// Lines of a running `marsh events --json`, read on a thread.
struct Events {
    child: Child,
    lines: std::sync::mpsc::Receiver<serde_json::Value>,
}

impl Events {
    fn next_where(&self, wanted: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("event before the deadline");
            if wanted(&line) {
                return line;
            }
        }
    }
}

impl Drop for Events {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn subscribe(fixture: &Fixture) -> Events {
    let mut child = fixture
        .command(&fixture.project, &["events", "--json"])
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, lines) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            let _ = sender.send(serde_json::from_str(&line).expect("one JSON object per line"));
        }
    });
    Events { child, lines }
}

#[test]
fn checkpoints_keep_the_tree_and_git_state_and_retain_twenty_per_label() {
    let fixture = Fixture::new();
    let events = subscribe(&fixture);
    let hello = events.next_where(|line| line["type"] == "hello");
    for feature in [
        "events-v1",
        "lanes-v1",
        "checkpoints-v1",
        "acp-descriptors-v1",
        "labels-v1",
        "apply-json-v1",
        "lane-cloud-v1",
        "lane-shell-v1",
        "lane-checkpoints-v1",
        "cloud-sync-v1",
    ] {
        assert!(
            hello["features"]
                .as_array()
                .unwrap()
                .contains(&feature.into()),
            "{hello}"
        );
    }
    let first = fixture.json(&["checkpoint", "--label", "turn", "--json"]);
    let id = first["checkpoint"].as_str().unwrap().to_owned();
    assert_eq!(first["label"], "turn");
    let event = events.next_where(|line| line["type"] == "checkpoint");
    assert_eq!(event["checkpoint"]["id"], id.as_str());
    assert_eq!(event["checkpoint"]["removed"], false);
    assert!(event["seq"].as_u64().unwrap() > hello["seq"].as_u64().unwrap());

    // The tree is the project as a split base sees it; Git state is saved.
    let dir = fixture.project.join(".marsh/checkpoints").join(&id);
    assert_eq!(
        fs::read_to_string(dir.join("tree/src/lib.rs")).unwrap(),
        "fn a() {}\n"
    );
    assert!(!dir.join("tree/target").exists() && !dir.join("tree/.git").exists());
    assert_eq!(
        fs::read_to_string(dir.join("git/HEAD")).unwrap().trim(),
        git(&fixture.project, &["rev-parse", "HEAD"]).trim()
    );
    assert!(dir.join("git/index").is_file());
    // `.marsh` is kept out of Time Machine and Spotlight.
    assert!(
        fixture
            .project
            .join(".marsh/.metadata_never_index")
            .is_file()
    );
    #[cfg(target_os = "macos")]
    {
        let excluded = Command::new("tmutil")
            .arg("isexcluded")
            .arg(fixture.project.join(".marsh"))
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&excluded.stdout).contains("[Excluded]"),
            "{excluded:?}"
        );
    }

    // Newest 20 per label; the oldest is removed (and announced).
    for _ in 0..20 {
        fixture.json(&["checkpoint", "--label", "turn", "--json"]);
    }
    let removed = events.next_where(|line| line["checkpoint"]["removed"] == true);
    assert_eq!(removed["checkpoint"]["id"], id.as_str());
    let listed = fixture.json(&["checkpoints", "--json", "--label", "turn"]);
    assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 20);
    assert!(
        listed["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .all(|checkpoint| checkpoint["checkpoint"] != id.as_str())
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while dir.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !dir.exists(),
        "a retired checkpoint's files go to .trash and away"
    );

    let storage = fixture.json(&["storage", "--json"]);
    let project = &storage["projects"][0];
    assert_eq!(project["root"], fixture.project.to_str().unwrap());
    assert_eq!(project["checkpoints"]["count"], 20);
    assert!(project["checkpoints"]["bytes"].as_u64().unwrap() >= 20 * 10);
    assert!(project["volume"]["free_bytes"].as_u64().unwrap() > 0);

    let newest = listed["checkpoints"][0]["checkpoint"].as_str().unwrap();
    let output = fixture.run_in(&fixture.project, &["checkpoints", "rm", newest]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let listed = fixture.json(&["checkpoints", "--json"]);
    assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 19);

    // Never inside .marsh, and the subscription never holds off shutdown.
    let output = fixture.run_in(dir.parent().unwrap(), &["checkpoint"]);
    assert_ne!(output.status.code(), Some(0));
    fixture.store().detach_shell(&fixture.session).unwrap();
    assert!(matches!(
        fixture.client().request(PublicRequest::Shutdown).unwrap(),
        PublicReply::ShuttingDown
    ));
}

#[test]
fn open_lanes_from_a_checkpoint_capture_their_forks_and_admit_only_the_creator() {
    let fixture = Fixture::new();
    let checkpoint = fixture.json(&["checkpoint", "--json"]);
    let checkpoint = checkpoint["checkpoint"].as_str().unwrap();
    // The project moves on; the lanes start from the checkpoint.
    fs::write(fixture.project.join("src/lib.rs"), "fn later() {}\n").unwrap();
    let events = subscribe(&fixture);
    events.next_where(|line| line["type"] == "hello");
    let opened = fixture.json(&["split", "--open", "--from", checkpoint, "a", "b"]);
    assert_eq!(opened["marsh_split"], 1);
    let id = opened["id"].as_str().unwrap().to_owned();
    let lane_a = PathBuf::from(opened["lanes"]["a"].as_str().unwrap());
    let lane_b = PathBuf::from(opened["lanes"]["b"].as_str().unwrap());
    assert_eq!(
        lane_a,
        fixture.project.join(".marsh/split").join(&id).join("a")
    );
    let event = events.next_where(|line| line["type"] == "split");
    assert_eq!(event["split"]["id"], id.as_str());
    assert_eq!(event["split"]["state"], "run");
    assert_eq!(
        fs::read_to_string(lane_a.join("src/lib.rs")).unwrap(),
        "fn a() {}\n"
    );
    // Each fork is a Git checkout of the checkpoint's HEAD.
    assert_eq!(
        git(&lane_a, &["rev-parse", "HEAD"]),
        git(&fixture.project, &["rev-parse", "HEAD"])
    );
    assert_eq!(git(&lane_a, &["status", "--porcelain"]), "");

    // A non-creator (a shell session) may not start a job in the lane.
    let shell = fixture.store().attach_shell(
        std::process::id(),
        SessionAuthority {
            username: "fixture".into(),
            uid: nix::unistd::Uid::effective().as_raw(),
            gid: nix::unistd::Gid::effective().as_raw(),
            launch_directory: fixture.project.clone(),
            guest_home: fixture.home.parent().unwrap().to_path_buf(),
            home_backing: fixture.home.join("home"),
            ephemeral_home: false,
        },
    );
    let relay = fixture.home.parent().unwrap().join("relay-token");
    fs::write(&relay, fixture.store().issue_relay_token(&shell).unwrap()).unwrap();
    fs::set_permissions(&relay, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let refused = fixture
        .command(&lane_a, &["run", "claude", "-p", "x"])
        .env("MARSH_DAEMON_TOKEN", &relay)
        .env("MARSH_EXTERNAL_SESSION", fixture.context(&shell))
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(125), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("only its creator"),
        "{refused:?}"
    );

    // What a lane job would do: edit its fork.
    fs::write(lane_a.join("src/lib.rs"), "fn a() { staged(); }\n").unwrap();
    fs::write(lane_b.join("new.txt"), "b\n").unwrap();
    let closed = fixture.json(&["split", "close", &id, "--json"]);
    assert_eq!(closed["state"], "kept");
    assert_eq!(closed["from"], checkpoint);
    for lane in closed["branches"].as_array().unwrap() {
        assert_eq!(lane["kind"], "lane");
        assert_eq!(lane["state"], "captured", "{lane}");
        assert_eq!(lane["files"], 1);
    }
    let out = PathBuf::from(closed["out"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(out.join("a/files")).unwrap(),
        "M\tsrc/lib.rs\n"
    );
    assert_eq!(
        fs::read_to_string(out.join("a/status")).unwrap(),
        "closed\n"
    );
    events.next_where(|line| line["type"] == "split" && line["split"]["state"] == "kept");

    // A closed lane admits nothing more, from anyone.
    let refused = fixture
        .command(&lane_a, &["run", "claude", "-p", "x"])
        .env("MARSH_DAEMON_TOKEN", &relay)
        .env("MARSH_EXTERNAL_SESSION", fixture.context(&shell))
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(125), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("closed"),
        "{refused:?}"
    );

    // Apply a lane: the project gets the lane's edit on top of the checkpoint.
    git(&fixture.project, &["checkout", "--", "src/lib.rs"]);
    let applied = fixture.run_in(&fixture.project, &["split", "apply", &id, "b", "--3way"]);
    assert_eq!(applied.status.code(), Some(0), "{applied:?}");
    assert_eq!(
        fs::read_to_string(fixture.project.join("new.txt")).unwrap(),
        "b\n"
    );

    let storage = fixture.json(&["storage", "--json"]);
    assert_eq!(storage["projects"][0]["kept_splits"]["count"], 1);
    let output = fixture.run_in(&fixture.project, &["splits", "rm", &id]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    events.next_where(|line| line["type"] == "split" && line["split"]["state"] == "removed");
}

/// A relay token file for a new shell session in the project, and its context.
fn other_session(fixture: &Fixture, name: &str) -> (PathBuf, String) {
    let shell = fixture.store().attach_shell(
        std::process::id(),
        SessionAuthority {
            username: "fixture".into(),
            uid: nix::unistd::Uid::effective().as_raw(),
            gid: nix::unistd::Gid::effective().as_raw(),
            launch_directory: fixture.project.clone(),
            guest_home: fixture.home.parent().unwrap().to_path_buf(),
            home_backing: fixture.home.join("home"),
            ephemeral_home: false,
        },
    );
    let token = fixture.home.parent().unwrap().join(name);
    fs::write(&token, fixture.store().issue_relay_token(&shell).unwrap()).unwrap();
    fs::set_permissions(&token, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    (token, fixture.context(&shell))
}

/// A host `marsh split` of its own: the host CLI's session lasts one split.
fn host_split(fixture: &Fixture, arguments: &[&str]) -> serde_json::Value {
    let shell = fixture.store().attach_shell(
        std::process::id(),
        SessionAuthority {
            username: "fixture".into(),
            uid: nix::unistd::Uid::effective().as_raw(),
            gid: nix::unistd::Gid::effective().as_raw(),
            launch_directory: fixture.project.clone(),
            guest_home: fixture.home.parent().unwrap().to_path_buf(),
            home_backing: fixture.home.join("home"),
            ephemeral_home: false,
        },
    );
    let output = fixture
        .command(&fixture.project, arguments)
        .env("MARSH_EXTERNAL_SESSION", fixture.context(&shell))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
#[allow(clippy::too_many_lines)] // One journey: checkpoint, refuse, start from it.
fn a_lane_checkpoint_starts_the_next_split_from_the_lanes_state_for_its_creator_only() {
    let fixture = Fixture::new();
    let opened = fixture.json(&["split", "--open", "a"]);
    let id = opened["id"].as_str().unwrap().to_owned();
    let lane = PathBuf::from(opened["lanes"]["a"].as_str().unwrap());
    let name = format!("{id}/a");
    // What a lane job would do: edit its fork and commit with the lane's Git.
    fs::write(lane.join("src/lib.rs"), "fn a() { lane(); }\n").unwrap();
    fs::write(lane.join("new.txt"), "from the lane\n").unwrap();
    fs::create_dir(lane.join("target")).unwrap();
    fs::write(lane.join("target/ignored"), "x").unwrap();
    git(&lane, &["add", "src/lib.rs", "new.txt"]);
    git(&lane, &["commit", "-q", "-m", "lane work"]);
    fs::write(lane.join("untracked.txt"), "u\n").unwrap();
    fs::write(lane.join("src/lib.rs"), "fn a() { staged(); }\n").unwrap();
    git(&lane, &["add", "src/lib.rs"]);
    let lane_head = git(&lane, &["rev-parse", "HEAD"]);
    assert_ne!(lane_head, git(&fixture.project, &["rev-parse", "HEAD"]));

    // Only the creator (here the host) may: not another session, not a job.
    let (other, context) = other_session(&fixture, "other-token");
    let refused = fixture
        .command(&fixture.project, &["checkpoint", "--lane", &name])
        .env("MARSH_DAEMON_TOKEN", &other)
        .env("MARSH_EXTERNAL_SESSION", &context)
        .output()
        .unwrap();
    assert_ne!(refused.status.code(), Some(0), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("only its creator"),
        "{refused:?}"
    );
    let (job, _) = fixture
        .store()
        .begin_job_at(lane_job(&fixture, &lane), Placement::Local)
        .unwrap();
    let capability = fixture.home.parent().unwrap().join("job-token");
    fs::write(
        &capability,
        fixture.store().issue_capability_token(&job).unwrap(),
    )
    .unwrap();
    fs::set_permissions(
        &capability,
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )
    .unwrap();
    let reply = marsh_daemon::Client::connect_relay(&fixture.endpoint.socket, &capability)
        .unwrap()
        .request(PublicRequest::Checkpoint {
            root: Some(fixture.project.clone()),
            action: marsh_daemon::CheckpointAction::CreateLane {
                lane: name.clone(),
                label: None,
            },
        });
    assert!(
        format!("{reply:?}").contains("job capability admits only"),
        "{reply:?}"
    );
    fixture
        .store()
        .finish_job(
            &job,
            ExitStatus {
                code: Some(0),
                cause: "exited".into(),
            },
            true,
            CleanupState::Verified,
            TimingReport::default(),
        )
        .unwrap();
    assert_eq!(
        fixture.json(&["checkpoints", "--json"])["checkpoints"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // The project root, not another directory; and a lane that exists.
    let elsewhere = fixture.run_in(&lane, &["checkpoint", "--lane", &name]);
    assert_ne!(elsewhere.status.code(), Some(0), "{elsewhere:?}");
    let missing = fixture.run_in(
        &fixture.project,
        &["checkpoint", "--lane", &format!("{id}/z")],
    );
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("no lane z"),
        "{missing:?}"
    );

    let made = fixture.json(&["checkpoint", "--lane", &name, "--label", "lane-a", "--json"]);
    assert_eq!(made["lane"], name.as_str());
    assert_eq!(made["label"], "lane-a");
    let checkpoint = made["checkpoint"].as_str().unwrap().to_owned();
    let dir = fixture.project.join(".marsh/checkpoints").join(&checkpoint);
    let tree = dir.join("tree");
    assert_eq!(
        fs::read_to_string(tree.join("src/lib.rs")).unwrap(),
        "fn a() { staged(); }\n"
    );
    assert_eq!(
        fs::read_to_string(tree.join("new.txt")).unwrap(),
        "from the lane\n"
    );
    // The same pruning as a project checkpoint: ignored files and `.git` are left out.
    assert!(!tree.join("target").exists() && !tree.join(".git").exists());
    assert_eq!(
        fs::read_to_string(dir.join("git/HEAD")).unwrap().trim(),
        lane_head.trim()
    );
    assert!(dir.join("git/index").is_file() && dir.join("git/objects").is_dir());
    let listed = fixture.json(&["checkpoints", "--json", "--label", "lane-a"]);
    assert_eq!(listed["checkpoints"][0]["lane"], name.as_str());
    // The lane is untouched and still open.
    assert_eq!(
        fixture.json(&["splits", "--json", &id])["splits"][0]["state"],
        "run"
    );

    // A split from it starts from the lane's state: files, HEAD, and index.
    fixture.run_in(&fixture.project, &["splits", "cancel", &id]);
    fixture.run_in(&fixture.project, &["splits", "rm", &id]);
    let next = host_split(&fixture, &["split", "--open", "--from", &checkpoint, "b"]);
    let next_lane = PathBuf::from(next["lanes"]["b"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(next_lane.join("src/lib.rs")).unwrap(),
        "fn a() { staged(); }\n"
    );
    assert_eq!(
        fs::read_to_string(next_lane.join("new.txt")).unwrap(),
        "from the lane\n"
    );
    assert_eq!(git(&next_lane, &["rev-parse", "HEAD"]), lane_head);
    assert_eq!(
        git(&next_lane, &["log", "-1", "--format=%s"]).trim(),
        "lane work"
    );
    // The index came along: the staged edit is staged, the new file untracked.
    assert_eq!(
        git(&next_lane, &["status", "--porcelain"]),
        "M  src/lib.rs\n?? untracked.txt\n"
    );
    // It does not lean on the checkpoint: the lane's commit survives its removal.
    let removed = fixture.run_in(&fixture.project, &["checkpoints", "rm", &checkpoint]);
    assert_eq!(removed.status.code(), Some(0), "{removed:?}");
    assert_eq!(
        git(&next_lane, &["cat-file", "-t", lane_head.trim()]).trim(),
        "commit"
    );

    // A kept lane can be checkpointed too; a session's own lane by that session.
    let next_id = next["id"].as_str().unwrap().to_owned();
    let closed = fixture.json(&["split", "close", &next_id, "--json"]);
    assert_eq!(closed["state"], "kept");
    let kept = fixture.json(&["checkpoint", "--lane", &format!("{next_id}/b"), "--json"]);
    assert!(kept["checkpoint"].is_string());
    let (own_token, own_context) = other_session(&fixture, "own-token");
    let as_session = |arguments: &[&str]| {
        fixture
            .command(&fixture.project, arguments)
            .env("MARSH_DAEMON_TOKEN", &own_token)
            .env("MARSH_EXTERNAL_SESSION", &own_context)
            .output()
            .unwrap()
    };
    let own = as_session(&["split", "--open", "c"]);
    assert_eq!(own.status.code(), Some(0), "{own:?}");
    let own: serde_json::Value = serde_json::from_slice(&own.stdout).unwrap();
    let own_name = format!("{}/c", own["id"].as_str().unwrap());
    let by_session = as_session(&["checkpoint", "--lane", &own_name]);
    assert_eq!(by_session.status.code(), Some(0), "{by_session:?}");
    // Another session still may not; the host may.
    let refused = fixture
        .command(&fixture.project, &["checkpoint", "--lane", &own_name])
        .env("MARSH_DAEMON_TOKEN", &other)
        .env("MARSH_EXTERNAL_SESSION", &context)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("only its creator"),
        "{refused:?}"
    );
    assert!(fixture.json(&["checkpoint", "--lane", &own_name, "--json"])["checkpoint"].is_string());
}

#[test]
fn cancelling_an_open_split_with_no_lane_job_removes_it() {
    let fixture = Fixture::new();
    let opened = fixture.json(&["split", "--open", "only"]);
    let id = opened["id"].as_str().unwrap();
    let output = fixture.run_in(&fixture.project, &["splits", "cancel", id]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let shown = fixture.json(&["splits", "--json", id]);
        if shown["splits"][0]["state"] == "cancelled" {
            assert!(
                shown["splits"][0]["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("every lane ended")),
                "{shown}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "{shown}");
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!fixture.project.join(".marsh/split").join(id).exists());
}

#[test]
fn events_take_at_most_eight_connections_and_never_hold_off_requests() {
    let fixture = Fixture::new();
    let subscribers = (0..8).map(|_| subscribe(&fixture)).collect::<Vec<_>>();
    for subscriber in &subscribers {
        subscriber.next_where(|line| line["type"] == "hello");
    }
    let ninth = fixture.run_in(&fixture.project, &["events", "--json"]);
    assert_ne!(ninth.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&ninth.stderr).contains("at most 8 subscribers"),
        "{ninth:?}"
    );
    // Ordinary and lifecycle requests still get through.
    fixture.json(&["storage", "--json"]);
    drop(subscribers);
    let deadline = Instant::now() + Duration::from_secs(20);
    // A subscriber that went away frees its place.
    loop {
        let again = subscribe(&fixture);
        let line = again
            .lines
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_default();
        if line["type"] == "hello" {
            break;
        }
        assert!(Instant::now() < deadline, "no place freed");
    }
    fixture.store().detach_shell(&fixture.session).unwrap();
    assert!(matches!(
        fixture.client().request(PublicRequest::Shutdown).unwrap(),
        PublicReply::ShuttingDown
    ));
}

#[test]
fn kept_splits_are_collected_after_retention_but_never_while_joined() {
    let fixture = Fixture::with_retention(Duration::from_secs(2));
    let opened = fixture.json(&["split", "--open", "a"]);
    let id = opened["id"].as_str().unwrap().to_owned();
    let lane = PathBuf::from(opened["lanes"]["a"].as_str().unwrap());
    fs::write(lane.join("new.txt"), "x\n").unwrap();
    fixture.json(&["split", "close", &id, "--json"]);
    // The consumer reads the results across the retention window (2 s); the
    // collector leaves a joined split alone, and a failing consumer keeps
    // it with a fresh retention clock.
    let joined = fixture
        .command(
            &fixture.project,
            &[
                "join",
                "--",
                "sh",
                "-c",
                "sleep 4; cat \"$SPLIT_DIR/a/files\"; exit 3",
            ],
        )
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write as _;
        let mut stdin = joined.stdin.as_ref().unwrap();
        writeln!(stdin, "{opened}").unwrap();
    }
    let output = joined.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "A\tnew.txt\n");
    let shown = fixture.json(&["splits", "--json", &id]);
    assert_eq!(shown["splits"][0]["state"], "kept", "{shown}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while fixture.project.join(".marsh/split").join(&id).exists() {
        assert!(Instant::now() < deadline, "kept split never collected");
        thread::sleep(Duration::from_millis(200));
    }
    let shown = fixture.json(&["splits", "--json", &id]);
    assert_eq!(shown["splits"][0]["removed"], true, "{shown}");
}

#[test]
fn an_open_split_label_is_recorded_listed_and_published() {
    let fixture = Fixture::new();
    let events = subscribe(&fixture);
    events.next_where(|line| line["type"] == "hello");
    let opened = fixture.json(&["split", "--open", "--label", "narwhal-run:r-7", "a"]);
    let id = opened["id"].as_str().unwrap().to_owned();
    let listed = fixture.json(&["splits", "--json"]);
    let row = listed["splits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|split| split["id"] == id.as_str())
        .expect("listed");
    assert_eq!(row["label"], "narwhal-run:r-7");
    let event =
        events.next_where(|line| line["type"] == "split" && line["split"]["id"] == id.as_str());
    assert_eq!(event["split"]["label"], "narwhal-run:r-7");
    let closed = fixture.json(&["split", "close", &id, "--json"]);
    assert_eq!(closed["label"], "narwhal-run:r-7");
    // Removal is confirmed by absence from a successful listing.
    let removed = fixture.run_in(&fixture.project, &["splits", "rm", &id]);
    assert_eq!(removed.status.code(), Some(0), "{removed:?}");
    let listed = fixture.json(&["splits", "--json"]);
    assert!(
        listed["splits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|split| split["label"] != "narwhal-run:r-7"),
        "{listed}"
    );
    for bad in [
        &["split", "--open", "--label", "", "a"][..],
        &["split", "--open", "--label", "bell\u{7}", "a"],
        &["split", "--open", "--label", &"x".repeat(129), "a"],
        &["split", "--label", "x", ":::", "a", "true"],
        &["split", "--open", "a", "--label"],
        &["cloud", "--label", "x", "--", "true"],
        &["cloud", "-d", "--label", "", "--", "true"],
        &["cloud", "-d", "--label", "nl\n", "--", "true"],
    ] {
        let refused = fixture.run_in(&fixture.project, bad);
        assert_eq!(refused.status.code(), Some(2), "{bad:?}: {refused:?}");
    }
    // A label's text is never read as an option: `--json`, `-c`, and `--`
    // are labels here, so only the missing -d is refused.
    for text in ["--json", "-c", "--"] {
        let refused = fixture.run_in(&fixture.project, &["cloud", "--label", text, "--", "true"]);
        assert_eq!(refused.status.code(), Some(2), "{text}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("--label is for a detached run"),
            "{text}: {refused:?}"
        );
    }
}

#[test]
fn a_close_that_cannot_be_journaled_stays_fenced_and_a_retry_completes_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new();
    let opened = fixture.json(&["split", "--open", "a"]);
    let id = opened["id"].as_str().unwrap().to_owned();
    let lane = PathBuf::from(opened["lanes"]["a"].as_str().unwrap());
    assert!(fixture.home.join("splits.json").is_file());
    fs::set_permissions(&fixture.home, fs::Permissions::from_mode(0o500)).unwrap();
    let refused = fixture.run_in(&fixture.project, &["split", "close", &id, "--json"]);
    fs::set_permissions(&fixture.home, fs::Permissions::from_mode(0o700)).unwrap();
    // Never undone: the split admits no more lane jobs.
    let fenced = fixture
        .store()
        .begin_job_at(lane_job(&fixture, &lane), Placement::Local);
    assert_ne!(refused.status.code(), Some(0), "{refused:?}");
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("could not journal the close") && message.contains("again"),
        "{refused:?}"
    );
    assert!(
        fenced.is_err(),
        "a lane job was recorded after the close began"
    );
    let shown = fixture.json(&["splits", "--json", &id]);
    assert_eq!(shown["splits"][0]["closing"], true, "{shown}");
    let closed = fixture.json(&["split", "close", &id, "--json"]);
    assert_eq!(closed["state"], "kept", "{closed}");
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.home.join("splits.json")).unwrap()).unwrap();
    assert_eq!(journal[&id]["state"], "kept", "{journal}");
}

/// The first journal write (`splits.json`) fails before its rename: disk
/// still says open, so a restart preserves the split as open, and a close
/// then completes.
#[test]
fn a_restart_after_a_close_that_never_reached_the_journal_keeps_the_split_open() {
    use std::os::unix::fs::PermissionsExt as _;
    let mut fixture = Fixture::new();
    let opened = fixture.json(&["split", "--open", "a"]);
    let id = opened["id"].as_str().unwrap().to_owned();
    let lane = PathBuf::from(opened["lanes"]["a"].as_str().unwrap());
    fs::write(lane.join("edited.txt"), "from the lane\n").unwrap();
    fs::set_permissions(&fixture.home, fs::Permissions::from_mode(0o500)).unwrap();
    let refused = fixture.run_in(&fixture.project, &["split", "close", &id, "--json"]);
    fs::set_permissions(&fixture.home, fs::Permissions::from_mode(0o700)).unwrap();
    assert_ne!(refused.status.code(), Some(0), "{refused:?}");
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.home.join("splits.json")).unwrap()).unwrap();
    assert_eq!(journal[&id]["state"], "run", "{journal}");
    assert_ne!(journal[&id]["closing"], true, "{journal}");

    fixture.restart();
    let shown = fixture.json(&["splits", "--json", &id]);
    assert_eq!(shown["splits"][0]["state"], "run", "{shown}");
    assert_ne!(shown["splits"][0]["closing"], true, "{shown}");
    assert!(lane.join("edited.txt").is_file());
    let closed = fixture.json(&["split", "close", &id, "--json"]);
    assert_eq!(closed["state"], "kept", "{closed}");
}

/// The second journal write (`split-lineage.json`) fails after `splits.json`
/// carries `closing`: the close reached the journal, so a restart keeps the
/// split and leaves its open lane uncertain, never open.
#[test]
fn a_restart_after_a_close_whose_lineage_write_failed_leaves_the_split_uncertain() {
    let mut fixture = Fixture::new();
    let opened = fixture.json(&["split", "--open", "a"]);
    let id = opened["id"].as_str().unwrap().to_owned();
    let lane = PathBuf::from(opened["lanes"]["a"].as_str().unwrap());
    fs::write(lane.join("edited.txt"), "from the lane\n").unwrap();
    // A directory where the lineage file goes: its rename fails, the
    // `splits.json` write before it succeeds.
    let lineage = fixture.home.join("split-lineage.json");
    fs::remove_file(&lineage).unwrap();
    fs::create_dir(&lineage).unwrap();
    let refused = fixture.run_in(&fixture.project, &["split", "close", &id, "--json"]);
    fs::remove_dir(&lineage).unwrap();
    assert_ne!(refused.status.code(), Some(0), "{refused:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("could not journal the close"),
        "{refused:?}"
    );
    let journal: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.home.join("splits.json")).unwrap()).unwrap();
    assert_eq!(journal[&id]["closing"], true, "{journal}");

    fixture.restart();
    let shown = fixture.json(&["splits", "--json", &id]);
    assert_eq!(shown["splits"][0]["state"], "kept", "{shown}");
    assert_ne!(shown["splits"][0]["closing"], true, "{shown}");
    assert!(
        shown.to_string().contains("uncertain"),
        "the lane must be uncertain: {shown}"
    );
    assert!(lane.join("edited.txt").is_file(), "nothing is deleted");
}

/// A job receipt whose mounts name `lane`, as a lane job's do (no VM).
fn lane_job(fixture: &Fixture, lane: &Path) -> NewJob {
    NewJob {
        label: None,
        session_id: fixture.session.clone(),
        command: "fixture".into(),
        kit_ref: "fixture-only-no-vm".into(),
        workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
        mounts: vec![marsh_daemon::PublicMount {
            target: lane.to_path_buf(),
            access: "read_write".into(),
        }],
    }
}

#[test]
fn a_close_under_way_refuses_a_concurrent_close_or_cancel_and_fences_new_jobs() {
    let fixture = Fixture::new();
    let opened = fixture.json(&["split", "--open", "a"]);
    let id = opened["id"].as_str().unwrap().to_owned();
    let lane = PathBuf::from(opened["lanes"]["a"].as_str().unwrap());
    // A live job in the lane keeps the close waiting (up to 30 s).
    let (live, _) = fixture
        .store()
        .begin_job_at(lane_job(&fixture, &lane), Placement::Local)
        .unwrap();
    let closing = fixture
        .command(&fixture.project, &["split", "close", &id, "--json"])
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fixture.json(&["splits", "--json", &id])["splits"][0]["closing"] != true {
        assert!(Instant::now() < deadline, "close never began");
        thread::sleep(Duration::from_millis(20));
    }
    let again = fixture.run_in(&fixture.project, &["split", "close", &id]);
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already closing"),
        "{again:?}"
    );
    let cancel = fixture.run_in(&fixture.project, &["splits", "cancel", &id]);
    assert!(
        String::from_utf8_lossy(&cancel.stderr).contains("is closing"),
        "{cancel:?}"
    );
    assert!(
        fixture
            .store()
            .begin_job_at(lane_job(&fixture, &lane), Placement::Local)
            .is_err()
    );
    fixture
        .store()
        .finish_job(
            &live,
            ExitStatus {
                code: Some(0),
                cause: "exited".into(),
            },
            true,
            CleanupState::Verified,
            TimingReport::default(),
        )
        .unwrap();
    let output = closing.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let closed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(closed["state"], "kept", "{closed}");
}

#[test]
fn apply_holds_the_split_as_a_join_and_rm_never_takes_a_joined_split() {
    let fixture = Fixture::new();
    let opened = fixture.json(&["split", "--open", "a"]);
    let id = opened["id"].as_str().unwrap().to_owned();
    let lane = PathBuf::from(opened["lanes"]["a"].as_str().unwrap());
    fs::write(lane.join("applied.txt"), "from the lane\n").unwrap();
    fixture.json(&["split", "close", &id, "--json"]);
    let kept = fixture.json(&["splits", "--json", &id])["splits"][0]["kept_unix_ms"].clone();
    thread::sleep(Duration::from_millis(20));
    let applied = fixture.run_in(&fixture.project, &["split", "apply", &id, "a"]);
    assert_eq!(applied.status.code(), Some(0), "{applied:?}");
    assert_eq!(
        fs::read_to_string(fixture.project.join("applied.txt")).unwrap(),
        "from the lane\n"
    );
    // The apply's release keeps the split, with a fresh retention clock.
    let shown = fixture.json(&["splits", "--json", &id])["splits"][0].clone();
    assert_eq!(shown["state"], "kept", "{shown}");
    assert_ne!(shown["kept_unix_ms"], kept, "{shown}");

    // While a join holds it, `splits rm` is refused and deletes nothing.
    let mut joined = fixture
        .command(&fixture.project, &["join", "--", "sh", "-c", "sleep 3"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write as _;
        writeln!(joined.stdin.as_ref().unwrap(), "{opened}").unwrap();
    }
    drop(joined.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    while fixture.json(&["splits", "--json", &id])["splits"][0]["state"] != "joined" {
        assert!(Instant::now() < deadline, "never joined");
        thread::sleep(Duration::from_millis(50));
    }
    let refused = fixture.run_in(&fixture.project, &["splits", "rm", &id]);
    assert_ne!(refused.status.code(), Some(0), "{refused:?}");
    assert!(fixture.project.join(".marsh/split").join(&id).exists());
    joined.wait().unwrap();
}
