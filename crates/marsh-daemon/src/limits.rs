//! Every limit a scope can change (`docs/configuration.md#job-limits`): its
//! name, variable, range, default, and when a change applies.
//!
//! A value comes from the daemon's launch environment (`MARSH_...`, fixed
//! for the daemon's life), else from `$MARSH_HOME/config.json` (`limits`,
//! written by `marsh config limit`, read again on every admission), else the
//! default. Admission limits apply to the next launch; resource limits to
//! jobs (and Kit VMs) started after the change.

use crate::DaemonError;
use marsh_contracts::JobResources;
use marsh_contracts::process::{self, LaunchRate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The key in `config.json` holding `{NAME: VALUE}`.
pub const CONFIG_KEY: &str = "limits";

/// When a changed value takes effect.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Applies {
    /// The next admission checks it: no restart.
    Live,
    /// Jobs started after the change (and, for memory, Kit VMs created
    /// after it); running jobs keep theirs.
    NewJobs,
}

impl Applies {
    #[must_use]
    pub const fn words(self) -> &'static str {
        match self {
            Self::Live => "now",
            Self::NewJobs => "new jobs",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Default,
    Config,
    Env,
}

/// A count, or a launch rate (`60/60s`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum LimitValue {
    Count(u64),
    Rate(String),
}

impl std::fmt::Display for LimitValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count(count) => write!(formatter, "{count}"),
            Self::Rate(rate) => formatter.write_str(rate),
        }
    }
}

/// The largest integer a JSON reader in JavaScript holds exactly.
const SAFE_MAX: u64 = (1 << 53) - 1;

#[derive(Clone, Copy, Debug)]
pub struct LimitSpec {
    /// `marsh config limit NAME`.
    pub name: &'static str,
    pub variable: &'static str,
    /// Counts only; a rate is checked by `LaunchRate::parse`.
    pub min: u64,
    pub max: u64,
    pub rate: bool,
    pub applies: Applies,
    pub unit: &'static str,
    pub description: &'static str,
}

const fn count(
    name: &'static str,
    variable: &'static str,
    max: u64,
    applies: Applies,
    unit: &'static str,
    description: &'static str,
) -> LimitSpec {
    LimitSpec {
        name,
        variable,
        min: 1,
        max,
        rate: false,
        applies,
        unit,
        description,
    }
}

/// Every limit, in listing order: admission first, then per-job resources.
pub const LIMITS: [LimitSpec; 12] = [
    count(
        "daemon-jobs",
        "MARSH_DAEMON_JOBS",
        process::POOL_MAX as u64,
        Applies::Live,
        "jobs",
        "Jobs at once in this scope, all depths. Each Kit VM also runs at most 64.",
    ),
    count(
        "job-children",
        "MARSH_JOB_CHILDREN",
        process::FAN_OUT_MAX as u64,
        Applies::Live,
        "jobs",
        "Live children of one job.",
    ),
    count(
        "tree-depth",
        "MARSH_TREE_DEPTH",
        process::DEPTH_MAX as u64,
        Applies::Live,
        "levels",
        "Depth of a job tree; your command is depth 1.",
    ),
    count(
        "tree-kit-vms",
        "MARSH_TREE_KIT_VMS",
        64,
        Applies::Live,
        "Kit VMs",
        "Distinct Kit VMs used by the live jobs of one tree.",
    ),
    count(
        "tasks-live",
        "MARSH_TASKS_LIVE",
        process::TASKS_LIVE_MAX as u64,
        Applies::NewJobs,
        "tasks",
        "Tasks one agent's task tool has running at once; finished tasks do not count.",
    ),
    LimitSpec {
        name: "tree-launch-rate",
        variable: "MARSH_TREE_LAUNCH_RATE",
        min: 1,
        max: 10_000,
        rate: true,
        applies: Applies::Live,
        unit: "launches/seconds",
        description: "Launches one job tree may make in any rolling window, as LAUNCHES/SECONDS.",
    },
    count(
        "job-cpu-millis",
        "MARSH_JOB_CPU_MILLIS",
        u32::MAX as u64,
        Applies::NewJobs,
        "millicpus",
        "CPU per job.",
    ),
    count(
        "job-memory-bytes",
        "MARSH_JOB_MEMORY_BYTES",
        SAFE_MAX,
        Applies::NewJobs,
        "bytes",
        "Memory per job. Kit VMs created after a change are sized to fit it.",
    ),
    count(
        "job-pids",
        "MARSH_JOB_PIDS",
        u32::MAX as u64,
        Applies::NewJobs,
        "processes",
        "Processes per job.",
    ),
    count(
        "job-writable-bytes",
        "MARSH_JOB_WRITABLE_BYTES",
        SAFE_MAX,
        Applies::NewJobs,
        "bytes",
        "The container's own files per job, not mounted directories.",
    ),
    count(
        "job-output-bytes",
        "MARSH_JOB_OUTPUT_BYTES",
        SAFE_MAX,
        Applies::NewJobs,
        "bytes",
        "stdout and stderr together per job.",
    ),
    count(
        "job-wall-seconds",
        "MARSH_JOB_WALL_SECONDS",
        SAFE_MAX,
        Applies::NewJobs,
        "seconds",
        "Wall time per job; a child ends no later than its parent.",
    ),
];

