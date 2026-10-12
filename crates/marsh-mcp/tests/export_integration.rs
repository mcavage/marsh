//! Export-only MCP server mode over real MCP JSON-RPC and the real daemon Unix
//! socket protocol. Only the daemon backend (the Kit side) is a test double.

use marsh_daemon::{
    AttachmentFrame, DaemonBackend, DaemonError, DaemonStore, ExecuteSpec, LoadSelection,
    PreparationProgress, PreparationResult, Server, ServerAttachment, SessionSpec, ShellSpec,
};
use marsh_mcp::{
    ArgBinding, ExportConfig, ExportMcp, HostConfig, StdinBinding, ToolBindings, ToolDeclaration,
    WorkspaceIdentity,
};
use rmcp::{
    ClientHandler, RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult, ClientConfig},
    service::RunningService,
};
use serde_json::{Value, json};
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
use tokio::sync::Notify;

#[derive(Clone, Debug, Default)]
struct RecordedExecution {
    command: String,
    arguments: Vec<Vec<u8>>,
    stdin: Vec<u8>,
    guest_home: PathBuf,
    home_backing: PathBuf,
}

/// What the Kit job does once its stdin is complete.
enum Job {
    /// Writes the injected output, records a receipt, then exits.
    Complete,
    /// Exits 0 without recording a receipt.
    ExitWithoutReceipt,
    /// Drops the attachment without an exit frame.
    DisconnectEarly,
    /// Runs until the attachment delivers a signal it obeys.
    HoldUntilSignalled,
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
    job: Job,
    ignore_sigint: bool,
    received_signals: Arc<Mutex<Vec<String>>>,
    started: Arc<Notify>,
    signalled: Arc<Notify>,
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
            job: Job::Complete,
            ignore_sigint: false,
            received_signals: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(Notify::new()),
            signalled: Arc::new(Notify::new()),
            decoy_session_after_own_job: Arc::new(Mutex::new(None)),
        }
    }

    fn signals(&self) -> Vec<String> {
        self.received_signals.lock().unwrap().clone()
    }

    /// Records `signal`; returns true when the job ends because of it.
    fn on_signal(&self, attachment: &ServerAttachment, signal: &str) -> bool {
        let obeyed = !self.ignore_sigint || signal != "SIGINT";
        self.received_signals
            .lock()
            .unwrap()
            .push(signal.to_owned());
        self.signalled.notify_one();
        if obeyed {
            let _ = attachment.send(&AttachmentFrame::Stderr {
                bytes: format!("interrupted by {signal}").into_bytes(),
            });
            let _ = attachment.send(&AttachmentFrame::Exited { code: 130 });
        }
        obeyed
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
                AttachmentFrame::Stdin { bytes } => stdin_bytes.extend_from_slice(&bytes),
                AttachmentFrame::StdinEof => break,
                AttachmentFrame::Signal { signal } if self.on_signal(&attachment, &signal) => {
                    return Ok(());
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
        self.started.notify_one();

        match self.job {
            Job::Complete => {}
            Job::ExitWithoutReceipt => {
                return attachment.send(&AttachmentFrame::Exited { code: 0 });
            }
            Job::DisconnectEarly => {
                drop(attachment);
                return Ok(());
            }
            Job::HoldUntilSignalled => loop {
                match attachment.receive() {
                    Ok(AttachmentFrame::Signal { signal })
                        if self.on_signal(&attachment, &signal) =>
                    {
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(_) => return Ok(()),
                }
            },
        }

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

/// A real daemon server on a private home, plus the host executables an
/// export server pins.
struct TestHarness {
    temp: tempfile::TempDir,
    workspace: PathBuf,
    home: PathBuf,
    marsh: PathBuf,
    sbx: PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl TestHarness {
    fn new(backend: Arc<dyn DaemonBackend>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = |name: &str| {
            let path = temp.path().join(name);
            fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let marsh = executable("marsh");
        executable("marshd");
        let sbx = executable("sbx");

        // The socket is bound before the serving thread starts.
        let server = Server::bind(&home).unwrap().with_backend(backend);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let server = thread::spawn(move || {
            server
                .serve_until(|| stopping.load(Ordering::Acquire))
                .unwrap();
        });
        Self {
            temp,
            workspace,
            home,
            marsh,
            sbx,
            stop,
            server: Some(server),
        }
    }

    fn host_config(&self) -> HostConfig {
        HostConfig::new(&self.workspace, &self.home, &self.marsh, &self.sbx, false).unwrap()
    }

    /// Serves `declaration` over a Unix socket and connects an MCP client.
    async fn connect(&self, declaration: ToolDeclaration) -> Connection {
        let server = ExportMcp::new(ExportConfig::new(self.host_config(), declaration).unwrap());
        let (server_socket, client_socket) = tokio::net::UnixStream::pair().unwrap();
        let serving = server.clone();
        let task = tokio::spawn(async move {
            let _ = serving.serve(server_socket).await.unwrap().waiting().await;
        });
        let client = TestClient.serve(client_socket).await.unwrap();
        Connection {
            client,
            server,
            _task: task,
        }
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = self.server.take()
            && server.join().is_err()
            && !thread::panicking()
        {
            panic!("daemon server thread panicked");
        }
    }
}

struct Connection {
    client: RunningService<RoleClient, TestClient>,
    server: ExportMcp,
    _task: tokio::task::JoinHandle<()>,
}

impl Connection {
    async fn call(&self, arguments: Value) -> CallToolResult {
        self.client.call_tool(call_params(arguments)).await.unwrap()
    }
}

fn call_params(arguments: Value) -> CallToolRequestParams {
    let Value::Object(arguments) = arguments else {
        panic!("tool arguments must be an object: {arguments}");
    };
    CallToolRequestParams::new("project_test").with_arguments(arguments)
}

fn structured(result: &CallToolResult) -> &Value {
    result.structured_content.as_ref().unwrap()
}

fn error_text(result: &CallToolResult) -> &str {
    structured(result)["error"].as_str().unwrap()
}

async fn notified(notify: &Notify, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), notify.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
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

fn pipeline_declaration(harness: &TestHarness) -> ToolDeclaration {
    let mut declaration = make_declaration("");
    declaration.pipeline = Some("printf safe".into());
    declaration.publication_generation = Some("00000000-0000-4000-8000-000000000001".into());
    declaration.bindings.argv.clear();
    declaration.bindings.stdin = None;
    declaration.input_schema =
        json!({"type": "object", "properties": {}, "additionalProperties": false});
    declaration.canonical_workspace = Some(harness.workspace.clone());
    declaration.workspace_identity =
        Some(WorkspaceIdentity::for_directory(&harness.workspace).unwrap());
    declaration
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
    let harness = TestHarness::new(Arc::new(TestKitBackend::new(vec!["test-report".into()])));
    let connection = harness.connect(make_declaration("test-report")).await;
    let tools = connection.client.list_all_tools().await.unwrap();

    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "project_test");
    assert_eq!(tools[0].description.as_deref(), Some("Run project tests"));
}

#[tokio::test]
async fn published_pipeline_rejects_replaced_project_for_live_and_restarted_clients() {
    let harness = TestHarness::new(Arc::new(TestKitBackend::new(vec![])));
    fs::write(&harness.marsh, b"#!/bin/sh\nprintf ran > \"$PWD/marker\"\n").unwrap();
    let declaration = pipeline_declaration(&harness);
    let connection = harness.connect(declaration.clone()).await;

    let moved = harness.temp.path().join("moved-project");
    let alternate = harness.temp.path().join("alternate-project");
    fs::rename(&harness.workspace, &moved).unwrap();
    fs::create_dir(&alternate).unwrap();
    std::os::unix::fs::symlink(&alternate, &harness.workspace).unwrap();

    let response = connection.call(json!({})).await;
    assert_eq!(response.is_error, Some(true));
    assert!(!alternate.join("marker").exists());
    assert!(ExportConfig::new(harness.host_config(), declaration).is_err());

    fs::remove_file(&harness.workspace).unwrap();
    fs::create_dir(&harness.workspace).unwrap();
    let response = connection.call(json!({})).await;
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
    let declaration = pipeline_declaration(&harness);
    let identity = declaration.workspace_identity.unwrap();
    let connection = harness.connect(declaration).await;
    let response = connection.call(json!({})).await;
    let result = structured(&response);
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
    let harness = TestHarness::new(Arc::new(TestKitBackend::new(vec!["test-report".into()])));
    let connection = harness.connect(make_declaration("test-report")).await;
    for forbidden in ["shell_run", "qualify", "doctor", "unknown_tool"] {
        let result = connection
            .client
            .call_tool(CallToolRequestParams::new(forbidden))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(structured(&result)["ok"], false);
        assert!(error_text(&result).contains("export-only server exposes only 'project_test'"));
    }
}

/// MCP-01 and MCP-02: arguments outside the declared schema, or that would
/// become options, never reach the daemon.
#[tokio::test]
async fn invalid_arguments_are_rejected_before_any_job() {
    for (arguments, error) in [
        (
            json!({"filter": "test1", "injected_unpermitted_field": "dangerous"}),
            "unknown field 'injected_unpermitted_field' is not permitted",
        ),
        (
            json!({"filter": "--injected-flag"}),
            "cannot start with '-' (option injection prevented)",
        ),
    ] {
        let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
        let harness = TestHarness::new(backend.clone());
        let connection = harness.connect(make_declaration("test-report")).await;
        let result = connection.call(arguments).await;
        assert_eq!(result.is_error, Some(true), "{error}");
        assert_eq!(structured(&result)["ok"], false, "{error}");
        assert!(error_text(&result).contains(error), "{result:?}");
        assert!(backend.last_execution.lock().unwrap().is_none(), "{error}");
    }
}

#[tokio::test]
async fn test_call_tool_rejects_unregistered_command_mcp03() {
    // The daemon registers "other-cmd"; the declaration demands "test-report".
    let harness = TestHarness::new(Arc::new(TestKitBackend::new(vec!["other-cmd".into()])));
    let connection = harness.connect(make_declaration("test-report")).await;
    let result = connection.call(json!({"filter": "valid_test"})).await;
    assert_eq!(result.is_error, Some(true));
    assert_eq!(structured(&result)["ok"], false);
    assert!(error_text(&result).contains("command 'test-report' is not registered"));
}

#[tokio::test]
async fn test_call_tool_success_executes_real_daemon_path() {
    let backend = Arc::new(TestKitBackend::new(vec!["test-report".into()]));
    let harness = TestHarness::new(backend.clone());
    let connection = harness.connect(make_declaration("test-report")).await;
    let result = connection
        .call(json!({
            "filter": "unit_suite",
            "coverage": true,
            "input_data": "sample payload"
        }))
        .await;

    assert_eq!(result.is_error, Some(false));
    let structured = structured(&result);
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

    // The daemon receives exact literal argv and stdin, without shell interpolation.
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
        let connection = harness.connect(make_declaration("test-report")).await;
        let result = connection
            .call(json!({"filter":"typed", "input_data":"probe"}))
            .await;
        let data = &structured(&result)["data"];
        assert_eq!(data["exit_code"], 125);
        assert_eq!(data["execution"], observed);
        assert!(data["job_id"].is_string());
    }
}

#[tokio::test]
async fn test_call_tool_nonzero_exit_code_mcp04() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.injected_exit_code = 42;
    backend.injected_stderr = b"test error logged\n".to_vec();
    let harness = TestHarness::new(Arc::new(backend));
    let connection = harness.connect(make_declaration("test-report")).await;
    let result = connection.call(json!({"filter": "failing_test"})).await;

    assert_eq!(result.is_error, Some(true));
    let data = &structured(&result)["data"];
    assert_eq!(data["outcome"], "exit_nonzero");
    assert_eq!(data["exit_code"], 42);
    assert_eq!(data["stderr"], "test error logged\n");
}

#[tokio::test]
async fn test_call_tool_truncates_oversized_output() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.injected_stdout = vec![b'A'; 2048]; // max_output_bytes is 1024
    let harness = TestHarness::new(Arc::new(backend));
    let connection = harness.connect(make_declaration("test-report")).await;
    let result = connection
        .call(json!({"filter": "large_output_test"}))
        .await;

    let data = &structured(&result)["data"];
    assert_eq!(data["stdout_truncated"], true);
    assert_eq!(data["output_complete"], false);
    assert_eq!(data["stdout"].as_str().unwrap().len(), 1024);
}

#[tokio::test]
async fn test_early_eof_marks_failed_uncertain() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.job = Job::DisconnectEarly;
    let harness = TestHarness::new(Arc::new(backend));
    let connection = harness.connect(make_declaration("test-report")).await;
    let result = connection.call(json!({"filter": "test1"})).await;

    let data = &structured(&result)["data"];
    assert_eq!(data["outcome"], "failed");
    assert_eq!(data["cleanup_certainty"], "uncertain");
    assert_eq!(data["output_complete"], false);
}

