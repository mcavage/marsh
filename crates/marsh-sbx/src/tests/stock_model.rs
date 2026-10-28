//! Stateful stand-in for stock `sbx` (and the host `docker buildx` marsh runs
//! beside it): a VM inventory with guest files, images and mounts that answers
//! each invocation by verb and flags. Flags are checked against the captured
//! `--help` of the selected version, so a flag stock SBX does not accept fails
//! here as it would for real. Nothing here runs a real SBX or VM.
use super::corpus::{self, InventoryShape, StockVersion};
use super::*;
use std::os::unix::net::UnixStream;

pub(super) type Matcher = Arc<dyn Fn(&Call) -> bool + Send + Sync>;

/// Long enough that only a hung adapter (a real failure) reaches it.
pub(super) const WAIT_LIMIT: Duration = Duration::from_secs(30);

/// One recorded invocation.
#[derive(Clone, Debug)]
pub(super) struct Call {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
}

impl Call {
    fn new(invocation: &Invocation) -> Self {
        let text = |value: &OsStr| value.to_string_lossy().into_owned();
        Self {
            program: invocation.program.clone(),
            args: invocation.arguments.iter().map(|a| text(a)).collect(),
            cwd: invocation.working_directory.clone(),
            env: invocation
                .environment
                .iter()
                .map(|(key, value)| (text(key), text(value)))
                .collect(),
        }
    }

    /// `buildx` for the host build, otherwise the stock verb.
    pub fn verb(&self) -> &str {
        self.args.first().map_or("", String::as_str)
    }

    pub fn is_host_build(&self) -> bool {
        self.program == Path::new("docker") && self.verb() == "buildx"
    }

    /// The value after `flag` (or in `flag=value`), wherever it appears.
    pub fn value(&self, flag: &str) -> Option<&str> {
        self.values(flag).into_iter().next()
    }

    pub fn values(&self, flag: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let mut words = self.args.iter();
        while let Some(word) = words.next() {
            if word == flag {
                if let Some(value) = words.next() {
                    found.push(value.as_str());
                }
            } else if let Some(value) = word
                .strip_prefix(flag)
                .and_then(|rest| rest.strip_prefix('='))
            {
                found.push(value);
            }
        }
        found
    }

    pub fn env_value(&self, name: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The guest command of an `sbx exec`.
    pub fn guest(&self) -> Vec<&str> {
        // Words after the sandbox are never flags: they are the trailing args.
        let command = parse(&self.args[1..], &corpus::flags(corpus::EXEC_HELP), true)
            .map_or(0, |parsed| parsed.positionals.len().saturating_sub(1));
        self.args[self.args.len() - command..]
            .iter()
            .map(String::as_str)
            .collect()
    }

    fn read_only(&self) -> bool {
        match self.verb() {
            "ls" | "inspect" | "version" | "template" => true,
            "create" => self.args.iter().any(|arg| arg == "--help"),
            "exec" => matches!(
                self.guest().as_slice(),
                ["cat", ..] | ["docker", "info", ..] | ["docker", "image", "inspect", ..]
            ),
            _ => false,
        }
    }
}

struct Parsed {
    flags: Vec<(String, Option<String>)>,
    positionals: Vec<String>,
}

impl Parsed {
    fn value(&self, long: &str) -> Option<&str> {
        self.values(long).into_iter().next()
    }

    fn values(&self, long: &str) -> Vec<&str> {
        self.flags
            .iter()
            .filter(|(name, _)| name == long)
            .filter_map(|(_, value)| value.as_deref())
            .collect()
    }

    fn has(&self, long: &str) -> bool {
        self.flags.iter().any(|(name, _)| name == long)
    }
}

/// Cobra-style parse against `flags`. `stop_at_positional` models verbs whose
/// trailing words belong to another program (`exec SANDBOX COMMAND...`).
fn parse(
    words: &[String],
    flags: &[corpus::Flag],
    stop_at_positional: bool,
) -> Result<Parsed, String> {
    let mut parsed = Parsed {
        flags: Vec::new(),
        positionals: Vec::new(),
    };
    let mut words = words.iter();
    while let Some(word) = words.next() {
        if (stop_at_positional && !parsed.positionals.is_empty()) || word == "--" {
            if word != "--" {
                parsed.positionals.push(word.clone());
            }
            parsed.positionals.extend(words.by_ref().cloned());
            break;
        }
        let flag = if let Some(long) = word.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, None), |(name, value)| (name, Some(value.to_owned())));
            let flag = flags
                .iter()
                .find(|flag| flag.long == name)
                .ok_or_else(|| format!("unknown flag: --{name}"))?;
            Some((flag, inline))
        } else if let Some(short) = word.strip_prefix('-').filter(|short| short.len() == 1) {
            let short = short.chars().next().unwrap_or_default();
            let flag = flags
                .iter()
                .find(|flag| flag.short == Some(short))
                .ok_or_else(|| format!("unknown shorthand flag: '{short}' in -{short}"))?;
            Some((flag, None))
        } else {
            None
        };
        match flag {
            Some((flag, inline)) => {
                let value = if flag.takes_value {
                    Some(match inline {
                        Some(value) => value,
                        None => words
                            .next()
                            .cloned()
                            .ok_or_else(|| format!("flag needs an argument: --{}", flag.long))?,
                    })
                } else {
                    None
                };
                parsed.flags.push((flag.long.clone(), value));
            }
            None => parsed.positionals.push(word.clone()),
        }
    }
    Ok(parsed)
}

/// One stock VM as the model sees it.
#[derive(Clone, Debug, Default)]
pub(super) struct Vm {
    pub id: String,
    pub running: bool,
    pub agent: String,
    pub workspaces: Vec<PathBuf>,
    pub files: BTreeMap<String, Vec<u8>>,
    /// Guest target -> (host path, mode).
    pub mounts: BTreeMap<PathBuf, (PathBuf, String)>,
    /// Image reference or tag -> image ID.
    pub images: BTreeMap<String, String>,
    /// What `sbx inspect --json` reports as `image_digest`.
    pub image_digest: Option<String>,
    /// Users already present in the guest image: name -> (uid, gid).
    pub users: BTreeMap<String, (u32, u32)>,
}

