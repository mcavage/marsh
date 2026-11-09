//! `marsh services status --json`'s `containers` and `ports` (E2, the shape
//! agreed with Narwhal's N3): display only, built from the base workspace's own
//! containers as the services agent lists them through the same filter as a job.
//!
//! ```json
//! "containers": [{"name": "app-db-1", "service": "db", "state": "running", "health": "healthy"}],
//! "ports": [{"service": "db", "container_port": 5432, "host_port": 5432, "protocol": "tcp"}]
//! ```
//!
//! Everything in the engine's answer is untrusted text: names, states and
//! health are reduced to plain bounded words, and nothing here names a VM,
//! volume, snapshot, engine or sbx.

use marsh_sbx::services_relay::{AttemptScope, BASE_WORKSPACE, Relay};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

const MAX_CONTAINERS: usize = 256;
const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_NAME: usize = 128;
const DEADLINE: Duration = Duration::from_secs(3);
/// The authority attempt of a status read: the base workspace, read only.
const STATUS_ATTEMPT: &str = "status";

/// The base workspace's containers and published ports, or why they could not
/// be read.
pub(crate) fn read(relay: &Relay) -> Result<(Vec<Value>, Vec<Value>), String> {
    let authority = AttemptScope::new(relay.project_path(), BASE_WORKSPACE, STATUS_ATTEMPT)
        .map_err(|error| error.to_string())?;
    let mut stream = relay
        .open_docker(&authority)
        .map_err(|error| error.to_string())?;
    let closer = stream.clone();
    let (send, receive) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let request = "GET /containers/json?all=1 HTTP/1.1\r\nHost: docker\r\nUser-Agent: marsh-status\r\nAccept: application/json\r\nConnection: close\r\n\r\n";
        let outcome = stream
            .write_all(request.as_bytes())
            .map_err(|error| error.to_string())
            .and_then(|()| response(&mut stream));
        let _ = send.send(outcome);
    });
    let body = match receive.recv_timeout(DEADLINE) {
        Ok(outcome) => outcome,
        Err(_) => Err("the services engine did not answer in time".to_owned()),
    };
    closer.close();
    parse(&body?)
}

/// One HTTP/1.1 response: the status, then a `Content-Length` or chunked body.
fn response(stream: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let header_end = loop {
        if let Some(at) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        if data.len() > 32 * 1024 {
            return Err("the services engine's answer has an oversized header".into());
        }
        let count = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("the services engine closed before it answered".into());
        }
        data.extend_from_slice(&buffer[..count]);
    };
    let head = String::from_utf8_lossy(&data[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if status.split(' ').nth(1) != Some("200") {
        return Err(format!("the services engine answered {}", bounded(status)));
    }
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse::<usize>().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    let mut rest = data[header_end + 4..].to_vec();
    let mut read_more = |rest: &mut Vec<u8>, stream: &mut dyn Read| -> Result<bool, String> {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        rest.extend_from_slice(&buffer[..count]);
        Ok(count > 0)
    };
    if chunked {
        let mut body = Vec::new();
        loop {
            let line_end = loop {
                if let Some(at) = rest.windows(2).position(|window| window == b"\r\n") {
                    break at;
                }
                if !read_more(&mut rest, stream)? {
                    return Err("a truncated chunked answer".into());
                }
            };
            let size = usize::from_str_radix(
                String::from_utf8_lossy(&rest[..line_end])
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim(),
                16,
            )
            .map_err(|_| "an invalid chunk size".to_owned())?;
            rest.drain(..line_end + 2);
            if size == 0 {
                return Ok(body);
            }
            if body.len() + size > MAX_BODY {
                return Err("the services engine's answer is too large".into());
            }
            while rest.len() < size + 2 {
                if !read_more(&mut rest, stream)? {
                    return Err("a truncated chunk".into());
                }
            }
            body.extend_from_slice(&rest[..size]);
            rest.drain(..size + 2);
        }
    }
    match length {
        Some(length) if length <= MAX_BODY => {
            while rest.len() < length {
                if !read_more(&mut rest, stream)? {
                    return Err("a truncated answer".into());
                }
            }
            rest.truncate(length);
            Ok(rest)
        }
        Some(_) => Err("the services engine's answer is too large".into()),
        None => {
            while rest.len() <= MAX_BODY && read_more(&mut rest, stream)? {}
            Ok(rest)
        }
    }
}

fn bounded(text: &str) -> String {
    plain(text, 64)
}

/// Plain text, one line, bounded: control characters and anything but
/// printable ASCII and ordinary letters are dropped.
fn plain(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control() && *c != '\u{202e}')
        .take(limit)
        .collect()
}

