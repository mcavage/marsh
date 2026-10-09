//! Runnable Linux boundary E2E: real daemon Unix sockets, real MCP JSON-RPC
//! over a Unix socket pair and the actual acceptance Node fixture subprocess.
//! Only stock Kit launch/host registration are replaced. No VM claims.
use marsh_daemon::*;
use marsh_mcp::{AcpDeclaration, AcpExportMcp};
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::json;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        fs::{MetadataExt as _, PermissionsExt},
        net::UnixStream,
    },
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

static HOST_INIT: std::sync::Once = std::sync::Once::new();

struct Fixture {
    preparations: Arc<AtomicUsize>,
    preparation_release: Arc<AtomicBool>,
    preparation_delay_ms: Arc<AtomicU64>,
}
impl DaemonBackend for Fixture {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(vec!["fixture".into()])
    }
    fn registered_kits(&self) -> Result<std::collections::BTreeMap<String, String>, DaemonError> {
        Ok([("fixture".into(), "fixture-process".into())].into())
    }
    fn resolve_acp_agent(
        &self,
        name: &str,
    ) -> Result<marsh_acp::AgentAdapterDeclaration, DaemonError> {
        Ok(marsh_acp::AgentAdapterDeclaration {
            schema_version: 1,
            name: name.into(),
            protocol: marsh_acp::AgentProtocol::AcpV1,
            command: "fixture".into(),
            workload_digest: "fixture-process".into(),
            required_capabilities: vec![],
            arguments: vec![],
            description: None,
        })
    }
    fn prepare(
        &self,
        selection: &LoadSelection,
        session: &SessionSpec,
        progress: PreparationProgress,
        _: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        assert_eq!(selection, &LoadSelection::Kits(vec!["fixture".into()]));
        self.preparations.fetch_add(1, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_mins(1);
        while !self.preparation_release.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "test did not release owned preparation"
            );
            thread::sleep(Duration::from_millis(5));
        }
        if session.launch_directory.join(".fail-prepare").exists() {
            return Err(DaemonError::InvalidState(
                "fixture preparation failed".into(),
            ));
        }
        progress.cold_boot("fixture", false)?;
        // One progress frame, then a genuinely quiet backend preparation.
        thread::sleep(Duration::from_millis(
            self.preparation_delay_ms.load(Ordering::SeqCst),
        ));
        Ok(PreparationResult {
            cold_kits: vec!["fixture".into()],
            sandboxes: [("fixture".into(), "exact-fixture-vm".into())].into(),
        })
    }
    fn open_shell(
        &self,
        _: ShellSpec,
        _: ServerAttachment,
        _: DaemonStore,
    ) -> Result<(), DaemonError> {
        unreachable!()
    }
    fn execute(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError> {
        let mut child = Command::new("node")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../tests/acceptance/acp-fixture/agent.mjs"
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let (job, _) = store.begin_job(NewJob {
            session_id: request.session.session_id,
            command: request.command,
            kit_ref: "fixture-process".into(),
            workload_image: "node-fixture".into(),
            mounts: vec![],
        })?;
        attachment.send(&AttachmentFrame::JobStarted {
            job_id: job.clone(),
        })?;
        let writer = attachment.clone();
        let pump = thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                let n = output.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                if writer
                    .send(&AttachmentFrame::Stdout {
                        bytes: buf[..n].to_vec(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        while let Ok(frame) = attachment.receive() {
            let keep_running = match frame {
                AttachmentFrame::Stdin { bytes } => input.write_all(&bytes).is_ok(),
                AttachmentFrame::Signal { .. } | AttachmentFrame::StdinEof => false,
                _ => true,
            };
            if !keep_running {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait()?;
        pump.join().unwrap();
        store.finish_job(
            &job,
            ExitStatus {
                code: Some(0),
                cause: "fixture reaped".into(),
            },
            true,
            CleanupState::Verified,
            TimingReport::default(),
        )?;
        attachment.send(&AttachmentFrame::Exited { code: 0 })?;
        Ok(())
    }
}

struct Rig {
    server: Arc<Server>,
    client: Client,
    relay: Client,
    shell: SessionSpec,
    id: String,
    lease: UnixStream,
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
    root: tempfile::TempDir,
    home: std::path::PathBuf,
    preparations: Arc<AtomicUsize>,
    preparation_release: Arc<AtomicBool>,
    preparation_delay_ms: Arc<AtomicU64>,
}
impl Rig {
    fn new() -> Self {
        Self::with_host(false)
    }

    fn with_host(real: bool) -> Self {
        Self::with_home_policy(real, false)
    }

    fn with_home_policy(real: bool, ephemeral: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let backing = home.join("home");
        let host_home = root.path().join("host");
        let control = root.path().join("control");
        let project = root.path().join("project");
        for p in [&home, &backing, &host_home, &control, &project] {
            fs::create_dir(p).unwrap();
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let home = home.canonicalize().unwrap();
        let backing = backing.canonicalize().unwrap();
        // The daemon requires exact canonical paths (macOS /var -> /private/var).
        let [project, control] = [project, control].map(|path| path.canonicalize().unwrap());
        // Substitute only stock registration. The grant and private host-to-
        // daemon preparation handshake still use their production paths.
        let host = std::env::current_exe().unwrap().with_file_name("marsh");
        HOST_INIT.call_once(|| {
            fs::write(
                &host,
                include_str!("../../../tests/acceptance/acp-fixture/host.py"),
            )
            .unwrap();
            fs::set_permissions(&host, fs::Permissions::from_mode(0o700)).unwrap();
        });
        let stock = root.path().join("stock-sbx");
        fs::write(
            &stock,
            include_str!("../../../tests/acceptance/acp-fixture/stock.py"),
        )
        .unwrap();
        fs::set_permissions(&stock, fs::Permissions::from_mode(0o700)).unwrap();
        if real {
            let binary = std::env::var("MARSH_ACP_CALLER_BIN").expect("exact CLI binary required");
            fs::write(project.join(".host-cli"), binary).unwrap();
        }
        let preparations = Arc::new(AtomicUsize::new(0));
        let preparation_release = Arc::new(AtomicBool::new(true));
        let preparation_delay_ms = Arc::new(AtomicU64::new(0));
        let server = Arc::new(
            Server::bind_with_stock_sbx_and_control_home(&home, &stock, &control)
                .unwrap()
                .with_backend(Arc::new(Fixture {
                    preparations: Arc::clone(&preparations),
                    preparation_release: Arc::clone(&preparation_release),
                    preparation_delay_ms: Arc::clone(&preparation_delay_ms),
                })),
        );
        let owner = fs::metadata(root.path()).unwrap();
        let authority = SessionAuthority {
            username: "fixture".into(),
            uid: owner.uid(),
            gid: owner.gid(),
            launch_directory: project.clone(),
            guest_home: "/fixture".into(),
            home_backing: backing.clone(),
            ephemeral_home: ephemeral,
        };
        let shell_id = server.store().attach_shell(std::process::id(), authority);
        let shell = SessionSpec {
            session_id: shell_id.clone(),
            username: "fixture".into(),
            uid: owner.uid(),
            gid: owner.gid(),
            launch_directory: project,
            guest_home: "/fixture".into(),
            home_backing: backing,
            ephemeral_home: ephemeral,
            terminal: false,
            terminal_size: None,
        };
        let token = server.store().issue_relay_token(&shell_id).unwrap();
        let token_path = root.path().join("relay-token");
        fs::write(&token_path, token).unwrap();
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let a = server.clone();
        let b = stop.clone();
        let task = thread::spawn(move || a.serve_until(|| b.load(Ordering::Acquire)).unwrap());
        let client = Client::connect(&home).unwrap();
        let relay =
            Client::connect_relay(&EndpointPaths::for_home(&home).unwrap().socket, &token_path)
                .unwrap();
        let reservation = client.acp_reserve("fixture".into(), shell.clone()).unwrap();
        let (id, _, lease) = client
            .acp_start_reserved("fixture".into(), shell.clone(), Some(reservation.clone()))
            .unwrap();
        assert_eq!(id, reservation);
        Self {
            server,
            client,
            relay,
            shell,
            id,
            lease,
            stop,
            task: Some(task),
            root,
            home,
            preparations,
            preparation_release,
            preparation_delay_ms,
        }
    }
    fn cli_at(&self, socket: &std::path::Path, args: &[&str]) -> std::process::Output {
        let binary = std::env::var_os("MARSH_ACP_CALLER_BIN").expect("exact CLI binary required");
        let link = self.root.path().join("acp");
        if !link.exists() {
            std::os::unix::fs::symlink(binary, &link).unwrap();
        }
        Command::new(link)
            .args([
                "--invoke-bundled",
                "acp",
                "--marsh-guest",
                "--marsh-session",
                &self.shell.session_id,
            ])
            .args(args)
            .env("MARSH_HOME", &self.home)
            .env("HOME", self.root.path().join("host"))
            .env("USER", &self.shell.username)
            .env(
                "MARSH_EXTERNAL_SESSION",
                serde_json::to_string(&self.shell).unwrap(),
            )
            .env("MARSH_DAEMON_SOCKET", socket)
            .env("MARSH_DAEMON_TOKEN", self.root.path().join("relay-token"))
            .current_dir(&self.shell.launch_directory)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn cli(&self, args: &[&str]) -> std::process::Output {
        self.cli_at(&EndpointPaths::for_home(&self.home).unwrap().socket, args)
    }

    fn registration(&self) -> serde_json::Value {
        fs::read(self.root.path().join("stock-registry.json"))
            .ok()
            .map_or_else(
                || json!({}),
                |bytes| serde_json::from_slice(&bytes).unwrap(),
            )
    }

    fn stop_and_receipt(&self) -> AcpSessionStatus {
        self.client
            .acp_stop(self.id.clone(), self.shell.clone())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = self.status(0);
            if status.attachment.terminal.is_some() {
                assert_eq!(
                    status.receipt.as_ref().unwrap().cleanup,
                    CleanupState::Verified
                );
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "owned Node job did not terminate"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn status(&self, cursor: u64) -> AcpSessionStatus {
        self.client
            .acp_status(self.id.clone(), self.shell.clone(), cursor)
            .unwrap()
    }
    fn prompt(&self, text: &str) -> String {
        self.client
            .acp_prompt_with_key(
                self.id.clone(),
                self.shell.clone(),
                Uuid::new_v4().to_string(),
                text.into(),
            )
            .unwrap()
    }
    fn done(&self) -> AcpSessionStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self.status(0);
            if !s.turn_active {
                return s;
            }
            assert!(Instant::now() < deadline, "turn stuck");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self
            .relay
            .acp_unpublish(self.shell.clone(), "fixture_tool".into());
        let _ = self.client.acp_stop(self.id.clone(), self.shell.clone());
        // macOS reports EINVAL for setsockopt once the peer has closed.
        let _ = self.lease.set_read_timeout(Some(Duration::from_secs(3)));
        let mut b = [0];
        let _ = self.lease.read(&mut b);
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.task.take() {
            t.join().unwrap();
        }
        let _ = (&self.server, &self.root);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(
    clippy::too_many_lines,
    reason = "One stateful real-socket journey preserves turn and publication history between actions"
)]
async fn fixture_caller_all_turns_and_published_actions() {
    let rig = Rig::new();
    // Idle documented cursors never advance.
    let mut cursor = 0;
    for _ in 0..5 {
        cursor = rig.status(cursor).next_cursor;
    }
    assert_eq!(cursor, 0);
    rig.prompt("lossy");
    let lossy = rig.done();
    assert!(lossy.updates_lost);
    assert_eq!(lossy.dropped_updates, 1);
    let before = lossy.latest_cursor;
    let clean = rig.prompt("clean");
    rig.done();
    let s = rig.status(before);
    assert!(!s.updates_lost);
    assert_eq!(s.updates[0].turn_id.as_deref(), Some(clean.as_str()));
    assert_eq!(s.updates[0].update["content"]["text"], "fixture:clean");
    let before = s.latest_cursor;
    rig.prompt("burst-400");
    rig.done();
    let mut chunks = Vec::new();
    cursor = before;
    loop {
        let s = rig.status(cursor);
        assert!(!s.updates_lost);
        chunks.extend(
            s.updates
                .iter()
                .map(|u| u.update["content"]["text"].as_str().unwrap().to_owned()),
        );
        cursor = s.next_cursor;
        if !s.more_updates {
            break;
        }
    }
    assert_eq!(
        chunks,
        std::iter::once("fixture:burst-400".to_owned())
            .chain((0..400).map(|n| format!("b{n};")))
            .collect::<Vec<_>>()
    );
    let before = cursor;
    rig.prompt("diff");
    rig.done();
    let diff = rig.status(before);
    assert!(!diff.updates_lost);
    assert_eq!(diff.updates[1].update["content"][0]["newText"], "new");
    assert_eq!(diff.updates[1].update["locations"][0]["line"], 3);
    assert_eq!(diff.updates[1].update["_meta"]["fixture"], "metadata");
    assert_eq!(
        diff.updates[2].update["availableCommands"][0]["name"],
        "review"
    );
    rig.prompt("always-only");
    let s = rig.done();
    assert!(s.permission_note.unwrap().contains("allow_once"));
    assert!(s.permissions.is_empty());
    rig.prompt("agent-error");
    let s = rig.done();
    let error = s.last_error.unwrap();
    assert!(error.len() < 256 && !error.contains("SECRET") && error.contains("configuration"));
    let before = s.latest_cursor;
    rig.prompt("oversize-update");
    rig.done();
    assert!(rig.status(before).updates_lost);
    // Publication authority and real MCP caller, after prior host history.
    rig.relay
        .acp_publish(
            rig.id.clone(),
            rig.shell.clone(),
            "fixture_tool".into(),
            None,
        )
        .unwrap();
    for action in ["prompt", "cancel", "stop"] {
        let error = match action {
            "prompt" => rig
                .client
                .acp_prompt(rig.id.clone(), rig.shell.clone(), "forbidden".into())
                .unwrap_err(),
            "cancel" => rig
                .client
                .acp_cancel(rig.id.clone(), rig.shell.clone())
                .unwrap_err(),
            _ => rig
                .client
                .acp_stop(rig.id.clone(), rig.shell.clone())
                .unwrap_err(),
        };
        assert!(error.to_string().contains("unpublish"));
    }
    let path = rig.shell.launch_directory.join("publication.json");
    let declaration = AcpDeclaration::load_private(&path).unwrap();
    let exporter = AcpExportMcp::new(rig.home.clone(), path, declaration).unwrap();
    let (a, b) = tokio::net::UnixStream::pair().unwrap();
    let serving = tokio::spawn(async move { exporter.serve(a).await.unwrap().waiting().await });
    let peer = ().serve(b).await.unwrap();
    assert_eq!(peer.list_all_tools().await.unwrap().len(), 1);
    macro_rules! call {
        ($args:expr) => {{
            let reply = peer
                .call_tool(
                    CallToolRequestParams::new("fixture_tool")
                        .with_arguments($args.as_object().unwrap().clone()),
                )
                .await
                .unwrap();
            assert_ne!(reply.is_error, Some(true), "{reply:?}");
            reply.structured_content.unwrap()["result"].clone()
        }};
    }
    let key = Uuid::new_v4().to_string();
    let ask = call!(json!({"action":"ask","key":key,"text":"slow-6"}));
    assert!(ask.get("status").is_none());
    let turn = ask["turn_id"].as_str().unwrap();
    let mut cursor = ask["start_cursor"].as_u64().unwrap();
    let mut texts = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let page = call!(json!({"action":"status","turn_id":turn,"cursor":cursor}));
        assert!(!page["updates_lost"].as_bool().unwrap());
        let next = page["next_cursor"].as_u64().unwrap();
        if page["updates"].as_array().unwrap().is_empty() {
            assert_eq!(cursor, next);
        }
        for u in page["updates"].as_array().unwrap() {
            assert_eq!(u["turn_id"], turn);
            texts.push(u["update"]["content"]["text"].as_str().unwrap().to_owned());
        }
        cursor = next;
        if page["turn_active"] == false && page["more_updates"] == false {
            break;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        texts,
        std::iter::once("fixture:slow-6".to_owned())
            .chain((0..6).map(|n| format!("u{n};")))
            .collect::<Vec<_>>()
    );
    let retry = call!(json!({"action":"ask","key":key,"text":"slow-6"}));
    assert_eq!(retry, ask);
    let hold = call!(json!({"action":"ask","key":Uuid::new_v4().to_string(),"text":"hold"}));
    // Idle polling is a property of a turn that cannot progress, so it is
    // observed on a held turn: after its echo the agent writes nothing until
    // cancelled, and every further page is empty with an unmoved cursor. (A
    // paced turn only shows empty pages if the poller outruns the pacing, which
    // a stalled machine does not.)
    let mut held_cursor = hold["start_cursor"].as_u64().unwrap();
    let mut idle = 0;
    let held_deadline = Instant::now() + Duration::from_secs(10);
    while idle < 4 {
        let page = call!(json!({"action":"status","turn_id":hold["turn_id"],"cursor":held_cursor}));
        assert_eq!(page["turn_active"], true, "{page}");
        let next = page["next_cursor"].as_u64().unwrap();
        if page["updates"].as_array().unwrap().is_empty() {
            assert_eq!(held_cursor, next);
            idle += 1;
        }
        held_cursor = next;
        assert!(Instant::now() < held_deadline, "held turn never went idle");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let _ = call!(json!({"action":"cancel"}));
    rig.done();
    let cancelled = call!(json!({"action":"status","turn_id":hold["turn_id"]}));
    assert_eq!(cancelled["last_stop_reason"], "cancelled");
    let _ = call!(json!({"action":"ask","key":Uuid::new_v4().to_string(),"text":"permission"}));
    let offered = loop {
        let s = call!(json!({"action":"status"}));
        if !s["permissions"].as_array().unwrap().is_empty() {
            break s;
        }
        assert!(Instant::now() < deadline + Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let _ = call!(
        json!({"action":"respond","request_id":offered["permissions"][0]["request_id"],"option_id":"once"})
    );
    rig.done();
    let permitted = call!(json!({"action":"status"}));
    assert_eq!(permitted["last_stop_reason"], "end_turn");
    assert!(
        permitted["updates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["update"]["content"]["text"] == "fixture:permission:allowed")
    );
    // Old turn remains separately readable after subsequent turns.
    let old = call!(json!({"action":"status","turn_id":turn,"cursor":ask["start_cursor"]}));
    assert_eq!(old["last_stop_reason"], "end_turn");
    assert!(
        old["updates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|u| u["turn_id"] == turn)
    );
    let _ =
        call!(json!({"action":"ask","key":Uuid::new_v4().to_string(),"text":"oversize-update"}));
    rig.done();
    let old_after_gap =
        call!(json!({"action":"status","turn_id":turn,"cursor":ask["start_cursor"]}));
    assert_eq!(
        old_after_gap["updates_lost"], false,
        "later omission poisoned a fully retained earlier turn"
    );
    let _ = call!(json!({"action":"ask","key":Uuid::new_v4().to_string(),"text":"always-only"}));
    rig.done();
    let old_note = call!(json!({"action":"status","turn_id":turn,"cursor":ask["start_cursor"]}));
    assert!(
        old_note["permission_note"].is_null(),
        "another turn's permission note leaked into selected turn"
    );
    rig.relay
        .acp_unpublish(rig.shell.clone(), "fixture_tool".into())
        .unwrap();
    let denied = peer
        .call_tool(
            CallToolRequestParams::new("fixture_tool")
                .with_arguments(json!({"action":"status"}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    assert_eq!(denied.is_error, Some(true));
    peer.cancel().await.unwrap();
    serving.await.unwrap().unwrap();
    rig.prompt("recovered");
    assert_eq!(
        rig.done().last_stop_reason,
        Some(marsh_acp::StopReason::EndTurn)
    );
    rig.client
        .acp_release(rig.id.clone(), rig.shell.clone())
        .unwrap();
    rig.client
        .acp_attach(rig.id.clone(), rig.shell.clone())
        .unwrap();
    tokio::task::spawn_blocking(move || drop(rig))
        .await
        .unwrap();
    eprintln!(
        "fixture-caller: actual Node process + daemon Unix socket + MCP Unix socket: paging, burst, raw JSON, loss/reset, permission, cancel, publication/revoke, recovery passed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_mcp_process_drives_real_exporter_and_node_agent() {
    let rig = Rig::new();
    rig.relay
        .acp_publish(
            rig.id.clone(),
            rig.shell.clone(),
            "fixture_tool".into(),
            None,
        )
        .unwrap();
    let path = rig.shell.launch_directory.join("publication.json");
    let declaration = AcpDeclaration::load_private(&path).unwrap();
    let exporter = AcpExportMcp::new(rig.home.clone(), path, declaration).unwrap();
    let socket = rig.root.path().join("mcp.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let serving = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        exporter.serve(stream).await.unwrap().waiting().await
    });
    let output = tokio::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/acceptance/acp-fixture/mcp_caller.py"
        ))
        .arg(socket)
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "external MCP caller: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["external_mcp_caller"], "passed");
    assert_eq!(result["cancel_tail_chunks"], 201);
    eprintln!("external MCP caller: {result}");
    serving.await.unwrap().unwrap();
    tokio::task::spawn_blocking(move || drop(rig))
        .await
        .unwrap();
}

#[test]
fn publication_rejection_has_typed_preadmission_outcome() {
    let rig = Rig::new();
    // A host publication runs the host CLI: under a loaded host it can take
    // longer than `request`'s 5 s reply budget.
    let reply = rig
        .relay
        .request_with_timeout(
            PublicRequest::AcpPublish {
                agent_session_id: rig.id.clone(),
                session: rig.shell.clone(),
                name: "fixture_tool".into(),
                sandbox: None,
                kit: Some("--invalid".into()),
            },
            Duration::from_mins(2),
        )
        .unwrap();
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 0);
    assert!(rig.status(0).published_name.is_none());
    let wire = serde_json::to_value(reply).unwrap();
    assert_eq!(
        wire["type"], "acp_publication",
        "publication rejection lost its typed public outcome: {wire}"
    );
    assert_eq!(
        wire["outcome"]["status"], "rejected_before_effect",
        "{wire}"
    );
}

#[test]
fn ephemeral_publication_cleanup_is_rejected_before_host_effects() {
    let rig = Rig::with_home_policy(false, true);
    let result = rig
        .relay
        .acp_unpublish(rig.shell.clone(), "fixture_tool".into());
    assert!(
        matches!(
            &result,
            Err(DaemonError::Publication(
                PublicationOutcome::RejectedBeforeEffect { .. }
            ))
        ),
        "{result:?}"
    );
    assert!(result.unwrap_err().to_string().contains("persistent home"));
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 0);
    assert!(
        !rig.shell
            .launch_directory
            .join(".host-events.jsonl")
            .exists()
    );
    assert!(!rig.root.path().join("stock-calls.jsonl").exists());
    rig.stop_and_receipt();
}

#[test]
fn publication_after_grant_failure_is_not_preadmission_rejection() {
    let rig = Rig::new();
    fs::write(rig.shell.launch_directory.join(".host-name-collision"), b"").unwrap();
    // A host publication runs the host CLI: under a loaded host it can take
    // longer than `request`'s 5 s reply budget.
    let reply = rig
        .relay
        .request_with_timeout(
            PublicRequest::AcpPublish {
                agent_session_id: rig.id.clone(),
                session: rig.shell.clone(),
                name: "fixture_tool".into(),
                sandbox: None,
                kit: Some("fixture".into()),
            },
            Duration::from_mins(2),
        )
        .unwrap();
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 0);
    let status = rig.status(0);
    assert!(status.published_name.is_none());
    assert!(status.attachment.terminal.is_none());
    let wire = serde_json::to_value(reply).unwrap();
    assert_eq!(
        wire["type"], "acp_publication",
        "post-grant failure collapsed to argument error: {wire}"
    );
    assert_eq!(
        wire["outcome"]["status"], "uncertain",
        "post-grant failure claimed no effect: {wire}"
    );
    assert!(
        wire["outcome"]["message"]
            .as_str()
            .unwrap()
            .contains("grant was revoked")
    );
}

#[test]
fn typed_host_lost_malformed_and_false_rejection_are_uncertain() {
    let rig = Rig::new();
    for marker in [
        ".host-lost-result",
        ".host-malformed-result",
        ".host-partial-result",
        ".host-false-rejection",
    ] {
        fs::write(rig.shell.launch_directory.join(marker), b"").unwrap();
        let result = rig.relay.acp_publish(
            rig.id.clone(),
            rig.shell.clone(),
            "fixture_tool".into(),
            None,
        );
        assert!(
            matches!(
                &result,
                Err(DaemonError::Publication(
                    PublicationOutcome::Uncertain { .. }
                ))
            ),
            "{marker}: {result:?}"
        );
        assert!(
            rig.status(0).published_name.is_none(),
            "failed publication retained a grant"
        );
        assert!(
            rig.shell.launch_directory.join("publication.json").exists(),
            "host fault did not reach the mutation boundary"
        );
        fs::remove_file(rig.shell.launch_directory.join(marker)).unwrap();
        rig.relay
            .acp_unpublish(rig.shell.clone(), "fixture_tool".into())
            .unwrap();
        assert!(!rig.shell.launch_directory.join("publication.json").exists());
    }
    rig.relay
        .acp_publish(
            rig.id.clone(),
            rig.shell.clone(),
            "fixture_tool".into(),
            None,
        )
        .unwrap();
    fs::write(
        rig.shell.launch_directory.join(".host-reject-unpublish"),
        b"",
    )
    .unwrap();
    let first = rig
        .relay
        .acp_unpublish(rig.shell.clone(), "fixture_tool".into());
    assert!(
        matches!(
            first,
            Err(DaemonError::Publication(
                PublicationOutcome::Uncertain { .. }
            ))
        ),
        "revocation is an effect, even when host preflight rejects"
    );
    assert!(rig.status(0).published_name.is_none());
    let second = rig
        .relay
        .acp_unpublish(rig.shell.clone(), "fixture_tool".into());
    assert!(
        matches!(
            second,
            Err(DaemonError::Publication(
                PublicationOutcome::RejectedBeforeEffect { .. }
            ))
        ),
        "no grant and rejected host preflight should remain typed"
    );
    fs::remove_file(rig.shell.launch_directory.join(".host-reject-unpublish")).unwrap();
    rig.relay
        .acp_unpublish(rig.shell.clone(), "fixture_tool".into())
        .unwrap();
    rig.prompt("after-typed-failures");
    assert_eq!(
        rig.done().last_stop_reason,
        Some(marsh_acp::StopReason::EndTurn)
    );
    rig.stop_and_receipt();
}

#[test]
fn cancellation_tail_is_lossless_and_wire_terminal_fences_late_updates() {
    let rig = Rig::new();
    let turn = rig.prompt("cancel-burst");
    let deadline = Instant::now() + Duration::from_secs(5);
    while rig.status(0).updates.is_empty() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    rig.client
        .acp_cancel(rig.id.clone(), rig.shell.clone())
        .unwrap();
    let done = rig.done();
    assert_eq!(done.turns[&turn].dropped_updates, 0);
    let mut cursor = 0;
    let mut texts = Vec::new();
    loop {
        let page = rig.status(cursor);
        assert!(!page.updates_lost);
        texts.extend(
            page.updates
                .iter()
                .map(|u| u.update["content"]["text"].as_str().unwrap().to_owned()),
        );
        cursor = page.next_cursor;
        if !page.more_updates {
            break;
        }
    }
    assert_eq!(
        texts,
        std::iter::once("fixture:cancel-burst".into())
            .chain((0..200).map(|n| format!("c{n};")))
            .collect::<Vec<String>>()
    );
    let before = cursor;
    let late = rig.prompt("late");
    rig.done();
    while rig.status(before).out_of_turn_updates == 0 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let page = rig.status(before);
    assert_eq!(page.updates.len(), 1);
    assert_eq!(page.updates[0].update["content"]["text"], "fixture:late");
    assert_eq!(page.turns[&late].dropped_updates, 0);
    assert_eq!(page.out_of_turn_updates, 1);
}

#[test]
fn concurrent_publications_have_one_name_owner_and_recover() {
    let rig = Rig::new();
    let (second, _, mut lease) = rig
        .client
        .acp_start("fixture".into(), rig.shell.clone())
        .unwrap();
    let barrier = std::sync::Barrier::new(2);
    let results = thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            rig.relay.acp_publish_target(
                rig.id.clone(),
                rig.shell.clone(),
                "fixture_tool".into(),
                None,
                Some("fixture".into()),
            )
        });
        let next = scope.spawn(|| {
            barrier.wait();
            rig.relay.acp_publish_target(
                second.clone(),
                rig.shell.clone(),
                "fixture_tool".into(),
                None,
                Some("fixture".into()),
            )
        });
        [first.join().unwrap(), next.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        rig.preparations.load(Ordering::SeqCst),
        1,
        "losing publication prepared a Kit"
    );
    let sessions = rig.client.acp_list(rig.shell.clone()).unwrap();
    assert_eq!(
        sessions
            .iter()
            .filter(|s| s.published_name.as_deref() == Some("fixture_tool"))
            .count(),
        1
    );
    rig.relay
        .acp_unpublish(rig.shell.clone(), "fixture_tool".into())
        .unwrap();
    rig.relay
        .acp_publish(
            second.clone(),
            rig.shell.clone(),
            "fixture_tool".into(),
            None,
        )
        .unwrap();
    rig.relay
        .acp_unpublish(rig.shell.clone(), "fixture_tool".into())
        .unwrap();
    rig.client
        .acp_stop(second.clone(), rig.shell.clone())
        .unwrap();
    lease
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut terminal = [0];
    lease.read_exact(&mut terminal).unwrap();
    assert_eq!(terminal, *b"T");
    assert!(
        rig.relay
            .acp_publish_target(
                second,
                rig.shell.clone(),
                "fixture_tool".into(),
                None,
                Some("fixture".into())
            )
            .is_err()
    );
    assert_eq!(
        rig.preparations.load(Ordering::SeqCst),
        1,
        "ended job publication prepared a Kit"
    );
}

#[test]
fn failed_publication_revokes_its_grant_even_after_project_replacement() {
    let rig = Rig::new();
    rig.preparation_release.store(false, Ordering::SeqCst);
    let result = thread::scope(|scope| {
        let publish = scope.spawn(|| {
            rig.relay.acp_publish_target(
                rig.id.clone(),
                rig.shell.clone(),
                "fixture_tool".into(),
                None,
                Some("fixture".into()),
            )
        });
        // Generous: the host publication runs a host CLI before preparing.
        let deadline = Instant::now() + Duration::from_mins(1);
        while rig.preparations.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "publication never reached preparation"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            rig.status(0).published_name.as_deref(),
            Some("fixture_tool")
        );
        let parked = rig.root.path().join("original-project");
        fs::rename(&rig.shell.launch_directory, &parked).unwrap();
        fs::create_dir(&rig.shell.launch_directory).unwrap();
        rig.preparation_release.store(true, Ordering::SeqCst);
        let result = publish.join().unwrap();
        // Restore before querying the original authority, without ever touching
        // a user directory or a stock mount.
        fs::remove_dir(&rig.shell.launch_directory).unwrap();
        fs::rename(parked, &rig.shell.launch_directory).unwrap();
        result
    });
    assert!(
        matches!(
            &result,
            Err(DaemonError::Publication(
                PublicationOutcome::Uncertain { .. }
            ))
        ),
        "project replacement failure lost its post-grant class: {result:?}"
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("grant was revoked")
    );
    let status = rig.status(0);
    assert!(
        status.published_name.is_none(),
        "failed publish left its admitted grant live"
    );
    assert_eq!(
        status.controller_shell_session_id.as_deref(),
        Some(rig.shell.session_id.as_str())
    );
    rig.prompt("after-rollback");
    assert_eq!(
        rig.done().last_stop_reason,
        Some(marsh_acp::StopReason::EndTurn)
    );
}

#[test]
fn status_remains_available_during_blocked_cancel_dispatch() {
    let rig = Rig::new();
    rig.prompt("control-flood");
    // The agent echoes the prompt and then stops reading its stdin (its bounded
    // hostile lifetime starts here). Wait for that echo, not a fixed delay.
    let deadline = Instant::now() + Duration::from_secs(30);
    while rig.status(0).updates.is_empty() {
        assert!(Instant::now() < deadline, "agent never received the prompt");
        thread::sleep(Duration::from_millis(5));
    }
    thread::scope(|scope| {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let rig_ref = &rig;
        scope.spawn(move || {
            let result = rig_ref
                .client
                .acp_cancel(rig_ref.id.clone(), rig_ref.shell.clone());
            done_tx.send(result).unwrap();
        });
        // The cancel has reached dispatch once status reports it requested; it
        // then blocks on the agent's unread stdin.
        while !rig.status(0).cancel_requested {
            assert!(Instant::now() < deadline, "cancel never reached dispatch");
            thread::sleep(Duration::from_millis(1));
        }
        let early = done_rx.try_recv();
        assert!(
            matches!(early, Err(std::sync::mpsc::TryRecvError::Empty)),
            "probe did not reach blocked cancellation: {early:?}"
        );
        let started = Instant::now();
        let status = rig.status(0);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "status blocked behind cancellation transport IO"
        );
        assert!(status.cancel_requested);
        let _ = done_rx
            .recv_timeout(Duration::from_secs(12))
            .expect("cancel never resolved after bounded peer exit");
    });
}

/// Relay real authenticated frames, but either close the listener before the
/// prompt connection or discard the real daemon's accepted reply. No stub reply.
fn prompt_reply_proxy(
    real: &std::path::Path,
    path: &std::path::Path,
    before: bool,
) -> thread::JoinHandle<()> {
    let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    let real = real.to_owned();
    thread::spawn(move || {
        fn frame(stream: &mut UnixStream) -> Vec<u8> {
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut length = [0; 4];
            stream.read_exact(&mut length).unwrap();
            let n = usize::try_from(u32::from_be_bytes(length)).unwrap();
            assert!(n <= 1_048_576);
            let mut body = vec![0; n];
            stream.read_exact(&mut body).unwrap();
            [length.as_slice(), body.as_slice()].concat()
        }
        // The supported bundled route installs descendant Kit links and then
        // discovers shell controls; neither lookup admits an ACP prompt.
        for lookup in 0..2 {
            let (mut caller, _) = listener.accept().unwrap();
            let request = frame(&mut caller);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request[4..]).unwrap()["body"]["type"],
                "registered_commands"
            );
            let mut daemon = UnixStream::connect(&real).unwrap();
            daemon.write_all(&request).unwrap();
            let reply = frame(&mut daemon);
            if before && lookup == 1 {
                drop(listener);
                caller.write_all(&reply).unwrap();
                return;
            }
            caller.write_all(&reply).unwrap();
        }
        let (mut caller, _) = listener.accept().unwrap();
        let request = frame(&mut caller);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request[4..]).unwrap()["body"]["type"],
            "acp_prompt"
        );
        let mut daemon = UnixStream::connect(&real).unwrap();
        daemon.write_all(&request).unwrap();
        let reply = frame(&mut daemon);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply[4..]).unwrap()["type"],
            "acp_prompt_accepted"
        );
        // Drop the socket after actual acceptance, before acknowledging to CLI.
    })
}

#[test]
#[ignore = "requires exact freshly built marsh and sibling marsh-mcp"]
#[allow(clippy::too_many_lines)] // One real CLI/private-host/daemon/Node transaction and recovery journey.
fn cli_typed_publication_drives_real_host_and_preserves_effect_classes() {
    let rig = Rig::with_host(true);
    let name = "review.v1";
    let published_name = PublishedName::parse(name).unwrap();
    let scope = PublicationScope::new(
        PublicationKind::Acp,
        &rig.shell.launch_directory,
        &rig.shell.home_backing,
    );
    let server_name = scope.server_name(&published_name);
    // The daemon and re-exec host must use the same retained control root.
    // The caller runner supplies an isolated HOME for this process.
    let host_home = std::path::PathBuf::from(std::env::var_os("HOME").expect("isolated host HOME"));
    let declaration_path = scope.declaration_path(&host_home, &published_name);
    let initial = rig.status(0);
    let job = initial.receipt.as_ref().unwrap().job_id.clone();

    let invalid = rig.cli(&[
        "publish",
        &rig.id,
        "--name",
        name,
        "--sandbox",
        "bad target",
    ]);
    assert_eq!(invalid.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("rejected before effect"));
    assert!(
        !rig.root.path().join("stock-calls.jsonl").exists(),
        "invalid target reached host inspection"
    );
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 0);
    assert!(rig.status(0).published_name.is_none());

    let published = rig.cli(&["publish", &rig.id, "--name", name, "--kit", "fixture"]);
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    assert!(String::from_utf8_lossy(&published.stderr).contains("starting fixture worker VM"));
    assert!(String::from_utf8_lossy(&published.stdout).contains(&server_name));
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 1);
    let declaration = AcpDeclaration::load_private(&declaration_path).unwrap();
    assert_eq!(
        declaration.tool_name, name,
        "shared PublishedName rejected a dotted tool"
    );
    assert_eq!(rig.registration()[&server_name]["name"], server_name);
    let loads = fs::read_to_string(rig.root.path().join("stock-loads.jsonl")).unwrap();
    let loaded: serde_json::Value = serde_json::from_str(loads.lines().last().unwrap()).unwrap();
    assert_eq!(loaded["sandbox"], "exact-fixture-vm");
    let turn = rig
        .client
        .acp_published_prompt(
            rig.id.clone(),
            declaration.generation.clone(),
            Uuid::new_v4().to_string(),
            "typed-host-turn".into(),
        )
        .unwrap();
    let done = rig.done();
    assert_eq!(
        done.turns[&turn].stop_reason,
        Some(marsh_acp::StopReason::EndTurn)
    );
    assert_eq!(done.receipt.as_ref().unwrap().job_id, job);
    assert!(
        !rig.cli(&["cancel", &rig.id]).status.success(),
        "parent steered published session"
    );
    assert!(rig.cli(&["unpublish", name]).status.success());
    assert!(rig.registration().get(&server_name).is_none());
    assert!(!declaration_path.exists());
    assert!(
        rig.client
            .acp_published_status(rig.id.clone(), declaration.generation, 0)
            .is_err()
    );

    // Host preflight rejects a foreign registration after the daemon grant;
    // no target preparation, but the whole ACP operation is post-effect.
    fs::write(rig.root.path().join("stock-registry.json"), json!({server_name.clone(): {"name":server_name,"type":"local","resolved_command":"/foreign","command":["/foreign"]}}).to_string()).unwrap();
    let rejected = rig.cli(&["publish", &rig.id, "--name", name, "--kit", "fixture"]);
    assert_eq!(rejected.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("outcome uncertain"));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("grant was revoked"));
    assert_eq!(
        rig.preparations.load(Ordering::SeqCst),
        1,
        "host rejection prepared target"
    );
    assert!(rig.status(0).published_name.is_none());
    assert_eq!(
        rig.registration()[&server_name]["resolved_command"],
        "/foreign"
    );
    fs::write(rig.root.path().join("stock-registry.json"), b"{}").unwrap();

    fs::write(rig.root.path().join("fail-load"), b"").unwrap();
    let failed = rig.cli(&["publish", &rig.id, "--name", name, "--kit", "fixture"]);
    assert_eq!(failed.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("outcome uncertain"));
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 2);
    assert!(
        rig.registration().get(&server_name).is_none(),
        "registration rollback failed"
    );
    assert!(!declaration_path.exists());
    assert!(rig.status(0).published_name.is_none());
    fs::remove_file(rig.root.path().join("fail-load")).unwrap();
    rig.prompt("after-real-host-rollback");
    assert_eq!(
        rig.done().last_stop_reason,
        Some(marsh_acp::StopReason::EndTurn)
    );
    let terminal = rig.stop_and_receipt();
    assert_eq!(terminal.receipt.as_ref().unwrap().job_id, job);
}