pub(super) enum Fault {
    /// Report this result without applying the effect.
    Respond(CommandOutput),
    /// Apply the effect, then report this result.
    FailAfterEffect(CommandOutput),
    /// The bounded call expired; `applied` says whether the effect landed.
    TimedOut { applied: bool },
    /// An attached spawn fails before starting.
    SpawnError(String),
}

/// Holds matching calls before their effect until released.
#[derive(Default)]
pub(super) struct Gate {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
}

impl Gate {
    fn pass(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        self.changed.notify_all();
        while !state.1 {
            state = self.changed.wait(state).unwrap();
        }
    }

    /// Wait until `count` calls have reached the gate.
    pub fn wait_entered(&self, count: usize) {
        let state = self.state.lock().unwrap();
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, WAIT_LIMIT, |state| state.0 < count)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "only {} of {count} calls reached the gate",
            state.0
        );
    }

    pub fn entered(&self) -> usize {
        self.state.lock().unwrap().0
    }

    pub fn release(&self) {
        self.state.lock().unwrap().1 = true;
        self.changed.notify_all();
    }
}

#[derive(Default)]
struct ProcState {
    exit: Mutex<Option<i32>>,
    changed: Condvar,
    socket: Mutex<Option<UnixStream>>,
    wedge: Option<Arc<Gate>>,
}

impl ProcState {
    fn finish(&self, code: i32) {
        self.exit.lock().unwrap().get_or_insert(code);
        self.changed.notify_all();
        if let Some(socket) = self.socket.lock().unwrap().take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

struct ModelProcess(Arc<ProcState>);

impl AttachedProcess for ModelProcess {
    fn wait(&mut self) -> io::Result<i32> {
        let exit = self.0.exit.lock().unwrap();
        let exit = self
            .0
            .changed
            .wait_while(exit, |exit| exit.is_none())
            .unwrap();
        Ok(exit.unwrap_or_default())
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(*self.0.exit.lock().unwrap())
    }

    fn terminate(&mut self) -> io::Result<()> {
        if let Some(wedge) = &self.0.wedge {
            wedge.pass();
        }
        self.0.finish(143);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcKind {
    Worker,
    Supervisor,
}

struct Running {
    vm: String,
    kind: ProcKind,
    state: Arc<ProcState>,
}

#[derive(Default)]
struct State {
    vms: BTreeMap<String, Vm>,
    calls: Vec<Call>,
    faults: Vec<(Matcher, Fault)>,
    gates: Vec<(Matcher, Arc<Gate>)>,
    next_id: u64,
    /// `repository:tag` -> digest a VM created from it reports.
    templates: BTreeMap<String, String>,
    /// Users present in every newly created guest image.
    template_users: BTreeMap<String, (u32, u32)>,
    /// `docker pull` reference -> image ID.
    registry: BTreeMap<String, String>,
    host_build_digest: String,
    stock_kit_digest: String,
    stock_rebuild_digest: String,
    /// 0.46-style: re-mounting an existing target fails instead of no-op.
    remount_conflicts: bool,
    active: BTreeMap<(String, String), usize>,
    max_active: BTreeMap<(String, String), usize>,
    running: Vec<Running>,
    supervisor_frames: Vec<shell_supervisor::Down>,
    worker_requests: Vec<(String, marsh_worker::WorkerRequest)>,
    /// Every byte written to each VM's worker input.
    worker_input: BTreeMap<String, Vec<u8>>,
    worker_ready_gate: Option<Arc<Gate>>,
    worker_input_gate: Option<Arc<Gate>>,
    fail_worker_flush: bool,
    wedge_worker_termination: Option<Arc<Gate>>,
    wrong_supervisor_generation: bool,
}

pub(super) struct StockModel {
    version: &'static StockVersion,
    state: Mutex<State>,
    changed: Condvar,
    this: std::sync::Weak<StockModel>,
}

pub(super) const HOST_BUILD_DIGEST: &str =
    "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
pub(super) const STALE_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn output(code: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> CommandOutput {
    CommandOutput {
        exit_code: Some(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    }
}

fn success(stdout: impl Into<Vec<u8>>) -> CommandOutput {
    output(0, stdout, Vec::new())
}

pub(super) fn failure(stderr: &str) -> CommandOutput {
    output(1, Vec::new(), stderr.as_bytes().to_vec())
}

impl StockModel {
    pub fn new() -> Arc<Self> {
        Self::of(&corpus::V047)
    }

    pub fn of(version: &'static StockVersion) -> Arc<Self> {
        Arc::new_cyclic(|this| Self {
            version,
            state: Mutex::new(State {
                host_build_digest: HOST_BUILD_DIGEST.into(),
                stock_kit_digest: HOST_BUILD_DIGEST.into(),
                stock_rebuild_digest: HOST_BUILD_DIGEST.into(),
                ..State::default()
            }),
            changed: Condvar::new(),
            this: this.clone(),
        })
    }

    pub fn adapter(self: &Arc<Self>) -> StockSbx {
        StockSbx::new("sbx", Arc::clone(self) as Arc<dyn CommandRunner>)
    }

    // ---- scenario setup -------------------------------------------------

    /// A VM present in stock inventory but not recorded by any adapter.
    pub fn seed_vm(&self, name: &str) -> String {
        let mut state = self.state.lock().unwrap();
        let id = Self::next_uuid(&mut state);
        state.vms.insert(
            name.to_owned(),
            Vm {
                id: id.clone(),
                running: true,
                agent: "shell".into(),
                ..Vm::default()
            },
        );
        id
    }

    /// A VM this adapter created and recorded.
    pub fn seed_owned(&self, adapter: &StockSbx, name: &str) -> String {
        let id = self.seed_vm(name);
        adapter.ownership.record(name, &id).unwrap();
        id
    }

    pub fn with_vm<R>(&self, name: &str, change: impl FnOnce(&mut Vm) -> R) -> R {
        change(
            self.state
                .lock()
                .unwrap()
                .vms
                .get_mut(name)
                .expect("model VM"),
        )
    }

    pub fn remove_vm(&self, name: &str) {
        self.state.lock().unwrap().vms.remove(name);
    }

    pub fn vm(&self, name: &str) -> Option<Vm> {
        self.state.lock().unwrap().vms.get(name).cloned()
    }

    pub fn vm_names(&self) -> Vec<String> {
        self.state.lock().unwrap().vms.keys().cloned().collect()
    }

    pub fn add_template(&self, reference: &str, digest: &str) {
        self.state
            .lock()
            .unwrap()
            .templates
            .insert(reference.into(), digest.into());
    }

    pub fn add_template_user(&self, name: &str, uid: u32, gid: u32) {
        self.state
            .lock()
            .unwrap()
            .template_users
            .insert(name.into(), (uid, gid));
    }

    pub fn publish(&self, reference: &str, image_id: &str) {
        self.state
            .lock()
            .unwrap()
            .registry
            .insert(reference.into(), image_id.into());
    }

    /// Digest of stock SBX's own Kit build without and with `SBX_KIT_REBUILD`.
    pub fn set_stock_kit_digests(&self, cached: &str, rebuilt: &str) {
        let mut state = self.state.lock().unwrap();
        state.stock_kit_digest = cached.into();
        state.stock_rebuild_digest = rebuilt.into();
    }

    pub fn set_remount_conflicts(&self) {
        self.state.lock().unwrap().remount_conflicts = true;
    }

    pub fn fault(&self, matcher: Matcher, fault: Fault) {
        self.state.lock().unwrap().faults.push((matcher, fault));
    }

    pub fn gate(&self, matcher: Matcher) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.state
            .lock()
            .unwrap()
            .gates
            .push((matcher, Arc::clone(&gate)));
        gate
    }

    pub fn gate_worker_ready(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.state.lock().unwrap().worker_ready_gate = Some(Arc::clone(&gate));
        gate
    }

    pub fn gate_worker_input(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.state.lock().unwrap().worker_input_gate = Some(Arc::clone(&gate));
        gate
    }

    /// The next flush of a worker's input reports an unknown completion.
    pub fn fail_next_worker_flush(&self) {
        self.state.lock().unwrap().fail_worker_flush = true;
    }

    pub fn wedge_worker_termination(&self) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        self.state.lock().unwrap().wedge_worker_termination = Some(Arc::clone(&gate));
        gate
    }

    pub fn answer_next_supervisor_with_wrong_generation(&self) {
        self.state.lock().unwrap().wrong_supervisor_generation = true;
    }

    /// End every retained process of `kind` in `vm`, as a VM-side crash would.
    fn kill(&self, vm: &str, kind: ProcKind) {
        let states = self
            .state
            .lock()
            .unwrap()
            .running
            .iter()
            .filter(|running| running.vm == vm && running.kind == kind)
            .map(|running| Arc::clone(&running.state))
            .collect::<Vec<_>>();
        for state in states {
            state.finish(137);
        }
    }

    pub fn kill_workers(&self, vm: &str) {
        self.kill(vm, ProcKind::Worker);
    }

    pub fn kill_supervisors(&self, vm: &str) {
        self.kill(vm, ProcKind::Supervisor);
    }

    // ---- observations ---------------------------------------------------

    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn calls_of(&self, verb: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.verb() == verb && !call.is_host_build())
            .collect()
    }

    pub fn count(&self, verb: &str) -> usize {
        self.calls_of(verb).len()
    }

    pub fn host_builds(&self) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.is_host_build())
            .count()
    }

    /// Calls that can change stock or guest state.
    pub fn mutations(&self) -> usize {
        self.calls().iter().filter(|call| !call.read_only()).count()
    }

    /// Spawns of a retained worker or supervisor (any VM).
    pub fn retained_spawns(&self, program: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.verb() == "exec" && call.guest().first() == Some(&program))
            .count()
    }