#[tokio::test]
async fn test_exit_zero_without_receipt_is_not_success() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.job = Job::ExitWithoutReceipt;
    let harness = TestHarness::new(Arc::new(backend));
    let connection = harness.connect(make_declaration("test-report")).await;
    let result = connection.call(json!({ "filter": "test1" })).await;
    assert_eq!(result.is_error, Some(true));
    let data = &structured(&result)["data"];
    assert_eq!(data["outcome"], "failed");
    assert_eq!(data["cleanup_certainty"], "uncertain");
    assert_eq!(data["output_complete"], false);
    assert!(data["job_id"].is_null());
}

fn run_decoy_job(
    client: &marsh_daemon::Client,
    session_id: &str,
    username: &str,
    harness: &TestHarness,
) {
    let spec = marsh_daemon::ExecuteSpec {
        process: None,
        placement: marsh_daemon::Placement::Local,
        environment: std::collections::BTreeMap::new(),
        command: "test-report".to_string(),
        arguments: vec![b"--decoy-1".to_vec()],
        working_directory: None,
        session: marsh_daemon::SessionSpec {
            session_id: session_id.to_string(),
            username: username.to_string(),
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

    // Attach a separate shell session to produce decoy jobs.
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
    run_decoy_job(&client_direct, &other_session, &username, &harness);
    *backend.decoy_session_after_own_job.lock().unwrap() = Some(other_session.clone());

    let connection = harness.connect(make_declaration("test-report")).await;
    let result = connection.call(json!({"filter": "test_receipt"})).await;
    let job_id = structured(&result)["data"]["job_id"].as_str().unwrap();

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

    // The bound job belongs to the session created for this call.
    let receipt = client_direct.job(job_id.to_string()).unwrap();
    assert_ne!(
        receipt.session_id, other_session,
        "receipt must bind to exact session, not decoy"
    );
    assert_eq!(receipt.job_id, job_id);
    assert_eq!(receipt.command, "test-report");
}

/// A job still running at the declared timeout is interrupted, then
/// terminated when it ignores the interrupt.
#[tokio::test]
async fn timeout_interrupts_then_terminates_a_job_that_ignores_interrupt() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.job = Job::HoldUntilSignalled;
    backend.ignore_sigint = true;
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let mut declaration = make_declaration("test-report");
    declaration.timeout_ms = 200;
    let connection = harness.connect(declaration).await;
    let result = connection.call(json!({"filter": "timeout_test"})).await;

    let data = &structured(&result)["data"];
    assert_eq!(data["outcome"], "timeout");
    assert_eq!(data["output_complete"], false);
    assert_eq!(backend.signals(), ["SIGINT", "SIGTERM"]);
}

/// Server shutdown cancels an in-flight call: the job is interrupted and the
/// caller receives a cancelled result.
#[tokio::test]
async fn shutdown_interrupts_an_in_flight_job_and_reports_cancelled() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.job = Job::HoldUntilSignalled;
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let Connection { client, server, .. } = harness.connect(make_declaration("test-report")).await;
    let call = tokio::spawn(async move {
        client
            .call_tool(call_params(json!({"filter": "cancel_test"})))
            .await
            .unwrap()
    });
    notified(&backend.started, "the job to start").await;

    server.shutdown_server().await.unwrap();

    let result = call.await.unwrap();
    let data = &structured(&result)["data"];
    assert_eq!(data["outcome"], "cancelled");
    assert_eq!(data["output_complete"], false);
    assert_eq!(backend.signals(), ["SIGINT"]);
}

