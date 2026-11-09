//! E3 dev harness: runs the filtered endpoint (and a few probes) inside a real
//! engine container. Not part of any shipped artifact (`dev-harness` feature).
//!
//!   e3-harness serve  --listen SOCK --upstream SOCK --root DIR --workspace W --attempt A
//!                     [--state DIR] [--break a,b] [--registry host,..] [--verbose]
//!   e3-harness restore --state DIR           (`pins::restore_all_in` + sockets; nonzero
//!                                            exit when restoration cannot run)
//!   serve limits for boundary tests: --max-memory B --max-nano-cpus N --max-pids N
//!   serve deadlines: --hijack-idle-ms N --progress-ms N
//!   e3-harness record --listen SOCK --upstream SOCK --out FILE
//!   e3-harness pin-probe --root DIR --rel REL --binds DIR
// A dev tool, not a shipped artifact: pedantic style lints are off.
#![allow(clippy::pedantic, clippy::explicit_counter_loop)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use marsh_services::ledger::Paths;
use marsh_services::policy::Authority;
use marsh_services::policy::endpoint::{Config, FilteredEndpoint, Services};

fn args() -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    let a: Vec<String> = std::env::args().skip(2).collect();
    let mut i = 0;
    while i < a.len() {
        if let Some(k) = a[i].strip_prefix("--") {
            if i + 1 < a.len() && !a[i + 1].starts_with("--") {
                m.insert(k.to_string(), a[i + 1].clone());
                i += 2;
                continue;
            }
            m.insert(k.to_string(), String::new());
        }
        i += 1;
    }
    m
}

fn start_scope(svc: &Arc<Services>, sock: &str, auth: Authority) {
    let _ = std::fs::remove_file(sock);
    let l = UnixListener::bind(sock).expect("bind");
    let _ = std::fs::set_permissions(sock, std::os::unix::fs::PermissionsExt::from_mode(0o666));
    eprintln!("serving {sock} as {auth:?}");
    let svc = Arc::clone(svc);
    std::thread::spawn(move || {
        let e = FilteredEndpoint::serve_with(&svc, &l, &auth);
        eprintln!("scope ended: {e:?}");
    });
}

fn paths(state: &str) -> Paths {
    let s = PathBuf::from(state);
    let d = Paths::default();
    Paths {
        ledger: s.join("ledger.json"),
        bind_root: d.bind_root,
        socket_root: d.socket_root,
    }
}

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let a = args();
    #[cfg(feature = "test-breaks")]
    if let Some(b) = a.get("break") {
        marsh_services::breaks::init(
            &b.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>(),
        );
    }
    let get = |k: &str| a.get(k).cloned().unwrap_or_else(|| panic!("missing --{k}"));
    match cmd.as_str() {
        "serve" => {
            let state = a
                .get("state")
                .cloned()
                .unwrap_or_else(|| "/var/lib/marsh-e3".into());
            let mut cfg = Config {
                paths: paths(&state),
                verbose: a.contains_key("verbose"),
                ..Config::default()
            };
            if let Some(r) = a.get("registry") {
                cfg.registry_allow = Some(r.split(',').map(str::to_string).collect());
            }
            if let Some(r) = a.get("socket-alias") {
                cfg.socket_aliases = r.split(',').map(str::to_string).collect();
            }
            if let Some(n) = a.get("max-attempt") {
                cfg.max_conns_attempt = n.parse().unwrap();
            }
            if let Some(n) = a.get("max-project") {
                cfg.max_conns_project = n.parse().unwrap();
            }
            if let Some(n) = a.get("max-total") {
                cfg.max_conns_total = n.parse().unwrap();
            }
            if let Some(n) = a.get("max-memory") {
                cfg.limits.max_memory = n.parse().unwrap();
            }
            if let Some(n) = a.get("max-nano-cpus") {
                cfg.limits.max_nano_cpus = n.parse().unwrap();
            }
            if let Some(n) = a.get("max-pids") {
                cfg.limits.max_pids = n.parse().unwrap();
            }
            if let Some(n) = a.get("hijack-idle-ms") {
                cfg.hijack_idle = std::time::Duration::from_millis(n.parse().unwrap());
            }
            if let Some(n) = a.get("progress-ms") {
                cfg.http.progress = std::time::Duration::from_millis(n.parse().unwrap());
            }
            if let Some(r) = a.get("socket-root") {
                cfg.paths.socket_root = PathBuf::from(r);
            }
            let upstream = PathBuf::from(get("upstream"));
            let svc = Services::new(cfg, &upstream).expect("services");
            // The engine must not start on a ledger that cannot be restored.
            let rep = match marsh_services::pins::restore_all_in(&svc.cfg.paths, &svc.ledger) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("restore failed, refusing to serve: {e}");
                    std::process::exit(1);
                }
            };
            eprintln!(
                "restored {} pins, {} failed: {:?}; blocked containers: {:?}; {} socket paths reserved",
                rep.restored.len(),
                rep.failed.len(),
                rep.failed,
                rep.blocked_containers,
                rep.sockets_reserved
            );
            let n = svc.restore_sockets();
            eprintln!("restored {n} sockets");
            let mut scopes: Vec<(String, Authority)> = Vec::new();
            if a.contains_key("listen") {
                scopes.push((
                    get("listen"),
                    Authority {
                        project_root: PathBuf::from(get("root")),
                        workspace: get("workspace"),
                        attempt: get("attempt"),
                    },
                ));
            }
            for (l, auth) in scopes {
                start_scope(&svc, &l, auth);
            }
            // Scopes can also be added while running: JSON files in --scope-dir.
            let dir = a.get("scope-dir").cloned().unwrap_or_default();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut freezes: std::collections::HashSet<String> = std::collections::HashSet::new();
            loop {
                // A `NAME.freeze` file holds a capture freeze on scope NAME's
                // workspace while it exists (CAP-11 stand-in for S3).
                if !dir.is_empty()
                    && let Ok(rd) = std::fs::read_dir(&dir)
                {
                    let present: Vec<String> = rd
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| n.ends_with(".freeze"))
                        .collect();
                    for n in &present {
                        if !freezes.insert(n.clone()) {
                            continue;
                        }
                        // The freeze may wait for in-flight mutations, so it
                        // is taken off this loop. `NAME.frozen` appears once
                        // it is held (nothing was in flight at that moment).
                        let (svc, dir, n) = (Arc::clone(&svc), PathBuf::from(&dir), n.clone());
                        std::thread::spawn(move || {
                            let ws = n.trim_end_matches(".freeze").to_string();
                            let guard = svc.freeze_scope(&ws);
                            let marker = dir.join(format!("{ws}.frozen"));
                            let _ = std::fs::write(&marker, b"held");
                            while dir.join(&n).exists() {
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                            drop(guard);
                            let _ = std::fs::remove_file(&marker);
                        });
                    }
                    freezes.retain(|n| present.contains(n));
                }
                if !dir.is_empty()
                    && let Ok(rd) = std::fs::read_dir(&dir)
                {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if !name.ends_with(".scope") || seen.contains(&name) {
                            continue;
                        }
                        let Ok(txt) = std::fs::read_to_string(e.path()) else {
                            continue;
                        };
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
                            continue;
                        };
                        let g =
                            |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                        seen.insert(name);
                        start_scope(
                            &svc,
                            &g("listen"),
                            Authority {
                                project_root: PathBuf::from(g("root")),
                                workspace: g("workspace"),
                                attempt: g("attempt"),
                            },
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
        "restore" => {
            let p = paths(&get("state"));
            let ledger = marsh_services::ledger::Ledger::open(&p.ledger).expect("ledger");
            match marsh_services::pins::restore_all_in(&p, &ledger) {
                Ok(rep) => println!(
                    "restored={} failed={:?} blocked={:?} sockets={}",
                    rep.restored.len(),
                    rep.failed,
                    rep.blocked_containers,
                    rep.sockets_reserved
                ),
                Err(e) => {
                    println!("restore cannot run: {e}");
                    std::process::exit(1);
                }
            }
        }
        "record" => record(&get("listen"), &get("upstream"), &get("out")),
        "pin-probe" => pin_probe(&get("root"), &get("rel"), &get("binds")),
        _ => {
            eprintln!("usage: e3-harness serve|restore|record|pin-probe ...");
            std::process::exit(2);
        }
    }
}

