#[path = "byte_carrier_tests.rs"]
mod byte_bridge_cases;

use super::*;
use marsh_contracts::{JobIdentity, JobMount, JobResources, MountAccess, OciImage};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Cursor},
    os::unix::net::UnixListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct FakeRunner {
    outputs: Mutex<VecDeque<CommandOutput>>,
    invocations: Mutex<Vec<Invocation>>,
    pty_sizes: Mutex<Vec<TerminalSize>>,
}

impl FakeRunner {
    fn with_outputs(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
        Self {
            outputs: Mutex::new(outputs.into_iter().collect()),
            invocations: Mutex::new(Vec::new()),
            pty_sizes: Mutex::new(Vec::new()),
        }
    }

    fn with_create_outputs(outputs: impl IntoIterator<Item = CommandOutput>) -> Self {
        Self::with_outputs(
            std::iter::once(byte_bridge_cases::inspected(
                &job(),
                serde_json::json!(["/entry"]),
                serde_json::json!(["default"]),
            ))
            .chain(outputs),
        )
    }

    fn lifecycle_calls(&self) -> Vec<Vec<String>> {
        let calls = self.calls();
        assert_eq!(
            calls[0],
            [
                "image",
                "inspect",
                "--format",
                "{{json .}}",
                "--",
                job().image.as_str()
            ]
        );
        calls[1..].to_vec()
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.invocations
            .lock()
            .unwrap()
            .iter()
            .map(|invocation| {
                invocation
                    .arguments
                    .iter()
                    .map(|argument| argument.to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }
}

// Completed in-memory argv fixture only: Cursor/Sink operations cannot block.
// Actual cancellation and reaping are exercised by the owned process callers.
struct FakeAttachedProcess(i32);

impl AttachedProcess for FakeAttachedProcess {
    fn supports_io_cancellation(&self) -> bool {
        true
    }
    fn cancel_io(&self) -> io::Result<()> {
        Ok(())
    }
    fn wait(&mut self) -> io::Result<i32> {
        Ok(self.0)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(Some(self.0))
    }

    fn terminate(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        self.invocations.lock().unwrap().push(invocation.clone());
        self.outputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| io::Error::other("unexpected fake invocation"))
    }

    fn run_bounded(
        &self,
        invocation: &Invocation,
        _timeout: Duration,
    ) -> io::Result<CommandOutput> {
        self.run(invocation)
    }

    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment> {
        self.invocations.lock().unwrap().push(invocation.clone());
        let output =
            if invocation
                .arguments
                .first()
                .is_some_and(|argument| argument == "start")
            {
                success(Vec::new())
            } else {
                self.outputs.lock().unwrap().pop_front().ok_or_else(|| {
                    io::Error::other("unexpected fake attached control invocation")
                })?
            };
        Ok(Attachment {
            stdin: Box::new(io::sink()),
            stdout: Box::new(Cursor::new(output.stdout)),
            stderr: Box::new(Cursor::new(output.stderr)),
            process: Box::new(FakeAttachedProcess(output.exit_code.unwrap_or(125))),
            control: no_attachment_control(),
        })
    }

    fn spawn_pty_sized(
        &self,
        invocation: &Invocation,
        size: TerminalSize,
    ) -> io::Result<Attachment> {
        self.pty_sizes.lock().unwrap().push(size);
        self.spawn_attached(invocation)
    }
}

fn engine_response(
    response: &'static [u8],
) -> (tempfile::TempDir, PathBuf, thread::JoinHandle<Vec<u8>>) {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("engine.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream.write_all(response).unwrap();
        request
    });
    (directory, socket, server)
}

fn success(stdout: impl Into<Vec<u8>>) -> CommandOutput {
    CommandOutput {
        exit_code: Some(0),
        stdout: stdout.into(),
        stderr: Vec::new(),
    }
}

#[test]
fn command_failure_preserves_bounded_sanitized_stderr_and_exit_code() {
    let mut stderr = b"manifest rejected\0: ".to_vec();
    stderr.extend(vec![b'x'; MAX_COMMAND_STDERR * 2]);
    let runner = FakeRunner::with_create_outputs([CommandOutput {
        exit_code: Some(125),
        stdout: Vec::new(),
        stderr,
    }]);
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");
    let error = runtime.create(&job()).unwrap_err();
    let RuntimeError::CommandFailed {
        operation,
        exit_code,
        stderr,
    } = error
    else {
        panic!("unexpected runtime error")
    };
    assert_eq!(operation, "create");
    assert_eq!(exit_code, Some(125));
    assert!(stderr.starts_with("manifest rejected�: "));
    assert!(stderr.len() <= MAX_COMMAND_STDERR);
}

fn job() -> JobSpec {
    JobSpec {
        image: OciImage::parse(format!("registry.example/agent@sha256:{}", "a".repeat(64)))
            .unwrap(),
        argv: vec![b"$(touch /tmp/must-not-execute)".to_vec()],
        identity: JobIdentity { uid: 501, gid: 20 },
        session_environment: BTreeMap::from([
            ("HOME".into(), "/Users/alice".into()),
            ("USER".into(), "alice".into()),
            ("LOGNAME".into(), "alice".into()),
            ("MARSH_SELECTED_HOME".into(), "/Users/alice".into()),
        ]),
        exported_environment: BTreeMap::new(),
        working_directory: "/Users/alice/dev/project".into(),
        mounts: vec![
            JobMount {
                source: "/run/marsh/grants/attempt-1/project".into(),
                target: "/Users/alice/dev/project".into(),
                access: MountAccess::ReadWrite,
                subpath: None,
            },
            JobMount {
                source: "/run/marsh/grants/attempt-1/home".into(),
                target: "/Users/alice".into(),
                access: MountAccess::ReadWrite,
                subpath: None,
            },
        ],
        resources: JobResources {
            cpu_millis: 1500,
            memory_bytes: 536_870_912,
            pids: 96,
            writable_bytes: 4_294_967_296,
            output_bytes: 67_108_864,
            wall_seconds: 900,
        },
        terminal: true,
        terminal_size: Some(TerminalSize {
            rows: 47,
            columns: 123,
        }),
        split_capability: None,
        capability: None,
    }
}

#[test]
fn nested_job_receives_only_exact_stock_sbx_proxy_plumbing() {
    let ca = tempfile::NamedTempFile::new().unwrap();
    let owner = ca.as_file().metadata().unwrap().uid();
    let runner = FakeRunner::with_create_outputs([success(format!("{}\n", "b".repeat(64)))]);
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker").with_sbx_environment(
        vec![
            (
                "HTTP_PROXY".into(),
                "http://gateway.docker.internal:3128".into(),
            ),
            (
                "HTTPS_PROXY".into(),
                "http://gateway.docker.internal:3128".into(),
            ),
            ("NODE_USE_ENV_PROXY".into(), "1".into()),
            ("SBX_CRED_ANTHROPIC_MODE".into(), "apikey".into()),
            ("SBX_CRED_OPENAI_MODE".into(), "oauth".into()),
        ],
        ca.path(),
        owner,
    );