/// The spec named `name` (kebab-case) or `variable`.
#[must_use]
pub fn spec(name: &str) -> Option<&'static LimitSpec> {
    LIMITS
        .iter()
        .find(|spec| spec.name == name || spec.variable == name)
}

/// How to raise a limit, for refusals: `marsh config limit NAME N or VAR`.
#[must_use]
pub fn raise(name: &str) -> String {
    spec(name).map_or_else(String::new, |spec| {
        format!(
            "raise it with `marsh config limit {} N` or {}",
            spec.name, spec.variable
        )
    })
}

impl LimitSpec {
    /// Parse and range-check a value given as text (a variable, or the CLI).
    ///
    /// # Errors
    /// Says what range the value must be in.
    pub fn parse(&self, text: &str) -> Result<LimitValue, String> {
        if self.rate {
            return LaunchRate::parse(text)
                .map(|rate| LimitValue::Rate(rate.to_string()))
                .map_err(|_| {
                    format!(
                        "{} must be LAUNCHES/SECONDS (e.g. 60/60s): 1-10000 launches in 1-3600 s, not {text:?}",
                        self.name
                    )
                });
        }
        let number = Some(text)
            .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|text| text.parse::<u64>().ok())
            .ok_or_else(|| format!("{} must be a whole number, not {text:?}", self.name))?;
        self.check(number).map(LimitValue::Count)
    }

    fn check(&self, number: u64) -> Result<u64, String> {
        if (self.min..=self.max).contains(&number) {
            Ok(number)
        } else {
            Err(format!(
                "{} must be between {} and {}, not {number}",
                self.name, self.min, self.max
            ))
        }
    }

    /// A value read from `config.json`: a number, or a rate's text.
    fn read_json(&self, value: &serde_json::Value) -> Result<LimitValue, String> {
        match value {
            serde_json::Value::Number(number) if !self.rate => number
                .as_u64()
                .ok_or_else(|| format!("{} must be a whole number", self.name))
                .and_then(|number| self.check(number))
                .map(LimitValue::Count),
            serde_json::Value::String(text) => self.parse(text),
            _ => Err(format!("{} has the wrong type", self.name)),
        }
    }

    /// The range as JSON, for `marsh config limits --json`.
    #[must_use]
    pub fn range_json(&self) -> serde_json::Value {
        if self.rate {
            serde_json::json!({
                "launches": {"min": 1, "max": 10_000},
                "window_seconds": {"min": 1, "max": 3600},
            })
        } else {
            serde_json::json!({"min": self.min, "max": self.max})
        }
    }

    #[must_use]
    pub fn range_text(&self) -> String {
        if self.rate {
            "1-10000 / 1-3600s".into()
        } else {
            format!("{}-{}", self.min, self.max)
        }
    }
}

/// CPU and memory per job, scaled to the host: CPU = max(2, half the
/// cores); memory = max(4 GiB, min(8 GiB, a quarter of RAM)).
#[must_use]
pub fn host_job_resources(cores: u64, memory_bytes: u64) -> JobResources {
    const GIB: u64 = 1 << 30;
    let cpus = (cores / 2).max(2);
    JobResources {
        cpu_millis: u32::try_from(cpus * 1000).unwrap_or(u32::MAX),
        memory_bytes: (memory_bytes / 4).clamp(4 * GIB, 8 * GIB),
        ..crate::DEFAULT_JOB_RESOURCES
    }
}

