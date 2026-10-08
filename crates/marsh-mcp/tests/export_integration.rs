//! Integration tests for export-only MCP server mode.
//!
//! Tests the real daemon Unix socket protocol, argument validation, option injection prevention,
//! command registration checks, execution streaming, truncation, and cancellation over real MCP JSON-RPC.

use marsh_daemon::{
    AttachmentFrame, DaemonBackend, DaemonError, DaemonStore, ExecuteSpec, LoadSelection,
    PreparationProgress, PreparationResult, Server, ServerAttachment, SessionSpec, ShellSpec,
};
use marsh_mcp::{
    ArgBinding, ExportConfig, ExportMcp, HostConfig, StdinBinding, ToolBindings, ToolDeclaration,
    WorkspaceIdentity,
};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{CallToolRequestParams, ClientConfig},
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Clone, Debug, Default)]
struct RecordedExecution {
    command: String,
    arguments: Vec<Vec<u8>>,
    stdin: Vec<u8>,
    guest_home: PathBuf,
    home_backing: PathBuf,
}

enum ReceiptBehavior {
    Record,
    ExitWithoutReceipt,
}

struct TestKitBackend {
    registered: Vec<String>,
    registered_kits: std::collections::BTreeMap<String, String>,
    registered_kits_error: bool,
    override_receipt_kit_ref: Option<String>,
    last_execution: Arc<Mutex<Option<RecordedExecution>>>,
    injected_exit_code: i32,
    observed_execution: Option<marsh_daemon::ExecutionOutcome>,
    injected_stdout: Vec<u8>,
    injected_stderr: Vec<u8>,
    delay: Duration,
    early_disconnect: bool,
    receipt_behavior: ReceiptBehavior,
    ignore_sigint: bool,
    received_signals: Arc<Mutex<Vec<String>>>,
    decoy_session_after_own_job: Arc<Mutex<Option<String>>>,
}

impl TestKitBackend {
    fn new(registered: Vec<String>) -> Self {
        Self {
            registered,
            registered_kits: std::collections::BTreeMap::new(),
            registered_kits_error: false,
            override_receipt_kit_ref: None,
            last_execution: Arc::new(Mutex::new(None)),
            injected_exit_code: 0,
            observed_execution: None,
            injected_stdout: b"test output\n".to_vec(),
            injected_stderr: Vec::new(),
            delay: Duration::ZERO,
            early_disconnect: false,
            receipt_behavior: ReceiptBehavior::Record,
            ignore_sigint: false,
            received_signals: Arc::new(Mutex::new(Vec::new())),
            decoy_session_after_own_job: Arc::new(Mutex::new(None)),
        }
    }

    fn publish_receipt_before_exit(
        &self,
        store: &DaemonStore,
        job_id: &str,
        attachment: &ServerAttachment,
    ) -> Result<(), DaemonError> {
        let exit = marsh_daemon::ExitStatus {
            code: Some(self.injected_exit_code),
            cause: "normal".into(),
        };
        if let Some(observed) = &self.observed_execution {
            store.finish_job_with_execution(
                job_id,
                observed.clone(),
                exit,
                true,
                marsh_daemon::CleanupState::Verified,
                marsh_daemon::TimingReport::default(),
            )?;
        } else {
            store.finish_job(
                job_id,
                exit,
                true,
                marsh_daemon::CleanupState::Verified,
                marsh_daemon::TimingReport::default(),
            )?;
        }
        // Persist before releasing terminal status, just like the real boundary.
        attachment.send(&AttachmentFrame::Exited {
            code: self.injected_exit_code,
        })
    }
}

fn record_late_decoy(store: &DaemonStore, other_session: String) -> Result<(), DaemonError> {
    let (decoy_id, _) = store
        .begin_job(marsh_daemon::NewJob {
            session_id: other_session,
            command: "test-report".into(),
            kit_ref: "kit:test".into(),
            workload_image: "image:test".into(),
            mounts: vec![],
        })
        .map_err(|e| DaemonError::InvalidState(e.to_string()))?;
    store
        .finish_job(
            &decoy_id,
            marsh_daemon::ExitStatus {
                code: Some(0),
                cause: "normal".into(),
            },
            true,
            marsh_daemon::CleanupState::Verified,
            marsh_daemon::TimingReport::default(),
        )
        .map_err(|e| DaemonError::InvalidState(e.to_string()))?;
    Ok(())
}