    runtime.create(&job()).unwrap();
    let call = &runtime.runner.lifecycle_calls()[0];
    for expected in [
        "HTTP_PROXY=http://gateway.docker.internal:3128",
        "HTTPS_PROXY=http://gateway.docker.internal:3128",
        "NODE_USE_ENV_PROXY=1",
        "SBX_CRED_ANTHROPIC_MODE=apikey",
        "SBX_CRED_OPENAI_MODE=oauth",
    ] {
        assert!(call.windows(2).any(|pair| pair == ["--env", expected]));
    }
    for name in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE"] {
        let expected = format!("{name}={}", ca.path().display());
        assert!(call.windows(2).any(|pair| pair == ["--env", &expected]));
    }
    assert!(call.contains(&format!(
        "type=bind,source={},target={},readonly",
        ca.path().display(),
        ca.path().display()
    )));
    assert!(!call.iter().any(|argument| {
        argument.starts_with("ANTHROPIC_API_KEY=")
            || argument.starts_with("OPENAI_API_KEY=")
            || argument.starts_with("NO_PROXY=")
            || argument.starts_with("PROXY_CA_CERT_B64=")
            || argument.contains("docker.sock")
    }));
}

#[test]
fn nested_job_receives_exported_shell_values_after_runtime_proxy_values() {
    let runner = FakeRunner::with_create_outputs([success(format!("{}\n", "b".repeat(64)))]);
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");
    let mut spec = job();
    spec.exported_environment
        .insert("MY_SETTING".into(), "per-command".into());
    runtime.create(&spec).unwrap();
    let call = &runtime.runner.lifecycle_calls()[0];
    assert!(
        call.windows(2)
            .any(|pair| pair == ["--env", "MY_SETTING=per-command"])
    );
    assert!(
        call.windows(2)
            .any(|pair| pair == ["--env", "HOME=/Users/alice"])
    );
}

#[test]
fn nested_job_receives_stock_sbx_mcp_gateway_pair() {
    let ca = tempfile::NamedTempFile::new().unwrap();
    let owner = ca.as_file().metadata().unwrap().uid();
    let runner = FakeRunner::with_create_outputs([success(format!("{}\n", "b".repeat(64)))]);
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker").with_sbx_environment(
        vec![
            (
                "HTTP_PROXY".into(),
                "http://gateway.docker.internal:3128".into(),
            ),
            (
                "HTTPS_PROXY".into(),
                "http://gateway.docker.internal:3128".into(),
            ),
            (
                "MCP_GATEWAY_URL".into(),
                "http://mcp-gateway.docker.internal/mcp".into(),
            ),
            ("MCP_SENTINEL_TOKEN_NAME".into(), "proxy-managed".into()),
        ],
        ca.path(),
        owner,
    );

    runtime.create(&job()).unwrap();
    let call = &runtime.runner.lifecycle_calls()[0];
    for expected in [
        "MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp",
        "MCP_SENTINEL_TOKEN_NAME=proxy-managed",
    ] {
        assert!(call.windows(2).any(|pair| pair == ["--env", expected]));
    }
}