/// A caller that disconnects mid-call does not leave its job running: the
/// export server (as `marsh-mcp` runs it: serve, wait for the transport to
/// close, then shut down) interrupts the job and exits cleanly.
#[tokio::test]
async fn caller_disconnect_interrupts_the_job_and_the_server_exits() {
    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    backend.job = Job::HoldUntilSignalled;
    let backend = Arc::new(backend);
    let harness = TestHarness::new(backend.clone());
    let server = ExportMcp::new(
        ExportConfig::new(harness.host_config(), make_declaration("test-report")).unwrap(),
    );
    let (server_socket, client_socket) = tokio::net::UnixStream::pair().unwrap();
    let server_task = tokio::spawn(async move {
        let running = server.clone().serve(server_socket).await.unwrap();
        let _ = running.waiting().await;
        server.shutdown_server().await
    });
    let client = TestClient.serve(client_socket).await.unwrap();
    let call = tokio::spawn(async move {
        let _ = client
            .call_tool(call_params(json!({"filter": "disconnect_test"})))
            .await;
    });
    notified(&backend.started, "the job to start").await;

    // Dropping the running client closes its transport.
    call.abort();
    let _ = call.await;

    notified(&backend.signalled, "the job to be interrupted").await;
    let shutdown = tokio::time::timeout(Duration::from_secs(30), server_task)
        .await
        .expect("export server did not exit after the caller disconnected")
        .unwrap();
    assert_eq!(shutdown, Ok(()));
    assert_eq!(backend.signals(), ["SIGINT"]);
}

