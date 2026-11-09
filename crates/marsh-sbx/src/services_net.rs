//! A project's services Internet grant, what governs it, and the gate in the
//! services engine that carries it out (SECURITY-DESIGN A5, DECISIONS 7;
//! `docs/design/services.md`, "Internet").
//!
//! Three pieces:
//!
//! - [`InternetGrants`]: the user's per-project grant, kept in a daemon-owned
//!   file. Absent means off; reading fails closed.
//! - [`assess`]: reads `sbx policy ls --json` and decides whether a policy that
//!   marsh did not create governs the project's services VM. Marsh adds no sbx
//!   rule of its own (a deny always beats an allow, so "closed except image
//!   pulls" cannot be written as sbx rules), so **every** rule listed is
//!   foreign. Anything unreadable counts as governed.
//! - [`StockSbx::services_gate`] / [`StockSbx::services_set_gate`]: read and
//!   change the engine's gate (`marsh --internal-services internet ...`).

use super::{SbxError, StockSbx, VmClass, command_failed, validate_name};
use marsh_services::engine::{ENGINE_CONTAINER, ENGINE_SOCKET, MARSH_BINARY};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// Test-only faults and defects of the cutover's engine queries
/// (`docs/design/test-breaks.md`).
pub mod breaks {
    /// A fault: the cutover's running count cannot reach the engine.
    pub const FAULT_ENGINE_QUERY: &str = "fault-engine-query";
    /// The defect for it: the count is read through `| wc -l`, so a failed
    /// query reads as zero running containers.
    pub const ENGINE_QUERY_FAIL_OPEN: &str = "engine-query-fail-open";
}

const GRANTS_SCHEMA: &str = "marsh.services-internet/v1";

/// The per-project Internet grants of one daemon scope.
#[derive(Debug)]
pub struct InternetGrants {
    path: PathBuf,
    write: Mutex<()>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct GrantsFile {
    #[serde(default)]
    schema: String,
    #[serde(default)]
    projects: BTreeMap<String, bool>,
}

impl InternetGrants {
    /// The grants file of the scope whose daemon home is `daemon_home`.
    #[must_use]
    pub fn new(daemon_home: &Path) -> Self {
        Self {
            path: daemon_home.join("services-internet.json"),
            write: Mutex::new(()),
        }
    }

    fn read(&self) -> Result<GrantsFile, String> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let file: GrantsFile = serde_json::from_slice(&bytes)
                    .map_err(|error| format!("{}: {error}", self.path.display()))?;
                if file.schema == GRANTS_SCHEMA {
                    Ok(file)
                } else {
                    Err(format!(
                        "{}: unknown schema {:?}",
                        self.path.display(),
                        file.schema
                    ))
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(GrantsFile {
                schema: GRANTS_SCHEMA.into(),
                projects: BTreeMap::new(),
            }),
            Err(error) => Err(format!("{}: {error}", self.path.display())),
        }
    }

    /// Whether the user granted Internet to the project. A project with no
    /// record is off, and so is anything unreadable (closed on doubt).
    #[must_use]
    pub fn is_on(&self, project: &str) -> bool {
        // Test break: a project nobody toggled starts with Internet on.
        let default = marsh_contracts::break_mode(super::services_breaks::SERVICES_DEFAULT_ON);
        match self.read() {
            Ok(file) => file.projects.get(project).copied().unwrap_or(default),
            Err(_) => false,
        }
    }

    /// Record the grant (atomically; the file is private to the daemon).
    ///
    /// # Errors
    /// The file cannot be read or written: the grant is then unchanged.
    pub fn set(&self, project: &str, on: bool) -> Result<(), String> {
        self.update(|file| {
            file.projects.insert(project.to_owned(), on);
        })
    }

    /// Forget the project's grant (`marsh services rm`): back to the default.
    ///
    /// # Errors
    /// As [`Self::set`].
    pub fn clear(&self, project: &str) -> Result<(), String> {
        self.update(|file| {
            file.projects.remove(project);
        })
    }

    fn update(&self, change: impl FnOnce(&mut GrantsFile)) -> Result<(), String> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let _held = self
            .write
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file = self.read()?;
        file.schema = GRANTS_SCHEMA.into();
        change(&mut file);
        let temporary = self.path.with_extension("json.new");
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        output
            .write_all(&serde_json::to_vec_pretty(&file).map_err(|error| error.to_string())?)
            .and_then(|()| output.sync_all())
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .map_err(|error| format!("{}: {error}", self.path.display()))
    }
}