#[test]
fn nested_job_rejects_forged_or_incomplete_mcp_gateway_before_create() {
    for (environment, offending) in [
        (
            vec![("MCP_GATEWAY_URL", "http://mcp-gateway.docker.internal/mcp")],
            "MCP_SENTINEL_TOKEN_NAME",
        ),
        (
            vec![("MCP_SENTINEL_TOKEN_NAME", "proxy-managed")],
            "MCP_GATEWAY_URL",
        ),
        (
            vec![
                ("MCP_GATEWAY_URL", "http://mcp-gateway.docker.internal/mcp"),
                ("MCP_SENTINEL_TOKEN_NAME", "proxy-managed"),
            ],
            "HTTP_PROXY",
        ),
        (
            vec![
                ("MCP_GATEWAY_URL", "http://evil.example/mcp"),
                ("MCP_SENTINEL_TOKEN_NAME", "mcp-gateway"),
            ],
            "MCP_GATEWAY_URL",
        ),
        (
            vec![
                (
                    "MCP_GATEWAY_URL",
                    "http://mcp-gateway.docker.internal@evil.example/mcp",
                ),
                ("MCP_SENTINEL_TOKEN_NAME", "mcp-gateway"),
            ],
            "MCP_GATEWAY_URL",
        ),
        (
            vec![
                (
                    "MCP_GATEWAY_URL",
                    "http://mcp-gateway.docker.internal/mcp?token=x",
                ),
                ("MCP_SENTINEL_TOKEN_NAME", "mcp-gateway"),
            ],
            "MCP_GATEWAY_URL",
        ),
        (
            vec![
                (
                    "MCP_GATEWAY_URL",
                    "http://mcp-gateway.docker.internal:8877/mcp",
                ),
                ("MCP_SENTINEL_TOKEN_NAME", "mcp-gateway"),
            ],
            "MCP_GATEWAY_URL",
        ),
        (
            vec![
                ("MCP_GATEWAY_URL", "http://mcp-gateway.docker.internal/mcp"),
                ("MCP_SENTINEL_TOKEN_NAME", ""),
            ],
            "MCP_SENTINEL_TOKEN_NAME",
        ),
        (
            vec![
                ("MCP_GATEWAY_URL", "http://mcp-gateway.docker.internal/mcp"),
                ("MCP_SENTINEL_TOKEN_NAME", "secret\nkey"),
            ],
            "MCP_SENTINEL_TOKEN_NAME",
        ),
        (
            vec![
                ("MCP_GATEWAY_URL", "http://mcp-gateway.docker.internal/mcp"),
                ("MCP_SENTINEL_TOKEN_NAME", "mcp-gateway"),
                ("MCP_SENTINEL_TOKEN_NAME", "mcp-gateway"),
            ],
            "MCP_SENTINEL_TOKEN_NAME",
        ),
    ] {
        let runner = FakeRunner::default();
        let runtime = DockerCliRuntime::new(runner, "/trusted/docker").with_sbx_environment(
            environment
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            "/nonexistent/ca.crt",
            0,
        );
        let error = runtime.create(&job()).unwrap_err();
        assert!(
            matches!(&error, RuntimeError::InvalidSbxEnvironment { variable } if variable == offending),
            "{error}"
        );
        assert!(runtime.runner.calls().is_empty());
        assert!(!error.to_string().contains("secret\nkey"));
    }
}

#[test]
fn nested_job_rejects_incomplete_or_expanded_proxy_authority() {
    for (environment, offending) in [
        (
            vec![("HTTP_PROXY".into(), "http://proxy:3128".into())],
            "HTTP_PROXY",
        ),
        (
            vec![(
                "HTTP_PROXY".into(),
                "http://gateway.docker.internal:3128".into(),
            )],
            "HTTPS_PROXY",
        ),
        (
            vec![
                (
                    "HTTP_PROXY".into(),
                    "http://token@gateway.docker.internal:3128/?secret".into(),
                ),
                (
                    "HTTPS_PROXY".into(),
                    "http://gateway.docker.internal:3128".into(),
                ),
            ],
            "HTTP_PROXY",
        ),
        (
            vec![
                (
                    "HTTP_PROXY".into(),
                    "http://gateway.docker.internal:3128".into(),
                ),
                (
                    "HTTPS_PROXY".into(),
                    "http://gateway.docker.internal:3128".into(),
                ),
                ("ANTHROPIC_API_KEY".into(), "must-not-cross".into()),
            ],
            "ANTHROPIC_API_KEY",
        ),
        (
            vec![
                (
                    "HTTP_PROXY".into(),
                    "http://gateway.docker.internal:3128".into(),
                ),
                (
                    "HTTPS_PROXY".into(),
                    "http://gateway.docker.internal:3128".into(),
                ),
                ("NO_PROXY".into(), "localhost,127.0.0.1".into()),
            ],
            "NO_PROXY",
        ),
        (
            vec![
                (
                    "HTTP_PROXY".into(),
                    "http://gateway.docker.internal:3128".into(),
                ),
                (
                    "HTTPS_PROXY".into(),
                    "http://gateway.docker.internal:3128".into(),
                ),
                (
                    "HTTPS_PROXY".into(),
                    "http://gateway.docker.internal:3128".into(),
                ),
            ],
            "HTTPS_PROXY",
        ),
    ] {
        let error = validate_sbx_environment(&environment).unwrap_err();
        assert!(matches!(
            &error,
            RuntimeError::InvalidSbxEnvironment { variable } if variable == offending
        ));
        assert!(error.to_string().contains(offending));
    }
}

#[test]
fn invalid_proxy_variable_diagnostic_escapes_control_bytes() {
    let error = validate_sbx_environment(&[("BAD\nNAME".into(), "value".into())]).unwrap_err();
    let diagnostic = error.to_string();
    assert!(diagnostic.contains(r"BAD\nNAME"));
    assert!(!diagnostic.contains('\n'));
}