impl DaemonBackend for TestKitBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(self.registered.clone())
    }

    fn registered_kits(&self) -> Result<std::collections::BTreeMap<String, String>, DaemonError> {
        if self.registered_kits_error {
            return Err(DaemonError::InvalidState("daemon store unavailable".into()));
        }
        Ok(self.registered_kits.clone())
    }

    fn prepare(
        &self,
        _selection: &LoadSelection,
        _session: &SessionSpec,
        _progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        Ok(PreparationResult {
            cold_kits: self.registered.clone(),
            sandboxes: self
                .registered
                .iter()
                .map(|name| (name.clone(), format!("test-{name}")))
                .collect(),
        })
    }

    fn execute(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError> {
        let mut stdin_bytes = Vec::new();
        loop {
            match attachment.receive()? {
                AttachmentFrame::Stdin { bytes } => {
                    stdin_bytes.extend_from_slice(&bytes);
                }
                AttachmentFrame::StdinEof => {
                    break;
                }
                AttachmentFrame::Signal { signal } => {
                    self.received_signals.lock().unwrap().push(signal.clone());
                    if !self.ignore_sigint || signal != "SIGINT" {
                        let _ = attachment.send(&AttachmentFrame::Stderr {
                            bytes: format!("interrupted by {signal}").into_bytes(),
                        });
                        let _ = attachment.send(&AttachmentFrame::Exited { code: 130 });
                        return Ok(());
                    }
                }
                _ => {}
            }
        }

        *self.last_execution.lock().unwrap() = Some(RecordedExecution {
            command: request.command.clone(),
            arguments: request.arguments.clone(),
            stdin: stdin_bytes,
            guest_home: request.session.guest_home.clone(),
            home_backing: request.session.home_backing.clone(),
        });

        if self.early_disconnect {
            // Drop connection without Exited frame
            drop(attachment);
            return Ok(());
        }

        if self.delay > Duration::ZERO {
            let deadline = std::time::Instant::now() + self.delay;
            while std::time::Instant::now() < deadline {
                match attachment.receive() {
                    Ok(AttachmentFrame::Signal { signal }) => {
                        self.received_signals.lock().unwrap().push(signal.clone());
                        if !self.ignore_sigint || signal != "SIGINT" {
                            let _ = attachment.send(&AttachmentFrame::Stderr {
                                bytes: format!("interrupted by {signal}").into_bytes(),
                            });
                            let _ = attachment.send(&AttachmentFrame::Exited { code: 130 });
                            return Ok(());
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return Ok(()),
                }
            }
        }

        if matches!(self.receipt_behavior, ReceiptBehavior::ExitWithoutReceipt) {
            attachment.send(&AttachmentFrame::Exited { code: 0 })?;
            return Ok(());
        }

        // Record a job receipt in store so client.jobs() returns a record
        let kit_ref = self
            .override_receipt_kit_ref
            .clone()
            .or_else(|| self.registered_kits.get(&request.command).cloned())
            .unwrap_or_else(|| "kit:test".into());

        let (job_id, _) = store
            .begin_job(marsh_daemon::NewJob {
                session_id: request.session.session_id,
                command: request.command,
                kit_ref,
                workload_image: "image:test".into(),
                mounts: vec![],
            })
            .map_err(|e| DaemonError::InvalidState(e.to_string()))?;

        // In the receipt-binding test, make a same-command decoy newer than
        // this execution before the MCP handler queries the job list.
        if let Some(other_session) = self.decoy_session_after_own_job.lock().unwrap().clone() {
            record_late_decoy(&store, other_session)?;
        }

        if !self.injected_stdout.is_empty() {
            attachment.send(&AttachmentFrame::Stdout {
                bytes: self.injected_stdout.clone(),
            })?;
        }
        if !self.injected_stderr.is_empty() {
            attachment.send(&AttachmentFrame::Stderr {
                bytes: self.injected_stderr.clone(),
            })?;
        }
        self.publish_receipt_before_exit(&store, &job_id, &attachment)
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }
}

struct TestHarness {
    temp: tempfile::TempDir,
    workspace: PathBuf,
    home: PathBuf,
    marsh: PathBuf,
    _marshd: PathBuf,
    sbx: PathBuf,
    server_shutdown: Arc<AtomicBool>,
}

impl TestHarness {
    fn new(backend: Arc<dyn DaemonBackend>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();

        let marsh = temp.path().join("marsh");
        fs::write(&marsh, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&marsh, fs::Permissions::from_mode(0o755)).unwrap();

        let marshd = temp.path().join("marshd");
        fs::write(&marshd, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&marshd, fs::Permissions::from_mode(0o755)).unwrap();

        let sbx = temp.path().join("sbx");
        fs::write(&sbx, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&sbx, fs::Permissions::from_mode(0o755)).unwrap();

        // Start the real daemon server in a background thread
        let server = Server::bind(&home).unwrap().with_backend(backend);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        thread::spawn(move || {
            while !shutdown_clone.load(Ordering::Relaxed) {
                if server.serve_one().is_err() {
                    break;
                }
            }
        });

        // Ensure daemon socket exists before proceeding
        let paths = marsh_daemon::EndpointPaths::for_home(&home).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if paths.socket.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        Self {
            temp,
            workspace,
            home,
            marsh,
            _marshd: marshd,
            sbx,
            server_shutdown: shutdown,
        }
    }

    fn host_config(&self) -> HostConfig {
        HostConfig::new(&self.workspace, &self.home, &self.marsh, &self.sbx, false).unwrap()
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.server_shutdown.store(true, Ordering::Relaxed);
    }
}

fn make_declaration(command: &str) -> ToolDeclaration {
    ToolDeclaration {
        schema_version: "marsh.published_tool/v1".into(),
        tool_name: "project_test".into(),
        description: "Run project tests".into(),
        publication_generation: None,
        command: command.into(),
        pipeline: None,
        input_schema: json!({
            "type": "object",
            "properties": {
                "filter": { "type": "string", "description": "Test name filter" },
                "coverage": { "type": "boolean", "description": "Enable coverage" },
                "input_data": { "type": "string", "description": "Stdin data" }
            },
            "required": ["filter"],
            "additionalProperties": false
        }),
        bindings: ToolBindings {
            argv: vec![
                ArgBinding::Literal {
                    value: "--run".into(),
                },
                ArgBinding::NamedOption {
                    option: "--filter".into(),
                    field: "filter".into(),
                },
                ArgBinding::Flag {
                    option: "--coverage".into(),
                    field: "coverage".into(),
                },
            ],
            stdin: Some(StdinBinding {
                field: "input_data".into(),
                max_bytes: 4096,
            }),
        },
        max_output_bytes: 1024,
        timeout_ms: 5000,
        kit_identity: None,
        canonical_workspace: None,
        workspace_identity: None,
    }
}

#[derive(Clone, Debug, Default)]
struct TestClient;

impl ClientHandler for TestClient {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::default()
    }
}

#[tokio::test]
async fn test_tools_list_exposes_only_published_tool() {
    let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
    let harness = TestHarness::new(backend);
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();

    assert_eq!(tools.len(), 1);
    let tool = &tools[0];
    assert_eq!(tool.name, "project_test");
    assert_eq!(tool.description.as_deref(), Some("Run project tests"));

    // Verify dev MCP tools are completely absent
    for forbidden in [
        "shell_run",
        "qualify",
        "doctor",
        "status",
        "results_list",
        "scope_start",
        "scope_run",
        "workers_reset",
        "prewarm",
    ] {
        assert!(
            tools.iter().all(|t| t.name != forbidden),
            "forbidden tool '{forbidden}' must not be exposed"
        );
    }
}

#[tokio::test]
async fn published_pipeline_rejects_replaced_project_for_live_and_restarted_clients() {
    let backend = Arc::new(TestKitBackend::new(vec![]));
    let harness = TestHarness::new(backend);
    fs::write(&harness.marsh, b"#!/bin/sh\nprintf ran > \"$PWD/marker\"\n").unwrap();
    let mut declaration = make_declaration("");
    declaration.pipeline = Some("printf safe".into());
    declaration.publication_generation = Some("00000000-0000-4000-8000-000000000001".into());
    declaration.bindings.argv.clear();
    declaration.input_schema = json!({
        "type": "object",
        "properties": {"input": {"type": "string"}},
        "additionalProperties": false
    });
    declaration.bindings.stdin = Some(StdinBinding {
        field: "input".into(),
        max_bytes: 4096,
    });
    declaration.canonical_workspace = Some(harness.workspace.clone());
    declaration.workspace_identity =
        Some(WorkspaceIdentity::for_directory(&harness.workspace).unwrap());
    let config = ExportConfig::new(harness.host_config(), declaration.clone()).unwrap();
    let server = ExportMcp::new(config);
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });
    let client = TestClient.serve(client_transport).await.unwrap();

    let moved = harness.temp.path().join("moved-project");
    let alternate = harness.temp.path().join("alternate-project");
    fs::rename(&harness.workspace, &moved).unwrap();
    fs::create_dir(&alternate).unwrap();
    std::os::unix::fs::symlink(&alternate, &harness.workspace).unwrap();

    let response = client
        .call_tool(CallToolRequestParams::new("project_test"))
        .await
        .unwrap();
    assert_eq!(response.is_error, Some(true));
    assert!(!alternate.join("marker").exists());
    assert!(ExportConfig::new(harness.host_config(), declaration).is_err());

    fs::remove_file(&harness.workspace).unwrap();
    fs::create_dir(&harness.workspace).unwrap();
    let response = client
        .call_tool(CallToolRequestParams::new("project_test"))
        .await
        .unwrap();
    assert_eq!(response.is_error, Some(true));
    assert!(!harness.workspace.join("marker").exists());
}

