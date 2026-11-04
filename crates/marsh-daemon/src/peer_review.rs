//! Peer review by another vendor's model (`docs/peer-review.md`).
//!
//! marsh owns the model classes: a curated table (`data/model-classes.yaml`,
//! overridable in `$MARSH_HOME/model-classes.yaml`) and, for a model no rule
//! names, its output price in a models.dev snapshot taken in CI
//! (`data/models-dev.json`, `scripts/update-models-dev.py`); nothing is
//! fetched at run time. Who may review comes from `$MARSH_HOME/peer-review.d/
//! *.yaml` drop-ins (Narwhal writes `narwhal.yaml`, `marsh config peer-review
//! on` writes `marsh.yaml`), read whenever they are needed (every job start
//! and `marsh status`), so an edit applies to the next job.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use marsh_contracts::process::ModelChoice;

/// The drop-in directory under `$MARSH_HOME`.
pub const DROPIN_DIR: &str = "peer-review.d";
/// The drop-in `marsh config peer-review on|off` writes.
pub const MARSH_DROPIN: &str = "marsh.yaml";
/// The class table override under `$MARSH_HOME`.
pub const OVERRIDE_FILE: &str = "model-classes.yaml";
const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_REVIEWERS: usize = 64;
const MAX_NAME_BYTES: usize = 128;
/// Rows of the reviewer table in `context.md`.
const CONTEXT_ROWS: usize = 8;
/// Models listed per agent in the `task` tool's description.
const CHOICE_ROWS: usize = 8;
/// A family whose newest model is older than the vendor's newest listed
/// model by more than this many days is left out of the list.
const CHOICE_RECENT_DAYS: i64 = 180;

const BUILTIN_TABLE: &str = include_str!("../data/model-classes.yaml");
const SNAPSHOT: &str = include_str!("../data/models-dev.json");
const ALIASES: &str = include_str!("../data/vendor-aliases.json");

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelClass {
    Small,
    Mid,
    Large,
}

impl ModelClass {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Small => "small",
            Self::Mid => "mid",
            Self::Large => "large",
        }
    }

    pub const ALL: [Self; 3] = [Self::Small, Self::Mid, Self::Large];
}

/// Output-price bands (USD per million output tokens) for models no rule
/// names: below `small_below` is small, from `large_from` up is large.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bands {
    pub small_below: f64,
    pub large_from: f64,
}

