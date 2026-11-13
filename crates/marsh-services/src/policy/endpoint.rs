//! The filtered Docker endpoint: accept, parse, admit, rebuild, relay (B1).
//!
//! One request in flight per connection. Each admitted request is sent on a
//! fresh connection to the production engine, so the engine never sees a
//! second request from the same client bytes. A connection goes raw only after
//! the engine itself answered `101`, or `200` with a Docker stream content
//! type, on a route that is allowed to hijack.

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock, RwLock};
use std::time::Duration;

use super::admit::{Admitted, BodyPolicy, Post, ReqBody, admit, body_policy};
use super::http::{
    BodyState, Conn, Framing, Head, HttpError, HttpLimits, RespFraming, RespHead, encode_query,
    parse_target, write_refusal,
};
use super::json::Json;
use super::routes::{Kind, classify};
use super::{
    Authority, CurrentLimits, Env, Lookup, PinRequest, PinnedBind, Refusal, ResourceLimits,
    VolumeScope,
};
use crate::breaks::break_mode;
use crate::ledger::{
    ContainerRec, Ledger, LedgerError, NetRec, Paths, PinRec, SockRec, VolumeClaim, now,
};
use crate::pins::{PinError, PinManager, ProjectRoot, observe};

#[derive(Debug, Clone)]
pub struct Config {
    pub paths: Paths,
    pub limits: ResourceLimits,
    pub http: HttpLimits,
    pub max_conns_attempt: usize,
    pub max_conns_project: usize,
    pub max_conns_total: usize,
    /// `None` allows every registry. Otherwise the registry host of a pulled
    /// image reference must be listed (`docker.io` for unqualified names).
    pub registry_allow: Option<Vec<String>>,
    /// When set, the engine's configuration file
    /// ([`crate::engine::EngineConfig`]) is read on every image reference and
    /// decides the allowlist instead of `registry_allow`: the registry list
    /// follows the Internet gate of a running engine (E5). A file that cannot be
    /// read means the closed list.
    pub registry_config: Option<std::path::PathBuf>,
    /// See [`Env::socket_aliases`]. The default is the job endpoint path E2
    /// gives every job (DECISIONS 20).
    pub socket_aliases: Vec<String>,
    /// A hijacked stream (attach, exec, session) that carries no byte either
    /// way for this long is closed.
    pub hijack_idle: Duration,
    /// How often a raw stream looks for a vanished peer and for idleness.
    pub hijack_poll: Duration,
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            paths: Paths::default(),
            limits: ResourceLimits::default(),
            http: HttpLimits::default(),
            max_conns_attempt: 64,
            max_conns_project: 256,
            max_conns_total: 1024,
            // The engine's configuration file decides in a running agent
            // (`relay::production_services`); `None` here allows every
            // registry (the dev harness and unit tests).
            registry_allow: None,
            registry_config: None,
            socket_aliases: vec!["/run/marsh/docker.sock".to_string()],
            hijack_idle: Duration::from_mins(30),
            hijack_poll: Duration::from_millis(500),
            verbose: false,
        }
    }
}

/// An open bind root and the workspaces that used it.
type HeldRoot = (Arc<ProjectRoot>, HashSet<String>);

#[derive(Default)]
struct Counts {
    attempts: HashMap<String, usize>,
    /// Per bind root: only the `project-cap-per-root` defect uses it.
    roots: HashMap<PathBuf, usize>,
    /// Per workspace: its last connection's end releases the bind roots of
    /// a workspace no longer served.
    workspaces: HashMap<String, usize>,
    /// Connections of this project (this `Services` is one project's engine),
    /// whatever workspace root each one is bound to.
    project: usize,
}

/// Connections of every `Services` in the process: the daemon-wide cap.
static PROCESS_CONNS: AtomicUsize = AtomicUsize::new(0);

/// In-flight state-changing requests and capture freezes, per workspace.
#[derive(Default)]
struct ScopeGate {
    frozen: usize,
    inflight: usize,
}

#[derive(Default)]
struct Gate {
    scopes: Mutex<HashMap<String, ScopeGate>>,
    changed: Condvar,
}

pub struct Services {
    pub cfg: Config,
    pub ledger: Ledger,
    pub pins: PinManager,
    upstream: PathBuf,
    /// Each bind root opened, with the workspaces that used it: a root goes
    /// when the last of them is forgotten (`forget_scope`).
    roots: Mutex<HashMap<PathBuf, HeldRoot>>,
    counts: Mutex<Counts>,
    sockets: Mutex<HashMap<String, (PathBuf, Arc<AtomicBool>)>>,
    net_lock: Mutex<()>,
    reconciler: Once,
    open_connections: AtomicUsize,
    gate: Gate,
    /// Serializes the claim, look and create of one volume name (and the
    /// ledger entry) so two scopes cannot both win it.
    vol_lock: Mutex<()>,
    /// A pin is mounted and then recorded; both happen under the read side, and
    /// the sweep of unrecorded pins runs under the write side, so it can never
    /// see the first without the second.
    pin_gate: RwLock<()>,
}

/// Held by capture (S3) while a scope's containers are paused: requests that
/// would change that scope's state are refused until it drops (CAP-11), and the
/// guard exists only once the ones already admitted have finished.
pub struct FreezeGuard {
    svc: Arc<Services>,
    scope: String,
}

impl Drop for FreezeGuard {
    fn drop(&mut self) {
        let mut g = self
            .svc
            .gate
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(e) = g.get_mut(&self.scope) {
            e.frozen -= 1;
            if e.frozen == 0 && e.inflight == 0 {
                g.remove(&self.scope);
            }
        }
        self.svc.gate.changed.notify_all();
    }
}

/// A state-changing request in flight for a scope, from before its body is
/// read until its answer (and the ledger update) is done.
struct MutationGuard {
    svc: Arc<Services>,
    scope: String,
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        let mut g = self
            .svc
            .gate
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(e) = g.get_mut(&self.scope) {
            e.inflight -= 1;
            if e.frozen == 0 && e.inflight == 0 {
                g.remove(&self.scope);
            }
        }
        self.svc.gate.changed.notify_all();
    }
}

struct ConnGuard {
    svc: Arc<Services>,
    attempt: String,
    root: PathBuf,
    workspace: String,
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        let mut c = self
            .svc
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A count at zero is removed: attempts and roots come and go.
        if let Some(n) = c.attempts.get_mut(&self.attempt) {
            *n -= 1;
            if *n == 0 {
                c.attempts.remove(&self.attempt);
            }
        }
        if let Some(n) = c.roots.get_mut(&self.root) {
            *n -= 1;
            if *n == 0 {
                c.roots.remove(&self.root);
            }
        }
        let last = c.workspaces.get_mut(&self.workspace).is_some_and(|n| {
            *n -= 1;
            *n == 0
        });
        if last {
            c.workspaces.remove(&self.workspace);
        }
        c.project -= 1;
        PROCESS_CONNS.fetch_sub(1, Ordering::SeqCst);
        self.svc.open_connections.fetch_sub(1, Ordering::Relaxed);
        drop(c);
        // A workspace no longer served (forgotten, or served one stream at a
        // time by the relay) keeps no bind root past its last connection,
        // even one a late request on that connection opened again.
        if last && !self.svc.serving(&self.workspace) {
            self.svc.release_roots(&self.workspace);
        }
    }
}

impl Services {
    pub fn new(cfg: Config, upstream: &Path) -> io::Result<Arc<Self>> {
        let ledger = if cfg.paths.ledger.as_os_str().is_empty() {
            Ledger::in_memory()
        } else {
            Ledger::open(&cfg.paths.ledger)?
        };
        let pins = PinManager::new(&cfg.paths.bind_root);
        Ok(Arc::new(Self {
            cfg,
            ledger,
            pins,
            upstream: upstream.to_path_buf(),
            roots: Mutex::default(),
            counts: Mutex::default(),
            sockets: Mutex::default(),
            net_lock: Mutex::new(()),
            reconciler: Once::new(),
            open_connections: AtomicUsize::new(0),
            gate: Gate::default(),
            vol_lock: Mutex::new(()),
            pin_gate: RwLock::new(()),
        }))
    }

    /// Refuse state-changing requests of `scope` from now on, wait for those
    /// already admitted to finish, and keep refusing while the guard lives.
    ///
    /// # Panics
    /// Never in practice: no timeout is given, so a guard is always returned.
    #[must_use]
    pub fn freeze_scope(self: &Arc<Self>, scope: &str) -> FreezeGuard {
        self.freeze_scope_within(scope, None)
            .expect("no timeout was given")
    }