#[tokio::test]
async fn published_pipeline_passes_persisted_project_identity_to_host_child() {
    let harness = TestHarness::new(Arc::new(TestKitBackend::new(vec![])));
    fs::write(
        &harness.marsh,
        b"#!/bin/sh\nprintf '%s\\n%s\\n%s' \"$MARSH_MCP_EXPECTED_PROJECT_PATH\" \"$MARSH_MCP_EXPECTED_PROJECT_DEV\" \"$MARSH_MCP_EXPECTED_PROJECT_INO\"\n",
    )
    .unwrap();
    let mut declaration = make_declaration("");
    declaration.pipeline = Some("printf safe".into());
    declaration.publication_generation = Some("00000000-0000-4000-8000-000000000001".into());
    declaration.bindings.argv.clear();
    declaration.bindings.stdin = None;
    declaration.input_schema =
        json!({"type": "object", "properties": {}, "additionalProperties": false});
    declaration.canonical_workspace = Some(harness.workspace.clone());
    let identity = WorkspaceIdentity::for_directory(&harness.workspace).unwrap();
    declaration.workspace_identity = Some(identity);
    let config = ExportConfig::new(harness.host_config(), declaration).unwrap();
    let server = ExportMcp::new(config);
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });
    let client = TestClient.serve(client_transport).await.unwrap();
    let response = client
        .call_tool(CallToolRequestParams::new("project_test"))
        .await
        .unwrap();
    let result = response.structured_content.unwrap();
    assert_eq!(
        result["data"]["stdout"],
        format!(
            "{}\n{}\n{}",
            harness.workspace.canonicalize().unwrap().display(),
            identity.device,
            identity.inode
        ),
        "{result}"
    );
}

