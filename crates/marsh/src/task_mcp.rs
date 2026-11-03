//! `marsh task-mcp` (in a job): a stdio MCP server whose `task` tool hands
//! work to another registered agent. It runs inside the calling job's
//! container and starts every child through this job's own in-job marsh
//! (`marsh run`, `marsh split`), so the child is the job's child and every
//! process rule (spawn set, limits, wall time) applies unchanged
//! (`docs/agents.md`, "The task tool").

use marsh_contracts::process::{self as contract, JOB_JSON, JobDocument, SOCKET};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{BufRead as _, Read as _, Seek as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const HELP: &str = "Usage: marsh task-mcp

Inside a marsh job: serve the `task`, `task_output` and `task_apply` tools
over MCP on stdin and stdout. A task starts a registered agent as a child
job of this job (its non-interactive prompt verbs, e.g. `claude -p`,
`codex exec`), in this job's folder or (files: \"copy\") in a private split
fork whose diff `task_apply` applies. Agent Kits register it in their MCP
configuration; see https://runmar.sh/docs/agents.html#the-task-tool.
";

const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// Linux refuses a single argv string over 128 KiB (`MAX_ARG_STRLEN`).
const MAX_PROMPT_BYTES: usize = 100 * 1024;
const MESSAGE_CAP: usize = 48 * 1024;
const DIFF_CAP: usize = 32 * 1024;
const TRANSCRIPT_CAP: usize = 128 * 1024;
const STDERR_TAIL: usize = 4 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_secs(20);
/// Largest JSON-RPC message read from stdin; a longer line is refused.
const MAX_LINE_BYTES: usize = 1 << 20;
/// `tools/call` requests in flight at once; more are refused at once.
const MAX_CALLS: usize = 32;
/// Tasks one server starts (a tree's launch limit is 128 too).
const MAX_TASKS: u64 = 128;
/// A description is also the task's job label, `task: DESCRIPTION`
/// (labels are at most 128 bytes).
const MAX_DESCRIPTION_BYTES: usize = 120;
/// The label prefix on every task's job receipt (`marsh jobs --json`,
/// `marsh events`): `task: DESCRIPTION`.
pub const LABEL_PREFIX: &str = "task: ";

const TASK_DESCRIPTION: &str = "Hand work to another agent. Fast and cheap: use it whenever you would hand \
work off — parallel parts, a second opinion, a review, a tool you don't hold. Defaults: the same agent as \
you, working in your folder. Set `agent` for another one (a different agent for reviews). Set `files: \
\"copy\"` when its edits could collide with yours or another task's, or for competing attempts and risky \
changes: it then works on a private copy and you get its diff to apply; read-only and independent work can \
share the folder.";

/// What `task-mcp` knows at start: this job and the agents it may start.
#[derive(Clone, Debug)]
struct Setup {
    /// The calling agent's registered name: the default `agent`.
    caller: String,
    /// Agent name -> prompt verbs, limited to this job's spawn set.
    agents: BTreeMap<String, Vec<String>>,
    /// Agent name -> its model option (`org.marsh/agent-model@1`), for
    /// `task(model)`; an agent without one refuses `model`.
    models: BTreeMap<String, Vec<String>>,
    /// Why no agent is available, when none is.
    none_reason: Option<String>,
    /// The in-job marsh (the artifact) used to start children.
    marsh: PathBuf,
    /// Where task transcripts are kept in this container.
    dir: PathBuf,
}

impl Setup {
    fn from_document(document: &JobDocument, narrowed: Option<&[String]>) -> Self {
        let allowed = |name: &str| {
            document.spawn.iter().any(|allowed| allowed == name)
                && narrowed.is_none_or(|narrowed| narrowed.iter().any(|allowed| allowed == name))
        };
        let agents = document
            .prompts
            .iter()
            .filter(|(name, _)| allowed(name))
            .map(|(name, verbs)| (name.clone(), verbs.clone()))
            .collect::<BTreeMap<_, _>>();
        let models = document
            .models
            .iter()
            .filter(|(name, _)| agents.contains_key(*name))
            .map(|(name, verbs)| (name.clone(), verbs.clone()))
            .collect();
        let none_reason = agents.is_empty().then(|| {
            if document.spawn.is_empty() || narrowed.is_some_and(<[String]>::is_empty) {
                "this job may start no agents (it was started with --no-spawn or MARSH_SPAWN=none)"
                    .to_owned()
            } else if document.prompts.is_empty() {
                "no registered agent declares non-interactive prompt verbs (agent-sessions@1 prompt)"
                    .to_owned()
            } else {
                format!(
                    "no agent with prompt verbs is in this job's spawn set ({})",
                    document.spawn.join(", ")
                )
            }
        });
        let short = document.job.get(..8).unwrap_or(&document.job);
        Self {
            caller: document.name.clone(),
            agents,
            models,
            none_reason,
            marsh: PathBuf::from(contract::ARTIFACT),
            dir: std::env::temp_dir().join(format!("marsh-tasks-{short}-{}", std::process::id())),
        }
    }

    fn default_agent(&self) -> Option<&str> {
        self.agents
            .contains_key(&self.caller)
            .then_some(self.caller.as_str())
            .or_else(|| {
                (self.agents.len() == 1)
                    .then(|| self.agents.keys().next().map(String::as_str))
                    .flatten()
            })
    }

    fn agent_list(&self) -> String {
        if self.agents.is_empty() {
            return self.none_reason.clone().unwrap_or_default();
        }
        self.agents
            .keys()
            .map(|name| {
                if *name == self.caller {
                    format!("{name} (you)")
                } else {
                    name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn tools(&self) -> Value {
        let agents = self.agent_list();
        let names = self.agents.keys().cloned().collect::<Vec<_>>();
        let mut agent = json!({
            "type": "string",
            "description": format!(
                "A registered agent. Default: the same kind as you. For a review or second opinion, \
                 pick a different one. Available: {agents}."
            ),
        });
        if !names.is_empty() {
            agent["enum"] = json!(names);
        }
        let with_models = self.models.keys().cloned().collect::<Vec<_>>();
        let model = json!({
            "type": "string",
            "description": format!(
                "The model the agent runs, passed to its own model option (e.g. a reviewer's model \
                 from the peer-review table). Default: the agent's own default. Agents that take \
                 one: {}.",
                if with_models.is_empty() { "none".to_owned() } else { with_models.join(", ") }
            ),
        });
        // `anthropic/alwaysLoad`: Claude Code declares these upfront rather
        // than behind its tool search, so the model sees `task` every turn.
        json!([
            {
                "name": "task",
                "description": format!("{TASK_DESCRIPTION}\n\nAgents: {agents}."),
                "_meta": {"anthropic/alwaysLoad": true},
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "description": {"type": "string", "description": "3-6 words, shown to the user."},
                        "prompt": {"type": "string", "description": "Everything the agent needs; it does not see this conversation."},
                        "agent": agent,
                        "model": model,
                        "files": {
                            "type": "string",
                            "enum": ["shared", "copy"],
                            "description": "\"shared\" (default): it works in your folder and its edits are yours at once. \"copy\": it works on a private copy; the result includes its diff, which you apply with task_apply or drop."
                        },
                        "background": {"type": "boolean", "description": "false (default) waits and returns the result; true returns a task id at once (read it with task_output)."}
                    },
                    "required": ["description", "prompt"]
                }
            },
            {
                "name": "task_output",
                "description": "The result of a task (or its status if it is not done). `wait: true` waits for it to finish; `transcript: true` returns its whole output.",
                "_meta": {"anthropic/alwaysLoad": true},
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "The task id."},
                        "wait": {"type": "boolean", "description": "Wait until the task is done (default false)."},
                        "transcript": {"type": "boolean", "description": "Return the task's whole output instead of its final message (default false)."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "task_apply",
                "description": "Apply a finished `copy` task's diff to your folder (3-way when it no longer applies cleanly; conflicts are named).",
                "_meta": {"anthropic/alwaysLoad": true},
                "inputSchema": {
                    "type": "object",
                    "properties": {"id": {"type": "string", "description": "The task id."}},
                    "required": ["id"]
                }
            }
        ])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Files {
    Shared,
    Copy,
}

/// A validated `task` call.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskRequest {
    description: String,
    agent: String,
    /// The model passed to the agent's model option, when the call set one.
    model: Option<String>,
    argv: Vec<String>,
    files: Files,
    background: bool,
}

/// Validate `task` arguments; the error is the tool error's text.
fn task_request(setup: &Setup, arguments: &Value) -> Result<TaskRequest, String> {
    let text = |name: &str| arguments.get(name).and_then(Value::as_str);
    let prompt = text("prompt")
        .filter(|prompt| !prompt.trim().is_empty())
        .ok_or("`prompt` is required: everything the agent needs to do the work")?;
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(format!(
            "the prompt is {} bytes, over {MAX_PROMPT_BYTES}: put the material in a file in your folder and point the prompt at it",
            prompt.len()
        ));
    }
    if prompt.contains('\0') {
        return Err("the prompt contains a NUL byte".into());
    }
    let description = match text("description").map(str::trim) {
        Some(description) if !description.is_empty() => {
            if description.len() > MAX_DESCRIPTION_BYTES
                || description.chars().any(char::is_control)
            {
                return Err(format!(
                    "`description` is 3-6 words: at most {MAX_DESCRIPTION_BYTES} bytes, no control characters"
                ));
            }
            description.to_owned()
        }
        _ => short_description(prompt),
    };
    if setup.agents.is_empty() {
        return Err(format!(
            "no agent can be started: {}",
            setup.none_reason.clone().unwrap_or_default()
        ));
    }
    let agent = match text("agent")
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
    {
        Some(agent) => agent.to_owned(),
        None => setup.default_agent().map(str::to_owned).ok_or_else(|| {
            format!(
                "set `agent`: you ({}) are not one this job may start; available: {}",
                setup.caller,
                setup.agent_list()
            )
        })?,
    };
    let Some(verbs) = setup.agents.get(&agent) else {
        return Err(format!(
            "`{agent}` is not an agent this job may start; available: {}",
            setup.agent_list()
        ));
    };
    let files = match text("files") {
        None | Some("shared") => Files::Shared,
        Some("copy") => Files::Copy,
        Some(other) => return Err(format!("`files` is \"shared\" or \"copy\", not {other:?}")),
    };
    let background = match arguments.get("background") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err("`background` is true or false".into()),
    };
    let mut argv = verbs
        .iter()
        .map(|verb| verb.replace(contract::PROMPT_PLACEHOLDER, prompt))
        .collect::<Vec<_>>();
    let model = model_argument(setup, &agent, arguments, &mut argv)?;
    Ok(TaskRequest {
        description,
        agent,
        model,
        argv,
        files,
        background,
    })
}

/// `task(model)`: validate it and append the agent's model option to `argv`
/// (after the prompt verbs, so `codex exec PROMPT -m M` keeps the subcommand
/// first); an agent whose Kit declares none refuses it.
fn model_argument(
    setup: &Setup,
    agent: &str,
    arguments: &Value,
    argv: &mut Vec<String>,
) -> Result<Option<String>, String> {
    let model = match arguments.get("model") {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(model)) if model.trim().is_empty() => return Ok(None),
        Some(Value::String(model)) => model.trim(),
        Some(_) => return Err("`model` is a string: the model id the agent should run".into()),
    };
    marsh_daemon::peer_review::valid_model(model).map_err(|error| format!("`model`: {error}"))?;
    let Some(option) = setup.models.get(agent) else {
        return Err(format!(
            "`{agent}` takes no `model`: its Kit declares no model option (org.marsh/agent-model@1); \
             agents that do: {}",
            if setup.models.is_empty() {
                "none".to_owned()
            } else {
                setup.models.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        ));
    };
    argv.extend(
        option
            .iter()
            .map(|verb| verb.replace(contract::MODEL_PLACEHOLDER, model)),
    );
    Ok(Some(model.to_owned()))
}

/// The first words of `prompt` as a description: no control characters,
/// at most `MAX_DESCRIPTION_BYTES`.
fn short_description(prompt: &str) -> String {
    let mut description = String::new();
    for word in prompt.split_whitespace().take(6) {
        let word = word.chars().filter(|c| !c.is_control()).collect::<String>();
        if description.len() + word.len() + 1 > MAX_DESCRIPTION_BYTES {
            break;
        }
        if !description.is_empty() {
            description.push(' ');
        }
        description.push_str(&word);
    }
    if description.is_empty() {
        "task".into()
    } else {
        description
    }
}

/// How a task ended.
#[derive(Clone, Debug, Default)]
struct Outcome {
    /// The child's exit status (`None`: killed by a signal or never started).
    code: Option<i32>,
    /// "exited 0", "failed: ...", "cancelled".
    status: String,
    /// The agent's final message (its stdout).
    message: String,
    /// The tail of its stderr when it failed.
    stderr: String,
    /// Where the whole output is, in this container.
    transcript: PathBuf,
    stderr_path: PathBuf,
    /// Set when it never started: the refusal, verbatim.
    refused: Option<String>,
    /// `copy`: the split and its patch.
    split: Option<String>,
    patch: Option<PathBuf>,
    files: Vec<String>,
}

#[derive(Debug)]
enum State {
    Running { pid: u32 },
    Done(Box<Outcome>),
}

#[derive(Debug)]
struct Task {
    id: String,
    request: TaskRequest,
    started: Instant,
    job: Option<String>,
    state: State,
    applied: bool,
}

#[derive(Default)]
#[allow(clippy::struct_field_names)]
struct Tasks {
    next: u64,
    /// stdin closed: no task starts any more.
    closing: bool,
    tasks: BTreeMap<String, Task>,
}

/// One `tools/call` in flight, registered before its thread starts so a
/// `notifications/cancelled` that arrives first is not lost.
#[derive(Default)]
struct Call {
    cancelled: bool,
    /// The task a blocking `task` call started.
    task: Option<String>,
}

struct Server {
    setup: Setup,
    tasks: Mutex<Tasks>,
    changed: Condvar,
    out: Mutex<std::io::Stdout>,
    calls: Mutex<BTreeMap<String, Call>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `marsh task-mcp` inside a job.
#[must_use]
pub fn main(arguments: &[OsString]) -> i32 {
    if arguments
        .first()
        .is_some_and(|word| matches!(word.to_str(), Some("-h" | "--help")))
    {
        print!("{HELP}");
        return 0;
    }
    if let Some(extra) = arguments.first() {
        eprintln!(
            "marsh: task-mcp takes no arguments (got {})",
            extra.to_string_lossy()
        );
        return 2;
    }
    let document = std::fs::read(JOB_JSON)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<JobDocument>(&bytes).ok());
    let Some(document) = document.filter(|_| Path::new(SOCKET).exists()) else {
        eprintln!("marsh: task-mcp runs inside a marsh job (no {JOB_JSON} or {SOCKET})");
        return 125;
    };
    let narrowed = std::env::var("MARSH_SPAWN")
        .ok()
        .map(|value| contract::parse_spawn(&value));
    let mut setup = Setup::from_document(&document, narrowed.as_deref());
    match private_directory(&setup.dir) {
        Ok(directory) => setup.dir = directory,
        Err(error) => {
            eprintln!("marsh: task-mcp: {error}");
            return 125;
        }
    }
    let server = Arc::new(Server {
        setup,
        tasks: Mutex::new(Tasks::default()),
        changed: Condvar::new(),
        out: Mutex::new(std::io::stdout()),
        calls: Mutex::new(BTreeMap::new()),
    });
    let mut stdin = std::io::stdin().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut stdin)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE_BYTES && !line.ends_with(b"\n") {
            // Discard the rest of the oversized message.
            let mut rest = Vec::new();
            while !rest.ends_with(b"\n") {
                rest.clear();
                match (&mut stdin).take(64 * 1024).read_until(b'\n', &mut rest) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            server.send(&json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32600, "message": format!("message over {MAX_LINE_BYTES} bytes")}
            }));
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                for message in batch {
                    Server::dispatch(&server, &message);
                }
            }
            Ok(message) => Server::dispatch(&server, &message),
            Err(error) => server.send(&json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": format!("parse error: {error}")}
            })),
        }
    }
    server.shutdown();
    0
}

