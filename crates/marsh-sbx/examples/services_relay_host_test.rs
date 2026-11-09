//! Test-only driver for the daemon-side services relay (E1). Not a product entry
//! point: there is no job endpoint until E2, so this process stands in for the
//! daemon. It builds the same pieces `marshd` does (`StockSbx` with an ownership
//! map, S1's `ensure_services_vm`, S2's engine generation, E1's relay), then
//! answers one JSON command per line on stdin with one JSON line on stdout.
//!
//! ```text
//! services_relay_host_test --home DIR --project DIR --artifacts DIR
//!     --shell-image REF --shell-binary PATH [--sbx sbx] [--disk-size 20g]
//!     [--max-attempt N] [--max-project N] [--max-daemon N]
//!     [--stall-ms N] [--test-breaks-from-env]
//! ```
//!
//! `--artifacts` holds `svc-engine.tar`, `svc-engine-image.json` and
//! `marsh-linux-arm64`. Break modes: `MARSH_TEST_BREAK=a,b` in this process's
//! environment together with `--test-breaks-from-env` (a build with
//! `test-breaks` only), exactly as for `marshd`.
//!
//! Commands: `up`, `status`, `docker`, `tcp-echo`, `tcp-send`, `hold`,
//! `release`, `capture`, `drop`, `list`, `ports`, `pg-latency`, `stall-test`,
//! `close`, `exit`.

#![allow(
    clippy::format_push_string,
    clippy::format_collect,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

use marsh_contracts::OciImage;
use marsh_runtime::{CommandRunner, SystemCommandRunner};
use marsh_sbx::services_relay::{
    AttemptScope, BASE_WORKSPACE, DaemonBudget, Limits, Relay, ServicesRelays, Stream,
};
use marsh_sbx::{ServicesVmSpec, StockSbx};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Driver {
    relays: ServicesRelays,
    /// One spec per `--project`; commands pick one with `"p"` (default 0).
    specs: Vec<ServicesVmSpec>,
    budget: Arc<DaemonBudget>,
    grants: marsh_sbx::InternetGrants,
    held: BTreeMap<String, Vec<Stream>>,
}

fn spec<'a>(driver: &'a Driver, command: &Value) -> Result<&'a ServicesVmSpec, String> {
    let index = usize::try_from(number(command, "p", 0)).map_err(|e| e.to_string())?;
    driver
        .specs
        .get(index)
        .ok_or_else(|| "no such project".into())
}

fn field<'a>(command: &'a Value, name: &str) -> Option<&'a str> {
    command.get(name).and_then(Value::as_str)
}

fn number(command: &Value, name: &str, default: u64) -> u64 {
    command.get(name).and_then(Value::as_u64).unwrap_or(default)
}

fn scope(driver: &Driver, command: &Value) -> Result<AttemptScope, String> {
    AttemptScope::new(
        spec(driver, command)?.project.clone(),
        field(command, "workspace").unwrap_or(BASE_WORKSPACE),
        field(command, "attempt").unwrap_or("a1"),
    )
    .map_err(|error| error.to_string())
}

fn relay(driver: &Driver, command: &Value) -> Result<Relay, String> {
    driver
        .relays
        .get(&spec(driver, command)?.project)
        .map(|entry| entry.relay().clone())
        .map_err(|error| error.to_string())
}

/// One HTTP/1.1 round trip over a docker channel. The daemon never parses
/// HTTP; this is the test's own client.
fn http(
    stream: &mut Stream,
    method: &str,
    path: &str,
    body: Option<&str>,
    upgrade: bool,
) -> Result<(u16, Vec<u8>), String> {
    let body = body.unwrap_or("");
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: docker\r\n");
    request.push_str(if upgrade {
        "Connection: Upgrade\r\nUpgrade: tcp\r\n"
    } else {
        "Connection: close\r\n"
    });
    if !body.is_empty() {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream
        .write_all(request.as_bytes())
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no response head")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("bad status line")?;
    let rest = raw[split + 4..].to_vec();
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    Ok((status, if chunked { dechunk(&rest) } else { rest }))
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&data[..line_end]).trim(), 16)
            .unwrap_or(0);
        data = &data[line_end + 2..];
        if size == 0 || data.len() < size {
            break;
        }
        out.extend_from_slice(&data[..size]);
        data = data.get(size + 2..).unwrap_or_default();
    }
    out
}