#[tokio::test]
async fn test_call_tool_rejects_unauthorized_tool_names() {
    let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
    let harness = TestHarness::new(backend);
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    for forbidden in ["shell_run", "qualify", "doctor", "unknown_tool"] {
        let result = client
            .call_tool(CallToolRequestParams::new(forbidden))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["ok"], false);
        assert!(
            structured["error"]
                .as_str()
                .unwrap()
                .contains("export-only server exposes only 'project_test'")
        );
    }
}

#[tokio::test]
async fn test_call_tool_rejects_unknown_field_mcp01() {
    let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
    let harness = TestHarness::new(backend.clone());
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "test1",
        "injected_unpermitted_field": "dangerous"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], false);
    assert!(
        structured["error"]
            .as_str()
            .unwrap()
            .contains("unknown field 'injected_unpermitted_field' is not permitted")
    );
    assert!(
        backend.last_execution.lock().unwrap().is_none(),
        "no job should be created when validation fails"
    );
}

#[tokio::test]
async fn test_call_tool_rejects_option_injection_mcp02() {
    let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
    let harness = TestHarness::new(backend.clone());
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "--injected-flag",
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], false);
    assert!(
        structured["error"]
            .as_str()
            .unwrap()
            .contains("cannot start with '-' (option injection prevented)")
    );
    assert!(
        backend.last_execution.lock().unwrap().is_none(),
        "no job should be created when option injection is attempted"
    );
}

#[tokio::test]
async fn test_call_tool_rejects_unregistered_command_mcp03() {
    // Backend registers "other-cmd", but tool declaration demands "test-report"
    let backend = Arc::new(TestKitBackend::new(vec!["other-cmd".into()]));
    let harness = TestHarness::new(backend);
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "valid_test",
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], false);
    assert!(
        structured["error"]
            .as_str()
            .unwrap()
            .contains("command 'test-report' is not registered")
    );
}