/// A new owner-only directory for this server's transcripts: never one that
/// already exists (another process could have planted links in it).
fn private_directory(base: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt as _;
    for attempt in 0..16_u32 {
        let candidate = if attempt == 0 {
            base.to_path_buf()
        } else {
            PathBuf::from(format!("{}-{attempt}", base.display()))
        };
        match std::fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("cannot create {}: {error}", candidate.display())),
        }
    }
    Err(format!(
        "cannot create a private directory near {}",
        base.display()
    ))
}

impl Server {
    fn send(&self, message: &Value) {
        let mut out = lock(&self.out);
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    fn reply(&self, id: &Value, result: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn dispatch(server: &Arc<Self>, message: &Value) {
        let id = message.get("id").cloned();
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id.filter(|id| !id.is_null()) else {
            if method == "notifications/cancelled" {
                server.cancel_call(&params["requestId"]);
            }
            return;
        };
        match method.as_str() {
            "initialize" => {
                let requested = params["protocolVersion"].as_str().unwrap_or_default();
                let version = PROTOCOL_VERSIONS
                    .iter()
                    .find(|version| **version == requested)
                    .unwrap_or(&PROTOCOL_VERSIONS[0]);
                server.reply(
                    &id,
                    &json!({
                        "protocolVersion": version,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": "marsh", "version": env!("CARGO_PKG_VERSION")},
                        "instructions": "Your subagents are marsh agents: hand work off with the `task` tool.",
                    }),
                );
            }
            "ping" => server.reply(&id, &json!({})),
            "tools/list" => server.reply(&id, &json!({"tools": server.setup.tools()})),
            "tools/call" => {
                let key = id.to_string();
                {
                    let mut calls = lock(&server.calls);
                    if calls.len() >= MAX_CALLS || calls.contains_key(&key) {
                        drop(calls);
                        server.send(&json!({
                            "jsonrpc": "2.0", "id": id,
                            "error": {"code": -32000, "message": format!(
                                "busy: {MAX_CALLS} tool calls are in flight (or this request id is in use)")}
                        }));
                        return;
                    }
                    calls.insert(key.clone(), Call::default());
                }
                let server = Arc::clone(server);
                std::thread::spawn(move || {
                    let result = server.call(&key, &params);
                    let cancelled = lock(&server.calls)
                        .remove(&key)
                        .is_some_and(|call| call.cancelled);
                    // A cancelled request gets no response (MCP).
                    if !cancelled {
                        server.reply(&id, &result);
                    }
                });
            }
            _ => server.send(&json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            })),
        }
    }

