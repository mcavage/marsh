//! The endpoint against a scripted engine, in process (no VM): the behaviors of
//! the E3 round-2 fixes that need a connection, a clock or a failing disk.
//!
//! Each test states what must hold. `e3/witness.sh` runs every test again with
//! the break mode that reintroduces its defect and requires the test to fail
//! (`E3_TEST_BREAKS`, read only by the unit-test build, see `breaks.rs`).

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Authority, Config, Services};
use crate::engine_api::Untag;
use crate::ledger::{ContainerRec, PinRec, now};

const NET_ID: &str = "7777777777777777777777777777777777777777777777777777777777777777";

#[derive(Clone, Debug)]
struct Req {
    method: String,
    target: String,
    body: Vec<u8>,
}

type Handler = Arc<dyn Fn(&Req, &mut UnixStream) -> bool + Send + Sync>;

struct Engine {
    log: Arc<Mutex<Vec<Req>>>,
    release: Arc<AtomicBool>,
    created: Arc<AtomicUsize>,
}

fn read_req(s: &mut UnixStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut b = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = s.read(&mut b).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&b[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.lines();
    let first = lines.next()?;
    let mut it = first.split(' ');
    let (method, target) = (it.next()?.to_string(), it.next()?.to_string());
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = buf[end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut b).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&b[..n]);
    }
    Some(Req {
        method,
        target,
        body,
    })
}

fn answer(s: &mut UnixStream, status: u16, body: &str) {
    let _ = write!(
        s,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

impl Engine {
    /// `custom` sees every request first; it returns whether it answered.
    fn start(path: &Path, custom: Handler) -> Self {
        let log: Arc<Mutex<Vec<Req>>> = Arc::default();
        let release = Arc::new(AtomicBool::new(false));
        let created = Arc::new(AtomicUsize::new(0));
        let l = UnixListener::bind(path).unwrap();
        let (log2, created2) = (log.clone(), created.clone());
        std::thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { continue };
                let (log, custom, created) = (log2.clone(), custom.clone(), created2.clone());
                std::thread::spawn(move || {
                    let Some(req) = read_req(&mut s) else { return };
                    log.lock().unwrap().push(req.clone());
                    if custom(&req, &mut s) {
                        return;
                    }
                    let t = req.target.split('?').next().unwrap_or("");
                    match (req.method.as_str(), t) {
                        ("POST", "/networks/create") => {
                            answer(&mut s, 201, &format!("{{\"Id\":\"{NET_ID}\"}}"));
                        }
                        ("GET", t) if t.starts_with("/networks/") => {
                            answer(&mut s, 200, &format!("{{\"Id\":\"{NET_ID}\"}}"));
                        }
                        ("POST", "/containers/create") => {
                            let n = created.fetch_add(1, Ordering::SeqCst) + 1;
                            answer(&mut s, 201, &format!("{{\"Id\":\"{n:064x}\"}}"));
                        }
                        ("GET", t) if t.starts_with("/containers/") && t.ends_with("/json") => {
                            answer(&mut s, 200, "{\"Mounts\":[]}");
                        }
                        ("DELETE", _) => {
                            let _ = s
                                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
                        }
                        _ => answer(&mut s, 404, "{\"message\":\"no such object\"}"),
                    }
                });
            }
        });
        Self {
            log,
            release,
            created,
        }
    }

    fn last_body(&self, method: &str, prefix: &str) -> String {
        self.log
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|r| r.method == method && r.target.starts_with(prefix))
            .map_or_default(|r| String::from_utf8_lossy(&r.body).into_owned())
    }

    fn saw(&self, method: &str, prefix: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.target.starts_with(prefix))
            .count()
    }
}

struct H {
    dir: PathBuf,
    svc: Arc<Services>,
    engine: Engine,
    auth: Authority,
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("e3-ep-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn no_custom() -> Handler {
    Arc::new(|_, _| false)
}

impl H {
    fn new(
        name: &str,
        file_ledger: bool,
        custom: Handler,
        tweak: impl FnOnce(&mut Config),
    ) -> Self {
        let dir = scratch(name);
        let mut cfg = Config::default();
        cfg.paths.ledger = if file_ledger {
            dir.join("ledger.json")
        } else {
            PathBuf::new()
        };
        cfg.paths.bind_root = dir.join("binds");
        cfg.paths.socket_root = dir.join("socks");
        cfg.hijack_poll = Duration::from_millis(20);
        tweak(&mut cfg);
        let engine_path = dir.join("engine.sock");
        let engine = Engine::start(&engine_path, custom);
        let svc = Services::new(cfg, &engine_path).unwrap();
        let auth = Authority {
            project_root: dir.clone(),
            workspace: "lane-a".into(),
            attempt: "att1".into(),
        };
        Self {
            dir,
            svc,
            engine,
            auth,
        }
    }

    /// Serve `auth` on its own socket; returns the client path.
    fn serve(&self, auth: &Authority) -> PathBuf {
        let p = self
            .dir
            .join(format!("c-{}-{}.sock", auth.workspace, auth.attempt));
        let _ = std::fs::remove_file(&p);
        let l = UnixListener::bind(&p).unwrap();
        let (svc, a) = (self.svc.clone(), auth.clone());
        std::thread::spawn(move || {
            let _ = svc.serve_listener(&l, &a);
        });
        p
    }

    fn own_container(&self, id: &str, pins: &[&str]) {
        self.svc
            .ledger
            .add_container(
                id,
                ContainerRec {
                    scope: self.auth.workspace.clone(),
                    attempt: self.auth.attempt.clone(),
                    name: None,
                    pins: pins.iter().map(|s| (*s).to_string()).collect(),
                    created: now(),
                },
            )
            .unwrap();
    }
}

impl Drop for H {
    fn drop(&mut self) {
        self.engine.release.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn call(sock: &Path, method: &str, target: &str, body: Option<&str>) -> (u16, String) {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let b = body.unwrap_or("");
    let ct = if body.is_some() {
        "Content-Type: application/json\r\n"
    } else {
        ""
    };
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: d\r\n{ct}Content-Length: {}\r\nConnection: close\r\n\r\n{b}",
        b.len()
    )
    .unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let status = text
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = text.split_once("\r\n\r\n").map_or("", |x| x.1).to_string();
    (status, body)
}

fn wait_for(limit: Duration, mut f: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

const C1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const CREATE: &str = r#"{"Image":"busybox"}"#;

const SWITCH: &[u8] = b"HTTP/1.1 101 UPGRADED\r\nContent-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n";

/// Attach to an owned container; the engine answers `101` and then runs
/// `after`. Returns the open client stream, positioned after the head.
fn attach(h: &H, sock: &Path) -> UnixStream {
    h.own_container(C1, &[]);
    let mut c = UnixStream::connect(sock).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        c,
        "POST /containers/{C1}/attach?stream=1&stdin=1&stdout=1 HTTP/1.1\r\nHost: d\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: 0\r\n\r\n"
    )
    .unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(c.read(&mut b).unwrap(), 1, "no switch head: {head:?}");
        head.push(b[0]);
    }
    assert!(
        head.starts_with(b"HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&head)
    );
    c
}

fn engine_attach(
    after: impl Fn(&mut UnixStream, &AtomicBool) + Send + Sync + 'static,
    release: Arc<AtomicBool>,
) -> Handler {
    Arc::new(move |req, s| {
        if req.target.contains("/attach") {
            s.write_all(SWITCH).unwrap();
            after(s, &release);
            return true;
        }
        false
    })
}

// --- hijack deadlines and disconnect cleanup (Sol: streams) ------------------

/// A hijacked stream that carries nothing either way is closed at the idle
/// limit. (`hijack-deadlines-off`: it stays open for ever.)
///
/// The read is bounded by the configured limit plus a generous slack, and the
/// fake engine keeps its side open for longer than that: only the endpoint's
/// idle deadline can end the read with a close, and without it the read times
/// out and the test fails.
#[test]
fn a_silent_hijacked_stream_is_closed_at_the_idle_limit() {
    const IDLE: Duration = Duration::from_millis(400);
    const SLACK: Duration = Duration::from_secs(10);
    let rel = Arc::new(AtomicBool::new(false));
    let r2 = rel.clone();
    let h = H::new(
        "silent",
        false,
        engine_attach(
            |s, rel| {
                let end = Instant::now() + IDLE + SLACK + Duration::from_secs(30);
                while !rel.load(Ordering::SeqCst) && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = s.shutdown(std::net::Shutdown::Both);
            },
            rel,
        ),
        |c| c.hijack_idle = IDLE,
    );
    let sock = h.serve(&h.auth);
    let mut c = attach(&h, &sock);
    c.set_read_timeout(Some(IDLE + SLACK)).unwrap();
    let mut b = [0u8; 16];
    let r = c.read(&mut b);
    r2.store(true, Ordering::SeqCst);
    assert!(
        matches!(r, Ok(0)),
        "the idle stream was not closed within {IDLE:?} + {SLACK:?}: {r:?}"
    );
    assert!(wait_for(SLACK, || h.svc.open_connections() == 0));
}

/// A client that vanishes frees its slot although the engine side is silent.
/// Linux only: it relies on POLLHUP meaning both directions are gone, which BSD
/// poll does not distinguish from a half-close.
#[cfg(target_os = "linux")]
#[test]
fn a_vanished_client_frees_the_slot() {
    let rel = Arc::new(AtomicBool::new(false));
    let r2 = rel.clone();
    let h = H::new(
        "vanish",
        false,
        engine_attach(
            |s, rel| {
                let end = Instant::now() + Duration::from_secs(20);
                while !rel.load(Ordering::SeqCst) && Instant::now() < end {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = s.shutdown(std::net::Shutdown::Both);
            },
            rel,
        ),
        |c| c.hijack_idle = Duration::from_secs(600),
    );
    let sock = h.serve(&h.auth);
    let c = attach(&h, &sock);
    assert_eq!(h.svc.open_connections(), 1);
    drop(c);
    let freed = wait_for(Duration::from_secs(4), || h.svc.open_connections() == 0);
    r2.store(true, Ordering::SeqCst);
    assert!(freed, "the slot of a vanished client was never released");
}

/// A client that stops reading is cut after the write timeout instead of
/// holding the pump in a blocked write.
#[test]
fn a_client_that_stops_reading_is_cut() {
    let rel = Arc::new(AtomicBool::new(false));
    let r2 = rel.clone();
    let h = H::new(
        "blocked",
        false,
        engine_attach(
            |s, rel| {
                let chunk = vec![b'x'; 256 * 1024];
                while !rel.load(Ordering::SeqCst) {
                    if s.write_all(&chunk).is_err() {
                        return;
                    }
                }
            },
            rel,
        ),
        |c| {
            c.hijack_idle = Duration::from_secs(600);
            c.http.progress = Duration::from_millis(400);
        },
    );
    let sock = h.serve(&h.auth);
    let c = attach(&h, &sock);
    // Never read again.
    let freed = wait_for(Duration::from_secs(6), || h.svc.open_connections() == 0);
    r2.store(true, Ordering::SeqCst);
    drop(c);
    assert!(freed, "a stalled reader held the stream");
}

/// Control: half-closing the client's side does not cut the engine's output
/// (`docker exec -i < file`).
#[test]
fn a_half_closed_client_still_gets_the_engine_output() {
    let rel = Arc::new(AtomicBool::new(false));
    let h = H::new(
        "halfclose",
        false,
        engine_attach(
            |s, _| {
                let mut all = Vec::new();
                let _ = s.read_to_end(&mut all);
                std::thread::sleep(Duration::from_millis(300));
                let _ = s.write_all(b"after-eof:");
                let _ = s.write_all(&all);
                let _ = s.shutdown(std::net::Shutdown::Both);
            },
            rel,
        ),
        |c| c.hijack_idle = Duration::from_secs(30),
    );
    let sock = h.serve(&h.auth);
    let mut c = attach(&h, &sock);
    c.write_all(b"input").unwrap();
    c.shutdown(std::net::Shutdown::Write).unwrap();
    let mut got = Vec::new();
    c.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"after-eof:input");
}

// --- connection caps ----------------------------------------------------------

/// The cap is per project, not per workspace root: connections from other
/// roots of the same engine count against it. A second project is separate.
#[test]
fn the_project_cap_counts_every_workspace_root() {
    let h = H::new("cap", false, no_custom(), |c| {
        c.max_conns_project = 2;
        c.max_conns_attempt = 64;
    });
    let auth = |ws: &str| Authority {
        project_root: scratch(&format!("cap-root-{ws}")),
        workspace: ws.into(),
        attempt: "att1".into(),
    };
    let socks: Vec<PathBuf> = ["w1", "w2", "w3"]
        .iter()
        .map(|w| h.serve(&auth(w)))
        .collect();
    let _c1 = UnixStream::connect(&socks[0]).unwrap();
    let _c2 = UnixStream::connect(&socks[1]).unwrap();
    assert!(wait_for(Duration::from_secs(2), || h
        .svc
        .open_connections()
        == 2));
    let mut c3 = UnixStream::connect(&socks[2]).unwrap();
    c3.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut b = [0u8; 12];
    let n = c3.read(&mut b).unwrap_or(0);
    assert_eq!(
        &b[..n],
        b"HTTP/1.1 429",
        "a third root was admitted past the project cap"
    );

    // Control: another project (its own Services) is not held back by it.
    let other = H::new("cap-other", false, no_custom(), |c| c.max_conns_project = 2);
    let s = other.serve(&auth("w9"));
    let mut c4 = UnixStream::connect(&s).unwrap();
    c4.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let r = c4.read(&mut b);
    assert!(
        matches!(&r, Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)),
        "a separate project was refused: {r:?}"
    );
}

// --- ledger persistence at the endpoint ---------------------------------------

/// A ledger that cannot be written refuses the create and takes the engine's
/// container back; admission resumes when writes work again.
#[test]
fn a_ledger_write_failure_refuses_and_leaves_no_engine_object() {
    let h = H::new("ledgerfail", true, no_custom(), |_| {});
    let sock = h.serve(&h.auth);
    let (st, body) = call(&sock, "POST", "/containers/create", Some(CREATE));
    assert_eq!(st, 201, "baseline create: {body}");
    assert_eq!(h.engine.created.load(Ordering::SeqCst), 1);

    let tmp = h.dir.join("ledger.lock");
    std::fs::remove_file(&tmp).unwrap();
    std::fs::create_dir(&tmp).unwrap();
    let (st, body) = call(&sock, "POST", "/containers/create", Some(CREATE));
    assert_eq!(st, 503, "an unsaved create was acknowledged: {body}");
    assert_eq!(
        h.engine.saw("DELETE", "/containers/"),
        1,
        "the engine's container was left behind"
    );
    // Degraded: nothing new is admitted, and the engine is not asked.
    let before = h.engine.created.load(Ordering::SeqCst);
    let (st, _) = call(&sock, "POST", "/containers/create", Some(CREATE));
    assert_eq!(st, 503);
    assert_eq!(h.engine.created.load(Ordering::SeqCst), before);

    std::fs::remove_dir(&tmp).unwrap();
    let (st, body) = call(&sock, "POST", "/containers/create", Some(CREATE));
    assert_eq!(st, 201, "admission did not resume: {body}");
}

/// A second agent serving the same ledger (a replacement that started before
/// the old relay exited) is refused, so it cannot answer from a stale view and
/// let a request delete a volume the ledger has since given to another
/// workspace.
#[test]
fn a_second_serving_agent_on_one_ledger_is_refused() {
    let h = H::new("secondagent", true, no_custom(), |_| {});
    let mut cfg = Config::default();
    cfg.paths.ledger = h.dir.join("ledger.json");
    cfg.ledger_serve_wait = Duration::from_millis(100);
    let error = Services::new(cfg.clone(), &h.dir.join("engine.sock"))
        .err()
        .expect("a second server must be refused");
    assert!(error.to_string().contains("another agent"), "{error}");
    drop(h);
    assert!(Services::new(cfg, &scratch("secondagent-engine").join("e.sock")).is_ok());
}

/// A corrupt ledger forgets which paths are protected: binds of a project
/// folder are refused (nothing reaches the engine) until an operator clears
/// the note; other requests still work.
#[cfg(target_os = "linux")]
#[test]
fn a_corrupt_ledger_refuses_project_binds_but_not_other_creates() {
    let h = H::new("quarantine", true, no_custom(), |_| {});
    std::fs::create_dir_all(h.dir.join("data")).unwrap();
    std::fs::write(
        h.dir.join("ledger.quarantine"),
        "corrupt ledger kept as x\n",
    )
    .unwrap();
    let sock = h.serve(&h.auth);
    let body = format!(
        r#"{{"Image":"busybox","HostConfig":{{"Binds":["{}:/data"]}}}}"#,
        h.dir.join("data").display()
    );
    let (status, reply) = call(&sock, "POST", "/containers/create", Some(&body));
    assert_eq!(status, 403, "{reply}");
    assert!(
        reply.contains(&h.dir.join("ledger.quarantine").display().to_string()),
        "the full path of the note: {reply}"
    );
    assert_eq!(h.engine.created.load(Ordering::SeqCst), 0);
    let (status, reply) = call(&sock, "POST", "/containers/create", Some(CREATE));
    assert_eq!(status, 201, "{reply}");
}

/// An engine whose volumes are a name -> body map; the first `DELETE` waits
/// for `gate` before removing the name (a request that is already at the
/// engine when its caller is gone, or has given up).
struct VolumeEngine {
    store: Arc<Mutex<std::collections::HashMap<String, String>>>,
    deletes: Arc<AtomicUsize>,
    first_done: Arc<AtomicBool>,
    gate: Arc<AtomicBool>,
    /// 0: delete normally; otherwise answer every DELETE with this status.
    delete_status: Arc<std::sync::atomic::AtomicU16>,
    /// Hold every DELETE (not only the first) until `gate`.
    hold_all_deletes: Arc<AtomicBool>,
    /// 0: create normally; otherwise answer every volume CREATE with this status.
    create_status: Arc<std::sync::atomic::AtomicU16>,
    /// Hold the next volume CREATE until `create_release`; `held_create_done`
    /// is set when it has been answered.
    hold_next_create: Arc<AtomicBool>,
    create_release: Arc<AtomicBool>,
    held_create_done: Arc<AtomicBool>,
}

impl VolumeEngine {
    fn new() -> Self {
        Self {
            store: Arc::default(),
            deletes: Arc::default(),
            first_done: Arc::default(),
            gate: Arc::default(),
            delete_status: Arc::default(),
            create_status: Arc::default(),
            hold_all_deletes: Arc::default(),
            hold_next_create: Arc::default(),
            create_release: Arc::default(),
            held_create_done: Arc::default(),
        }
    }