#[test]
#[allow(clippy::too_many_lines)] // One exact argv audit covers the complete Docker lifecycle.
fn complete_lifecycle_uses_runtime_identity_and_exact_arguments() {
    let id = "b".repeat(64);
    let runner = FakeRunner::with_create_outputs(
        [success(format!("{id}\n"))]
            .into_iter()
            // Docker is still starting it, then it has already exited: the
            // PID search stops without spending every attempt.
            .chain([
                success(b"0\tcreated\n".to_vec()),
                success(b"0\texited\n".to_vec()),
            ])
            .chain([
                success(b"17\n".to_vec()),
                success(b"false\t17\t4096\n".to_vec()),
                success(Vec::new()),
                success(Vec::new()),
                success(Vec::new()),
                success(Vec::new()),
            ]),
    );
    let (_engine_directory, engine_socket, engine) =
        engine_response(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let runtime =
        DockerCliRuntime::new(runner, "/trusted/docker").with_engine_socket(engine_socket);

    let container = runtime.create(&job()).unwrap();
    assert_eq!(container.as_str(), id);
    let mut attachment = runtime.attach(&container, job().terminal_size).unwrap();
    runtime.start(&container).unwrap();
    assert_eq!(
        runtime.wait(&container).unwrap(),
        RuntimeExit {
            code: 17,
            oom_killed: false,
            pids_max_events: None,
            writable_bytes: 4096,
            writable_exceeded: false,
        }
    );
    runtime.signal(&container, JobSignal::Terminate).unwrap();
    runtime
        .resize(
            &container,
            TerminalSize {
                rows: 48,
                columns: 160,
            },
        )
        .unwrap();
    runtime.delete(&container).unwrap();
    attachment.process.wait().unwrap();
    let resize_request = String::from_utf8(engine.join().unwrap()).unwrap();
    assert!(resize_request.starts_with(&format!(
        "POST /containers/{id}/resize?h=48&w=160 HTTP/1.1\r\n"
    )));
    assert_eq!(
        runtime.runner.pty_sizes.lock().unwrap().as_slice(),
        &[TerminalSize {
            rows: 47,
            columns: 123,
        }]
    );

    let calls = runtime.runner.lifecycle_calls();
    assert_eq!(calls[0][0], "create");
    assert!(calls[0].iter().any(|argument| argument == "--init"));
    assert!(calls[0].windows(2).any(|pair| pair == ["--user", "501:20"]));
    assert!(
        calls[0]
            .windows(2)
            .any(|pair| pair == ["--env", "HOME=/Users/alice"])
    );
    assert!(!calls[0].iter().any(|argument| argument == "--entrypoint"));
    assert!(
        calls[0]
            .windows(2)
            .any(|pair| pair == ["--pids-limit", "96"])
    );
    assert!(
        calls[0]
            .windows(2)
            .any(|pair| pair == ["--memory", "536870912"])
    );
    assert!(calls[0].windows(2).any(|pair| pair == ["--cpus", "1.500"]));
    assert!(
        calls[0]
            .windows(2)
            .any(|pair| pair == ["--storage-opt", "size=4294967296"])
    );
    assert!(
        calls[0].contains(
            &"type=bind,source=/run/marsh/grants/attempt-1/project,target=/Users/alice/dev/project"
                .into()
        )
    );
    let home_mount = calls[0]
        .iter()
        .position(|argument| {
            argument == "type=bind,source=/run/marsh/grants/attempt-1/home,target=/Users/alice"
        })
        .unwrap();
    let project_mount = calls[0]
        .iter()
        .position(|argument| {
            argument
                == "type=bind,source=/run/marsh/grants/attempt-1/project,target=/Users/alice/dev/project"
        })
        .unwrap();
    assert!(home_mount < project_mount);
    assert!(calls[0].contains(&"$(touch /tmp/must-not-execute)".into()));
    let image_index = calls[0]
        .iter()
        .position(|argument| argument == &format!("sha256:{}", "d".repeat(64)))
        .unwrap();
    assert_eq!(calls[0][image_index - 1], "--");
    assert_eq!(calls[0][image_index + 1], "$(touch /tmp/must-not-execute)");
    assert!(
        !calls[0]
            .iter()
            .any(|argument| argument.contains("docker.sock"))
    );
    assert_atomic_start_attach(&calls[1], &id);
    assert_eq!(
        calls[2],
        [
            "container",
            "inspect",
            "--format",
            "{{.State.Pid}}\t{{.State.Status}}",
            &id
        ]
    );
    let wait = 4;
    assert_eq!(calls[wait], ["wait", &id]);
    assert_eq!(
        calls[wait + 1],
        [
            "container",
            "inspect",
            "--size",
            "--format",
            "{{.State.OOMKilled}}\t{{.State.ExitCode}}\t{{.SizeRw}}",
            &id
        ]
    );
    assert_eq!(calls[wait + 2], ["kill", "--signal", "TERM", &id]);
    assert_eq!(calls[wait + 3], ["rm", "--force", &id]);
    assert_eq!(
        calls[wait + 4][..4],
        ["container", "ls", "--all", "--no-trunc"]
    );
}

#[test]
fn terminal_resource_state_is_strictly_parsed() {
    assert_eq!(
        parse_resource_state(b"true\t137\t16777216\n").unwrap(),
        (true, 137, 16_777_216)
    );
    for invalid in [
        b"yes\t137\t1".as_slice(),
        b"false\tbad\t1",
        b"false\t1\tbad",
        b"false\t1\t2\textra",
    ] {
        assert!(matches!(
            parse_resource_state(invalid),
            Err(RuntimeError::InvalidResourceEvidence)
        ));
    }
}

#[test]
fn engine_resize_rejects_non_success_with_bounded_sanitized_body() {
    let (_directory, socket, server) = engine_response(
        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 13\r\n\r\nbad\0resize\n",
    );
    let runtime =
        DockerCliRuntime::new(FakeRunner::default(), "/trusted/docker").with_engine_socket(socket);
    let container = ContainerId::parse("c".repeat(64)).unwrap();

    let error = runtime
        .resize(
            &container,
            TerminalSize {
                rows: 55,
                columns: 144,
            },
        )
        .unwrap_err();
    server.join().unwrap();

    assert!(matches!(
        error,
        RuntimeError::EngineResizeFailed { status: 500, body }
            if body == "bad�resize"
    ));
}

#[test]
fn engine_resize_rejects_malformed_http_status() {
    let (_directory, socket, server) =
        engine_response(b"NOT-HTTP 200 OK\r\nContent-Length: 0\r\n\r\n");
    let runtime =
        DockerCliRuntime::new(FakeRunner::default(), "/trusted/docker").with_engine_socket(socket);

    let error = runtime
        .resize(
            &ContainerId::parse("d".repeat(64)).unwrap(),
            TerminalSize {
                rows: 24,
                columns: 80,
            },
        )
        .unwrap_err();
    server.join().unwrap();

    assert!(matches!(error, RuntimeError::InvalidEngineResponse));
}

#[test]
fn pids_event_evidence_is_strictly_parsed() {
    assert_eq!(parse_pids_max_events("max 3\n").unwrap(), 3);
    for invalid in ["", "max nope\n", "max 1\nmax 2\n", "max\t1\n"] {
        assert!(matches!(
            parse_pids_max_events(invalid),
            Err(RuntimeError::InvalidResourceEvidence)
        ));
    }
}

#[test]
fn successful_exit_does_not_consult_tearing_down_pids_evidence() {
    assert_eq!(
        terminal_pids_evidence(0, Err(RuntimeError::InvalidResourceEvidence)).unwrap(),
        None
    );

    assert!(matches!(
        terminal_pids_evidence(1, Err(RuntimeError::InvalidResourceEvidence)),
        Err(RuntimeError::InvalidResourceEvidence)
    ));
}

struct UnavailableCgroupEvidence;

impl io::Read for UnavailableCgroupEvidence {
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(nix::libc::ENODEV))
    }
}