    fn call(self: &Arc<Self>, key: &str, params: &Value) -> Value {
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let progress = params
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"))
            .cloned();
        let result = match params["name"].as_str().unwrap_or_default() {
            "task" => self.task(key, &arguments, progress.as_ref()),
            "task_output" => self.task_output(key, &arguments, progress.as_ref()),
            "task_apply" => self.task_apply(&arguments),
            other => Err(format!("unknown tool `{other}`")),
        };
        match result {
            Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
            Err(text) => json!({"content": [{"type": "text", "text": text}], "isError": true}),
        }
    }

    fn cancelled(&self, key: &str) -> bool {
        lock(&self.calls)
            .get(key)
            .is_some_and(|call| call.cancelled)
    }

    fn task(
        self: &Arc<Self>,
        key: &str,
        arguments: &Value,
        progress: Option<&Value>,
    ) -> Result<String, String> {
        let request = task_request(&self.setup, arguments)?;
        // Start and record the child under one lock, so shutdown either sees
        // it running or refuses it.
        let (id, pid) = {
            let mut tasks = lock(&self.tasks);
            if tasks.closing {
                return Err("the task server is shutting down".into());
            }
            if tasks.next >= MAX_TASKS {
                return Err(format!("this server already started {MAX_TASKS} tasks"));
            }
            if self.cancelled(key) {
                return Err("cancelled".into());
            }
            tasks.next += 1;
            let id = format!("t{}", tasks.next);
            let directory = self.setup.dir.join(&id);
            std::fs::create_dir(&directory)
                .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
            let child = self.spawn(&request, &id, &directory)?;
            let pid = child.id();
            tasks.tasks.insert(
                id.clone(),
                Task {
                    id: id.clone(),
                    request: request.clone(),
                    started: Instant::now(),
                    job: None,
                    state: State::Running { pid },
                    applied: false,
                },
            );
            let server = Arc::clone(self);
            let watched = id.clone();
            let watched_request = request.clone();
            std::thread::spawn(move || server.watch(&watched, &watched_request, child, &directory));
            (id, pid)
        };
        if request.background {
            return Ok(format!(
                "Task {id} started in the background ({}, files {}). Get its result with \
                 task_output(id: \"{id}\", wait: true), or keep working and check later.",
                agent_words(&request),
                files_word(request.files),
            ));
        }
        let cancelled = {
            let mut calls = lock(&self.calls);
            let call = calls.entry(key.to_owned()).or_default();
            call.task = Some(id.clone());
            call.cancelled
        };
        if cancelled {
            terminate(pid);
        }
        self.wait(&id, progress, None);
        let tasks = lock(&self.tasks);
        render(&tasks.tasks[&id], false)
    }