/// Whether a policy that marsh did not create governs the project's services
/// VM, and the description `marsh services status` shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Governance {
    pub governed: bool,
    /// What governs, naming the rules; or that nothing does.
    pub summary: String,
    /// Hosts your own global deny rules block. They narrow what Internet "on"
    /// reaches, never widen it, so they do not govern the toggle.
    pub also_denied: Vec<String>,
}

impl Governance {
    fn governed(summary: String) -> Self {
        Self {
            governed: true,
            summary,
            also_denied: Vec::new(),
        }
    }
}

/// A resource is one exact host (optionally with a port): no glob, no CIDR.
fn exact_host(resource: &str) -> bool {
    !resource.is_empty()
        && resource
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':'))
}

/// Decide from the document of `sbx policy ls --json [SANDBOX]`.
///
/// A rule governs when it comes from an external layer (org, remote, kit), is
/// scoped to this sandbox, or is a global deny that is not an exact host. The
/// global policy must also still allow everything: a restrictive preset (no
/// active global allow-all) governs. Exact-host global denies and every other
/// allow rule never govern: a deny can only narrow what the toggle reaches. The
/// built-in defaults are the baseline, not governance.
///
/// This fails closed: a document that does not parse as expected, or a rule
/// that lacks the fields this code reads, counts as governed.
#[must_use]
pub fn assess(document: &Value, sandbox: Option<&str>) -> Governance {
    let Some(rules) = document.get("rules").and_then(Value::as_array) else {
        return Governance::governed(
            "sbx policy could not be read (no `rules` list); treated as governed".into(),
        );
    };
    let own_scope = sandbox.map(|name| format!("sandbox:{name}"));
    let mut governing = Vec::new();
    let mut also_denied = Vec::new();
    let mut baseline_open = false;
    for rule in rules {
        let text = |key: &str| rule.get(key).and_then(Value::as_str);
        let (Some(id), Some(decision), Some(resource_type)) =
            (text("id"), text("decision"), text("resource_type"))
        else {
            return Governance::governed(
                "sbx policy could not be read (a rule lacks id, decision or type); treated as governed"
                    .into(),
            );
        };
        if resource_type != "network" || text("status").is_some_and(|status| status != "active") {
            continue;
        }
        let applies_to = text("applies_to").unwrap_or("");
        let scope = text("scope").unwrap_or("");
        let other_sandbox =
            applies_to.starts_with("sandbox:") && own_scope.as_deref() != Some(applies_to);
        if other_sandbox {
            continue;
        }
        let resources: Vec<&str> = rule
            .get("resources")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let describe = || {
            format!(
                "{decision} {} (rule {id}, scope {})",
                resources.join(","),
                if scope.is_empty() { applies_to } else { scope }
            )
        };
        let local = text("origin") == Some("local") && text("layer") == Some("local");
        // sbx's own baseline: `default-allow-all`, the filesystem defaults, and
        // `default-deny-all` (the fallback that applies when no rule decides,
        // which carries no provenance). Rules a user adds have UUID ids.
        let built_in = id.starts_with("default-")
            || rule
                .get("provenance")
                .and_then(|provenance| provenance.get("created_via"))
                .and_then(Value::as_str)
                == Some("default");
        let allows_everything = decision == "allow" && resources.contains(&"**");
        if !local || scope != "global" {
            governing.push(describe());
        } else if built_in || decision == "allow" {
            baseline_open |= allows_everything;
        } else if !resources.is_empty()
            && resources.iter().all(|resource| exact_host(resource))
            // Test break: any global deny governs, however narrow.
            && !marsh_contracts::break_mode(super::services_breaks::GOVERNANCE_ANY_DENY)
        {
            also_denied.extend(resources.iter().map(|resource| (*resource).to_owned()));
        } else {
            governing.push(describe());
        }
    }
    if governing.is_empty() && !baseline_open {
        governing.push(
            "the global sbx network policy has no active allow-all rule (a restrictive preset)"
                .into(),
        );
    }
    if governing.is_empty() {
        let note = if also_denied.is_empty() {
            String::new()
        } else {
            format!("; your global rules also deny {}", also_denied.join(", "))
        };
        Governance {
            governed: false,
            summary: format!("no sbx policy governs this project's services VM{note}"),
            also_denied,
        }
    } else {
        Governance::governed(format!("sbx policy governs: {}", governing.join("; ")))
    }
}