impl Bands {
    #[must_use]
    pub fn class(&self, output: f64) -> ModelClass {
        if output < self.small_below {
            ModelClass::Small
        } else if output >= self.large_from {
            ModelClass::Large
        } else {
            ModelClass::Mid
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rule {
    #[serde(rename = "match")]
    pattern: String,
    class: ModelClass,
    vendor: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TableFile {
    #[serde(default)]
    bands: Option<Bands>,
    #[serde(default)]
    rules: Vec<Rule>,
}

/// How a model was classed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Classified {
    pub class: ModelClass,
    /// The model's maker; `None` when models.dev names none that marsh can
    /// trust (the host that lists a model is never its vendor), so it can
    /// only be paired with an explicit `vendor`.
    pub vendor: Option<String>,
    /// `model-classes: claude-sonnet-*`, `$MARSH_HOME/model-classes.yaml:
    /// my-*`, `models.dev family gpt-sol: gpt-*sol*`, or `models.dev price
    /// $5/M output`.
    pub how: String,
}

/// The effective class table: override rules first, then marsh's.
#[derive(Clone, Debug)]
pub struct ClassTable {
    rules: Vec<(Rule, &'static str)>,
    bands: Bands,
}

const BUILTIN: &str = "model-classes";
const OVERRIDE: &str = "$MARSH_HOME/model-classes.yaml";

impl ClassTable {
    /// marsh's curated table alone.
    ///
    /// # Panics
    /// When the shipped table is malformed (a unit test keeps it parsing).
    #[must_use]
    pub fn builtin() -> Self {
        let file: TableFile =
            serde_yaml::from_str(BUILTIN_TABLE).expect("data/model-classes.yaml parses");
        Self {
            rules: file.rules.into_iter().map(|rule| (rule, BUILTIN)).collect(),
            bands: file.bands.expect("data/model-classes.yaml has bands"),
        }
    }

    /// marsh's table with `$MARSH_HOME/model-classes.yaml` over it. A
    /// malformed override is ignored and its error returned.
    #[must_use]
    pub fn load(scope: &Path) -> (Self, Option<String>) {
        let mut table = Self::builtin();
        let path = scope.join(OVERRIDE_FILE);
        match read_small(&path) {
            Ok(None) => (table, None),
            Ok(Some(text)) => match parse_table(&text) {
                Ok(file) => {
                    if let Some(bands) = file.bands {
                        table.bands = bands;
                    }
                    let mut rules = file
                        .rules
                        .into_iter()
                        .map(|rule| (rule, OVERRIDE))
                        .collect::<Vec<_>>();
                    rules.append(&mut table.rules);
                    table.rules = rules;
                    (table, None)
                }
                Err(error) => (table, Some(format!("{}: {error}", path.display()))),
            },
            Err(error) => (table, Some(error)),
        }
    }

    #[must_use]
    pub const fn bands(&self) -> Bands {
        self.bands
    }

    fn rule_for(&self, text: &str) -> Option<&(Rule, &'static str)> {
        self.rules
            .iter()
            .find(|(rule, _)| glob(&rule.pattern.to_ascii_lowercase(), text))
    }

    /// The class and vendor of `model` (an id as the agent's model option
    /// takes it, optionally `provider/model`): the first rule matching its
    /// name or its models.dev family, else its models.dev output price.
    #[must_use]
    pub fn classify(&self, model: &str) -> Option<Classified> {
        let name = normalize(model);
        if name.is_empty() {
            return None;
        }
        if let Some((rule, origin)) = self.rule_for(&name) {
            return Some(Classified {
                class: rule.class,
                vendor: Some(rule.vendor.clone()),
                how: format!("{origin}: {}", rule.pattern),
            });
        }
        let row = lookup(model)?;
        let family = row.family.to_ascii_lowercase();
        if !family.is_empty()
            && let Some((rule, origin)) = self.rule_for(&family)
        {
            return Some(Classified {
                class: rule.class,
                vendor: Some(rule.vendor.clone()),
                how: format!("models.dev family {family}; {origin}: {}", rule.pattern),
            });
        }
        Some(Classified {
            class: self.bands.class(row.output),
            vendor: (!row.vendor.is_empty()).then(|| row.vendor.clone()),
            how: format!("models.dev price ${}/M output", row.output),
        })
    }

    /// The newest model of `vendor` in `class` that the vendor itself serves,
    /// by the rules alone (what `marsh config peer-review on` offers).
    #[must_use]
    pub fn newest(&self, vendor: &str, class: ModelClass) -> Option<String> {
        let mut best: Option<&SnapshotModel> = None;
        for row in snapshot()
            .models
            .iter()
            .filter(|row| row.vendor == vendor && row.first_party() && plain_chat_model(&row.id))
        {
            let name = normalize(&row.id);
            let family = row.family.to_ascii_lowercase();
            let Some((rule, _)) = self.rule_for(&name).or_else(|| {
                (!family.is_empty())
                    .then(|| self.rule_for(&family))
                    .flatten()
            }) else {
                continue;
            };
            if rule.class != class || rule.vendor != vendor {
                continue;
            }
            let better = best.is_none_or(|best| {
                (row.released.as_str(), std::cmp::Reverse(row.id.len()))
                    > (best.released.as_str(), std::cmp::Reverse(best.id.len()))
            });
            if better {
                best = Some(row);
            }
        }
        best.map(|row| row.id.clone())
    }

    /// The models of `vendors` an agent can be asked for by name: each
    /// family's newest model the vendor itself serves (`luna`, `terra`,
    /// `sol`, `astra`; `haiku`, `sonnet`), without families a half year
    /// behind the vendor's newest, at most `CHOICE_ROWS` rows by recency,
    /// ordered by class. `qualify` writes `vendor/model`, the id a
    /// multi-provider agent takes.
    #[must_use]
    pub fn choices(&self, vendors: &[String], qualify: bool) -> Vec<ModelChoice> {
        let mut picked = Vec::<(String, &SnapshotModel, ModelClass, String)>::new();
        for vendor in vendors {
            let mut families = BTreeMap::<String, (&SnapshotModel, ModelClass)>::new();
            for row in snapshot().models.iter().filter(|row| {
                row.vendor == *vendor && row.first_party() && plain_chat_model(&row.id)
            }) {
                let name = normalize(&row.id);
                let family = row.family.to_ascii_lowercase();
                let Some((rule, _)) = self.rule_for(&name).or_else(|| {
                    (!family.is_empty())
                        .then(|| self.rule_for(&family))
                        .flatten()
                }) else {
                    continue;
                };
                if rule.vendor != *vendor {
                    continue;
                }
                let key = family_nickname(&rule.pattern);
                let better = families.get(&key).is_none_or(|(best, _)| {
                    (row.released.as_str(), std::cmp::Reverse(row.id.len()))
                        > (best.released.as_str(), std::cmp::Reverse(best.id.len()))
                });
                if better {
                    families.insert(key, (row, rule.class));
                }
            }
            let latest = families
                .values()
                .map(|(row, _)| day_number(&row.released))
                .max()
                .unwrap_or(0);
            for (key, (row, class)) in families {
                if day_number(&row.released) + CHOICE_RECENT_DAYS >= latest {
                    picked.push((vendor.clone(), row, class, key));
                }
            }
        }
        picked.sort_by(|a, b| b.1.released.cmp(&a.1.released).then(a.1.id.cmp(&b.1.id)));
        picked.truncate(CHOICE_ROWS);
        picked.sort_by(|a, b| {
            (a.2, &a.0, std::cmp::Reverse(&a.1.released)).cmp(&(
                b.2,
                &b.0,
                std::cmp::Reverse(&b.1.released),
            ))
        });
        picked
            .into_iter()
            .map(|(vendor, row, class, family)| ModelChoice {
                model: if qualify {
                    format!("{vendor}/{}", row.id)
                } else {
                    row.id.clone()
                },
                class: class.name().to_owned(),
                family,
            })
            .collect()
    }
}

/// The nickname people use for the family a rule names: its last word that
/// is not a version (`gpt-*luna*` is `luna`, `claude-*-haiku*` is `haiku`).
fn family_nickname(pattern: &str) -> String {
    let pattern = pattern.to_ascii_lowercase();
    let words = pattern
        .split(['*', '-'])
        .filter(|word| !word.is_empty() && !word.starts_with(|c: char| c.is_ascii_digit()))
        .collect::<Vec<_>>();
    match words.as_slice() {
        // `mistral-large*` is `mistral-large`: a size alone names no family.
        [first, .., size @ ("small" | "medium" | "large")] => format!("{first}-{size}"),
        [.., last] => (*last).to_owned(),
        [] => pattern,
    }
}

/// Days since year 0 (365-day years, 31-day months: only for ordering and
/// distances of release dates `YYYY-MM-DD`); 0 for anything else.
fn day_number(date: &str) -> i64 {
    let mut parts = date.split('-').map(|part| part.parse::<i64>().ok());
    match (
        parts.next().flatten(),
        parts.next().flatten(),
        parts.next().flatten(),
    ) {
        (Some(year), Some(month), Some(day)) => year * 372 + month * 31 + day,
        _ => 0,
    }
}

/// The models an agent can run, from its Kit's credential services and, for
/// a multi-provider agent, the vendors its Kit reaches: what the `task`
/// tool lists for it (`ClassTable::choices`).
#[must_use]
pub fn agent_choices(
    table: &ClassTable,
    agent: &str,
    services: &[String],
    reaches: &BTreeMap<String, Vec<String>>,
) -> Vec<ModelChoice> {
    let any = services.iter().any(|service| service == ANY_PROVIDER);
    let reaches = reaches
        .iter()
        .map(|(service, hosts)| (service.clone(), hosts.iter().cloned().collect()))
        .collect::<BTreeMap<String, BTreeSet<String>>>();
    let vendors = if any {
        VENDOR_HOSTS
            .iter()
            .map(|(vendor, _)| (*vendor).to_owned())
            .filter(|vendor| cannot_reach(agent, vendor, &reaches).is_none())
            .collect::<Vec<_>>()
    } else {
        services
            .iter()
            .map(|service| vendor_name(service))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    table.choices(&vendors, any)
}

/// `nickname` (a family such as `astra`) as the model id `choices` gives it;
/// `None` when it names no family, or the id itself.
///
/// # Errors
/// Returns the candidates when several vendors have a family of that name.
pub fn resolve_family(
    choices: &[ModelChoice],
    nickname: &str,
) -> Result<Option<ModelChoice>, String> {
    let nickname = nickname.trim().to_ascii_lowercase();
    let found = choices
        .iter()
        .filter(|choice| choice.family == nickname)
        .collect::<Vec<_>>();
    match found.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some((*one).clone())),
        many => Err(format!(
            "`{nickname}` is more than one model here ({}): name one",
            many.iter()
                .map(|choice| choice.model.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The list of models in the `task` tool's description: `NAME: MODEL (CLASS,
/// FAMILY), ...` per agent with models.
#[must_use]
pub fn describe_choices(choices: &BTreeMap<String, Vec<ModelChoice>>) -> String {
    choices
        .iter()
        .filter(|(_, models)| !models.is_empty())
        .map(|(agent, models)| {
            let models = models
                .iter()
                .map(|choice| format!("{} ({}, {})", choice.model, choice.class, choice.family))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{agent}: {models}")
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse_table(text: &str) -> Result<TableFile, String> {
    let file: TableFile = serde_yaml::from_str(text).map_err(|error| error.to_string())?;
    if let Some(bands) = file.bands
        && !(bands.small_below.is_finite()
            && bands.large_from.is_finite()
            && 0.0 <= bands.small_below
            && bands.small_below <= bands.large_from)
    {
        return Err("bands: need 0 <= small_below <= large_from".into());
    }
    for rule in &file.rules {
        if rule.pattern.trim().is_empty() || rule.pattern.len() > MAX_NAME_BYTES {
            return Err(format!("rule {:?}: `match` is 1-128 bytes", rule.pattern));
        }
        valid_vendor(&rule.vendor).map_err(|error| format!("rule {:?}: {error}", rule.pattern))?;
    }
    let mut file = file;
    for rule in &mut file.rules {
        rule.vendor = vendor_name(&rule.vendor);
    }
    Ok(file)
}

/// Ids that are not ordinary chat models, or are moving aliases.
fn plain_chat_model(id: &str) -> bool {
    const SKIP: [&str; 14] = [
        "latest",
        "preview",
        "-exp",
        "image",
        "tts",
        "live",
        "realtime",
        "audio",
        "search",
        "research",
        "codex",
        "computer-use",
        "customtools",
        "translate",
    ];
    let id = id.to_ascii_lowercase();
    !SKIP.iter().any(|word| id.contains(word))
        // Dated snapshots (`...-20250929`) repeat their undated name.
        && !id
            .rsplit('-')
            .next()
            .is_some_and(|tail| tail.len() == 8 && tail.bytes().all(|byte| byte.is_ascii_digit()))
}

/// A model id as the rules see it: lowercase, without `provider/`, a cloud
/// region or vendor prefix (`us.`, `global.anthropic.`), or a host suffix
/// (`@eu`, `:free`).
fn normalize(model: &str) -> String {
    const PREFIXES: [&str; 16] = [
        "global.",
        "us.",
        "eu.",
        "apac.",
        "jp.",
        "au.",
        "anthropic.",
        "openai.",
        "google.",
        "meta.",
        "mistral.",
        "deepseek.",
        "qwen.",
        "moonshotai.",
        "zai.",
        "minimax.",
    ];
    let model = model.trim().to_ascii_lowercase();
    let mut name = model.rsplit('/').next().unwrap_or_default();
    name = name.split(['@', ':']).next().unwrap_or_default();
    while let Some(rest) = PREFIXES.iter().find_map(|prefix| name.strip_prefix(prefix)) {
        name = rest;
    }
    name.to_owned()
}

/// A model's name across hosts (`grok-4.6` on one, `grok-4-6` on another),
/// as the snapshot generator groups them.
fn name_key(model: &str) -> String {
    normalize(model).replace('.', "-")
}

/// `*` matches any run of characters; everything else matches itself.
fn glob(pattern: &str, text: &str) -> bool {
    let parts = pattern.split('*').collect::<Vec<_>>();
    if parts.len() == 1 {
        return pattern == text;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    let mut rest = &text[first.len()..text.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    true
}

#[derive(Debug, Deserialize)]
struct SnapshotModel {
    provider: String,
    id: String,
    family: String,
    /// The model's maker, canonical (`vendor_name`); "" when unknown.
    vendor: String,
    output: f64,
    #[allow(dead_code)]
    open_weights: bool,
    released: String,
}

impl SnapshotModel {
    /// The vendor's own listing of its model.
    fn first_party(&self) -> bool {
        !self.vendor.is_empty() && vendor_name(&self.provider) == self.vendor
    }
}

/// A maker's canonical vendor id: lowercase, a leading `~` dropped, and
/// spellings such as `x-ai` or `zhipuai` mapped by `data/vendor-aliases.json`
/// (which the snapshot generator applies too).
///
/// # Panics
/// When the shipped alias file is malformed (a unit test keeps it parsing).
#[must_use]
pub fn vendor_name(name: &str) -> String {
    static ALIAS_MAP: OnceLock<BTreeMap<String, String>> = OnceLock::new();
    let aliases = ALIAS_MAP.get_or_init(|| {
        let file: serde_json::Value =
            serde_json::from_str(ALIASES).expect("data/vendor-aliases.json parses");
        file["aliases"]
            .as_object()
            .expect("vendor-aliases.json has aliases")
            .iter()
            .filter_map(|(from, to)| Some((from.clone(), to.as_str()?.to_owned())))
            .collect()
    });
    let name = name.trim().to_ascii_lowercase();
    let name = name.trim_start_matches('~');
    aliases
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_owned())
}

#[derive(Debug, Deserialize)]
struct SnapshotFile {
    #[allow(dead_code)]
    fields: Vec<String>,
    models: Vec<SnapshotModel>,
}

struct Snapshot {
    models: Vec<SnapshotModel>,
    by_name: BTreeMap<String, Vec<usize>>,
}

fn snapshot() -> &'static Snapshot {
    static SNAPSHOT_INDEX: OnceLock<Snapshot> = OnceLock::new();
    SNAPSHOT_INDEX.get_or_init(|| {
        // Rows are positional arrays (`fields`); serde reads them as structs.
        let file: SnapshotFile =
            serde_json::from_str(SNAPSHOT).expect("data/models-dev.json parses");
        let mut by_name = BTreeMap::<String, Vec<usize>>::new();
        for (index, row) in file.models.iter().enumerate() {
            by_name.entry(name_key(&row.id)).or_default().push(index);
        }
        Snapshot {
            models: file.models,
            by_name,
        }
    })
}

/// The snapshot row for `model`: the named provider's (`provider/model`),
/// else the one its vendor serves itself, else the first.
fn lookup(model: &str) -> Option<&'static SnapshotModel> {
    let snapshot = snapshot();
    let rows = snapshot.by_name.get(&name_key(model))?;
    let rows = rows.iter().map(|index| &snapshot.models[*index]);
    let provider = model
        .trim()
        .split_once('/')
        .map(|(provider, _)| provider.to_ascii_lowercase());
    let rows = rows.collect::<Vec<_>>();
    rows.iter()
        .find(|row| provider.as_deref() == Some(row.provider.as_str()))
        .or_else(|| rows.iter().find(|row| row.first_party()))
        .or_else(|| rows.iter().find(|row| !row.vendor.is_empty()))
        .or_else(|| rows.first())
        .copied()
}

fn valid_vendor(vendor: &str) -> Result<(), String> {
    if vendor.is_empty()
        || vendor.len() > 64
        || !vendor.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte)
        })
    {
        return Err(format!(
            "vendor {vendor:?} is 1-64 lowercase letters, digits, `-`, `_` or `.`"
        ));
    }
    Ok(())
}

/// A model id `task(model)` may pass to an agent's model option.
///
/// # Errors
/// Returns why `model` is not one.
pub fn valid_model(model: &str) -> Result<(), String> {
    if model.is_empty()
        || model.len() > MAX_NAME_BYTES
        || model.starts_with('-')
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/@[]".contains(&byte))
    {
        return Err(format!(
            "model {model:?} is 1-128 letters, digits and `-_.:/@[]`, not starting with `-`"
        ));
    }
    Ok(())
}

/// One drop-in file (`docs/peer-review.md`).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DropIn {
    /// The schema version: 1 (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// `true` asks for peer review; `false` in any file turns it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<DropInReviewer>,
}

/// One ready pairing: `agent` can run `model` (its vendor's credentials are
/// in place). `class` and `vendor` default to the model's classification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DropInReviewer {
    pub agent: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<ModelClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
}

impl DropIn {
    /// Parse and validate one drop-in.
    ///
    /// # Errors
    /// Returns why the text is not a valid drop-in.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: Self = if text.trim().is_empty() {
            Self::default()
        } else {
            serde_yaml::from_str(text).map_err(|error| error.to_string())?
        };
        if file.version.is_some_and(|version| version != 1) {
            return Err("version: only 1 is supported".into());
        }
        if file.reviewers.len() > MAX_REVIEWERS {
            return Err(format!("at most {MAX_REVIEWERS} reviewers per file"));
        }
        for (index, reviewer) in file.reviewers.iter().enumerate() {
            let at = |error: String| format!("reviewers[{index}]: {error}");
            marsh_contracts::command_registry::CommandName::parse(reviewer.agent.clone())
                .map_err(|error| at(format!("agent: {error}")))?;
            valid_model(&reviewer.model).map_err(at)?;
            if let Some(vendor) = &reviewer.vendor {
                valid_vendor(vendor).map_err(at)?;
            }
        }
        Ok(file)
    }

    /// The YAML `marsh config peer-review` writes.
    ///
    /// # Panics
    /// Never: the document always serializes.
    #[must_use]
    pub fn render(&self, comment: &str) -> String {
        let mut text = String::new();
        for line in comment.lines() {
            let _ = writeln!(text, "# {line}");
        }
        text.push_str(&serde_yaml::to_string(self).expect("drop-in serializes"));
        text
    }
}

/// One reviewer in the effective table.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Reviewer {
    pub class: ModelClass,
    pub vendor: String,
    pub agent: String,
    pub model: String,
    /// The drop-in that named it.
    pub source: PathBuf,
    /// Where class and vendor came from: `declared` or the classification.
    pub classified: String,
    /// Why it is not ready (only in `skipped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `marsh status` `peer_review`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PeerReviewStatus {
    /// A drop-in says `enabled: true` and none says `enabled: false`.
    pub enabled: bool,
    /// The drop-ins read, in name order.
    #[serde(default)]
    pub files: Vec<PathBuf>,
    /// Drop-ins that say `enabled: false`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_by: Vec<PathBuf>,
    /// Ready reviewers, by class then vendor.
    #[serde(default)]
    pub reviewers: Vec<Reviewer>,
    /// Named but not ready, each with its reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Reviewer>,
    /// Files (or the class override) that failed validation; a failing
    /// file contributes nothing.
    #[serde(default)]
    pub errors: Vec<String>,
}

/// What marsh knows about one registered agent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AgentFacts {
    /// Its Kit declares non-interactive prompt verbs (`agent-sessions@1`
    /// `prompt`): `task` can start it.
    pub prompt: bool,
    /// Its Kit declares a model option (`org.marsh/agent-model@1`).
    pub model_option: bool,
    /// Its Kit declares itself multi-provider (`services: ["*"]`): it can use
    /// a vendor's models only where `reaches` says its Kit can.
    pub multi_provider: bool,
    /// Per credential service, the hosts its Kit allows and injects that
    /// service's key for.
    pub reaches: BTreeMap<String, BTreeSet<String>>,
}

/// Read `$MARSH_HOME/peer-review.d/*.yaml` and the class table and resolve
/// the effective reviewers against the registered agents.
#[must_use]
pub fn load(scope: &Path, agents: &BTreeMap<String, AgentFacts>) -> PeerReviewStatus {
    let (table, table_error) = ClassTable::load(scope);
    let mut status = PeerReviewStatus::default();
    status.errors.extend(table_error);
    let directory = scope.join(DROPIN_DIR);
    let mut said_on = false;
    let mut seen = BTreeMap::<(String, String), PathBuf>::new();
    for path in dropin_files(&directory, &mut status.errors) {
        let file = match read_small(&path).and_then(|text| {
            DropIn::parse(&text.unwrap_or_default())
                .map_err(|error| format!("{}: {error}", path.display()))
        }) {
            Ok(file) => file,
            Err(error) => {
                status.errors.push(error);
                continue;
            }
        };
        status.files.push(path.clone());
        match file.enabled {
            Some(true) => said_on = true,
            Some(false) => status.disabled_by.push(path.clone()),
            None => {}
        }
        for entry in file.reviewers {
            let classified = table.classify(&entry.model);
            let declared = entry.class.is_some() && entry.vendor.is_some();
            let class = entry
                .class
                .or_else(|| classified.as_ref().map(|found| found.class));
            let vendor = entry
                .vendor
                .as_deref()
                .map(vendor_name)
                .or_else(|| classified.as_ref().and_then(|found| found.vendor.clone()));
            let mut reviewer = Reviewer {
                class: class.unwrap_or(ModelClass::Mid),
                vendor: vendor.clone().unwrap_or_default(),
                agent: entry.agent.clone(),
                model: entry.model.clone(),
                source: path.clone(),
                classified: if declared {
                    "declared".into()
                } else {
                    classified
                        .as_ref()
                        .map_or_else(|| "unknown".into(), |found| found.how.clone())
                },
                reason: None,
            };
            let key = (entry.agent.clone(), entry.model.to_ascii_lowercase());
            reviewer.reason = if class.is_none() {
                Some(format!(
                    "marsh cannot class {}: add `class` and `vendor`, or a rule in $MARSH_HOME/{OVERRIDE_FILE}",
                    entry.model
                ))
            } else if vendor.is_none() {
                Some(format!(
                    "marsh does not know who makes {} (models.dev names only hosts): add `vendor`",
                    entry.model
                ))
            } else if let Some(first) = seen.get(&key) {
                Some(format!("already named in {}", first.display()))
            } else {
                match agents.get(&entry.agent) {
                    None => Some(format!("{} is not a registered command", entry.agent)),
                    Some(facts) if !facts.prompt => Some(format!(
                        "{}'s Kit declares no prompt verbs (agent-sessions@1 `prompt`), so `task` cannot start it",
                        entry.agent
                    )),
                    Some(facts) if !facts.model_option => Some(format!(
                        "{}'s Kit declares no model option (org.marsh/agent-model@1)",
                        entry.agent
                    )),
                    Some(facts) if facts.multi_provider => {
                        cannot_reach(&entry.agent, &reviewer.vendor, &facts.reaches)
                    }
                    Some(_) => None,
                }
            };
            if reviewer.reason.is_some() {
                status.skipped.push(reviewer);
            } else {
                seen.insert(key, path.clone());
                status.reviewers.push(reviewer);
            }
        }
    }
    status.enabled = said_on && status.disabled_by.is_empty();
    status.reviewers.sort_by(|a, b| {
        (a.class, &a.vendor, &a.agent, &a.model).cmp(&(b.class, &b.vendor, &b.agent, &b.model))
    });
    status
}

/// `peer-review.d/*.yaml` (and `.yml`) in name order; dotfiles are skipped.
fn dropin_files(directory: &Path, errors: &mut Vec<String>) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            errors.push(format!("{}: {error}", directory.display()));
            return Vec::new();
        }
    };
    let mut names = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| {
            !name.starts_with('.')
                && (name.strip_suffix(".yaml").is_some() || name.strip_suffix(".yml").is_some())
        })
        .collect::<Vec<_>>();
    names.sort();
    if names.len() > MAX_FILES {
        errors.push(format!(
            "{}: only the first {MAX_FILES} drop-ins are read",
            directory.display()
        ));
        names.truncate(MAX_FILES);
    }
    names.into_iter().map(|name| directory.join(name)).collect()
}

