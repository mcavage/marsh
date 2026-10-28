//! Checkpoints, open lanes, storage, and events through the public CLI
//! against a real authenticated daemon. No SBX backend or VM is involved: a
//! lane's edits are made directly in its fork, as a lane job would make them,
//! and a lane job's admission is observed at the daemon boundary.

use marsh_daemon::{EndpointPaths, PublicReply, PublicRequest, Server, SessionAuthority};
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
    store: marsh_daemon::DaemonStore,
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
            store,
            _directory: directory,
            home,
            project,
            endpoint,
            session,
            stop,
            server: Some(server),
        }
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
    let excluded = Command::new("tmutil")
        .arg("isexcluded")
        .arg(fixture.project.join(".marsh"))
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&excluded.stdout).contains("[Excluded]"),
        "{excluded:?}"
    );

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
    fixture.store.detach_shell(&fixture.session).unwrap();
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
    let shell = fixture.store.attach_shell(
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
    fs::write(&relay, fixture.store.issue_relay_token(&shell).unwrap()).unwrap();
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
    fs::write(lane_a.join("src/lib.rs"), "fn a() { lane(); }\n").unwrap();
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
    fixture.store.detach_shell(&fixture.session).unwrap();
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
}