/// This host's cores and RAM (0 when unknown), read once.
#[must_use]
pub fn host_size() -> (u64, u64) {
    static SIZE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(0, |cores| cores.get() as u64);
        let memory = if cfg!(target_os = "macos") {
            std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "hw.memsize"])
                .stderr(std::process::Stdio::null())
                .output()
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|text| text.trim().parse::<u64>().ok())
        } else {
            std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find_map(|line| line.strip_prefix("MemTotal:"))
                        .and_then(|rest| {
                            rest.trim()
                                .trim_end_matches("kB")
                                .trim()
                                .parse::<u64>()
                                .ok()
                        })
                        .map(|kib| kib * 1024)
                })
        };
        (cores, memory.unwrap_or(0))
    })
}

/// Every limit's value, captured or effective (`JobDefaults` keeps the
/// name it always had: it is in `marsh status --json` as `job_defaults`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobDefaults {
    pub resources: JobResources,
    /// Variables set in the daemon's launch environment. They win over
    /// `config.json` for the daemon's life.
    pub environment_overrides: Vec<String>,
    /// Distinct Kit VMs one job tree may use at once (`tree-kit-vms`).
    #[serde(default = "default_tree_kit_vms")]
    pub tree_kit_vms: usize,
    /// Launches one job tree may make in a rolling window
    /// (`tree-launch-rate`, e.g. `60/60s`).
    #[serde(default = "default_tree_launch_rate")]
    pub tree_launch_rate: LaunchRate,
    #[serde(default = "default_daemon_jobs")]
    pub daemon_jobs: usize,
    #[serde(default = "default_job_children")]
    pub job_children: usize,
    #[serde(default = "default_tree_depth")]
    pub tree_depth: u32,
    #[serde(default = "default_tasks_live")]
    pub tasks_live: usize,
    /// Limits whose value came from `config.json` (names).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_overrides: Vec<String>,
}

fn default_tree_launch_rate() -> LaunchRate {
    process::TREE_LAUNCH_RATE
}
fn default_tree_kit_vms() -> usize {
    process::TREE_KIT_VM_LIMIT
}
fn default_daemon_jobs() -> usize {
    process::POOL_LIMIT
}
fn default_job_children() -> usize {
    process::FAN_OUT_LIMIT
}
fn default_tree_depth() -> u32 {
    process::DEPTH_LIMIT
}
fn default_tasks_live() -> usize {
    process::TASKS_LIVE
}

impl Default for JobDefaults {
    fn default() -> Self {
        let (cores, memory) = host_size();
        Self::for_host(cores, memory)
    }
}

impl JobDefaults {
    /// The defaults on a host with `cores` and `memory_bytes`.
    #[must_use]
    pub fn for_host(cores: u64, memory_bytes: u64) -> Self {
        Self {
            resources: host_job_resources(cores, memory_bytes),
            environment_overrides: Vec::new(),
            tree_kit_vms: default_tree_kit_vms(),
            tree_launch_rate: default_tree_launch_rate(),
            daemon_jobs: default_daemon_jobs(),
            job_children: default_job_children(),
            tree_depth: default_tree_depth(),
            tasks_live: default_tasks_live(),
            config_overrides: Vec::new(),
        }
    }