fn pattern_at(offset: usize) -> u8 {
    ((offset * 31 + offset / 251) % 256) as u8
}

fn hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Sends `kib` KiB of a deterministic pattern through a channel to an echo
/// server and hashes what comes back, both streamed (the test client's own
/// memory stays small, so the daemon's can be watched).
fn tcp_echo(driver: &Driver, command: &Value) -> Result<Value, String> {
    let relay = relay(driver, command)?;
    let port = u16::try_from(number(command, "port", 0)).map_err(|e| e.to_string())?;
    let bytes = usize::try_from(number(command, "kib", 1024) * 1024).map_err(|e| e.to_string())?;
    let scope = scope(driver, command)?;
    let started = Instant::now();
    let stream = relay.open_tcp(&scope, port).map_err(|e| e.to_string())?;
    let writer = {
        let mut stream = stream.clone();
        std::thread::spawn(move || {
            let mut hash = Sha256::new();
            let mut offset = 0;
            let mut chunk = vec![0_u8; 256 * 1024];
            while offset < bytes {
                let take = chunk.len().min(bytes - offset);
                for (i, byte) in chunk[..take].iter_mut().enumerate() {
                    *byte = pattern_at(offset + i);
                }
                hash.update(&chunk[..take]);
                stream
                    .write_all(&chunk[..take])
                    .map_err(|e| e.to_string())?;
                offset += take;
            }
            let _ = stream.shutdown_write();
            Ok::<String, String>(hex(&hash.finalize()))
        })
    };
    let mut reader = stream.clone();
    let mut hash = Sha256::new();
    let mut received = 0_usize;
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        received += n;
    }
    let sent = writer.join().map_err(|_| "writer panicked")??;
    let got = hex(&hash.finalize());
    Ok(json!({
        "ok": sent == got && received == bytes,
        "sent": bytes, "received": received,
        "sha_sent": sent, "sha_received": got,
        "ms": started.elapsed().as_millis(),
    }))
}

/// How long opening a channel takes (and closing it), without any traffic.
fn open_latency(driver: &Driver, command: &Value) -> Result<Value, String> {
    let relay = relay(driver, command)?;
    let scope = scope(driver, command)?;
    let rounds = number(command, "n", 50);
    let port = u16::try_from(number(command, "port", 0)).map_err(|e| e.to_string())?;
    let tcp = field(command, "kind") == Some("tcp");
    let mut micros = Vec::new();
    for _ in 0..rounds {
        let started = Instant::now();
        let stream = if tcp {
            relay.open_tcp(&scope, port)
        } else {
            relay.open_docker(&scope)
        }
        .map_err(|e| e.to_string())?;
        micros.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
        drop(stream);
    }
    micros.sort_unstable();
    Ok(
        json!({"n": micros.len(), "p50_us": micros[micros.len() / 2], "min_us": micros[0],
        "max_us": micros[micros.len() - 1]}),
    )
}