#[test]
#[ignore = "requires exact freshly built marsh and sibling marsh-mcp"]
fn cli_host_direct_cannot_forge_daemon_dispatch_with_environment() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let home = root.path().join("home");
    for path in [&project, &home] {
        fs::create_dir(path).unwrap();
    }
    let project = project.canonicalize().unwrap();
    let identity = fs::metadata(&project).unwrap();
    for operation in ["host-publish", "host-unpublish"] {
        for forged_channel in [false, true] {
            let mut command = Command::new(std::env::var_os("MARSH_ACP_CALLER_BIN").unwrap());
            command.args(["acp", operation, "direct.tool"]);
            if operation == "host-publish" {
                command.args([Uuid::new_v4().to_string(), Uuid::new_v4().to_string()]);
            }
            command.env_clear();
            if forged_channel {
                command.env("MARSH_PUBLICATION_CHANNEL", "1");
            }
            let output = command
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &home)
                .env("USER", "fixture")
                .env("MARSH_HOME", root.path().join("selected"))
                .env("MARSH_MCP_EXPECTED_PROJECT_PATH", &project)
                .env("MARSH_MCP_EXPECTED_PROJECT_DEV", identity.dev().to_string())
                .env("MARSH_MCP_EXPECTED_PROJECT_INO", identity.ino().to_string())
                .current_dir(&project)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                !output.status.success(),
                "environment forged a daemon transaction"
            );
            assert_eq!(
                fs::read_dir(&home).unwrap().count(),
                0,
                "direct host command created control files"
            );
            assert!(!root.path().join("selected").exists());
            assert_eq!(fs::read_dir(&project).unwrap().count(), 0);
        }
    }
}