    fn handler(&self) -> Handler {
        let (store, deletes, gate, first_done, delete_status) = (
            self.store.clone(),
            self.deletes.clone(),
            self.gate.clone(),
            self.first_done.clone(),
            self.delete_status.clone(),
        );
        let create_status = self.create_status.clone();
        let (hold_all, hold_create, release, create_done) = (
            self.hold_all_deletes.clone(),
            self.hold_next_create.clone(),
            self.create_release.clone(),
            self.held_create_done.clone(),
        );
        // One removal of a name at a time, in no particular order.
        let by_name: Arc<Mutex<()>> = Arc::default();
        Arc::new(move |req, s| {
            let name = req.target.trim_start_matches("/volumes/").to_owned();
            match (req.method.as_str(), req.target.as_str()) {
                ("GET", "/containers/json?all=1" | "/networks") => {
                    answer(s, 200, "[]");
                    true
                }
                ("GET", "/volumes") => {
                    let bodies = store.lock().unwrap().values().cloned().collect::<Vec<_>>();
                    answer(s, 200, &format!("{{\"Volumes\":[{}]}}", bodies.join(",")));
                    true
                }
                ("POST", "/volumes/create")
                    if String::from_utf8_lossy(&req.body).contains("\"slowbody\"") =>
                {
                    // A valid head, then the body only much later.
                    let _ = s.write_all(
                        b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n",
                    );
                    std::thread::sleep(Duration::from_millis(1500));
                    let _ = s.write_all(b"{}");
                    true
                }
                ("POST", "/volumes/create")
                    if String::from_utf8_lossy(&req.body).contains("\"bigreply\"") =>
                {
                    answer(s, 201, &"x".repeat(4 << 20));
                    true
                }
                ("POST", "/volumes/create") if create_status.load(Ordering::SeqCst) != 0 => {
                    answer(
                        s,
                        create_status.load(Ordering::SeqCst),
                        "{\"message\":\"refused\"}",
                    );
                    true
                }
                ("POST", "/volumes/create")
                    if !String::from_utf8_lossy(&req.body).contains("\"Name\":\"") =>
                {
                    // A nameless create: the engine picks the name.
                    let body = "{\"Name\":\"anon-1\",\"Labels\":{}}".to_owned();
                    store.lock().unwrap().insert("anon-1".into(), body.clone());
                    answer(s, 201, &body);
                    true
                }
                ("POST", "/volumes/create") => {
                    let held = hold_create.swap(false, Ordering::SeqCst);
                    if held {
                        while !release.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    let body = String::from_utf8_lossy(&req.body).into_owned();
                    let named = body
                        .split("\"Name\":\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or("")
                        .to_owned();
                    store.lock().unwrap().insert(named, body.clone());
                    answer(s, 201, &body);
                    if held {
                        create_done.store(true, Ordering::SeqCst);
                    }
                    true
                }
                ("GET", t) if t.starts_with("/volumes/") => {
                    match store.lock().unwrap().get(&name).cloned() {
                        Some(body) => answer(s, 200, &body),
                        None => answer(s, 404, "{\"message\":\"no such volume\"}"),
                    }
                    true
                }
                ("DELETE", t) if t.starts_with("/volumes/") => {
                    if name == "bigdel" {
                        // An error answer with a large body (the owner keeps the volume).
                        answer(s, 409, &"x".repeat(4 << 20));
                        return true;
                    }
                    if name == "drip" {
                        // A reply that trickles in, a byte at a time.
                        let _ = s.write_all(b"HTTP/1.1 500 X\r\nContent-Length: 1000\r\n\r\n");
                        for _ in 0..1000 {
                            if s.write_all(b"x").is_err() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        return true;
                    }
                    // Accepted first, executed whenever it gets the name's lock:
                    // a request can be delayed before the lock while a later one
                    // runs (arrival order is not execution order).
                    let first = deletes.fetch_add(1, Ordering::SeqCst) == 0;
                    if first || hold_all.load(Ordering::SeqCst) {
                        while !gate.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    let _one_at_a_time = by_name.lock().unwrap();
                    let status = delete_status.load(Ordering::SeqCst);
                    if status != 0 {
                        answer(s, status, "{\"message\":\"refused\"}");
                        return true;
                    }
                    let found = store.lock().unwrap().remove(&name).is_some();
                    if found {
                        let _ = s.write_all(
                            b"HTTP/1.1 204 No Content\r\nApi-Version: 1.45\r\nConnection: close\r\n\r\n",
                        );
                    } else {
                        answer(s, 404, "{\"message\":\"no such volume\"}");
                    }
                    if first {
                        first_done.store(true, Ordering::SeqCst);
                    }
                    true
                }
                _ => false,
            }
        })
    }
}

fn lane_b(h: &H) -> Authority {
    Authority {
        project_root: h.dir.clone(),
        workspace: "lane-b".into(),
        attempt: "att2".into(),
    }
}

/// The caller of a DELETE that is already at the engine goes away. The name
/// stays reserved until the engine has finished: a second delete and lane B's
/// create wait, so the pending one cannot remove lane B's replacement.
#[test]
fn a_delete_whose_client_disconnected_still_holds_the_name_until_the_engine_finishes() {
    let engine = VolumeEngine::new();
    let h = H::new("deletegone", false, engine.handler(), |_| {});
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    let (status, body) = call(
        &sock_a,
        "POST",
        "/volumes/create",
        Some(r#"{"Name":"data"}"#),
    );
    assert_eq!(status, 201, "{body}");

    {
        let mut c = UnixStream::connect(&sock_a).unwrap();
        c.write_all(b"DELETE /volumes/data HTTP/1.1\r\nHost: d\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        assert!(wait_for(Duration::from_secs(5), || engine
            .deletes
            .load(Ordering::SeqCst)
            == 1));
    } // the client is gone
    let second = {
        let sock = sock_a.clone();
        std::thread::spawn(move || call(&sock, "DELETE", "/volumes/data", None).0)
    };
    let create_b = {
        let sock = sock_b.clone();
        std::thread::spawn(move || {
            call(&sock, "POST", "/volumes/create", Some(r#"{"Name":"data"}"#)).0
        })
    };
    // Long enough for the endpoint to notice the vanished client.
    std::thread::sleep(Duration::from_millis(3500));
    assert!(
        !second.is_finished(),
        "the name was released with the client"
    );
    assert!(
        !create_b.is_finished(),
        "another workspace claimed the name"
    );
    engine.gate.store(true, Ordering::SeqCst);
    second.join().unwrap();
    assert_eq!(create_b.join().unwrap(), 201);
    assert!(engine.first_done.load(Ordering::SeqCst));
    assert!(
        engine.store.lock().unwrap().contains_key("data"),
        "lane B's volume was deleted"
    );
    assert!(h.svc.ledger.has_volume("lane-b", "data"));
}

/// A lane A volume whose first DELETE the engine does not answer in time (it
/// is held at the engine until `gate`), on a Services with short deadlines.
const SHORT_FENCE: Duration = Duration::from_millis(2500);

fn timed_out_delete(name: &str) -> (VolumeEngine, H, PathBuf, PathBuf) {
    let engine = VolumeEngine::new();
    let h = H::new(name, false, engine.handler(), |cfg| {
        cfg.up_progress = Duration::from_millis(250);
        cfg.delete_fence = SHORT_FENCE;
    });
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    let (status, body) = call(
        &sock_a,
        "POST",
        "/volumes/create",
        Some(r#"{"Name":"data"}"#),
    );
    assert_eq!(status, 201, "{body}");
    let (status, _) = call(&sock_a, "DELETE", "/volumes/data", None);
    assert_eq!(status, 502, "the first delete timed out");
    (engine, h, sock_a, sock_b)
}

fn release_first_delete(engine: &VolumeEngine) {
    engine.gate.store(true, Ordering::SeqCst);
    assert!(wait_for(Duration::from_secs(5), || engine
        .first_done
        .load(Ordering::SeqCst)));
}

const DATA: Option<&str> = Some(r#"{"Name":"data"}"#);

/// The engine does not answer a DELETE in time: its outcome is unknown, so the
/// name is fenced against OTHER workspaces until the fence expires, whatever
/// else happens meanwhile: the owner's retry succeeding (the first request may
/// still be queued at the engine and run later), the engine showing the name
/// absent, or the reconciler dropping the ledger entry. Lane B then creates
/// its volume only after the fence expired, and keeps it.
#[test]
fn another_workspace_waits_out_the_fence_of_a_delete_that_was_not_answered() {
    let (engine, h, sock_a, sock_b) = timed_out_delete("dt1");
    // The retry runs ahead of the first request and removes the volume.
    assert_eq!(call(&sock_a, "DELETE", "/volumes/data", None).0, 204);
    assert!(!h.svc.ledger.has_volume("lane-a", "data"));
    assert!(!engine.store.lock().unwrap().contains_key("data"));
    // Neither that answer, nor an absent inspect, nor the reconciler ends it.
    h.svc.reconcile();
    assert_eq!(call(&sock_b, "POST", "/volumes/create", DATA).0, 503);
    assert!(h.svc.volume_fenced("data"));
    // The first request finally runs (and finds nothing to remove) ...
    release_first_delete(&engine);
    assert_eq!(call(&sock_b, "POST", "/volumes/create", DATA).0, 503);
    // ... and once the fence has expired lane B's volume is created and stays.
    std::thread::sleep(SHORT_FENCE + Duration::from_millis(300));
    let (status, body) = call(&sock_b, "POST", "/volumes/create", DATA);
    assert_eq!(status, 201, "{body}");
    assert!(engine.store.lock().unwrap().contains_key("data"));
    assert!(h.svc.ledger.has_volume("lane-b", "data"));
    assert!(!h.svc.volume_fenced("data"));
}

/// The fence protects other workspaces from the late landing of a delete the
/// owner sent; the owner is exempt (a late landing can only remove its own
/// volume): after a retry released the name, it re-creates it at once, by
/// create and by a container with a named volume.
#[test]
fn the_workspace_that_sent_the_delete_is_not_refused_by_its_own_fence() {
    let (engine, h, sock_a, _) = timed_out_delete("dt2");
    assert_eq!(call(&sock_a, "DELETE", "/volumes/data", None).0, 204);
    let (status, body) = call(&sock_a, "POST", "/volumes/create", DATA);
    assert_eq!(status, 201, "{body}");
    assert!(h.svc.volume_fenced("data"));
    let create = r#"{"Image":"busybox","HostConfig":{"Binds":["data:/x"]}}"#;
    let (status, body) = call(&sock_a, "POST", "/containers/create", Some(create));
    assert_eq!(status, 201, "{body}");
    release_first_delete(&engine);
}

/// `docker run -v data:/x` creates the named volume through another path
/// (`ensure_volume`): it honors the fence against other workspaces too.
#[test]
fn a_container_create_by_another_workspace_honors_the_fence() {
    let (engine, h, sock_a, sock_b) = timed_out_delete("dt4");
    assert_eq!(call(&sock_a, "DELETE", "/volumes/data", None).0, 204);
    let create = r#"{"Image":"busybox","HostConfig":{"Binds":["data:/x"]}}"#;
    let (status, reply) = call(&sock_b, "POST", "/containers/create", Some(create));
    assert_eq!(status, 503, "{reply}");
    assert_eq!(h.engine.created.load(Ordering::SeqCst), 0);
    release_first_delete(&engine);
    std::thread::sleep(SHORT_FENCE + Duration::from_millis(300));
    let (status, reply) = call(&sock_b, "POST", "/containers/create", Some(create));
    assert_eq!(status, 201, "{reply}");
}

/// The first client disconnects, the engine does not answer within the
/// deadline (the delete is fenced), the owner retries and succeeds, and lane B
/// is still refused until expiry: its replacement survives the first request.
#[test]
fn a_disconnected_delete_that_timed_out_fences_the_name_too() {
    let engine = VolumeEngine::new();
    let h = H::new("dt5", false, engine.handler(), |cfg| {
        cfg.up_progress = Duration::from_millis(250);
        cfg.delete_fence = SHORT_FENCE;
    });
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    assert_eq!(call(&sock_a, "POST", "/volumes/create", DATA).0, 201);
    {
        let mut c = UnixStream::connect(&sock_a).unwrap();
        c.write_all(b"DELETE /volumes/data HTTP/1.1\r\nHost: d\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
    }
    assert!(wait_for(Duration::from_secs(5), || h
        .svc
        .volume_fenced("data")));
    assert_eq!(call(&sock_a, "DELETE", "/volumes/data", None).0, 204);
    assert_eq!(call(&sock_b, "POST", "/volumes/create", DATA).0, 503);
    release_first_delete(&engine);
    std::thread::sleep(SHORT_FENCE + Duration::from_millis(300));
    assert_eq!(call(&sock_b, "POST", "/volumes/create", DATA).0, 201);
    assert!(engine.store.lock().unwrap().contains_key("data"));
}

/// A delete that never reached the engine (it could not be dialed) leaves
/// nothing pending, so nothing is fenced.
#[test]
fn a_delete_that_was_never_sent_fences_nothing() {
    let engine = VolumeEngine::new();
    let h = H::new("deletenotsent", false, engine.handler(), |_| {});
    let sock = h.serve(&h.auth);
    assert_eq!(call(&sock, "POST", "/volumes/create", DATA).0, 201);
    std::fs::remove_file(h.dir.join("engine.sock")).unwrap();
    assert_eq!(call(&sock, "DELETE", "/volumes/data", None).0, 502);
    assert!(!h.svc.volume_fenced("data"));
    assert!(h.svc.ledger.has_volume("lane-a", "data"));
}

/// An engine that answers with an error (the volume is in use, or it failed)
/// has answered: lane A keeps the volume and the reply is the engine's.
#[test]
fn an_engine_error_on_delete_keeps_the_owner_and_is_passed_on() {
    for status in [409_u16, 500] {
        let engine = VolumeEngine::new();
        engine.gate.store(true, Ordering::SeqCst);
        let h = H::new("deleteerror", false, engine.handler(), |_| {});
        let sock = h.serve(&h.auth);
        assert_eq!(call(&sock, "POST", "/volumes/create", DATA).0, 201);
        engine.delete_status.store(status, Ordering::SeqCst);
        let (got, body) = call(&sock, "DELETE", "/volumes/data", None);
        assert_eq!(got, status);
        assert!(body.contains("refused"), "{body}");
        assert!(h.svc.ledger.has_volume("lane-a", "data"), "{status}");
        assert!(!h.svc.volume_fenced("data"), "{status}");
        assert!(engine.store.lock().unwrap().contains_key("data"));
    }
}

/// A create that was admitted before a delete (and so before any fence) must
/// not publish its ownership after that delete and a later fence: volume
/// creates and deletes of a name run one at a time from admission to the end
/// of the engine call, so the delete waits for the create instead of
/// overtaking it.
#[test]
fn a_volume_create_holds_the_name_from_admission_to_the_end_of_its_engine_call() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("createhold", false, engine.handler(), |_| {});
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    assert_eq!(call(&sock_b, "POST", "/volumes/create", DATA).0, 201);
    engine.hold_next_create.store(true, Ordering::SeqCst);
    let second_create = {
        let sock = sock_b.clone();
        std::thread::spawn(move || call(&sock, "POST", "/volumes/create", DATA).0)
    };
    // Admitted and at the engine.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !engine.hold_next_create.load(Ordering::SeqCst),
        "the create never reached the engine"
    );
    let delete_b = {
        let sock = sock_b.clone();
        std::thread::spawn(move || call(&sock, "DELETE", "/volumes/data", None).0)
    };
    let create_a = {
        let sock = sock_a.clone();
        std::thread::spawn(move || call(&sock, "POST", "/volumes/create", DATA).0)
    };
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !delete_b.is_finished(),
        "a delete overtook a create at the engine"
    );
    assert!(
        !create_a.is_finished(),
        "another workspace claimed the name meanwhile"
    );
    engine.create_release.store(true, Ordering::SeqCst);
    assert_eq!(second_create.join().unwrap(), 201);
    assert!(engine.held_create_done.load(Ordering::SeqCst));
    assert_eq!(delete_b.join().unwrap(), 204);
    // Whichever of the two waiting requests ran first decides lane A's answer:
    // refused while lane B still owned the name, or created after B's delete.
    let a_status = create_a.join().unwrap();
    assert!(matches!(a_status, 201 | 403), "{a_status}");
    assert_eq!(h.svc.ledger.has_volume("lane-a", "data"), a_status == 201);
    assert!(
        !h.svc.ledger.has_volume("lane-b", "data"),
        "lane B deleted it"
    );
}

/// A retry that the engine answers 404 (the first request finished late)
/// releases the name like any answered delete.
#[test]
fn a_delete_retry_answered_404_releases_the_name() {
    let (engine, h, sock_a, _) = timed_out_delete("dt6");
    release_first_delete(&engine);
    assert_eq!(call(&sock_a, "DELETE", "/volumes/data", None).0, 404);
    assert!(!h.svc.ledger.has_volume("lane-a", "data"));
}

/// Each unanswered delete refreshes the fence: a second failure well after the
/// first extends it past the first one's expiry.
#[test]
fn a_repeated_unanswered_delete_refreshes_the_fence_time() {
    let (engine, h, sock_a, sock_b) = timed_out_delete("dt7");
    engine.hold_all_deletes.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(1700));
    assert_eq!(call(&sock_a, "DELETE", "/volumes/data", None).0, 502);
    // Past the first failure's expiry (2.5 s), inside the second's.
    std::thread::sleep(Duration::from_millis(900));
    assert!(h.svc.volume_fenced("data"));
    assert_eq!(call(&sock_b, "POST", "/volumes/create", DATA).0, 503);
    engine.gate.store(true, Ordering::SeqCst);
}

/// The terminal chunk arriving a byte at a time (each piece inside the
/// per-read limit) is cut off by the call's absolute deadline, in time.
#[test]
fn a_drip_fed_terminal_chunk_obeys_the_whole_call_deadline() {
    let h = H::new(
        "dripterminalw4",
        false,
        Arc::new(|req, s| {
            if req.method != "DELETE" {
                return false;
            }
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
            for byte in b"0\r\n\r\n" {
                std::thread::sleep(Duration::from_millis(120));
                if s.write_all(&[*byte]).is_err() {
                    return true;
                }
            }
            true
        }),
        |cfg| cfg.up_progress = Duration::from_millis(200),
    );
    let sock = h.serve(&h.auth);
    h.svc.ledger.claim_volume("lane-a", "att1", "data").unwrap();
    let started = Instant::now();
    let (status, _) = call(&sock, "DELETE", "/volumes/data", None);
    assert_eq!(
        status,
        502,
        "accepted a reply after {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_millis(450),
        "{:?}",
        started.elapsed()
    );
    assert!(h.svc.volume_fenced("data"));
}

/// A request that the engine reads slowly (partial writes, each inside the
/// write timeout) is cut off by the same deadline.
#[test]
fn a_slowly_read_request_obeys_the_whole_call_deadline() {
    let h = H::new("slowwritew4", false, no_custom(), |cfg| {
        cfg.up_progress = Duration::from_millis(200);
    });
    let (client, mut server) = UnixStream::pair().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0_u8; 8 * 1024];
        while let Ok(n) = server.read(&mut buf) {
            if n == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    });
    let body = vec![b'x'; 8 * 1024 * 1024];
    let started = Instant::now();
    let result = h
        .svc
        .exchange(client, "POST", "/volumes/create", Some(&body));
    assert!(result.is_err());
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "{:?}",
        started.elapsed()
    );
    drop(reader);
}

/// A gRPC client (Compose 5.8's jobs probe dials the Docker socket and speaks
/// HTTP/2 with prior knowledge) gets a prompt `400` and a closed connection:
/// nothing reaches the engine, and the client fails at once instead of waiting
/// for HTTP/2 frames. (`filter-off`: the bytes are relayed to the engine and
/// the client gets no `400`.)
#[test]
fn an_http2_preface_gets_a_fast_400_and_the_connection_is_closed() {
    const LIMIT: Duration = Duration::from_secs(5);
    let h = H::new("h2preface", false, no_custom(), |_| {});
    let sock = h.serve(&h.auth);
    let mut c = UnixStream::connect(&sock).unwrap();
    c.set_read_timeout(Some(LIMIT)).unwrap();
    // The preface and an empty SETTINGS frame, as one write, as gRPC sends them.
    c.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .unwrap();
    let started = Instant::now();
    let mut out = Vec::new();
    let mut b = [0u8; 512];
    let closed = loop {
        match c.read(&mut b) {
            Ok(0) => break true,
            Ok(n) => out.extend_from_slice(&b[..n]),
            // Unread preface bytes left behind make some kernels report the
            // close as a reset after the reply was delivered.
            Err(e) if e.kind() == ErrorKind::ConnectionReset => break true,
            Err(_) => break false,
        }
    };
    let text = String::from_utf8_lossy(&out).into_owned();
    assert!(
        closed,
        "the connection was left open after the preface; got {text:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(text.starts_with("HTTP/1.1 400 "), "{text:?}");
    assert!(text.contains("Connection: close\r\n"), "{text:?}");
    assert!(
        h.engine.log.lock().unwrap().is_empty(),
        "the engine saw the preface: {:?}",
        h.engine.log.lock().unwrap()
    );
}

/// A volume create is made by the agent, not through the client's connection:
/// the client leaving cannot end it early. Lane A's create is held at the
/// engine and A disconnects; lane B's create of the name waits (it is not let
/// in after the old 2 s disconnect poll), and when A's create lands its
/// ownership is recorded, so B is refused and nothing is left without an owner.
#[test]
fn a_volume_create_whose_client_disconnected_still_records_its_owner() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("createdisc", false, engine.handler(), |_| {});
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    engine.hold_next_create.store(true, Ordering::SeqCst);
    {
        let mut c = UnixStream::connect(&sock_a).unwrap();
        let body = r#"{"Name":"data"}"#;
        write!(
            c,
            "POST /volumes/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        assert!(wait_for(Duration::from_secs(5), || !engine
            .hold_next_create
            .load(Ordering::SeqCst)));
    } // A is gone, its create still at the engine
    let create_b = {
        let sock = sock_b.clone();
        std::thread::spawn(move || call(&sock, "POST", "/volumes/create", DATA).0)
    };
    std::thread::sleep(Duration::from_millis(2600));
    assert!(
        !create_b.is_finished(),
        "the name was released with the client"
    );
    assert!(!engine.store.lock().unwrap().contains_key("data"));
    engine.create_release.store(true, Ordering::SeqCst);
    assert_eq!(
        create_b.join().unwrap(),
        403,
        "lane A's create was recorded first"
    );
    assert!(engine.store.lock().unwrap().contains_key("data"));
    assert!(h.svc.ledger.has_volume("lane-a", "data"));
    assert!(!h.svc.ledger.has_volume("lane-b", "data"));
}

/// A create the engine does not answer is bounded by the call deadline like a
/// delete: the lock is not held past it (another name's create goes through),
/// the reservation stays with the sender, and the name is fenced against other
/// workspaces, which the sender itself is exempt from.
#[test]
fn a_hung_volume_create_is_bounded_and_fences_the_name() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("createhung", false, engine.handler(), |cfg| {
        cfg.up_progress = Duration::from_millis(300);
        cfg.delete_fence = SHORT_FENCE;
    });
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    engine.hold_next_create.store(true, Ordering::SeqCst);
    let started = Instant::now();
    let first = {
        let sock = sock_a.clone();
        std::thread::spawn(move || call(&sock, "POST", "/volumes/create", DATA).0)
    };
    std::thread::sleep(Duration::from_millis(100));
    let (status, _) = call(
        &sock_b,
        "POST",
        "/volumes/create",
        Some(r#"{"Name":"other"}"#),
    );
    assert_eq!(status, 201);
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(first.join().unwrap(), 502);
    assert!(
        h.svc.ledger.has_volume("lane-a", "data"),
        "the reservation was given up"
    );
    assert!(h.svc.volume_fenced("data"));
    assert_eq!(call(&sock_b, "POST", "/volumes/create", DATA).0, 503);
    assert_eq!(
        call(&sock_a, "POST", "/volumes/create", DATA).0,
        201,
        "the sender is exempt"
    );
    engine.create_release.store(true, Ordering::SeqCst);
}

/// One reply read whole from a kept-alive connection: the head up to the blank
/// line, then exactly `Content-Length` body bytes. The endpoint writes a head
/// and a body in separate writes, so a single `read` may return only the head.
fn read_reply(c: &mut UnixStream) -> (String, Vec<u8>) {
    let mut head = Vec::new();
    let mut b = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        assert_eq!(
            c.read(&mut b).unwrap(),
            1,
            "closed in a reply head: {head:?}"
        );
        head.push(b[0]);
    }
    let head = String::from_utf8(head).unwrap();
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    let mut body = vec![0_u8; len];
    c.read_exact(&mut body).unwrap();
    (head, body)
}

/// The lock a volume create holds for its whole request is released with it: a
/// later request on the same connection (the same thread) takes it again.
///
/// The first reply is read whole, so nothing of it is left to be mistaken for
/// the second. The wait for the lock can only be observed as silence, so it
/// is bounded in time; but a request that got past the lock shows at once in
/// the engine's log, and the silence is checked to be a timeout, not a close.
/// Slowness can therefore only let a bug go unseen, never fail a correct run.
#[test]
fn a_connection_takes_the_volume_lock_again_after_a_volume_create() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("lockflag", false, engine.handler(), |_| {});
    let sock = h.serve(&h.auth);
    let mut c = UnixStream::connect(&sock).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = r#"{"Name":"data"}"#;
    write!(
        c,
        "POST /volumes/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let (first, _) = read_reply(&mut c);
    assert!(first.starts_with("HTTP/1.1 201"), "{first}");
    assert!(
        !first.to_ascii_lowercase().contains("connection: close"),
        "the connection was not kept: {first}"
    );
    // Another thread holds the volume lock; the next request on this
    // connection (a container with a named volume) must wait for it.
    let held = h.svc.vol_lock.lock().unwrap();
    let create = r#"{"Image":"busybox","HostConfig":{"Binds":["data:/x"]}}"#;
    write!(
        c,
        "POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{create}",
        create.len()
    )
    .unwrap();
    c.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut buf = [0_u8; 1];
    let silent = c.read(&mut buf);
    assert!(
        matches!(&silent, Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)),
        "the request did not wait for the volume lock: {silent:?}"
    );
    assert_eq!(
        h.engine.saw("POST", "/containers/create"),
        0,
        "the request got past the volume lock"
    );
    drop(held);
    c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let (second, _) = read_reply(&mut c);
    assert!(second.starts_with("HTTP/1.1 201"), "{second}");
    assert_eq!(h.engine.saw("POST", "/containers/create"), 1);
}