    /// Start the child through this job's own marsh: `marsh run AGENT VERBS`
    /// (shared) or a one-branch argv split (copy), labelled with the task's
    /// description.
    fn spawn(
        &self,
        request: &TaskRequest,
        id: &str,
        directory: &Path,
    ) -> Result<std::process::Child, String> {
        let file = |name: &str| {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))
                .map_err(|error| format!("cannot create {}/{name}: {error}", directory.display()))
        };
        let label = task_label(request);
        // The task's model is recorded on the child job from here, never
        // read back from the agent's arguments.
        let model = request
            .model
            .as_ref()
            .map(|model| vec!["--model", model.as_str()])
            .unwrap_or_default();
        let mut command = Command::new(&self.setup.marsh);
        match request.files {
            Files::Shared => {
                command
                    .args(["run", "--label", &label])
                    .args(&model)
                    .arg(&request.agent)
                    .args(&request.argv)
                    .env(crate::job::STARTED_FILE, directory.join("job"));
            }
            Files::Copy => {
                command
                    .args(["split", "-n", "--label", &label])
                    .args(&model)
                    .args([":::", id])
                    .arg(&request.agent)
                    .args(&request.argv);
            }
        }
        command
            .stdin(Stdio::null())
            .stdout(file(match request.files {
                Files::Shared => "stdout",
                Files::Copy => "handle",
            })?)
            .stderr(file("stderr")?)
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", self.setup.marsh.display()))
    }

    /// Follow one task to its end (reaping its process), then record it.
    fn watch(
        &self,
        id: &str,
        request: &TaskRequest,
        mut child: std::process::Child,
        directory: &Path,
    ) {
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(_) => break child.wait().ok(),
            }
            self.note_job(id, directory);
            std::thread::sleep(Duration::from_millis(100));
        };
        self.note_job(id, directory);
        let code = status.and_then(|status| status.code());
        let signal = status.and_then(|status| status.signal());
        let mut outcome = match request.files {
            Files::Shared => shared_outcome(directory, code, signal),
            Files::Copy => self.copy_outcome(directory, code, signal),
        };
        let copy_job = (request.files == Files::Copy)
            .then(|| read_json(&directory.join("manifest.json")))
            .flatten()
            .and_then(|manifest| {
                manifest["branches"][0]["job_id"]
                    .as_str()
                    .map(str::to_owned)
            });
        let mut tasks = lock(&self.tasks);
        if let Some(task) = tasks.tasks.get_mut(id) {
            if copy_job.is_some() {
                task.job = copy_job;
            }
            if task.job.is_some() {
                // It started: its status is the agent's, not a refusal.
                outcome.refused = None;
            }
            task.state = State::Done(Box::new(outcome));
        }
        drop(tasks);
        self.changed.notify_all();
    }

    /// The child's job id, as `marsh run` wrote it from the daemon's
    /// `JobStarted` (shared tasks).
    fn note_job(&self, id: &str, directory: &Path) {
        let mut tasks = lock(&self.tasks);
        let Some(task) = tasks.tasks.get_mut(id).filter(|task| task.job.is_none()) else {
            return;
        };
        let job = read_head(&directory.join("job"), 64);
        let job = job.trim();
        if !job.is_empty()
            && job
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        {
            task.job = Some(job.to_owned());
            drop(tasks);
            self.changed.notify_all();
        }
    }

    fn copy_outcome(&self, directory: &Path, code: Option<i32>, signal: Option<i32>) -> Outcome {
        let mut outcome = Outcome {
            transcript: directory.join("stdout"),
            stderr_path: directory.join("stderr"),
            ..Outcome::default()
        };
        let errors = read_capped(&directory.join("stderr"), STDERR_TAIL, true);
        let handle = read_head(&directory.join("handle"), 4096);
        let split = handle
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|line| line["id"].as_str().map(str::to_owned));
        let Some(split) = split else {
            outcome.code = code;
            outcome.status = exit_words(code, signal);
            outcome.refused = Some(refusal_text(&errors, code, signal));
            return outcome;
        };
        let manifest = Command::new(&self.setup.marsh)
            .args(["join", "--keep", "--json"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .and_then(|mut join| {
                if let Some(mut input) = join.stdin.take() {
                    let _ = input.write_all(handle.as_bytes());
                }
                join.wait_with_output()
            });
        let manifest = match manifest {
            Ok(output) if !output.stdout.is_empty() => {
                let _ = std::fs::write(directory.join("manifest.json"), &output.stdout);
                serde_json::from_slice::<Value>(&output.stdout).ok()
            }
            Ok(output) => {
                outcome.stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                None
            }
            Err(error) => {
                outcome.stderr = error.to_string();
                None
            }
        };
        outcome.split = Some(split.clone());
        let Some(manifest) = manifest else {
            outcome.code = code;
            outcome.status = format!(
                "{} (the split's result could not be read)",
                exit_words(code, signal)
            );
            return outcome;
        };
        let branch = &manifest["branches"][0];
        let out = PathBuf::from(manifest["out"].as_str().unwrap_or_default());
        let label = branch["label"].as_str().unwrap_or_default();
        let lane = out.join(label);
        outcome.code = branch["code"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok())
            .or(code);
        outcome.status = branch["status"]
            .as_str()
            .map_or_else(|| exit_words(code, signal), str::to_owned);
        outcome.transcript = lane.join("stdout");
        outcome.stderr_path = lane.join("stderr");
        outcome.message = read_capped(&lane.join("stdout"), MESSAGE_CAP, false);
        if outcome.code != Some(0) {
            let tail = read_capped(&lane.join("stderr"), STDERR_TAIL, true);
            outcome.stderr = if tail.is_empty() { errors } else { tail };
        }
        if lane.join("diff.patch").is_file() {
            outcome.patch = Some(lane.join("diff.patch"));
        }
        outcome.files = read_head(&lane.join("files"), 64 * 1024)
            .lines()
            .map(str::to_owned)
            .collect();
        if branch["job_id"].is_null() && outcome.code != Some(0) {
            // The branch's admission refusal is its status (`failed: ...`).
            let reason = format!("{}\n{}", outcome.status, outcome.stderr);
            outcome.refused = Some(refusal_text(&reason, outcome.code, signal));
        }
        outcome
    }

    /// Wait for a task to finish, sending progress when the client asked;
    /// with `call`, stop waiting (false) when that call is cancelled.
    fn wait(&self, id: &str, progress: Option<&Value>, call: Option<&str>) -> bool {
        let started = Instant::now();
        let mut tick = 0_u64;
        let mut tasks = lock(&self.tasks);
        loop {
            if !matches!(
                tasks.tasks.get(id).map(|task| &task.state),
                Some(State::Running { .. })
            ) {
                return true;
            }
            if call.is_some_and(|call| self.cancelled(call)) {
                return false;
            }
            let (next, _) = self
                .changed
                .wait_timeout(tasks, Duration::from_millis(500))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tasks = next;
            if let Some(token) = progress
                && started.elapsed() >= PROGRESS_EVERY * u32::try_from(tick + 1).unwrap_or(u32::MAX)
            {
                tick += 1;
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": token,
                        "progress": tick,
                        "message": format!("task {id} running for {}s", started.elapsed().as_secs()),
                    }
                }));
            }
        }
    }

    fn task_output(
        &self,
        key: &str,
        arguments: &Value,
        progress: Option<&Value>,
    ) -> Result<String, String> {
        let id = self.known(arguments)?;
        let flag = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        if flag("wait") && !self.wait(&id, progress, Some(key)) {
            return Err("cancelled".into());
        }
        let tasks = lock(&self.tasks);
        render(&tasks.tasks[&id], flag("transcript"))
    }

    fn task_apply(&self, arguments: &Value) -> Result<String, String> {
        let id = self.known(arguments)?;
        let (split, applied) = {
            let tasks = lock(&self.tasks);
            let task = &tasks.tasks[&id];
            if task.request.files != Files::Copy {
                return Err(format!(
                    "task {id} worked in your folder (files: \"shared\"): its edits are already yours"
                ));
            }
            let State::Done(outcome) = &task.state else {
                return Err(format!(
                    "task {id} is still running; wait for it with task_output(id: \"{id}\", wait: true)"
                ));
            };
            let Some(split) = outcome.split.clone().filter(|_| outcome.refused.is_none()) else {
                return Err(format!(
                    "task {id} never started, so there is nothing to apply"
                ));
            };
            if outcome.patch.is_none() {
                return Ok(format!(
                    "task {id} changed nothing; there is nothing to apply."
                ));
            }
            (split, task.applied)
        };
        if applied {
            return Err(format!("task {id}'s diff is already applied"));
        }
        let output = Command::new(&self.setup.marsh)
            .args(["split", "apply", &split, &id, "--json"])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("cannot run marsh split apply: {error}"))?;
        let document = serde_json::from_slice::<Value>(&output.stdout).unwrap_or_else(|_| {
            json!({
                "result": "failed",
                "message": String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            })
        });
        let result = document["result"].as_str().unwrap_or("failed");
        let files = string_list(&document["files"]);
        let conflicts = string_list(&document["conflicts"]);
        let message = document["message"].as_str().unwrap_or_default().trim();
        if matches!(result, "clean" | "conflicts")
            && let Some(task) = lock(&self.tasks).tasks.get_mut(&id)
        {
            task.applied = true;
        }
        let mut text = format!("task {id}: apply {result}");
        if !files.is_empty() {
            let _ = write!(text, "\nfiles: {}", files.join(", "));
        }
        if !conflicts.is_empty() {
            let _ = write!(
                text,
                "\nconflicts (resolve the markers, then `git add` each): {}",
                conflicts.join(", ")
            );
        }
        if !message.is_empty() {
            let _ = write!(text, "\n{message}");
        }
        if result == "failed" || result == "unknown" {
            Err(text)
        } else {
            Ok(text)
        }
    }

    fn known(&self, arguments: &Value) -> Result<String, String> {
        let id = arguments
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .ok_or("`id` is required: the task id `task` returned")?;
        let tasks = lock(&self.tasks);
        if tasks.tasks.contains_key(id) {
            Ok(id.to_owned())
        } else {
            let known = tasks.tasks.keys().cloned().collect::<Vec<_>>();
            Err(if known.is_empty() {
                format!("no task {id}: no task was started by this server")
            } else {
                format!("no task {id}; tasks: {}", known.join(", "))
            })
        }
    }

    /// A client cancelled a call: a blocking `task` stops its child (the
    /// in-job marsh relays the signal, which cancels the child job or the
    /// split); a waiting `task_output` stops waiting.
    fn cancel_call(&self, request: &Value) {
        let task = {
            let mut calls = lock(&self.calls);
            let Some(call) = calls.get_mut(&request.to_string()) else {
                return;
            };
            call.cancelled = true;
            call.task.clone()
        };
        if let Some(id) = task
            && let Some(Task {
                state: State::Running { pid },
                ..
            }) = lock(&self.tasks).tasks.get(&id)
        {
            terminate(*pid);
        }
        self.changed.notify_all();
    }

    /// stdin closed: the agent is gone. No task starts any more; every
    /// running one is stopped (TERM, then KILL after 15 s) and reaped.
    fn shutdown(&self) {
        let running = |tasks: &Tasks| {
            tasks
                .tasks
                .values()
                .filter_map(|task| match task.state {
                    State::Running { pid } => Some(pid),
                    State::Done(_) => None,
                })
                .collect::<Vec<_>>()
        };
        let mut tasks = lock(&self.tasks);
        tasks.closing = true;
        for pid in running(&tasks) {
            terminate(pid);
        }
        for (grace, kill) in [
            (Duration::from_secs(15), true),
            (Duration::from_secs(5), false),
        ] {
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline && !running(&tasks).is_empty() {
                tasks = self
                    .changed
                    .wait_timeout(tasks, Duration::from_millis(200))
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            if kill {
                for pid in running(&tasks) {
                    signal(pid, nix::sys::signal::Signal::SIGKILL);
                }
            }
        }
    }
}