    /// The daemon's snapshot: the defaults, then its launch environment.
    ///
    /// # Errors
    /// Names the variable that is malformed or out of range.
    pub fn from_environment(
        mut value: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self, DaemonError> {
        let mut result = Self::default();
        for spec in &LIMITS {
            let Some(raw) = value(spec.variable) else {
                continue;
            };
            let text = raw.to_str().ok_or_else(|| {
                DaemonError::InvalidState(format!("{} must be text", spec.variable))
            })?;
            let parsed = if spec.rate {
                LaunchRate::parse(text)
                    .map(|rate| LimitValue::Rate(rate.to_string()))
                    .map_err(DaemonError::InvalidState)?
            } else {
                let number = Some(text)
                    .filter(|text| {
                        !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
                    })
                    .and_then(|text| text.parse::<u64>().ok())
                    .filter(|number| *number > 0)
                    .ok_or_else(|| {
                        DaemonError::InvalidState(format!(
                            "{} must be a positive decimal integer within its supported range",
                            spec.variable
                        ))
                    })?;
                if number > spec.max {
                    return Err(DaemonError::InvalidState(
                        if spec.max == u64::from(u32::MAX) {
                            format!("{} exceeds u32", spec.variable)
                        } else {
                            format!(
                                "{} must be between {} and {}",
                                spec.variable, spec.min, spec.max
                            )
                        },
                    ));
                }
                LimitValue::Count(number)
            };
            result.set(spec.name, &parsed);
            result.environment_overrides.push(spec.variable.into());
        }
        Ok(result)
    }

    /// The value of the limit `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> LimitValue {
        let count = |value: usize| LimitValue::Count(value as u64);
        match name {
            "daemon-jobs" => count(self.daemon_jobs),
            "job-children" => count(self.job_children),
            "tree-depth" => LimitValue::Count(u64::from(self.tree_depth)),
            "tree-kit-vms" => count(self.tree_kit_vms),
            "tasks-live" => count(self.tasks_live),
            "tree-launch-rate" => LimitValue::Rate(self.tree_launch_rate.to_string()),
            "job-cpu-millis" => LimitValue::Count(u64::from(self.resources.cpu_millis)),
            "job-memory-bytes" => LimitValue::Count(self.resources.memory_bytes),
            "job-pids" => LimitValue::Count(u64::from(self.resources.pids)),
            "job-writable-bytes" => LimitValue::Count(self.resources.writable_bytes),
            "job-output-bytes" => LimitValue::Count(self.resources.output_bytes),
            "job-wall-seconds" => LimitValue::Count(self.resources.wall_seconds),
            _ => unreachable!("limit {name}"),
        }
    }

    /// Set the limit `name` to an already validated value.
    fn set(&mut self, name: &str, value: &LimitValue) {
        let number = match value {
            LimitValue::Count(number) => *number,
            LimitValue::Rate(text) => {
                if let Ok(rate) = LaunchRate::parse(text) {
                    self.tree_launch_rate = rate;
                }
                return;
            }
        };
        let small = |number: u64| usize::try_from(number).unwrap_or(usize::MAX);
        let word = |number: u64| u32::try_from(number).unwrap_or(u32::MAX);
        match name {
            "daemon-jobs" => self.daemon_jobs = small(number),
            "job-children" => self.job_children = small(number),
            "tree-depth" => self.tree_depth = word(number),
            "tree-kit-vms" => self.tree_kit_vms = small(number),
            "tasks-live" => self.tasks_live = small(number),
            "job-cpu-millis" => self.resources.cpu_millis = word(number),
            "job-memory-bytes" => self.resources.memory_bytes = number,
            "job-pids" => self.resources.pids = word(number),
            "job-writable-bytes" => self.resources.writable_bytes = number,
            "job-output-bytes" => self.resources.output_bytes = number,
            "job-wall-seconds" => self.resources.wall_seconds = number,
            _ => {}
        }
    }

    /// Where the limit `name` comes from in this effective value.
    #[must_use]
    pub fn source(&self, spec: &LimitSpec) -> Source {
        if self
            .environment_overrides
            .iter()
            .any(|name| name == spec.variable)
        {
            Source::Env
        } else if self.config_overrides.iter().any(|name| name == spec.name) {
            Source::Config
        } else {
            Source::Default
        }
    }

    /// This snapshot with `config` applied to the limits the environment
    /// did not set. A value out of range is skipped (`marsh config limits`
    /// reports it).
    #[must_use]
    pub fn with_config(&self, config: &ConfigLimits) -> Self {
        let mut effective = self.clone();
        effective.config_overrides.clear();
        for spec in &LIMITS {
            if effective.source(spec) == Source::Env {
                continue;
            }
            if let Some(Ok(value)) = config.values.get(spec.name) {
                effective.set(spec.name, value);
                effective.config_overrides.push(spec.name.into());
            }
        }
        effective
    }