#[test]
#[ignore = "requires exact freshly built CLI pair; discards an actual publication reply"]
fn cli_disconnect_after_preparation_reports_uncertainty_not_cancellation() {
    let rig = Rig::with_host(true);
    rig.preparation_delay_ms.store(2_000, Ordering::SeqCst);
    let proxy_path = rig.root.path().join("publication-proxy.sock");
    let listener = std::os::unix::net::UnixListener::bind(&proxy_path).unwrap();
    fs::set_permissions(&proxy_path, fs::Permissions::from_mode(0o600)).unwrap();
    let real = EndpointPaths::for_home(&rig.home).unwrap().socket;
    let proxy = thread::spawn(move || {
        fn frame(stream: &mut UnixStream) -> Vec<u8> {
            stream
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            let mut prefix = [0; 4];
            stream.read_exact(&mut prefix).unwrap();
            let length = usize::try_from(u32::from_be_bytes(prefix)).unwrap();
            assert!(length <= 1_048_576);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            [prefix.as_slice(), body.as_slice()].concat()
        }
        for _ in 0..2 {
            let (mut cli, _) = listener.accept().unwrap();
            let request = frame(&mut cli);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&request[4..]).unwrap()["body"]["type"],
                "registered_commands"
            );
            let mut daemon = UnixStream::connect(&real).unwrap();
            daemon.write_all(&request).unwrap();
            cli.write_all(&frame(&mut daemon)).unwrap();
        }
        let (mut cli, _) = listener.accept().unwrap();
        let request = frame(&mut cli);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&request[4..]).unwrap()["body"]["type"],
            "acp_publish"
        );
        let mut daemon = UnixStream::connect(&real).unwrap();
        daemon.write_all(&request).unwrap();
        let progress = frame(&mut daemon);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&progress[4..]).unwrap()["type"],
            "cold_boot",
            "probe did not reach preparation"
        );
        cli.shutdown(std::net::Shutdown::Both).unwrap();
        drop(cli); // The real CLI loses delivery after a real backend effect.
        let complete = frame(&mut daemon);
        serde_json::from_slice::<serde_json::Value>(&complete[4..]).unwrap()
    });
    let result = rig.cli_at(
        &proxy_path,
        &[
            "publish",
            &rig.id,
            "--name",
            "fixture_tool",
            "--kit",
            "fixture",
        ],
    );
    assert_eq!(result.status.code(), Some(125));
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("outcome uncertain") && error.contains("not cancelled"),
        "{error}"
    );
    let completed = proxy.join().unwrap();
    assert_eq!(completed["type"], "acp_publication");
    assert_eq!(completed["outcome"]["status"], "committed", "{completed}");
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.status(0).published_name.as_deref(),
        Some("fixture_tool")
    );
    assert_eq!(
        rig.registration().as_object().unwrap().len(),
        1,
        "lost delivery was mistaken for remote cancellation"
    );
    assert!(rig.cli(&["unpublish", "fixture_tool"]).status.success());
    assert!(rig.registration().as_object().unwrap().is_empty());
    assert!(rig.status(0).published_name.is_none());
    rig.stop_and_receipt();
}

