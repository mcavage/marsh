//! Stock CLI contract callers: reuse, first registration and no overwrite.
use marsh_runtime::{Attachment, CommandOutput, CommandRunner, Invocation};
use marsh_sbx::{McpServer, StockSbx};
use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Runner {
    responses: Mutex<VecDeque<CommandOutput>>,
    calls: Mutex<Vec<Vec<String>>>,
}
impl CommandRunner for Runner {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        self.calls.lock().unwrap().push(
            invocation
                .arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
        );
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| io::Error::other("unexpected stock effect"))
    }
    fn run_bounded(&self, invocation: &Invocation, _: Duration) -> io::Result<CommandOutput> {
        self.run(invocation)
    }
    fn spawn_attached(&self, _: &Invocation) -> io::Result<Attachment> {
        Err(io::Error::other("unexpected attachment"))
    }
}
fn output(code: i32, stdout: &str, stderr: &str) -> CommandOutput {
    CommandOutput {
        exit_code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
    }
}
fn fixture(responses: Vec<CommandOutput>) -> (StockSbx, Arc<Runner>) {
    let runner = Arc::new(Runner {
        responses: Mutex::new(responses.into()),
        calls: Mutex::new(Vec::new()),
    });
    (StockSbx::new("sbx", runner.clone()), runner)
}
fn remote() -> McpServer {
    McpServer {
        name: "granola".into(),
        url: "https://mcp.granola.ai/mcp".into(),
        command: String::new(),
        args: Vec::new(),
    }
}

#[test]
fn matching_registration_is_reused_without_add_or_auth() {
    let (sbx, runner) = fixture(vec![output(
        0,
        r#"{"name":"granola","type":"remote","url":"https://mcp.granola.ai/mcp","requires_oauth":true}"#,
        "",
    )]);
    sbx.ensure_mcp_server(&remote()).unwrap();
    assert_eq!(
        *runner.calls.lock().unwrap(),
        vec![vec!["mcp", "inspect", "granola", "--json"]]
    );
}

#[test]
fn missing_registration_is_added_without_starting_browser_auth() {
    let (sbx, runner) = fixture(vec![
        output(1, "", "error: mcp server not found"),
        output(0, "registered", ""),
    ]);
    sbx.ensure_mcp_server(&remote()).unwrap();
    assert_eq!(
        runner.calls.lock().unwrap()[1],
        [
            "mcp",
            "add",
            "granola",
            "--url",
            "https://mcp.granola.ai/mcp",
            "--skip-auth"
        ]
    );
}

#[test]
fn conflicting_or_unreadable_registrations_never_get_overwritten() {
    for response in [
        output(
            0,
            r#"{"name":"granola","url":"https://different.example/mcp"}"#,
            "",
        ),
        output(1, "", "permission denied"),
        output(0, "not json", ""),
    ] {
        let (sbx, runner) = fixture(vec![response]);
        assert!(sbx.ensure_mcp_server(&remote()).is_err());
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn local_argv_is_preserved_as_csv_without_shell_interpretation() {
    let (sbx, runner) = fixture(vec![
        output(1, "", "mcp server not found"),
        output(0, "registered", ""),
    ]);
    let server = McpServer {
        name: "local".into(),
        url: String::new(),
        command: "/usr/bin/printf".into(),
        args: vec![
            "a,b".into(),
            "say \"hello\"".into(),
            "$(touch forbidden)".into(),
        ],
    };
    sbx.ensure_mcp_server(&server).unwrap();
    assert_eq!(
        runner.calls.lock().unwrap()[1],
        [
            "mcp",
            "add",
            "local",
            "--command",
            "/usr/bin/printf",
            "--args",
            "\"a,b\",\"say \"\"hello\"\"\",\"$(touch forbidden)\""
        ]
    );
}

#[test]
fn invalid_declarations_have_no_stock_effects() {
    let (sbx, runner) = fixture(Vec::new());
    for server in [
        McpServer {
            name: "../escape".into(),
            ..remote()
        },
        McpServer {
            command: "also-command".into(),
            ..remote()
        },
        McpServer {
            args: vec!["unexpected".into()],
            ..remote()
        },
    ] {
        assert!(sbx.ensure_mcp_server(&server).is_err());
    }
    assert!(runner.calls.lock().unwrap().is_empty());
}