fn word(text: &str) -> String {
    let word: String = text
        .chars()
        .take(16)
        .filter(char::is_ascii_lowercase)
        .collect();
    if word.is_empty() {
        "unknown".into()
    } else {
        word
    }
}

fn parse(body: &[u8]) -> Result<(Vec<Value>, Vec<Value>), String> {
    let listed: Vec<Value> = serde_json::from_slice(body)
        .map_err(|_| "the services engine's list is not readable".to_owned())?;
    let mut containers = Vec::new();
    let mut ports = BTreeSet::new();
    for entry in listed.iter().take(MAX_CONTAINERS) {
        let name = entry
            .get("Names")
            .and_then(Value::as_array)
            .and_then(|names| names.first())
            .and_then(Value::as_str)
            .map(|name| plain(name.trim_start_matches('/'), MAX_NAME))
            .filter(|name| !name.is_empty());
        let Some(name) = name else { continue };
        let service = entry
            .get("Labels")
            .and_then(|labels| labels.get("com.docker.compose.service"))
            .and_then(Value::as_str)
            .map(|service| plain(service, MAX_NAME))
            .filter(|service| !service.is_empty())
            .unwrap_or_else(|| name.clone());
        let status = entry
            .get("Status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let health = if status.contains("(healthy)") {
            json!("healthy")
        } else if status.contains("(unhealthy)") {
            json!("unhealthy")
        } else if status.contains("(health: starting)") {
            json!("starting")
        } else {
            Value::Null
        };
        let state = word(
            entry
                .get("State")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        for port in entry
            .get("Ports")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(container), Some(host)) = (
                port.get("PrivatePort").and_then(Value::as_u64),
                port.get("PublicPort").and_then(Value::as_u64),
            ) else {
                continue;
            };
            let protocol = match port.get("Type").and_then(Value::as_str) {
                Some("udp") => "udp",
                _ => "tcp",
            };
            if let (Ok(container), Ok(host)) = (u16::try_from(container), u16::try_from(host)) {
                ports.insert((service.clone(), container, host, protocol));
            }
        }
        containers.push((service, name, state, health));
    }
    containers.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    Ok((
        containers
            .into_iter()
            .map(|(service, name, state, health)| {
                json!({"name": name, "service": service, "state": state, "health": health})
            })
            .collect(),
        ports
            .into_iter()
            .map(|(service, container_port, host_port, protocol)| {
                json!({"service": service, "container_port": container_port,
                       "host_port": host_port, "protocol": protocol})
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engines_list_becomes_the_agreed_shape() {
        let body = br#"[
          {"Names":["/app-db-1"],"State":"running","Status":"Up 3 minutes (healthy)",
           "Labels":{"com.docker.compose.service":"db"},
           "Ports":[{"IP":"127.0.0.1","PrivatePort":5432,"PublicPort":5432,"Type":"tcp"},
                    {"IP":"::1","PrivatePort":5432,"PublicPort":5432,"Type":"tcp"},
                    {"PrivatePort":9999,"Type":"tcp"}]},
          {"Names":["/\u001b[31mevil\n"],"State":"Run\nning","Status":"Up","Labels":{},"Ports":[]}]"#;
        let (containers, ports) = parse(body).unwrap();
        // Hostile text becomes one plain bounded line and a lowercase word.
        assert_eq!(containers[0]["name"], "[31mevil");
        assert_eq!(containers[0]["state"], "unning");
        assert_eq!(
            containers[1],
            json!({"name": "app-db-1", "service": "db", "state": "running", "health": "healthy"})
        );
        assert_eq!(
            ports,
            vec![
                json!({"service": "db", "container_port": 5432, "host_port": 5432, "protocol": "tcp"})
            ]
        );
    }

    #[test]
    fn a_chunked_answer_is_read_whole() {
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n[1,2\r\n2\r\n]\n\r\n0\r\n\r\n";
        assert_eq!(response(&mut &wire[..]).unwrap(), b"[1,2]\n");
        let sized = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n[]\n";
        assert_eq!(response(&mut &sized[..]).unwrap(), b"[]\n");
        assert!(response(&mut &b"HTTP/1.1 403 Forbidden\r\n\r\n"[..]).is_err());
    }
}
