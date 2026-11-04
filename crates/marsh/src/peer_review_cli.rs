//! `marsh peer-review ls|classify` and `marsh config peer-review [on|off|ls]`
//! (`docs/peer-review.md`).

use marsh_daemon::peer_review::{
    ClassTable, DROPIN_DIR, DetectAgent, DropIn, MARSH_DROPIN, ModelClass, PeerReviewStatus, detect,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

pub const HELP: &str = "Usage: marsh peer-review ls [--json]
       marsh peer-review classify MODEL... [--json]
       marsh config peer-review [on|off|ls [--json]]

Peer review: before an agent reports substantial work as done, it asks a
model of another vendor in the same class (small, mid, large) for findings
through the `task` tool. It is off until a drop-in turns it on.

ls      The effective table: each ready reviewer by class, vendor, agent and
        model, the drop-in that named it and how its class was found; the
        reviewers that are not ready, and why; files that failed validation.
        `marsh config peer-review ls` is the same.
classify
        Print each MODEL's class, vendor and how it was classed, from the
        same table the daemon uses ($MARSH_HOME/model-classes.yaml over
        marsh's own); needs no daemon. --json prints
        [{\"model\",\"class\",\"vendor\",\"how\",\"eligible\"}]. Only a model the
        table names is eligible; a retired one (how: \"retired\") or one classed
        by price alone is shown but never picked. class, vendor and how are
        null for a model the table cannot class.
on      Write $MARSH_HOME/peer-review.d/marsh.yaml with `enabled: true` and
        the reviewers marsh can detect: each registered agent whose Kit
        declares a model option, for each vendor it can reach whose key
        `sbx secret ls` shows, that vendor's newest model in each class. A
        multi-provider agent (Pi, pix) is used for a vendor with no agent of
        its own, with the id `vendor/model`.
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

/// The registered commands whose Kit descriptor the daemon has not read yet
/// (`acp` is null while it fetches it), from `marsh status --json`.
#[must_use]
pub fn unread_agents(status: &serde_json::Value) -> Vec<String> {
    status["commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|command| command["acp"].is_null())
        .filter_map(|command| command["name"].as_str().map(str::to_owned))
        .collect()
}

/// `marsh peer-review classify`: classes `models` with the daemon's
/// classifier and `scope`'s `model-classes.yaml`. Returns the write result
/// and a malformed override's error.
pub fn classify(
    out: &mut impl Write,
    models: &[String],
    scope: &Path,
    json: bool,
) -> (std::io::Result<()>, Option<String>) {
    let (table, error) = ClassTable::load(scope);
    let rows = models
        .iter()
        .map(|model| (model, table.classify(model)))
        .collect::<Vec<_>>();
    let written = if json {
        #[derive(serde::Serialize)]
        struct Row<'a> {
            model: &'a str,
            class: Option<&'static str>,
            vendor: Option<&'a str>,
            how: Option<&'a str>,
            eligible: bool,
        }
        let document = rows
            .iter()
            .map(|(model, found)| Row {
                model,
                class: found
                    .as_ref()
                    .and_then(|found| found.class)
                    .map(ModelClass::name),
                vendor: found.as_ref().and_then(|found| found.vendor.as_deref()),
                how: found.as_ref().map(|found| found.how.as_str()),
                eligible: found.as_ref().is_some_and(|found| found.eligible),
            })
            .collect::<Vec<_>>();
        serde_json::to_writer(&mut *out, &document)
            .map_err(std::io::Error::from)
            .and_then(|()| writeln!(out))
    } else {
        rows.iter().try_for_each(|(model, found)| match found {
            Some(found) => writeln!(
                out,
                "{model}  {}  {}  ({}){}",
                found.class.map_or("unknown class", ModelClass::name),
                found.vendor.as_deref().unwrap_or("unknown vendor"),
                found.how,
                if found.eligible { "" } else { "  not eligible" }
            ),
            None => writeln!(
                out,
                "{model}  unknown (not in the class table or snapshot)  not eligible"
            ),
        })
    };
    (written, error)
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

/// Each registered agent with prompt verbs and a model option, the
/// credential services its Kit declares and where it can use them, from
/// `marsh status --json`.
#[must_use]
pub fn model_agents(status: &serde_json::Value) -> BTreeMap<String, DetectAgent> {
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
            let reaches = command["reaches"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(service, hosts)| {
                    (
                        service.clone(),
                        hosts
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect::<BTreeSet<_>>(),
                    )
                })
                .collect();
            Some((
                command["name"].as_str()?.to_owned(),
                DetectAgent { services, reaches },
            ))
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
    agents: &BTreeMap<String, DetectAgent>,
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
        if detected.ready.is_empty() {
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
        }
        if !detected.ready.is_empty() || !detected.unreachable.is_empty() {
            let reasons = detected
                .unreachable
                .iter()
                .map(|(_, reason)| reason.clone())
                .collect::<BTreeSet<_>>();
            if !reasons.is_empty() {
                if !note.is_empty() {
                    note.push_str("; ");
                }
                let _ = write!(
                    note,
                    "not ready (listed by `marsh peer-review ls`): {}",
                    reasons.into_iter().collect::<Vec<_>>().join("; ")
                );
            }
            file.reviewers = detected
                .ready
                .into_iter()
                .chain(
                    detected
                        .unreachable
                        .into_iter()
                        .map(|(reviewer, _)| reviewer),
                )
                .collect();
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
    fn classify_prints_the_class_table_answer() {
        let scope = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let models =
            ["claude-sonnet-5-5", "deepseek/deepseek-v4-flash", "nope-1"].map(String::from);
        let (written, error) = classify(&mut out, &models, scope.path(), true);
        assert!(written.is_ok() && error.is_none());
        assert!(
            out.starts_with(
                br#"[{"model":"claude-sonnet-5-5","class":"mid","vendor":"anthropic","how":"#
            ),
            "{}",
            String::from_utf8_lossy(&out)
        );
        let rows: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            rows[0],
            serde_json::json!({"model": "claude-sonnet-5-5", "class": "mid", "vendor": "anthropic",
                "how": "model-classes: claude-sonnet-*", "eligible": true})
        );
        assert_eq!(rows[1]["class"], "small");
        assert_eq!(rows[1]["vendor"], "deepseek");
        assert_eq!(rows[1]["eligible"], true);
        assert_eq!(rows[2]["class"], serde_json::Value::Null);
        assert_eq!(rows[2]["eligible"], false);
        let mut text = Vec::new();
        let _ = classify(&mut text, &models[..1], scope.path(), false);
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "claude-sonnet-5-5  mid  anthropic  (model-classes: claude-sonnet-*)\n"
        );
    }

    #[test]
    fn classify_marks_retired_and_price_classed_models_ineligible() {
        let scope = tempfile::tempdir().unwrap();
        let models = [
            "openai/o3-pro",
            "gpt-4o",
            "claude-3-opus",
            "gpt-3.5-turbo-0301",
            "grok-4.7",
            "gpt-6.1-terra",
            "claude-opus-5-5",
        ]
        .map(String::from);
        let mut out = Vec::new();
        let (written, error) = classify(&mut out, &models, scope.path(), true);
        assert!(written.is_ok() && error.is_none());
        let rows: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let rows = rows.as_array().unwrap();
        let by = |model: &str| rows.iter().find(|row| row["model"] == model).unwrap();
        for retired in [
            "openai/o3-pro",
            "gpt-4o",
            "claude-3-opus",
            "gpt-3.5-turbo-0301",
        ] {
            assert_eq!(by(retired)["eligible"], false, "{retired}");
            assert_eq!(by(retired)["how"], "retired", "{retired}");
        }
        // Still shown with a class and vendor where marsh knows them.
        assert_eq!(by("openai/o3-pro")["class"], "large");
        assert_eq!(by("openai/o3-pro")["vendor"], "openai");
        assert_eq!(by("gpt-3.5-turbo-0301")["class"], serde_json::Value::Null);
        // The price band classes a model for display only.
        assert_eq!(by("grok-4.7")["class"], "mid");
        assert_eq!(by("grok-4.7")["eligible"], false);
        for current in ["gpt-6.1-terra", "claude-opus-5-5"] {
            assert_eq!(by(current)["eligible"], true, "{current}");
        }
        let mut text = Vec::new();
        let _ = classify(&mut text, &models[..1], scope.path(), false);
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "openai/o3-pro  large  openai  (retired)  not eligible\n"
        );
    }

    #[test]
    fn a_command_whose_descriptor_is_unread_has_a_null_acp() {
        let status = serde_json::json!({"commands": [
            {"name": "claude", "acp": true},
            {"name": "shell", "acp": false},
            {"name": "codex", "acp": null},
            {"name": "pi"},
        ]});
        assert_eq!(unread_agents(&status), ["codex", "pi"]);
        assert_eq!(unread_agents(&serde_json::json!({"schema": "x"})).len(), 0);
    }

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

    #[test]
    fn on_writes_a_multi_provider_agent_only_where_its_kit_reaches() {
        let scope = tempfile::tempdir().unwrap();
        let verbs = serde_json::json!({"prompt": ["-p", "{{.Prompt}}"], "model": ["--model", "{{.Model}}"]});
        let status = serde_json::json!({"commands": [
            {"name": "claude", "services": ["anthropic"], "sessions": verbs},
            {"name": "pi", "services": ["*", "anthropic", "openai"], "sessions": verbs,
             "reaches": {"anthropic": ["api.anthropic.com"], "openai": ["api.openai.com"]}},
            {"name": "pix", "services": ["*", "anthropic"], "sessions": verbs,
             "reaches": {"anthropic": ["api.anthropic.com"]}},
        ]});
        let agents = model_agents(&status);
        let credentials = BTreeSet::from([
            "openai".to_owned(),
            "anthropic".to_owned(),
            "mistral".to_owned(),
        ]);
        let note = write_switch(scope.path(), true, &agents, &credentials).unwrap();
        // pi reaches openai; neither reaches mistral.
        assert!(note.contains("pi can't reach api.mistral.ai"), "{note}");
        let path = scope.path().join(DROPIN_DIR).join(MARSH_DROPIN);
        let file = DropIn::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let pairs = |vendor: &str| {
            file.reviewers
                .iter()
                .filter(|r| r.vendor.as_deref() == Some(vendor))
                .map(|r| (r.agent.clone(), r.model.starts_with(&format!("{vendor}/"))))
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            pairs("anthropic"),
            BTreeSet::from([("claude".to_owned(), false)])
        );
        assert_eq!(pairs("openai"), BTreeSet::from([("pi".to_owned(), true)]));
        assert_eq!(pairs("mistral"), BTreeSet::from([("pi".to_owned(), true)]));
    }
}