struct SequencedCgroupEvidence {
    samples: VecDeque<io::Result<Vec<u8>>>,
    current: Cursor<Vec<u8>>,
}

struct CountingCgroupEvidence {
    samples: Arc<AtomicUsize>,
    current: Cursor<Vec<u8>>,
}

struct ExitTransitionCgroupEvidence {
    samples: Arc<AtomicUsize>,
    current: Cursor<Vec<u8>>,
}

impl io::Read for ExitTransitionCgroupEvidence {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.current.read(buffer)
    }
}

impl io::Seek for ExitTransitionCgroupEvidence {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        if position != io::SeekFrom::Start(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected seek",
            ));
        }
        let sample = self.samples.fetch_add(1, Ordering::SeqCst);
        self.current = Cursor::new(if sample == 0 {
            b"max 0\n".to_vec()
        } else {
            b"max 1\n".to_vec()
        });
        Ok(0)
    }
}

impl io::Read for CountingCgroupEvidence {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.current.read(buffer)
    }
}

impl io::Seek for CountingCgroupEvidence {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        if position != io::SeekFrom::Start(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected seek",
            ));
        }
        self.samples.fetch_add(1, Ordering::SeqCst);
        self.current = Cursor::new(b"max 0\n".to_vec());
        Ok(0)
    }
}

impl io::Read for SequencedCgroupEvidence {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.current.read(buffer)
    }
}

impl io::Seek for SequencedCgroupEvidence {
    fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
        if position != io::SeekFrom::Start(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected seek",
            ));
        }
        let sample = self
            .samples
            .pop_front()
            .ok_or_else(|| io::Error::other("sample exhausted"))??;
        self.current = Cursor::new(sample);
        Ok(0)
    }
}

impl io::Seek for UnavailableCgroupEvidence {
    fn seek(&mut self, _position: io::SeekFrom) -> io::Result<u64> {
        Err(io::Error::from_raw_os_error(nix::libc::ENODEV))
    }
}

#[test]
fn nonzero_exit_survives_terminal_cgroup_teardown() {
    let stop = AtomicBool::new(false);
    assert_eq!(
        monitor_pids_events_with_delay(UnavailableCgroupEvidence, &stop, Duration::ZERO).unwrap(),
        None
    );
}

#[test]
fn pids_monitor_retains_the_last_valid_counter() {
    let stop = AtomicBool::new(false);
    let evidence = SequencedCgroupEvidence {
        samples: VecDeque::from([
            Ok(b"max 0\n".to_vec()),
            Ok(b"max 1\n".to_vec()),
            Err(io::Error::from_raw_os_error(nix::libc::ENODEV)),
        ]),
        current: Cursor::new(Vec::new()),
    };
    assert_eq!(
        monitor_pids_events_with_delay(evidence, &stop, Duration::ZERO).unwrap(),
        Some(1)
    );
}

#[test]
fn pids_monitor_is_rate_limited_and_wakes_immediately_on_exit() {
    assert_eq!(RESOURCE_EVENT_POLL_DELAY, Duration::from_millis(50));
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(AtomicUsize::new(0));
    let evidence = CountingCgroupEvidence {
        samples: Arc::clone(&samples),
        current: Cursor::new(Vec::new()),
    };
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        monitor_pids_events_with_delay(evidence, &thread_stop, Duration::from_secs(5))
    });
    let sample_deadline = std::time::Instant::now() + Duration::from_secs(1);
    while samples.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < sample_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(samples.load(Ordering::SeqCst), 1);

    let stopped_at = std::time::Instant::now();
    stop.store(true, Ordering::Release);
    handle.thread().unpark();
    assert_eq!(handle.join().unwrap().unwrap(), Some(0));
    assert!(stopped_at.elapsed() < Duration::from_secs(1));
}

#[test]
fn pids_monitor_takes_a_final_sample_when_the_process_exits_between_polls() {
    let stop = Arc::new(AtomicBool::new(false));
    let samples = Arc::new(AtomicUsize::new(0));
    let evidence = ExitTransitionCgroupEvidence {
        samples: Arc::clone(&samples),
        current: Cursor::new(Vec::new()),
    };
    let thread_stop = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        monitor_pids_events_with_delay(evidence, &thread_stop, Duration::from_secs(5))
    });
    let sample_deadline = std::time::Instant::now() + Duration::from_secs(1);
    while samples.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < sample_deadline {
        thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(samples.load(Ordering::SeqCst), 1);

    stop.store(true, Ordering::Release);
    handle.thread().unpark();
    assert_eq!(handle.join().unwrap().unwrap(), Some(1));
    assert_eq!(samples.load(Ordering::SeqCst), 2);
}

#[test]
fn only_vanished_cgroup_errors_are_optional() {
    assert!(cgroup_evidence_unavailable(&io::Error::new(
        io::ErrorKind::NotFound,
        "removed"
    )));
    assert!(cgroup_evidence_unavailable(&io::Error::from_raw_os_error(
        nix::libc::ENODEV
    )));
    assert!(!cgroup_evidence_unavailable(&io::Error::new(
        io::ErrorKind::PermissionDenied,
        "denied"
    )));
}

fn assert_atomic_start_attach(call: &[String], container: &str) {
    assert_eq!(call, ["start", "--attach", "--interactive", container,]);
}