use std::fmt::Write as _;

fn terminate(pid: u32) {
    signal(pid, nix::sys::signal::Signal::SIGTERM);
}

fn signal(pid: u32, signal: nix::sys::signal::Signal) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal);
    }
}

fn shared_outcome(directory: &Path, code: Option<i32>, signal: Option<i32>) -> Outcome {
    let stderr = read_capped(&directory.join("stderr"), STDERR_TAIL, true);
    let mut outcome = Outcome {
        code,
        status: exit_words(code, signal),
        message: read_capped(&directory.join("stdout"), MESSAGE_CAP, false),
        transcript: directory.join("stdout"),
        stderr_path: directory.join("stderr"),
        ..Outcome::default()
    };
    // `marsh run` exits 125 (126 from the spawn set) when admission refused
    // the child; the reason is on its stderr. `watch` clears this when the
    // child's job was found (then it ran and the agent chose that status).
    if matches!(code, Some(125 | 126)) && outcome.message.is_empty() {
        outcome.refused = Some(refusal_text(&stderr, code, signal));
    }
    if code != Some(0) {
        outcome.stderr = stderr;
    }
    outcome
}

fn refusal_text(stderr: &str, code: Option<i32>, signal: Option<i32>) -> String {
    let reason = stderr
        .lines()
        .map(|line| line.strip_prefix("marsh: ").unwrap_or(line).trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if reason.is_empty() {
        format!("the task did not start ({})", exit_words(code, signal))
    } else {
        format!("the task did not start: {reason}")
    }
}

fn exit_words(code: Option<i32>, signal: Option<i32>) -> String {
    match (code, signal) {
        (Some(130), _) => "cancelled (exit 130)".into(),
        (Some(code), _) => format!("exited {code}"),
        (None, Some(signal)) => format!("killed by signal {signal}"),
        (None, None) => "ended (status unknown)".into(),
    }
}

/// The task's job label, `task: DESCRIPTION`. The model a task runs is the
/// job's own `model` field (from its arguments), not part of the label.
fn task_label(request: &TaskRequest) -> String {
    format!("{LABEL_PREFIX}{}", request.description)
}

/// `codex` or `codex, model gpt-6.1-terra`.
fn agent_words(request: &TaskRequest) -> String {
    match &request.model {
        Some(model) => format!("{}, model {model}", request.agent),
        None => request.agent.clone(),
    }
}

fn files_word(files: Files) -> &'static str {
    match files {
        Files::Shared => "shared",
        Files::Copy => "copy",
    }
}

