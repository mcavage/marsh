//! The services Internet gate (SECURITY-DESIGN A5, DECISIONS 7).
//!
//! sbx network policy can only narrow: a deny rule always beats an allow rule,
//! so "deny general egress except image pulls" cannot be written as per-sandbox
//! rules (`docs/design/services.md`, "Internet"). The gate is therefore the
//! engine's own: a marsh-owned chain [`CHAIN`] in the `filter` table of both
//! address families, jumped to from `DOCKER-USER`, which drops every NEW
//! connection that is forwarded to anything but another container network.
//! Image pulls are made by the production dockerd itself (`OUTPUT`, not
//! `FORWARD`), so they are not affected, and sbx policy still applies to every
//! packet that does pass.
//!
//! The gate also decides the registry allowlist of the filtered Docker endpoint
//! (E3's `Config::registry_allow`), through the engine's one configuration
//! file ([`crate::engine::EngineConfig`], [`crate::engine::ENGINE_CONFIG`]):
//! the default registries while closed, every registry while open. [`set`]
//! writes the rules and the file together, `engine-up` calls it before dockerd,
//! and the relay's filter reads the file on every image reference, so a toggle
//! reaches a running agent at once.
//!
//! The chain holds the closed rules while the gate is closed (the default: the
//! state `engine-up` installs unless the daemon asks for `--internet on`) and
//! no rule while it is open. `internet on|off|status` change and read it inside
//! a running engine. The intended state is also kept in [`STATE_FILE`], which
//! the supervisor compares against the rules.

use crate::engine::{DEFAULT_REGISTRIES, ENGINE_CONFIG, EngineConfig};
use std::fs;
use std::path::Path;
use std::process::Command;

/// The marsh-owned chain.
pub const CHAIN: &str = "MARSH-INTERNET";
const USER_CHAIN: &str = "DOCKER-USER";
/// Where the intended state is kept inside the engine (`open` or `closed`).
pub const STATE_FILE: &str = "/run/marsh-svc/internet";

#[derive(Clone, Copy)]
enum Family {
    V4,
    V6,
}

impl Family {
    const fn tool(self) -> &'static str {
        match self {
            Self::V4 => "iptables",
            Self::V6 => "ip6tables",
        }
    }
}

const FAMILIES: [Family; 2] = [Family::V4, Family::V6];

fn tool(family: Family, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(family.tool())
        .arg("-w")
        .args(arguments)
        .output()
        .map_err(|error| format!("{}: {error}", family.tool()))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{} {} failed ({}): {}",
            family.tool(),
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn exists(family: Family, arguments: &[&str]) -> bool {
    tool(family, arguments).is_ok()
}

/// The closed chain, in order: traffic toward a container network returns to
/// `DOCKER-USER`'s caller (container to container, published ports), and a
/// connection that anything starts toward the VM's uplink is dropped. Replies
/// are ESTABLISHED and never match the last rule's NEW.
const CLOSED_RULES: [&[&str]; 3] = [
    &["-o", "br+", "-j", "RETURN"],
    &["-o", "docker0", "-j", "RETURN"],
    &["-m", "conntrack", "--ctstate", "NEW", "-j", "DROP"],
];

/// `iptables <action> MARSH-INTERNET <rule>` as arguments.
fn rule_arguments(action: &'static str, rule: &[&'static str]) -> Vec<&'static str> {
    let mut arguments = vec![action, CHAIN];
    arguments.extend(rule);
    arguments
}

/// Whether every closed rule is present (checked with `-C`).
fn closed_rules_present(family: Family) -> bool {
    CLOSED_RULES
        .iter()
        .all(|rule| exists(family, &rule_arguments("-C", rule)))
}

/// Put the gate in the open or closed state in both families, creating the
/// chain and its jump when they are missing, and read it back. Closing adds the
/// DROP rule; opening removes it. The intended state is recorded last.
///
/// # Errors
/// The command that failed, or what differs on read-back. The caller must not
/// report the gate as changed.
pub fn set(open: bool) -> Result<(), String> {
    if let Some(parent) = Path::new(STATE_FILE).parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    // Closing narrows the allowlist before the rules close; opening widens it
    // after they open: neither order is ever more open than the gate.
    if !open {
        EngineConfig::for_internet(false).store(Path::new(ENGINE_CONFIG))?;
    }
    for family in FAMILIES {
        if !exists(family, &["-S", CHAIN]) {
            tool(family, &["-N", CHAIN])?;
        }
        if !exists(family, &["-S", USER_CHAIN]) {
            tool(family, &["-N", USER_CHAIN])?;
        }
        // The sbx-gateway firewall (A12) keeps position 1 of `DOCKER-USER`;
        // this jump goes right behind it (or first, when that is absent).
        if !exists(family, &["-C", USER_CHAIN, "-j", CHAIN]) {
            let behind_firewall = exists(family, &["-C", USER_CHAIN, "-j", crate::firewall::CHAIN]);
            tool(
                family,
                &[
                    "-I",
                    USER_CHAIN,
                    if behind_firewall { "2" } else { "1" },
                    "-j",
                    CHAIN,
                ],
            )?;
        }
        // A chain that is already as asked is left alone, so asking for the
        // state it is in never opens a moment. Otherwise it is rebuilt from
        // empty: opening is the flush, closing appends the RETURNs and the
        // DROP last.
        let as_asked = if open {
            chain_rules(family)? == 0
        } else {
            chain_rules(family)? == CLOSED_RULES.len() && closed_rules_present(family)
        };
        if !as_asked {
            tool(family, &["-F", CHAIN])?;
            if !open {
                for rule in CLOSED_RULES {
                    tool(family, &rule_arguments("-A", rule))?;
                }
            }
        }
    }
    verify(open)?;
    if open {
        EngineConfig::for_internet(true).store(Path::new(ENGINE_CONFIG))?;
    }
    verify_config(open)?;
    write_state(open)
}