#[tokio::test]
async fn test_call_tool_success_executes_real_daemon_path() {
    let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
    let harness = TestHarness::new(backend.clone());
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "unit_suite",
        "coverage": true,
        "input_data": "sample payload"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], true);
    let data = &structured["data"];
    assert_eq!(data["tool_name"], "project_test");
    assert_eq!(data["command"], "test-report");
    assert_eq!(data["outcome"], "success");
    assert_eq!(data["exit_code"], 0);
    assert_eq!(data["stdout"], "test output\n");
    assert_eq!(data["cleanup_certainty"], "verified");
    assert_eq!(data["output_complete"], true);
    assert!(data["receipt_selector"].is_string());

    // Verify backend received exact literal argv and stdin without shell interpolation
    let recorded = backend.last_execution.lock().unwrap().clone().unwrap();
    assert_eq!(recorded.command, "test-report");
    assert_eq!(recorded.home_backing, harness.home.canonicalize().unwrap());
    let expected_guest = nix::unistd::User::from_uid(nix::unistd::Uid::effective())
        .unwrap()
        .unwrap()
        .dir;
    assert_eq!(recorded.guest_home, expected_guest);
    let argv_strings: Vec<String> = recorded
        .arguments
        .into_iter()
        .map(|a| String::from_utf8(a).unwrap())
        .collect();
    assert_eq!(
        argv_strings,
        vec!["--run", "--filter", "unit_suite", "--coverage"]
    );
    assert_eq!(String::from_utf8(recorded.stdin).unwrap(), "sample payload");
}

#[tokio::test]
async fn observed_worker_outcome_survives_real_mcp_and_daemon_sockets() {
    for observed in [
        json!({"status":"exited", "code":23}),
        json!({"status":"limit_exceeded", "resource":"output"}),
        json!({"status":"setup_failed", "stage":"create"}),
        json!({"status":"unknown"}),
    ] {
        let mut backend = TestKitBackend::new(vec!["test-report".into()]);
        // The public delivery status and legacy prose deliberately cannot
        // reconstruct the worker fact. Only the bound typed receipt can.
        backend.injected_exit_code = 125;
        backend.observed_execution = Some(serde_json::from_value(observed.clone()).unwrap());
        let harness = TestHarness::new(Arc::new(backend));
        let server = ExportMcp::new(
            ExportConfig::new(harness.host_config(), make_declaration("test-report")).unwrap(),
        );
        let (server_socket, client_socket) = tokio::net::UnixStream::pair().unwrap();
        let task = tokio::spawn(async move {
            server
                .serve(server_socket)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap()
        });
        let client = TestClient.serve(client_socket).await.unwrap();
        let arguments = json!({"filter":"typed", "input_data":"probe"})
            .as_object()
            .unwrap()
            .clone();
        let result = client
            .call_tool(CallToolRequestParams::new("project_test").with_arguments(arguments))
            .await
            .unwrap();
        let data = &result.structured_content.as_ref().unwrap()["data"];
        assert_eq!(data["exit_code"], 125);
        assert_eq!(data["execution"], observed);
        assert!(data["job_id"].is_string());
        client.cancel().await.unwrap();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn test_call_tool_nonzero_exit_code_mcp04() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.injected_exit_code = 42;
    backend.injected_stderr = b"test error logged\n".to_vec();
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend);
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "failing_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    let data = &structured["data"];
    assert_eq!(data["outcome"], "exit_nonzero");
    assert_eq!(data["exit_code"], 42);
    assert_eq!(data["stderr"], "test error logged\n");
}

#[tokio::test]
async fn test_call_tool_truncates_oversized_output() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.injected_stdout = vec![b'A'; 2048]; // max_output_bytes is 1024
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend);
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "large_output_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    let structured = result.structured_content.unwrap();
    let data = &structured["data"];
    assert_eq!(data["stdout_truncated"], true);
    assert_eq!(data["output_complete"], false);
    assert_eq!(data["stdout"].as_str().unwrap().len(), 1024);
}

#[tokio::test]
async fn test_early_eof_marks_failed_uncertain() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.early_disconnect = true;
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend);
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "test1"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    let structured = result.structured_content.unwrap();
    let data = &structured["data"];
    assert_eq!(data["outcome"], "failed");
    assert_eq!(data["cleanup_certainty"], "uncertain");
    assert_eq!(data["output_complete"], false);
}

#[tokio::test]
async fn test_exit_zero_without_receipt_is_not_success() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.receipt_behavior = ReceiptBehavior::ExitWithoutReceipt;
    let harness = TestHarness::new(Arc::new(backend));
    let server = ExportMcp::new(
        ExportConfig::new(harness.host_config(), make_declaration("test-report")).unwrap(),
    );
    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });
    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({ "filter": "test1" }).as_object().unwrap().clone();
    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    let data = &structured["data"];
    assert_eq!(data["outcome"], "failed");
    assert_eq!(data["cleanup_certainty"], "uncertain");
    assert_eq!(data["output_complete"], false);
    assert!(data["job_id"].is_null());
}