#[test]
#[ignore = "observes the real five-minute host budget; no fake clock or timeout override"]
fn publication_host_deadline_retains_post_grant_uncertainty() {
    let rig = Rig::new();
    let marker = rig.shell.launch_directory.join(".host-timeout");
    fs::write(&marker, b"").unwrap();
    let start = Instant::now();
    let result = rig.relay.acp_publish(
        rig.id.clone(),
        rig.shell.clone(),
        "fixture_tool".into(),
        None,
    );
    let elapsed = start.elapsed();
    fs::remove_file(marker).unwrap(); // Cleanup/recovery must not stall another host.
    assert!(
        matches!(
            &result,
            Err(DaemonError::Publication(
                PublicationOutcome::Uncertain { .. }
            ))
        ),
        "{result:?}"
    );
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("five minutes") && error.contains("grant was revoked"),
        "{error}"
    );
    assert!(
        elapsed >= Duration::from_mins(5) && elapsed < Duration::from_secs(330),
        "host budget was not exercised: {elapsed:?}"
    );
    assert!(rig.status(0).published_name.is_none());
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 0);
    assert!(!rig.shell.launch_directory.join("publication.json").exists());
    rig.prompt("after-host-deadline");
    assert_eq!(
        rig.done().last_stop_reason,
        Some(marsh_acp::StopReason::EndTurn)
    );
    rig.stop_and_receipt();
    eprintln!(
        "actual ACP host budget expired in {:.3}s; public uncertainty and exact-generation revocation observed",
        elapsed.as_secs_f64()
    );
}