/// The text of a task's state: running, refused, or its result.
fn render(task: &Task, transcript: bool) -> Result<String, String> {
    let id = &task.id;
    let request = &task.request;
    let job = task.job.as_deref().map_or_else(String::new, |job| {
        format!("; job {job} (`marsh jobs show {job}`)")
    });
    let outcome = match &task.state {
        State::Running { .. } => {
            return Ok(format!(
                "task {id} ({}, \"{}\", files {}) is running ({}s){job}. \
                 task_output(id: \"{id}\", wait: true) waits for it.",
                agent_words(request),
                request.description,
                files_word(request.files),
                task.started.elapsed().as_secs()
            ));
        }
        State::Done(outcome) => outcome,
    };
    if let Some(refusal) = &outcome.refused {
        return Err(format!("task {id} ({}): {refusal}", agent_words(request)));
    }
    let mut text = format!(
        "task {id} ({}, \"{}\", files {}) {}\ntranscript: {} (stderr: {}){job}\n",
        agent_words(request),
        request.description,
        files_word(request.files),
        outcome.status,
        outcome.transcript.display(),
        outcome.stderr_path.display(),
    );
    if transcript {
        let _ = write!(
            text,
            "\n--- whole output ---\n{}\n",
            read_capped(&outcome.transcript, TRANSCRIPT_CAP, false)
        );
    } else {
        let message = if outcome.message.trim().is_empty() {
            "(no final message)"
        } else {
            outcome.message.trim_end()
        };
        let _ = write!(text, "\n--- final message ---\n{message}\n");
    }
    if !outcome.stderr.trim().is_empty() {
        let _ = write!(
            text,
            "\n--- stderr (tail) ---\n{}\n",
            outcome.stderr.trim_end()
        );
    }
    if request.files == Files::Copy {
        match &outcome.patch {
            Some(patch) => {
                let _ = write!(
                    text,
                    "\n--- diff ({}; full patch: {}) ---\n{}\n",
                    marsh_daemon::split::plural(outcome.files.len(), "file", "files"),
                    patch.display(),
                    read_capped(patch, DIFF_CAP, false).trim_end()
                );
                let _ = write!(
                    text,
                    "\n{}",
                    if task.applied {
                        format!("This diff is applied (task_apply(id: \"{id}\")).")
                    } else {
                        format!(
                            "Your folder is unchanged. Apply it with task_apply(id: \"{id}\"), or drop it."
                        )
                    }
                );
            }
            None => text.push_str("\nIt changed no files.\n"),
        }
    }
    Ok(text)
}