fn run_decoy_job(
    client: &marsh_daemon::Client,
    session_id: &str,
    username: &str,
    workspace: &std::path::Path,
    home: &std::path::Path,
    arg: &str,
) {
    let spec = marsh_daemon::ExecuteSpec {
        process: None,
        placement: marsh_daemon::Placement::Local,
        environment: std::collections::BTreeMap::new(),
        command: "test-report".to_string(),
        arguments: vec![arg.into()],
        working_directory: None,
        session: marsh_daemon::SessionSpec {
            session_id: session_id.to_string(),
            username: username.to_string(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            launch_directory: workspace.to_path_buf(),
            guest_home: home.to_path_buf(),
            home_backing: home.to_path_buf(),
            ephemeral_home: false,
            terminal: false,
            terminal_size: None,
        },
    };
    let exec = client.start_execution(spec).unwrap();
    exec.send(&marsh_daemon::AttachmentFrame::StdinEof).unwrap();
    loop {
        match exec.receive() {
            Ok(marsh_daemon::AttachmentFrame::Exited { .. }) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[tokio::test]
async fn test_concurrent_receipt_binding_exact_session() {
    let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
    let harness = TestHarness::new(backend.clone());
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    // Attach a separate shell session to produce decoy jobs
    let client_direct = marsh_daemon::Client::connect(&harness.home).unwrap();
    let username = harness.host_config().username().to_string();
    let authority = marsh_daemon::SessionAuthority {
        username: username.clone(),
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        launch_directory: harness.workspace.clone(),
        guest_home: harness.home.clone(),
        home_backing: harness.home.clone(),
        ephemeral_home: false,
    };
    let marsh_daemon::PublicReply::ShellAttached {
        session_id: other_session,
    } = client_direct
        .request(marsh_daemon::PublicRequest::AttachShell {
            pid: std::process::id(),
            session: authority,
        })
        .unwrap()
    else {
        panic!("attach failed")
    };

    // Pre-populate one decoy, then let the backend create a second decoy
    // *after* the MCP job begins and *before* its receipt lookup.
    run_decoy_job(
        &client_direct,
        &other_session,
        &username,
        &harness.workspace,
        &harness.home,
        "--decoy-1",
    );
    *backend.decoy_session_after_own_job.lock().unwrap() = Some(other_session.clone());

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let mcp_client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "test_receipt"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = mcp_client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    let structured = result.structured_content.unwrap();
    let data = &structured["data"];
    let job_id = data["job_id"].as_str().unwrap();

    let all_jobs = client_direct.jobs().unwrap();
    assert_eq!(
        all_jobs.jobs.len(),
        3,
        "must have two decoys and the MCP job"
    );
    assert_eq!(
        client_direct
            .job(all_jobs.jobs[0].job_id.clone())
            .unwrap()
            .session_id,
        other_session
    );
    assert_eq!(all_jobs.jobs[0].command, "test-report");
    assert_ne!(
        job_id, all_jobs.jobs[0].job_id,
        "must not bind to newer decoy"
    );
    assert_ne!(
        job_id, all_jobs.jobs[2].job_id,
        "must not bind to earlier decoy"
    );

    // Verify the bound job in the daemon belongs to the exact session created for this call, NOT other_session
    let receipt = client_direct.job(job_id.to_string()).unwrap();
    assert_ne!(
        receipt.session_id, other_session,
        "receipt must bind to exact session, not decoy"
    );
    assert_eq!(receipt.job_id, job_id);
    assert_eq!(receipt.command, "test-report");
}

#[tokio::test]
async fn test_timeout_escalates_and_reaps() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.delay = Duration::from_secs(5);
    backend.ignore_sigint = true; // Forces escalation to SIGTERM
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let mut decl = make_declaration("test-report");
    decl.timeout_ms = 200; // Fast timeout
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "timeout_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let start = std::time::Instant::now();
    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(4),
        "must reap promptly, took {elapsed:?}"
    );

    let structured = result.structured_content.unwrap();
    let data = &structured["data"];
    assert_eq!(data["outcome"], "timeout");
    assert_eq!(data["output_complete"], false);

    // Backend must have received SIGINT, and then SIGTERM upon escalation
    let signals = backend.received_signals.lock().unwrap().clone();
    assert!(
        signals.contains(&"SIGINT".to_string()),
        "must send SIGINT first"
    );
    assert!(
        signals.contains(&"SIGTERM".to_string()),
        "must escalate to SIGTERM"
    );
}

#[tokio::test]
async fn test_kit_identity_match_succeeds() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend
        .registered_kits
        .insert("test-report".into(), "local-v3:/path/to/test-kit".into());
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend);
    let mut decl = make_declaration("test-report");
    decl.kit_identity = Some("local-v3:/path/to/test-kit".into());
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "kit_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], true);
}