/// A small regular file's text; `None` when it does not exist.
fn read_small(path: &Path) -> Result<Option<String>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    if !metadata.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Err(format!("{}: over {MAX_FILE_BYTES} bytes", path.display()));
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// The section appended to a job's `context.md` while peer review is on:
/// the instruction and the ready reviewers this job may start. Empty when
/// peer review is off or no reviewer is in the job's spawn set.
#[must_use]
pub fn context(status: &PeerReviewStatus, spawn: &[String]) -> String {
    if !status.enabled {
        return String::new();
    }
    // One reviewer per class and vendor (the first, in table order), at
    // most CONTEXT_ROWS rows: the section stays short.
    let mut seen = BTreeSet::new();
    let rows = status
        .reviewers
        .iter()
        .filter(|reviewer| spawn.contains(&reviewer.agent))
        .filter(|reviewer| seen.insert((reviewer.class, reviewer.vendor.clone())))
        .take(CONTEXT_ROWS)
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "## Peer review: required before your final answer\n\
         Before you report substantial work as done, get a review: call the marsh `task` tool \
         with the agent and model of the row below for your own model's class whose vendor is \
         not your model's, `description: \"peer review\"`, and a prompt that says what to \
         review and asks for findings only, changing no files (e.g. \"Review the uncommitted \
         changes in this folder for bugs, missed cases and missing tests\", or the plan or \
         document itself). Substantial means, e.g., a code change beyond a trivial edit (new \
         behavior, a bug fix, a refactor, anything with tests), a plan or design, an analysis \
         someone will act on, a document the user will send. Not for quick answers, typo or \
         one-line fixes, or pure lookups. Then fix what it finds, or tell the user which \
         findings you left and why. No row for your class from another vendor: skip it.\n\
         Classes: small (claude-haiku, gpt-luna, gemini-flash), mid (claude-sonnet, gpt-terra, \
         gemini-pro), large (claude-opus, claude-fable, gpt-sol, gpt-astra).\n\n\
         | class | vendor | task arguments |\n|---|---|---|\n",
    );
    for reviewer in rows {
        let _ = writeln!(
            text,
            "| {} | {} | agent: \"{}\", model: \"{}\" |",
            reviewer.class.name(),
            reviewer.vendor,
            reviewer.agent,
            reviewer.model
        );
    }
    text
}