    /// As [`Self::freeze_scope`], giving up (and letting requests through
    /// again) after `limit`.
    #[must_use]
    pub fn freeze_scope_within(
        self: &Arc<Self>,
        scope: &str,
        limit: Option<Duration>,
    ) -> Option<FreezeGuard> {
        let deadline = limit.map(|l| std::time::Instant::now() + l);
        let mut g = self
            .gate
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.entry(scope.to_string()).or_default().frozen += 1;
        let guard = FreezeGuard {
            svc: Arc::clone(self),
            scope: scope.to_string(),
        };
        while !break_mode("freeze-no-wait") && g.get(scope).is_some_and(|e| e.inflight > 0) {
            let wait = deadline.map_or(Duration::from_secs(1), |d| {
                d.saturating_duration_since(std::time::Instant::now())
            });
            if wait.is_zero() {
                drop(g);
                drop(guard);
                return None;
            }
            g = self
                .gate
                .changed
                .wait_timeout(g, wait.min(Duration::from_secs(1)))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        drop(g);
        Some(guard)
    }

    fn is_frozen(&self, scope: &str) -> bool {
        self.gate
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(scope)
            .is_some_and(|e| e.frozen > 0)
    }

    /// Register a state-changing request, unless the scope is frozen.
    fn begin_mutation(self: &Arc<Self>, scope: &str) -> Option<MutationGuard> {
        let mut g = self
            .gate
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let e = g.entry(scope.to_string()).or_default();
        if e.frozen > 0 && !break_mode("capture-api-gate-off") {
            return None;
        }
        e.inflight += 1;
        Some(MutationGuard {
            svc: Arc::clone(self),
            scope: scope.to_string(),
        })
    }

    #[must_use]
    pub fn open_connections(&self) -> usize {
        self.open_connections.load(Ordering::Relaxed)
    }

    fn log(&self, msg: &str) {
        if self.cfg.verbose {
            eprintln!("marsh-services: {msg}");
        }
    }

    fn admit_conn(self: &Arc<Self>, a: &Authority) -> Result<ConnGuard, &'static str> {
        let mut c = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = format!("{}/{}", a.workspace, a.attempt);
        if PROCESS_CONNS.load(Ordering::SeqCst) >= self.cfg.max_conns_total {
            return Err("too many connections to the services agent");
        }
        // The project is this engine: every workspace's connections count,
        // whatever tree each endpoint is bound to.
        let project = if break_mode("project-cap-per-root") {
            c.roots.get(&a.project_root).copied().unwrap_or(0)
        } else {
            c.project
        };
        if project >= self.cfg.max_conns_project {
            return Err("too many connections for this project");
        }
        if c.attempts.get(&key).copied().unwrap_or(0) >= self.cfg.max_conns_attempt {
            return Err("too many connections for this attempt");
        }
        *c.attempts.entry(key.clone()).or_default() += 1;
        *c.roots.entry(a.project_root.clone()).or_default() += 1;
        *c.workspaces.entry(a.workspace.clone()).or_default() += 1;
        c.project += 1;
        PROCESS_CONNS.fetch_add(1, Ordering::SeqCst);
        self.open_connections.fetch_add(1, Ordering::Relaxed);
        Ok(ConnGuard {
            svc: Arc::clone(self),
            attempt: key,
            root: a.project_root.clone(),
            workspace: a.workspace.clone(),
        })
    }

    /// Whether `workspace` has a substituted socket being served.
    fn serving(&self, workspace: &str) -> bool {
        let prefix = format!("{workspace}/");
        self.sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .any(|key| key.starts_with(&prefix))
    }