#[tokio::test]
async fn test_kit_identity_mismatch_fails_closed() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend
        .registered_kits
        .insert("test-report".into(), "local-v3:/path/to/actual-kit".into());
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend);
    let mut decl = make_declaration("test-report");
    decl.kit_identity = Some("local-v3:/path/to/expected-different-kit".into());
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "kit_mismatch_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], false);
    assert!(
        structured["error"]
            .as_str()
            .unwrap()
            .contains("does not match declared kit_identity")
    );
}

#[tokio::test]
async fn test_early_disconnect_server_shutdown() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.delay = Duration::from_secs(10);
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend);
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let server_clone = server.clone();
    let server_task = tokio::spawn(async move {
        let running = server_clone.clone().serve(server_transport).await.unwrap();
        let _ = running.waiting().await;
        server_clone.shutdown_server().await
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "disconnect_test"
    })
    .as_object()
    .unwrap()
    .clone();

    // Start call in background
    let call_task = tokio::spawn(async move {
        let _ = client
            .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
            .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Simulate early disconnect / transport drop
    drop(call_task);

    // Shutdown completes boundedly without leaking or hanging
    let start = std::time::Instant::now();
    let shutdown_res = tokio::time::timeout(Duration::from_secs(5), server.shutdown_server()).await;
    assert!(shutdown_res.is_ok(), "shutdown must finish within timeout");
    assert!(start.elapsed() < Duration::from_secs(4));
    let _ = server_task.await;
}

#[tokio::test]
async fn test_cancel_escalates_and_reaps() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.delay = Duration::from_secs(5);
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let decl = make_declaration("test-report");
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let server_clone = server.clone();
    let _server_task = tokio::spawn(async move {
        let _ = server_clone
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "cancel_test"
    })
    .as_object()
    .unwrap()
    .clone();

    // Call tool in background
    let call_future = tokio::spawn(async move {
        client
            .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
            .await
            .unwrap()
    });

    // Wait until tool starts executing in backend
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if backend.last_execution.lock().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Trigger cancellation via server in-flight cancellation
    let _ = server.shutdown_server().await;

    let result = call_future.await.unwrap();
    let structured = result.structured_content.unwrap();
    let data = &structured["data"];
    assert_eq!(data["outcome"], "cancelled");
    assert_eq!(data["output_complete"], false);

    // Backend must have received cancellation signal
    let signals = backend.received_signals.lock().unwrap().clone();
    assert!(
        signals.contains(&"SIGINT".to_string()) || signals.contains(&"SIGTERM".to_string()),
        "must send signal on cancellation, got: {signals:?}"
    );
}

#[tokio::test]
async fn test_kit_identity_sibling_local_v3_fails() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    // Daemon has registered kit as sibling path (e.g. /path/to/test-kit-sibling)
    backend.registered_kits.insert(
        "test-report".into(),
        "local-v3:/path/to/test-kit-sibling".into(),
    );
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let mut decl = make_declaration("test-report");
    decl.kit_identity = Some("local-v3:/path/to/test-kit".into());
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "sibling_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], false);
    assert!(
        structured["error"]
            .as_str()
            .unwrap()
            .contains("does not match declared kit_identity")
    );
    // Ensure execution did NOT proceed
    assert!(backend.last_execution.lock().unwrap().is_none());
}

#[tokio::test]
async fn test_registered_kits_error_fails_before_execution() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.registered_kits_error = true;
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let mut decl = make_declaration("test-report");
    decl.kit_identity = Some("local-v3:/path/to/test-kit".into());
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "map_err_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], false);
    assert!(
        structured["error"]
            .as_str()
            .unwrap()
            .contains("cannot query registered kits from daemon")
    );
    // Must fail before execution
    assert!(backend.last_execution.lock().unwrap().is_none());
}

#[tokio::test]
async fn test_registered_kits_missing_map_fails_before_execution() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    // backend.registered_kits is empty -> command missing from map
    backend.registered_kits.clear();
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let mut decl = make_declaration("test-report");
    decl.kit_identity = Some("local-v3:/path/to/test-kit".into());
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "missing_map_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], false);
    assert!(
        structured["error"]
            .as_str()
            .unwrap()
            .contains("has no registered kit identity in daemon")
    );
    // Must fail before execution
    assert!(backend.last_execution.lock().unwrap().is_none());
}

