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
use std::io::{BufRead as _, Write as _};
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

const TASK_DESCRIPTION: &str = "Hand work to another agent. Fast and cheap: use it whenever you would hand \
work off — parallel parts, a second opinion, a review, a tool you don't hold. Defaults: the same agent as \
you, working in your folder. Set `agent` for another one (a different agent for reviews). Set `files: \
\"copy\"` when its edits could collide with yours or another task's, or for competing attempts and risky \
changes: it then works on a private copy and you get its diff to apply; read-only and independent work can \
share the folder.";

/// What `task-mcp` knows at start: this job and the agents it may start.
#[derive(Clone, Debug)]
struct Setup {
    /// This job's id (`job:<id>` is its children's parent).
    job: String,
    /// The calling agent's registered name: the default `agent`.
    caller: String,
    /// Agent name -> prompt verbs, limited to this job's spawn set.
    agents: BTreeMap<String, Vec<String>>,
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
            job: document.job.clone(),
            caller: document.name.clone(),
            agents,
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
        json!([
            {
                "name": "task",
                "description": format!("{TASK_DESCRIPTION}\n\nAgents: {agents}."),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "description": {"type": "string", "description": "3-6 words, shown to the user."},
                        "prompt": {"type": "string", "description": "Everything the agent needs; it does not see this conversation."},
                        "agent": agent,
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
    let description = text("description")
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map_or_else(
            || {
                prompt
                    .split_whitespace()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(" ")
            },
            str::to_owned,
        );
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
    let argv = verbs
        .iter()
        .map(|verb| verb.replace(contract::PROMPT_PLACEHOLDER, prompt))
        .collect();
    Ok(TaskRequest {
        description,
        agent,
        argv,
        files,
        background,
    })
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
struct Tasks {
    next: u64,
    tasks: BTreeMap<String, Task>,
}

struct Server {
    setup: Setup,
    tasks: Mutex<Tasks>,
    changed: Condvar,
    out: Mutex<std::io::Stdout>,
    /// Request id -> task id, for blocking calls a client may cancel.
    calls: Mutex<BTreeMap<String, String>>,
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
    let setup = Setup::from_document(&document, narrowed.as_deref());
    let server = Arc::new(Server {
        setup,
        tasks: Mutex::new(Tasks::default()),
        changed: Condvar::new(),
        out: Mutex::new(std::io::stdout()),
        calls: Mutex::new(BTreeMap::new()),
    });
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
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
                let server = Arc::clone(server);
                std::thread::spawn(move || {
                    let result = server.call(&id, &params);
                    lock(&server.calls).remove(&id.to_string());
                    server.reply(&id, &result);
                });
            }
            _ => server.send(&json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": format!("method not found: {method}")}
            })),
        }
    }

    fn call(self: &Arc<Self>, id: &Value, params: &Value) -> Value {
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let progress = params
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"))
            .cloned();
        let result = match params["name"].as_str().unwrap_or_default() {
            "task" => self.task(id, &arguments, progress.as_ref()),
            "task_output" => self.task_output(&arguments, progress.as_ref()),
            "task_apply" => self.task_apply(&arguments),
            other => Err(format!("unknown tool `{other}`")),
        };
        match result {
            Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
            Err(text) => json!({"content": [{"type": "text", "text": text}], "isError": true}),
        }
    }

    fn task(
        self: &Arc<Self>,
        call: &Value,
        arguments: &Value,
        progress: Option<&Value>,
    ) -> Result<String, String> {
        let request = task_request(&self.setup, arguments)?;
        let id = {
            let mut tasks = lock(&self.tasks);
            tasks.next += 1;
            format!("t{}", tasks.next)
        };
        let directory = self.setup.dir.join(&id);
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
        let (child, start) = self.spawn(&request, &id, &directory)?;
        let background = request.background;
        let started = format!(
            "Task {id} started in the background ({}, files {}). Get its result with \
             task_output(id: \"{id}\", wait: true), or keep working and check later.",
            request.agent,
            files_word(request.files),
        );
        lock(&self.tasks).tasks.insert(
            id.clone(),
            Task {
                id: id.clone(),
                request: request.clone(),
                started: Instant::now(),
                job: None,
                state: State::Running { pid: child.id() },
                applied: false,
            },
        );
        if !background {
            lock(&self.calls).insert(call.to_string(), id.clone());
        }
        let server = Arc::clone(self);
        let watched = id.clone();
        std::thread::spawn(move || server.watch(&watched, &request, child, &directory, start));
        if background {
            return Ok(started);
        }
        self.wait(&id, progress);
        let tasks = lock(&self.tasks);
        render(&tasks.tasks[&id], false)
    }

    /// Start the child through this job's own marsh: `marsh run AGENT VERBS`
    /// (shared) or a one-branch argv split (copy).
    fn spawn(
        &self,
        request: &TaskRequest,
        id: &str,
        directory: &Path,
    ) -> Result<(std::process::Child, i64), String> {
        let file = |name: &str| {
            std::fs::File::create(directory.join(name))
                .map_err(|error| format!("cannot create {}/{name}: {error}", directory.display()))
        };
        let mut command = Command::new(&self.setup.marsh);
        match request.files {
            Files::Shared => {
                command.arg("run").arg(&request.agent).args(&request.argv);
            }
            Files::Copy => {
                command
                    .args(["split", "-n", ":::", id])
                    .arg(&request.agent)
                    .args(&request.argv);
            }
        }
        let start = now_unix_ms();
        let child = command
            .stdin(Stdio::null())
            .stdout(file(match request.files {
                Files::Shared => "stdout",
                Files::Copy => "handle",
            })?)
            .stderr(file("stderr")?)
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", self.setup.marsh.display()))?;
        Ok((child, start))
    }

    /// Follow one task to its end, then record its outcome.
    fn watch(
        &self,
        id: &str,
        request: &TaskRequest,
        mut child: std::process::Child,
        directory: &Path,
        start: i64,
    ) {
        let mut looked = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(_) => break child.wait().ok(),
            }
            let unknown = lock(&self.tasks)
                .tasks
                .get(id)
                .is_some_and(|task| task.job.is_none());
            if request.files == Files::Shared
                && unknown
                && looked.elapsed() >= Duration::from_millis(500)
            {
                looked = Instant::now();
                if let Some(job) = self.find_child(request, start) {
                    if let Some(task) = lock(&self.tasks).tasks.get_mut(id) {
                        task.job = Some(job);
                    }
                    self.changed.notify_all();
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let code = status.and_then(|status| status.code());
        let signal = status.and_then(|status| status.signal());
        let mut outcome = match request.files {
            Files::Shared => shared_outcome(directory, code, signal),
            Files::Copy => self.copy_outcome(directory, code, signal),
        };
        let known = lock(&self.tasks)
            .tasks
            .get(id)
            .and_then(|task| task.job.clone());
        let job = match request.files {
            Files::Shared => known.or_else(|| self.find_child(request, start)),
            Files::Copy => outcome.split.as_ref().and_then(|_| {
                read_json(&directory.join("manifest.json")).and_then(|manifest| {
                    manifest["branches"][0]["job_id"]
                        .as_str()
                        .map(str::to_owned)
                })
            }),
        };
        if job.is_some() {
            outcome.refused = None;
        }
        if let Some(task) = lock(&self.tasks).tasks.get_mut(id) {
            task.job = job.or_else(|| task.job.clone());
            task.state = State::Done(Box::new(outcome));
        }
        self.changed.notify_all();
    }

    /// This job's child for `request`, from the job's subtree: the newest
    /// one with the agent's name and arguments started since `start` that
    /// no other task claimed.
    fn find_child(&self, request: &TaskRequest, start: i64) -> Option<String> {
        let client = marsh_daemon::Client::capability(Path::new(SOCKET));
        let marsh_daemon::PublicReply::ProcessTree { document } = client
            .request(marsh_daemon::PublicRequest::ProcessShow)
            .ok()?
        else {
            return None;
        };
        let claimed = lock(&self.tasks)
            .tasks
            .values()
            .filter_map(|task| task.job.clone())
            .collect::<Vec<_>>();
        let parent = format!("job:{}", self.setup.job);
        let mut found = Vec::new();
        collect_children(&document["jobs"], &parent, &mut found);
        found
            .into_iter()
            .filter(|node| {
                node["command"] == request.agent.as_str()
                    && node["created_unix_ms"].as_i64().unwrap_or(0) >= start - 2_000
                    && args_match(&node["args"], &request.argv)
            })
            .filter_map(|node| node["job_id"].as_str().map(str::to_owned))
            .find(|job| !claimed.contains(job))
    }

    fn copy_outcome(&self, directory: &Path, code: Option<i32>, signal: Option<i32>) -> Outcome {
        let mut outcome = Outcome {
            transcript: directory.join("stdout"),
            stderr_path: directory.join("stderr"),
            ..Outcome::default()
        };
        let errors = read_capped(&directory.join("stderr"), STDERR_TAIL, true);
        let handle = std::fs::read_to_string(directory.join("handle")).unwrap_or_default();
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
        outcome.files = std::fs::read_to_string(lane.join("files"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        if branch["job_id"].is_null() && outcome.code != Some(0) {
            outcome.refused = Some(refusal_text(&outcome.stderr, outcome.code, signal));
        }
        outcome
    }

    /// Wait for a task to finish, sending progress when the client asked.
    fn wait(&self, id: &str, progress: Option<&Value>) {
        let started = Instant::now();
        let mut tick = 0_u64;
        let mut tasks = lock(&self.tasks);
        loop {
            match tasks.tasks.get(id) {
                Some(Task {
                    state: State::Running { .. },
                    ..
                }) => {}
                _ => return,
            }
            let (next, _) = self
                .changed
                .wait_timeout(tasks, PROGRESS_EVERY)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tasks = next;
            if let Some(token) = progress
                && started.elapsed() >= PROGRESS_EVERY * u32::try_from(tick + 1).unwrap_or(u32::MAX)
            {
                tick += 1;
                let running = matches!(
                    tasks.tasks.get(id).map(|task| &task.state),
                    Some(State::Running { .. })
                );
                if running {
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
    }

    fn task_output(&self, arguments: &Value, progress: Option<&Value>) -> Result<String, String> {
        let id = self.known(arguments)?;
        let flag = |name: &str| {
            arguments
                .get(name)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        if flag("wait") {
            self.wait(&id, progress);
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

    /// A client cancelled a blocking `task` call: stop its child (the in-job
    /// marsh relays the signal, which cancels the child job or the split).
    fn cancel_call(&self, request: &Value) {
        let Some(id) = lock(&self.calls).remove(&request.to_string()) else {
            return;
        };
        if let Some(Task {
            state: State::Running { pid },
            ..
        }) = lock(&self.tasks).tasks.get(&id)
        {
            terminate(*pid);
        }
    }

    /// stdin closed: the agent is gone. Stop every running task and wait
    /// briefly for them to end.
    fn shutdown(&self) {
        for task in lock(&self.tasks).tasks.values() {
            if let State::Running { pid } = task.state {
                terminate(pid);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut tasks = lock(&self.tasks);
        while Instant::now() < deadline
            && tasks
                .tasks
                .values()
                .any(|task| matches!(task.state, State::Running { .. }))
        {
            tasks = self
                .changed
                .wait_timeout(tasks, Duration::from_millis(200))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

use std::fmt::Write as _;

fn terminate(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
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
                request.agent,
                request.description,
                files_word(request.files),
                task.started.elapsed().as_secs()
            ));
        }
        State::Done(outcome) => outcome,
    };
    if let Some(refusal) = &outcome.refused {
        return Err(format!("task {id} ({}): {refusal}", request.agent));
    }
    let mut text = format!(
        "task {id} ({}, \"{}\", files {}) {}\ntranscript: {} (stderr: {}){job}\n",
        request.agent,
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
/// note when cut.
fn read_capped(path: &Path, cap: usize, tail: bool) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    if bytes.len() <= cap {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    if tail {
        format!(
            "[... first {} bytes cut]\n{}",
            bytes.len() - cap,
            String::from_utf8_lossy(&bytes[bytes.len() - cap..])
        )
    } else {
        format!(
            "{}\n[... cut at {cap} of {} bytes; the whole text is in {}]",
            String::from_utf8_lossy(&bytes[..cap]),
            bytes.len(),
            path.display()
        )
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
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

fn collect_children<'a>(nodes: &'a Value, parent: &str, found: &mut Vec<&'a Value>) {
    for node in nodes.as_array().into_iter().flatten() {
        if node["node"] == "job" && node["lineage"]["parent"] == parent {
            found.push(node);
        }
        collect_children(&node["children"], parent, found);
    }
}

/// A receipt's display arguments are the first 16, each cut to 200
/// characters: each must be a prefix of the argument we passed.
fn args_match(recorded: &Value, argv: &[String]) -> bool {
    let recorded = string_list(recorded);
    recorded.len() <= argv.len()
        && recorded
            .iter()
            .zip(argv)
            .all(|(shown, passed)| passed.starts_with(shown.trim_end_matches('…')))
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
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
    fn display_arguments_match_by_prefix() {
        let long = "x".repeat(300);
        assert!(args_match(
            &json!(["-p", "x".repeat(200)]),
            &["-p".into(), long.clone()]
        ));
        assert!(!args_match(&json!(["-p", "y"]), &["-p".into(), long]));
        assert!(args_match(&json!([]), &["-p".into()]));
    }
}