/// The gate in a services engine, as read from inside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Gate {
    Open,
    Closed,
    /// The engine container is not running (the intended state is applied the
    /// next time `engine-up` runs).
    NoEngine,
    /// The engine answered, but the rules do not match what it says it holds.
    Unverified(String),
}

impl StockSbx {
    /// `sbx policy ls --json [VM]` assessed by [`assess`]; any failure to read
    /// is governed.
    #[must_use]
    pub fn services_governance(&self, vm: Option<&str>) -> Governance {
        if let Some(name) = vm
            && validate_name(name).is_err()
        {
            return Governance::governed("invalid sandbox name; treated as governed".into());
        }
        let mut arguments = vec!["policy", "ls", "--json"];
        if let Some(name) = vm {
            arguments.push(name);
        }
        let output = match self.run_bounded_os(
            "list sbx policy",
            self.preparation_timeouts.sbx,
            arguments,
        ) {
            Ok(output) if output.succeeded() => output,
            Ok(output) => {
                return Governance::governed(format!(
                    "sbx policy could not be read ({}); treated as governed",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            Err(error) => {
                return Governance::governed(format!(
                    "sbx policy could not be read ({error}); treated as governed"
                ));
            }
        };
        match serde_json::from_slice::<Value>(&output.stdout) {
            Ok(document) => assess(&document, vm),
            Err(error) => Governance::governed(format!(
                "sbx policy could not be read ({error}); treated as governed"
            )),
        }
    }

    /// Run `marsh --internal-services internet ARG` in the VM's engine
    /// container and parse its one JSON line.
    fn engine_internet(&self, vm: &str, argument: &str) -> Result<Gate, SbxError> {
        validate_name(vm)?;
        // The engine is addressed by the id the daemon's own `docker create`
        // returned, never by name: the VM can rename containers, and a name
        // would reach whatever holds it. No recorded generation (the daemon
        // started after the engine, or has not started one) means no engine
        // to reach: the grant applies at the next start.
        let engine = if marsh_contracts::break_mode(super::services_breaks::GATE_BY_NAME) {
            ENGINE_CONTAINER.to_owned()
        } else {
            let recorded = self
                .services_engine_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(vm)
                .cloned();
            match recorded {
                Some(id) if id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) => id,
                _ => return Ok(Gate::NoEngine),
            }
        };
        let output = self.run_bounded_os(
            "services Internet gate",
            self.preparation_timeouts.sbx,
            [
                "exec",
                "-u",
                "root",
                vm,
                "docker",
                "exec",
                &engine,
                MARSH_BINARY,
                "--internal-services",
                "internet",
                argument,
            ],
        )?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.succeeded()
            && output.stdout.is_empty()
            && (stderr.contains("No such container") || stderr.contains("is not running"))
        {
            return Ok(Gate::NoEngine);
        }
        let Ok(report) = serde_json::from_slice::<Value>(&output.stdout) else {
            return Err(command_failed("services Internet gate", output));
        };
        let open = report.get("open").and_then(Value::as_bool);
        let verified = report.get("verified").and_then(Value::as_bool) == Some(true);
        Ok(match (open, verified) {
            (Some(true), true) => Gate::Open,
            (Some(false), true) => Gate::Closed,
            _ => Gate::Unverified(
                report
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the engine did not verify its gate")
                    .to_owned(),
            ),
        })
    }

    /// The recorded engine id of an **owned, running** services VM, checked now
    /// against the ownership map and a fresh inventory: a name that another
    /// sandbox holds (the old relay may not know yet) is refused, and nothing
    /// is executed on it (AGENTS.md: never exec into a VM that is not in the
    /// map with its recorded UUID).
    fn owned_engine(&self, vm: &str, operation: &'static str) -> Result<String, SbxError> {
        validate_name(vm)?;
        match self.classify_vm(vm, true)? {
            VmClass::Owned { running: true, .. } => {}
            VmClass::Owned { .. } | VmClass::Absent => {
                return Err(SbxError::LifecycleSchema {
                    operation,
                    detail: "the services VM is not running".into(),
                });
            }
            VmClass::Foreign => return Err(SbxError::ForeignVm(vm.to_owned())),
        }
        let recorded = self
            .services_engine_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(vm)
            .cloned();
        match recorded {
            Some(id) if id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) => Ok(id),
            _ => Err(SbxError::LifecycleSchema {
                operation,
                detail: "no engine generation is recorded".into(),
            }),
        }
    }

    /// Run `script` (`sh -c`) in the owned VM's engine container, addressed by
    /// the recorded id. Returns stdout.
    fn engine_sh(
        &self,
        vm: &str,
        operation: &'static str,
        script: &str,
    ) -> Result<String, SbxError> {
        let engine = self.owned_engine(vm, operation)?;
        let output = self.run_bounded_os(
            operation,
            self.preparation_timeouts.sbx,
            [
                "exec", "-u", "root", vm, "docker", "exec", &engine, "sh", "-c", script,
            ],
        )?;
        if !output.succeeded() {
            return Err(command_failed(operation, output));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Freeze every running container of the production engine (all
    /// workspaces) for a stop cutover: nothing can write from here on. Returns
    /// the ids paused, so a failed stop can thaw exactly those. Verified: the
    /// engine lists no running container afterwards.
    ///
    /// # Errors
    /// The VM is not the owned, running one, no engine is recorded, stock could
    /// not run the command, an engine query failed, or something is still
    /// running (the ids that were paused are thawed first).
    pub fn services_quiesce(&self, vm: &str) -> Result<Vec<String>, SbxError> {
        // Every engine query is checked by its own exit status: a pipeline
        // (`docker ps | wc -l`) reports the status of its last command, so a
        // failed `docker ps` would read as "nothing is running".
        let sock = ENGINE_SOCKET;
        let script = format!(
            "D='docker -H unix://{sock}'; \
             ids=$($D ps -q --no-trunc --filter status=running) || exit 71; \
             for i in $ids; do if $D pause $i >/dev/null 2>&1; then echo $i; fi; done; \
             rest=$($D ps -q --filter status=running) || {{ $D unpause $ids >/dev/null 2>&1; exit 72; }}; \
             set -- $rest; echo @@running=$#"
        );
        let text = self.engine_sh(vm, "services quiesce", &script)?;
        let (paused, running) = parse_quiesce(&text)?;
        if running != Some(0) {
            let _ = self.services_unquiesce(vm, &paused);
            return Err(SbxError::LifecycleSchema {
                operation: "services quiesce",
                detail: format!("{running:?} containers still running after the freeze"),
            });
        }
        Ok(paused)
    }

    /// Thaw the containers a cutover froze (by the ids it recorded).
    ///
    /// # Errors
    /// As [`Self::services_quiesce`], or the engine refused to unpause one.
    pub fn services_unquiesce(&self, vm: &str, ids: &[String]) -> Result<(), SbxError> {
        if ids.is_empty() {
            return Ok(());
        }
        if ids
            .iter()
            .any(|id| id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(SbxError::LifecycleSchema {
                operation: "services unquiesce",
                detail: "not a container id".into(),
            });
        }
        let sock = ENGINE_SOCKET;
        self.engine_sh(
            vm,
            "services unquiesce",
            &format!(
                "docker -H unix://{sock} unpause {} >/dev/null",
                ids.join(" ")
            ),
        )
        .map(|_| ())
    }

    /// How many containers the engine runs (the cutover's check after its
    /// captures). Fails closed: an engine query that fails is an error, never
    /// a count of zero.
    ///
    /// # Errors
    /// As [`Self::services_quiesce`]; the count is unreadable.
    pub fn services_running_count(&self, vm: &str) -> Result<u64, SbxError> {
        // Test fault: the query cannot reach the engine. Test defect: the count
        // is read through a pipeline, as the first version did, so the failed
        // `docker ps` reads as zero.
        let sock = if marsh_contracts::break_mode(breaks::FAULT_ENGINE_QUERY) {
            "/nonexistent/docker.sock"
        } else {
            ENGINE_SOCKET
        };
        let script = if marsh_contracts::break_mode(breaks::ENGINE_QUERY_FAIL_OPEN) {
            format!(
                "echo @@running=$(docker -H unix://{sock} ps -q --filter status=running | wc -l)"
            )
        } else {
            format!(
                "r=$(docker -H unix://{sock} ps -q --filter status=running) || exit 72; \
                 set -- $r; echo @@running=$#"
            )
        };
        let text = self.engine_sh(vm, "services running count", &script)?;
        text.lines()
            .find_map(|line| line.trim().strip_prefix("@@running="))
            .and_then(|count| count.trim().parse().ok())
            .ok_or_else(|| SbxError::LifecycleSchema {
                operation: "services running count",
                detail: "unreadable count".into(),
            })
    }

    /// The gate as the running engine reports and verifies it. Never starts a
    /// VM: the caller checks that the VM runs first.
    ///
    /// # Errors
    /// Stock could not run the command or its answer is unreadable.
    pub fn services_gate(&self, vm: &str) -> Result<Gate, SbxError> {
        self.engine_internet(vm, "status")
    }

    /// Open (`true`) or close the running engine's gate, verified by reading
    /// the rules back inside the engine.
    ///
    /// # Errors
    /// As [`Self::services_gate`].
    pub fn services_set_gate(&self, vm: &str, open: bool) -> Result<Gate, SbxError> {
        self.engine_internet(vm, if open { "on" } else { "off" })
    }
}

/// What the freeze script printed: the full ids it paused, one per line, and
/// `@@running=N`. An id that is not a full 64-hex id is an error (a thaw is
/// addressed by exactly these ids, so a short one would silently never thaw).
fn parse_quiesce(text: &str) -> Result<(Vec<String>, Option<u64>), SbxError> {
    let mut paused = Vec::new();
    let mut running = None;
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(count) = line.strip_prefix("@@running=") {
            running = count.trim().parse::<u64>().ok();
        } else if line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit()) {
            paused.push(line.to_owned());
        } else {
            return Err(SbxError::LifecycleSchema {
                operation: "services quiesce",
                detail: "the freeze printed something that is not a full container id".into(),
            });
        }
    }
    Ok((paused, running))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(
        id: &str,
        scope: &str,
        applies_to: &str,
        origin: &str,
        decision: &str,
        resources: &[&str],
        created_via: &str,
    ) -> Value {
        json!({
            "id": id, "scope": scope, "applies_to": applies_to, "resource_type": "network",
            "decision": decision, "resources": resources, "origin": origin, "layer": origin,
            "status": "active", "provenance": {"created_via": created_via},
        })
    }

    fn baseline() -> Value {
        rule(
            "default-allow-all",
            "global",
            "all",
            "local",
            "allow",
            &["**"],
            "default",
        )
    }

    fn assessed(rules: &[Value]) -> Governance {
        assess(&json!({"rules": rules}), Some("vm-a"))
    }

    /// What the freeze script's answer may be (table): full ids and the count are
    /// read; a short id (what `docker ps -q` prints without `--no-trunc`), or any
    /// other line, is refused rather than skipped, since a thaw names exactly these ids.
    #[test]
    fn the_freeze_answer_names_full_ids_only() {
        let full = "a".repeat(64);
        let other = "b".repeat(64);
        for (text, expected) in [
            (
                format!("{full}\n{other}\n@@running=0\n"),
                Some((vec![full.clone(), other.clone()], Some(0))),
            ),
            ("@@running=0\n".to_owned(), Some((vec![], Some(0)))),
            (
                format!("{full}\n@@running=2\n"),
                Some((vec![full.clone()], Some(2))),
            ),
            ("193418d676fd\n@@running=0\n".to_owned(), None),
            (format!("{full}x\n@@running=0\n"), None),
            ("Error response\n@@running=0\n".to_owned(), None),
            (format!("{full}\n"), Some((vec![full.clone()], None))),
        ] {
            let parsed = parse_quiesce(&text).ok();
            assert_eq!(parsed, expected, "{text:?}");
        }
    }

    #[test]
    fn the_fallback_deny_rule_is_the_baseline_not_governance() {
        // `sbx policy ls --json` lists the fallback as an active global deny of
        // `**` with no provenance (as sbx v0.51 does).
        let mut fallback = rule(
            "default-deny-all",
            "global",
            "all",
            "local",
            "deny",
            &["**"],
            "default",
        );
        fallback["provenance"] = Value::Null;
        let found = assessed(&[baseline(), fallback]);
        assert!(!found.governed, "{found:?}");
    }

    #[test]
    fn the_open_default_governs_nothing_and_exact_global_denies_only_narrow() {
        let plain = assessed(&[baseline()]);
        assert!(!plain.governed, "{plain:?}");
        let narrowed = assessed(&[
            baseline(),
            rule(
                "r1",
                "global",
                "all",
                "local",
                "deny",
                &["telemetry.example.net"],
                "added",
            ),
        ]);
        assert!(!narrowed.governed, "{narrowed:?}");
        assert_eq!(narrowed.also_denied, ["telemetry.example.net"]);
        assert!(narrowed.summary.contains("telemetry.example.net"));
    }

    #[test]
    fn every_other_foreign_rule_governs_and_the_summary_names_it() {
        let sandbox = "sandbox:vm-a";
        for (name, extra) in [
            (
                "a rule scoped to this sandbox",
                rule(
                    "r-sbx",
                    sandbox,
                    sandbox,
                    "local",
                    "deny",
                    &["example.com"],
                    "added",
                ),
            ),
            (
                "a global wildcard deny",
                rule(
                    "r-wild",
                    "global",
                    "all",
                    "local",
                    "deny",
                    &["*.com"],
                    "added",
                ),
            ),
            (
                "a global deny of everything",
                rule("r-all", "global", "all", "local", "deny", &["**"], "added"),
            ),
            (
                "a deny that lists no resources",
                rule("r-none", "global", "all", "local", "deny", &[], "added"),
            ),
            (
                "a CIDR deny",
                rule(
                    "r-cidr",
                    "global",
                    "all",
                    "local",
                    "deny",
                    &["10.0.0.0/8"],
                    "added",
                ),
            ),
            (
                "an organization rule",
                rule(
                    "r-org",
                    "global",
                    "all",
                    "organization",
                    "deny",
                    &["x.example"],
                    "added",
                ),
            ),
            (
                "a kit rule for this sandbox",
                rule(
                    "r-kit",
                    sandbox,
                    sandbox,
                    "kit",
                    "allow",
                    &["a.example"],
                    "provisioned",
                ),
            ),
        ] {
            let found = assessed(&[baseline(), extra.clone()]);
            assert!(found.governed, "{name}: {found:?}");
            assert!(
                found.summary.contains(extra["id"].as_str().unwrap()),
                "{name}: {}",
                found.summary
            );
        }
    }

    #[test]
    fn a_restrictive_global_preset_governs() {
        // No active allow-all: the user's own Locked Down or Balanced preset.
        let preset = rule(
            "p1",
            "global",
            "all",
            "local",
            "allow",
            &["api.example.com"],
            "added",
        );
        let found = assessed(&[preset]);
        assert!(found.governed, "{found:?}");
        assert!(found.summary.contains("allow-all"), "{}", found.summary);
    }

    #[test]
    fn another_sandboxes_rules_and_inactive_rules_do_not_count() {
        let other = rule(
            "r-other",
            "sandbox:vm-b",
            "sandbox:vm-b",
            "local",
            "deny",
            &["**"],
            "added",
        );
        let mut inactive = rule(
            "r-off",
            "sandbox:vm-a",
            "sandbox:vm-a",
            "local",
            "deny",
            &["**"],
            "added",
        );
        inactive["status"] = json!("inactive");
        let found = assessed(&[baseline(), other, inactive]);
        assert!(!found.governed, "{found:?}");
    }

    #[test]
    fn anything_unreadable_is_governed() {
        for document in [
            json!({}),
            json!({"rules": "no"}),
            json!({"rules": [{"id": "x"}]}),
        ] {
            let found = assess(&document, Some("vm-a"));
            assert!(found.governed, "{document}: {found:?}");
            assert!(found.summary.contains("treated as governed"));
        }
    }

    #[test]
    fn grants_default_off_persist_and_read_closed() {
        let home = tempfile::tempdir().unwrap();
        let grants = InternetGrants::new(home.path());
        assert!(!grants.is_on("/p"));
        grants.set("/p", true).unwrap();
        assert!(grants.is_on("/p"));
        assert!(!grants.is_on("/q"));
        grants.set("/p", false).unwrap();
        assert!(!grants.is_on("/p"));
        grants.set("/p", true).unwrap();
        grants.clear("/p").unwrap();
        assert!(!grants.is_on("/p"));
        // Unreadable: never on, and a write refuses to overwrite it.
        fs::write(home.path().join("services-internet.json"), "not json").unwrap();
        grants.set("/p", true).unwrap_err();
        assert!(!grants.is_on("/p"));
    }
}