/// A transparent recording proxy: every byte in both directions goes to the
/// file as `> <hex>` / `< <hex>` lines per connection.
fn record(listen: &str, upstream: &str, out: &str) {
    let _ = std::fs::remove_file(listen);
    let l = UnixListener::bind(listen).expect("bind");
    let file = Arc::new(Mutex::new(std::fs::File::create(out).expect("out")));
    let mut n = 0u32;
    for s in l.incoming().flatten() {
        n += 1;
        let id = n;
        let up = UnixStream::connect(upstream).expect("dial");
        let (mut c_r, mut u_w) = (s.try_clone().unwrap(), up.try_clone().unwrap());
        let (mut u_r, mut c_w) = (up, s);
        let f1 = Arc::clone(&file);
        let f2 = Arc::clone(&file);
        std::thread::spawn(move || {
            let mut b = [0u8; 16384];
            while let Ok(n) = c_r.read(&mut b) {
                if n == 0 {
                    break;
                }
                let hex: String = b[..n].iter().map(|x| format!("{x:02x}")).collect();
                let _ = writeln!(f1.lock().unwrap(), "{id} > {hex}");
                if u_w.write_all(&b[..n]).is_err() {
                    break;
                }
            }
            let _ = u_w.shutdown(std::net::Shutdown::Write);
        });
        std::thread::spawn(move || {
            let mut b = [0u8; 16384];
            while let Ok(n) = u_r.read(&mut b) {
                if n == 0 {
                    break;
                }
                let hex: String = b[..n].iter().map(|x| format!("{x:02x}")).collect();
                let _ = writeln!(f2.lock().unwrap(), "{id} < {hex}");
                if c_w.write_all(&b[..n]).is_err() {
                    break;
                }
            }
            let _ = c_w.shutdown(std::net::Shutdown::Write);
        });
    }
}

fn pin_probe(root: &str, rel: &str, binds: &str) {
    use marsh_services::pins::{PinManager, ProjectRoot};
    let mgr = PinManager::new(Path::new(binds));
    let pr = ProjectRoot::open(Path::new(root)).expect("root");
    let src = if rel.is_empty() {
        root.to_string()
    } else {
        format!("{}/{rel}", root.trim_end_matches('/'))
    };
    match mgr.pin(&pr, "probe", &src, false, false, &[]) {
        Ok(p) => println!(
            "pinned id={} path={} rel={:?} ro={} subs={:?}",
            p.id,
            p.path.display(),
            p.rec.rel,
            p.rec.read_only,
            p.rec.subs
        ),
        Err(e) => println!("pin failed: {e}"),
    }
}