#[test]
#[ignore = "real CLI scope contention across two Node agents; crosses the production 30s admission wait"]
fn cli_scope_contention_rejects_before_second_agent_grant() {
    let rig = Rig::with_host(true);
    let (second, _, second_lease) = rig
        .client
        .acp_start_reserved("fixture".into(), rig.shell.clone(), None)
        .unwrap();
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rig.preparation_delay_ms.store(35_000, Ordering::SeqCst);
        thread::scope(|threads| {
            let first = threads.spawn(|| {
                rig.cli(&[
                    "publish",
                    &rig.id,
                    "--name",
                    "scope-first",
                    "--kit",
                    "fixture",
                ])
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            while rig.preparations.load(Ordering::SeqCst) == 0 {
                assert!(
                    Instant::now() < deadline,
                    "first publication did not enter actual preparation"
                );
                thread::sleep(Duration::from_millis(10));
            }
            let calls = rig.root.path().join("stock-calls.jsonl");
            let before = fs::read(&calls).unwrap();
            let started = Instant::now();
            let refused = rig.cli(&["publish", &second, "--name", "scope-second"]);
            assert!(!refused.status.success());
            let diagnostic = String::from_utf8_lossy(&refused.stderr);
            assert!(
                diagnostic.contains("rejected before effect")
                    && diagnostic.contains("scope is busy"),
                "{diagnostic}"
            );
            assert!(started.elapsed() >= Duration::from_secs(30));
            assert_eq!(
                fs::read(&calls).unwrap(),
                before,
                "contending request reached stock before admission"
            );
            let status = rig
                .client
                .acp_status(second.clone(), rig.shell.clone(), 0)
                .unwrap();
            assert!(
                status.published_name.is_none(),
                "rejected request left a second grant"
            );
            assert_eq!(rig.preparations.load(Ordering::SeqCst), 1);
            let published = first.join().unwrap();
            assert!(
                published.status.success(),
                "{}",
                String::from_utf8_lossy(&published.stderr)
            );
        });
        rig.relay
            .acp_unpublish(rig.shell.clone(), "scope-first".into())
            .unwrap();
        let prompt = rig.client.acp_prompt_with_key(
            second.clone(),
            rig.shell.clone(),
            Uuid::new_v4().to_string(),
            "still usable after scope rejection".into(),
        );
        assert!(
            prompt.is_ok(),
            "second agent lost controller authority: {prompt:?}"
        );
    }));
    // Cleanup runs even when the external-user assertion fails; only these
    // two fixture-owned agents are addressed, never process-name/global kills.
    let _ = rig.client.acp_stop(second.clone(), rig.shell.clone());
    drop(second_lease);
    rig.stop_and_receipt();
    if let Err(panic) = checked {
        std::panic::resume_unwind(panic);
    }
}

