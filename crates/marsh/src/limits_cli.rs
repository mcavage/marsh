//! `marsh config limits [--json]` and `marsh config limit NAME VALUE|default`
//! (`docs/configuration.md#job-limits`). Values are kept in the scope's
//! `config.json` under `limits`; the daemon reads them on every admission.

use marsh_daemon::JobDefaults;
use marsh_daemon::limits::{self, Applies, ConfigLimits, LIMITS, LimitEntry, Source};
use serde::Serialize;
use std::fmt::Write as _;
use std::path::Path;

/// Where a listing's environment values come from.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Daemon {
    /// The running daemon's launch environment.
    Running,
    /// No daemon: this shell's environment, which the next daemon started
    /// from it would read.
    Stopped,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    pub schema: &'static str,
    pub config: String,
    pub daemon: Daemon,
    pub limits: Vec<LimitEntry>,
}

/// The limits for `scope_root`: the running daemon's environment values
/// (else this process's), then `config.json`, then the defaults.
///
/// # Errors
/// Reports a malformed variable in this process's environment when no
/// daemon runs.
pub fn listing(scope_root: &Path) -> Result<Listing, String> {
    let config_path = scope_root.join(crate::shell_choice::CONFIG_FILE);
    let config = ConfigLimits::read(&config_path);
    let running = marsh_daemon::Client::connect_if_running(scope_root)
        .ok()
        .flatten()
        .and_then(|client| client.status(None).ok())
        .and_then(|status| status.job_defaults);
    let (snapshot, daemon) = match running {
        Some(effective) => (effective.environment_part(), Daemon::Running),
        None => (
            JobDefaults::from_environment(|name| std::env::var_os(name))
                .map_err(|error| error.to_string())?,
            Daemon::Stopped,
        ),
    };
    Ok(Listing {
        schema: "marsh.limits/v1",
        config: config_path.display().to_string(),
        daemon,
        limits: snapshot.with_config(&config).listing(&config),
    })
}

fn source_words(source: Source) -> &'static str {
    match source {
        Source::Default => "default",
        Source::Config => "config",
        Source::Env => "env",
    }
}

/// `marsh config limits [--json]`.
///
/// # Errors
/// Returns a message and exit status.
pub fn list_command(scope_root: &Path, json: bool) -> Result<i32, (String, i32)> {
    let listing = listing(scope_root).map_err(|message| (message, 1))?;
    if json {
        let text =
            serde_json::to_string_pretty(&listing).map_err(|error| (error.to_string(), 1))?;
        println!("{text}");
        return Ok(0);
    }
    let rows = listing
        .limits
        .iter()
        .map(|entry| {
            let source = match entry.source {
                Source::Env => format!("env {}", entry.variable),
                other => source_words(other).to_owned(),
            };
            let range =
                limits::spec(entry.name).map_or_else(String::new, limits::LimitSpec::range_text);
            let mut applies = entry.applies.words().to_owned();
            if let Some(error) = &entry.config_error {
                let _ = write!(applies, " (config.json ignored: {error})");
            }
            [
                entry.name.to_owned(),
                entry.value.to_string(),
                entry.default.to_string(),
                source,
                range,
                applies,
            ]
        })
        .collect::<Vec<_>>();
    let header = ["NAME", "VALUE", "DEFAULT", "SOURCE", "RANGE", "APPLIES"];
    let widths = (0..header.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].len())
                .chain([header[column].len()])
                .max()
                .unwrap_or(0)
        })
        .collect::<Vec<_>>();
    let line = |cells: &[&str]| {
        let mut text = String::new();
        for (column, cell) in cells.iter().enumerate() {
            if column + 1 == cells.len() {
                text.push_str(cell);
            } else {
                let _ = write!(text, "{cell:<width$}  ", width = widths[column]);
            }
        }
        text
    };
    println!("{}", line(&header));
    for row in &rows {
        println!(
            "{}",
            line(&row.iter().map(String::as_str).collect::<Vec<_>>())
        );
    }
    if listing.daemon == Daemon::Stopped {
        println!("\nmarsh is not running here: env values are this shell's, read when it starts.");
    }
    println!(
        "\nChange one with `marsh config limit NAME VALUE` (or `default`). \"now\" applies to the next launch; \"new jobs\" to jobs started after the change."
    );
    Ok(0)
}