#[test]
fn fast_output_uses_one_atomic_start_and_attach_invocation() {
    let runner = FakeRunner::default();
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");
    let container = ContainerId::parse("f".repeat(64)).unwrap();

    let _attachment = runtime.attach(&container, job().terminal_size).unwrap();
    runtime.start(&container).unwrap();

    assert_eq!(
        runtime.runner.calls(),
        [vec![
            "start".to_owned(),
            "--attach".to_owned(),
            "--interactive".to_owned(),
            container.as_str().to_owned(),
        ]]
    );
}

#[test]
fn deletion_is_uncertain_while_the_exact_container_remains() {
    let id = "c".repeat(64);
    let runner = FakeRunner::with_outputs(
        [success(Vec::new())]
            .into_iter()
            .chain((0..DELETION_VERIFY_ATTEMPTS).map(|_| success(format!("{id}\n")))),
    );
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");
    let container = ContainerId::parse(id).unwrap();
    assert!(matches!(
        runtime.delete(&container),
        Err(RuntimeError::DeletionUncertain)
    ));
    assert_eq!(runtime.runner.calls().len(), DELETION_VERIFY_ATTEMPTS + 1);
}

#[test]
fn deletion_verification_retries_until_container_disappears() {
    let id = "c".repeat(64);
    let runner = FakeRunner::with_outputs([
        success(Vec::new()),
        success(format!("{id}\n")),
        success(format!("{id}\n")),
        success(Vec::new()),
    ]);
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");
    let container = ContainerId::parse(id).unwrap();
    runtime.delete(&container).unwrap();
    let calls = runtime.runner.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0], ["rm", "--force", container.as_str()]);
    assert!(
        calls[1..]
            .iter()
            .all(|call| call[0..4] == ["container", "ls", "--all", "--no-trunc"])
    );
}

#[test]
fn deletion_verification_command_error_is_immediately_uncertain() {
    let id = "c".repeat(64);
    let runner = FakeRunner::with_outputs([
        success(Vec::new()),
        CommandOutput {
            exit_code: Some(125),
            stdout: Vec::new(),
            stderr: b"daemon unavailable".to_vec(),
        },
    ]);
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");
    let container = ContainerId::parse(id).unwrap();
    assert!(matches!(
        runtime.delete(&container),
        Err(RuntimeError::CommandFailed {
            operation: "verify deletion",
            ..
        })
    ));
    assert_eq!(runtime.runner.calls().len(), 2);
}

#[test]
fn invalid_specs_never_reach_the_runtime() {
    let runner = FakeRunner::default();
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");
    let mut socket = job();
    socket.mounts.push(JobMount {
        source: "/var/run/docker.sock".into(),
        target: "/var/run/docker.sock".into(),
        access: MountAccess::ReadWrite,
        subpath: None,
    });
    assert!(matches!(
        runtime.create(&socket),
        Err(RuntimeError::InvalidSpec(JobSpecError::InvalidMount))
    ));
    assert!(runtime.runner.calls().is_empty());
}

#[test]
fn grant_subpaths_resolve_only_through_real_directories() {
    let grant = tempfile::tempdir().unwrap();
    let root = grant.path();
    fs::create_dir_all(root.join(".marsh/split/abcd1234/fix")).unwrap();
    fs::write(root.join("file"), "x").unwrap();
    std::os::unix::fs::symlink("/", root.join(".marsh/split/abcd1234/escape")).unwrap();
    assert_eq!(
        resolve_grant_subpath(root, Path::new(".marsh/split/abcd1234/fix")).unwrap(),
        root.join(".marsh/split/abcd1234/fix")
    );
    // A regular file is admitted only as the leaf (workspace admin binds).
    assert_eq!(
        resolve_grant_subpath(root, Path::new("file")).unwrap(),
        root.join("file")
    );
    for rejected in [".marsh/split/abcd1234/escape", "file/x", "missing"] {
        assert!(
            resolve_grant_subpath(root, Path::new(rejected)).is_err(),
            "{rejected}"
        );
    }
}

#[test]
fn docker_mount_fields_quote_csv_punctuation_without_changing_the_path() {
    assert_eq!(
        docker_mount_field(
            "target",
            Path::new("/Users/example/dev/comma,equals=project")
        )
        .unwrap(),
        "\"target=/Users/example/dev/comma,equals=project\""
    );
    assert_eq!(
        docker_mount_field("target", Path::new("/Users/example/dev/equals=project")).unwrap(),
        "target=/Users/example/dev/equals=project"
    );
}

#[test]
fn docker_create_preserves_comma_equals_project_target_as_one_csv_field() {
    let runner = FakeRunner::with_create_outputs([success(format!("{}\n", "c".repeat(64)))]);
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");
    let mut spec = job();
    spec.mounts[0].target = "/Users/example/dev/comma,equals=project".into();
    spec.working_directory = spec.mounts[0].target.clone();

    runtime.create(&spec).unwrap();

    let calls = runtime.runner.lifecycle_calls();
    assert!(calls[0].iter().any(|argument| {
        argument
            == "type=bind,source=/run/marsh/grants/attempt-1/project,\"target=/Users/example/dev/comma,equals=project\""
    }));
}

#[test]
fn system_runner_honors_argument_safe_working_directory() {
    let directory = tempfile::tempdir().unwrap();
    let runner = SystemCommandRunner::new("/tmp");
    let invocation = Invocation {
        program: "/bin/pwd".into(),
        arguments: Vec::new(),
        working_directory: Some(directory.path().to_owned()),
        environment: Vec::new(),
    };

    let output = runner.run(&invocation).unwrap();
    assert!(output.succeeded());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        directory.path().canonicalize().unwrap().to_str().unwrap()
    );
}