#[test]
#[ignore = "actual >six-minute production daemon-client/private-channel preparation"]
fn cold_publication_daemon_client_exceeds_six_minutes() {
    let rig = Rig::new();
    rig.preparation_delay_ms.store(365_250, Ordering::SeqCst);
    let start = Instant::now();
    let result = rig.relay.acp_publish_target(
        rig.id.clone(),
        rig.shell.clone(),
        "fixture_tool".into(),
        None,
        Some("fixture".into()),
    );
    let elapsed = start.elapsed();
    assert!(
        result.is_ok(),
        "quiet preparation failed after {elapsed:?}: {result:?}"
    );
    assert!(elapsed >= Duration::from_secs(365));
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.status(0).published_name.as_deref(),
        Some("fixture_tool")
    );
    let events = fs::read_to_string(rig.shell.launch_directory.join(".host-events.jsonl")).unwrap();
    let kinds: Vec<_> = events
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, ["prepare_kit", "begin_commit", "complete"]);
    rig.relay
        .acp_unpublish(rig.shell.clone(), "fixture_tool".into())
        .unwrap();
    assert!(!rig.shell.launch_directory.join("publication.json").exists());
    rig.stop_and_receipt();
    eprintln!(
        "actual public daemon client/private host cold preparation {:.3}s; committed once; no read-cap timeout (CLI/stock qualification separate)",
        elapsed.as_secs_f64()
    );
}