/// `marsh config limit NAME VALUE|default`.
///
/// # Errors
/// Returns a message and exit status for an unknown name, a value out of
/// range, or a filesystem failure.
pub fn set_command(scope_root: &Path, name: &str, value: &str) -> Result<i32, (String, i32)> {
    let spec = limits::spec(name)
        .filter(|spec| spec.name == name)
        .ok_or_else(|| {
            (
                format!(
                    "marsh config limit: unknown limit {name:?}; limits are: {}",
                    LIMITS
                        .iter()
                        .map(|spec| spec.name)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                2,
            )
        })?;
    let parsed = if value == "default" {
        None
    } else {
        Some(
            spec.parse(value)
                .map_err(|message| (format!("marsh config limit: {message}"), 2))?,
        )
    };
    let path = scope_root.join(crate::shell_choice::CONFIG_FILE);
    let mut document = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| (format!("invalid {}: {error}", path.display()), 1))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err((format!("cannot read {}: {error}", path.display()), 1)),
    };
    let mut values = document
        .get_mut(limits::CONFIG_KEY)
        .and_then(serde_json::Value::as_object_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    match &parsed {
        Some(parsed) => {
            values.insert(
                spec.name.into(),
                serde_json::to_value(parsed).map_err(|error| (error.to_string(), 1))?,
            );
        }
        None => {
            values.remove(spec.name);
        }
    }
    crate::shell_choice::write_config(scope_root, limits::CONFIG_KEY, values.into())
        .map_err(|error| (format!("cannot write {}: {error}", path.display()), 1))?;
    let listing = listing(scope_root).map_err(|message| (message, 1))?;
    let Some(entry) = listing.limits.iter().find(|entry| entry.name == spec.name) else {
        return Ok(0);
    };
    println!("{} {}", spec.name, entry.value);
    if entry.source == Source::Env {
        eprintln!(
            "marsh: {} in marsh's launch environment wins over config.json until marsh restarts without it",
            spec.variable
        );
    } else if listing.daemon == Daemon::Running {
        eprintln!(
            "marsh: {}",
            match spec.applies {
                Applies::Live => "applies to the next launch",
                Applies::NewJobs => "applies to jobs started from now on; running jobs keep theirs",
            }
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn entry<'a>(listing: &'a Listing, name: &str) -> &'a LimitEntry {
        listing
            .limits
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
    }

    #[test]
    fn a_limit_is_kept_in_config_json_and_listed_with_its_source() {
        let root = scope();
        std::fs::write(root.path().join("config.json"), r#"{"shell":"zsh"}"#).unwrap();
        assert_eq!(set_command(root.path(), "job-children", "32"), Ok(0));
        assert_eq!(
            set_command(root.path(), "tree-launch-rate", "120/2m"),
            Ok(0)
        );
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join("config.json")).unwrap())
                .unwrap();
        assert_eq!(document["shell"], "zsh", "other settings are kept");
        assert_eq!(document["limits"]["job-children"], 32);
        assert_eq!(document["limits"]["tree-launch-rate"], "120/120s");
        let listing = listing(root.path()).unwrap();
        assert_eq!(listing.daemon, Daemon::Stopped);
        assert_eq!(listing.limits.len(), LIMITS.len());
        let children = entry(&listing, "job-children");
        assert_eq!(children.value, limits::LimitValue::Count(32));
        assert_eq!(children.default, limits::LimitValue::Count(16));
        assert_eq!(children.source, Source::Config);
        assert_eq!(children.applies, Applies::Live);
        assert_eq!(
            entry(&listing, "job-memory-bytes").applies,
            Applies::NewJobs
        );
        assert_eq!(entry(&listing, "daemon-jobs").source, Source::Default);
        // `default` removes the value.
        assert_eq!(set_command(root.path(), "job-children", "default"), Ok(0));
        let listing = super::listing(root.path()).unwrap();
        assert_eq!(entry(&listing, "job-children").source, Source::Default);
        assert_eq!(
            entry(&listing, "job-children").value,
            limits::LimitValue::Count(16)
        );
    }

    #[test]
    fn values_are_validated_before_anything_is_written() {
        let root = scope();
        for (name, value) in [
            ("daemon-jobs", "513"),
            ("daemon-jobs", "0"),
            ("tree-depth", "17"),
            ("job-children", "lots"),
            ("tree-launch-rate", "60"),
            ("same-kit", "3"),
            ("MARSH_DAEMON_JOBS", "8"),
        ] {
            let (message, status) = set_command(root.path(), name, value).unwrap_err();
            assert_eq!(status, 2, "{name} {value}: {message}");
        }
        assert!(!root.path().join("config.json").exists());
    }

    #[test]
    fn the_json_listing_has_the_documented_shape() {
        let root = scope();
        set_command(root.path(), "tree-kit-vms", "6").unwrap();
        let value = serde_json::to_value(listing(root.path()).unwrap()).unwrap();
        assert_eq!(value["schema"], "marsh.limits/v1");
        assert_eq!(value["daemon"], "stopped");
        let first = &value["limits"][0];
        for key in [
            "name",
            "value",
            "default",
            "source",
            "variable",
            "kind",
            "range",
            "applies",
            "unit",
            "description",
        ] {
            assert!(first.get(key).is_some(), "{key} missing: {first}");
        }
        assert_eq!(first["name"], "daemon-jobs");
        assert_eq!(first["range"], serde_json::json!({"min": 1, "max": 512}));
        let rate = value["limits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "tree-launch-rate")
            .unwrap();
        assert_eq!(rate["kind"], "rate");
        assert_eq!(rate["value"], "60/60s");
        let kits = value["limits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "tree-kit-vms")
            .unwrap();
        assert_eq!(
            (
                kits["value"].clone(),
                kits["source"].clone(),
                kits["config"].clone()
            ),
            (6.into(), "config".into(), 6.into())
        );
    }
}