    pub fn max_concurrent(&self, verb: &str, vm: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .max_active
            .get(&(verb.to_owned(), vm.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    pub fn supervisor_frames(&self) -> Vec<shell_supervisor::Down> {
        self.state.lock().unwrap().supervisor_frames.clone()
    }

    /// Wait until the supervisors have received `count` start frames.
    pub fn wait_supervisor_starts(&self, count: usize) -> Vec<shell_supervisor::StartSpec> {
        let starts = |state: &State| {
            state
                .supervisor_frames
                .iter()
                .filter_map(|frame| match frame {
                    shell_supervisor::Down::Start { spec, .. } => Some(spec.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let state = self.state.lock().unwrap();
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, WAIT_LIMIT, |state| starts(state).len() < count)
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "supervisor start frames: {:?}",
            starts(&state)
        );
        starts(&state)
    }

    /// Attempts in the Start frames written to `vm`'s worker transport,
    /// whether or not the worker read them before it ended.
    pub fn written_worker_starts(&self, vm: &str) -> Vec<String> {
        let written = self
            .state
            .lock()
            .unwrap()
            .worker_input
            .get(vm)
            .cloned()
            .unwrap_or_default();
        let mut reader = Cursor::new(written);
        std::iter::from_fn(|| {
            marsh_worker::read_frame::<marsh_worker::WorkerRequest>(&mut reader).ok()
        })
        .filter_map(|request| match request {
            marsh_worker::WorkerRequest::Start { attempt, .. } => Some(attempt),
            _ => None,
        })
        .collect()
    }

    pub fn wait_worker_starts(&self, vm: &str, count: usize) {
        let state = self.state.lock().unwrap();
        let (_state, timeout) = self
            .changed
            .wait_timeout_while(state, WAIT_LIMIT, |state| {
                state
                    .worker_requests
                    .iter()
                    .filter(|(name, request)| {
                        name == vm && matches!(request, marsh_worker::WorkerRequest::Start { .. })
                    })
                    .count()
                    < count
            })
            .unwrap();
        assert!(
            !timeout.timed_out(),
            "worker did not receive {count} starts"
        );
    }

    // ---- dispatch -------------------------------------------------------

    fn next_uuid(state: &mut State) -> String {
        state.next_id += 1;
        let digest = marsh_contracts::process::hex(&Sha256::digest(state.next_id.to_be_bytes()));
        format!(
            "{}-{}-4{}-8{}-{}",
            &digest[..8],
            &digest[8..12],
            &digest[13..16],
            &digest[17..20],
            &digest[20..32]
        )
    }

    /// Record `call`, then pass its gate and take its fault, if any.
    fn admit(&self, call: &Call) -> Option<Fault> {
        let gates = {
            let mut state = self.state.lock().unwrap();
            state.calls.push(call.clone());
            state
                .gates
                .iter()
                .filter(|(matcher, _)| matcher(call))
                .map(|(_, gate)| Arc::clone(gate))
                .collect::<Vec<_>>()
        };
        for gate in gates {
            gate.pass();
        }
        let mut state = self.state.lock().unwrap();
        let index = state.faults.iter().position(|(matcher, _)| matcher(call))?;
        Some(state.faults.remove(index).1)
    }

    fn missing(name: &str) -> CommandOutput {
        output(1, Vec::new(), corpus::missing_vm_stderr(name))
    }

    fn respond(&self, call: &Call) -> io::Result<CommandOutput> {
        let fault = self.admit(call);
        let key = (
            call.verb().to_owned(),
            call.args.get(1).cloned().unwrap_or_default(),
        );
        {
            let mut state = self.state.lock().unwrap();
            let active = state.active.entry(key.clone()).or_default();
            *active += 1;
            let now = *active;
            let max = state.max_active.entry(key.clone()).or_default();
            *max = (*max).max(now);
        }
        let result = match fault {
            Some(Fault::Respond(output)) => Ok(output),
            Some(Fault::FailAfterEffect(output)) => self.apply(call).map(|_| output),
            Some(Fault::TimedOut { applied }) => {
                if applied {
                    self.apply(call)?;
                }
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "stock call expired",
                ))
            }
            Some(Fault::SpawnError(message)) => Err(io::Error::other(message)),
            None => self.apply(call),
        };
        *self.state.lock().unwrap().active.entry(key).or_default() -= 1;
        self.changed.notify_all();
        result
    }

    fn apply(&self, call: &Call) -> io::Result<CommandOutput> {
        if call.is_host_build() {
            return self.host_build(call);
        }
        if call.program != Path::new("sbx") {
            return Err(io::Error::other(format!("unexpected program {call:?}")));
        }
        let help = match call.verb() {
            "version" => return Ok(success(self.version.version_output)),
            "create" => self.version.create_help,
            "exec" => corpus::EXEC_HELP,
            "rm" => corpus::RM_HELP,
            "stop" => corpus::STOP_HELP,
            "cp" => corpus::CP_HELP,
            "mount" => corpus::MOUNT_HELP,
            "umount" => corpus::UMOUNT_HELP,
            "inspect" => corpus::INSPECT_HELP,
            "ls" => corpus::LS_HELP,
            "template" if call.args.get(1).is_some_and(|word| word == "ls") => {
                corpus::TEMPLATE_LS_HELP
            }
            "mcp" if call.args.get(1).is_some_and(|word| word == "load") => corpus::MCP_LOAD_HELP,
            _ => {
                return Ok(failure(&format!(
                    "unknown command {:?} for \"sbx\"",
                    call.verb()
                )));
            }
        };
        let skip = if matches!(call.verb(), "template" | "mcp") {
            2
        } else {
            1
        };
        let parsed = match parse(
            &call.args[skip..],
            &corpus::flags(help),
            call.verb() == "exec",
        ) {
            Ok(parsed) => parsed,
            Err(message) => return Ok(failure(&format!("Error: {message}"))),
        };
        if parsed.has("help") {
            return Ok(success(help));
        }
        let mut state = self.state.lock().unwrap();
        Ok(match call.verb() {
            "ls" => success(self.render_inventory(&state)),
            "template" => success(self.version.template_ls_json),
            "inspect" => {
                let name = parsed.positionals.first().cloned().unwrap_or_default();
                match state.vms.get(&name) {
                    None => Self::missing(&name),
                    Some(vm) => success(
                        serde_json::to_vec(&serde_json::json!({
                            "name": name,
                            "id": vm.id,
                            "agent": vm.agent,
                            "status": if vm.running { "running" } else { "stopped" },
                            "image_digest": vm.image_digest,
                        }))
                        .unwrap(),
                    ),
                }
            }
            "create" => Self::create(&mut state, call, &parsed),
            "rm" => {
                for name in &parsed.positionals {
                    if state.vms.remove(name).is_none() {
                        return Ok(Self::missing(name));
                    }
                }
                success(Vec::new())
            }
            "stop" => {
                for name in &parsed.positionals {
                    match state.vms.get_mut(name) {
                        Some(vm) => vm.running = false,
                        None => return Ok(Self::missing(name)),
                    }
                }
                success(Vec::new())
            }
            "mcp" => match parsed.value("sandbox") {
                Some(name) if state.vms.contains_key(name) => success(Vec::new()),
                Some(name) => Self::missing(name),
                None => failure("Error: required flag(s) \"sandbox\" not set"),
            },
            "cp" => Self::copy(&mut state, &parsed)?,
            "mount" => Self::mount(&mut state, call, &parsed),
            "umount" => Self::umount(&mut state, call, &parsed),
            "exec" => Self::exec(&mut state, &parsed),
            _ => unreachable!("verbs are filtered above"),
        })
    }

    fn render_inventory(&self, state: &State) -> Vec<u8> {
        let rows = state
            .vms
            .iter()
            .map(|(name, vm)| {
                let mut row = serde_json::json!({
                    "name": name,
                    "id": if self.version.shape == InventoryShape::Envelope046 {
                        vm.id.to_ascii_uppercase()
                    } else {
                        vm.id.clone()
                    },
                    "agent": vm.agent,
                    "status": if vm.running { "running" } else { "stopped" },
                    "last_used_at": "2026-10-11T22:58:37.25171Z",
                    "created_at": "2026-10-11T22:58:15Z",
                });
                if !vm.workspaces.is_empty() {
                    row["workspaces"] = serde_json::json!(vm.workspaces);
                }
                if self.version.shape == InventoryShape::BareArray && vm.running {
                    row["detached"] = serde_json::json!(false);
                }
                row
            })
            .collect::<Vec<_>>();
        let document = match self.version.shape {
            InventoryShape::BareArray => serde_json::json!(rows),
            _ => serde_json::json!({ "sandboxes": rows }),
        };
        serde_json::to_vec(&document).unwrap()
    }

    fn create(state: &mut State, call: &Call, parsed: &Parsed) -> CommandOutput {
        let Some(name) = parsed.value("name").map(str::to_owned) else {
            return failure("Error: a name is required");
        };
        if state.vms.contains_key(&name) {
            return failure(&format!("Error: sandbox '{name}' already exists"));
        }
        let Some(kit) = parsed.positionals.first() else {
            return failure("Error: requires an agent or kit reference");
        };
        let image_digest = if let Some(template) = parsed.value("template") {
            match state.templates.get(template) {
                Some(digest) => Some(digest.clone()),
                None if template.contains("@sha256:") => template
                    .split_once('@')
                    .map(|(_, digest)| digest.to_owned()),
                None if parsed.value("pull") == Some("never") => {
                    return failure(&format!("Error: image {template} not found locally"));
                }
                None => None,
            }
        } else if Path::new(kit).is_dir() {
            Some(if call.env_value("SBX_KIT_REBUILD") == Some("1") {
                state.stock_rebuild_digest.clone()
            } else {
                state.stock_kit_digest.clone()
            })
        } else {
            None
        };
        let id = Self::next_uuid(state);
        let vm = Vm {
            id,
            running: true,
            agent: if kit == "shell" {
                "shell".into()
            } else {
                "kit".into()
            },
            workspaces: parsed.positionals[1..].iter().map(PathBuf::from).collect(),
            image_digest,
            users: state.template_users.clone(),
            ..Vm::default()
        };
        state.vms.insert(name, vm);
        success(Vec::new())
    }

    fn copy(state: &mut State, parsed: &Parsed) -> io::Result<CommandOutput> {
        let [source, destination] = parsed.positionals.as_slice() else {
            return Ok(failure("Error: accepts 2 arg(s)"));
        };
        let Some((vm, path)) = destination.split_once(':') else {
            return Ok(failure("Error: one of SRC or DST must be a sandbox path"));
        };
        let bytes = fs::read(source)?;
        Ok(match state.vms.get_mut(vm) {
            Some(guest) => {
                guest.files.insert(path.to_owned(), bytes);
                success(Vec::new())
            }
            None => Self::missing(vm),
        })
    }

    /// `HOST[:TARGET[:MODE]]` with HOST relative to the caller's directory.
    fn mount_spec(call: &Call, spec: &str) -> Option<(PathBuf, PathBuf, String)> {
        let cwd = call.cwd.clone()?;
        let mut parts = spec.splitn(3, ':');
        let host = cwd.join(parts.next()?);
        let host = PathBuf::from(host.to_str()?.trim_end_matches("/."));
        let target = parts.next().map_or_else(|| host.clone(), PathBuf::from);
        let mode = parts.next().unwrap_or("rw").to_owned();
        Some((host, target, mode))
    }

    fn mount(state: &mut State, call: &Call, parsed: &Parsed) -> CommandOutput {
        let [vm, spec] = parsed.positionals.as_slice() else {
            return failure("Error: accepts 2 arg(s)");
        };
        let Some((host, target, mode)) = Self::mount_spec(call, spec) else {
            return failure("Error: invalid mount spec");
        };
        let remount_conflicts = state.remount_conflicts;
        let Some(guest) = state.vms.get_mut(vm) else {
            return Self::missing(vm);
        };
        match guest.mounts.get(&target) {
            // "Mount operations are idempotent" (v0.47.0 `sbx mount --help`).
            Some(existing) if *existing == (host.clone(), mode.clone()) && !remount_conflicts => {
                success(Vec::new())
            }
            Some(_) => failure(corpus::UNVERIFIED_ALREADY_MOUNTED),
            None => {
                guest.mounts.insert(target, (host, mode));
                success(Vec::new())
            }
        }
    }

    fn umount(state: &mut State, call: &Call, parsed: &Parsed) -> CommandOutput {
        let [vm, spec] = parsed.positionals.as_slice() else {
            return failure("Error: accepts 2 arg(s)");
        };
        let Some((host, target, _)) = Self::mount_spec(call, spec) else {
            return failure("Error: invalid mount spec");
        };
        let Some(guest) = state.vms.get_mut(vm) else {
            return Self::missing(vm);
        };
        // "If CTR_TARGET is neither mounted nor saved, the command fails".
        if guest.mounts.remove(&target).is_some() {
            return success(Vec::new());
        }
        let quoted = |path: &Path| format!("{:?}", path.display().to_string());
        let message = if guest.mounts.values().any(|(source, _)| *source == host) {
            corpus::UNVERIFIED_UMOUNT_NOT_BOUND.replace("\"TARGET\"", &quoted(&target))
        } else {
            corpus::UNVERIFIED_UMOUNT_NOT_EXPOSED.to_owned()
        };
        failure(&message.replace("\"HOST\"", &quoted(&host)))
    }

    /// "If the sandbox is stopped, it is started first." Stock v0.50.0
    /// refuses one whose saved mount's host path is gone.
    fn start(guest: &mut Vm) -> Option<CommandOutput> {
        if !guest.running
            && let Some((target, (host, _))) =
                guest.mounts.iter().find(|(_, (host, _))| !host.exists())
        {
            let (host, target) = (host.display(), target.display());
            return Some(failure(&format!(
                "error: cannot start sandbox: start runtime: request failed: 422 Unprocessable Entity: cannot restore mount \"{host}\" at \"{target}\": stat {host}: no such file or directory; restore the host path or remove the saved mount with sbx umount while the sandbox is stopped"
            )));
        }
        guest.running = true;
        None
    }

    #[allow(clippy::too_many_lines)] // One table of the guest commands marsh runs.
    fn exec(state: &mut State, parsed: &Parsed) -> CommandOutput {
        let Some((vm, command)) = parsed.positionals.split_first() else {
            return failure("Error: requires at least 2 arg(s)");
        };
        let registry = state.registry.clone();
        let Some(guest) = state.vms.get_mut(vm) else {
            return Self::missing(vm);
        };
        if let Some(refused) = Self::start(guest) {
            return refused;
        }
        let environment = parsed
            .values("env")
            .into_iter()
            .filter_map(|entry| entry.split_once('='))
            .collect::<BTreeMap<_, _>>();
        let command = command.iter().map(String::as_str).collect::<Vec<_>>();
        match command.as_slice() {
            ["cat", path] => match guest.files.get(*path) {
                Some(bytes) => success(bytes.clone()),
                None => output(
                    1,
                    Vec::new(),
                    format!("cat: {path}: No such file or directory"),
                ),
            },
            ["install", "-d", ..] | ["docker", "info", ..] => success(Vec::new()),
            ["install", "-m", _, source, destination] => {
                Self::install(guest, &[(source, destination)])
            }
            ["docker", "pull", reference] => match registry.get(*reference) {
                Some(id) => {
                    guest.images.insert((*reference).to_owned(), id.clone());
                    success(Vec::new())
                }
                None => failure(&format!(
                    "Error response from daemon: manifest for {reference} not found"
                )),
            },
            [
                "docker",
                "image",
                "inspect",
                "--format",
                "{{.Id}}",
                reference,
            ] => match guest.images.get(*reference) {
                Some(id) => success(format!("{id}\n")),
                None => failure(&format!("Error: No such image: {reference}")),
            },
            ["sh", "-c", _, "marsh-install-worker", pairs @ ..] => {
                let pairs = pairs
                    .chunks(2)
                    .filter_map(|pair| Some((pair.first()?, pair.get(1)?)))
                    .collect::<Vec<_>>();
                Self::install(guest, &pairs)
            }
            ["sh", "-c", script] if script.contains("/var/lib/marsh/worker.json") => {
                if let Some(user) = environment.get("MARSH_SHELL_USER") {
                    let uid = environment
                        .get("MARSH_RELAY_UID")
                        .and_then(|v| v.parse().ok());
                    let gid = environment
                        .get("MARSH_RELAY_GID")
                        .and_then(|v| v.parse().ok());
                    let (Some(uid), Some(gid)) = (uid, gid) else {
                        return failure("missing shell identity");
                    };
                    if guest.users.get(*user).is_some_and(|ids| *ids != (uid, gid)) {
                        return output(SHELL_USER_MISMATCH_EXIT, Vec::new(), Vec::new());
                    }
                    let mut installs = vec![("/tmp/marsh-shell.install", "/usr/local/bin/marsh")];
                    if let Some(relay) = environment
                        .get("MARSH_RELAY_INSTALL")
                        .filter(|v| !v.is_empty())
                    {
                        installs.push((*relay, RELAY_PATH));
                    }
                    let installed = Self::install(
                        guest,
                        &installs.iter().map(|(a, b)| (a, b)).collect::<Vec<_>>(),
                    );
                    if !installed.succeeded() {
                        return installed;
                    }
                    guest.users.insert((*user).to_owned(), (uid, gid));
                }
                let Some(marker) = environment.get("MARSH_WORKER_MARKER") else {
                    return failure("MARSH_WORKER_MARKER: parameter not set");
                };
                guest.files.insert(
                    "/var/lib/marsh/worker.json".into(),
                    format!("{marker}\n").into_bytes(),
                );
                success(Vec::new())
            }
            _ => output(
                127,
                Vec::new(),
                format!("model has no guest command {command:?}"),
            ),
        }
    }

    fn install<S: AsRef<str>>(guest: &mut Vm, pairs: &[(&S, &S)]) -> CommandOutput {
        for (source, destination) in pairs {
            let Some(bytes) = guest.files.get(source.as_ref()).cloned() else {
                return failure(&format!("install: cannot stat '{}'", source.as_ref()));
            };
            guest.files.insert(destination.as_ref().to_owned(), bytes);
        }
        success(Vec::new())
    }

    fn host_build(&self, call: &Call) -> io::Result<CommandOutput> {
        let digest = self.state.lock().unwrap().host_build_digest.clone();
        let tag = call.value("--tag").unwrap_or_default().to_owned();
        let archive = serde_json::json!({ "tag": tag, "manifest": digest });
        for destination in call
            .values("--output")
            .into_iter()
            .filter_map(|output| output.strip_prefix("type=docker,oci-mediatypes=true,dest="))
        {
            fs::write(destination, serde_json::to_vec(&archive).unwrap())?;
        }
        if let Some(metadata) = call.value("--metadata-file") {
            fs::write(
                metadata,
                serde_json::to_vec(&serde_json::json!({ "containerimage.digest": digest }))
                    .unwrap(),
            )?;
        }
        Ok(success(Vec::new()))
    }

    // ---- attached processes ---------------------------------------------

    fn register(&self, vm: &str, kind: ProcKind, state: &Arc<ProcState>) {
        self.state.lock().unwrap().running.push(Running {
            vm: vm.to_owned(),
            kind,
            state: Arc::clone(state),
        });
    }

    fn exited(code: i32, stderr: &str) -> Attachment {
        let state = Arc::new(ProcState::default());
        state.finish(code);
        Attachment {
            stdin: Box::new(io::sink()),
            stdout: Box::new(Cursor::new(Vec::new())),
            stderr: Box::new(Cursor::new(stderr.as_bytes().to_vec())),
            process: Box::new(ModelProcess(state)),
            control: no_attachment_control(),
        }
    }

    fn spawn(&self, call: &Call) -> io::Result<Attachment> {
        let fault = self.admit(call);
        match fault {
            Some(Fault::SpawnError(message)) => return Err(io::Error::other(message)),
            Some(Fault::Respond(output)) => {
                return Ok(Self::exited(
                    output.exit_code.unwrap_or(1),
                    &String::from_utf8_lossy(&output.stderr),
                ));
            }
            _ => {}
        }
        let parsed = parse(&call.args[1..], &corpus::flags(corpus::EXEC_HELP), true)
            .map_err(io::Error::other)?;
        let Some((vm, command)) = parsed.positionals.split_first() else {
            return Err(io::Error::other("attached call without a sandbox"));
        };
        let installed = |path: &str| {
            self.state
                .lock()
                .unwrap()
                .vms
                .get(vm)
                .map(|guest| guest.files.contains_key(path))
        };
        let command = command.iter().map(String::as_str).collect::<Vec<_>>();
        match command.as_slice() {
            [WORKER_PATH, "--serve", generation] => match installed(WORKER_PATH) {
                None => Ok(Self::exited(
                    1,
                    &String::from_utf8_lossy(&corpus::missing_vm_stderr(vm)),
                )),
                Some(false) => Ok(Self::exited(127, "marsh-worker: not found")),
                Some(true) => self.spawn_worker(vm, generation.parse().map_err(io::Error::other)?),
            },
            ["/usr/local/bin/marsh", "--internal-supervisor"] => {
                match installed("/usr/local/bin/marsh") {
                    None => Ok(Self::exited(
                        1,
                        &String::from_utf8_lossy(&corpus::missing_vm_stderr(vm)),
                    )),
                    Some(false) => Ok(Self::exited(127, "marsh: not found")),
                    Some(true) => self.spawn_supervisor(vm),
                }
            }
            ["sh", "-c", _, "marsh-load", "docker", "image", "load"] => self.spawn_load(vm),
            ["sh", "-c", _, "marsh-save", reference, tag] => {
                let image = self
                    .state
                    .lock()
                    .unwrap()
                    .vms
                    .get(vm)
                    .and_then(|guest| guest.images.get(*reference).cloned());
                Ok(match image {
                    Some(id) => {
                        let state = Arc::new(ProcState::default());
                        state.finish(0);
                        Attachment {
                            stdin: Box::new(io::sink()),
                            stdout: Box::new(Cursor::new(
                                serde_json::to_vec(
                                    &serde_json::json!({"tag": tag, "manifest": id}),
                                )
                                .unwrap(),
                            )),
                            stderr: Box::new(Cursor::new(Vec::new())),
                            process: Box::new(ModelProcess(state)),
                            control: no_attachment_control(),
                        }
                    }
                    None => Self::exited(1, "Error: No such image"),
                })
            }
            _ => Err(io::Error::other(format!(
                "model has no attached command {command:?}"
            ))),
        }
    }

    fn spawn_load(&self, vm: &str) -> io::Result<Attachment> {
        let (host, mut guest) = UnixStream::pair()?;
        let state = Arc::new(ProcState::default());
        let finished = Arc::clone(&state);
        let model = self.this.clone();
        let vm = vm.to_owned();
        thread::spawn(move || {
            let mut archive = Vec::new();
            let loaded = guest.read_to_end(&mut archive).is_ok()
                && serde_json::from_slice::<serde_json::Value>(&archive)
                    .ok()
                    .zip(model.upgrade())
                    .is_some_and(|(archive, model)| {
                        let (Some(tag), Some(id)) =
                            (archive["tag"].as_str(), archive["manifest"].as_str())
                        else {
                            return false;
                        };
                        let mut state = model.state.lock().unwrap();
                        let Some(guest) = state.vms.get_mut(&vm) else {
                            return false;
                        };
                        guest.images.insert(tag.to_owned(), id.to_owned());
                        if let Some((repository, _)) = tag.rsplit_once(':') {
                            guest
                                .images
                                .insert(format!("{repository}@{id}"), id.to_owned());
                        }
                        true
                    });
            finished.finish(i32::from(!loaded));
        });
        Ok(Attachment {
            stdin: Box::new(host),
            stdout: Box::new(Cursor::new(Vec::new())),
            stderr: Box::new(Cursor::new(Vec::new())),
            process: Box::new(ModelProcess(state)),
            control: no_attachment_control(),
        })
    }

    fn spawn_worker(&self, vm: &str, generation: u64) -> io::Result<Attachment> {
        let (controller, mut worker) = UnixStream::pair()?;
        let (ready_gate, wedge) = {
            let state = self.state.lock().unwrap();
            (
                state.worker_ready_gate.clone(),
                state.wedge_worker_termination.clone(),
            )
        };
        let state = Arc::new(ProcState {
            socket: Mutex::new(Some(worker.try_clone()?)),
            wedge,
            ..ProcState::default()
        });
        self.register(vm, ProcKind::Worker, &state);
        let model = self.this.clone();
        let name = vm.to_owned();
        thread::spawn(move || {
            if let Some(gate) = ready_gate {
                gate.pass();
            }
            let _ = marsh_worker::write_frame(
                &mut worker,
                &marsh_worker::WorkerResponse::Ready { generation },
            );
            while let Ok(request) =
                marsh_worker::read_frame::<marsh_worker::WorkerRequest>(&mut worker)
            {
                if let marsh_worker::WorkerRequest::Ping { generation, nonce } = &request {
                    let _ = marsh_worker::write_frame(
                        &mut worker,
                        &marsh_worker::WorkerResponse::Pong {
                            generation: *generation,
                            nonce: *nonce,
                        },
                    );
                }
                let Some(model) = model.upgrade() else { return };
                model
                    .state
                    .lock()
                    .unwrap()
                    .worker_requests
                    .push((name.clone(), request));
                model.changed.notify_all();
            }
        });
        Ok(Attachment {
            stdin: Box::new(WorkerInput {
                stream: controller.try_clone()?,
                model: self.this.clone(),
                vm: vm.to_owned(),
            }),
            stdout: Box::new(controller),
            stderr: Box::new(Cursor::new(Vec::new())),
            process: Box::new(ModelProcess(state)),
            control: no_attachment_control(),
        })
    }

    /// In-process `marsh --internal-supervisor`: acknowledges its generation,
    /// answers pings, runs a shell to a clean exit, acknowledges controls.
    fn spawn_supervisor(&self, vm: &str) -> io::Result<Attachment> {
        use shell_supervisor::{Down, StartKind, Up, read_frame, write_frame};
        let (controller, mut guest) = UnixStream::pair()?;
        let wrong_generation =
            std::mem::take(&mut self.state.lock().unwrap().wrong_supervisor_generation);
        let state = Arc::new(ProcState {
            socket: Mutex::new(Some(guest.try_clone()?)),
            ..ProcState::default()
        });
        self.register(vm, ProcKind::Supervisor, &state);
        let model = self.this.clone();
        thread::spawn(move || {
            let mut reader = guest.try_clone().unwrap();
            while let Ok((frame, _)) = read_frame::<Down>(&mut reader) {
                let Some(model) = model.upgrade() else { return };
                model
                    .state
                    .lock()
                    .unwrap()
                    .supervisor_frames
                    .push(frame.clone());
                model.changed.notify_all();
                let replies = match frame {
                    Down::Hello { generation } => vec![Up::Ready {
                        generation: generation + u64::from(wrong_generation),
                    }],
                    Down::Ping { generation, nonce } => vec![Up::Pong { generation, nonce }],
                    Down::Start { attempt, spec } => match spec.kind {
                        StartKind::Shell { .. } => vec![
                            Up::Started { attempt },
                            Up::OutputEof { attempt, stream: 1 },
                            Up::OutputEof { attempt, stream: 2 },
                            Up::Exited { attempt, code: 0 },
                            Up::Cleaned {
                                attempt,
                                error: None,
                            },
                        ],
                        StartKind::Child { .. } => vec![Up::Started { attempt }],
                    },
                    Down::Control { attempt, seq, .. } => vec![Up::Done {
                        attempt,
                        seq,
                        error: None,
                    }],
                    _ => Vec::new(),
                };
                for reply in replies {
                    if write_frame(&mut guest, &reply, &[]).is_err() {
                        return;
                    }
                }
            }
        });
        Ok(Attachment {
            stdin: Box::new(controller.try_clone()?),
            stdout: Box::new(controller),
            stderr: Box::new(Cursor::new(Vec::new())),
            process: Box::new(ModelProcess(state)),
            control: no_attachment_control(),
        })
    }
}

/// A retained worker's stdin: forwards bytes, with the model's input gate and
/// one-shot ambiguous flush.
struct WorkerInput {
    stream: UnixStream,
    model: std::sync::Weak<StockModel>,
    vm: String,
}

impl Write for WorkerInput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let gate = self
            .model
            .upgrade()
            .and_then(|model| model.state.lock().unwrap().worker_input_gate.clone());
        if let Some(gate) = gate {
            gate.pass();
        }
        let written = self.stream.write(bytes)?;
        if let Some(model) = self.model.upgrade() {
            model
                .state
                .lock()
                .unwrap()
                .worker_input
                .entry(self.vm.clone())
                .or_default()
                .extend_from_slice(&bytes[..written]);
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()?;
        let fail = self.model.upgrade().is_some_and(|model| {
            std::mem::take(&mut model.state.lock().unwrap().fail_worker_flush)
        });
        if fail {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "flush completion unknown",
            ));
        }
        Ok(())
    }
}