#[test]
#[ignore = "observed >six-minute backend preparation; requires freshly built CLI pair"]
fn cold_publication_exceeds_six_minutes() {
    let rig = Rig::with_host(true);
    rig.preparation_delay_ms.store(365_250, Ordering::SeqCst);
    let start = Instant::now();
    let result = rig.cli(&[
        "publish",
        &rig.id,
        "--name",
        "fixture_tool",
        "--kit",
        "fixture",
    ]);
    let elapsed = start.elapsed();
    assert!(
        result.status.success(),
        "quiet cold preparation failed after {elapsed:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        elapsed >= Duration::from_secs(365),
        "slow backend boundary was not exercised"
    );
    assert_eq!(
        String::from_utf8_lossy(&result.stderr)
            .matches("starting fixture worker VM")
            .count(),
        1,
        "periodic progress masked the former read cap"
    );
    assert_eq!(rig.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(
        rig.status(0).published_name.as_deref(),
        Some("fixture_tool")
    );
    eprintln!(
        "observed quiet ACP Kit preparation: {:.3}s; one cold frame; committed, not timed out or replayed",
        elapsed.as_secs_f64()
    );
    assert!(rig.cli(&["unpublish", "fixture_tool"]).status.success());
    rig.stop_and_receipt();
}

#[test]
#[ignore = "requires MARSH_ACP_CALLER_BIN pointing to the exact newly built marsh"]
#[allow(
    clippy::too_many_lines,
    reason = "One exact CLI subprocess journey with byte, authority and job identity assertions"
)]
fn cli_stdin_permissions_reservations_and_publication_options() {
    use std::os::unix::fs::symlink;
    let rig = Rig::new();
    let binary = std::env::var_os("MARSH_ACP_CALLER_BIN").expect("exact caller binary required");
    let acp = rig.root.path().join("acp");
    symlink(binary, &acp).unwrap();
    let context = serde_json::to_string(&rig.shell).unwrap();
    let socket = EndpointPaths::for_home(&rig.home).unwrap().socket;
    let invoke_at = |socket: &std::path::Path, args: &[&str], bytes: &[u8]| {
        let mut child = Command::new(&acp)
            .args([
                "--invoke-bundled",
                "acp",
                "--marsh-guest",
                "--marsh-session",
                &rig.shell.session_id,
            ])
            .args(args)
            .env("MARSH_HOME", &rig.home)
            .env("HOME", rig.root.path().join("host"))
            .env("USER", &rig.shell.username)
            .env("MARSH_EXTERNAL_SESSION", &context)
            .env("MARSH_DAEMON_SOCKET", socket)
            .env("MARSH_DAEMON_TOKEN", rig.root.path().join("relay-token"))
            .current_dir(&rig.shell.launch_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = bytes.to_vec();
        let writer = thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let result = child.wait_with_output().unwrap();
        writer.join().unwrap();
        result
    };
    let invoke = |args: &[&str], bytes: &[u8]| invoke_at(&socket, args, bytes);
    let help = invoke(&["--help"], &[]);
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    let example = help
        .split_once("Example:\n")
        .unwrap()
        .1
        .split("\n\nOther commands:")
        .next()
        .unwrap();
    // A syntax-only compatibility oracle, not Bash as the implementation.
    let parsed = Command::new("bash")
        .args(["-n", "-c", example])
        .output()
        .unwrap();
    assert!(
        parsed.status.success(),
        "headline example is not shell syntax: {}",
        String::from_utf8_lossy(&parsed.stderr)
    );
    let before = rig.status(0).turns.len();
    let proxy_path = rig.root.path().join("pre-dispatch.sock");
    let proxy = prompt_reply_proxy(&socket, &proxy_path, true);
    let unavailable = invoke_at(&proxy_path, &["ask", &rig.id, "clean"], &[]);
    proxy.join().unwrap();
    assert!(!unavailable.status.success());
    assert!(
        !String::from_utf8_lossy(&unavailable.stderr).contains("uncertain"),
        "connect-before-dispatch advice: {}",
        String::from_utf8_lossy(&unavailable.stderr)
    );
    assert_eq!(rig.status(0).turns.len(), before);
    let proxy_path = rig.root.path().join("lost-reply.sock");
    let proxy = prompt_reply_proxy(&socket, &proxy_path, false);
    let lost_reply = invoke_at(&proxy_path, &["ask", &rig.id, "accepted-once"], &[]);
    proxy.join().unwrap();
    assert!(!lost_reply.status.success());
    let diagnostic = String::from_utf8(lost_reply.stderr).unwrap();
    assert!(
        diagnostic.contains("outcome may be uncertain"),
        "lost reply advice: {diagnostic}"
    );
    let key = diagnostic
        .split("--key ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    rig.done();
    assert_eq!(rig.status(0).turns.len(), before + 1);
    let retry = invoke(&["prompt", "--key", key, &rig.id, "accepted-once"], &[]);
    assert!(retry.status.success());
    assert_eq!(
        rig.status(0).turns.len(),
        before + 1,
        "lost reply retry duplicated the provider turn"
    );
    let lossy = invoke(&["ask", &rig.id, "lossy"], &[]);
    assert_eq!(lossy.status.code(), Some(125));
    assert!(
        !rig.status(0).turn_active,
        "ask returned before the lossy turn ended"
    );
    let clean = invoke(&["ask", &rig.id, "clean"], &[]);
    assert!(clean.status.success());
    assert_eq!(clean.stdout, b"fixture:clean\n");
    let text = b"  exact\nbytes\t\0\n\n";
    let result = invoke(&["ask", &rig.id, "-"], text);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        [b"fixture:".as_slice(), text, b"\n"].concat()
    );
    let before = rig.status(0).turns.len();
    let result = invoke(&["prompt", &rig.id, "-"], &vec![b'x'; 1_048_577]);
    assert_eq!(result.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&result.stderr).contains("exceeds 1 MiB"));
    assert_eq!(
        rig.status(0).turns.len(),
        before,
        "oversize stdin dispatched a turn"
    );
    let near_limit = invoke(&["ask", &rig.id, "-"], &vec![b'x'; 1_048_064]);
    assert_eq!(near_limit.status.code(), Some(125));
    assert!(
        String::from_utf8_lossy(&near_limit.stderr).contains("shorten"),
        "control-frame limit lacks remedy: {}",
        String::from_utf8_lossy(&near_limit.stderr)
    );
    assert!(!String::from_utf8_lossy(&near_limit.stderr).contains("uncertain"));
    assert_eq!(rig.status(0).turns.len(), before);
    let large = vec![b'x'; 180_000]; // exceeds Linux's single-argv bound
    let result = invoke(&["ask", &rig.id, "-"], &large);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        result.stdout,
        [b"fixture:".as_slice(), large.as_slice(), b"\n"].concat()
    );
    let mine = invoke(&["list", "--json", "--mine"], &[]);
    assert!(mine.status.success());
    let rows: serde_json::Value = serde_json::from_slice(&mine.stdout).unwrap();
    assert_eq!(rows[0]["owner_shell_session_id"], rig.shell.session_id);
    rig.prompt("permission");
    let deadline = Instant::now() + Duration::from_secs(5);
    while rig.status(0).permissions.is_empty() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let rejected = invoke(&["ask", &rig.id, "busy"], &[]);
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stderr).contains("uncertain"));
    let cold_before = rig.preparations.load(Ordering::SeqCst);
    let rejected = invoke(
        &[
            "publish",
            &rig.id,
            "--kit",
            "fixture",
            "--name",
            "fixture_tool",
        ],
        &[],
    );
    assert!(!rejected.status.success());
    assert_eq!(
        rig.preparations.load(Ordering::SeqCst),
        cold_before,
        "rejected active publication prepared a Kit"
    );
    let human = invoke(&["permissions", &rig.id], &[]);
    assert!(human.status.success());
    let human = String::from_utf8(human.stdout).unwrap();
    assert!(
        human.contains("Allow once") && human.contains("acp respond") && !human.starts_with('[')
    );
    let permission = rig.status(0).permissions.remove(0);
    let reply = invoke(&["respond", &rig.id, &permission.request_id, "deny"], &[]);
    assert!(reply.status.success());
    rig.done();
    fs::write(rig.shell.launch_directory.join(".host-name-collision"), b"").unwrap();
    let collision = invoke(
        &[
            "publish",
            &rig.id,
            "--kit",
            "fixture",
            "--name",
            "fixture_tool",
        ],
        &[],
    );
    assert!(!collision.status.success());
    assert!(String::from_utf8_lossy(&collision.stderr).contains("name conflict"));
    assert_eq!(
        rig.preparations.load(Ordering::SeqCst),
        cold_before,
        "host preflight rejection prepared a Kit"
    );
    assert!(rig.status(0).published_name.is_none());
    fs::remove_file(rig.shell.launch_directory.join(".host-name-collision")).unwrap();
    // Actual host adapter argv/private handshake crosses a process boundary;
    // no stock VM claim.
    let published = invoke(
        &[
            "publish",
            &rig.id,
            "--kit",
            "fixture",
            "--name",
            "fixture_tool",
        ],
        &[],
    );
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    assert_eq!(rig.preparations.load(Ordering::SeqCst), cold_before + 1);
    assert!(String::from_utf8_lossy(&published.stderr).contains("[starting fixture worker VM…]"));
    assert!(!invoke(&["stop", &rig.id], &[]).status.success());
    let rejected = invoke(
        &[
            "publish",
            &rig.id,
            "--kit",
            "fixture",
            "--name",
            "fixture_tool",
        ],
        &[],
    );
    assert!(!rejected.status.success());
    assert_eq!(
        rig.preparations.load(Ordering::SeqCst),
        cold_before + 1,
        "rejected duplicate publication prepared a Kit"
    );
    assert!(invoke(&["unpublish", "fixture_tool"], &[]).status.success());
    // An admitted preparation failure revokes its exact grant, not the session.
    fs::write(rig.shell.launch_directory.join(".fail-prepare"), b"").unwrap();
    let failed = invoke(
        &[
            "publish",
            &rig.id,
            "--kit",
            "fixture",
            "--name",
            "fixture_tool",
        ],
        &[],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("grant was revoked"));
    assert!(rig.status(0).published_name.is_none());
    fs::remove_file(rig.shell.launch_directory.join(".fail-prepare")).unwrap();
    assert!(
        invoke(&["ask", &rig.id, "after-failed-publication"], &[])
            .status
            .success()
    );
    assert!(invoke(&["stop", &rig.id], &[]).status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    while rig.status(0).attachment.terminal.is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let reserve = invoke(&["reserve", "fixture"], &[]);
    assert!(reserve.status.success());
    let id = String::from_utf8(reserve.stdout).unwrap().trim().to_owned();
    assert!(
        rig.client
            .acp_list(rig.shell.clone())
            .unwrap()
            .iter()
            .any(|s| s.agent_session_id == id && s.owner_shell_session_id == rig.shell.session_id)
    );
    thread::scope(|scope| {
        let run = scope.spawn(|| invoke(&["run", "--reservation", &id, "fixture"], &[]));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let ready = rig
                .client
                .acp_list(rig.shell.clone())
                .unwrap()
                .into_iter()
                .find(|s| s.agent_session_id == id && s.job_id.is_some());
            if ready.is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "reserved CLI job never became ready"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let ready = invoke(&["list", "--wait", "--json"], &[]);
        let exact = invoke(&["list", "--mine", "--wait", &id, "--json"], &[]);
        // Reap the owned background caller before asserting discovery, so a
        // red no-ID discovery regression cannot hang the scoped thread join.
        let stopped = invoke(&["stop", &id], &[]);
        let result = run.join().unwrap();
        assert!(stopped.status.success());
        assert!(exact.status.success());
        assert!(
            ready.status.success(),
            "stopped then new --wait failed: {}",
            String::from_utf8_lossy(&ready.stderr)
        );
        let rows: serde_json::Value = serde_json::from_slice(&ready.stdout).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["agent_session_id"], id);
        assert!(result.status.success());
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains(&format!("session {id} ready; Kit job"))
        );
    });
}
