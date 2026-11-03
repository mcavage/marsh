//! `marsh peer-review ls` and `marsh config peer-review [on|off]`
//! (`docs/peer-review.md`).

use marsh_daemon::peer_review::{
    ClassTable, DROPIN_DIR, DropIn, MARSH_DROPIN, PeerReviewStatus, detect,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

pub const HELP: &str = "Usage: marsh peer-review ls [--json]
       marsh config peer-review [on|off]

Peer review: before an agent reports substantial work as done, it asks a
model of another vendor in the same class (small, mid, large) for findings
through the `task` tool. It is off until a drop-in turns it on.

ls      The effective table: each ready reviewer by class, vendor, agent and
        model, the drop-in that named it and how its class was found; the
        reviewers that are not ready, and why; files that failed validation.
on      Write $MARSH_HOME/peer-review.d/marsh.yaml with `enabled: true` and
        the reviewers marsh can detect: each registered agent whose Kit
        declares a model option, for each vendor it can reach whose key
        `sbx secret ls` shows, that vendor's newest model in each class.
off     Set `enabled: false` in marsh.yaml (off in any drop-in is off).

Drop-ins (Narwhal writes narwhal.yaml) and $MARSH_HOME/model-classes.yaml
are read at every job start. See https://runmar.sh/docs/peer-review.html.
";

/// The `peer_review` field of a `marsh status --json` document.
///
/// # Errors
/// Returns why the daemon's document has none.
pub fn status_of(value: &serde_json::Value) -> Result<PeerReviewStatus, String> {
    match value.get("peer_review") {
        None | Some(serde_json::Value::Null) => {
            Err("this marsh daemon does not report peer review (restart it: marsh stop)".into())
        }
        Some(field) => serde_json::from_value(field.clone())
            .map_err(|error| format!("cannot decode peer_review: {error}")),
    }
}

/// The table `marsh peer-review ls` prints.
///
/// # Errors
/// Returns write failures.
pub fn render(out: &mut impl Write, status: &PeerReviewStatus) -> std::io::Result<()> {
    let name = |path: &Path| {
        path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    };
    let state = if status.enabled {
        "on".to_owned()
    } else if !status.disabled_by.is_empty() {
        format!(
            "off ({} says enabled: false)",
            status
                .disabled_by
                .iter()
                .map(|path| name(path))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        "off (no drop-in says enabled: true)".to_owned()
    };
    writeln!(out, "Peer review: {state}")?;
    if status.reviewers.is_empty() {
        writeln!(out, "No ready reviewers.")?;
    } else {
        let rows = status
            .reviewers
            .iter()
            .map(|reviewer| {
                [
                    reviewer.class.name().to_owned(),
                    reviewer.vendor.clone(),
                    reviewer.agent.clone(),
                    reviewer.model.clone(),
                    name(&reviewer.source),
                    reviewer.classified.clone(),
                ]
            })
            .collect::<Vec<_>>();
        let header = ["CLASS", "VENDOR", "AGENT", "MODEL", "FROM", "CLASSED BY"].map(str::to_owned);
        let mut widths = header.clone().map(|cell| cell.len());
        for row in &rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.len());
            }
        }
        for row in std::iter::once(&header).chain(&rows) {
            let line = row
                .iter()
                .zip(widths)
                .map(|(cell, width)| format!("{cell:<width$}"))
                .collect::<Vec<_>>()
                .join("  ");
            writeln!(out, "{}", line.trim_end())?;
        }
    }
    if !status.skipped.is_empty() {
        writeln!(out, "Not ready:")?;
        for reviewer in &status.skipped {
            writeln!(
                out,
                "  {} {} ({}): {}",
                reviewer.agent,
                reviewer.model,
                name(&reviewer.source),
                reviewer.reason.as_deref().unwrap_or_default()
            )?;
        }
    }
    if !status.errors.is_empty() {
        writeln!(out, "Errors:")?;
        for error in &status.errors {
            writeln!(out, "  {error}")?;
        }
    }
    Ok(())
}

/// Each registered agent with prompt verbs and a model option, and the
/// credential services its Kit declares, from `marsh status --json`.
#[must_use]
pub fn model_agents(status: &serde_json::Value) -> BTreeMap<String, BTreeSet<String>> {
    let has = |sessions: &serde_json::Value, key: &str, placeholder: &str| {
        sessions[key].as_array().is_some_and(|verbs| {
            verbs
                .iter()
                .any(|verb| verb.as_str().is_some_and(|verb| verb.contains(placeholder)))
        })
    };
    status["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|command| {
            has(
                &command["sessions"],
                "prompt",
                marsh_contracts::process::PROMPT_PLACEHOLDER,
            ) && has(
                &command["sessions"],
                "model",
                marsh_contracts::process::MODEL_PLACEHOLDER,
            )
        })
        .filter_map(|command| {
            let services = command["services"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect::<BTreeSet<_>>();
            Some((command["name"].as_str()?.to_owned(), services))
        })
        .collect()
}

