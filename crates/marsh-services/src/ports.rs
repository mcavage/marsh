//! Published-port tracking from the production engine's event stream.
//!
//! The watcher keeps, per workspace scope (the `dev.marsh.scope` label), the
//! set of TCP ports published by running containers, and reports the whole
//! state to the host as a `ports` frame whenever it changes. It reacts to
//! engine events (start, die, stop, ...), coalescing bursts, and re-lists
//! after every reconnect, so it never depends on a poll interval.

use crate::engine_api::{Engine, SCOPE_LABEL};
use crate::frame::{AgentFrame, ScopePorts};
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::time::Duration;

const MAX_SCOPES: usize = 256;
const MAX_PORTS_PER_SCOPE: usize = 512;
const ACTIONS: &[&str] = &[
    "start", "die", "stop", "kill", "destroy", "restart", "oom", "create", "rename",
];

fn snapshot(
    engine: &Engine,
    admitted: &dyn Fn(&str, &str) -> bool,
) -> Option<BTreeMap<String, BTreeSet<u16>>> {
    let containers = engine.list_containers(false).ok()?;
    let mut map: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
    for container in containers.iter().filter(|c| c.state == "running") {
        let Some(scope) = container.label(SCOPE_LABEL) else {
            continue;
        };
        // Only what the policy admitted into this scope: a label alone is not authority.
        if !admitted(scope, &container.id) {
            continue;
        }
        for port in container.ports.iter().flatten() {
            if port.public_port != 0 && port.kind == "tcp" {
                map.entry(scope.to_owned())
                    .or_default()
                    .insert(port.public_port);
            }
        }
    }
    Some(map)
}

fn frame_for(map: &BTreeMap<String, BTreeSet<u16>>) -> AgentFrame {
    let mut truncated = map.len() > MAX_SCOPES;
    let scopes = map
        .iter()
        .take(MAX_SCOPES)
        .map(|(workspace, ports)| {
            truncated |= ports.len() > MAX_PORTS_PER_SCOPE;
            ScopePorts {
                workspace: workspace.clone(),
                ports: ports.iter().copied().take(MAX_PORTS_PER_SCOPE).collect(),
            }
        })
        .collect();
    AgentFrame::Ports { scopes, truncated }
}

/// Starts the watcher thread. `send` delivers frames to the host.
///
/// `admitted(scope, container id)` says whether the policy admitted that
/// container into the scope; the label alone is never enough.
pub fn spawn_watcher(
    engine: Engine,
    admitted: impl Fn(&str, &str) -> bool + Send + 'static,
    send: impl Fn(AgentFrame) + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut first = true;
        let mut current: BTreeMap<String, BTreeSet<u16>> = BTreeMap::new();
        loop {
            if let Ok(mut reader) = engine.events() {
                let _ = reader
                    .get_ref()
                    .set_read_timeout(Some(Duration::from_millis(40)));
                let mut dirty = true;
                let mut quiet = true;
                let mut line = String::new();
                loop {
                    if dirty && quiet {
                        dirty = false;
                        if let Some(now) = snapshot(&engine, &admitted) {
                            let changed = current != now;
                            if changed {
                                current.clone_from(&now);
                            }
                            if changed || first {
                                first = false;
                                send(frame_for(&now));
                            }
                        }
                    }
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            quiet = false;
                            if line.ends_with('\n') {
                                if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line)
                                {
                                    let action = event["Action"].as_str().unwrap_or_default();
                                    dirty |= ACTIONS.contains(&action);
                                }
                                line.clear();
                            }
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            quiet = true;
                        }
                        Err(_) => break,
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}