/// A create whose engine body is slow does not hold every workspace's volume
/// lock past the call deadline: another workspace's create of another name goes
/// through while the first is still waiting for its body.
#[test]
fn a_slow_engine_body_does_not_block_other_workspaces_past_the_deadline() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("slowbodyw4", false, engine.handler(), |cfg| {
        cfg.up_progress = Duration::from_millis(250);
    });
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    let started = Instant::now();
    let slow = {
        let sock = sock_a.clone();
        std::thread::spawn(move || {
            call(
                &sock,
                "POST",
                "/volumes/create",
                Some(r#"{"Name":"slowbody"}"#),
            )
            .0
        })
    };
    std::thread::sleep(Duration::from_millis(80));
    let (status, _) = call(
        &sock_b,
        "POST",
        "/volumes/create",
        Some(r#"{"Name":"other"}"#),
    );
    assert_eq!(status, 201);
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(slow.join().unwrap(), 502);
}

/// A client that never reads its reply does not hold the volume lock: the
/// engine's answer is in memory and recorded, and the lock is released before
/// the reply is written to the client.
#[test]
fn a_client_that_does_not_read_its_reply_does_not_block_other_workspaces() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("noreadw4", false, engine.handler(), |_| {});
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    let mut stuck = UnixStream::connect(&sock_a).unwrap();
    let body = r#"{"Name":"bigreply"}"#;
    write!(
        stuck,
        "POST /volumes/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300)); // the reply is being written
    let started = Instant::now();
    let (status, _) = call(
        &sock_b,
        "POST",
        "/volumes/create",
        Some(r#"{"Name":"other"}"#),
    );
    assert_eq!(status, 201);
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "{:?}",
        started.elapsed()
    );
    assert!(
        h.svc.ledger.has_volume("lane-a", "bigreply"),
        "recorded before the reply"
    );
    drop(stuck);
}

/// A reader that drains a few hundred bytes at a time (progress inside every
/// write) cannot stretch the request write past the call deadline.
#[test]
fn a_trickling_reader_cannot_stretch_the_request_write_past_the_deadline() {
    // (bytes per read, pause between reads): progress inside every write.
    for (chunk, pause) in [(512_usize, 60_u64), (256, 50)] {
        let h = H::new("trickleww4", false, no_custom(), |cfg| {
            cfg.up_progress = Duration::from_millis(150);
        });
        let (client, mut server) = UnixStream::pair().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = vec![0_u8; chunk];
            while let Ok(n) = server.read(&mut buf) {
                if n == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(pause));
            }
        });
        let body = vec![b'x'; 64 * 1024];
        let started = Instant::now();
        let result = h
            .svc
            .exchange(client, "POST", "/volumes/create", Some(&body));
        let took = started.elapsed();
        assert!(result.is_err());
        assert!(
            took < Duration::from_millis(450),
            "{chunk}/{pause}: {took:?}"
        );
        reader.join().unwrap();
    }
}

/// The same for a delete: its answer is in memory and recorded, and the lock is
/// released before a reply that its client does not read is written.
#[test]
fn a_client_that_does_not_read_a_delete_reply_does_not_block_other_workspaces() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("noreaddelw4", false, engine.handler(), |_| {});
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    assert_eq!(
        call(
            &sock_a,
            "POST",
            "/volumes/create",
            Some(r#"{"Name":"bigdel"}"#)
        )
        .0,
        201
    );
    let mut stuck = UnixStream::connect(&sock_a).unwrap();
    stuck
        .write_all(b"DELETE /volumes/bigdel HTTP/1.1\r\nHost: d\r\nContent-Length: 0\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let (status, _) = call(
        &sock_b,
        "POST",
        "/volumes/create",
        Some(r#"{"Name":"other"}"#),
    );
    assert_eq!(status, 201);
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "{:?}",
        started.elapsed()
    );
    drop(stuck);
}

/// The volume create inside a container create (`docker run -v data:/x`) is
/// agent-owned like an explicit one: when the engine does not answer a SENT
/// create, the reservation stays with the sender and the name is fenced, so
/// another workspace cannot adopt (and then delete) the volume that create
/// makes when it lands late.
#[test]
fn an_unanswered_implicit_volume_create_keeps_the_reservation_and_fences_the_name() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("implicitcreatew4", false, engine.handler(), |cfg| {
        cfg.up_progress = Duration::from_millis(250);
        cfg.delete_fence = SHORT_FENCE;
    });
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
    let create = r#"{"Image":"busybox","HostConfig":{"Binds":["data:/x"]}}"#;
    engine.hold_next_create.store(true, Ordering::SeqCst);
    let (status, reply) = call(&sock_a, "POST", "/containers/create", Some(create));
    assert!(status >= 500, "{status} {reply}");
    assert_eq!(h.engine.created.load(Ordering::SeqCst), 0);
    assert!(
        h.svc.ledger.has_volume("lane-a", "data"),
        "the reservation was given up"
    );
    assert!(h.svc.volume_fenced("data"));
    // Another workspace cannot take the name, directly or implicitly.
    assert_eq!(call(&sock_b, "POST", "/volumes/create", DATA).0, 503);
    let (status, _) = call(&sock_b, "POST", "/containers/create", Some(create));
    assert_eq!(status, 503);
    // The held create lands late: the volume has its owner.
    engine.create_release.store(true, Ordering::SeqCst);
    assert!(wait_for(Duration::from_secs(5), || engine
        .held_create_done
        .load(Ordering::SeqCst)));
    assert!(engine.store.lock().unwrap().contains_key("data"));
    assert!(h.svc.ledger.has_volume("lane-a", "data"));
    assert!(!h.svc.ledger.has_volume("lane-b", "data"));
    // The sender is exempt from its own fence.
    let (status, reply) = call(&sock_a, "POST", "/containers/create", Some(create));
    assert_eq!(status, 201, "{reply}");
}

/// A volume create that never left (the engine could not be dialed) gives
/// the reservation back, for both the explicit and the implicit path.
#[test]
fn a_volume_create_that_was_never_sent_gives_the_reservation_back() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("notsentcreatew4", false, engine.handler(), |_| {});
    let sock = h.serve(&h.auth);
    std::fs::remove_file(h.dir.join("engine.sock")).unwrap();
    assert_eq!(call(&sock, "POST", "/volumes/create", DATA).0, 502);
    let create = r#"{"Image":"busybox","HostConfig":{"Binds":["other:/x"]}}"#;
    let (status, _) = call(&sock, "POST", "/containers/create", Some(create));
    assert!(status >= 500);
    assert!(!h.svc.ledger.has_volume("lane-a", "data"));
    assert!(!h.svc.ledger.has_volume("lane-a", "other"));
    assert!(!h.svc.volume_fenced("data"));
    assert!(!h.svc.volume_fenced("other"));
    // Nothing was sent, so nothing is in flight.
    assert!(!h.svc.create_pending("data"));
    assert!(!h.svc.create_pending("other"));
}

/// The implicit create's dial fails after the inspect worked (the engine is
/// gone between the two calls): nothing was sent, so the reservation is given
/// back and nothing is fenced.
#[test]
fn an_implicit_volume_create_that_could_not_be_dialed_gives_the_reservation_back() {
    let socket = std::env::temp_dir()
        .join(format!("e3-ep-{}-implicitdialw4", std::process::id()))
        .join("engine.sock");
    let h = H::new(
        "implicitdialw4",
        false,
        Arc::new(move |req, s| {
            if req.method == "GET" && req.target == "/volumes/other" {
                answer(s, 404, "{\"message\":\"no such volume\"}");
                let _ = std::fs::remove_file(&socket);
                return true;
            }
            false
        }),
        |_| {},
    );
    let sock = h.serve(&h.auth);
    let create = r#"{"Image":"busybox","HostConfig":{"Binds":["other:/x"]}}"#;
    let (status, reply) = call(&sock, "POST", "/containers/create", Some(create));
    assert!(status >= 500, "{status} {reply}");
    assert!(!h.svc.ledger.has_volume("lane-a", "other"));
    assert!(!h.svc.volume_fenced("other"));
}

/// A refused volume request is answered without holding the volume lock: a
/// client that pipelines refused creates and never reads its replies (the
/// server's refusal write blocks) cannot stall another workspace's create.
#[test]
fn a_client_that_never_reads_its_refusals_does_not_block_other_workspaces() {
    // Refused creates (a name another workspace owns) and refused deletes (a
    // name this workspace does not own), pipelined without reading.
    for (round, request) in [
        "POST /volumes/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: 16\r\n\r\n{\"Name\":\"owned\"}",
        "DELETE /volumes/owned HTTP/1.1\r\nHost: d\r\nContent-Length: 0\r\n\r\n",
    ]
    .into_iter()
    .enumerate()
    {
        let engine = VolumeEngine::new();
        engine.gate.store(true, Ordering::SeqCst);
        let h = H::new(&format!("refusalsw4{round}"), false, engine.handler(), |_| {});
        let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
        // Lane B owns `owned`: lane A's requests for it are refused.
        assert_eq!(
            call(&sock_b, "POST", "/volumes/create", Some(r#"{"Name":"owned"}"#)).0,
            201
        );
        let stuck = UnixStream::connect(&sock_a).unwrap();
        let mut writer = stuck.try_clone().unwrap();
        let batch = request.repeat(4000);
        let pipelined = std::thread::spawn(move || {
            let _ = writer.write_all(batch.as_bytes());
        });
        std::thread::sleep(Duration::from_millis(1000));
        let started = Instant::now();
        let (status, _) = call(&sock_b, "POST", "/volumes/create", Some(r#"{"Name":"other"}"#));
        assert_eq!(status, 201, "round {round}");
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "round {round}: {:?}",
            started.elapsed()
        );
        let _ = stuck.shutdown(std::net::Shutdown::Both);
        drop(stuck);
        let _ = pipelined.join();
    }
}

/// An engine error on a volume create leaves the name unowned (the reservation
/// made at admission is given back), and a nameless create is owned by its
/// sender under the name the engine chose.
#[test]
fn record_gives_back_a_refused_create_and_claims_a_nameless_one() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("recordw4", false, engine.handler(), |_| {});
    let sock = h.serve(&h.auth);
    engine.create_status.store(500, Ordering::SeqCst);
    assert_eq!(call(&sock, "POST", "/volumes/create", DATA).0, 500);
    assert!(
        !h.svc.ledger.has_volume("lane-a", "data"),
        "the reservation stayed"
    );
    engine.create_status.store(0, Ordering::SeqCst);
    let (status, body) = call(&sock, "POST", "/volumes/create", Some("{}"));
    assert_eq!(status, 201, "{body}");
    assert!(h.svc.ledger.has_volume("lane-a", "anon-1"), "{body}");
}

/// A reconcile (a prune, or the periodic pass) that runs while a volume create
/// of unknown outcome is still pending must not drop the sender's record of
/// the volume (older than the 30 s grace, absent from the engine's list): when
/// the create lands inside the fence it has its owner. After the fence expired
/// the normal rule applies again.
#[test]
fn reconcile_keeps_the_owner_of_a_create_that_may_still_land() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("reconcilecreatew4", true, engine.handler(), |cfg| {
        cfg.up_progress = Duration::from_millis(250);
        cfg.delete_fence = SHORT_FENCE;
        // An established record (outside reconcile's 30 s grace) for a volume
        // the engine does not have.
        std::fs::write(
            &cfg.paths.ledger,
            r#"{"containers":{},"networks":{},"volumes":{"data":{"scope":"lane-a","attempt":"att1","created":1}},"execs":{},"pins":{},"scope_networks":{}}"#,
        )
        .unwrap();
    });
    assert!(
        !h.svc.ledger.quarantined(),
        "the seeded ledger did not parse"
    );
    assert!(h.svc.ledger.has_volume("lane-a", "data"));
    let sock_a = h.serve(&h.auth);
    let create = r#"{"Image":"busybox","HostConfig":{"Binds":["data:/x"]}}"#;
    engine.hold_next_create.store(true, Ordering::SeqCst);
    let (status, _) = call(&sock_a, "POST", "/containers/create", Some(create));
    assert!(status >= 500, "{status}");
    assert!(h.svc.volume_fenced("data"));
    h.svc.reconcile();
    assert!(
        h.svc.ledger.has_volume("lane-a", "data"),
        "reconcile dropped the owner"
    );
    engine.create_release.store(true, Ordering::SeqCst);
    assert!(wait_for(Duration::from_secs(5), || engine
        .held_create_done
        .load(Ordering::SeqCst)));
    assert!(engine.store.lock().unwrap().contains_key("data"));
    h.svc.reconcile();
    assert!(h.svc.ledger.has_volume("lane-a", "data"));
    // Once the fence has expired, an absent volume's record goes as usual.
    engine.store.lock().unwrap().remove("data");
    std::thread::sleep(SHORT_FENCE + Duration::from_millis(300));
    h.svc.reconcile();
    assert!(!h.svc.ledger.has_volume("lane-a", "data"));
}

const SEEDED_OLD_A: &str = r#"{"containers":{},"networks":{},"volumes":{"data":{"scope":"lane-a","attempt":"att1","created":1}},"execs":{},"pins":{},"scope_networks":{}}"#;

/// The two ways a create of `data` reaches the engine: an explicit create and a
/// container create with a named volume.
fn create_request(sock: &Path, implicit: bool) -> u16 {
    if implicit {
        let body = r#"{"Image":"busybox","HostConfig":{"Binds":["data:/x"]}}"#;
        call(sock, "POST", "/containers/create", Some(body)).0
    } else {
        call(sock, "POST", "/volumes/create", DATA).0
    }
}

/// A reconcile that runs while a create of the name is in flight (before its
/// outcome is known) must not drop the owner: the name is in the in-flight
/// registry from before the first byte is sent, and the reconcile cannot
/// overtake the create.
#[test]
fn a_reconcile_during_a_create_that_then_times_out_keeps_the_owner() {
    for implicit in [false, true] {
        let engine = VolumeEngine::new();
        engine.gate.store(true, Ordering::SeqCst);
        let h = H::new(
            &format!("inflightreconw4{implicit}"),
            true,
            engine.handler(),
            |cfg| {
                cfg.up_progress = Duration::from_millis(400);
                cfg.delete_fence = SHORT_FENCE;
                std::fs::write(&cfg.paths.ledger, SEEDED_OLD_A).unwrap();
            },
        );
        assert!(h.svc.ledger.has_volume("lane-a", "data"));
        let sock = h.serve(&h.auth);
        engine.hold_next_create.store(true, Ordering::SeqCst);
        let create = {
            let sock = sock.clone();
            std::thread::spawn(move || create_request(&sock, implicit))
        };
        std::thread::sleep(Duration::from_millis(150));
        // The create is at the engine: a reconcile now sees no volume listed.
        let svc = h.svc.clone();
        let reconcile = std::thread::spawn(move || svc.reconcile());
        assert!(create.join().unwrap() >= 500);
        reconcile.join().unwrap();
        h.svc.reconcile();
        assert!(
            h.svc.ledger.has_volume("lane-a", "data"),
            "implicit={implicit}"
        );
        engine.create_release.store(true, Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(5), || engine
            .held_create_done
            .load(Ordering::SeqCst)));
        assert!(engine.store.lock().unwrap().contains_key("data"));
        assert!(
            h.svc.ledger.has_volume("lane-a", "data"),
            "implicit={implicit}"
        );
    }
}