/// The engine configuration file must be what the gate state means.
///
/// # Errors
/// The file is missing, unreadable, or says something else.
pub fn verify_config(open: bool) -> Result<(), String> {
    let found = EngineConfig::load(Path::new(ENGINE_CONFIG))?;
    if found == EngineConfig::for_internet(open) {
        Ok(())
    } else {
        Err(format!(
            "{ENGINE_CONFIG} holds registry_allow {:?}, which the {} gate does not mean",
            found.registry_allow,
            if open { "open" } else { "closed" }
        ))
    }
}

fn write_state(open: bool) -> Result<(), String> {
    let path = Path::new(STATE_FILE);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let temporary = path.with_extension("new");
    fs::write(&temporary, if open { "open\n" } else { "closed\n" })
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))
}

/// The state kept in [`STATE_FILE`]: `Some(true)` for open.
#[must_use]
pub fn recorded() -> Option<bool> {
    match fs::read_to_string(STATE_FILE).ok()?.trim() {
        "open" => Some(true),
        "closed" => Some(false),
        _ => None,
    }
}

/// Read both families back and require exactly the rules `open` means: the
/// chain is declared, `DOCKER-USER` jumps to it, and it holds the one DROP rule
/// (closed) or nothing (open).
///
/// # Errors
/// What differs.
pub fn verify(open: bool) -> Result<(), String> {
    for family in FAMILIES {
        let rules = chain_rules(family)?;
        let closed = closed_rules_present(family);
        if rules != if open { 0 } else { CLOSED_RULES.len() } || closed == open {
            return Err(format!(
                "{} chain {CHAIN} holds {rules} rule(s) (the closed rules are {}); the gate should be {}",
                family.tool(),
                if closed { "all there" } else { "not all there" },
                if open {
                    "open: no rules"
                } else {
                    "closed: the three rules"
                }
            ));
        }
        if !exists(family, &["-C", USER_CHAIN, "-j", CHAIN]) {
            return Err(format!(
                "{} {USER_CHAIN} does not jump to {CHAIN}",
                family.tool()
            ));
        }
    }
    Ok(())
}

/// The filter's registry allowlist (`policy::endpoint::Config::registry_allow`)
/// for the gate state: [`DEFAULT_REGISTRIES`] while closed, `None` (any registry)
/// while open. The one place the allowlist is decided:
/// [`EngineConfig::for_internet`] calls it, and everything else reads that.
#[must_use]
pub fn registry_allow(internet_open: bool) -> Option<Vec<String>> {
    (!internet_open).then(|| DEFAULT_REGISTRIES.map(str::to_owned).to_vec())
}

/// How many rules the chain holds.
fn chain_rules(family: Family) -> Result<usize, String> {
    let listing = tool(family, &["-S", CHAIN])?;
    let prefix = format!("-A {CHAIN} ");
    Ok(listing
        .lines()
        .filter(|line| line.starts_with(&prefix))
        .count())
}

/// What the rules say now: `Ok(open)`, or why they say neither.
///
/// # Errors
/// The rules are in neither state.
pub fn observed() -> Result<bool, String> {
    match verify(false) {
        Ok(()) => Ok(false),
        Err(closed) => verify(true)
            .map(|()| true)
            .map_err(|open| format!("neither closed ({closed}) nor open ({open})")),
    }
}

/// `marsh --internal-services internet on|off|status|registries on|off`. Prints one JSON line:
/// `{"open", "verified", "error"}`; `verified` is true when the rules were read
/// back as the state says (and, for `status`, agree with the recorded state).
#[must_use]
pub fn run(arguments: &[String]) -> i32 {
    let print = |open: bool, verified: bool, error: Option<&str>| {
        println!(
            "{}",
            serde_json::json!({"open": open, "verified": verified, "error": error})
        );
    };
    match arguments.first().map(String::as_str) {
        Some(word @ ("on" | "off")) => {
            let open = word == "on";
            match set(open) {
                Ok(()) => {
                    print(open, true, None);
                    0
                }
                Err(message) => {
                    print(open, false, Some(&message));
                    1
                }
            }
        }
        Some("status") => match observed() {
            Ok(open) if recorded() == Some(open) => {
                print(open, true, None);
                0
            }
            Ok(open) => {
                print(
                    open,
                    false,
                    Some("the rules differ from the recorded state, or no state is recorded"),
                );
                1
            }
            Err(message) => {
                print(false, false, Some(&message));
                1
            }
        },
        // `registries on|off`: the filter's registry list for a grant, in the
        // `--registry-allow` form (`*` means every registry).
        Some("registries") => {
            let Some(word @ ("on" | "off")) = arguments.get(1).map(String::as_str) else {
                eprintln!("marsh: internet registries: expected on or off");
                return 2;
            };
            match registry_allow(word == "on") {
                Some(list) => println!("{}", list.join(",")),
                None => println!("*"),
            }
            0
        }
        _ => {
            eprintln!("marsh: internet: expected on, off, status or registries");
            2
        }
    }
}