#[test]
fn bounded_system_runner_preserves_output_and_status() {
    let runner = SystemCommandRunner::new("/tmp");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf stdout; printf stderr >&2; exit 37".into(),
        ],
        working_directory: None,
        environment: Vec::new(),
    };

    let output = runner
        .run_bounded(&invocation, Duration::from_secs(2))
        .unwrap();
    assert_eq!(output.exit_code, Some(37));
    assert_eq!(output.stdout, b"stdout");
    assert_eq!(output.stderr, b"stderr");
}

#[test]
fn bounded_system_runner_kills_descendant_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let child_pid = directory.path().join("child.pid");
    let script = format!("sleep 30 & echo $! > '{}'; wait", child_pid.display());
    let runner = SystemCommandRunner::new("/tmp");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), script.into()],
        working_directory: None,
        environment: Vec::new(),
    };

    let started = std::time::Instant::now();
    let error = runner
        .run_bounded(&invocation, Duration::from_millis(200))
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
    let child = std::fs::read_to_string(child_pid)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(child), None),
        Err(nix::errno::Errno::ESRCH)
    );
}

struct TryWaitErrorProcess {
    terminated: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
}

impl AttachedProcess for TryWaitErrorProcess {
    fn supports_io_cancellation(&self) -> bool {
        true
    }
    fn cancel_io(&self) -> io::Result<()> {
        Ok(())
    }

    fn wait(&mut self) -> io::Result<i32> {
        panic!("bounded control must not call blocking wait")
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        self.reaped.store(true, Ordering::SeqCst);
        Ok(Some(125))
    }

    fn try_wait_unreaped(&mut self) -> io::Result<Option<i32>> {
        Err(io::Error::other("try-wait failed"))
    }

    fn terminate(&mut self) -> io::Result<()> {
        self.terminated.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn bounded_attachment_unknown_identity_withholds_signal_and_reports_uncertainty() {
    let terminated = Arc::new(AtomicBool::new(false));
    let reaped = Arc::new(AtomicBool::new(false));
    let attachment = Attachment {
        stdin: Box::new(Cursor::new(Vec::new())),
        stdout: Box::new(Cursor::new(b"stdout".to_vec())),
        stderr: Box::new(Cursor::new(b"stderr".to_vec())),
        process: Box::new(TryWaitErrorProcess {
            terminated: Arc::clone(&terminated),
            reaped: Arc::clone(&reaped),
        }),
        control: no_attachment_control(),
    };

    let error = run_attachment_bounded(attachment, Duration::from_secs(1)).unwrap_err();
    assert!(error.to_string().contains("cleanup uncertain"));
    assert!(!terminated.load(Ordering::SeqCst));
    assert!(reaped.load(Ordering::SeqCst));
}

struct DeniedTermination;

impl AttachedProcess for DeniedTermination {
    fn supports_io_cancellation(&self) -> bool {
        true
    }

    fn cancel_io(&self) -> io::Result<()> {
        Ok(())
    }

    fn wait(&mut self) -> io::Result<i32> {
        panic!("bounded control must not call blocking wait")
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(Some(0))
    }

    fn terminate(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture group termination denied",
        ))
    }
}

#[test]
fn bounded_attachment_successful_output_cannot_hide_termination_failure() {
    let attachment = Attachment {
        stdin: Box::new(Cursor::new(Vec::new())),
        stdout: Box::new(Cursor::new(b"apparently successful output".to_vec())),
        stderr: Box::new(Cursor::new(Vec::new())),
        process: Box::new(DeniedTermination),
        control: no_attachment_control(),
    };
    let error = run_attachment_bounded(attachment, Duration::from_secs(1)).unwrap_err();
    // The rule: apparent success never hides a failed termination.
    assert!(error.to_string().contains("cleanup uncertain"), "{error}");
}

#[test]
#[cfg(target_os = "linux")]
fn bounded_system_runner_cancels_escaped_pipe_holders_without_waiting_for_eof() {
    let root = tempfile::tempdir().unwrap();
    let pid_file = root.path().join("escaped-pid");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            format!(
                "setsid /bin/sh -c 'echo $$ > {}; exec sleep 60' & exit 7",
                pid_file.display()
            )
            .into(),
        ],
        working_directory: None,
        environment: Vec::new(),
    };
    let started = std::time::Instant::now();
    let result =
        SystemCommandRunner::new("/tmp").run_bounded(&invocation, Duration::from_millis(200));
    let elapsed = started.elapsed();
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // This deliberately escaped fixture remains alive: EOF was not manufactured
    // by killing it. Test teardown explicitly terminates its own exact group.
    let live = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    assert!(live);
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
}

#[test]
fn local_control_is_fenced_after_reaping_for_both_pipe_and_pty() {
    let runner = SystemCommandRunner::new("/tmp");
    for terminal in [false, true] {
        let invocation = Invocation {
            program: "/usr/bin/true".into(),
            arguments: vec![],
            working_directory: None,
            environment: Vec::new(),
        };
        let mut attachment = if terminal {
            runner.spawn_pty(&invocation)
        } else {
            runner.spawn_attached(&invocation)
        }
        .unwrap();
        assert_eq!(attachment.process.wait().unwrap(), 0);
        assert_eq!(
            attachment
                .control
                .signal(JobSignal::Kill)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        if terminal {
            assert_eq!(
                attachment
                    .control
                    .resize(TerminalSize {
                        rows: 24,
                        columns: 80
                    })
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::NotFound
            );
        }
        // Late cleanup is harmless and must not send kill(-old_pid).
        attachment.process.terminate().unwrap();
    }
}

#[test]
#[cfg(target_os = "linux")]
fn bounded_system_runner_kills_group_even_when_leader_exited_before_pipe_drain() {
    let root = tempfile::tempdir().unwrap();
    let pid_file = root.path().join("child-pid");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            format!(
                "/bin/sh -c 'echo $$ > {}; exec sleep 60' & exit 7",
                pid_file.display()
            )
            .into(),
        ],
        working_directory: None,
        environment: Vec::new(),
    };
    let result =
        SystemCommandRunner::new("/tmp").run_bounded(&invocation, Duration::from_millis(200));
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let live = fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .rsplit_once(')')
            .unwrap()
            .1
            .trim_start()
            .starts_with('Z')
    });
    if live {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(
        !live,
        "local group descendant survived after its leader exited"
    );
}