/// The service a multi-provider agent's Kit declares (`org.marsh/agent-model@1`
/// `services`): it reaches every vendor whose key the host holds.
pub use marsh_contracts::kit_assets::ANY_PROVIDER;

/// The API host of each vendor whose key a multi-provider agent's Kit might
/// carry; a credential service is named for its vendor (`sbx secret ls`).
const VENDOR_HOSTS: [(&str, &str); 6] = [
    ("anthropic", "api.anthropic.com"),
    ("openai", "api.openai.com"),
    ("google", "generativelanguage.googleapis.com"),
    ("mistral", "api.mistral.ai"),
    ("xai", "api.x.ai"),
    ("deepseek", "api.deepseek.com"),
];

/// Why a multi-provider `agent` cannot use `vendor`'s models: its Kit must
/// allow the vendor's API host and inject the vendor's key for it
/// (`reaches`, per credential service).
fn cannot_reach(
    agent: &str,
    vendor: &str,
    reaches: &BTreeMap<String, BTreeSet<String>>,
) -> Option<String> {
    let Some((_, host)) = VENDOR_HOSTS.iter().find(|(name, _)| *name == vendor) else {
        return Some(format!(
            "{agent} can't reach {vendor}: marsh does not know its API host"
        ));
    };
    (!reaches
        .get(vendor)
        .is_some_and(|hosts| hosts.contains(*host)))
    .then(|| {
        format!(
            "{agent} can't reach {host} (its Kit must allow the host and inject the {vendor} key for it)"
        )
    })
}