/// `docker exec` through the filtered endpoint: create, start with an upgrade,
/// demultiplex the output.
fn exec(driver: &Driver, command: &Value) -> Result<Value, String> {
    let relay = relay(driver, command)?;
    let scope = scope(driver, command)?;
    let container = field(command, "container").ok_or("no container")?;
    let body = json!({"AttachStdout": true, "AttachStderr": true, "Cmd": command["cmd_line"]});
    let mut stream = relay.open_docker(&scope).map_err(|e| e.to_string())?;
    let (status, created) = http(
        &mut stream,
        "POST",
        &format!("/containers/{container}/exec"),
        Some(&body.to_string()),
        false,
    )?;
    if status != 201 {
        return Ok(json!({"status": status, "body": String::from_utf8_lossy(&created)}));
    }
    let id = serde_json::from_slice::<Value>(&created)
        .ok()
        .and_then(|v| v["Id"].as_str().map(str::to_owned))
        .ok_or("no exec id")?;
    let mut stream = relay.open_docker(&scope).map_err(|e| e.to_string())?;
    let (status, raw) = http(
        &mut stream,
        "POST",
        &format!("/exec/{id}/start"),
        Some(r#"{"Detach":false,"Tty":false}"#),
        true,
    )?;
    let (mut out, mut err, mut at) = (Vec::new(), Vec::new(), 0);
    while at + 8 <= raw.len() {
        let size =
            u32::from_be_bytes([raw[at + 4], raw[at + 5], raw[at + 6], raw[at + 7]]) as usize;
        let end = (at + 8 + size).min(raw.len());
        if raw[at] == 2 { &mut err } else { &mut out }.extend_from_slice(&raw[at + 8..end]);
        at = end;
    }
    Ok(
        json!({"status": status, "stdout": String::from_utf8_lossy(&out), "stderr": String::from_utf8_lossy(&err)}),
    )
}

/// Opens many channels at once and keeps them (the bounds scenarios).
fn hold(driver: &mut Driver, command: &Value) -> Result<Value, String> {
    let relay = relay(driver, command)?;
    let count = number(command, "count", 1);
    let kind = field(command, "kind").unwrap_or("docker");
    let key = field(command, "attempt").unwrap_or("a1").to_owned();
    let mut opened = 0;
    let mut first_error = None;
    for _ in 0..count {
        let scope = scope(driver, command)?;
        let result = if kind == "tcp" {
            let port = u16::try_from(number(command, "port", 0)).map_err(|e| e.to_string())?;
            relay.open_tcp(&scope, port)
        } else {
            relay.open_docker(&scope)
        };
        match result {
            Ok(stream) => {
                opened += 1;
                driver.held.entry(key.clone()).or_default().push(stream);
            }
            Err(error) => {
                first_error = Some(error.to_string());
                break;
            }
        }
    }
    Ok(
        json!({"opened": opened, "refused_with": first_error, "open_channels": relay.open_channels(),
        "daemon_budget": driver.budget.in_use()}),
    )
}

/// The PG wire protocol's simplest select loop (trust auth): the latency of one
/// request/response over the relay, for `seconds`. With `connect_each` every
/// round opens a channel, starts a session, runs one query and closes (the
/// `pgbench -C` shape). The query is pgbench's select-only statement with a
/// random account.
fn pg_latency(driver: &Driver, command: &Value) -> Result<Value, String> {
    let relay = relay(driver, command)?;
    let port = u16::try_from(number(command, "port", 0)).map_err(|e| e.to_string())?;
    let seconds = number(command, "seconds", 10);
    let connect_each = command.get("connect_each").and_then(Value::as_bool) == Some(true);
    let scope = scope(driver, command)?;
    let accounts = number(command, "accounts", 100_000);
    let mut random = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next_aid = move || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random % accounts + 1
    };
    let session = |stream: &mut Stream| -> Result<(), String> {
        let mut startup = Vec::new();
        startup.extend_from_slice(&196_608_u32.to_be_bytes());
        startup.extend_from_slice(b"user\0postgres\0database\0postgres\0\0");
        let mut message = (u32::try_from(startup.len() + 4).map_err(|e| e.to_string())?)
            .to_be_bytes()
            .to_vec();
        message.extend_from_slice(&startup);
        stream.write_all(&message).map_err(|e| e.to_string())?;
        read_until_ready(stream)
    };
    let query = |stream: &mut Stream, aid: u64| -> Result<(), String> {
        let text = format!("SELECT abalance FROM pgbench_accounts WHERE aid = {aid}");
        let mut q = vec![b'Q'];
        q.extend_from_slice(
            &u32::try_from(text.len() + 5)
                .map_err(|e| e.to_string())?
                .to_be_bytes(),
        );
        q.extend_from_slice(text.as_bytes());
        q.push(0);
        stream.write_all(&q).map_err(|e| e.to_string())?;
        read_until_ready(stream)
    };
    let mut micros = Vec::new();
    let mut stream = relay.open_tcp(&scope, port).map_err(|e| e.to_string())?;
    session(&mut stream)?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        let started = Instant::now();
        if connect_each {
            drop(stream);
            stream = relay.open_tcp(&scope, port).map_err(|e| e.to_string())?;
            session(&mut stream)?;
        }
        query(&mut stream, next_aid())?;
        micros.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    micros.sort_unstable();
    let at = |q: f64| micros[((micros.len() as f64 * q) as usize).min(micros.len() - 1)];
    Ok(
        json!({"n": micros.len(), "p50_us": at(0.5), "p95_us": at(0.95), "min_us": micros[0],
        "mean_us": micros.iter().sum::<u64>() / micros.len() as u64}),
    )
}

