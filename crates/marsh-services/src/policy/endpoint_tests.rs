//! The endpoint against a scripted engine, in process (no VM): the behaviors of
//! the E3 round-2 fixes that need a connection, a clock or a failing disk.
//!
//! Each test states what must hold. `e3/witness.sh` runs every test again with
//! the break mode that reintroduces its defect and requires the test to fail
//! (`E3_TEST_BREAKS`, read only by the unit-test build, see `breaks.rs`).

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Authority, Config, Services};
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
            .map(|r| String::from_utf8_lossy(&r.body).into_owned())
            .unwrap_or_default()
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
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
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
#[test]
fn a_silent_hijacked_stream_is_closed_at_the_idle_limit() {
    let rel = Arc::new(AtomicBool::new(false));
    let r2 = rel.clone();
    let h = H::new(
        "silent",
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
        |c| c.hijack_idle = Duration::from_millis(400),
    );
    let sock = h.serve(&h.auth);
    let mut c = attach(&h, &sock);
    c.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
    let mut b = [0u8; 16];
    let r = c.read(&mut b);
    r2.store(true, Ordering::SeqCst);
    assert!(matches!(r, Ok(0)), "the idle stream was not closed: {r:?}");
    assert!(wait_for(Duration::from_secs(2), || h
        .svc
        .open_connections()
        == 0));
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
    c3.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
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
    assert!(r.is_err(), "a separate project was refused: {r:?}");
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

    let tmp = h.dir.join("ledger.json.tmp");
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

/// DA1-06: per-attempt and per-root connection counts go when their last
/// connection does; the endpoint still serves afterwards.
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
            let (_, attempts, roots) = h.svc.tracked();
            (attempts, roots) == (0, 0)
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