/// Up to `cap` bytes of `path` (its tail when `tail`), lossy UTF-8, with a
/// note when cut; never reads more than `cap` bytes.
fn read_capped(path: &Path, cap: usize, tail: bool) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map_or(0, |metadata| metadata.len());
    let cap64 = cap as u64;
    if length > cap64 && tail && file.seek(std::io::SeekFrom::Start(length - cap64)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = (&mut file).take(cap64).read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes).into_owned();
    match (length > cap64, tail) {
        (false, _) => text,
        (true, true) => format!("[... first {} bytes cut]\n{text}", length - cap64),
        (true, false) => format!(
            "{text}\n[... cut at {cap} of {length} bytes; the whole text is in {}]",
            path.display()
        ),
    }
}

/// The first `cap` bytes of a small file, lossy UTF-8.
fn read_head(path: &Path, cap: u64) -> String {
    let mut bytes = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(cap).read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&read_head(path, 4 << 20)).ok()
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(spawn: &[&str], narrowed: Option<&[&str]>) -> Setup {
        let document = JobDocument {
            job: "0123456789abcdef".into(),
            name: "claude".into(),
            spawn: spawn.iter().map(|name| (*name).to_owned()).collect(),
            prompts: [
                ("claude", vec!["-p", "{{.Prompt}}"]),
                ("codex", vec!["exec", "{{.Prompt}}"]),
                ("pi", vec!["-p", "{{.Prompt}}"]),
            ]
            .into_iter()
            .map(|(name, verbs)| {
                (
                    name.to_owned(),
                    verbs.into_iter().map(str::to_owned).collect(),
                )
            })
            .collect(),
            models: [
                ("claude", vec!["--model", "{{.Model}}"]),
                ("codex", vec!["-m", "{{.Model}}"]),
            ]
            .into_iter()
            .map(|(name, verbs)| {
                (
                    name.to_owned(),
                    verbs.into_iter().map(str::to_owned).collect(),
                )
            })
            .collect(),
            ..JobDocument::default()
        };
        let narrowed = narrowed.map(|names| {
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        });
        Setup::from_document(&document, narrowed.as_deref())
    }

    #[test]
    fn defaults_are_the_callers_agent_shared_and_blocking() {
        let setup = setup(&["claude", "codex", "shell"], None);
        let request = task_request(
            &setup,
            &json!({"description": "fix it", "prompt": "fix the bug"}),
        )
        .unwrap();
        assert_eq!(
            request,
            TaskRequest {
                description: "fix it".into(),
                agent: "claude".into(),
                model: None,
                argv: vec!["-p".into(), "fix the bug".into()],
                files: Files::Shared,
                background: false,
            }
        );
        let request = task_request(
            &setup,
            &json!({"prompt": "review", "agent": "codex", "files": "copy", "background": true}),
        )
        .unwrap();
        assert_eq!(request.argv, ["exec", "review"]);
        assert_eq!((request.files, request.background), (Files::Copy, true));
        assert_eq!(request.description, "review");
    }

    #[test]
    fn agents_follow_the_spawn_set_and_refusals_say_why() {
        let narrowed = setup(&["claude", "codex"], Some(&["codex"]));
        assert_eq!(narrowed.agents.keys().collect::<Vec<_>>(), ["codex"]);
        // The caller is not startable, but the only agent is the default.
        assert_eq!(narrowed.default_agent(), Some("codex"));
        let error =
            task_request(&narrowed, &json!({"prompt": "x", "agent": "claude"})).unwrap_err();
        assert!(
            error.contains("`claude` is not an agent this job may start; available: codex"),
            "{error}"
        );
        let none = setup(&[], None);
        let error = task_request(&none, &json!({"prompt": "x"})).unwrap_err();
        assert!(error.contains("--no-spawn"), "{error}");
        let tools = setup(&["claude", "codex"], None).tools();
        let description = tools[0]["description"].as_str().unwrap();
        assert!(description.starts_with("Hand work to another agent. Fast and cheap"));
        assert!(
            description.ends_with("Agents: claude (you), codex."),
            "{description}"
        );
        assert_eq!(
            tools[0]["inputSchema"]["properties"]["agent"]["enum"],
            json!(["claude", "codex"])
        );
        let error = task_request(&none, &json!({"description": "x"})).unwrap_err();
        assert!(error.contains("`prompt` is required"));
        let error = task_request(
            &setup(&["claude"], None),
            &json!({"prompt": "x", "files": "fork"}),
        )
        .unwrap_err();
        assert!(error.contains("\"shared\" or \"copy\""), "{error}");
    }

    #[test]
    fn model_goes_to_the_agents_model_option_or_is_refused() {
        let setup = setup(&["claude", "codex", "pi"], None);
        let request = task_request(
            &setup,
            &json!({"description": "review the change", "prompt": "review", "agent": "codex", "model": "gpt-6.1-terra"}),
        )
        .unwrap();
        assert_eq!(request.argv, ["exec", "review", "-m", "gpt-6.1-terra"]);
        assert_eq!(task_label(&request), "task: review the change");
        assert_eq!(agent_words(&request), "codex, model gpt-6.1-terra");
        let request = task_request(&setup, &json!({"prompt": "x", "model": "sonnet"})).unwrap();
        assert_eq!(request.argv, ["-p", "x", "--model", "sonnet"]);
        // Pi's Kit (here) declares no model option.
        let error =
            task_request(&setup, &json!({"prompt": "x", "agent": "pi", "model": "m"})).unwrap_err();
        assert!(
            error.contains("`pi` takes no `model`")
                && error.contains("agents that do: claude, codex"),
            "{error}"
        );
        for bad in [json!("--dangerously"), json!("a b"), json!(7)] {
            let error = task_request(&setup, &json!({"prompt": "x", "model": bad})).unwrap_err();
            assert!(error.contains("`model`"), "{error}");
        }
        // No model: unchanged argv and label.
        let request = task_request(&setup, &json!({"prompt": "x", "model": ""})).unwrap();
        assert_eq!(
            (request.model, request.argv),
            (None, vec!["-p".to_owned(), "x".to_owned()])
        );
        let tools = setup.tools();
        let model = tools[0]["inputSchema"]["properties"]["model"]["description"]
            .as_str()
            .unwrap();
        assert!(
            model.ends_with("Agents that take one: claude, codex."),
            "{model}"
        );
    }

    #[test]
    fn descriptions_are_bounded_labels() {
        let setup = setup(&["claude"], None);
        let long = "word ".repeat(40);
        let error = task_request(&setup, &json!({"prompt": "x", "description": long})).unwrap_err();
        assert!(error.contains("at most 120 bytes"), "{error}");
        let error =
            task_request(&setup, &json!({"prompt": "x", "description": "a\nb"})).unwrap_err();
        assert!(error.contains("no control characters"), "{error}");
        let request = task_request(
            &setup,
            &json!({"prompt": "fix\tthe\u{1b}[31m bug now please, thanks a lot"}),
        )
        .unwrap();
        assert_eq!(request.description, "fix the[31m bug now please, thanks");
        assert!(LABEL_PREFIX.len() + MAX_DESCRIPTION_BYTES <= 128);
    }
}