    /// `workspace` no longer uses its bind roots: a root goes once no other
    /// workspace that used it remains.
    fn release_roots(&self, workspace: &str) {
        self.roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|_, (_, users)| {
                users.remove(workspace);
                !users.is_empty()
            });
    }

    fn root_for(&self, project: &Path, workspace: &str) -> Result<Arc<ProjectRoot>, PinError> {
        let mut g = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((r, users)) = g.get_mut(project) {
            users.insert(workspace.to_owned());
            return Ok(Arc::clone(r));
        }
        let r = Arc::new(ProjectRoot::open(project)?);
        self.note_protected(&r);
        g.insert(
            project.to_path_buf(),
            (Arc::clone(&r), HashSet::from([workspace.to_owned()])),
        );
        Ok(r)
    }

    /// Open (or reuse) `project`'s bind root for `workspace` (tests).
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn hold_root(&self, project: &Path, workspace: &str) {
        self.root_for(project, workspace).unwrap();
    }

    /// Bind roots held open, and attempts and roots with a connection count
    /// (tests).
    #[cfg(test)]
    pub(crate) fn tracked(&self) -> (usize, usize, usize) {
        let c = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len();
        (roots, c.attempts.len(), c.roots.len())
    }

    /// Record what `.git` and `.marsh` are under this root, the first time
    /// they are seen: later renames cannot make them unprotected (A3).
    fn note_protected(&self, root: &ProjectRoot) {
        let given = root.given.to_string_lossy();
        match observe(root).map(|seen| self.ledger.note_protected(&given, &seen)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => self.log(&format!("could not record protected paths of {given}: {e}")),
            Err(e) => self.log(&format!(
                "could not look at protected paths of {given}: {e}"
            )),
        }
    }

    /// Release pins no container references. Safe to call with ids already
    /// freed. A pin is unmounted even if the ledger cannot be written: the
    /// request that made it is gone, and a stale record is cleaned by
    /// reconcile.
    pub fn free_pins(&self, ids: &[String]) {
        for id in ids {
            let rec = self.ledger.pin(id);
            match self.ledger.drop_pins(std::slice::from_ref(id)) {
                Ok(freed) => {
                    for (fid, frec) in freed {
                        self.pins.unpin(&frec.scope, &fid, Some(&frec));
                    }
                }
                Err(e) => {
                    self.log(&format!("could not release pin {id} in the ledger: {e}"));
                    if let Some(rec) = rec {
                        self.pins.unpin(&rec.scope, id, Some(&rec));
                    }
                }
            }
        }
    }

    /// Unmount pins the ledger has just dropped.
    pub(crate) fn unpin_freed(&self, freed: Vec<(String, PinRec)>) {
        for (id, rec) in freed {
            self.pins.unpin(&rec.scope, &id, Some(&rec));
        }
    }

    // --- engine access ----------------------------------------------------------

    fn dial(&self) -> io::Result<UnixStream> {
        let s = UnixStream::connect(&self.upstream)?;
        s.set_write_timeout(Some(self.cfg.http.progress))?;
        Ok(s)
    }

    pub(crate) fn up_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> io::Result<(u16, Vec<u8>)> {
        let mut s = self.dial()?;
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n");
        if let Some(b) = body {
            head.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                b.len()
            ));
        } else if method != "GET" {
            head.push_str("Content-Length: 0\r\n");
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes())?;
        if let Some(b) = body {
            s.write_all(b)?;
        }
        let mut conn = Conn::new(
            s,
            HttpLimits {
                progress: Duration::from_secs(30),
                ..HttpLimits::default()
            },
        );
        let resp = conn.read_response_head(method == "HEAD").map_err(http_io)?;
        let body = read_resp_body(&mut conn, &resp, 32 << 20).map_err(http_io)?;
        Ok((resp.status, body))
    }

    /// Drop ledger entries for things the engine no longer has; unmount pins
    /// nothing references. Run on a timer and after prunes.
    pub fn reconcile(&self) {
        let ids = |path: &str, key: Option<&str>, field: &str| -> Option<Vec<String>> {
            let (st, body) = self.up_request("GET", path, None).ok()?;
            if st != 200 {
                return None;
            }
            let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
            let arr = match key {
                Some(k) => v.get(k)?.as_array()?.clone(),
                None => v.as_array()?.clone(),
            };
            Some(
                arr.iter()
                    .filter_map(|x| x.get(field).and_then(|f| f.as_str()).map(str::to_string))
                    .collect(),
            )
        };
        let (Some(cs), Some(ns), Some(vs)) = (
            ids("/containers/json?all=1", None, "Id"),
            ids("/networks", None, "Id"),
            ids("/volumes", Some("Volumes"), "Name"),
        ) else {
            return;
        };
        match self.ledger.reconcile(&cs, &ns, &vs, now()) {
            Ok(freed) => self.unpin_freed(freed),
            Err(e) => self.log(&format!("reconcile: {e}")),
        }
        self.sweep_pins();
    }

    /// Unmount and remove pin paths nothing in the ledger knows about (an
    /// agent that died between mounting and recording). A pin of a request in
    /// flight is mounted and recorded under the read side of `pin_gate`, and
    /// this holds the write side while it looks, so the ledger it reads is
    /// never older than a pin it finds. A pin's age is not a factor.
    fn sweep_pins(&self) {
        if break_mode("sweep-pins-by-source-age") {
            return self.sweep_pins_unserialized();
        }
        let _exclusive = self
            .pin_gate
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let known: Vec<String> = self.ledger.pins().into_iter().map(|(id, _)| id).collect();
        let Ok(scopes) = std::fs::read_dir(&self.cfg.paths.bind_root) else {
            return;
        };
        for scope in scopes.flatten() {
            let Ok(entries) = std::fs::read_dir(scope.path()) else {
                continue;
            };
            for e in entries.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                if !known.contains(&id) {
                    self.pins
                        .unpin(&scope.file_name().to_string_lossy(), &id, None);
                }
            }
        }
    }

    /// Test break (`sweep-pins-by-source-age`): the sweep as it was. The
    /// ledger is read before the directory, and "old" is the modification time
    /// of the mounted source folder, not of the pin.
    fn sweep_pins_unserialized(&self) {
        let known: Vec<String> = self.ledger.pins().into_iter().map(|(id, _)| id).collect();
        let Ok(scopes) = std::fs::read_dir(&self.cfg.paths.bind_root) else {
            return;
        };
        for scope in scopes.flatten() {
            let Ok(entries) = std::fs::read_dir(scope.path()) else {
                continue;
            };
            for e in entries.flatten() {
                let id = e.file_name().to_string_lossy().into_owned();
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(60));
                if old && !known.contains(&id) {
                    self.pins
                        .unpin(&scope.file_name().to_string_lossy(), &id, None);
                }
            }
        }
    }

    fn start_reconciler(self: &Arc<Self>) {
        let me = Arc::clone(self);
        self.reconciler.call_once(move || {
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_secs(5));
                    me.reconcile();
                }
            });
        });
    }

    /// Serve the persisted substituted sockets again (before the engine runs
    /// containers that bind them).
    pub fn restore_sockets(self: &Arc<Self>) -> usize {
        let mut n = 0;
        for rec in self.ledger.sockets() {
            let auth = Authority {
                project_root: PathBuf::from(&rec.project_root),
                workspace: rec.scope.clone(),
                attempt: rec.attempt.clone(),
            };
            if self.socket_for(&auth).is_ok() {
                n += 1;
            }
        }
        n
    }

    fn socket_for(self: &Arc<Self>, auth: &Authority) -> Result<PathBuf, Refusal> {
        let key = format!("{}/{}", auth.workspace, auth.attempt);
        let mut g = self
            .sockets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((p, _)) = g.get(&key) {
            return Ok(p.clone());
        }
        let internal = |e: &dyn std::fmt::Display| Refusal {
            status: 500,
            msg: format!("could not create the per-scope socket: {e}"),
            close: false,
        };
        let dir = self
            .cfg
            .paths
            .socket_root
            .join(&auth.workspace)
            .join(&auth.attempt);
        std::fs::create_dir_all(&dir).map_err(|e| internal(&e))?;
        for d in [
            &self.cfg.paths.socket_root,
            &self.cfg.paths.socket_root.join(&auth.workspace),
            &dir,
        ] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| internal(&e))?;
        }
        let path = dir.join("docker.sock");
        let _ = std::fs::remove_dir(&path);
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(|e| internal(&e))?;
        // The socket file is the thing bind-mounted into containers of any
        // user; its directory is root-only.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .map_err(|e| internal(&e))?;
        if let Err(e) = self.ledger.add_socket(SockRec {
            scope: auth.workspace.clone(),
            attempt: auth.attempt.clone(),
            project_root: auth.project_root.to_string_lossy().into_owned(),
            upstream: self.upstream.to_string_lossy().into_owned(),
        }) {
            drop(listener);
            let _ = std::fs::remove_file(&path);
            return Err(led(e));
        }
        // The listener is polled so that dropping the scope (a lane is closed)
        // can end its thread.
        listener.set_nonblocking(true).map_err(|e| internal(&e))?;
        let stop = Arc::new(AtomicBool::new(false));
        let svc = Arc::clone(self);
        let a = auth.clone();
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            let _ = svc.serve_listener_until(&listener, &a, &flag);
        });
        g.insert(key, (path.clone(), stop));
        Ok(path)
    }

    /// Forget a dropped workspace's substituted sockets (A3.1): the listener
    /// threads end, the socket files go, and the ledger no longer serves them
    /// again after an engine restart. Objects that are still there stay in the
    /// ledger; this only releases what exists for the scope itself.
    ///
    /// # Errors
    /// The ledger could not be written (the sockets are then still recorded).
    pub fn forget_scope(&self, scope: &str) -> Result<(), LedgerError> {
        let prefix = format!("{scope}/");
        let stopped: Vec<(String, (PathBuf, Arc<AtomicBool>))> = {
            let mut g = self
                .sockets
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let keys: Vec<String> = g
                .keys()
                .filter(|k| k.starts_with(&prefix))
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| g.remove(&k).map(|v| (k, v)))
                .collect()
        };
        for (_, (path, stop)) in &stopped {
            stop.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_file(path);
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
        let _ = std::fs::remove_dir(self.cfg.paths.socket_root.join(scope));
        // Its bind roots' descriptors close once no other workspace uses them.
        self.release_roots(scope);
        self.ledger.forget_scope(scope)
    }

    // --- serving ---------------------------------------------------------------

    /// Serve one connection for `auth` and return when it ends: the
    /// per-connection part of [`Self::serve_listener`], for callers that
    /// already hold the stream and the authority (the relay opens one stream per
    /// channel, with the host-attested authority from the channel open frame).
    /// The connection caps count it like any other.
    ///
    /// # Errors
    /// `InvalidInput` when the authority is not valid (it is never served).
    pub fn serve_stream(self: &Arc<Self>, auth: &Authority, stream: UnixStream) -> io::Result<()> {
        auth.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.start_reconciler();
        let guard = match self.admit_conn(auth) {
            Ok(g) => g,
            Err(why) => {
                let mut s = stream;
                let _ = write_refusal(&mut s, 429, why, true);
                return Ok(());
            }
        };
        let scope = Arc::new(Scope {
            svc: Arc::clone(self),
            auth: auth.clone(),
        });
        let _guard = guard;
        self.log(&format!("connection from scope {}", scope.auth.workspace));
        serve_conn(&scope, stream);
        Ok(())
    }

    pub fn serve_listener(
        self: &Arc<Self>,
        listener: &UnixListener,
        auth: &Authority,
    ) -> io::Result<()> {
        self.serve_listener_until(listener, auth, &AtomicBool::new(false))
    }

    /// [`Self::serve_listener`] for a listener that may be non-blocking: it
    /// polls, and returns once `stop` is set.
    pub fn serve_listener_until(
        self: &Arc<Self>,
        listener: &UnixListener,
        auth: &Authority,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        auth.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        self.start_reconciler();
        let scope = Arc::new(Scope {
            svc: Arc::clone(self),
            auth: auth.clone(),
        });
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let stream = match listener.accept() {
                Ok((s, _)) => {
                    s.set_nonblocking(false)?;
                    s
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e),
            };
            let guard = match self.admit_conn(auth) {
                Ok(g) => g,
                Err(why) => {
                    let mut s = stream;
                    let _ = write_refusal(&mut s, 429, why, true);
                    continue;
                }
            };
            let scope = Arc::clone(&scope);
            std::thread::spawn(move || {
                let _guard = guard;
                scope
                    .svc
                    .log(&format!("connection from scope {}", scope.auth.workspace));
                serve_conn(&scope, stream);
            });
        }
    }
}

/// A ledger that cannot be written refuses the request (503): nothing may be
/// admitted that the agent could not remember after a restart.
/// Does an engine object (inspect output) carry this scope's label?
fn labelled_for(inspect: &[u8], ws: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(inspect).is_ok_and(|v| {
        v.pointer("/Labels/dev.marsh.scope")
            .and_then(|s| s.as_str())
            == Some(ws)
    })
}

#[allow(clippy::needless_pass_by_value)]
fn led(e: LedgerError) -> Refusal {
    Refusal {
        status: 503,
        msg: e.to_string(),
        close: false,
    }
}

fn http_io(e: HttpError) -> io::Error {
    match e {
        HttpError::Io(e) => e,
        other => io::Error::new(io::ErrorKind::InvalidData, format!("{other:?}")),
    }
}

/// An authority bound to the shared services: what the policy sees.
pub struct Scope {
    pub svc: Arc<Services>,
    pub auth: Authority,
}

impl Env for Scope {
    fn authority(&self) -> &Authority {
        &self.auth
    }

    fn limits(&self) -> &ResourceLimits {
        &self.svc.cfg.limits
    }