/// While a create of the name is of unknown outcome, a delete of it by any
/// workspace, its sender too, is refused (503) instead of being answered 404 and
/// releasing the name before the late create lands.
#[test]
fn a_delete_of_a_name_with_a_create_of_unknown_outcome_is_refused() {
    for implicit in [false, true] {
        let engine = VolumeEngine::new();
        engine.gate.store(true, Ordering::SeqCst);
        let h = H::new(
            &format!("deletependingw4{implicit}"),
            false,
            engine.handler(),
            |cfg| {
                cfg.up_progress = Duration::from_millis(250);
                cfg.delete_fence = SHORT_FENCE;
            },
        );
        let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&lane_b(&h)));
        engine.hold_next_create.store(true, Ordering::SeqCst);
        assert!(create_request(&sock_a, implicit) >= 500);
        assert!(h.svc.ledger.has_volume("lane-a", "data"));
        let (status, body) = call(&sock_a, "DELETE", "/volumes/data", None);
        assert_eq!(status, 503, "implicit={implicit}: {body}");
        // The owner is still the owner, and so is it when the create lands.
        assert!(h.svc.ledger.has_volume("lane-a", "data"));
        engine.create_release.store(true, Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(5), || engine
            .held_create_done
            .load(Ordering::SeqCst)));
        assert!(engine.store.lock().unwrap().contains_key("data"));
        assert!(h.svc.ledger.has_volume("lane-a", "data"));
        // Another workspace's delete is not forwarded either.
        let (status, _) = call(&sock_b, "DELETE", "/volumes/data", None);
        assert_ne!(status, 204);
        assert!(engine.store.lock().unwrap().contains_key("data"));
        // After the fence time the owner can delete.
        std::thread::sleep(SHORT_FENCE + Duration::from_millis(300));
        assert_eq!(call(&sock_a, "DELETE", "/volumes/data", None).0, 204);
    }
}

/// A reconcile whose engine snapshot is taken while a create is in flight must
/// not use that snapshot after the create was answered: the snapshot (the
/// engine's lists) is taken under the volume lock, so a slow create that
/// succeeds is either fully before or fully after it, and the owner of a record
/// older than the grace survives.
#[test]
fn a_reconcile_cannot_use_a_snapshot_older_than_a_create_it_waited_for() {
    for implicit in [false, true] {
        let engine = VolumeEngine::new();
        engine.gate.store(true, Ordering::SeqCst);
        let h = H::new(
            &format!("staleSnapshotw4{implicit}"),
            true,
            engine.handler(),
            |cfg| {
                cfg.up_progress = Duration::from_secs(5);
                std::fs::write(&cfg.paths.ledger, SEEDED_OLD_A).unwrap();
            },
        );
        let sock = h.serve(&h.auth);
        engine.hold_next_create.store(true, Ordering::SeqCst);
        let create = {
            let sock = sock.clone();
            std::thread::spawn(move || create_request(&sock, implicit))
        };
        std::thread::sleep(Duration::from_millis(150));
        // The create is at the engine; a reconcile starts now.
        let svc = h.svc.clone();
        let reconcile = std::thread::spawn(move || svc.reconcile());
        std::thread::sleep(Duration::from_millis(300));
        engine.create_release.store(true, Ordering::SeqCst);
        assert_eq!(create.join().unwrap(), 201, "implicit={implicit}");
        reconcile.join().unwrap();
        assert!(engine.store.lock().unwrap().contains_key("data"));
        assert!(
            h.svc.ledger.has_volume("lane-a", "data"),
            "OWNER LOST (implicit={implicit}): the volume exists, the ledger forgot it"
        );
    }
}

/// A later answered attempt does not complete an earlier unanswered one: after
/// create #1 timed out, a retry that is answered (201, or an error) must not end
/// the unknown state, so the owner's delete is still refused and the late
/// create #1 lands with its owner.
#[test]
fn an_answered_retry_does_not_complete_an_earlier_unanswered_create() {
    for implicit in [false, true] {
        for retry_status in [0_u16, 500] {
            let engine = VolumeEngine::new();
            engine.gate.store(true, Ordering::SeqCst);
            let h = H::new(
                &format!("retryw4{implicit}{retry_status}"),
                false,
                engine.handler(),
                |cfg| {
                    cfg.up_progress = Duration::from_millis(250);
                    cfg.delete_fence = SHORT_FENCE;
                },
            );
            let sock = h.serve(&h.auth);
            engine.hold_next_create.store(true, Ordering::SeqCst);
            assert!(create_request(&sock, implicit) >= 500);
            // The retry is answered.
            engine.create_status.store(retry_status, Ordering::SeqCst);
            let _ = create_request(&sock, implicit);
            engine.create_status.store(0, Ordering::SeqCst);
            let label = format!("implicit={implicit} retry={retry_status}");
            let (status, body) = call(&sock, "DELETE", "/volumes/data", None);
            assert_eq!(status, 503, "{label}: {body}");
            assert!(h.svc.ledger.has_volume("lane-a", "data"), "{label}");
            engine.create_release.store(true, Ordering::SeqCst);
            assert!(wait_for(Duration::from_secs(5), || engine
                .held_create_done
                .load(Ordering::SeqCst)));
            assert!(engine.store.lock().unwrap().contains_key("data"), "{label}");
            assert!(h.svc.ledger.has_volume("lane-a", "data"), "{label}");
        }
    }
}

/// The in-flight registry and the fence end at the same instant (the time of the
/// timeout plus the fence time): there is no gap in which the fence still holds
/// and the owner's delete is answered 404 before the late create lands.
#[test]
fn the_registry_outlasts_the_fence_of_an_unknown_create() {
    for implicit in [false, true] {
        let engine = VolumeEngine::new();
        engine.gate.store(true, Ordering::SeqCst);
        let h = H::new(
            &format!("lifetimew4{implicit}"),
            false,
            engine.handler(),
            |cfg| {
                cfg.up_progress = Duration::from_millis(800);
                cfg.delete_fence = Duration::from_millis(1200);
            },
        );
        let sock = h.serve(&h.auth);
        engine.hold_next_create.store(true, Ordering::SeqCst);
        assert!(create_request(&sock, implicit) >= 500);
        // The fence runs from the timeout; so must the registry.
        std::thread::sleep(Duration::from_millis(500));
        assert!(h.svc.volume_fenced("data"));
        let (status, _) = call(&sock, "DELETE", "/volumes/data", None);
        assert_eq!(
            status, 503,
            "implicit={implicit}: the fence held but the registry did not"
        );
        engine.create_release.store(true, Ordering::SeqCst);
        assert!(wait_for(Duration::from_secs(5), || engine
            .held_create_done
            .load(Ordering::SeqCst)));
        assert!(h.svc.ledger.has_volume("lane-a", "data"));
    }
}

/// One clock: the registry entry and the fence of an unknown create both end at
/// the timeout's time plus the fence time, neither earlier nor later.
#[test]
fn an_unknown_creates_registry_entry_and_fence_end_together() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("oneclockw4", false, engine.handler(), |cfg| {
        cfg.up_progress = Duration::from_millis(250);
        cfg.delete_fence = Duration::from_millis(1200);
    });
    let sock = h.serve(&h.auth);
    engine.hold_next_create.store(true, Ordering::SeqCst);
    assert!(create_request(&sock, false) >= 500);
    assert!(h.svc.volume_fenced("data") && h.svc.create_pending("data"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!h.svc.volume_fenced("data"), "the fence outlived its time");
    assert!(
        !h.svc.create_pending("data"),
        "the registry outlived the fence"
    );
    engine.create_release.store(true, Ordering::SeqCst);
}