    /// Only what the launch environment set, over the defaults: a running
    /// daemon's snapshot, recovered from its effective values.
    #[must_use]
    pub fn environment_part(&self) -> Self {
        let mut snapshot = Self::default();
        for spec in &LIMITS {
            if self.source(spec) == Source::Env {
                snapshot.set(spec.name, &self.get(spec.name));
                snapshot.environment_overrides.push(spec.variable.into());
            }
        }
        snapshot
    }

    /// The limits the job capability carries into `job.json`.
    #[must_use]
    pub fn process_limits(&self) -> process::Limits {
        process::Limits {
            pool: self.daemon_jobs,
            depth: self.tree_depth,
            fan_out: self.job_children,
            launch_rate: self.tree_launch_rate,
            tasks_live: self.tasks_live,
        }
    }

    /// `marsh status --json` `limits`: `{NAME: {value, default, source}}`.
    #[must_use]
    pub fn status(&self) -> BTreeMap<String, LimitStatus> {
        let defaults = Self::default();
        LIMITS
            .iter()
            .map(|spec| {
                (
                    spec.name.to_owned(),
                    LimitStatus {
                        value: self.get(spec.name),
                        default: defaults.get(spec.name),
                        source: self.source(spec),
                    },
                )
            })
            .collect()
    }

    /// One entry per limit, in order, for `marsh config limits [--json]`.
    #[must_use]
    pub fn listing(&self, config: &ConfigLimits) -> Vec<LimitEntry> {
        let defaults = Self::default();
        LIMITS
            .iter()
            .map(|spec| LimitEntry {
                name: spec.name,
                value: self.get(spec.name),
                default: defaults.get(spec.name),
                source: self.source(spec),
                variable: spec.variable,
                kind: if spec.rate { "rate" } else { "count" },
                range: spec.range_json(),
                applies: spec.applies,
                unit: spec.unit,
                description: spec.description,
                config: config.values.get(spec.name).map(|value| match value {
                    Ok(value) => serde_json::to_value(value).unwrap_or_default(),
                    Err((raw, _)) => raw.clone(),
                }),
                config_error: config
                    .values
                    .get(spec.name)
                    .and_then(|value| value.as_ref().err().map(|(_, error)| error.clone())),
            })
            .collect()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LimitStatus {
    pub value: LimitValue,
    pub default: LimitValue,
    pub source: Source,
}

/// One row of `marsh config limits --json`.
#[derive(Clone, Debug, Serialize)]
pub struct LimitEntry {
    pub name: &'static str,
    pub value: LimitValue,
    pub default: LimitValue,
    pub source: Source,
    pub variable: &'static str,
    pub kind: &'static str,
    pub range: serde_json::Value,
    pub applies: Applies,
    pub unit: &'static str,
    pub description: &'static str,
    /// The value in `config.json`, even when the environment wins over it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Why the `config.json` value is not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
}

/// The `limits` object of a scope's `config.json`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConfigLimits {
    /// Name -> the value, or the raw JSON and why it is not used.
    pub values: BTreeMap<String, Result<LimitValue, (serde_json::Value, String)>>,
}

impl ConfigLimits {
    /// Read `config.json`; a missing or unreadable file, or one without
    /// `limits`, has none.
    #[must_use]
    pub fn read(config: &Path) -> Self {
        let Some(document) = std::fs::read(config)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        else {
            return Self::default();
        };
        Self::from_document(&document)
    }