/// The credential services `sbx secret ls --json` shows as stored.
///
/// # Errors
/// Returns why sbx could not list them.
pub fn stored_credentials(sbx: &Path) -> Result<BTreeSet<String>, String> {
    let output = std::process::Command::new(sbx)
        .args(["secret", "ls", "--global", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("cannot run {} secret ls: {error}", sbx.display()))?;
    if !output.status.success() {
        return Err(format!(
            "sbx secret ls failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cannot read sbx secret ls --json: {error}"))?;
    Ok(document["secrets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|secret| secret["type"] == "service")
        .filter_map(|secret| secret["name"].as_str().map(str::to_owned))
        .collect())
}

/// Write `marsh.yaml` for `marsh config peer-review on|off`. On, the
/// detected reviewers replace its list (an empty detection keeps the old
/// list); off keeps the list. Returns a note for the user.
///
/// # Errors
/// Returns why the file could not be read or written.
pub fn write_switch(
    scope: &Path,
    on: bool,
    agents: &BTreeMap<String, BTreeSet<String>>,
    credentials: &BTreeSet<String>,
) -> Result<String, String> {
    let directory = scope.join(DROPIN_DIR);
    let path = directory.join(MARSH_DROPIN);
    let mut file = match std::fs::read_to_string(&path) {
        Ok(text) => DropIn::parse(&text).unwrap_or_default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DropIn::default(),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    file.version = Some(1);
    file.enabled = Some(on);
    let mut note = String::new();
    if on {
        let (table, _) = ClassTable::load(scope);
        let detected = detect(&table, agents, credentials);
        if detected.is_empty() {
            note = format!(
                "marsh detected no reviewer: no registered agent with a model option ({}) can reach a vendor whose key sbx holds ({})",
                if agents.is_empty() {
                    "none".to_owned()
                } else {
                    agents.keys().cloned().collect::<Vec<_>>().join(", ")
                },
                if credentials.is_empty() {
                    "none".to_owned()
                } else {
                    credentials.iter().cloned().collect::<Vec<_>>().join(", ")
                }
            );
        } else {
            file.reviewers = detected;
        }
    }
    let text = file.render(
        "Written by `marsh config peer-review on|off` (docs/peer-review.md).\n\
         Edit freely; `marsh config peer-review on` replaces `reviewers` with what it detects.",
    );
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let temporary = directory.join(format!(".{MARSH_DROPIN}.{}", std::process::id()));
    std::fs::write(&temporary, text)
        .and_then(|()| std::fs::rename(&temporary, &path))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_detects_reviewers_and_off_keeps_them() {
        let scope = tempfile::tempdir().unwrap();
        let status = serde_json::json!({"commands": [
            {"name": "claude", "services": ["anthropic"], "sessions": {"prompt": ["-p", "{{.Prompt}}"], "model": ["--model", "{{.Model}}"]}},
            {"name": "codex", "services": ["openai"], "sessions": {"prompt": ["exec", "{{.Prompt}}"], "model": ["-m", "{{.Model}}"]}},
            {"name": "hermes", "services": ["openai"], "sessions": {"prompt": ["-p", "{{.Prompt}}"]}},
        ]});
        let agents = model_agents(&status);
        assert_eq!(agents.keys().collect::<Vec<_>>(), ["claude", "codex"]);
        let credentials = BTreeSet::from(["openai".to_owned(), "anthropic".to_owned()]);
        assert_eq!(
            write_switch(scope.path(), true, &agents, &credentials),
            Ok(String::new())
        );
        let path = scope.path().join(DROPIN_DIR).join(MARSH_DROPIN);
        let file = DropIn::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(file.enabled, Some(true));
        assert!(
            file.reviewers
                .iter()
                .any(|r| r.agent == "codex" && r.model == "gpt-6.1-terra")
        );
        write_switch(scope.path(), false, &agents, &credentials).unwrap();
        let off = DropIn::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((off.enabled, off.reviewers), (Some(false), file.reviewers));
        let note = write_switch(scope.path(), true, &BTreeMap::new(), &credentials).unwrap();
        assert!(note.contains("detected no reviewer"), "{note}");
    }
}