/// A declared `kit_identity` binds the call to the daemon's registered Kit,
/// before execution and again against the job receipt.
#[tokio::test]
async fn declared_kit_identity_is_enforced_before_and_after_execution() {
    const KIT: &str = "local-v3:/path/to/test-kit";
    struct Case {
        name: &'static str,
        registered: Option<&'static str>,
        query_fails: bool,
        receipt: Option<&'static str>,
        error: Option<&'static str>,
        executes: bool,
    }
    for case in [
        Case {
            name: "match",
            registered: Some(KIT),
            query_fails: false,
            receipt: None,
            error: None,
            executes: true,
        },
        Case {
            name: "sibling path",
            registered: Some("local-v3:/path/to/test-kit-sibling"),
            query_fails: false,
            receipt: None,
            error: Some("does not match declared kit_identity"),
            executes: false,
        },
        Case {
            name: "registry query fails",
            registered: None,
            query_fails: true,
            receipt: None,
            error: Some("cannot query registered kits from daemon"),
            executes: false,
        },
        Case {
            name: "command has no Kit",
            registered: None,
            query_fails: false,
            receipt: None,
            error: Some("has no registered kit identity in daemon"),
            executes: false,
        },
        Case {
            name: "receipt names another Kit",
            registered: Some(KIT),
            query_fails: false,
            receipt: Some("local-v3:/path/to/unexpected-kit"),
            error: Some("does not match declared kit_identity"),
            executes: true,
        },
    ] {
        let name = case.name;
        let mut backend = TestKitBackend::new(vec!["test-report".into()]);
        if let Some(kit) = case.registered {
            backend
                .registered_kits
                .insert("test-report".into(), kit.into());
        }
        backend.registered_kits_error = case.query_fails;
        backend.override_receipt_kit_ref = case.receipt.map(Into::into);
        let backend = Arc::new(backend);
        let harness = TestHarness::new(backend.clone());
        let mut declaration = make_declaration("test-report");
        declaration.kit_identity = Some(KIT.into());
        let connection = harness.connect(declaration).await;
        let result = connection.call(json!({"filter": "kit_test"})).await;
        let structured = structured(&result);

        assert_eq!(
            backend.last_execution.lock().unwrap().is_some(),
            case.executes,
            "{name}"
        );
        match (case.error, case.executes) {
            (None, _) => assert_eq!(structured["ok"], true, "{name}: {structured}"),
            (Some(error), false) => {
                assert_eq!(result.is_error, Some(true), "{name}");
                assert_eq!(structured["ok"], false, "{name}");
                assert!(error_text(&result).contains(error), "{name}: {structured}");
            }
            (Some(error), true) => {
                assert_eq!(result.is_error, Some(true), "{name}");
                let data = &structured["data"];
                assert_eq!(data["outcome"], "failed", "{name}");
                assert!(data["job_id"].is_string(), "{name}: must return job id");
                assert!(
                    data["stderr"].as_str().unwrap().contains(error),
                    "{name}: {structured}"
                );
            }
        }
    }
}