impl CommandRunner for StockModel {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        self.respond(&Call::new(invocation))
    }

    fn run_bounded(&self, invocation: &Invocation, _: Duration) -> io::Result<CommandOutput> {
        self.respond(&Call::new(invocation))
    }

    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment> {
        self.spawn(&Call::new(invocation))
    }
}

/// Calls of stock `verb`.
pub(super) fn verb(verb: &'static str) -> Matcher {
    Arc::new(move |call: &Call| call.verb() == verb && !call.is_host_build())
}

/// Host `docker buildx` builds.
pub(super) fn host_build() -> Matcher {
    Arc::new(Call::is_host_build)
}

/// Stock `verb` calls made from the host directory `cwd`.
pub(super) fn verb_in(verb: &'static str, cwd: PathBuf) -> Matcher {
    Arc::new(move |call: &Call| call.verb() == verb && call.cwd.as_deref() == Some(cwd.as_path()))
}

/// Stock `verb` calls addressed to sandbox `vm`.
pub(super) fn verb_on(verb: &'static str, vm: String) -> Matcher {
    Arc::new(move |call: &Call| call.verb() == verb && call.args.get(1) == Some(&vm))
}

/// The exec that writes the guest ownership marker on its own.
pub(super) fn marker_write() -> Matcher {
    Arc::new(|call: &Call| {
        call.verb() == "exec"
            && matches!(call.guest().as_slice(), ["sh", "-c", script] if script.contains("worker.json"))
            && call
                .values("-e")
                .iter()
                .all(|entry| !entry.starts_with("MARSH_SHELL_USER="))
    })
}

/// `sbx exec` calls whose guest command contains `word`.
pub(super) fn guest_word(word: &'static str) -> Matcher {
    Arc::new(move |call: &Call| call.verb() == "exec" && call.guest().contains(&word))
}