/// One bulk upload to a sink while small round trips run on other channels:
/// the round trips must not wait for the upload.
fn fairness_test(driver: &Driver, command: &Value) -> Result<Value, String> {
    let relay = relay(driver, command)?;
    let sink = u16::try_from(number(command, "sink_port", 0)).map_err(|e| e.to_string())?;
    let echo = u16::try_from(number(command, "echo_port", 0)).map_err(|e| e.to_string())?;
    let mib = number(command, "mib", 256);
    let with_probe = command.get("probe").and_then(Value::as_bool) != Some(false);
    let scope = scope(driver, command)?;
    let mut upload = relay.open_tcp(&scope, sink).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let uploader = std::thread::spawn(move || {
        let chunk = vec![7_u8; 1 << 20];
        for _ in 0..mib {
            if upload.write_all(&chunk).is_err() {
                return None;
            }
        }
        let _ = upload.shutdown_write();
        Some(started.elapsed().as_millis())
    });
    let mut rounds = Vec::new();
    while with_probe && !uploader.is_finished() && rounds.len() < 200 {
        // The echo server takes one connection at a time and relistens after
        // each; a connection that lands in between sees end of stream, so a
        // round is retried (its time includes the retries).
        let begun = Instant::now();
        let mut tries = 0;
        loop {
            let mut z = relay.open_tcp(&scope, echo).map_err(|e| e.to_string())?;
            z.write_all(b"z").map_err(|e| e.to_string())?;
            let mut one = [0_u8; 1];
            match z.read_exact(&mut one) {
                Ok(()) => break,
                Err(error) if tries < 5 => {
                    tries += 1;
                    let _ = error;
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        rounds.push(begun.elapsed().as_millis());
        std::thread::sleep(Duration::from_millis(100));
    }
    let probing_ended_ms = started.elapsed().as_millis();
    let upload_ms = uploader.join().map_err(|_| "uploader panicked")?;
    rounds.sort_unstable();
    Ok(
        json!({"upload_mib": mib, "upload_ms": upload_ms, "probing_ended_ms": probing_ended_ms,
        "z_rounds": rounds.len(), "z_max_ms": rounds.last(), "z_p50_ms": rounds.get(rounds.len() / 2)}),
    )
}

/// Reads ticks (`date +%s` lines) from a channel in the background and appends
/// each, with the local time it arrived, to a file: the evidence that nothing
/// arrives after the daemon is killed.
fn tcp_ticks(driver: &Driver, command: &Value) -> Result<Value, String> {
    let relay = relay(driver, command)?;
    let port = u16::try_from(number(command, "port", 0)).map_err(|e| e.to_string())?;
    let log = PathBuf::from(field(command, "log").ok_or("no log")?);
    let mut stream = relay
        .open_tcp(&scope(driver, command)?, port)
        .map_err(|e| e.to_string())?;
    stream.write_all(b"go\n").map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .expect("tick log");
        let mut buffer = [0_u8; 256];
        while let Ok(n) = stream.read(&mut buffer) {
            if n == 0 {
                let _ = writeln!(file, "EOF {}", now_ms());
                break;
            }
            let _ = writeln!(
                file,
                "TICK {} {}",
                now_ms(),
                String::from_utf8_lossy(&buffer[..n]).trim()
            );
        }
        let _ = writeln!(file, "END {}", now_ms());
    });
    Ok(json!({"following": true}))
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn read_until_ready(stream: &mut Stream) -> Result<(), String> {
    loop {
        let mut head = [0_u8; 5];
        stream.read_exact(&mut head).map_err(|e| e.to_string())?;
        let length = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let mut body = vec![0_u8; length - 4];
        stream.read_exact(&mut body).map_err(|e| e.to_string())?;
        match head[0] {
            b'Z' => return Ok(()),
            b'E' => {
                return Err(format!(
                    "postgres error: {}",
                    String::from_utf8_lossy(&body)
                ));
            }
            _ => {}
        }
    }
}

/// A channel that is never read must not stall another: X is opened to a
/// firehose port and never read; Y does round trips on an echo port.
fn stall_test(driver: &Driver, command: &Value) -> Result<Value, String> {
    let relay = relay(driver, command)?;
    let firehose = u16::try_from(number(command, "firehose_port", 0)).map_err(|e| e.to_string())?;
    let echo = u16::try_from(number(command, "echo_port", 0)).map_err(|e| e.to_string())?;
    let seconds = number(command, "seconds", 8);
    let scope = scope(driver, command)?;
    let bytes_before = relay.stats().bytes_in;
    let x = relay
        .open_tcp(&scope, firehose)
        .map_err(|e| e.to_string())?;
    let mut y = relay.open_tcp(&scope, echo).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut worst = 0_u128;
    let mut rounds = 0_u64;
    let mut buffer = [0_u8; 64];
    let mut x_probe = x.clone();
    // X must be started (the firehose sends on connect only after a byte).
    x_probe.write_all(b"go\n").map_err(|e| e.to_string())?;
    while Instant::now() < deadline {
        let started = Instant::now();
        y.write_all(b"ping\n").map_err(|e| e.to_string())?;
        let mut got = 0;
        while got < 5 {
            got += y.read(&mut buffer[got..]).map_err(|e| e.to_string())?;
        }
        worst = worst.max(started.elapsed().as_micros());
        rounds += 1;
        std::thread::sleep(Duration::from_millis(20));
    }
    let after = relay.stats().bytes_in;
    let echoed = rounds * 5;
    drop(x);
    Ok(json!({"rounds": rounds, "worst_round_trip_us": worst,
        "bytes_buffered_for_unread_channel": after.saturating_sub(bytes_before).saturating_sub(echoed)}))
}

#[allow(clippy::too_many_lines)]
fn handle(driver: &mut Driver, command: &Value) -> Result<Value, String> {
    match field(command, "cmd").ok_or("no cmd")? {
        "up" => {
            let started = Instant::now();
            let entry = driver
                .relays
                .ensure(spec(driver, command)?, &driver.grants)?;
            Ok(
                json!({"vm": entry.vm(), "boot_id": entry.generation().boot_id,
                "engine_id": entry.generation().engine_id,
                "agent_breaks": entry.relay().agent_breaks(),
                "agent_pid": entry.relay().agent_pid(),
                "ms": started.elapsed().as_millis()}),
            )
        }
        "status" => {
            let view = driver.relays.view(&spec(driver, command)?.project);
            let stats = relay(driver, command)
                .ok()
                .map(|r| format!("{:?}", r.stats()));
            Ok(
                json!({"state": view.state, "reason": view.reason, "channels": view.channels,
                "engine_id": view.generation.map(|g| g.engine_id), "agent_pid": view.agent_pid,
                "stats": stats, "daemon_budget": driver.budget.in_use()}),
            )
        }
        "docker" => {
            let relay = relay(driver, command)?;
            let mut stream = relay
                .open_docker(&scope(driver, command)?)
                .map_err(|e| e.to_string())?;
            let (status, body) = http(
                &mut stream,
                field(command, "method").unwrap_or("GET"),
                field(command, "path").ok_or("no path")?,
                field(command, "body"),
                false,
            )?;
            let text = String::from_utf8_lossy(&body[..body.len().min(1 << 20)]).into_owned();
            Ok(json!({"status": status, "body": text, "bytes": body.len()}))
        }
        "exec" => exec(driver, command),
        "restore-prepare" | "restore-up" => {
            let relay = relay(driver, command)?;
            let project = spec(driver, command)?
                .project
                .to_string_lossy()
                .into_owned();
            let lane = field(command, "lane_dir").ok_or("no lane_dir")?;
            let workspace = field(command, "workspace").unwrap_or("lane-1");
            let fork = field(command, "fork").unwrap_or("f1");
            let reply = if field(command, "cmd") == Some("restore-prepare") {
                relay.restore_prepare(
                    field(command, "id").ok_or("no id")?,
                    workspace,
                    &project,
                    None,
                    lane,
                    fork,
                )
            } else {
                relay.restore_up(workspace, &project, None, lane, fork)
            }
            .map_err(|e| e.to_string())?;
            Ok(json!({"result": reply.result}))
        }
        "tcp-echo" => tcp_echo(driver, command),
        "open-latency" => open_latency(driver, command),
        "tcp-send" => {
            let relay = relay(driver, command)?;
            let port = u16::try_from(number(command, "port", 0)).map_err(|e| e.to_string())?;
            let mut stream = relay
                .open_tcp(&scope(driver, command)?, port)
                .map_err(|e| e.to_string())?;
            stream
                .write_all(field(command, "text").unwrap_or("hello\n").as_bytes())
                .map_err(|e| e.to_string())?;
            let _ = stream.shutdown_write();
            let mut answer = Vec::new();
            let _ = stream.read_to_end(&mut answer);
            Ok(json!({"answer": String::from_utf8_lossy(&answer)}))
        }
        "hold" => hold(driver, command),
        "release" => {
            let key = field(command, "attempt").unwrap_or("a1");
            let count = driver.held.remove(key).map_or(0, |streams| streams.len());
            Ok(json!({"released": count, "daemon_budget": driver.budget.in_use()}))
        }
        "capture" => {
            let reply = relay(driver, command)?
                .capture(
                    field(command, "id").ok_or("no id")?,
                    field(command, "workspace").unwrap_or(BASE_WORKSPACE),
                    number(command, "reserve_bytes", 0),
                )
                .map_err(|e| e.to_string())?;
            Ok(
                json!({"id": reply.id, "outcome": reply.outcome, "gaps": reply.gaps,
                "manifest_volumes": reply.manifest.as_ref().and_then(|m| m.get("volumes")).map(|v| v.as_array().map_or(0, Vec::len))}),
            )
        }
        "drop" => {
            let existed = relay(driver, command)?
                .drop_snapshot(field(command, "id").ok_or("no id")?)
                .map_err(|e| e.to_string())?;
            Ok(json!({"existed": existed}))
        }
        "list" => {
            let snapshots = relay(driver, command)?.list().map_err(|e| e.to_string())?;
            Ok(
                json!({"snapshots": snapshots.iter().map(|s| json!({"id": s.id, "workspace": s.workspace, "complete": s.complete})).collect::<Vec<_>>()}),
            )
        }
        "ports" => {
            let mut subscription = relay(driver, command)?
                .ports(field(command, "workspace").unwrap_or(BASE_WORKSPACE))
                .map_err(|e| e.to_string())?;
            let wait = number(command, "wait_ms", 0);
            let changed = (wait > 0)
                .then(|| subscription.wait_change(Duration::from_millis(wait)))
                .flatten();
            Ok(json!({"ports": subscription.current(), "changed": changed}))
        }
        "pg-latency" => pg_latency(driver, command),
        "fairness-test" => fairness_test(driver, command),
        "tcp-ticks" => tcp_ticks(driver, command),
        "stall-test" => stall_test(driver, command),
        "close" => {
            driver.relays.close(
                &spec(driver, command)?.project.clone(),
                field(command, "reason").unwrap_or("closed by the test"),
            );
            Ok(json!({"closed": true}))
        }
        other => Err(format!("unknown command {other}")),
    }
}

fn main() {
    let mut raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    marsh_contracts::test_breaks::init_from_daemon_launch(&mut raw);
    let mut arguments = raw
        .into_iter()
        .map(|argument| argument.to_string_lossy().into_owned());
    let (mut home, mut artifacts, mut image, mut binary) = (None, None, None, None);
    let mut projects: Vec<PathBuf> = Vec::new();
    let (mut sbx, mut disk) = ("sbx".to_owned(), "20g".to_owned());
    let mut limits = Limits::default();
    let mut daemon_cap = 1024_usize;
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().expect("missing value");
        match argument.as_str() {
            "--home" => home = Some(PathBuf::from(value())),
            "--project" => projects.push(PathBuf::from(value())),
            "--artifacts" => artifacts = Some(PathBuf::from(value())),
            "--shell-image" => image = Some(value()),
            "--shell-binary" => binary = Some(PathBuf::from(value())),
            "--sbx" => sbx = value(),
            "--disk-size" => disk = value(),
            "--max-attempt" => limits.per_attempt = value().parse().expect("number"),
            "--max-project" => limits.per_project = value().parse().expect("number"),
            "--max-daemon" => daemon_cap = value().parse().expect("number"),
            "--stall-ms" => limits.stall = Duration::from_millis(value().parse().expect("ms")),
            other => panic!("unknown argument {other}"),
        }
    }
    let home = home.expect("--home");
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemCommandRunner::new(
        std::env::var_os("HOME").expect("HOME"),
    ));
    let stock = Arc::new(
        StockSbx::new(sbx, runner)
            .with_vm_ownership(home.join("vm-ownership.json"))
            .expect("ownership map"),
    );
    let budget = DaemonBudget::new(daemon_cap);
    let relays = ServicesRelays::new(
        Arc::clone(&stock),
        Some(artifacts.expect("--artifacts")),
        Arc::clone(&budget),
        Some(home.join("test-holds")),
    )
    .with_limits(limits);
    let image = OciImage::parse(image.expect("--shell-image")).expect("shell image");
    let binary = binary.expect("--shell-binary");
    let specs = projects
        .iter()
        .map(|project| ServicesVmSpec {
            project: std::fs::canonicalize(project).expect("project"),
            image: image.clone(),
            shell_binary: binary.clone(),
            disk_size: disk.clone(),
        })
        .collect();
    let mut driver = Driver {
        relays,
        specs,
        budget,
        grants: marsh_sbx::InternetGrants::new(&home),
        held: BTreeMap::new(),
    };
    println!(
        "{}",
        json!({"ready": true, "breaks": marsh_sbx::services_relay::breaks::active()})
    );
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(command) = serde_json::from_str::<Value>(&line) else {
            println!("{}", json!({"error": "not json"}));
            continue;
        };
        if field(&command, "cmd") == Some("exit") {
            driver.relays.close_all("the test driver exits");
            println!("{}", json!({"exit": true}));
            return;
        }
        let answer = match handle(&mut driver, &command) {
            Ok(value) => json!({"ok": true, "result": value}),
            Err(error) => json!({"ok": false, "error": error}),
        };
        println!("{answer}");
        let _ = std::io::stdout().flush();
    }
    driver.relays.close_all("the test driver's input ended");
}