    #[must_use]
    pub fn from_document(document: &serde_json::Value) -> Self {
        let mut values = BTreeMap::new();
        if let Some(limits) = document
            .get(CONFIG_KEY)
            .and_then(serde_json::Value::as_object)
        {
            for (name, raw) in limits {
                let value = match spec(name).filter(|spec| spec.name == name) {
                    Some(spec) => spec.read_json(raw),
                    None => Err(format!("{name} is not a limit")),
                };
                values.insert(name.clone(), value.map_err(|error| (raw.clone(), error)));
            }
        }
        Self { values }
    }
}

/// Reads a scope's `config.json` limits, again only when the file changed.
#[derive(Debug, Default)]
pub(crate) struct ConfigCache {
    path: Option<PathBuf>,
    seen: std::sync::Mutex<Option<(Option<std::time::SystemTime>, u64, ConfigLimits)>>,
}

impl ConfigCache {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            seen: std::sync::Mutex::default(),
        }
    }

    pub(crate) fn current(&self) -> ConfigLimits {
        let Some(path) = &self.path else {
            return ConfigLimits::default();
        };
        let stamp = std::fs::metadata(path)
            .ok()
            .map(|metadata| (metadata.modified().ok(), metadata.len()));
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((modified, length)) = stamp else {
            *seen = None;
            return ConfigLimits::default();
        };
        if let Some((seen_modified, seen_length, limits)) = seen.as_ref()
            && *seen_modified == modified
            && *seen_length == length
        {
            return limits.clone();
        }
        let limits = ConfigLimits::read(path);
        *seen = Some((modified, length, limits.clone()));
        limits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    #[test]
    fn cpu_and_memory_scale_to_the_host() {
        let small = host_job_resources(4, 8 * GIB);
        assert_eq!((small.cpu_millis, small.memory_bytes), (2_000, 4 * GIB));
        let mid = host_job_resources(10, 24 * GIB);
        assert_eq!((mid.cpu_millis, mid.memory_bytes), (5_000, 6 * GIB));
        let large = host_job_resources(16, 128 * GIB);
        assert_eq!((large.cpu_millis, large.memory_bytes), (8_000, 8 * GIB));
        let unknown = host_job_resources(0, 0);
        assert_eq!((unknown.cpu_millis, unknown.memory_bytes), (2_000, 4 * GIB));
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let defaults = JobDefaults::for_host(8, 16 * GIB);
        assert_eq!(defaults.daemon_jobs, 64);
        assert_eq!(defaults.job_children, 16);
        assert_eq!(defaults.tree_depth, 4);
        assert_eq!(defaults.tree_kit_vms, 8);
        assert_eq!(defaults.tasks_live, 16);
        assert_eq!(defaults.tree_launch_rate.to_string(), "60/60s");
        assert_eq!(LIMITS.len(), 12);
    }

    #[test]
    fn environment_beats_config_which_beats_the_default() {
        let snapshot = JobDefaults::from_environment(|name| {
            (name == "MARSH_JOB_CHILDREN").then(|| "20".into())
        })
        .unwrap();
        let config = ConfigLimits::from_document(&serde_json::json!({"limits": {
            "job-children": 4,
            "daemon-jobs": 100,
            "tree-launch-rate": "120/2m",
            "tree-depth": 99,
            "bogus": 1,
        }}));
        let effective = snapshot.with_config(&config);
        assert_eq!(effective.job_children, 20);
        assert_eq!(effective.daemon_jobs, 100);
        assert_eq!(effective.tree_launch_rate.to_string(), "120/120s");
        assert_eq!(effective.tree_depth, 4, "out of range is skipped");
        let status = effective.status();
        assert_eq!(status["job-children"].source, Source::Env);
        assert_eq!(status["daemon-jobs"].source, Source::Config);
        assert_eq!(status["tree-depth"].source, Source::Default);
        let listing = effective.listing(&config);
        let depth = listing
            .iter()
            .find(|entry| entry.name == "tree-depth")
            .unwrap();
        assert!(
            depth
                .config_error
                .as_deref()
                .unwrap()
                .contains("between 1 and 16")
        );
    }

    #[test]
    fn variables_are_range_checked() {
        for (variable, bad) in [
            ("MARSH_DAEMON_JOBS", "513"),
            ("MARSH_JOB_CHILDREN", "129"),
            ("MARSH_TREE_DEPTH", "17"),
            ("MARSH_TREE_KIT_VMS", "65"),
            ("MARSH_TASKS_LIVE", "0"),
        ] {
            let error =
                JobDefaults::from_environment(|name| (name == variable).then(|| bad.into()))
                    .unwrap_err();
            assert!(error.to_string().contains(variable), "{error}");
        }
        let ok = JobDefaults::from_environment(|name| {
            (name == "MARSH_DAEMON_JOBS").then(|| "512".into())
        })
        .unwrap();
        assert_eq!(ok.daemon_jobs, 512);
        assert!(spec("tree-depth").unwrap().parse("0").is_err());
        assert_eq!(
            spec("tree-launch-rate").unwrap().parse("30/1m").unwrap(),
            LimitValue::Rate("30/60s".into())
        );
    }
}