fn raw_exchange(sock: &Path, request: &str, keep_reading_for: Duration) -> String {
    let mut c = UnixStream::connect(sock).unwrap();
    c.set_read_timeout(Some(keep_reading_for)).unwrap();
    c.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = c.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

/// The reply to a volume delete carries the engine's status line and headers,
/// no body headers on a 204, and keeps the connection when the client did.
#[test]
fn the_delete_reply_is_the_engines_and_keeps_a_keep_alive_connection() {
    let engine = VolumeEngine::new();
    engine.gate.store(true, Ordering::SeqCst);
    let h = H::new("deletereply", false, engine.handler(), |_| {});
    let sock = h.serve(&h.auth);
    for round in 0..2 {
        assert_eq!(call(&sock, "POST", "/volumes/create", DATA).0, 201);
        if round == 0 {
            // Keep-alive: a second request goes down the same connection.
            let mut c = UnixStream::connect(&sock).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            c.write_all(b"DELETE /volumes/data HTTP/1.1\r\nHost: d\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            let mut buf = vec![0_u8; 4096];
            let n = c.read(&mut buf).unwrap();
            let reply = String::from_utf8_lossy(&buf[..n]).into_owned();
            assert!(reply.starts_with("HTTP/1.1 204 No Content\r\n"), "{reply}");
            assert!(reply.contains("Api-Version: 1.45"), "{reply}");
            assert!(!reply.contains("Content-Length"), "{reply}");
            assert!(!reply.contains("Connection: close"), "{reply}");
            c.write_all(b"GET /volumes/data HTTP/1.1\r\nHost: d\r\nConnection: close\r\n\r\n")
                .unwrap();
            let mut rest = Vec::new();
            let _ = c.read_to_end(&mut rest);
            assert!(
                String::from_utf8_lossy(&rest).contains("404"),
                "the connection was closed"
            );
        } else {
            let reply = raw_exchange(
                &sock,
                "DELETE /volumes/data HTTP/1.1\r\nHost: d\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                Duration::from_secs(30),
            );
            assert!(reply.starts_with("HTTP/1.1 204 No Content\r\n"), "{reply}");
            assert!(reply.contains("Connection: close"), "{reply}");
            assert!(!reply.contains("Error"), "{reply}");
        }
    }
}

/// An engine that trickles its reply is cut off by the call's total deadline,
/// not just the per-read one; the delete is then of unknown outcome.
#[test]
fn a_trickling_engine_reply_is_bounded_by_the_whole_call() {
    let engine = VolumeEngine::new();
    let h = H::new("deletedrip", false, engine.handler(), |cfg| {
        cfg.up_progress = Duration::from_millis(400);
    });
    let sock = h.serve(&h.auth);
    assert_eq!(
        call(&sock, "POST", "/volumes/create", Some(r#"{"Name":"drip"}"#)).0,
        201
    );
    let started = Instant::now();
    assert_eq!(call(&sock, "DELETE", "/volumes/drip", None).0, 502);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(h.svc.volume_fenced("drip"));
}

/// A DELETE admitted while lane A owned `data` must not reach the engine after
/// lane A's ownership ended and lane B created its own `data`: each volume
/// operation is checked, sent and recorded under one lock, so the later
/// requests wait for the first (or are refused) instead of overtaking it.
/// (An earlier DELETE held at the engine, a second DELETE that releases A's
/// ownership, B's create; then the first is released.)
#[test]
fn a_delayed_delete_cannot_remove_a_volume_another_workspace_now_owns() {
    let store: Arc<Mutex<std::collections::HashMap<String, String>>> = Arc::default();
    let deletes = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(AtomicBool::new(false));
    let custom: Handler = {
        let (store, deletes, gate) = (store.clone(), deletes.clone(), gate.clone());
        Arc::new(move |req, s| {
            let name = req.target.trim_start_matches("/volumes/").to_owned();
            match (req.method.as_str(), req.target.as_str()) {
                ("POST", "/volumes/create") => {
                    let body = String::from_utf8_lossy(&req.body).into_owned();
                    let named = body
                        .split("\"Name\":\"")
                        .nth(1)
                        .and_then(|rest| rest.split('"').next())
                        .unwrap_or("")
                        .to_owned();
                    store.lock().unwrap().insert(named, body.clone());
                    answer(s, 201, &body);
                    true
                }
                ("GET", t) if t.starts_with("/volumes/") => {
                    match store.lock().unwrap().get(&name).cloned() {
                        Some(body) => answer(s, 200, &body),
                        None => answer(s, 404, "{\"message\":\"no such volume\"}"),
                    }
                    true
                }
                ("DELETE", t) if t.starts_with("/volumes/") => {
                    if deletes.fetch_add(1, Ordering::SeqCst) == 0 {
                        while !gate.load(Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    let found = store.lock().unwrap().remove(&name).is_some();
                    if found {
                        let _ =
                            s.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
                    } else {
                        answer(s, 404, "{\"message\":\"no such volume\"}");
                    }
                    true
                }
                _ => false,
            }
        })
    };
    let h = H::new("staledelete", false, custom, |_| {});
    let auth_b = Authority {
        project_root: h.dir.clone(),
        workspace: "lane-b".into(),
        attempt: "att2".into(),
    };
    let (sock_a, sock_b) = (h.serve(&h.auth), h.serve(&auth_b));
    let (status, body) = call(
        &sock_a,
        "POST",
        "/volumes/create",
        Some(r#"{"Name":"data"}"#),
    );
    assert_eq!(status, 201, "{body}");

    let first = {
        let sock = sock_a.clone();
        std::thread::spawn(move || call(&sock, "DELETE", "/volumes/data", None).0)
    };
    assert!(wait_for(Duration::from_secs(5), || deletes
        .load(Ordering::SeqCst)
        == 1));
    let second = {
        let sock = sock_a.clone();
        std::thread::spawn(move || call(&sock, "DELETE", "/volumes/data", None).0)
    };
    let create_b = {
        let sock = sock_b.clone();
        std::thread::spawn(move || {
            call(&sock, "POST", "/volumes/create", Some(r#"{"Name":"data"}"#)).0
        })
    };
    // Neither may overtake the first while it is still at the engine.
    std::thread::sleep(Duration::from_millis(400));
    assert!(!second.is_finished(), "a second delete overtook the first");
    assert!(
        !create_b.is_finished(),
        "another workspace claimed the name meanwhile"
    );
    gate.store(true, Ordering::SeqCst);
    assert_eq!(first.join().unwrap(), 204);
    assert_eq!(
        second.join().unwrap(),
        404,
        "the name was no longer lane A's"
    );
    assert_eq!(create_b.join().unwrap(), 201);
    // Lane B's volume exists, in the engine and in the ledger.
    assert!(store.lock().unwrap().contains_key("data"));
    assert!(h.svc.ledger.has_volume("lane-b", "data"));
    assert!(!h.svc.ledger.has_volume("lane-a", "data"));
}

// --- freeze ordering ------------------------------------------------------------

fn inflight(svc: &Services, ws: &str) -> usize {
    svc.gate
        .scopes
        .lock()
        .unwrap()
        .get(ws)
        .map_or(0, |e| e.inflight)
}

/// A freeze begins only after an admitted mutation has finished, even one whose
/// body is still arriving; mutations after it are refused.
#[test]
fn a_freeze_waits_for_a_mutation_whose_body_is_still_arriving() {
    let h = H::new("freeze", false, no_custom(), |c| {
        c.http.progress = Duration::from_secs(5);
    });
    let sock = h.serve(&h.auth);
    // Warm the scope network so the delayed create is one engine call.
    let (st, _) = call(&sock, "POST", "/containers/create", Some(CREATE));
    assert_eq!(st, 201);
    let seen_before = h.engine.created.load(Ordering::SeqCst);

    let sock2 = sock.clone();
    let slow = std::thread::spawn(move || {
        let mut s = UnixStream::connect(&sock2).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            s,
            "POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            CREATE.len()
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(700));
        s.write_all(CREATE.as_bytes()).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        String::from_utf8_lossy(&out).starts_with("HTTP/1.1 201")
    });
    assert!(wait_for(Duration::from_secs(3), || inflight(
        &h.svc, "lane-a"
    ) == 1));
    let guard = h.svc.freeze_scope("lane-a");
    let engine_has_it = h.engine.created.load(Ordering::SeqCst) > seen_before;
    assert!(
        engine_has_it,
        "the freeze was granted while a mutation was still in flight"
    );
    // Frozen: further mutations are refused, every time (a refusal must not end
    // the freeze), reads are not.
    for round in 0..3 {
        let (st, _) = call(&sock, "POST", "/containers/create", Some(CREATE));
        assert_eq!(
            st, 503,
            "refusal {round}: the freeze ended after an earlier refusal"
        );
    }
    let (st, _) = call(&sock, "DELETE", &format!("/containers/{C1}"), None);
    assert_eq!(st, 503);
    let (st, _) = call(&sock, "GET", "/containers/json", None);
    assert_ne!(st, 503);
    drop(guard);
    assert!(slow.join().unwrap(), "the delayed create was not answered");
    let (st, _) = call(&sock, "POST", "/containers/create", Some(CREATE));
    assert_eq!(st, 201, "the freeze did not end with its guard");
}

/// The same when the delay is in the engine (upstream admission): the freeze is
/// granted only after the engine has answered and the ledger is updated.
#[test]
fn a_freeze_waits_for_a_mutation_stuck_in_the_engine() {
    let answered = Arc::new(AtomicBool::new(false));
    let a2 = answered.clone();
    let h = H::new(
        "freeze-engine",
        false,
        Arc::new(move |req, s| {
            if req.method == "POST" && req.target.starts_with("/containers/create") {
                std::thread::sleep(Duration::from_millis(900));
                answer(s, 201, &format!("{{\"Id\":\"{:064x}\"}}", 5));
                a2.store(true, Ordering::SeqCst);
                return true;
            }
            false
        }),
        |_| {},
    );
    let sock = h.serve(&h.auth);
    let sock2 = sock.clone();
    let slow =
        std::thread::spawn(move || call(&sock2, "POST", "/containers/create", Some(CREATE)).0);
    assert!(wait_for(Duration::from_secs(3), || inflight(
        &h.svc, "lane-a"
    ) == 1));
    let guard = h.svc.freeze_scope("lane-a");
    assert!(
        answered.load(Ordering::SeqCst),
        "the freeze was granted while the engine had not answered an admitted create"
    );
    // The ledger knows the container the moment the freeze is granted.
    assert!(matches!(
        h.svc
            .ledger
            .find_container("lane-a", &format!("{:064x}", 5)),
        super::Lookup::Own(_)
    ));
    drop(guard);
    assert_eq!(slow.join().unwrap(), 201);
}

// --- list filtering --------------------------------------------------------------

/// An engine answer that cannot be filtered is not passed on (it would show the
/// other workspaces' objects); a parseable one is filtered by the ledger.
#[test]
fn a_list_that_cannot_be_filtered_fails_closed() {
    let garbage = Arc::new(AtomicBool::new(true));
    let g2 = garbage.clone();
    let h = H::new(
        "listfail",
        false,
        Arc::new(move |req, s| {
            if req.method == "GET" && req.target.starts_with("/containers/json") {
                if g2.load(Ordering::SeqCst) {
                    answer(s, 200, "[{\"Id\":\"aaaa\"} this is not json");
                } else {
                    answer(
                        s,
                        200,
                        &format!("[{{\"Id\":\"{C1}\"}},{{\"Id\":\"{:064x}\"}}]", 9),
                    );
                }
                return true;
            }
            false
        }),
        |_| {},
    );
    let sock = h.serve(&h.auth);
    let (st, body) = call(&sock, "GET", "/containers/json", None);
    assert_eq!(st, 502, "an unfiltered answer was passed on: {body}");
    assert!(!body.contains("aaaa"));

    garbage.store(false, Ordering::SeqCst);
    h.own_container(C1, &[]);
    let (st, body) = call(&sock, "GET", "/containers/json", None);
    assert_eq!(st, 200, "{body}");
    assert!(body.contains(C1), "{body}");
    assert!(!body.contains(&format!("{:064x}", 9)), "{body}");
}

// --- inherited pins ----------------------------------------------------------------

/// A container that takes volumes from another holds that container's pins: they
/// survive the original's deletion.
#[test]
fn volumes_from_keeps_the_pin_alive_after_the_original_is_deleted() {
    let h = H::new("inherit", false, no_custom(), |_| {});
    let sock = h.serve(&h.auth);
    h.svc
        .ledger
        .add_pin(
            "p1",
            PinRec {
                scope: "lane-a".into(),
                project_root: h.dir.to_string_lossy().into_owned(),
                rel: "data".into(),
                read_only: false,
                is_dir: true,
                subs: vec![],
                dev: 0,
                ino: 0,
                created: now(),
                failed: false,
            },
        )
        .unwrap();
    h.own_container(C1, &["p1"]);
    let body = format!(r#"{{"Image":"busybox","HostConfig":{{"VolumesFrom":["{C1}"]}}}}"#);
    let (st, out) = call(&sock, "POST", "/containers/create", Some(&body));
    assert_eq!(st, 201, "{out}");
    // The engine is given the scope network's ID, which Docker resolves before
    // any name, not the name.
    let sent = h.engine.last_body("POST", "/containers/create");
    assert!(
        sent.contains(&format!("\"NetworkMode\":\"{NET_ID}\"")),
        "{sent}"
    );
    let (st, _) = call(&sock, "DELETE", &format!("/containers/{C1}"), None);
    assert_eq!(st, 204);
    assert!(
        h.svc.ledger.pin("p1").is_some(),
        "the pin was released while a container still takes volumes from it"
    );
    let child = format!("{:064x}", 1);
    assert_eq!(h.svc.ledger.container_pins(&child), vec!["p1".to_string()]);
    // And it is released with the last holder.
    let (st, _) = call(&sock, "DELETE", &format!("/containers/{child}"), None);
    assert_eq!(st, 204);
    assert!(h.svc.ledger.pin("p1").is_none());
}

// --- long-lived state ---------------------------------------------------------

/// DA1-06: per-attempt, per-root and per-workspace connection counts go
/// when their last connection does; the endpoint still serves afterwards.
#[test]
fn ended_attempts_leave_no_connection_counts_behind() {
    let h = H::new("state", false, no_custom(), |_| {});
    let socks = (0..8)
        .map(|n| {
            h.serve(&Authority {
                project_root: scratch(&format!("state-root-{n}")),
                workspace: format!("ws{n}"),
                attempt: format!("att{n}"),
            })
        })
        .collect::<Vec<_>>();
    let clients = socks
        .iter()
        .map(|sock| UnixStream::connect(sock).unwrap())
        .collect::<Vec<_>>();
    assert!(wait_for(Duration::from_secs(5), || h.svc.tracked().1 == 8));
    drop(clients);
    assert!(
        wait_for(Duration::from_secs(5), || {
            let (_, attempts, roots, workspaces) = h.svc.tracked();
            (attempts, roots, workspaces) == (0, 0, 0)
        }),
        "{:?}",
        h.svc.tracked()
    );
    let (status, body) = call(
        &h.serve(&h.auth),
        "POST",
        "/containers/create",
        Some(CREATE),
    );
    assert_eq!(status, 201, "{body}");
}

/// DA1-06: a bind root's descriptor goes with the last workspace that used
/// it (bind roots open only on Linux).
#[cfg(target_os = "linux")]
#[test]
fn a_forgotten_workspace_releases_its_bind_roots() {
    let h = H::new("roots", false, no_custom(), |_| {});
    let shared = scratch("roots-shared");
    h.svc.hold_root(&shared, "ws-a");
    h.svc.hold_root(&shared, "ws-b");
    h.svc.hold_root(&scratch("roots-own"), "ws-a");
    assert_eq!(h.svc.tracked().0, 2);
    h.svc.forget_scope("ws-a").unwrap();
    assert_eq!(h.svc.tracked().0, 1, "a root another workspace uses stays");
    h.svc.forget_scope("ws-b").unwrap();
    assert_eq!(h.svc.tracked().0, 0);
}

/// A `containers/create` with a bind under `root`, as a request.
#[cfg(target_os = "linux")]
fn bind_create(root: &Path) -> String {
    let body = format!(
        r#"{{"Image":"busybox","HostConfig":{{"Binds":["{}:/data"]}}}}"#,
        root.join("data").display()
    );
    format!(
        "POST /containers/create HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// DA1-06: a bind mount in a real `containers/create` opens its root under
/// the connection's own workspace, kept while the workspace is served, so
/// forgetting that workspace releases it.
#[cfg(target_os = "linux")]
#[test]
fn a_bind_in_a_create_is_released_with_its_workspace() {
    let h = H::new("bindroot", false, no_custom(), |_| {});
    std::fs::create_dir_all(h.dir.join("data")).unwrap();
    let sock = h.svc.socket_for(&h.auth).unwrap();
    let mut c = UnixStream::connect(&sock).unwrap();
    c.write_all(bind_create(&h.dir).as_bytes()).unwrap();
    // Whether or not this host may mount the pin, the root was opened.
    let _ = c.read_to_end(&mut Vec::new());
    assert!(wait_for(Duration::from_secs(5), || h
        .svc
        .open_connections()
        == 0));
    assert_eq!(h.svc.tracked().0, 1);
    h.svc.forget_scope("another-workspace").unwrap();
    assert_eq!(h.svc.tracked().0, 1);
    h.svc.forget_scope(&h.auth.workspace).unwrap();
    assert_eq!(h.svc.tracked().0, 0);
}

/// DA1-06: a bind request that arrives on a connection accepted before its
/// workspace was forgotten opens the root again, but only for that
/// connection's life: eight such workspaces leave no root behind.
#[cfg(target_os = "linux")]
#[test]
fn a_late_bind_does_not_bring_back_a_forgotten_root() {
    let h = H::new("lateroot", false, no_custom(), |_| {});
    for index in 0..8 {
        let root = scratch(&format!("lateroot-{index}"));
        std::fs::create_dir_all(root.join("data")).unwrap();
        let auth = Authority {
            project_root: root.clone(),
            workspace: format!("ws{index}"),
            attempt: "att1".into(),
        };
        let sock = h.svc.socket_for(&auth).unwrap();
        let mut c = UnixStream::connect(&sock).unwrap();
        assert!(wait_for(Duration::from_secs(5), || h
            .svc
            .open_connections()
            == 1));
        h.svc.forget_scope(&auth.workspace).unwrap();
        c.write_all(bind_create(&root).as_bytes()).unwrap();
        let _ = c.read_to_end(&mut Vec::new());
        drop(c);
        // The connection's teardown drops its count first and releases the
        // roots after: wait for the roots themselves, not for the count.
        assert!(
            wait_for(Duration::from_secs(30), || {
                let (roots, _, _, workspaces) = h.svc.tracked();
                h.svc.open_connections() == 0 && roots == 0 && workspaces == 0
            }),
            "workspace {index}: forgotten roots came back: {:?}",
            h.svc.tracked()
        );
    }
    assert_eq!(h.svc.tracked().0, 0, "forgotten roots came back");
    assert_eq!(h.svc.tracked().3, 0, "ended workspaces kept a count");
}

// --- image tag ownership (DA1-02) ---------------------------------------------

/// An engine that has `postgres:16` (pulled by the base: it has a registry
/// digest) and learns every tag the engine accepts and every delete;
/// `/images/<name>/json` says whether it has the name. `slow` is how long a tag
/// call takes (the request is in flight meanwhile).
fn image_engine_with(start: &[&str], slow: Duration) -> (Handler, Arc<Mutex<Vec<String>>>) {
    let mut names: Vec<String> = vec!["postgres:16".to_owned()];
    names.extend(start.iter().map(|s| (*s).to_owned()));
    let tags: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(names));
    let state = tags.clone();
    // Names that are another tag of the pulled `postgres:16` image.
    let aliases: Arc<Mutex<Vec<String>>> = Arc::default();
    let handler: Handler = Arc::new(move |req, s| {
        let path = req.target.split('?').next().unwrap_or("");
        let has = |name: &str| tags.lock().unwrap().iter().any(|t| t == name);
        if req.method == "GET"
            && let Some(name) = path
                .strip_prefix("/images/")
                .and_then(|p| p.strip_suffix("/json"))
        {
            let alias = aliases.lock().unwrap().clone();
            if name == "postgres:16" || alias.iter().any(|a| a == name && has(a)) {
                let all: Vec<String> = std::iter::once("postgres:16".to_owned())
                    .chain(alias.into_iter().filter(|a| has(a)))
                    .map(|t| format!("\"{t}\""))
                    .collect();
                answer(
                    s,
                    200,
                    &format!(
                        r#"{{"RepoTags":[{}],"RepoDigests":["postgres@sha256:aa"]}}"#,
                        all.join(",")
                    ),
                );
            } else if has(name) {
                answer(s, 200, &format!(r#"{{"RepoTags":["{name}"]}}"#));
            } else {
                answer(s, 404, "{}");
            }
            return true;
        }
        match (req.method.as_str(), path) {
            ("GET", "/containers/json" | "/networks") => {
                answer(s, 200, "[]");
                return true;
            }
            ("GET", "/volumes") => {
                answer(s, 200, r#"{"Volumes":[]}"#);
                return true;
            }
            _ => {}
        }
        if req.method == "GET" && path == "/images/json" {
            let list: Vec<String> = tags
                .lock()
                .unwrap()
                .iter()
                .map(|t| format!(r#"{{"RepoTags":["{t}"]}}"#))
                .collect();
            answer(s, 200, &format!("[{}]", list.join(",")));
            return true;
        }
        if req.method == "DELETE"
            && let Some(name) = path.strip_prefix("/images/")
        {
            tags.lock().unwrap().retain(|t| t != name);
            answer(s, 200, "[]");
            return true;
        }
        if req.method == "POST" && path.starts_with("/images/") && path.ends_with("/tag") {
            std::thread::sleep(slow);
            if path == "/images/postgres:16/tag" {
                let q = req.target.split('?').nth(1).unwrap_or("");
                let get = |k: &str| {
                    q.split('&')
                        .find_map(|p| p.strip_prefix(&format!("{k}=")))
                        .unwrap_or("")
                        .to_owned()
                };
                aliases
                    .lock()
                    .unwrap()
                    .push(format!("{}:{}", get("repo"), get("tag")));
            }
            let q: Vec<&str> = req
                .target
                .split('?')
                .nth(1)
                .unwrap_or("")
                .split('&')
                .collect();
            let get = |k: &str| {
                q.iter()
                    .find_map(|p| p.strip_prefix(&format!("{k}=")))
                    .unwrap_or("")
                    .to_owned()
            };
            tags.lock()
                .unwrap()
                .push(format!("{}:{}", get("repo"), get("tag")));
            answer(s, 201, "{}");
            return true;
        }
        false
    });
    (handler, state)
}

fn image_engine() -> Handler {
    image_engine_with(&[], Duration::ZERO).0
}

fn lane(h: &H, name: &str) -> PathBuf {
    h.serve(&Authority {
        workspace: name.into(),
        attempt: format!("att-{name}"),
        ..h.auth.clone()
    })
}

fn tag_call(sock: &Path, repo: &str, tag: &str) -> u16 {
    call(
        sock,
        "POST",
        &format!("/images/src/tag?repo={repo}&tag={tag}"),
        None,
    )
    .0
}

/// Another workspace of the project cannot move a tag a workspace created or
/// the engine holds from a pull, by `tag`, `build -t`, or a build's `outputs`;
/// the creator can, and a name nobody has is free. (REST routes only: a build
/// through `/grpc` is not seen, see docs/docker.md.)
#[test]
fn one_workspace_cannot_move_a_tag_another_created_or_pulled() {
    let h = H::new("tags", false, image_engine(), |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    assert_eq!(tag_call(&lane_a, "app", "1"), 201, "a free name");
    assert_eq!(
        h.svc.ledger.tag_owner("app:1").map(|r| r.scope),
        Some("lane-a".into()),
        "the engine accepted it, so the creator is recorded"
    );
    assert_eq!(
        tag_call(&lane_a, "app", "1"),
        201,
        "the creator moves its own tag"
    );
    assert_eq!(
        tag_call(&lane_b, "app", "1"),
        403,
        "another workspace's tag"
    );
    assert_eq!(tag_call(&lane_b, "postgres", "16"), 403, "a pulled image");
    assert_eq!(tag_call(&lane_b, "mine", "1"), 201);
    assert_eq!(tag_call(&lane_a, "mine", "1"), 403);

    let builds_before = h.engine.saw("POST", "/build");
    let outputs =
        "%5B%7B%22Type%22%3A%22moby%22%2C%22Attrs%22%3A%7B%22name%22%3A%22app%3A1%22%7D%7D%5D";
    for target in [
        "/build?t=app%3A1".to_owned(),
        "/build?t=postgres%3A16".to_owned(),
        format!("/build?outputs={outputs}"),
    ] {
        let (st, body) = call(&lane_b, "POST", &target, None);
        assert_eq!(st, 403, "{target}: {body}");
    }
    assert_eq!(
        h.engine.saw("POST", "/build"),
        builds_before,
        "a refused build reached the engine"
    );
}

/// Review witness (gpt): a pull over another workspace's tag is refused.
#[test]
fn a_foreign_pull_is_refused() {
    let custom: Handler = {
        let inner = image_engine_with(&["app:1"], Duration::ZERO).0;
        Arc::new(move |req, s| {
            if req.method == "POST" && req.target.starts_with("/images/create?") {
                answer(s, 200, "{}");
                return true;
            }
            inner(req, s)
        })
    };
    let h = H::new("tag-pull", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    let lane_b = lane(&h, "lane-b");
    let (st, body) = call(&lane_b, "POST", "/images/create?fromImage=app&tag=1", None);
    assert_eq!(st, 403, "foreign pull reached the engine: {body}");
    // Its own tag, a free name and a digest are not refused.
    let lane_a = h.serve(&h.auth);
    for target in [
        "/images/create?fromImage=app&tag=1",
        "/images/create?fromImage=busybox&tag=latest",
        "/images/create?fromImage=app%40sha256%3Aaa",
    ] {
        let who = if target.contains("app&") {
            &lane_a
        } else {
            &lane_b
        };
        assert_eq!(call(who, "POST", target, None).0, 200, "{target}");
    }
}

/// Review witness (gpt), extended: deleting another workspace's tag, a pulled
/// image, or an image by ID that carries such a tag is refused; your own is not.
#[test]
fn a_foreign_delete_is_refused() {
    let (custom, names) = image_engine_with(&["app:1", "mine:1"], Duration::ZERO);
    let h = H::new("tag-delete", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    h.svc.ledger.set_tag("lane-b", "mine:1").unwrap();
    let lane_b = lane(&h, "lane-b");
    for target in ["/images/app:1", "/images/postgres:16"] {
        let (st, body) = call(&lane_b, "DELETE", target, None);
        assert_eq!(st, 403, "{target}: {body}");
    }
    assert!(names.lock().unwrap().iter().any(|t| t == "app:1"));
    assert_eq!(call(&lane_b, "DELETE", "/images/mine:1", None).0, 200);
}

/// After `docker rmi` the name is free again at once: a record whose image is
/// gone holds nothing.
#[test]
fn a_tag_is_free_again_after_its_image_is_deleted() {
    let h = H::new("tag-rmi", false, image_engine(), |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    assert_eq!(tag_call(&lane_a, "app", "1"), 201);
    assert_eq!(tag_call(&lane_b, "app", "1"), 403);
    assert_eq!(call(&lane_a, "DELETE", "/images/app:1", None).0, 200);
    assert_eq!(tag_call(&lane_b, "app", "1"), 201);
    assert_eq!(
        h.svc.ledger.tag_owner("app:1").map(|r| r.scope),
        Some("lane-b".into())
    );
}

/// Review witnesses (gpt): a request the engine refused, or one refused part
/// way through a set of names, leaves no record.
#[test]
fn a_failed_or_refused_request_leaves_no_claim() {
    let custom: Handler = {
        let inner = image_engine_with(&["app:1"], Duration::ZERO).0;
        Arc::new(move |req, s| {
            if req.method == "POST" && req.target.starts_with("/images/missing/tag?") {
                answer(s, 404, "{}");
                return true;
            }
            inner(req, s)
        })
    };
    let h = H::new("tag-failed", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    let lane_b = lane(&h, "lane-b");
    let (st, _) = call(
        &lane_b,
        "POST",
        "/images/missing/tag?repo=unused&tag=1",
        None,
    );
    assert_eq!(st, 404);
    assert!(
        h.svc.ledger.tag_owner("unused:1").is_none(),
        "a failed tag left a record"
    );
    let target = "/build?outputs=%5B%7B%22Type%22%3A%22moby%22%2C%22Attrs%22%3A%7B%22name%22%3A%22aaa%3A1%2Capp%3A1%22%7D%7D%5D";
    assert_eq!(call(&lane_b, "POST", target, None).0, 403);
    assert!(
        h.svc.ledger.tag_owner("aaa:1").is_none(),
        "a refused set left a partial claim"
    );
    assert!(
        h.svc.lock_tags().is_empty(),
        "a reservation was left behind"
    );
}

/// Review witness (gpt): while a workspace's build of a tag is running, no other
/// workspace can take the name, however old or missing the record is.
#[test]
fn an_active_build_keeps_its_tag() {
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let inner = image_engine();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "POST" && req.target.starts_with("/build?") {
            let until = Instant::now() + Duration::from_secs(5);
            while !gate.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            answer(s, 200, "{}");
            return true;
        }
        inner(req, s)
    });
    let h = H::new("tag-active", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let building = std::thread::spawn(move || call(&lane_a, "POST", "/build?t=app%3A1", None));
    assert!(wait_for(Duration::from_secs(5), || h
        .engine
        .saw("POST", "/build")
        > 0));
    let (st, body) = call(&lane_b, "POST", "/images/src/tag?repo=app&tag=1", None);
    assert_eq!(
        st, 403,
        "another workspace took an active build's name: {body}"
    );
    release.store(true, Ordering::SeqCst);
    assert_eq!(building.join().unwrap().0, 200);
    assert!(
        h.svc.lock_tags().is_empty(),
        "the reservation outlived the build"
    );
}

/// The reservation is atomic: workspaces racing for one new name, while the
/// engine is slow, give it to exactly one.
#[test]
fn racing_workspaces_get_a_new_tag_once() {
    let (custom, _) = image_engine_with(&[], Duration::from_millis(150));
    let h = H::new("tag-race", false, custom, |_| {});
    let socks: Vec<PathBuf> = (0..8).map(|n| lane(&h, &format!("lane-{n}"))).collect();
    let results: Vec<u16> = std::thread::scope(|sc| {
        let handles: Vec<_> = socks
            .iter()
            .map(|s| sc.spawn(move || tag_call(s, "race", "1")))
            .collect();
        handles.into_iter().map(|t| t.join().unwrap()).collect()
    });
    assert_eq!(
        results.iter().filter(|c| **c == 201).count(),
        1,
        "{results:?}"
    );
    assert_eq!(
        results.iter().filter(|c| **c == 403).count(),
        7,
        "{results:?}"
    );
}

/// A full tag table does not refuse for ever: records of images that are gone
/// go, and a record of an image that is there stays.
#[test]
fn a_full_tag_table_drops_records_of_missing_images_only() {
    let (custom, _) = image_engine_with(&["keep:1"], Duration::ZERO);
    let h = H::new("tag-cap", false, custom, |_| {});
    for n in 0..crate::ledger::MAX_TAGS - 1 {
        h.svc.ledger.set_tag("dead", &format!("dead:{n}")).unwrap();
    }
    h.svc.ledger.set_tag("lane-b", "keep:1").unwrap();
    assert_eq!(h.svc.ledger.tag_count(), crate::ledger::MAX_TAGS);
    let lane_a = h.serve(&h.auth);
    assert_eq!(tag_call(&lane_a, "fresh", "1"), 201);
    assert!(h.svc.ledger.tag_count() < 10, "stale records stayed");
    assert_eq!(
        h.svc.ledger.tag_owner("keep:1").map(|r| r.scope),
        Some("lane-b".into()),
        "a live owner's record was evicted"
    );
    assert_eq!(tag_call(&lane_a, "keep", "1"), 403);
}

const NONE: [&str; 0] = [];

#[test]
fn a_lost_engine_reply_is_not_counted_as_a_refusal() {
    use crate::engine_api::untag_outcome;
    use crate::policy::endpoint::MAX_UNTAG_REFUSALS;
    use crate::relay::{DockerEndpoint, Filtered};
    let (inner, names) = image_engine_with(&[], Duration::ZERO);
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "DELETE" {
            // The engine accepted the request, but the connection ends without
            // an answer. It may or may not have performed the delete.
            return true;
        }
        inner(req, s)
    });
    let h = H::new("review-unknown-wire", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    assert_eq!(tag_as(&lane_a, "postgres:16", "app"), 201);
    let engine = crate::engine_api::Engine::new(lane_a);
    let endpoint = Filtered::new(h.svc.clone());
    let mut statuses = Vec::new();
    for _ in 0..MAX_UNTAG_REFUSALS {
        let _ = endpoint.untag_scope("lane-a", &mut |name| {
            let status = untag_outcome(&engine.call("DELETE", &format!("/images/{name}"), None));
            statuses.push(status);
            status
        });
    }
    assert!(names.lock().unwrap().iter().any(|t| t == "app:1"));
    assert_eq!(
        h.svc.ledger.scope_tags("lane-a"),
        ["app:1"],
        "unknown outcomes freed the record; statuses delivered by the filter: {statuses:?}"
    );
    assert_eq!(h.svc.ledger.tag_owner("app:1").unwrap().untag_refused, 0);
}

/// A delete's late answer must not retire a newer claim by the same workspace.
#[test]
fn a_late_rmi_answer_keeps_the_same_workspaces_newer_claim() {
    let (inner, names) = image_engine_with(&[], Duration::ZERO);
    let removed = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let removed_engine = removed.clone();
    let release_engine = release.clone();
    let engine_names = names.clone();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "DELETE" && req.target == "/images/app:1" {
            engine_names.lock().unwrap().retain(|t| t != "app:1");
            removed_engine.store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(5);
            while !release_engine.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            answer(s, 200, "[]");
            return true;
        }
        inner(req, s)
    });
    let h = H::new("review-late-rmi", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    assert_eq!(tag_as(&lane_a, "postgres:16", "app"), 201);
    std::thread::scope(|sc| {
        let deleting_socket = lane_a.clone();
        let deleting = sc.spawn(move || call(&deleting_socket, "DELETE", "/images/app:1", None).0);
        assert!(wait_for(Duration::from_secs(5), || removed.load(Ordering::SeqCst)));
        assert_eq!(tag_as(&lane_a, "postgres:16", "app"), 201);
        assert_eq!(h.svc.ledger.scope_tags("lane-a"), ["app:1"]);
        release.store(true, Ordering::SeqCst);
        assert_eq!(deleting.join().unwrap(), 200);
    });
    assert!(names.lock().unwrap().iter().any(|t| t == "app:1"));
    let recorded = h.svc.ledger.scope_tags("lane-a");
    assert_eq!(drop_tags(&h, &lane_a, "lane-a"), NONE);
    assert!(
        !names.lock().unwrap().iter().any(|t| t == "app:1"),
        "closing the workspace orphaned its newly claimed alias; record after late rmi: {recorded:?}"
    );
}

/// What `Restorer::drop_workspace` does for tags: the unforced delete by name
/// through the filtered endpoint, as the workspace.
fn drop_tags(h: &H, sock: &Path, scope: &str) -> Vec<String> {
    h.svc.untag_scope(scope, |name| {
        let (status, body) = call(sock, "DELETE", &format!("/images/{name}"), None);
        Untag::from_reply(status, &body)
    })
}

fn tag_as(sock: &Path, src: &str, repo: &str) -> u16 {
    call(
        sock,
        "POST",
        &format!("/images/{src}/tag?repo={repo}&tag=1"),
        None,
    )
    .0
}

/// F2: closing a lane untags what it created, even an extra alias of a pulled
/// image (which no one can move or remove once the record is gone), and not
/// the image's other names. Another workspace can then take the name.
#[test]
fn closing_a_workspace_untags_its_own_tags_and_frees_the_names() {
    let (custom, names) = image_engine_with(&["built:1"], Duration::ZERO);
    let h = H::new("tag-close", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    assert_eq!(tag_as(&lane_a, "postgres:16", "p1-app"), 201);
    h.svc.ledger.set_tag("lane-a", "built:1").unwrap();
    assert_eq!(tag_as(&lane_b, "postgres:16", "p1-app"), 403);

    assert_eq!(drop_tags(&h, &lane_a, "lane-a"), NONE);
    h.svc.forget_scope("lane-a").unwrap();

    let left = names.lock().unwrap().clone();
    assert_eq!(left, ["postgres:16"], "only the lane's own tags go");
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
    let deletes: Vec<String> = h
        .engine
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.method == "DELETE")
        .map(|r| r.target.clone())
        .collect();
    assert_eq!(deletes, ["/images/built:1", "/images/p1-app:1"]);
    assert_eq!(tag_as(&lane_b, "postgres:16", "p1-app"), 201);
    assert_eq!(
        h.svc.ledger.tag_owner("p1-app:1").map(|r| r.scope),
        Some("lane-b".into())
    );
}

/// A tag another workspace owns now is not the closing workspace's: nothing is
/// sent for it, and a delete that does name it is refused by the filter.
#[test]
fn closing_a_workspace_leaves_a_tag_another_workspace_took() {
    let (custom, names) = image_engine_with(&[], Duration::ZERO);
    let h = H::new("tag-moved", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    // The image is gone, so the name is free: lane-b takes it over.
    assert_eq!(tag_as(&lane_b, "postgres:16", "app"), 201);
    assert_eq!(
        h.svc.ledger.tag_owner("app:1").map(|r| r.scope),
        Some("lane-b".into())
    );
    let mut asked = Vec::new();
    let stuck = h.svc.untag_scope("lane-a", |name| {
        asked.push(name.to_owned());
        Untag::Unknown
    });
    assert_eq!(stuck, NONE);
    assert_eq!(
        asked, NONE,
        "asked to remove a tag that is not the closing workspace's"
    );
    assert_eq!(h.engine.saw("DELETE", "/images/"), 0);
    assert!(names.lock().unwrap().iter().any(|t| t == "app:1"));
    // A delete of it by the closing workspace (a stale list) is refused, and
    // the record stays lane-b's.
    h.svc.ledger.set_tag("lane-a", "other:1").unwrap();
    assert_eq!(call(&lane_a, "DELETE", "/images/app:1", None).0, 403);
    assert_eq!(
        h.svc.ledger.tag_owner("app:1").map(|r| r.scope),
        Some("lane-b".into())
    );
    assert!(!h.svc.ledger.release_tag("lane-a", "app:1", 0).unwrap());
}

/// An engine failure, or no answer, keeps the record: the name stays the
/// closing workspace's (refused to others) until a repeated drop untags it.
#[test]
fn a_failed_untag_keeps_the_claim_until_it_is_confirmed() {
    let fail = Arc::new(AtomicBool::new(true));
    let inner = image_engine_with(&["built:1"], Duration::ZERO).0;
    let gate = fail.clone();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "DELETE" && gate.load(Ordering::SeqCst) {
            answer(s, 500, "{}");
            return true;
        }
        inner(req, s)
    });
    let h = H::new("tag-fail", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    h.svc.ledger.set_tag("lane-a", "built:1").unwrap();

    assert_eq!(drop_tags(&h, &lane_a, "lane-a"), ["built:1"]);
    assert_eq!(
        h.svc.untag_scope("lane-a", |_| Untag::Unknown),
        ["built:1"],
        "no answer is not a confirmation"
    );
    h.svc.forget_scope("lane-a").unwrap();
    assert_eq!(
        h.svc.ledger.tag_owner("built:1").map(|r| r.scope),
        Some("lane-a".into()),
        "forgetting the scope freed a tag the engine still has"
    );
    assert_eq!(tag_as(&lane_b, "postgres:16", "built"), 403);

    fail.store(false, Ordering::SeqCst);
    assert_eq!(drop_tags(&h, &lane_a, "lane-a"), NONE);
    assert!(h.svc.ledger.tag_owner("built:1").is_none());
    assert_eq!(tag_as(&lane_b, "postgres:16", "built"), 201);
}

/// The cleanup deletes a name only while it is still what the workspace tagged.
/// A name the workspace removed itself, and another workspace then pulled, is the
/// registry's tag and stays: with a record left behind by the workspace's own
/// `docker rmi`, or by an untag whose answer was lost.
fn repulled_name(unknown_untag: bool) {
    let (inner, names) = image_engine_with(&[], Duration::ZERO);
    let pulled_names = names.clone();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "POST" && req.target == "/images/create?fromImage=app&tag=1" {
            pulled_names.lock().unwrap().push("app:1".into());
            answer(s, 200, "{}");
            return true;
        }
        inner(req, s)
    });
    let h = H::new("tag-repull", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    assert_eq!(tag_as(&lane_a, "postgres:16", "app"), 201);
    if unknown_untag {
        let stuck = h.svc.untag_scope("lane-a", |name| {
            // The engine applied the delete; the answer never came back.
            names.lock().unwrap().retain(|t| t != name);
            Untag::Unknown
        });
        assert_eq!(stuck, ["app:1"]);
        h.svc.forget_scope("lane-a").unwrap();
    } else {
        assert_eq!(call(&lane_a, "DELETE", "/images/app:1", None).0, 200);
    }
    assert!(!names.lock().unwrap().iter().any(|t| t == "app:1"));
    let pull = "/images/create?fromImage=app&tag=1";
    assert_eq!(call(&lane_b, "POST", pull, None).0, 200);
    assert!(names.lock().unwrap().iter().any(|t| t == "app:1"));
    assert_eq!(drop_tags(&h, &lane_a, "lane-a"), NONE);
    assert!(
        names.lock().unwrap().iter().any(|t| t == "app:1"),
        "closing lane-a removed the tag lane-b pulled"
    );
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
}

#[test]
fn closing_a_workspace_leaves_a_name_another_workspace_pulled_after_its_rmi() {
    repulled_name(false);
}

#[test]
fn a_retry_leaves_a_name_pulled_after_an_untag_of_unknown_outcome() {
    repulled_name(true);
}

/// The cleanup releases the claim it saw: if the workspace claims the name again
/// while the delete runs, the confirmation is of the old claim and the newer
/// record stays (and is reported, so the next drop untags it).
#[test]
fn a_confirmation_does_not_release_a_newer_claim_of_the_name() {
    let (custom, _) = image_engine_with(&["x:1"], Duration::ZERO);
    let h = H::new("tag-newer", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-a", "x:1").unwrap();
    let stuck = h.svc.untag_scope("lane-a", |name| {
        h.svc.ledger.set_tag("lane-a", name).unwrap();
        Untag::Confirmed
    });
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), ["x:1"]);
    assert_eq!(
        stuck,
        ["x:1"],
        "the newer claim is reported, so it is retried"
    );
    assert_eq!(h.svc.untag_scope("lane-a", |_| Untag::Confirmed), NONE);
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
    h.svc.ledger.set_tag("lane-a", "x:1").unwrap();
    // A refusal of the old claim is not counted against the new one either.
    let stuck = h.svc.untag_scope("lane-a", |name| {
        h.svc.ledger.set_tag("lane-a", name).unwrap();
        Untag::Refused(409)
    });
    assert_eq!(stuck, ["x:1"]);
    assert_eq!(h.svc.ledger.tag_owner("x:1").unwrap().untag_refused, 0);
}

/// A tag the workspace claimed again while the cleanup's delete of the old one
/// was in flight (a leftover client of the closing lane) is still its own: the
/// drop reports it, so the daemon retries until a later round has untagged it.
#[test]
fn a_late_confirmation_of_the_cleanup_reports_a_newer_claim() {
    let (inner, names) = image_engine_with(&[], Duration::ZERO);
    let removed = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let removed_engine = removed.clone();
    let release_engine = release.clone();
    let engine_names = names.clone();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "DELETE" && req.target == "/images/app:1" {
            engine_names.lock().unwrap().retain(|t| t != "app:1");
            removed_engine.store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(5);
            while !release_engine.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            answer(s, 200, "[]");
            return true;
        }
        inner(req, s)
    });
    let h = H::new("review-drop-newer", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    assert_eq!(tag_as(&lane_a, "postgres:16", "app"), 201);
    let left = std::thread::scope(|sc| {
        let dropping = sc.spawn(|| drop_tags(&h, &lane_a, "lane-a"));
        assert!(wait_for(Duration::from_secs(5), || removed.load(Ordering::SeqCst)));
        assert_eq!(tag_as(&lane_a, "postgres:16", "app"), 201);
        release.store(true, Ordering::SeqCst);
        dropping.join().unwrap()
    });
    assert!(names.lock().unwrap().iter().any(|t| t == "app:1"));
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), ["app:1"]);
    assert_eq!(
        left,
        ["app:1"],
        "without left the daemon ends its cleanup journal"
    );
}

/// The filter's own replies (a full connection cap, its holds, a refused route)
/// are never the engine's word: they are not counted, however many come, and the
/// record is kept while its tag is in the engine.
#[test]
fn a_busy_project_cap_is_not_counted_as_an_engine_refusal() {
    use crate::policy::endpoint::MAX_UNTAG_REFUSALS;
    let (inner, names) = image_engine_with(&[], Duration::ZERO);
    let h = H::new("tag-429", false, inner, |c| c.max_conns_project = 3);
    let lane_a = h.serve(&h.auth);
    assert_eq!(tag_as(&lane_a, "postgres:16", "app"), 201);
    let auth = |ws: &str| Authority {
        project_root: h.dir.clone(),
        workspace: ws.into(),
        attempt: "att1".into(),
    };
    let idle: Vec<UnixStream> = ["w1", "w2", "w3"]
        .iter()
        .map(|ws| UnixStream::connect(h.serve(&auth(ws))).unwrap())
        .collect();
    assert!(wait_for(Duration::from_secs(2), || h
        .svc
        .open_connections()
        == 3));
    let engine = crate::engine_api::Engine::new(lane_a);
    assert!(
        engine.call("GET", "/_ping", None).is_err(),
        "the cap is full"
    );
    for _ in 0..MAX_UNTAG_REFUSALS * 2 {
        let left = h.svc.untag_scope("lane-a", |name| {
            let path = format!("/images/{name}");
            crate::engine_api::untag_outcome(&engine.call("DELETE", &path, None))
        });
        assert_eq!(left, ["app:1"]);
    }
    assert!(names.lock().unwrap().iter().any(|t| t == "app:1"));
    assert_eq!(h.svc.ledger.tag_owner("app:1").unwrap().untag_refused, 0);
    drop(idle);
}

/// A pull of an absent name releases the stale record of the PULLING workspace
/// too: its own record over a gone image would otherwise make closing it untag
/// the name it pulled.
#[test]
fn pulling_over_your_own_stale_record_releases_it() {
    let (inner, names) = image_engine_with(&[], Duration::ZERO);
    let pulled = names.clone();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "POST" && req.target == "/images/create?fromImage=app&tag=1" {
            pulled.lock().unwrap().push("app:1".into());
            answer(s, 200, "{}");
            return true;
        }
        inner(req, s)
    });
    let h = H::new("tag-own-stale", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    let pull = "/images/create?fromImage=app&tag=1";
    assert_eq!(call(&lane_a, "POST", pull, None).0, 200);
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
    assert_eq!(drop_tags(&h, &lane_a, "lane-a"), NONE);
    assert!(names.lock().unwrap().iter().any(|t| t == "app:1"));
    // While the image is there a pull over one's own live record keeps it.
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    assert_eq!(call(&lane_a, "POST", pull, None).0, 200);
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), ["app:1"]);
}

/// A workspace's own `docker rmi` of its tag releases the record at once, so no
/// stale record outlives the name (even before any other request or reconcile).
#[test]
fn deleting_your_own_tag_releases_its_record() {
    let h = H::new("tag-rmi-release", false, image_engine(), |_| {});
    let lane_a = h.serve(&h.auth);
    assert_eq!(tag_as(&lane_a, "postgres:16", "app"), 201);
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), ["app:1"]);
    assert_eq!(call(&lane_a, "DELETE", "/images/app:1", None).0, 200);
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
}

/// The cleanup starts by dropping the records of images the engine no longer
/// has (a prune, a delete by ID): nothing is asked of the engine for them.
#[test]
fn the_cleanup_starts_by_dropping_records_of_missing_images() {
    let h = H::new("tag-evict-first", false, image_engine(), |_| {});
    h.svc.ledger.set_tag("lane-a", "gone:1").unwrap();
    let mut asked = Vec::new();
    let stuck = h.svc.untag_scope("lane-a", |name| {
        asked.push(name.to_owned());
        Untag::Confirmed
    });
    assert_eq!(stuck, NONE);
    assert_eq!(asked, NONE);
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
}

/// A tag the route cannot name (255 characters, as for `DELETE /images/<name>`)
/// is refused when it is claimed, so no record can exist that no cleanup could
/// ever release.
#[test]
fn a_tag_longer_than_the_delete_route_allows_is_not_claimed() {
    let h = H::new("tag-long", false, image_engine(), |_| {});
    let lane_a = h.serve(&h.auth);
    let claim = |repo_len: usize| {
        let repo = "a".repeat(repo_len);
        let target = format!("/images/postgres:16/tag?repo={repo}&tag=0123456789");
        (
            call(&lane_a, "POST", &target, None).0,
            format!("{repo}:0123456789"),
        )
    };
    // `repo:0123456789` is 245 + 1 + 10 = 256 characters: one too many.
    assert_eq!(claim(245).0, 403);
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
    assert_eq!(claim(250).0, 403);
    // 244 + 1 + 10 = 255 is the longest the route names, and it is deletable.
    let (status, name) = claim(244);
    assert_eq!(status, 201);
    assert_eq!(name.len(), 255);
    assert_eq!(
        h.svc.ledger.scope_tags("lane-a"),
        std::slice::from_ref(&name)
    );
    assert_eq!(
        call(&lane_a, "DELETE", &format!("/images/{name}"), None).0,
        200
    );
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
}

/// Only a 2xx or a 404 confirms the untag; a refusal, a conflict, an engine
/// error and no answer keep the record. A record someone else took over in the
/// meantime is not reported.
#[test]
fn only_a_2xx_or_a_404_confirms_an_untag() {
    let (custom, _) = image_engine_with(&["x:1"], Duration::ZERO);
    let h = H::new("tag-statuses", false, custom, |_| {});
    for (status, confirmed) in [
        (Untag::Confirmed, true),
        (Untag::Refused(403), false),
        (Untag::Refused(409), false),
        (Untag::Refused(500), false),
        (Untag::Unknown, false),
    ] {
        h.svc.ledger.set_tag("lane-a", "x:1").unwrap();
        let stuck = h.svc.untag_scope("lane-a", |_| status);
        assert_eq!(stuck.is_empty(), confirmed, "{status:?}");
        assert_eq!(
            h.svc.ledger.scope_tags("lane-a").is_empty(),
            confirmed,
            "{status:?}"
        );
    }
    h.svc.ledger.set_tag("lane-a", "x:1").unwrap();
    let stuck = h.svc.untag_scope("lane-a", |name| {
        h.svc.ledger.set_tag("lane-b", name).unwrap();
        Untag::Refused(403)
    });
    assert_eq!(
        stuck, NONE,
        "a tag lane-b took over is not lane-a's to report"
    );
}

/// A refusal that never changes (a 409: the last tag of an image a container
/// uses) is not retried for ever: after `MAX_UNTAG_REFUSALS` answers the record
/// is released and the last report says what stays and how to remove it; the
/// next drop is clean. An unknown outcome is never counted.
#[test]
fn a_permanent_refusal_is_given_up_with_a_message() {
    use crate::policy::endpoint::MAX_UNTAG_REFUSALS;
    assert_eq!(
        MAX_UNTAG_REFUSALS, 12,
        "about a minute at the daemon's retry"
    );
    let (custom, _) = image_engine_with(&["x:1"], Duration::ZERO);
    let h = H::new("tag-giveup", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-a", "x:1").unwrap();
    // Neither an unknown outcome nor the filter's lost-reply 502 is counted.
    for _ in 0..30 {
        assert_eq!(h.svc.untag_scope("lane-a", |_| Untag::Unknown), ["x:1"]);
        let lost = Untag::from_reply(502, "");
        assert_eq!(h.svc.untag_scope("lane-a", |_| lost), ["x:1"]);
    }
    assert_eq!(h.svc.ledger.tag_owner("x:1").unwrap().untag_refused, 0);
    for round in 1..=11 {
        assert_eq!(
            h.svc.untag_scope("lane-a", |_| Untag::Refused(409)),
            ["x:1"],
            "round {round}"
        );
    }
    let last = h.svc.untag_scope("lane-a", |_| Untag::Refused(409));
    assert_eq!(last.len(), 1);
    assert!(last[0].starts_with("x:1 ("), "{last:?}");
    assert!(last[0].contains("409") && last[0].contains("12 times"));
    assert!(last[0].contains("marsh docker rm"));
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
    assert_eq!(h.svc.untag_scope("lane-a", |_| Untag::Refused(409)), NONE);
}

/// The production wiring: the relay's endpoint forwards the cleanup to the
/// services (a no-op there would leave every lane's tags in the engine).
#[test]
fn the_relays_endpoint_runs_the_cleanup() {
    use crate::relay::{DockerEndpoint, Filtered};
    let (custom, names) = image_engine_with(&["built:1"], Duration::ZERO);
    let h = H::new("tag-relay", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    h.svc.ledger.set_tag("lane-a", "built:1").unwrap();
    let endpoint = Filtered::new(h.svc.clone());
    let stuck = endpoint.untag_scope("lane-a", &mut |name| {
        let (status, body) = call(&lane_a, "DELETE", &format!("/images/{name}"), None);
        Untag::from_reply(status, &body)
    });
    assert_eq!(stuck, NONE);
    assert!(!names.lock().unwrap().iter().any(|t| t == "built:1"));
    assert_eq!(h.svc.ledger.scope_tags("lane-a"), NONE);
    h.svc.ledger.set_tag("lane-a", "postgres:16").unwrap();
    assert_eq!(
        endpoint.untag_scope("lane-a", &mut |_| Untag::Unknown),
        ["postgres:16"]
    );
}

/// While the engine untags a name another workspace cannot take it, and can
/// right after. The record keeps the name (it is the closing workspace's until
/// the engine answers); the delete's own hold, which covers a name no record
/// covers, is pinned by `a_delete_holds_its_name_until_the_engine_finishes`.
#[test]
fn a_claim_during_the_untag_is_refused() {
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let inner = image_engine_with(&["built:1"], Duration::ZERO).0;
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "DELETE" {
            let until = Instant::now() + Duration::from_secs(5);
            while !gate.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        inner(req, s)
    });
    let h = H::new("tag-during", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    h.svc.ledger.set_tag("lane-a", "built:1").unwrap();
    std::thread::scope(|sc| {
        let svc = h.svc.clone();
        let closing = sc.spawn(move || {
            svc.untag_scope("lane-a", |name| {
                let (status, body) = call(&lane_a, "DELETE", &format!("/images/{name}"), None);
                Untag::from_reply(status, &body)
            })
        });
        assert!(wait_for(Duration::from_secs(5), || h
            .engine
            .saw("DELETE", "/images/built:1")
            > 0));
        assert_eq!(tag_as(&lane_b, "postgres:16", "built"), 403);
        release.store(true, Ordering::SeqCst);
        assert_eq!(closing.join().unwrap(), NONE);
    });
    assert_eq!(tag_as(&lane_b, "postgres:16", "built"), 201);
}

// --- review round 2 ---------------------------------------------------------

fn gated(engine: Handler, prefix: &'static str) -> (Handler, Arc<AtomicBool>) {
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let handler: Handler = Arc::new(move |req, s| {
        if req.method == "POST" && req.target.starts_with(prefix) {
            let until = Instant::now() + Duration::from_secs(5);
            while !gate.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        engine(req, s)
    });
    (handler, release)
}

/// A workspace may delete its own alias of a pulled image (`docker tag
/// postgres:16 mine:pg; docker rmi mine:pg`); the image's other tags are
/// untouched, so only the name asked for is checked. An image ID would remove
/// every tag, so all of them are checked. (Claude r2 witness.)
#[test]
fn deleting_your_own_alias_of_a_pulled_image_is_allowed() {
    let (custom, _) = image_engine_with(&[], Duration::ZERO);
    let h = H::new("tag-alias", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let (st, body) = call(
        &lane_a,
        "POST",
        "/images/postgres:16/tag?repo=mine&tag=pg",
        None,
    );
    assert_eq!(st, 201, "{body}");
    assert_eq!(
        call(&lane_b, "DELETE", "/images/mine:pg", None).0,
        403,
        "another's alias"
    );
    assert_eq!(
        call(&lane_a, "DELETE", "/images/postgres:16", None).0,
        403,
        "the pulled tag"
    );
    assert_eq!(
        call(&lane_a, "DELETE", "/images/mine:pg", None).0,
        200,
        "its own alias"
    );
    // By ID, every tag of the image counts, the pulled one included.
    let inner = image_engine_with(&[], Duration::ZERO).0;
    let by_id: Handler = Arc::new(move |req, s| {
        if req.method == "GET" && req.target == "/images/sha256:abcdabcdabcd/json" {
            answer(
                s,
                200,
                r#"{"RepoTags":["postgres:16","mine:pg"],"RepoDigests":["x@sha256:aa"]}"#,
            );
            return true;
        }
        inner(req, s)
    });
    let h = H::new("tag-alias-id", false, by_id, |_| {});
    let lane_a = h.serve(&h.auth);
    assert_eq!(
        call(&lane_a, "DELETE", "/images/sha256:abcdabcdabcd", None).0,
        403
    );
}

/// `docker pull app` without a tag pulls every tag: refused. (gpt r2 witness.)
#[test]
fn a_pull_of_every_tag_is_refused() {
    let h = H::new("tag-pull-all", false, image_engine(), |_| {});
    let lane_b = lane(&h, "lane-b");
    for target in [
        "/images/create?fromImage=app",
        "/images/create?fromImage=app&tag=",
        // The engine drops one trailing colon, then pulls every tag.
        "/images/create?fromImage=app%3A",
        "/images/create?fromImage=app%3A&tag=",
        "/images/create?fromImage=localhost%3A5000%2Fapp",
    ] {
        assert_eq!(call(&lane_b, "POST", target, None).0, 403, "{target}");
    }
    assert_eq!(h.engine.saw("POST", "/images/create"), 0);
}

/// A pull in flight holds the name against a tag or build of it, and a pull
/// does not hold it against other pulls. (gpt r2 witness.)
#[test]
fn an_active_pull_keeps_its_name_but_not_against_pulls() {
    let (inner, _) = image_engine_with(&[], Duration::ZERO);
    let (custom, release) = gated(inner, "/images/create?");
    let h = H::new("tag-pull-race", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let lane_c = lane(&h, "lane-c");
    let b = lane_b.clone();
    let pulling =
        std::thread::spawn(move || call(&b, "POST", "/images/create?fromImage=app&tag=1", None).0);
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("POST", "/images/create")
        > 0));
    assert_eq!(
        tag_call(&lane_a, "app", "1"),
        403,
        "tag during another's pull"
    );
    assert_eq!(
        call(&lane_a, "POST", "/build?t=app%3A1", None).0,
        403,
        "build during a pull"
    );
    let c = std::thread::spawn(move || {
        call(&lane_c, "POST", "/images/create?fromImage=app&tag=1", None).0
    });
    release.store(true, Ordering::SeqCst);
    assert_eq!(pulling.join().unwrap(), 404);
    assert_eq!(
        c.join().unwrap(),
        404,
        "a second pull is not blocked by the first"
    );
    assert!(h.svc.lock_tags().is_empty());
}

/// A tag the engine accepted keeps its owner when the client has gone before
/// the answer. (gpt r2 witness.)
#[test]
fn a_client_that_disconnects_keeps_the_ownership_of_its_tag() {
    let (inner, _) = image_engine_with(&[], Duration::ZERO);
    let (custom, release) = gated(inner, "/images/src/tag");
    let h = H::new("tag-disconnect", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let mut client = UnixStream::connect(&lane_a).unwrap();
    client
        .write_all(b"POST /images/src/tag?repo=app&tag=1 HTTP/1.1\r\nHost: d\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("POST", "/images/src/tag")
        > 0));
    drop(client);
    release.store(true, Ordering::SeqCst);
    assert!(wait_for(Duration::from_secs(3), || h
        .svc
        .lock_tags()
        .is_empty()));
    assert_eq!(
        tag_call(&lane_b, "app", "1"),
        403,
        "adoptable because its client left"
    );
}

/// A request in flight counts against the table, so two requests cannot both
/// fill the last slot. (gpt r2 witness.)
#[test]
fn requests_in_flight_reserve_capacity_in_the_tag_table() {
    let (inner, present) = image_engine_with(&[], Duration::ZERO);
    let (custom, release) = gated(inner, "/images/src/tag?repo=first&");
    let h = H::new("tag-cap-inflight", false, custom, |_| {});
    for n in 0..crate::ledger::MAX_TAGS - 1 {
        let name = format!("keep:{n}");
        h.svc.ledger.set_tag("lane-a", &name).unwrap();
        present.lock().unwrap().push(name);
    }
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let tagging = std::thread::spawn(move || tag_call(&lane_a, "first", "1"));
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("POST", "/images/src/tag")
        > 0));
    assert_eq!(
        tag_call(&lane_b, "second", "1"),
        403,
        "the last slot is taken"
    );
    release.store(true, Ordering::SeqCst);
    assert_eq!(tagging.join().unwrap(), 201);
    assert_eq!(
        h.svc.ledger.tag_owner("first:1").map(|r| r.scope),
        Some("lane-a".into())
    );
}

/// An active request's name, and a name nobody recorded, are not evicted; the
/// reconcile pass sheds records of images that are gone. (Claude r2 survivors.)
#[test]
fn reconcile_sheds_stale_tag_records_but_not_held_or_live_ones() {
    let (custom, _) = image_engine_with(&["live:1"], Duration::ZERO);
    let h = H::new("tag-reconcile", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-a", "live:1").unwrap();
    h.svc.ledger.set_tag("lane-a", "gone:1").unwrap();
    h.svc.ledger.set_tag("lane-a", "held:1").unwrap();
    h.svc
        .lock_tags()
        .entry("held:1".into())
        .or_default()
        .add("lane-a", super::HoldKind::Claim);
    h.svc.reconcile();
    assert!(
        h.svc.ledger.tag_owner("gone:1").is_none(),
        "reconcile left a stale record"
    );
    assert!(
        h.svc.ledger.tag_owner("live:1").is_some(),
        "a live record went"
    );
    assert!(
        h.svc.ledger.tag_owner("held:1").is_some(),
        "an in-flight name went"
    );
}

/// An active build's name also blocks another workspace's delete of an image
/// that has it (no record yet) and its pull. (Claude r2 survivor.)
#[test]
fn an_active_build_blocks_a_delete_of_its_name() {
    let (inner, _) = image_engine_with(&["app:1"], Duration::ZERO);
    let (custom, release) = gated(inner, "/build?");
    let h = H::new("tag-build-delete", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let building = std::thread::spawn(move || call(&lane_a, "POST", "/build?t=app%3A1", None).0);
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("POST", "/build")
        > 0));
    assert_eq!(call(&lane_b, "DELETE", "/images/app:1", None).0, 403);
    assert_eq!(
        call(&lane_b, "POST", "/images/create?fromImage=app&tag=1", None).0,
        403
    );
    release.store(true, Ordering::SeqCst);
    let _ = building.join();
}

/// Two requests of one workspace for one name: the first to end does not
/// release the other's hold. (Claude r2 survivor.)
#[test]
fn a_name_stays_held_until_every_request_of_the_workspace_ends() {
    use super::HoldKind::{Claim, Delete, Pull};
    let mut held = super::TagHeld::default();
    held.add("a", Claim);
    held.add("a", Claim);
    assert!(!held.release("a", Claim));
    assert!(held.blocks_claim("b"));
    assert!(held.release("a", Claim));
    // Who excludes whom: pulls share a name, a delete shares only with deletes.
    held.add("a", Pull);
    assert!(held.blocks_claim("b") && !held.blocks_pull("b") && held.blocks_delete("b"));
    assert!(held.release("a", Pull));
    held.add("a", Delete);
    assert!(held.blocks_claim("b") && held.blocks_pull("b") && !held.blocks_delete("b"));
    assert!(!held.blocks_claim("a") && !held.blocks_pull("a") && !held.blocks_delete("a"));
}

/// The Go SDK sends `busybox@sha256:..` as `fromImage=busybox&tag=sha256:..`;
/// a digest pull moves no tag, so it is not refused and needs no hold.
/// (gpt r3 witness.)
#[test]
fn a_digest_pull_in_either_encoding_is_allowed() {
    let h = H::new("tag-pull-digest", false, image_engine(), |_| {});
    let lane_b = lane(&h, "lane-b");
    let digest = format!("sha256%3A{}", "ab".repeat(32));
    for target in [
        format!("/images/create?fromImage=busybox&tag={digest}"),
        format!("/images/create?fromImage=busybox%40{digest}"),
    ] {
        let (st, body) = call(&lane_b, "POST", &target, None);
        assert_ne!(st, 403, "{target}: {body}");
    }
    assert_eq!(h.engine.saw("POST", "/images/create"), 2);
    // Something that is not a digest in `tag` is still refused.
    let (st, _) = call(
        &lane_b,
        "POST",
        "/images/create?fromImage=busybox&tag=a%3Ab",
        None,
    );
    assert_eq!(st, 403);
}

/// A short image ID (Docker takes any unique prefix) removes every tag of the
/// image, so it is checked like a full ID. (gpt r3 witness.)
#[test]
fn a_short_image_id_cannot_delete_a_foreign_tag() {
    let inner = image_engine_with(&["app:1"], Duration::ZERO).0;
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "GET" && req.target == "/images/abcd/json" {
            answer(s, 200, r#"{"RepoTags":["app:1"]}"#);
            return true;
        }
        inner(req, s)
    });
    let h = H::new("tag-short-id", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    let lane_b = lane(&h, "lane-b");
    assert_eq!(call(&lane_b, "DELETE", "/images/abcd?force=1", None).0, 403);
    assert_eq!(h.engine.saw("DELETE", "/images/abcd"), 0);
}

/// A delete holds the names it removes until the engine has finished, even a
/// name no image has yet, so a tag made meanwhile is not removed from under its
/// owner. (gpt r3 witness.)
#[test]
fn a_delete_holds_its_name_until_the_engine_finishes() {
    let inner = image_engine();
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "DELETE" && req.target.starts_with("/images/app:1") {
            let until = Instant::now() + Duration::from_secs(5);
            while !gate.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        inner(req, s)
    });
    let h = H::new("tag-delete-hold", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let deleting = std::thread::spawn(move || call(&lane_b, "DELETE", "/images/app:1", None).0);
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("DELETE", "/images/app:1")
        > 0));
    assert_eq!(
        tag_call(&lane_a, "app", "1"),
        403,
        "a tag made during another's delete"
    );
    release.store(true, Ordering::SeqCst);
    assert_eq!(deleting.join().unwrap(), 200);
    assert!(h.svc.lock_tags().is_empty());
    assert!(
        h.svc.ledger.tag_owner("app:1").is_none(),
        "a delete recorded its names as the deleter's"
    );
    assert_eq!(
        tag_call(&lane_a, "app", "1"),
        201,
        "free once the delete is over"
    );
}

/// A pull of a name another workspace's delete holds is refused (the delete
/// would otherwise remove the pulled tag and free the name), while the delete's
/// own workspace and other deletes are not blocked. (gpt r4 witness.)
#[test]
fn a_pull_waits_for_another_workspaces_delete_of_its_name() {
    let inner = image_engine();
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "DELETE" && req.target.starts_with("/images/app:1") {
            let until = Instant::now() + Duration::from_secs(5);
            while !gate.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        inner(req, s)
    });
    let h = H::new("tag-delete-pull", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let b = lane_b.clone();
    let deleting = std::thread::spawn(move || call(&b, "DELETE", "/images/app:1", None).0);
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("DELETE", "/images/app:1")
        > 0));
    let (st, body) = call(&lane_a, "POST", "/images/create?fromImage=app&tag=1", None);
    assert_eq!(st, 403, "pull during another's delete: {body}");
    assert_ne!(
        call(&lane_b, "POST", "/images/create?fromImage=app&tag=1", None).0,
        403,
        "a workspace's own pull is not blocked by its delete"
    );
    release.store(true, Ordering::SeqCst);
    assert_eq!(deleting.join().unwrap(), 200);
    assert!(h.svc.lock_tags().is_empty());
}

/// A delete of a name no image has yet still waits for another workspace that
/// is building it. (Claude r4 survivor.)
#[test]
fn a_delete_of_an_absent_name_waits_for_a_build_of_it() {
    let (inner, _) = image_engine_with(&[], Duration::ZERO);
    let (custom, release) = gated(inner, "/build?");
    let h = H::new("tag-absent-delete", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let building = std::thread::spawn(move || call(&lane_a, "POST", "/build?t=app%3A1", None).0);
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("POST", "/build")
        > 0));
    assert_eq!(call(&lane_b, "DELETE", "/images/app:1", None).0, 403);
    assert_eq!(h.engine.saw("DELETE", "/images/app:1"), 0);
    release.store(true, Ordering::SeqCst);
    let _ = building.join();
}

/// An engine whose image `abcd` (ID `sha256:abcd`) has B's `mine:1`; a delete
/// of `/images/abcd` or a tag from it can be held at the engine.
fn id_engine(gate_on: &'static str) -> (Handler, Arc<AtomicBool>) {
    let inner = image_engine_with(&["mine:1"], Duration::ZERO).0;
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let handler: Handler = Arc::new(move |req, s| {
        if req.method == "GET" && req.target == "/images/abcd/json" {
            answer(s, 200, r#"{"Id":"sha256:abcd","RepoTags":["mine:1"]}"#);
            return true;
        }
        if req.method == gate_on.split_once(' ').unwrap().0
            && req.target.starts_with(gate_on.split_once(' ').unwrap().1)
        {
            let until = Instant::now() + Duration::from_secs(5);
            while !gate.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        inner(req, s)
    });
    (handler, release)
}

/// A tag put on an image while an ID delete of it is running would be removed
/// with the image: refused. (gpt r4 witness.)
#[test]
fn a_tag_cannot_be_put_on_an_image_while_it_is_being_deleted_by_id() {
    let (custom, release) = id_engine("DELETE /images/abcd");
    let h = H::new("tag-id-delete", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-b", "mine:1").unwrap();
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let deleting =
        std::thread::spawn(move || call(&lane_b, "DELETE", "/images/abcd?force=1", None).0);
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("DELETE", "/images/abcd")
        > 0));
    let (st, body) = call(&lane_a, "POST", "/images/abcd/tag?repo=app&tag=1", None);
    assert_eq!(st, 403, "tag on an image being deleted: {body}");
    release.store(true, Ordering::SeqCst);
    assert_eq!(deleting.join().unwrap(), 200);
    assert!(h.svc.lock_tags().is_empty());
}

/// ... and the other way round: an ID delete waits for a tag being put on the
/// image.
#[test]
fn an_id_delete_waits_for_a_tag_being_put_on_the_image() {
    let (custom, release) = id_engine("POST /images/abcd/tag");
    let h = H::new("tag-id-delete2", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-b", "mine:1").unwrap();
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let tagging = std::thread::spawn(move || {
        call(&lane_a, "POST", "/images/abcd/tag?repo=app&tag=1", None).0
    });
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("POST", "/images/abcd/tag")
        > 0));
    assert_eq!(call(&lane_b, "DELETE", "/images/abcd?force=1", None).0, 403);
    assert_eq!(h.engine.saw("DELETE", "/images/abcd"), 0);
    release.store(true, Ordering::SeqCst);
    assert_eq!(tagging.join().unwrap(), 201);
    assert!(h.svc.lock_tags().is_empty());
}

// --- fourth window: no forced ID deletes -------------------------------------

fn delete_target(h: &H, prefix: &str) -> String {
    h.engine
        .log
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.method == "DELETE" && r.target.starts_with(prefix))
        .map_or_default(|r| r.target.clone())
}

/// A delete by ID, short ID or digest never carries `force` to the engine (the
/// engine then refuses an image that has several tags), while a delete by one of
/// the image's own tag names keeps it. (gpt and claude r5.)
#[test]
fn an_id_delete_is_never_forced_and_a_named_one_keeps_force() {
    let (custom, _) = id_engine("DELETE /nothing");
    let h = H::new("tag-unforced", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-b", "mine:1").unwrap();
    let lane_b = lane(&h, "lane-b");
    assert_eq!(
        call(&lane_b, "DELETE", "/images/abcd?force=1&noprune=0", None).0,
        200
    );
    let sent = delete_target(&h, "/images/abcd");
    assert!(!sent.contains("force"), "an ID delete was forced: {sent}");
    assert!(sent.contains("noprune=0"), "{sent}");
    assert_eq!(
        call(&lane_b, "DELETE", "/images/mine:1?force=1", None).0,
        200
    );
    assert!(delete_target(&h, "/images/mine:1").contains("force=1"));
}

/// An engine whose image `abcd` has the tags in `tags`; a REST build of `app:1`
/// is a cache hit that adds that tag to it; a delete of `/images/abcd` is held
/// until released, then does what the real engine does: refuses an unforced
/// delete of an image with several tags, removes everything when forced.
fn cache_engine(tags: Arc<Mutex<Vec<String>>>) -> (Handler, Arc<AtomicBool>) {
    let inner = image_engine();
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let handler: Handler = Arc::new(move |req, s| {
        let path = req.target.split('?').next().unwrap_or("");
        let has = |n: &str| tags.lock().unwrap().iter().any(|t| t == n);
        if req.method == "GET"
            && (path == "/images/abcd/json"
                || path == "/images/mine:1/json"
                || path == "/images/app:1/json")
        {
            let name = path
                .trim_start_matches("/images/")
                .trim_end_matches("/json");
            if name == "abcd" || has(name) {
                let list: Vec<String> = tags
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|t| format!("\"{t}\""))
                    .collect();
                answer(
                    s,
                    200,
                    &format!(r#"{{"Id":"sha256:abcd","RepoTags":[{}]}}"#, list.join(",")),
                );
            } else {
                answer(s, 404, "{}");
            }
            return true;
        }
        if req.method == "POST" && path == "/build" {
            tags.lock().unwrap().push("app:1".into());
            answer(s, 200, "{}");
            return true;
        }
        if req.method == "DELETE" && path == "/images/abcd" {
            let until = Instant::now() + Duration::from_secs(5);
            while !gate.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            let forced = req.target.contains("force=1");
            let mut t = tags.lock().unwrap();
            if !forced && t.len() > 1 {
                answer(
                    s,
                    409,
                    r#"{"message":"image is referenced in multiple repositories"}"#,
                );
            } else {
                t.clear();
                answer(s, 200, "[]");
            }
            return true;
        }
        inner(req, s)
    });
    (handler, release)
}

/// gpt r5: B's forced ID delete is held, A's cached build puts `app:1` on the same
/// image, the delete is released. The delete must not take A's tag with it (the
/// engine refuses because it is not forced), so B cannot then take the name.
#[test]
fn a_cached_build_alias_survives_a_forced_id_delete_and_cannot_be_taken() {
    let tags = Arc::new(Mutex::new(vec!["mine:1".to_owned()]));
    let (custom, release) = cache_engine(tags.clone());
    let h = H::new("tag-cache-delete", false, custom, |_| {});
    h.svc.ledger.set_tag("lane-b", "mine:1").unwrap();
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let b = lane_b.clone();
    let deleting = std::thread::spawn(move || call(&b, "DELETE", "/images/abcd?force=1", None).0);
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("DELETE", "/images/abcd")
        > 0));
    assert_eq!(call(&lane_a, "POST", "/build?t=app%3A1", None).0, 200);
    assert_eq!(
        h.svc.ledger.tag_owner("app:1").map(|r| r.scope),
        Some("lane-a".into())
    );
    release.store(true, Ordering::SeqCst);
    assert_eq!(
        deleting.join().unwrap(),
        409,
        "the engine must refuse the unforced delete"
    );
    assert!(
        tags.lock().unwrap().iter().any(|t| t == "app:1"),
        "the alias was removed"
    );
    assert_eq!(
        tag_call(&lane_b, "app", "1"),
        403,
        "the name was taken after the delete"
    );
    assert!(h.svc.lock_tags().is_empty());
}

/// An image with no tags (empty `RepoTags`) is deleted by ID under the image hold:
/// a `docker tag` of it waits, the delete is not forced, and the table empties.
/// (claude r5.)
#[test]
fn an_untagged_image_is_deleted_by_id_under_its_image_hold() {
    let inner = image_engine();
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let custom: Handler = Arc::new(move |req, s| {
        if req.method == "GET" && req.target == "/images/dddd/json" {
            answer(s, 200, r#"{"Id":"sha256:dddd","RepoTags":[]}"#);
            return true;
        }
        if req.method == "DELETE" && req.target.starts_with("/images/dddd") {
            let until = Instant::now() + Duration::from_secs(5);
            while !gate.load(Ordering::SeqCst) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        inner(req, s)
    });
    let h = H::new("tag-untagged", false, custom, |_| {});
    let lane_a = h.serve(&h.auth);
    let lane_b = lane(&h, "lane-b");
    let b = lane_b.clone();
    let deleting = std::thread::spawn(move || call(&b, "DELETE", "/images/dddd?force=1", None).0);
    assert!(wait_for(Duration::from_secs(3), || h
        .engine
        .saw("DELETE", "/images/dddd")
        > 0));
    assert_eq!(
        call(&lane_a, "POST", "/images/dddd/tag?repo=app&tag=1", None).0,
        403
    );
    assert!(!delete_target(&h, "/images/dddd").contains("force"));
    release.store(true, Ordering::SeqCst);
    assert_eq!(deleting.join().unwrap(), 200);
    assert!(h.svc.lock_tags().is_empty());
    assert_eq!(
        call(&lane_a, "POST", "/images/dddd/tag?repo=app&tag=1", None).0,
        201
    );
}

// --- tag named like an ID prefix (claude, fourth window) ---------------------------------------------------

/// Engine: image ID `sha256:abcdef123456...` with A's `app:1` and a tag named like
/// the ID's own prefix, `abcdef123456:latest`, owned by B. The engine's delete code
/// (`isImageIDPrefix`) treats a reference that is a prefix of the image ID as an ID
/// delete even when it is also a tag name; forced, that removes every tag.
fn collide_engine(tags: Arc<Mutex<Vec<String>>>, absent_first: Option<Arc<AtomicBool>>) -> Handler {
    let inner = image_engine();
    Arc::new(move |req, s| {
        let path = req.target.split('?').next().unwrap_or("");
        if req.method == "GET" && path == "/images/abcdef123456/json" {
            if let Some(a) = &absent_first
                && !a.swap(true, Ordering::SeqCst)
            {
                answer(s, 404, "{}");
                return true;
            }
            let list: Vec<String> = tags
                .lock()
                .unwrap()
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect();
            answer(
                s,
                200,
                &format!(
                    r#"{{"Id":"sha256:abcdef1234560000","RepoTags":[{}]}}"#,
                    list.join(",")
                ),
            );
            return true;
        }
        if req.method == "DELETE" && path == "/images/abcdef123456" {
            let forced = req.target.contains("force=1");
            let mut t = tags.lock().unwrap();
            if !forced && t.len() > 1 {
                answer(s, 409, r#"{"message":"multiple"}"#);
            } else {
                t.clear();
                answer(s, 200, "[]");
            }
            return true;
        }
        inner(req, s)
    })
}

#[test]
fn a_tag_named_like_the_id_prefix_cannot_force_delete_the_image() {
    let tags = Arc::new(Mutex::new(vec![
        "app:1".to_owned(),
        "abcdef123456:latest".to_owned(),
    ]));
    let h = H::new(
        "w4-collide",
        false,
        collide_engine(tags.clone(), None),
        |_| {},
    );
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    let lane_b = lane(&h, "lane-b");
    // B puts a name on A's image through the real route; that is allowed.
    assert_eq!(tag_call_from(&lane_b, "app:1", "abcdef123456"), 201);
    assert_eq!(
        h.svc
            .ledger
            .tag_owner("abcdef123456:latest")
            .map(|r| r.scope),
        Some("lane-b".into())
    );
    let st = call(&lane_b, "DELETE", "/images/abcdef123456?force=1", None).0;
    let left = tags.lock().unwrap().clone();
    assert!(
        left.iter().any(|t| t == "app:1"),
        "A's tag was removed (status {st}); sent: {}",
        delete_target(&h, "/images/abcdef123456")
    );
}

/// Same, but the reference is absent when marsh looks and present when the engine does.
#[test]
fn an_absent_id_looking_name_is_not_forced() {
    let tags = Arc::new(Mutex::new(vec!["app:1".to_owned(), "mine:1".to_owned()]));
    let h = H::new(
        "w4-absent",
        false,
        collide_engine(tags.clone(), Some(Arc::new(AtomicBool::new(false)))),
        |_| {},
    );
    h.svc.ledger.set_tag("lane-a", "app:1").unwrap();
    h.svc.ledger.set_tag("lane-b", "mine:1").unwrap();
    let lane_b = lane(&h, "lane-b");
    let st = call(&lane_b, "DELETE", "/images/abcdef123456?force=1", None).0;
    let left = tags.lock().unwrap().clone();
    assert!(
        left.iter().any(|t| t == "app:1"),
        "A's tag was removed (status {st}); sent: {}",
        delete_target(&h, "/images/abcdef123456")
    );
}

fn tag_call_from(sock: &Path, src: &str, repo: &str) -> u16 {
    call(
        sock,
        "POST",
        &format!("/images/{src}/tag?repo={repo}"),
        None,
    )
    .0
}

/// A reply head that arrives in pieces, each inside the per-read limit, is cut
/// off by the whole-call deadline (a 204 returns no body to check afterwards).
#[test]
fn a_drip_fed_response_head_obeys_the_whole_call_deadline() {
    let h = H::new(
        "dripheadw4",
        false,
        Arc::new(|req, s| {
            if req.method != "DELETE" {
                return false;
            }
            for part in [
                "HTTP/1.1 204 No Content\r\n",
                "Api-Version: 1.45\r\n",
                "X-One: 1\r\n",
                "\r\n",
            ] {
                if s.write_all(part.as_bytes()).is_err() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            true
        }),
        |cfg| cfg.up_progress = Duration::from_millis(150),
    );
    let sock = h.serve(&h.auth);
    h.svc.ledger.claim_volume("lane-a", "att1", "data").unwrap();
    let started = Instant::now();
    let (status, _) = call(&sock, "DELETE", "/volumes/data", None);
    assert_eq!(status, 502, "accepted a head after {:?}", started.elapsed());
    assert!(h.svc.volume_fenced("data"));
}

/// Chunked framing delivered a piece at a time is bounded by the same deadline.
#[test]
fn a_drip_fed_chunked_reply_obeys_the_whole_call_deadline() {
    let h = H::new(
        "dripchunkw4",
        false,
        Arc::new(|req, s| {
            if req.method != "DELETE" {
                return false;
            }
            for part in [
                "HTTP/1.1 409 X\r\nTransfer-Encoding: chunked\r\n\r\n",
                "5\r\n",
                "hello\r\n",
                "1\r\n",
                "!\r\n",
                "0\r\n\r\n",
            ] {
                if s.write_all(part.as_bytes()).is_err() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            true
        }),
        |cfg| cfg.up_progress = Duration::from_millis(250),
    );
    let sock = h.serve(&h.auth);
    h.svc.ledger.claim_volume("lane-a", "att1", "data").unwrap();
    let started = Instant::now();
    let (status, _) = call(&sock, "DELETE", "/volumes/data", None);
    assert_eq!(
        status,
        502,
        "accepted a reply after {:?}",
        started.elapsed()
    );
}

#[path = "bk_watch_tests.rs"]
mod bk_watch_tests;