#[test]
fn system_pty_preserves_color_paste_bytes_and_resize() {
    let runner = SystemCommandRunner::new("/tmp");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "test -t 0 && test -t 1 || exit 90; printf '\\033[31mRED\\033[0m'; IFS= read -r line; stty size; printf '<%s>' \"$line\"".into(),
        ],
        working_directory: None,
        environment: Vec::new(),
    };
    let mut attachment = runner.spawn_pty(&invocation).unwrap();
    let mut stdout = attachment.stdout;
    let output = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output);
        output
    });
    attachment
        .control
        .resize(TerminalSize {
            rows: 47,
            columns: 123,
        })
        .unwrap();
    attachment.stdin.write_all(b"paste $() [] {}\n").unwrap();
    attachment.stdin.flush().unwrap();
    attachment.process.wait().unwrap();
    let output = output.join().unwrap();
    assert!(
        output
            .windows(12)
            .any(|bytes| bytes == b"\x1b[31mRED\x1b[0m"),
        "PTY output: {:?}",
        String::from_utf8_lossy(&output)
    );
    assert!(String::from_utf8_lossy(&output).contains("47 123"));
    assert!(String::from_utf8_lossy(&output).contains("<paste $() [] {}>"));
}

#[test]
fn pty_reader_treats_linux_slave_close_as_eof() {
    assert_eq!(
        normalize_pty_read(Err(std::io::Error::from_raw_os_error(nix::libc::EIO))).unwrap(),
        0
    );
    let denied =
        normalize_pty_read(Err(std::io::Error::from_raw_os_error(nix::libc::EACCES))).unwrap_err();
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EACCES));
}

#[test]
fn system_pty_delivers_cursor_response_bytes_without_echo_or_newline() {
    let runner = SystemCommandRunner::new("/tmp");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "dd bs=1 count=7 2>/dev/null".into()],
        working_directory: None,
        environment: Vec::new(),
    };
    let mut attachment = runner.spawn_pty(&invocation).unwrap();
    let response = b"\x1b[56;1R";
    let mut stdout = attachment.stdout;
    let output = std::thread::spawn(move || {
        let mut output = [0_u8; 7];
        stdout.read_exact(&mut output).unwrap();
        output
    });
    attachment.stdin.write_all(response).unwrap();
    attachment.stdin.flush().unwrap();
    attachment.process.wait().unwrap();
    let output = output.join().unwrap();

    assert_eq!(output, *response);
}

#[test]
fn system_pty_forwards_sigint_to_exact_child() {
    let runner = SystemCommandRunner::new("/tmp");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "trap 'printf INTERRUPTED; exit 130' INT; printf READY; while :; do :; done".into(),
        ],
        working_directory: None,
        environment: Vec::new(),
    };
    let mut attachment = runner.spawn_pty(&invocation).unwrap();
    let mut output = vec![0; 5];
    attachment.stdout.read_exact(&mut output).unwrap();
    assert_eq!(output, b"READY");
    let mut stdout = attachment.stdout;
    let remaining = std::thread::spawn(move || {
        let mut output = Vec::new();
        let _ = stdout.read_to_end(&mut output);
        output
    });
    attachment.control.signal(JobSignal::Interrupt).unwrap();
    attachment.process.wait().unwrap();
    output.extend(remaining.join().unwrap());
    assert!(String::from_utf8_lossy(&output).contains("INTERRUPTED"));
}

#[test]
fn system_pipe_attachment_forwards_sigint_to_exact_child() {
    let runner = SystemCommandRunner::new("/tmp");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "trap 'printf INTERRUPTED; exit 130' INT; printf READY; while :; do :; done".into(),
        ],
        working_directory: None,
        environment: Vec::new(),
    };
    let mut attachment = runner.spawn_attached(&invocation).unwrap();
    let mut output = vec![0; 5];
    attachment.stdout.read_exact(&mut output).unwrap();
    assert_eq!(output, b"READY");
    attachment.control.signal(JobSignal::Interrupt).unwrap();
    assert_eq!(attachment.process.wait().unwrap(), 130);
    attachment.stdout.read_to_end(&mut output).unwrap();
    assert!(String::from_utf8_lossy(&output).contains("INTERRUPTED"));
}

#[test]
fn attached_process_reports_real_pre_execution_failure_status() {
    let runner = SystemCommandRunner::new("/tmp");
    let invocation = Invocation {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "exit 37".into()],
        working_directory: None,
        environment: Vec::new(),
    };
    let mut attachment = runner.spawn_attached(&invocation).unwrap();
    assert_eq!(attachment.process.wait().unwrap(), 37);
}

#[test]
fn only_explicit_session_identity_enters_the_job_environment() {
    let id = "c".repeat(64);
    let runner = FakeRunner::with_create_outputs([success(format!("{id}\n"))]);
    let runtime = DockerCliRuntime::new(runner, "/trusted/docker");

    runtime.create(&job()).unwrap();
    let create = &runtime.runner.lifecycle_calls()[0];
    let environment = create
        .windows(2)
        .filter(|pair| pair[0] == "--env")
        .map(|pair| pair[1].as_str())
        .collect::<Vec<_>>();
    assert_eq!(environment.len(), 4);
    assert!(environment.contains(&"HOME=/Users/alice"));
    assert!(environment.contains(&"MARSH_SELECTED_HOME=/Users/alice"));
    for forbidden in [
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "DOCKER_HOST",
        "SBX_TOKEN",
        "HTTPS_PROXY",
        "PATH",
    ] {
        assert!(!environment.iter().any(|value| value.starts_with(forbidden)));
    }
}