/// A registered agent as `marsh config peer-review on` sees it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DetectAgent {
    /// Its Kit's credential services, `"*"` for a multi-provider agent.
    pub services: BTreeSet<String>,
    /// Per credential service, the hosts its Kit allows and injects that
    /// service's key for.
    pub reaches: BTreeMap<String, BTreeSet<String>>,
}

/// What detection found.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Detected {
    /// Reviewers marsh can start, by class then vendor.
    pub ready: Vec<DropInReviewer>,
    /// Vendors whose key the host holds, with no agent that reaches them
    /// except a multi-provider one whose Kit cannot (the reason says why).
    /// `marsh peer-review ls` lists them as not ready.
    pub unreachable: Vec<(DropInReviewer, String)>,
}

/// The reviewers `marsh config peer-review on` offers: for each class and
/// each vendor an agent with a model option can reach with a stored
/// credential, the vendor's newest model in that class. A vendor's own
/// agent (one that reaches only it) is preferred. A multi-provider agent
/// (services `["*"]`) is the last resort, takes the provider-qualified id
/// (`openai/gpt-6.1-terra`), and counts for a vendor only where its Kit
/// allows the vendor's API host and injects its key.
#[must_use]
pub fn detect(
    table: &ClassTable,
    agents: &BTreeMap<String, DetectAgent>,
    credentials: &BTreeSet<String>,
) -> Detected {
    let mut chosen = BTreeMap::<(ModelClass, String), (usize, String, bool)>::new();
    let mut blocked = BTreeMap::<(ModelClass, String), (String, String)>::new();
    for (agent, facts) in agents {
        let any = facts.services.contains(ANY_PROVIDER);
        for vendor in credentials
            .iter()
            .filter(|vendor| any || facts.services.contains(*vendor))
        {
            let named = facts.services.contains(vendor);
            let refusal = if any {
                cannot_reach(agent, vendor, &facts.reaches)
            } else {
                None
            };
            let rank = if named {
                facts.services.len()
            } else {
                usize::MAX
            };
            for class in ModelClass::ALL {
                if table.newest(vendor, class).is_none() {
                    continue;
                }
                let key = (class, vendor.clone());
                if let Some(reason) = &refusal {
                    blocked
                        .entry(key)
                        .or_insert_with(|| (agent.clone(), reason.clone()));
                } else if chosen.get(&key).is_none_or(|(best, _, _)| rank < *best) {
                    chosen.insert(key, (rank, agent.clone(), any));
                }
            }
        }
    }
    let reviewer = |class: ModelClass, vendor: &str, agent: String, qualified: bool| {
        let newest = table.newest(vendor, class)?;
        Some(DropInReviewer {
            model: if qualified {
                format!("{vendor}/{newest}")
            } else {
                newest
            },
            agent,
            class: Some(class),
            vendor: Some(vendor.to_owned()),
        })
    };
    Detected {
        unreachable: blocked
            .into_iter()
            .filter(|(key, _)| !chosen.contains_key(key))
            .filter_map(|((class, vendor), (agent, reason))| {
                Some((reviewer(class, &vendor, agent, true)?, reason))
            })
            .collect(),
        ready: chosen
            .into_iter()
            .filter_map(|((class, vendor), (_, agent, qualified))| {
                reviewer(class, &vendor, agent, qualified)
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(table: &ClassTable, model: &str) -> Option<(ModelClass, String)> {
        table
            .classify(model)
            .map(|found| (found.class, found.vendor.unwrap_or_default()))
    }

    #[test]
    fn the_curated_table_pairs_the_vendors_families() {
        let table = ClassTable::builtin();
        let small = ModelClass::Small;
        let mid = ModelClass::Mid;
        let large = ModelClass::Large;
        for (model, expected, vendor) in [
            ("claude-haiku-5-5", small, "anthropic"),
            ("gpt-6-luna", small, "openai"),
            ("claude-sonnet-5-5", mid, "anthropic"),
            ("sonnet", mid, "anthropic"),
            ("gpt-6.1-terra", mid, "openai"),
            ("openai/gpt-6.1-terra", mid, "openai"),
            ("us.anthropic.claude-sonnet-5-5-v1:0", mid, "anthropic"),
            ("claude-fable-5-1", large, "anthropic"),
            ("gpt-6-astra", large, "openai"),
            ("claude-opus-5-5", large, "anthropic"),
            ("gpt-6.1-sol", large, "openai"),
            ("gemini-3.8-flash", small, "google"),
            ("gemini-3.1-pro-preview", mid, "google"),
            ("deepseek-v4-flash", small, "deepseek"),
            ("qwen3.8-max", large, "alibaba"),
            ("glm-5.3", mid, "zai"),
            ("kimi-k2.6", mid, "moonshotai"),
        ] {
            assert_eq!(
                class(&table, model),
                Some((expected, vendor.to_owned())),
                "{model}"
            );
        }
        // A family rule reaches an id the patterns do not name.
        let found = table.classify("gpt-5.6").unwrap();
        assert_eq!(
            (found.class, found.vendor.as_deref()),
            (large, Some("openai"))
        );
        assert!(
            found.how.starts_with("models.dev family gpt-sol"),
            "{}",
            found.how
        );
    }

    #[test]
    fn the_override_comes_first_and_can_move_the_bands() {
        let scope = tempfile::tempdir().unwrap();
        std::fs::write(
            scope.path().join(OVERRIDE_FILE),
            "bands: {small_below: 1, large_from: 3}\n\
             rules:\n  - {match: \"gpt-*sol*\", class: mid, vendor: openai}\n",
        )
        .unwrap();
        let (table, error) = ClassTable::load(scope.path());
        assert_eq!(error, None);
        let found = table.classify("gpt-6.1-sol").unwrap();
        assert_eq!(found.class, ModelClass::Mid);
        assert_eq!(found.how, "$MARSH_HOME/model-classes.yaml: gpt-*sol*");
        // An unnamed model moves with the bands: grok-4.7 lists $6/M output.
        assert_eq!(table.classify("grok-4.7").unwrap().class, ModelClass::Large);
        // A malformed override is reported and marsh's table still applies.
        std::fs::write(scope.path().join(OVERRIDE_FILE), "rules: [{match: x}]").unwrap();
        let (table, error) = ClassTable::load(scope.path());
        assert!(error.unwrap().contains("model-classes.yaml"));
        assert_eq!(
            table.classify("gpt-6.1-sol").unwrap().class,
            ModelClass::Large
        );
    }

    #[test]
    fn unnamed_models_fall_back_to_their_price_band_and_unknown_ones_to_none() {
        let table = ClassTable::builtin();
        // xAI has no rule: $6/M output is mid; the maker is the vendor.
        let found = table.classify("grok-4.7").unwrap();
        assert_eq!(
            (found.class, found.vendor.as_deref()),
            (ModelClass::Mid, Some("xai"))
        );
        assert_eq!(found.how, "models.dev price $6/M output");
        // Meta's Muse Wonder lists $20/M output: large.
        assert_eq!(
            class(&table, "muse-wonder-1.0"),
            Some((ModelClass::Large, "meta".into()))
        );
        assert_eq!(table.bands().class(0.5), ModelClass::Small);
        assert_eq!(table.classify("no-such-model-anywhere"), None);
        assert_eq!(table.classify(""), None);
    }

    #[test]
    fn a_model_has_its_makers_vendor_on_every_host() {
        let table = ClassTable::builtin();
        // Hosts spell the maker differently (`x-ai`, `xai`) or name none.
        for model in [
            "fastrouter/x-ai/grok-4",
            "llmgateway-providers/xai/grok-4",
            "xai/grok-4",
            "grok-4",
        ] {
            assert_eq!(
                class(&table, model),
                Some((ModelClass::Mid, "xai".into())),
                "{model}"
            );
        }
        assert_eq!(
            class(&table, "venice/grok-4-6"),
            class(&table, "xai/grok-4.6")
        );
        // Muse on a host is still Meta's, never the host's.
        for model in [
            "venice/muse-wonder",
            "kilo/meta/muse-wonder",
            "meta/muse-wonder-1.0",
        ] {
            assert_eq!(
                class(&table, model).map(|found| found.1),
                Some("meta".into()),
                "{model}"
            );
        }
        // No maker anywhere: classed by price, but no vendor to pair.
        let found = table.classify("abliteration-ai/abliterated-model").unwrap();
        assert_eq!(found.vendor, None, "{found:?}");
        assert_eq!(vendor_name("~X-AI"), "xai");
        assert_eq!(vendor_name("zhipuai"), "zai");
        assert_eq!(vendor_name("openai"), "openai");
    }

    #[test]
    fn newest_picks_the_vendors_current_model_in_a_class() {
        let table = ClassTable::builtin();
        assert_eq!(
            table.newest("openai", ModelClass::Mid).as_deref(),
            Some("gpt-6.1-terra")
        );
        assert_eq!(
            table.newest("anthropic", ModelClass::Mid).as_deref(),
            Some("claude-sonnet-5-5")
        );
        assert_eq!(
            table.newest("anthropic", ModelClass::Small).as_deref(),
            Some("claude-haiku-5-5")
        );
    }

    fn agents(entries: &[(&str, bool)]) -> BTreeMap<String, AgentFacts> {
        entries
            .iter()
            .map(|(name, model_option)| {
                (
                    (*name).to_owned(),
                    AgentFacts {
                        prompt: *name != "shell",
                        model_option: *model_option,
                        ..AgentFacts::default()
                    },
                )
            })
            .collect()
    }

    #[test]
    fn dropins_resolve_to_ready_reviewers_and_say_why_others_are_not() {
        let scope = tempfile::tempdir().unwrap();
        let directory = scope.path().join(DROPIN_DIR);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join("narwhal.yaml"),
            "enabled: true\nreviewers:\n\
             - {agent: codex, model: gpt-6.1-terra}\n\
             - {agent: claude, model: claude-sonnet-5-5}\n\
             - {agent: pi, model: claude-opus-5-5}\n\
             - {agent: hermes, model: gpt-6-astra}\n\
             - {agent: shell, model: gpt-6-astra}\n\
             - {agent: codex, model: my-house-model}\n\
             - {agent: codex, model: my-house-model, class: large, vendor: openai}\n\
             - {agent: codex, model: abliteration-ai/abliterated-model}\n\
             - {agent: claude, model: x-ai/grok-4, vendor: x-ai}\n\
             - {agent: codex, model: grok-4, vendor: xai}\n",
        )
        .unwrap();
        std::fs::write(directory.join("broken.yaml"), "enabled: maybe\n").unwrap();
        std::fs::write(directory.join(".swap.yaml"), "not: [yaml").unwrap();
        let known = agents(&[
            ("claude", true),
            ("codex", true),
            ("pi", false),
            ("shell", false),
        ]);
        let status = load(scope.path(), &known);
        assert!(status.enabled);
        assert_eq!(status.files, [directory.join("narwhal.yaml")]);
        assert_eq!(status.errors.len(), 1, "{:?}", status.errors);
        assert!(status.errors[0].contains("broken.yaml"));
        let ready = status
            .reviewers
            .iter()
            .map(|r| {
                (
                    r.class.name(),
                    r.vendor.as_str(),
                    r.agent.as_str(),
                    r.model.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ready,
            [
                ("mid", "anthropic", "claude", "claude-sonnet-5-5"),
                ("mid", "openai", "codex", "gpt-6.1-terra"),
                // An explicit vendor alias is canonical too.
                ("mid", "xai", "claude", "x-ai/grok-4"),
                ("mid", "xai", "codex", "grok-4"),
                ("large", "openai", "codex", "my-house-model"),
            ]
        );
        let reasons = status
            .skipped
            .iter()
            .map(|r| r.reason.clone().unwrap())
            .collect::<Vec<_>>();
        assert!(
            reasons[0].contains("pi's Kit declares no model option"),
            "{reasons:?}"
        );
        assert!(reasons[1].contains("hermes is not a registered command"));
        assert!(reasons[2].contains("shell's Kit declares no prompt verbs"));
        assert!(reasons[3].contains("cannot class my-house-model"));
        assert!(
            reasons[4].contains("does not know who makes abliteration-ai/abliterated-model"),
            "{reasons:?}"
        );

        let text = context(&status, &["claude".into(), "codex".into()]);
        assert!(text.starts_with("## Peer review: required before your final answer\n"));
        assert!(text.contains("| mid | openai | agent: \"codex\", model: \"gpt-6.1-terra\" |"));
        // Two reviewers of one class and vendor: only the first is shown.
        assert_eq!(text.matches("| mid | xai |").count(), 1);
        let only_claude = context(&status, &["claude".into()]);
        assert!(!only_claude.contains("codex"));
        assert_eq!(context(&status, &[]), "");

        // `enabled: false` anywhere turns it off.
        std::fs::write(directory.join("marsh.yaml"), "enabled: false\n").unwrap();
        let status = load(scope.path(), &known);
        assert!(!status.enabled);
        assert_eq!(status.disabled_by, [directory.join("marsh.yaml")]);
        assert_eq!(context(&status, &["codex".into()]), "");
    }

    #[test]
    fn dropins_are_validated() {
        for (text, error) in [
            ("version: 2\n", "version"),
            ("colour: blue\n", "unknown field"),
            ("reviewers: [{agent: codex}]\n", "missing field `model`"),
            (
                "reviewers: [{agent: codex, model: '--yolo'}]\n",
                "not starting with `-`",
            ),
            ("reviewers: [{agent: 'Co dex', model: x}]\n", "agent"),
            (
                "reviewers: [{agent: codex, model: x, class: huge}]\n",
                "unknown variant",
            ),
            (
                "reviewers: [{agent: codex, model: x, vendor: OpenAI}]\n",
                "lowercase",
            ),
        ] {
            let found = DropIn::parse(text).unwrap_err();
            assert!(found.contains(error), "{text}: {found}");
        }
        assert_eq!(DropIn::parse("").unwrap(), DropIn::default());
    }

    fn set(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|v| (*v).to_owned()).collect()
    }

    fn agent(services: &[&str], reaches: &[(&str, &str)]) -> DetectAgent {
        let mut found = DetectAgent {
            services: set(services),
            reaches: BTreeMap::new(),
        };
        for (service, host) in reaches {
            found
                .reaches
                .entry((*service).to_owned())
                .or_default()
                .insert((*host).to_owned());
        }
        found
    }

    #[test]
    fn detect_prefers_each_vendors_own_agent() {
        let table = ClassTable::builtin();
        let agents = BTreeMap::from([
            ("claude".to_owned(), agent(&["anthropic"], &[])),
            ("codex".to_owned(), agent(&["openai"], &[])),
            (
                "pix".to_owned(),
                agent(&["anthropic", "openai", "google"], &[]),
            ),
        ]);
        let found = detect(&table, &agents, &set(&["anthropic", "openai"]));
        let mid = found
            .ready
            .iter()
            .filter(|r| r.class == Some(ModelClass::Mid))
            .map(|r| (r.agent.as_str(), r.model.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            mid,
            [("claude", "claude-sonnet-5-5"), ("codex", "gpt-6.1-terra")]
        );
        assert!(!found.ready.iter().any(|r| r.agent == "pix"));
        let rendered = DropIn {
            version: Some(1),
            enabled: Some(true),
            reviewers: found.ready,
        }
        .render("written by marsh");
        assert!(DropIn::parse(&rendered).is_ok(), "{rendered}");
    }

    #[test]
    fn a_multi_provider_agent_counts_only_for_vendors_its_kit_reaches() {
        let table = ClassTable::builtin();
        let pi = agent(
            &[ANY_PROVIDER, "anthropic"],
            &[
                ("anthropic", "api.anthropic.com"),
                ("openai", "api.openai.com"),
                // A key injected for a host the Kit does not allow never gets here.
            ],
        );
        let agents = BTreeMap::from([
            ("claude".to_owned(), agent(&["anthropic"], &[])),
            ("pi".to_owned(), pi),
        ]);
        let found = detect(&table, &agents, &set(&["anthropic", "openai", "deepseek"]));
        for class in ModelClass::ALL {
            let by = |vendor: &str| {
                found
                    .ready
                    .iter()
                    .find(|r| r.class == Some(class) && r.vendor.as_deref() == Some(vendor))
                    .map(|r| (r.agent.as_str(), r.model.clone()))
            };
            assert_eq!(by("anthropic").map(|r| r.0), Some("claude"), "{class:?}");
            // openai has no agent of its own here: pi, with the qualified id.
            assert_eq!(
                by("openai"),
                table
                    .newest("openai", class)
                    .map(|model| ("pi", format!("openai/{model}"))),
                "{class:?}"
            );
            // deepseek: pi's Kit has neither its host nor its key.
            assert_eq!(by("deepseek"), None, "{class:?}");
        }
        let blocked = found
            .unreachable
            .iter()
            .map(|(r, reason)| (r.agent.as_str(), r.vendor.as_deref(), reason.as_str()))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            blocked.into_iter().collect::<Vec<_>>(),
            [(
                "pi",
                Some("deepseek"),
                "pi can't reach api.deepseek.com (its Kit must allow the host and inject the deepseek key for it)"
            )]
        );
        assert!(
            found
                .unreachable
                .iter()
                .all(|(r, _)| r.model.starts_with("deepseek/deepseek-"))
        );
        // The host alone is not enough: no injected key, no reach.
        let no_key = BTreeMap::from([("pi".to_owned(), agent(&[ANY_PROVIDER], &[]))]);
        let found = detect(&table, &no_key, &set(&["openai"]));
        assert_eq!(found.ready.len(), 0);
        assert_eq!(found.unreachable.len(), 3);
        // Without a stored key nothing is offered or blocked.
        let found = detect(&table, &agents, &BTreeSet::new());
        assert_eq!((found.ready.len(), found.unreachable.len()), (0, 0));
    }

    #[test]
    fn load_skips_a_multi_provider_agent_for_a_vendor_its_kit_cannot_reach() {
        let scope = tempfile::tempdir().unwrap();
        let directory = scope.path().join(DROPIN_DIR);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("marsh.yaml"),
            "enabled: true\nreviewers:\n  - {agent: pi, model: openai/gpt-6.1-terra}\n  - {agent: pi, model: deepseek/deepseek-v4-flash}\n  - {agent: pi, model: claude-sonnet-5-5}\n",
        )
        .unwrap();
        let facts = AgentFacts {
            prompt: true,
            model_option: true,
            multi_provider: true,
            reaches: BTreeMap::from([(
                "openai".to_owned(),
                BTreeSet::from(["api.openai.com".to_owned()]),
            )]),
        };
        let status = load(scope.path(), &BTreeMap::from([("pi".to_owned(), facts)]));
        assert_eq!(
            status
                .reviewers
                .iter()
                .map(|r| r.model.as_str())
                .collect::<Vec<_>>(),
            ["openai/gpt-6.1-terra"]
        );
        let mut reasons = status
            .skipped
            .iter()
            .map(|r| r.reason.clone().unwrap())
            .collect::<Vec<_>>();
        reasons.sort();
        assert_eq!(
            reasons,
            [
                "pi can't reach api.anthropic.com (its Kit must allow the host and inject the anthropic key for it)",
                "pi can't reach api.deepseek.com (its Kit must allow the host and inject the deepseek key for it)"
            ]
        );
    }

    #[test]
    fn an_agents_models_are_each_familys_newest_and_nicknames_resolve() {
        let table = ClassTable::builtin();
        let none = BTreeMap::new();
        let describe = |agent: &str, services: &[&str], reaches: &BTreeMap<String, Vec<String>>| {
            let services = services.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
            describe_choices(&BTreeMap::from([(
                agent.to_owned(),
                agent_choices(&table, agent, &services, reaches),
            )]))
        };
        assert_eq!(
            describe("codex", &["openai"], &none),
            "codex: gpt-6-luna (small, luna), gpt-6.1-terra (mid, terra), \
             gpt-6.1-sol (large, sol), gpt-6-astra (large, astra)"
        );
        assert_eq!(
            describe("claude", &["anthropic"], &none),
            "claude: claude-haiku-5-5 (small, haiku), claude-sonnet-5-5 (mid, sonnet), \
             claude-opus-5-5 (large, opus), claude-fable-5-1 (large, fable)"
        );
        // A multi-provider agent lists only the vendors its Kit reaches, with
        // qualified ids, within the row cap.
        let reaches = BTreeMap::from([("openai".to_owned(), vec!["api.openai.com".to_owned()])]);
        let pi = agent_choices(&table, "pi", &[ANY_PROVIDER.to_owned()], &reaches);
        assert!(!pi.is_empty() && pi.len() <= CHOICE_ROWS);
        assert!(
            pi.iter()
                .all(|choice| choice.model.starts_with("openai/gpt-")),
            "{pi:?}"
        );
        let unreached = agent_choices(&table, "pi", &[ANY_PROVIDER.to_owned()], &none);
        assert_eq!(unreached.len(), 0);

        let codex = agent_choices(&table, "codex", &["openai".to_owned()], &none);
        let astra = resolve_family(&codex, " Astra ").unwrap().unwrap();
        assert_eq!(
            (astra.model.as_str(), astra.class.as_str()),
            ("gpt-6-astra", "large")
        );
        assert_eq!(resolve_family(&codex, "gpt-6-astra"), Ok(None));
        assert_eq!(resolve_family(&codex, "mystery"), Ok(None));
        let sol = ModelChoice {
            model: "x/sol".into(),
            class: "large".into(),
            family: "sol".into(),
        };
        let error = resolve_family(&[codex, vec![sol]].concat(), "sol").unwrap_err();
        assert!(error.contains("gpt-6.1-sol, x/sol"), "{error}");
        assert_eq!(family_nickname("gpt-*luna*"), "luna");
        assert_eq!(family_nickname("claude-*-haiku*"), "haiku");
        assert_eq!(family_nickname("gpt-oss-20b*"), "oss");
        assert_eq!(family_nickname("mistral-large*"), "mistral-large");
        assert_eq!(family_nickname("haiku"), "haiku");
    }

    #[test]
    fn globs() {
        assert!(glob("claude-sonnet-*", "claude-sonnet-5-5"));
        assert!(glob("gpt-*sol*", "gpt-sol"));
        assert!(glob("gemini-*flash*", "gemini-3.8-flash"));
        assert!(!glob("gemini-*flash*", "gemini-3.1-pro"));
        assert!(glob("sonnet", "sonnet"));
        assert!(!glob("sonnet", "sonnet-4"));
        assert!(!glob("a*a", "a"));
    }
}