#[tokio::test]
async fn test_mismatched_postreceipt_returns_failed_with_job_id() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    // Pre-execution matches registered_kits
    backend
        .registered_kits
        .insert("test-report".into(), "local-v3:/path/to/test-kit".into());
    // Post-execution receipt will report mismatched kit_ref
    backend.override_receipt_kit_ref = Some("local-v3:/path/to/unexpected-kit".into());
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let mut decl = make_declaration("test-report");
    decl.kit_identity = Some("local-v3:/path/to/test-kit".into());
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(65_536);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "postreceipt_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(true));
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["ok"], false);
    let data = &structured["data"];
    assert_eq!(data["outcome"], "failed");
    assert!(data["job_id"].as_str().is_some(), "must return job id");
    assert!(
        data["stderr"]
            .as_str()
            .unwrap()
            .contains("does not match declared kit_identity")
    );
    // Execution DID occur
    assert!(backend.last_execution.lock().unwrap().is_some());
}

#[tokio::test]
async fn test_bound_response_data_dual_max_nonascii_escape_outputs() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    // 256 KiB of mixed non-ASCII (4-byte emojis, 3-byte CJK, Cyrillic) and JSON escape characters (\, ", \n, \t, etc.)
    let pattern = "🦀🚀✨\"\\/\n\t\r\x0c你好世界приветαβγδε";
    let reps = (262_144 / pattern.len()) + 1;
    let large_bytes = pattern.repeat(reps).into_bytes();
    backend.injected_stdout = large_bytes[..262_144].to_vec();
    backend.injected_stderr = large_bytes[..262_144].to_vec();
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend);
    let mut decl = make_declaration("test-report");
    decl.max_output_bytes = 262_144;
    let export_config = ExportConfig::new(harness.host_config(), decl).unwrap();
    let server = ExportMcp::new(export_config);

    let (server_transport, client_transport) = tokio::io::duplex(2_000_000);
    let _server_task = tokio::spawn(async move {
        let _ = server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await;
    });

    let client = TestClient.serve(client_transport).await.unwrap();
    let args = json!({
        "filter": "dual_max_test"
    })
    .as_object()
    .unwrap()
    .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("project_test").with_arguments(args))
        .await
        .unwrap();

    let structured = result.structured_content.unwrap();
    let data = &structured["data"];
    assert_eq!(data["stdout_truncated"], false);
    assert_eq!(data["stderr_truncated"], false);
    assert_eq!(data["output_complete"], true);

    // Measure the full rmcp CallToolResult frame serialized to JSON
    let envelope = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "resultType": "complete",
            "content": [{ "type": "text", "text": "Exact output bytes and status are in structuredContent." }],
            "structuredContent": structured,
            "isError": false
        }
    });
    let frame_bytes = serde_json::to_vec(&envelope).unwrap();
    assert!(
        frame_bytes.len() <= 1_048_576,
        "full frame {} must be <= 1 MiB",
        frame_bytes.len()
    );
}

#[tokio::test]
async fn test_escalate_and_drain_join_error_uncertain() {
    let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
    let harness = TestHarness::new(backend);
    let client_direct = marsh_daemon::Client::connect(&harness.home).unwrap();
    let authority = marsh_daemon::SessionAuthority {
        username: harness.host_config().username().to_string(),
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        launch_directory: harness.workspace.clone(),
        guest_home: harness.home.clone(),
        home_backing: harness.home.clone(),
        ephemeral_home: false,
    };
    let marsh_daemon::PublicReply::ShellAttached { session_id } = client_direct
        .request(marsh_daemon::PublicRequest::AttachShell {
            pid: std::process::id(),
            session: authority,
        })
        .unwrap()
    else {
        panic!("attach failed")
    };
    let spec = marsh_daemon::ExecuteSpec {
        process: None,
        placement: marsh_daemon::Placement::Local,
        environment: std::collections::BTreeMap::new(),
        command: "test-report".to_string(),
        arguments: vec![],
        working_directory: None,
        session: marsh_daemon::SessionSpec {
            session_id,
            username: harness.host_config().username().to_string(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            launch_directory: harness.workspace.clone(),
            guest_home: harness.home.clone(),
            home_backing: harness.home.clone(),
            ephemeral_home: false,
            terminal: false,
            terminal_size: None,
        },
    };
    let execution = client_direct.start_execution(spec).unwrap();
    let mut panicking_handle: tokio::task::JoinHandle<marsh_mcp::export::TaskOutput> =
        tokio::task::spawn(async {
            panic!("simulated task panic for join error test");
        });
    let output =
        marsh_mcp::export::test_escalate_and_drain(&execution, &mut panicking_handle).await;
    // eof is the 7th element: must be true on join error to guarantee uncertain cleanup certainty
    assert!(
        output.6,
        "early_eof must be true on join error to guarantee uncertain cleanup"
    );
    assert!(
        String::from_utf8_lossy(&output.2).contains("join error"),
        "stderr must note the join error"
    );
}