/// The largest permitted output (256 KiB on each stream, dense in multi-byte
/// UTF-8 and JSON escapes) is delivered complete in a response frame of at
/// most 1 MiB, measured on the wire.
#[tokio::test]
async fn maximum_output_is_delivered_complete_within_one_mib_frame() {
    use base64::Engine as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut backend = TestKitBackend::new(vec!["test-report".into()]);
    let pattern = "🦀🚀✨\"\\/\n\t\r\x0c你好世界приветαβγδε";
    let large = pattern.repeat(262_144 / pattern.len() + 1).into_bytes();
    backend.injected_stdout = large[..262_144].to_vec();
    backend.injected_stderr = large[..262_144].to_vec();
    let expected = backend.injected_stdout.clone();
    let harness = TestHarness::new(Arc::new(backend));
    let mut declaration = make_declaration("test-report");
    declaration.max_output_bytes = 262_144;
    let server = ExportMcp::new(ExportConfig::new(harness.host_config(), declaration).unwrap());

    // server <-> relay <-> client; the relay records each server frame's size.
    let (server_socket, mut relay_server) = tokio::net::UnixStream::pair().unwrap();
    let (mut relay_client, client_socket) = tokio::net::UnixStream::pair().unwrap();
    let largest = Arc::new(Mutex::new(0_usize));
    let recorded = Arc::clone(&largest);
    let _server = tokio::spawn(async move {
        let _ = server.serve(server_socket).await.unwrap().waiting().await;
    });
    let _relay = tokio::spawn(async move {
        let (mut from_server, mut to_server) = relay_server.split();
        let (mut from_client, mut to_client) = relay_client.split();
        let upstream = tokio::io::copy(&mut from_client, &mut to_server);
        let downstream = async {
            let (mut buffer, mut frame) = (vec![0_u8; 65_536], 0_usize);
            loop {
                let count = from_server.read(&mut buffer).await?;
                if count == 0 {
                    return Ok::<_, std::io::Error>(());
                }
                for &byte in &buffer[..count] {
                    if byte == b'\n' {
                        let mut largest = recorded.lock().unwrap();
                        *largest = (*largest).max(frame);
                        frame = 0;
                    } else {
                        frame += 1;
                    }
                }
                to_client.write_all(&buffer[..count]).await?;
            }
        };
        let _ = tokio::join!(upstream, downstream);
    });
    let client = TestClient.serve(client_socket).await.unwrap();
    let result = client
        .call_tool(call_params(json!({"filter": "dual_max_test"})))
        .await
        .unwrap();

    let data = &structured(&result)["data"];
    assert_eq!(data["stdout_truncated"], false);
    assert_eq!(data["stderr_truncated"], false);
    assert_eq!(data["output_complete"], true);
    let stdout = base64::engine::general_purpose::STANDARD
        .decode(data["stdout_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(stdout, expected);
    let largest = *largest.lock().unwrap();
    assert!(
        largest <= 1_048_576,
        "response frame of {largest} bytes exceeds 1 MiB"
    );
}