    fn pin_bind(&self, req: &PinRequest) -> Result<PinnedBind, Refusal> {
        let root = self
            .svc
            .root_for(&self.auth.project_root, &self.auth.workspace)
            .map_err(|e| Refusal {
                status: 500,
                msg: e.to_string(),
                close: false,
            })?;
        self.svc.note_protected(&root);
        let known = self.svc.ledger.protected(&root.given.to_string_lossy());
        // Mounted, then recorded, with the sweep of unrecorded pins held off.
        let _recording = self
            .svc
            .pin_gate
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pinned = self
            .svc
            .pins
            .pin(
                &root,
                &self.auth.workspace,
                &req.source,
                req.read_only,
                req.create_missing,
                &known,
            )
            .map_err(|e| match e {
                PinError::Refused(m) => Refusal::forbid(m),
                PinError::Internal(m) => Refusal {
                    status: 500,
                    msg: m,
                    close: false,
                },
            })?;
        crate::breaks::hold("pin-before-ledger", &mut || {});
        if break_mode("slow-pin-record") {
            std::thread::sleep(Duration::from_millis(200));
        }
        if break_mode("fault-pin-orphan") {
            // The agent died between mounting and recording.
            return Err(Refusal {
                status: 500,
                msg: "marsh: injected fault: the pin was mounted and not recorded".into(),
                close: false,
            });
        }
        let force = pinned.rec.read_only && !req.read_only;
        let pin_id = if pinned.id.is_empty() {
            None
        } else {
            if let Err(e) = self.svc.ledger.add_pin(&pinned.id, pinned.rec.clone()) {
                // Mounted but not remembered: undo, and refuse.
                self.svc
                    .pins
                    .unpin(&self.auth.workspace, &pinned.id, Some(&pinned.rec));
                return Err(led(e));
            }
            Some(pinned.id.clone())
        };
        self.svc.note_protected(&root);
        Ok(PinnedBind {
            path: pinned.path.to_string_lossy().into_owned(),
            force_read_only: force,
            pin_id,
        })
    }

    fn release_pins(&self, ids: &[String]) {
        self.svc.free_pins(ids);
    }

    fn substituted_socket(&self) -> Result<String, Refusal> {
        Ok(self
            .svc
            .socket_for(&self.auth)?
            .to_string_lossy()
            .into_owned())
    }

    fn scope_network(&self) -> Result<String, Refusal> {
        let ws = &self.auth.workspace;
        // The network may have been removed behind the ledger's back (a prune
        // before reconcile ran): trust it only if the engine still has it.
        let alive = |n: &str| {
            matches!(
                self.svc.up_request("GET", &format!("/networks/{n}"), None),
                Ok((200, _))
            )
        };
        if let Some(n) = self.svc.ledger.scope_network(ws)
            && alive(&n)
        {
            return Ok(n);
        }
        let _g = self
            .svc
            .net_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(n) = self.svc.ledger.scope_network(ws) {
            if alive(&n) {
                return Ok(n);
            }
            if let Lookup::Own(id) = self.svc.ledger.find_network(ws, &n) {
                self.svc.ledger.remove_network(&id).map_err(led)?;
            }
        }
        let name = format!("marsh-scope-{ws}");
        let fail = |m: String| Refusal {
            status: 500,
            msg: m,
            close: false,
        };
        let body = format!(
            "{{\"Name\":{},\"Driver\":\"bridge\",\"CheckDuplicate\":true,\"Labels\":{{\"dev.marsh.scope\":{},\"dev.marsh.attempt\":{}}}}}",
            Json::Str(name.clone()).to_string_json(),
            Json::Str(ws.clone()).to_string_json(),
            Json::Str(self.auth.attempt.clone()).to_string_json()
        );
        let (st, resp) = self
            .svc
            .up_request("POST", "/networks/create", Some(body.as_bytes()))
            .map_err(|e| fail(format!("engine unavailable: {e}")))?;
        let id = if st == 201 {
            serde_json::from_slice::<serde_json::Value>(&resp)
                .ok()
                .and_then(|v| v.get("Id").and_then(|i| i.as_str().map(str::to_string)))
        } else {
            // Already there (ledger lost, or a racing create): adopt it only
            // if it carries this scope's label.
            let (st2, r2) = self
                .svc
                .up_request("GET", &format!("/networks/{name}"), None)
                .map_err(|e| fail(format!("engine unavailable: {e}")))?;
            let v: Option<serde_json::Value> = if st2 == 200 {
                serde_json::from_slice(&r2).ok()
            } else {
                None
            };
            v.filter(|v| {
                v.pointer("/Labels/dev.marsh.scope")
                    .and_then(|s| s.as_str())
                    == Some(ws.as_str())
            })
            .and_then(|v| v.get("Id").and_then(|i| i.as_str().map(str::to_string)))
        };
        let id = id.ok_or_else(|| fail("could not create the workspace network".into()))?;
        let recorded = self
            .svc
            .ledger
            .add_network(
                &id,
                NetRec {
                    scope: ws.clone(),
                    attempt: self.auth.attempt.clone(),
                    name: name.clone(),
                    created: now(),
                },
            )
            .and_then(|()| self.svc.ledger.set_scope_network(ws, &name));
        if let Err(e) = recorded {
            let _ = self
                .svc
                .up_request("DELETE", &format!("/networks/{id}"), None);
            return Err(led(e));
        }
        Ok(name)
    }

    fn container(&self, reference: &str) -> Lookup {
        self.svc
            .ledger
            .find_container(&self.auth.workspace, reference)
    }

    fn exec_container(&self, exec_id: &str) -> Lookup {
        self.svc
            .ledger
            .exec_container(&self.auth.workspace, exec_id)
    }

    fn network(&self, reference: &str) -> Lookup {
        self.svc
            .ledger
            .find_network(&self.auth.workspace, reference)
    }

    fn container_pins(&self, container_id: &str) -> Vec<String> {
        self.svc.ledger.container_pins(container_id)
    }

    fn container_blocked(&self, container_id: &str) -> bool {
        self.svc.ledger.container_has_failed_pin(container_id)
    }

    fn image_volumes(&self, image: &str) -> Vec<String> {
        // The reference was validated by the schema; this is built into a request
        // line of our own, so it is checked once more here.
        if image.is_empty()
            || !image
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-:/@".contains(&b))
        {
            return Vec::new();
        }
        let Ok((200, body)) = self
            .svc
            .up_request("GET", &format!("/images/{image}/json"), None)
        else {
            return Vec::new();
        };
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        v.pointer("/Config/Volumes")
            .and_then(|m| m.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    fn current_limits(&self, container_id: &str) -> Result<CurrentLimits, Refusal> {
        let fail = |m: String| Refusal {
            status: 500,
            msg: m,
            close: false,
        };
        let (st, body) = self
            .svc
            .up_request("GET", &format!("/containers/{container_id}/json"), None)
            .map_err(|e| fail(format!("engine unavailable: {e}")))?;
        if st != 200 {
            return Err(Refusal::not_found(format!(
                "No such container: {container_id}"
            )));
        }
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let n = |k: &str| {
            v.pointer(&format!("/HostConfig/{k}"))
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0)
        };
        Ok(CurrentLimits {
            memory: n("Memory"),
            memory_swap: n("MemorySwap"),
            nano_cpus: n("NanoCpus"),
            cpu_quota: n("CpuQuota"),
            cpu_period: n("CpuPeriod"),
            pids: n("PidsLimit"),
        })
    }

    fn volume(&self, name: &str) -> VolumeScope {
        self.svc.ledger.volume(&self.auth.workspace, name)
    }

    fn ensure_volume(&self, name: &str) -> Result<(), Refusal> {
        let _g = self
            .svc
            .vol_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fail = |m: String| Refusal {
            status: 500,
            msg: m,
            close: false,
        };
        let ws = &self.auth.workspace;
        let claim = self
            .svc
            .ledger
            .claim_volume(ws, &self.auth.attempt, name)
            .map_err(led)?;
        if claim == VolumeClaim::Foreign {
            return Err(Refusal::forbid(format!(
                "volume {name:?} belongs to another workspace"
            )));
        }
        let give_back = |r: Refusal| {
            if claim == VolumeClaim::New {
                let _ = self.svc.ledger.unclaim_volume(ws, name);
            }
            r
        };
        let (st, body) = self
            .svc
            .up_request("GET", &format!("/volumes/{name}"), None)
            .map_err(|e| give_back(fail(format!("engine unavailable: {e}"))))?;
        if st == 200 {
            // An engine object nobody owns (and that is not ours by label) is
            // not ours to adopt.
            let mine = labelled_for(&body, ws);
            if claim == VolumeClaim::New && !mine {
                return Err(give_back(Refusal::forbid(format!(
                    "volume {name:?} exists and is not owned by this workspace"
                ))));
            }
            return Ok(());
        }
        let create = format!(
            "{{\"Name\":{},\"Labels\":{{\"dev.marsh.scope\":{},\"dev.marsh.attempt\":{}}}}}",
            Json::Str(name.to_string()).to_string_json(),
            Json::Str(ws.clone()).to_string_json(),
            Json::Str(self.auth.attempt.clone()).to_string_json()
        );
        let (st, _) = self
            .svc
            .up_request("POST", "/volumes/create", Some(create.as_bytes()))
            .map_err(|e| give_back(fail(format!("engine unavailable: {e}"))))?;
        if st != 201 {
            return Err(give_back(fail("could not create the volume".into())));
        }
        Ok(())
    }

    fn reserve_volume(&self, name: &str) -> Result<bool, Refusal> {
        let _g = self
            .svc
            .vol_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ws = &self.auth.workspace;
        let claim = self
            .svc
            .ledger
            .claim_volume(ws, &self.auth.attempt, name)
            .map_err(led)?;
        match claim {
            VolumeClaim::Foreign => Err(Refusal::forbid(format!(
                "volume {name:?} belongs to another workspace"
            ))),
            VolumeClaim::Own => Ok(false),
            VolumeClaim::New => {
                // The engine may already hold an object of that name that
                // nobody here owns: creating is idempotent and would adopt it.
                if let Ok((200, body)) =
                    self.svc
                        .up_request("GET", &format!("/volumes/{name}"), None)
                {
                    let mine = labelled_for(&body, ws);
                    if !mine {
                        let _ = self.svc.ledger.unclaim_volume(ws, name);
                        return Err(Refusal::forbid(format!(
                            "volume {name:?} exists and is not owned by this workspace"
                        )));
                    }
                }
                Ok(true)
            }
        }
    }

    fn release_volume(&self, name: &str) {
        let _ = self.svc.ledger.unclaim_volume(&self.auth.workspace, name);
    }

    fn socket_aliases(&self) -> &[String] {
        &self.svc.cfg.socket_aliases
    }

    fn registry_allowed(&self, image: &str) -> bool {
        let configured = match &self.svc.cfg.registry_config {
            Some(path) => {
                crate::engine::EngineConfig::load(path)
                    .unwrap_or_default()
                    .registry_allow
            }
            None => self.svc.cfg.registry_allow.clone(),
        };
        let Some(allow) = &configured else {
            return true;
        };
        let first = image.split('/').next().unwrap_or("");
        let host = if image.contains('/')
            && (first.contains('.') || first.contains(':') || first == "localhost")
        {
            first
        } else {
            "docker.io"
        };
        allow.iter().any(|a| a == host)
    }
}

/// `FilteredEndpoint::serve(listener, authority, upstream_socket)`.
pub struct FilteredEndpoint;

impl FilteredEndpoint {
    /// Serve `listener` with a fixed authority until it fails. `upstream` is
    /// the production engine's socket. The ledger and pins come from the
    /// process-wide default [`Services`] for that upstream.
    pub fn serve(
        listener: &UnixListener,
        authority: &Authority,
        upstream: &Path,
    ) -> io::Result<()> {
        let svc = default_services(upstream)?;
        svc.serve_listener(listener, authority)
    }

    pub fn serve_with(
        svc: &Arc<Services>,
        listener: &UnixListener,
        authority: &Authority,
    ) -> io::Result<()> {
        svc.serve_listener(listener, authority)
    }
}

static DEFAULT: OnceLock<Mutex<HashMap<PathBuf, Arc<Services>>>> = OnceLock::new();

pub fn default_services(upstream: &Path) -> io::Result<Arc<Services>> {
    let map = DEFAULT.get_or_init(Mutex::default);
    let mut g = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(s) = g.get(upstream) {
        return Ok(Arc::clone(s));
    }
    let s = Services::new(Config::default(), upstream)?;
    g.insert(upstream.to_path_buf(), Arc::clone(&s));
    Ok(s)
}

// --- one connection --------------------------------------------------------------

fn client_gone(s: &UnixStream) -> bool {
    use rustix::net::{RecvFlags, recv};
    let mut b = [0u8; 1];
    s.set_nonblocking(true).ok();
    let r = recv(s, &mut b[..], RecvFlags::PEEK);
    s.set_nonblocking(false).ok();
    matches!(r, Ok((0, _)))
}

fn serve_conn(scope: &Arc<Scope>, stream: UnixStream) {
    let limits = scope.svc.cfg.http.clone();
    let _ = stream.set_write_timeout(Some(limits.progress));
    if break_mode("filter-off") {
        let Ok(up) = scope.svc.dial() else { return };
        pump(scope, stream, up, &[], &[]);
        return;
    }
    let mut conn = Conn::new(stream, limits);
    loop {
        let head = match conn.read_request_head() {
            Ok(h) => h,
            Err(HttpError::Closed | HttpError::Timeout | HttpError::Io(_)) => return,
            Err(HttpError::Bad(m)) => return reply(&mut conn.stream, 400, m, true),
            Err(HttpError::HeadersTooLarge) => {
                return reply(&mut conn.stream, 431, "request headers are too large", true);
            }
            Err(HttpError::BodyTooLarge) => {
                return reply(&mut conn.stream, 413, "request body is too large", true);
            }
            Err(HttpError::Expect) => {
                return reply(
                    &mut conn.stream,
                    417,
                    "Expect: 100-continue is not supported",
                    true,
                );
            }
        };
        if head.upgrade.is_some() && break_mode("filter-raw-after-upgrade-header") {
            // Defect: any Upgrade header switches to a blind byte pump.
            let Ok(up) = scope.svc.dial() else { return };
            let leftover = conn.take_buffered();
            pump(scope, conn.stream, up, &head.raw, &leftover);
            return;
        }
        match handle_request(scope, &mut conn, &head) {
            Ok(true) if !head.close => {}
            _ => return,
        }
    }
}

fn reply(s: &mut UnixStream, status: u16, msg: &str, close: bool) {
    let _ = write_refusal(s, status, msg, close);
}

/// Answer a refusal. `body_read`: the request body was already consumed.
fn refuse(conn: &mut Conn, head: &Head, r: &Refusal, body_read: bool) -> bool {
    let keep = !r.close && (body_read || conn.drain(head.framing, 64 * 1024));
    let close = !keep || head.close;
    let _ = write_refusal(&mut conn.stream, r.status, &r.msg, close);
    scope_log(&format!(
        "refused {} {}: {}",
        head.method,
        head.target.split('?').next().unwrap_or(""),
        r.msg
    ));
    keep
}

fn scope_log(m: &str) {
    if std::env::var_os("MARSH_E3_LOG").is_some() {
        eprintln!("marsh-services: {m}");
    }
}

/// Returns whether the connection can carry another request.
#[allow(clippy::unnecessary_wraps)]
fn handle_request(scope: &Arc<Scope>, conn: &mut Conn, head: &Head) -> io::Result<bool> {
    let svc = &scope.svc;
    let target = match parse_target(&head.target) {
        Ok(t) => t,
        Err(m) => return Ok(refuse(conn, head, &Refusal::forbid(m), false)),
    };
    let m = match classify(&head.method, &target) {
        Ok(m) => m,
        Err(r) => return Ok(refuse(conn, head, &r, false)),
    };
    let _mutation = if head.method == "GET" || head.method == "HEAD" {
        None
    } else {
        // Nothing new is admitted while the ledger cannot be written.
        if svc.ledger.degraded() && !svc.ledger.probe() {
            let r = Refusal {
                status: 503,
                msg: "ownership records cannot be saved; try again later".into(),
                close: false,
            };
            return Ok(refuse(conn, head, &r, false));
        }
        let busy = || Refusal {
            status: 503,
            msg: "a checkpoint is being taken; try again in a moment".into(),
            close: false,
        };
        match gate_class(m.kind) {
            Gate_::Counted => match svc.begin_mutation(&scope.auth.workspace) {
                Some(g) => Some(g),
                None => return Ok(refuse(conn, head, &busy(), false)),
            },
            Gate_::RefuseOnly => {
                if svc.is_frozen(&scope.auth.workspace) && !break_mode("capture-api-gate-off") {
                    return Ok(refuse(conn, head, &busy(), false));
                }
                None
            }
            Gate_::Free => None,
        }
    };
    let policy = body_policy(m.kind);
    let body = match policy {
        BodyPolicy::None => {
            if !matches!(head.framing, Framing::None | Framing::Length(0)) {
                return Ok(refuse(
                    conn,
                    head,
                    &Refusal::forbid("this route does not take a request body").closing(),
                    false,
                ));
            }
            None
        }
        BodyPolicy::Discard => match conn.read_body_limited(head.framing, svc.cfg.http.max_json) {
            Ok(_) => None,
            Err(HttpError::BodyTooLarge) => {
                reply(&mut conn.stream, 413, "request body is too large", true);
                return Ok(false);
            }
            Err(_) => return Ok(false),
        },
        BodyPolicy::Json { .. } => {
            match conn.read_body_limited(head.framing, svc.cfg.http.max_json) {
                Ok(b) => Some(b),
                Err(HttpError::BodyTooLarge) => {
                    reply(
                        &mut conn.stream,
                        413,
                        "JSON body is larger than 1 MiB",
                        true,
                    );
                    return Ok(false);
                }
                Err(HttpError::Bad(m)) => {
                    reply(&mut conn.stream, 400, m, true);
                    return Ok(false);
                }
                Err(_) => return Ok(false),
            }
        }
        BodyPolicy::Stream => None,
    };
    let client_body = body.clone();
    let mut adm = match admit(&m, head, body, scope.as_ref()) {
        Ok(a) => a,
        Err(r) => {
            return Ok(refuse(
                conn,
                head,
                &r,
                matches!(policy, BodyPolicy::Json { .. } | BodyPolicy::Discard),
            ));
        }
    };
    adm.client_body = client_body;
    if break_mode("filter-no-upgrade") {
        adm.hijack = None;
        adm.upgrade = None;
    }
    let result = forward(scope, conn, head, &mut adm, m.kind);
    match result {
        Ok(keep) => Ok(keep),
        Err(e) => {
            svc.log(&format!("forward failed: {e}"));
            Ok(false)
        }
    }
}

/// How a request interacts with a capture freeze (CAP-11).
enum Gate_ {
    /// Changes the scope's state: counted, so a freeze waits for it, and
    /// refused once the freeze has begun.
    Counted,
    /// Long-lived or outside the scope's container state (exec, build, image
    /// tags): refused while frozen, not waited for.
    RefuseOnly,
    /// Observes or resizes: never blocked.
    Free,
}

fn gate_class(kind: Kind) -> Gate_ {
    match kind {
        Kind::ContainerWait
        | Kind::ContainerAttach
        | Kind::ContainerAttachWs
        | Kind::ExecResize
        | Kind::ContainerResize => Gate_::Free,
        Kind::ExecStart
        | Kind::Build
        | Kind::ImageCreate
        | Kind::ImageTag
        | Kind::ImageDelete
        | Kind::Session
        | Kind::Grpc => Gate_::RefuseOnly,
        _ => Gate_::Counted,
    }
}

fn write_upstream_head(
    up: &mut UnixStream,
    adm: &Admitted,
    raw_head: Option<&Head>,
) -> io::Result<()> {
    if let Some(h) = raw_head {
        // Defect: the client's own head, byte for byte.
        return up.write_all(&h.raw);
    }
    let mut path = adm.path.clone();
    if !adm.query.is_empty() {
        path.push('?');
        path.push_str(&encode_query(&adm.query));
    }
    let mut out = format!("{} {path} HTTP/1.1\r\nHost: docker\r\n", adm.method);
    for (k, v) in &adm.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    if let Some(ct) = &adm.content_type {
        out.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    match &adm.upgrade {
        Some(u) if adm.hijack.is_some() => {
            out.push_str(&format!("Connection: Upgrade\r\nUpgrade: {u}\r\n"));
        }
        _ => out.push_str("Connection: close\r\n"),
    }
    match &adm.body {
        ReqBody::None | ReqBody::Stream(Framing::None) => {
            if adm.method != "GET" && adm.method != "HEAD" {
                out.push_str("Content-Length: 0\r\n");
            }
        }
        ReqBody::Json(b) => out.push_str(&format!("Content-Length: {}\r\n", b.len())),
        ReqBody::Stream(Framing::Length(n)) => out.push_str(&format!("Content-Length: {n}\r\n")),
        ReqBody::Stream(Framing::Chunked) => out.push_str("Transfer-Encoding: chunked\r\n"),
    }
    out.push_str("\r\n");
    up.write_all(out.as_bytes())
}

fn is_raw_stream(resp: &RespHead) -> bool {
    let ct = resp
        .content_type
        .as_deref()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    resp.status == 200
        && (ct == "application/vnd.docker.raw-stream"
            || ct == "application/vnd.docker.multiplexed-stream")
}

fn forward(
    scope: &Arc<Scope>,
    conn: &mut Conn,
    head: &Head,
    adm: &mut Admitted,
    kind: Kind,
) -> io::Result<bool> {
    let svc = &scope.svc;
    let mut up = match svc.dial() {
        Ok(u) => u,
        Err(e) => {
            undo_post(scope, &adm.post);
            reply(
                &mut conn.stream,
                502,
                &format!("the engine is not available: {e}"),
                true,
            );
            return Ok(false);
        }
    };
    let forward_original = break_mode("filter-forward-original-bytes");
    if forward_original {
        // Defect: the client's head and body, byte for byte. The JSON body was
        // read for validation already; a streamed one has not been.
        write_upstream_head(&mut up, adm, Some(head))?;
        let body = match adm.client_body.clone() {
            Some(b) => b,
            None => conn
                .read_body_limited(head.framing, svc.cfg.http.max_json)
                .map_err(http_io)?,
        };
        if head.framing == Framing::Chunked {
            if !body.is_empty() {
                write!(up, "{:x}\r\n", body.len())?;
                up.write_all(&body)?;
                up.write_all(b"\r\n")?;
            }
            up.write_all(b"0\r\n\r\n")?;
        } else {
            up.write_all(&body)?;
        }
    } else {
        // Canonical body: the one the policy validated (or, under the
        // lenient-JSON defect, the client's).
        if let Some(raw) = &adm.raw_body {
            adm.body = ReqBody::Json(raw.clone());
        }
        write_upstream_head(&mut up, adm, None)?;
        match &adm.body {
            ReqBody::Json(b) => up.write_all(b)?,
            ReqBody::Stream(f) => {
                let chunked = *f == Framing::Chunked;
                let mut st = BodyState::new(*f);
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    let n = match conn.read_body(&mut st, &mut buf) {
                        Ok(n) => n,
                        Err(e) => {
                            // The upstream request is incomplete; drop it.
                            undo_post(scope, &adm.post);
                            return Err(http_io(e));
                        }
                    };
                    if n == 0 {
                        break;
                    }
                    if chunked {
                        write!(up, "{n:x}\r\n")?;
                        up.write_all(&buf[..n])?;
                        up.write_all(b"\r\n")?;
                    } else if up.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                if chunked {
                    up.write_all(b"0\r\n\r\n")?;
                }
            }
            ReqBody::None => {}
        }
    }
    up.flush()?;

    let mut upc = Conn::new(
        up,
        HttpLimits {
            progress: Duration::from_hours(24),
            max_head: 128 * 1024,
            ..svc.cfg.http.clone()
        },
    );
    let watch = conn.stream.try_clone()?;
    upc.poll = Some((
        Duration::from_secs(2),
        Box::new(move || client_gone(&watch)),
    ));
    let resp = match upc.read_response_head(head.method == "HEAD") {
        Ok(r) => r,
        Err(e) => {
            undo_post(scope, &adm.post);
            reply(
                &mut conn.stream,
                502,
                &format!("bad answer from the engine: {e:?}"),
                true,
            );
            return Ok(false);
        }
    };

    // Hijack: only on an allowed route, and only if the engine itself switched.
    if adm.hijack.is_some() {
        let switched = (resp.status == 101 && adm.upgrade.is_some()) || is_raw_stream(&resp);
        if switched {
            let mut head_out = format!(
                "HTTP/1.1 {} {}\r\n",
                resp.status,
                if resp.reason.is_empty() {
                    "OK"
                } else {
                    &resp.reason
                }
            );
            for (k, v) in &resp.headers {
                if !matches!(
                    k.as_str(),
                    "connection"
                        | "upgrade"
                        | "keep-alive"
                        | "content-length"
                        | "transfer-encoding"
                ) {
                    head_out.push_str(&format!("{}: {v}\r\n", canonical_header(k)));
                }
            }
            if resp.status == 101 {
                head_out.push_str(&format!(
                    "Connection: Upgrade\r\nUpgrade: {}\r\n",
                    resp.upgrade.clone().unwrap_or_else(|| "tcp".into())
                ));
            }
            head_out.push_str("\r\n");
            conn.stream.write_all(head_out.as_bytes())?;
            // Bytes the engine sent right behind its head belong to the stream.
            let up_left = upc.take_buffered();
            conn.stream.write_all(&up_left)?;
            let client_left = conn.take_buffered();
            let client = conn.stream.try_clone()?;
            let upstream = upc.stream.try_clone()?;
            pump(scope, client, upstream, &[], &client_left);
            return Ok(false);
        }
    }
    if resp.status == 101 {
        reply(
            &mut conn.stream,
            502,
            "unexpected protocol switch from the engine",
            true,
        );
        return Ok(false);
    }

    respond(scope, conn, head, adm, kind, &mut upc, &resp)
}

/// The request failed before the engine answered: give back what admission took
/// (the pins of a create, the reservation of a volume name).
fn undo_post(scope: &Scope, p: &Post) {
    match p {
        Post::ContainerCreate { pins, .. } => scope.svc.free_pins(pins),
        Post::VolumeCreate {
            name: Some(n),
            reserved: true,
        } => scope.release_volume(n),
        _ => {}
    }
}

fn canonical_header(k: &str) -> String {
    k.split('-')
        .map(|p| {
            let mut c = p.chars();
            c.next().map_or(String::new(), |f| {
                f.to_ascii_uppercase().to_string() + c.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn read_resp_body(c: &mut Conn, resp: &RespHead, cap: usize) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    let mut st = match resp.framing {
        RespFraming::NoBody => return Ok(out),
        RespFraming::Length(n) => BodyState::new(Framing::Length(n)),
        RespFraming::Chunked => BodyState::new(Framing::Chunked),
        RespFraming::UntilClose => loop {
            let n = c.read_until_close(&mut buf)?;
            if n == 0 {
                return Ok(out);
            }
            if out.len() + n > cap {
                return Err(HttpError::BodyTooLarge);
            }
            out.extend_from_slice(&buf[..n]);
        },
    };
    loop {
        let n = c.read_body(&mut st, &mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        if out.len() + n > cap {
            return Err(HttpError::BodyTooLarge);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Out {
    Length(u64),
    Chunked,
    Close,
    /// HEAD and bodiless statuses keep the engine's own Content-Length.
    Bodiless,
}

fn write_resp_head(
    client: &mut UnixStream,
    resp: &RespHead,
    out: Out,
    keep: bool,
    content_length_override: Option<usize>,
) -> io::Result<()> {
    let mut s = format!(
        "HTTP/1.1 {} {}\r\n",
        resp.status,
        if resp.reason.is_empty() {
            "OK"
        } else {
            &resp.reason
        }
    );
    for (k, v) in &resp.headers {
        let drop = matches!(
            k.as_str(),
            "connection"
                | "keep-alive"
                | "transfer-encoding"
                | "upgrade"
                | "trailer"
                | "te"
                | "proxy-authenticate"
                | "proxy-authorization"
        ) || (k == "content-length" && out != Out::Bodiless);
        if !drop {
            s.push_str(&format!("{}: {v}\r\n", canonical_header(k)));
        }
    }
    match out {
        Out::Length(n) => s.push_str(&format!(
            "Content-Length: {}\r\n",
            content_length_override.map_or(n, |o| o as u64)
        )),
        Out::Chunked => s.push_str("Transfer-Encoding: chunked\r\n"),
        Out::Close | Out::Bodiless => {}
    }
    if !keep || out == Out::Close {
        s.push_str("Connection: close\r\n");
    }
    s.push_str("\r\n");
    client.write_all(s.as_bytes())
}

fn write_chunk(client: &mut UnixStream, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    write!(client, "{:x}\r\n", data.len())?;
    client.write_all(data)?;
    client.write_all(b"\r\n")?;
    client.flush()
}

/// Events are filtered line by line against the ledger.
struct EventFilter<'a> {
    scope: &'a Scope,
    buf: Vec<u8>,
}

impl EventFilter<'_> {
    fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            if self.keep(&line) {
                out.extend_from_slice(&line);
            }
        }
        if self.buf.len() > 1 << 20 {
            self.buf.clear();
        }
        out
    }

    fn keep(&self, line: &[u8]) -> bool {
        if break_mode("lane-authority-off") {
            return true;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
            return false;
        };
        let ty = v.get("Type").and_then(|t| t.as_str()).unwrap_or("");
        let id = v
            .pointer("/Actor/ID")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let ws = &self.scope.auth.workspace;
        let l = &self.scope.svc.ledger;
        match ty {
            "container" => l.container(id).is_some_and(|c| &c.scope == ws),
            "network" => l.owns_network(ws, id, ""),
            "volume" => l.owns_volume(ws, id),
            _ => false,
        }
    }
}

/// `None`: the body could not be filtered.
fn filter_list(scope: &Scope, post: &Post, body: &[u8]) -> Option<Vec<u8>> {
    if break_mode("lane-authority-off") {
        return Some(body.to_vec());
    }
    let ws = &scope.auth.workspace;
    let l = &scope.svc.ledger;
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    match post {
        Post::ListContainers => {
            let arr = v.as_array_mut()?;
            arr.retain(|c| {
                c.get("Id")
                    .and_then(|i| i.as_str())
                    .is_some_and(|id| l.container(id).is_some_and(|r| &r.scope == ws))
            });
        }
        Post::ListNetworks => {
            let arr = v.as_array_mut()?;
            arr.retain(|n| {
                let id = n.get("Id").and_then(|i| i.as_str()).unwrap_or("");
                let name = n.get("Name").and_then(|i| i.as_str()).unwrap_or("");
                l.owns_network(ws, id, name)
            });
        }
        Post::ListVolumes => {
            let arr = v.get_mut("Volumes")?.as_array_mut()?;
            arr.retain(|x| {
                x.get("Name")
                    .and_then(|i| i.as_str())
                    .is_some_and(|n| l.owns_volume(ws, n))
            });
        }
        _ => return None,
    }
    serde_json::to_vec(&v).ok()
}

fn respond(
    scope: &Arc<Scope>,
    conn: &mut Conn,
    head: &Head,
    adm: &Admitted,
    _kind: Kind,
    upc: &mut Conn,
    resp: &RespHead,
) -> io::Result<bool> {
    let svc = &scope.svc;
    let keep = !head.close;
    let ok = resp.status / 100 == 2;
    let buffered = matches!(
        adm.post,
        Post::ContainerCreate { .. }
            | Post::ExecCreate { .. }
            | Post::NetworkCreate { .. }
            | Post::VolumeCreate { .. }
            | Post::ListContainers
            | Post::ListNetworks
            | Post::ListVolumes
    );
    if buffered && resp.framing != RespFraming::NoBody {
        let body = match read_resp_body(upc, resp, 32 << 20) {
            Ok(b) => b,
            Err(e) => {
                undo_post(scope, &adm.post);
                reply(
                    &mut conn.stream,
                    502,
                    &format!("bad answer from the engine: {e:?}"),
                    true,
                );
                return Ok(false);
            }
        };
        let mut out = body.clone();
        if let Err(r) = record(scope, &adm.post, resp.status, &body, &mut out) {
            scope_log(&format!("not recorded: {}", r.msg));
            reply(&mut conn.stream, r.status, &r.msg, false);
            return Ok(keep);
        }
        write_resp_head(
            &mut conn.stream,
            resp,
            Out::Length(out.len() as u64),
            keep,
            Some(out.len()),
        )?;
        conn.stream.write_all(&out)?;
        conn.stream.flush()?;
        return Ok(keep);
    }
    if let Post::ContainerCreate { pins, .. } = &adm.post
        && !ok
    {
        svc.free_pins(pins);
    }

    // Streamed: logs, events, build output, archives, pulls, waits.
    let client_minor = head.minor;
    let out = match resp.framing {
        RespFraming::NoBody => Out::Bodiless,
        RespFraming::Length(n) if !matches!(adm.post, Post::Events) => Out::Length(n),
        RespFraming::UntilClose => Out::Close,
        _ if client_minor == 0 => Out::Close,
        _ => Out::Chunked,
    };
    write_resp_head(&mut conn.stream, resp, out, keep, None)?;
    let mut filter = matches!(adm.post, Post::Events).then(|| EventFilter {
        scope,
        buf: Vec::new(),
    });
    let mut st = match resp.framing {
        RespFraming::NoBody | RespFraming::UntilClose => BodyState::Done,
        RespFraming::Length(n) => BodyState::new(Framing::Length(n)),
        RespFraming::Chunked => BodyState::new(Framing::Chunked),
    };
    let mut buf = vec![0u8; 16 * 1024];
    let mut clean = true;
    loop {
        let n = if resp.framing == RespFraming::UntilClose {
            upc.read_until_close(&mut buf)
        } else {
            upc.read_body(&mut st, &mut buf)
        };
        let n = match n {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => {
                clean = false;
                break;
            }
        };
        let piece: Vec<u8>;
        let data: &[u8] = match filter.as_mut() {
            Some(f) => {
                piece = f.feed(&buf[..n]);
                &piece
            }
            None => &buf[..n],
        };
        let res = match out {
            Out::Chunked => write_chunk(&mut conn.stream, data),
            _ => conn
                .stream
                .write_all(data)
                .and_then(|()| conn.stream.flush()),
        };
        if res.is_err() {
            return Ok(false);
        }
    }
    if out == Out::Chunked && clean {
        conn.stream.write_all(b"0\r\n\r\n")?;
    }
    conn.stream.flush()?;

    if ok {
        post_actions(scope, &adm.post);
    }
    Ok(clean && keep && out != Out::Close)
}

/// Engine-side undo of an object whose ownership could not be recorded.
fn undo(svc: &Services, path: &str) {
    if let Err(e) = svc.up_request("DELETE", path, None) {
        svc.log(&format!("could not undo {path}: {e}"));
    }
}

fn id_of(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("Id").and_then(|i| i.as_str().map(str::to_string)))
}

/// Remember what the engine just created for this scope. A ledger that cannot
/// be written refuses the request and takes the new object back: nothing
/// exists that the agent cannot account for.
fn record(
    scope: &Arc<Scope>,
    post: &Post,
    status: u16,
    body: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), Refusal> {
    let svc = &scope.svc;
    let ws = &scope.auth.workspace;
    let created = status == 201;
    match post {
        Post::ContainerCreate {
            name,
            pins,
            inherited,
        } => {
            if created {
                record_container(scope, body, name.as_deref(), pins, inherited)?;
            } else {
                svc.free_pins(pins);
                if status == 409 {
                    *out = br#"{"message":"Conflict. The container name is already in use."}"#
                        .to_vec();
                }
            }
        }
        Post::ExecCreate { container } if created => {
            if let Some(id) = id_of(body) {
                svc.ledger.add_exec(&id, container).map_err(led)?;
            }
        }
        Post::NetworkCreate { name } if created => {
            if let Some(id) = id_of(body) {
                let rec = NetRec {
                    scope: ws.clone(),
                    attempt: scope.auth.attempt.clone(),
                    name: name.clone(),
                    created: now(),
                };
                if let Err(e) = svc.ledger.add_network(&id, rec) {
                    undo(svc, &format!("/networks/{id}"));
                    return Err(led(e));
                }
            }
        }
        Post::VolumeCreate { name, reserved } => {
            if created {
                let n = name.clone().or_else(|| {
                    serde_json::from_slice::<serde_json::Value>(body)
                        .ok()
                        .and_then(|v| v.get("Name").and_then(|i| i.as_str().map(str::to_string)))
                });
                if let Some(n) = n {
                    match svc.ledger.claim_volume(ws, &scope.auth.attempt, &n) {
                        Ok(VolumeClaim::Foreign) => {
                            // Cannot happen under the reservation; never adopt.
                            svc.log(&format!("volume {n} created but owned elsewhere"));
                            return Err(Refusal::forbid("volume belongs to another workspace"));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            undo(svc, &format!("/volumes/{n}"));
                            return Err(led(e));
                        }
                    }
                }
            } else if *reserved && let Some(n) = name {
                scope.release_volume(n);
            }
        }
        Post::ListContainers | Post::ListNetworks | Post::ListVolumes if status / 100 == 2 => {
            match filter_list(scope, post, body) {
                Some(f) => *out = f,
                // An answer that cannot be filtered is not passed on: the
                // client would see other workspaces' objects.
                None if break_mode("list-fail-open") => {}
                None => {
                    return Err(Refusal {
                        status: 502,
                        msg: "the engine's list could not be filtered".into(),
                        close: false,
                    });
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn record_container(
    scope: &Arc<Scope>,
    body: &[u8],
    name: Option<&str>,
    pins: &[String],
    inherited: &[String],
) -> Result<(), Refusal> {
    let svc = &scope.svc;
    let ws = &scope.auth.workspace;
    let Some(id) = id_of(body) else {
        svc.free_pins(pins);
        return Err(Refusal {
            status: 502,
            msg: "bad answer from the engine".into(),
            close: false,
        });
    };
    let mut held = pins.to_vec();
    held.extend(inherited.iter().cloned());
    held.dedup();
    let rec = ContainerRec {
        scope: ws.clone(),
        attempt: scope.auth.attempt.clone(),
        name: name.map(str::to_string),
        pins: held,
        created: now(),
    };
    if let Err(e) = svc.ledger.add_container(&id, rec) {
        undo(svc, &format!("/containers/{id}?force=true&v=true"));
        svc.free_pins(pins);
        return Err(led(e));
    }
    // Volumes Docker made for the container (image VOLUME, anonymous mounts)
    // belong to the creator's scope too.
    if let Ok((200, b)) = svc.up_request("GET", &format!("/containers/{id}/json"), None)
        && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b)
        && let Some(ms) = v.get("Mounts").and_then(|m| m.as_array())
    {
        for m in ms
            .iter()
            .filter(|m| m.get("Type").and_then(|t| t.as_str()) == Some("volume"))
        {
            if let Some(n) = m.get("Name").and_then(|n| n.as_str())
                && svc.ledger.volume(ws, n) == VolumeScope::Unknown
                && let Err(e) = svc.ledger.claim_volume(ws, &scope.auth.attempt, n)
            {
                undo(svc, &format!("/containers/{id}?force=true&v=true"));
                let _ = svc.ledger.remove_container(&id);
                svc.free_pins(pins);
                return Err(led(e));
            }
        }
    }
    Ok(())
}

fn post_actions(scope: &Arc<Scope>, post: &Post) {
    let svc = &scope.svc;
    let ws = &scope.auth.workspace;
    match post {
        Post::ContainerDelete { id } => match svc.ledger.remove_container(id) {
            Ok(freed) => svc.unpin_freed(freed),
            Err(e) => svc.log(&format!("container {id} deleted but not forgotten: {e}")),
        },
        Post::ContainerRename { id, name } => {
            if let Err(e) = svc.ledger.rename_container(id, name) {
                svc.log(&format!("rename of {id} not recorded: {e}"));
            }
        }
        Post::NetworkDelete { id } => {
            if let Err(e) = svc.ledger.remove_network(id) {
                svc.log(&format!("network {id} deleted but not forgotten: {e}"));
            }
        }
        Post::VolumeDelete { name } => {
            if let Err(e) = svc.ledger.remove_volume(ws, name) {
                svc.log(&format!("volume {name} deleted but not forgotten: {e}"));
            }
        }
        Post::Prune => svc.reconcile(),
        _ => {}
    }
}

/// Bidirectional raw copy after an authorized hijack.
///
/// One thread polls both ends, so that neither a vanished peer nor a stalled
/// one can hold the connection (and its slot) for ever:
/// - any byte either way resets the idle deadline (`hijack_idle`);
/// - a peer that has fully gone (POLLHUP/POLLERR, which a half-close does not
///   raise) ends the stream at once, and the other side is shut;
/// - writes time out after `progress`, so a peer that stops reading is cut;
/// - after the client half-closes, the engine's output is still delivered
///   (`docker exec -i < file`), within the idle deadline;
/// - when the engine side ends, the stream ends: nothing more can arrive.
fn pump(
    scope: &Arc<Scope>,
    client: UnixStream,
    upstream: UnixStream,
    to_upstream_first: &[u8],
    from_client_buffered: &[u8],
) {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use std::io::Read;
    use std::net::Shutdown;
    use std::time::Instant;

    let cfg = &scope.svc.cfg;
    if break_mode("hijack-deadlines-off") {
        return pump_blind(client, upstream, to_upstream_first, from_client_buffered);
    }
    let (mut c, mut u) = (client, upstream);
    let _ = c.set_read_timeout(None);
    let _ = u.set_read_timeout(None);
    let _ = c.set_write_timeout(Some(cfg.http.progress));
    let _ = u.set_write_timeout(Some(cfg.http.progress));
    if u.write_all(to_upstream_first).is_err() || u.write_all(from_client_buffered).is_err() {
        return;
    }
    let tick = cfg.hijack_poll.max(Duration::from_millis(10));
    let mut last = Instant::now();
    let mut client_open = true;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        if last.elapsed() >= cfg.hijack_idle {
            scope_log("hijacked stream idle: closed");
            break;
        }
        let ts = Timespec {
            tv_sec: i64::try_from(tick.as_secs()).unwrap_or(1),
            tv_nsec: i64::from(tick.subsec_nanos()),
        };
        let (cf, uf) = {
            let mut fds = [
                PollFd::new(
                    &c,
                    if client_open {
                        PollFlags::IN
                    } else {
                        PollFlags::empty()
                    },
                ),
                PollFd::new(&u, PollFlags::IN),
            ];
            match poll(&mut fds, Some(&ts)) {
                Ok(_) => (fds[0].revents(), fds[1].revents()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => break,
            }
        };
        let gone = PollFlags::ERR | PollFlags::NVAL;
        if cf.intersects(gone) || (!client_open && cf.contains(PollFlags::HUP)) {
            scope_log("hijacked client is gone: closed");
            break;
        }
        if uf.contains(PollFlags::NVAL) || uf.contains(PollFlags::ERR) {
            break;
        }
        if client_open && cf.intersects(PollFlags::IN | PollFlags::HUP) {
            match c.read(&mut buf) {
                Ok(0) | Err(_) => {
                    client_open = false;
                    let _ = u.shutdown(Shutdown::Write);
                    last = Instant::now();
                }
                Ok(n) => {
                    if u.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    last = Instant::now();
                }
            }
        }
        if uf.intersects(PollFlags::IN | PollFlags::HUP) {
            match u.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if c.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    last = Instant::now();
                }
            }
        }
    }
    let _ = c.shutdown(Shutdown::Both);
    let _ = u.shutdown(Shutdown::Both);
}

/// `hijack-deadlines-off`: the original two-thread copy with no deadline and
/// no check for a vanished peer.
fn pump_blind(
    client: UnixStream,
    upstream: UnixStream,
    to_upstream_first: &[u8],
    from_client_buffered: &[u8],
) {
    use std::io::Read;
    use std::net::Shutdown;
    let (Ok(mut c_r), Ok(mut u_w)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let (mut u_r, mut c_w) = (upstream, client);
    let _ = c_r.set_read_timeout(None);
    let _ = u_r.set_read_timeout(None);
    let _ = c_w.set_write_timeout(None);
    let _ = u_w.set_write_timeout(None);
    let t = std::thread::spawn(move || {
        let mut b = vec![0u8; 16 * 1024];
        loop {
            match u_r.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if c_w.write_all(&b[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = c_w.shutdown(Shutdown::Write);
    });
    if u_w.write_all(to_upstream_first).is_ok() && u_w.write_all(from_client_buffered).is_ok() {
        let mut b = vec![0u8; 16 * 1024];
        loop {
            match c_r.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if u_w.write_all(&b[..n]).is_err() {
                        break;
                    }
                }
            }
        }
    }
    let _ = u_w.shutdown(Shutdown::Write);
    let _ = t.join();
}

#[cfg(test)]
#[path = "endpoint_tests.rs"]
mod tests;
