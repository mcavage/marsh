//! Token usage of agent jobs (`docs/agents.md`, "What a task cost").
//!
//! An agent Kit opts in with marsh's `org.marsh/agent-usage@1` capability:
//! `config.args` are words appended to its non-interactive prompt verbs that
//! make the agent report usage on stdout (`claude -p P --output-format json`,
//! `codex exec P --json`, `pi -p P --mode json`), and `config.parser` names
//! how marsh reads that output. The task tool asks for it; the daemon reads
//! the job's stdout with the parser and records [`Usage`] on the job, and the
//! task tool returns the agent's final message from the same output. An agent
//! that reports nothing gets no usage: it is never estimated.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// marsh's own capability: how an agent reports its token usage.
pub const AGENT_USAGE_CAPABILITY: &str = "org.marsh/agent-usage@1";

/// Claude Code's `-p --output-format json` (or `stream-json`): the `result`
/// object's `modelUsage` (else `usage`), `total_cost_usd` and `result`.
pub const CLAUDE_JSON: &str = "claude-json";
/// Codex's `exec --json` events: every `turn.completed` `usage`, and the last
/// `agent_message` item.
pub const CODEX_JSONL: &str = "codex-jsonl";
/// Pi's `--mode json` events: every assistant `message_end` `usage` (with
/// its cost), and the last assistant message's text.
pub const PI_JSONL: &str = "pi-jsonl";
/// The parsers marsh knows.
pub const PARSERS: [&str; 3] = [CLAUDE_JSON, CODEX_JSONL, PI_JSONL];

/// A line longer than this is not read for usage (pi's streaming updates
/// repeat the whole message; they are skipped without being buffered).
const MAX_LINE: usize = 8 * 1024 * 1024;
/// Bytes of a line read before deciding whether its `type` matters.
const TYPE_PREFIX: usize = 64;

/// One job's own token usage, as its agent reported it.
///
/// `input_tokens` counts input read at the full price: cache reads are
/// `cached_tokens` and cache writes `cache_write_tokens`, never also in
/// `input_tokens` (Codex reports both inside its input count; marsh
/// subtracts them). `cost_usd` is the agent's own figure, when it reports
/// one. `source` is the parser that read it.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub source: String,
}

// Costs are always finite (`finite_cost`), so equality is total.
impl Eq for Usage {}

/// A job tree's usage: the job's own plus every descendant's that reported
/// one. `jobs` counts the jobs that did; `cost_usd` sums the costs reported
/// and `uncosted_jobs` counts jobs whose agent reported tokens but no cost.
/// `models` breaks the totals down by model; its entries sum to them.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TreeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub jobs: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub uncosted_jobs: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelUsage>,
}

impl Eq for TreeUsage {}

/// One model's share of a tree's usage: the jobs that reported it and what
/// they used. `cost_usd` sums the costs those jobs reported.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cache_write_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    pub jobs: u64,
}

impl Eq for ModelUsage {}

/// The `models` key of a job whose agent reported no model.
pub const UNKNOWN_MODEL: &str = "unknown";
/// The `models` key past [`MAX_MODELS`] distinct models, so the entries
/// still sum to the totals.
pub const OTHER_MODELS: &str = "other";
const MAX_MODELS: usize = 32;

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn add_cost(total: &mut Option<f64>, cost: Option<f64>) {
    if let Some(cost) = cost {
        *total = Some(round_cost(total.unwrap_or(0.0) + cost));
    }
}

impl ModelUsage {
    fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(other.cached_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.jobs = self.jobs.saturating_add(other.jobs);
        add_cost(&mut self.cost_usd, other.cost_usd);
    }
}

impl TreeUsage {
    /// Add one job's own usage.
    pub fn add_usage(&mut self, usage: &Usage) {
        if usage.cost_usd.is_none() {
            self.uncosted_jobs = self.uncosted_jobs.saturating_add(1);
        }
        let model = usage.model.as_deref().unwrap_or(UNKNOWN_MODEL);
        self.add_model(
            model,
            &ModelUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_tokens: usage.cached_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                cost_usd: usage.cost_usd,
                jobs: 1,
            },
        );
    }

    /// Add a child's tree.
    pub fn add_tree(&mut self, tree: &Self) {
        self.uncosted_jobs = self.uncosted_jobs.saturating_add(tree.uncosted_jobs);
        for (model, share) in &tree.models {
            self.add_model(model, share);
        }
    }

    /// Add one model's share to the totals and to its entry.
    fn add_model(&mut self, model: &str, share: &ModelUsage) {
        self.input_tokens = self.input_tokens.saturating_add(share.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(share.output_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(share.cached_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(share.cache_write_tokens);
        self.jobs = self.jobs.saturating_add(share.jobs);
        add_cost(&mut self.cost_usd, share.cost_usd);
        let key = if self.models.contains_key(model) || self.models.len() < MAX_MODELS {
            model
        } else {
            OTHER_MODELS
        };
        self.models.entry(key.to_owned()).or_default().add(share);
    }
}

/// Costs to a millionth of a dollar, so sums stay readable.
fn round_cost(cost: f64) -> f64 {
    (cost * 1_000_000.0).round() / 1_000_000.0
}

fn finite_cost(value: Option<&serde_json::Value>) -> Option<f64> {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
        .map(round_cost)
}

/// A Kit's usage option: the words that make its agent report usage, and
/// the parser for that output.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageOption {
    pub args: Vec<String>,
    pub parser: String,
}

impl UsageOption {
    /// `org.marsh/agent-usage@1` `config`: 1-8 non-empty `args` (no
    /// placeholders) and a known `parser`.
    ///
    /// # Errors
    /// Returns why the declaration is malformed.
    pub fn from_config(config: &serde_json::Value) -> Result<Self, String> {
        let args = config
            .get("args")
            .and_then(serde_json::Value::as_array)
            .filter(|args| (1..=8).contains(&args.len()))
            .and_then(|args| {
                args.iter()
                    .map(|arg| {
                        arg.as_str()
                            .filter(|arg| {
                                !arg.is_empty()
                                    && arg.len() <= 256
                                    && !arg.contains("{{")
                                    && !arg.contains('\0')
                            })
                            .map(str::to_owned)
                    })
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| {
                format!("{AGENT_USAGE_CAPABILITY}: args must be 1-8 non-empty strings")
            })?;
        let parser = config
            .get("parser")
            .and_then(serde_json::Value::as_str)
            .filter(|parser| PARSERS.contains(parser))
            .ok_or_else(|| {
                format!(
                    "{AGENT_USAGE_CAPABILITY}: parser must be one of {}",
                    PARSERS.join(", ")
                )
            })?;
        Ok(Self {
            args,
            parser: parser.to_owned(),
        })
    }
}

/// What an agent's output said: its usage and its final message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Captured {
    pub usage: Option<Usage>,
    /// The agent's final message, as its plain output would have printed it.
    pub message: Option<String>,
}

/// Reads an agent's stdout as it streams, one JSON line at a time, holding at
/// most one line (lines whose `type` does not matter are skipped unread).
#[derive(Debug)]
pub struct Collector {
    parser: &'static str,
    line: Vec<u8>,
    skipping: bool,
    usage: Option<Usage>,
    message: Option<String>,
}

impl Collector {
    /// A collector for `parser`, or `None` for an unknown one.
    #[must_use]
    pub fn new(parser: &str) -> Option<Self> {
        let parser = PARSERS.into_iter().find(|known| *known == parser)?;
        Some(Self {
            parser,
            line: Vec::new(),
            skipping: false,
            usage: None,
            message: None,
        })
    }

    pub fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let (chunk, rest, ended) = match bytes.iter().position(|byte| *byte == b'\n') {
                Some(end) => (&bytes[..end], &bytes[end + 1..], true),
                None => (bytes, &[][..], false),
            };
            bytes = rest;
            if !self.skipping {
                self.line.extend_from_slice(chunk);
                if self.line.len() > MAX_LINE || self.uninteresting() {
                    self.line.clear();
                    self.skipping = true;
                }
            }
            if ended {
                if !self.skipping {
                    let line = std::mem::take(&mut self.line);
                    self.line_done(&line);
                }
                self.skipping = false;
            }
        }
    }

    /// The usage and message read; a last line without a newline counts.
    #[must_use]
    pub fn finish(mut self) -> Captured {
        if !self.skipping && !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.line_done(&line);
        }
        Captured {
            usage: self.usage,
            message: self.message,
        }
    }

    /// Whether the line so far names a `type` this parser ignores. Agents
    /// print `type` first (`{"type":"message_update",...`).
    fn uninteresting(&self) -> bool {
        const START: &[u8] = b"{\"type\":\"";
        if self.line.len() < TYPE_PREFIX || !self.line.starts_with(START) {
            return false;
        }
        let rest = &self.line[START.len()..];
        let Some(end) = rest.iter().position(|byte| *byte == b'"') else {
            return false;
        };
        let wanted: &[&[u8]] = match self.parser {
            CLAUDE_JSON => &[b"result"],
            CODEX_JSONL => &[b"turn.completed", b"item.completed"],
            _ => &[b"message_end"],
        };
        !wanted.contains(&&rest[..end])
    }

    fn line_done(&mut self, line: &[u8]) {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            return;
        };
        match self.parser {
            CLAUDE_JSON => self.claude(&value),
            CODEX_JSONL => self.codex(&value),
            _ => self.pi(&value),
        }
    }

    fn usage(&mut self) -> &mut Usage {
        let source = self.parser;
        self.usage.get_or_insert_with(|| Usage {
            source: source.to_owned(),
            ..Usage::default()
        })
    }

    fn claude(&mut self, value: &serde_json::Value) {
        if value["type"] != "result" {
            return;
        }
        if let Some(result) = value["result"].as_str() {
            self.message = Some(result.to_owned());
        }
        let count = |value: &serde_json::Value, key: &str| value[key].as_u64().unwrap_or(0);
        let mut usage = Usage {
            source: CLAUDE_JSON.to_owned(),
            cost_usd: finite_cost(value.get("total_cost_usd")),
            ..Usage::default()
        };
        // `modelUsage` covers every model the run used (its own helper
        // calls too); `usage` only the main model's turns.
        if let Some(models) = value["modelUsage"]
            .as_object()
            .filter(|models| !models.is_empty())
        {
            let mut main = None::<(&str, u64)>;
            for (model, counts) in models {
                usage.input_tokens += count(counts, "inputTokens");
                usage.output_tokens += count(counts, "outputTokens");
                usage.cached_tokens += count(counts, "cacheReadInputTokens");
                usage.cache_write_tokens += count(counts, "cacheCreationInputTokens");
                let output = count(counts, "outputTokens");
                if main.is_none_or(|(_, most)| output > most) {
                    main = Some((model, output));
                }
            }
            usage.model = main.map(|(model, _)| model.to_owned());
        } else {
            let Some(counts) = value.get("usage").filter(|usage| usage.is_object()) else {
                return;
            };
            usage.input_tokens = count(counts, "input_tokens");
            usage.output_tokens = count(counts, "output_tokens");
            usage.cached_tokens = count(counts, "cache_read_input_tokens");
            usage.cache_write_tokens = count(counts, "cache_creation_input_tokens");
        }
        self.usage = Some(usage);
    }

    fn codex(&mut self, value: &serde_json::Value) {
        match value["type"].as_str() {
            Some("item.completed") if value["item"]["type"] == "agent_message" => {
                if let Some(text) = value["item"]["text"].as_str() {
                    self.message = Some(text.to_owned());
                }
            }
            Some("turn.completed") => {
                let counts = &value["usage"];
                if !counts.is_object() {
                    return;
                }
                let count = |key: &str| counts[key].as_u64().unwrap_or(0);
                // Codex's `input_tokens` includes cache reads and writes.
                let cached = count("cached_input_tokens");
                let write = count("cache_write_input_tokens");
                let input = count("input_tokens")
                    .saturating_sub(cached)
                    .saturating_sub(write);
                let output = count("output_tokens");
                let usage = self.usage();
                usage.input_tokens += input;
                usage.cached_tokens += cached;
                usage.cache_write_tokens += write;
                usage.output_tokens += output;
            }
            _ => {}
        }
    }

    fn pi(&mut self, value: &serde_json::Value) {
        let message = &value["message"];
        if value["type"] != "message_end" || message["role"] != "assistant" {
            return;
        }
        let text = message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            self.message = Some(text);
        } else if let Some(error) = message["errorMessage"].as_str() {
            self.message = Some(error.to_owned());
        }
        let counts = &message["usage"];
        if !counts.is_object() {
            return;
        }
        let count = |key: &str| counts[key].as_u64().unwrap_or(0);
        let (input, output) = (count("input"), count("output"));
        let (cached, write) = (count("cacheRead"), count("cacheWrite"));
        let cost = finite_cost(counts["cost"].get("total"));
        let model = message["model"]
            .as_str()
            .filter(|model| !model.is_empty() && model.len() <= 128)
            .map(str::to_owned);
        let usage = self.usage();
        usage.input_tokens += input;
        usage.output_tokens += output;
        usage.cached_tokens += cached;
        usage.cache_write_tokens += write;
        if let Some(cost) = cost {
            usage.cost_usd = Some(round_cost(usage.cost_usd.unwrap_or(0.0) + cost));
        }
        if model.is_some() {
            usage.model = model;
        }
    }
}

/// Read a whole output with `parser`.
#[must_use]
pub fn capture(parser: &str, output: &[u8]) -> Captured {
    Collector::new(parser).map_or_else(Captured::default, |mut collector| {
        collector.feed(output);
        collector.finish()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real outputs, captured from the agents marsh's Kits pin (Claude Code
    // 2.1.318, Codex 0.166.0, Pi 1.3.0) run in marsh jobs, and Codex 0.164.0
    // on a Mac; only the account's home path is replaced.
    const CLAUDE: &str = include_str!("usage_fixtures/claude-2.1.318.json");
    const CODEX: &str = include_str!("usage_fixtures/codex-0.166.0.jsonl");
    const CODEX_MAC: &str = include_str!("usage_fixtures/codex-0.164.0.jsonl");
    const PI: &str = include_str!("usage_fixtures/pi-1.3.0.jsonl");

    /// The same output fed whole and one byte at a time reads the same.
    fn read(parser: &str, output: &str) -> Captured {
        let whole = capture(parser, output.as_bytes());
        let mut collector = Collector::new(parser).unwrap();
        for byte in output.as_bytes() {
            collector.feed(std::slice::from_ref(byte));
        }
        assert_eq!(collector.finish(), whole);
        whole
    }

    #[test]
    fn claude_json_reads_model_usage_cost_and_the_result() {
        let captured = read(CLAUDE_JSON, CLAUDE);
        assert_eq!(captured.message.as_deref(), Some("pong"));
        assert_eq!(
            captured.usage,
            Some(Usage {
                model: Some("claude-opus-5-5".into()),
                input_tokens: 2,
                output_tokens: 4,
                cached_tokens: 0,
                cache_write_tokens: 14502,
                cost_usd: Some(0.072_598),
                source: CLAUDE_JSON.into(),
            })
        );
        // Without `modelUsage`, the top-level `usage` counts.
        let mut value: serde_json::Value = serde_json::from_str(CLAUDE).unwrap();
        value.as_object_mut().unwrap().remove("modelUsage");
        let usage = capture(CLAUDE_JSON, value.to_string().as_bytes())
            .usage
            .unwrap();
        assert_eq!((usage.model, usage.cache_write_tokens), (None, 14502));
    }

    #[test]
    fn codex_jsonl_reads_turn_usage_without_cached_input_and_the_last_message() {
        let captured = read(CODEX_JSONL, CODEX);
        assert_eq!(captured.message.as_deref(), Some("pong"));
        assert_eq!(
            captured.usage,
            Some(Usage {
                model: None,
                input_tokens: 3,
                output_tokens: 5,
                cached_tokens: 0,
                cache_write_tokens: 12236,
                cost_usd: None,
                source: CODEX_JSONL.into(),
            })
        );
        let usage = read(CODEX_JSONL, CODEX_MAC).usage.unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (15759, 5));
        // Two turns add up; cache reads leave the input count.
        let turn = r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":7}}"#;
        let usage = capture(CODEX_JSONL, format!("{turn}\n{turn}\n").as_bytes())
            .usage
            .unwrap();
        assert_eq!(
            (usage.input_tokens, usage.cached_tokens, usage.output_tokens),
            (80, 120, 14)
        );
    }

    #[test]
    fn pi_jsonl_reads_assistant_usage_cost_model_and_text() {
        let captured = read(PI_JSONL, PI);
        assert_eq!(captured.message.as_deref(), Some("pong"));
        assert_eq!(
            captured.usage,
            Some(Usage {
                model: Some("claude-opus-4-8".into()),
                input_tokens: 2,
                output_tokens: 4,
                cached_tokens: 0,
                cache_write_tokens: 4798,
                cost_usd: Some(0.030_098),
                source: PI_JSONL.into(),
            })
        );
    }

    #[test]
    fn output_without_a_report_has_no_usage() {
        // Killed before Claude printed its result; plain text; nothing.
        let truncated = &CLAUDE[..CLAUDE.len() / 2];
        assert_eq!(
            capture(CLAUDE_JSON, truncated.as_bytes()),
            Captured::default()
        );
        assert_eq!(capture(CODEX_JSONL, b"pong\n"), Captured::default());
        assert_eq!(capture(PI_JSONL, b""), Captured::default());
        assert!(Collector::new("guess").is_none());
    }

    #[test]
    fn streaming_lines_of_other_types_are_skipped_unbuffered() {
        let mut collector = Collector::new(PI_JSONL).unwrap();
        collector.feed(br#"{"type":"message_update","message":{"content":""#);
        let filler = vec![b'x'; 64 * 1024];
        for _ in 0..(MAX_LINE / filler.len() + 2) {
            collector.feed(&filler);
            assert_eq!(collector.line.len(), 0);
        }
        collector.feed(b"\"}}\n");
        let end = PI
            .lines()
            .find(|line| line.contains("\"stopReason\":\"stop\""))
            .unwrap();
        collector.feed(end.as_bytes());
        assert_eq!(collector.finish().usage.unwrap().output_tokens, 4);
    }

    #[test]
    fn tree_models_break_the_totals_down_and_name_unknown_models() {
        let claude = read(CLAUDE_JSON, CLAUDE).usage.unwrap();
        let codex = read(CODEX_JSONL, CODEX).usage.unwrap();
        let mut child = TreeUsage::default();
        child.add_usage(&codex);
        let mut tree = TreeUsage::default();
        tree.add_usage(&claude);
        tree.add_tree(&child);
        tree.add_usage(&claude);
        let opus = &tree.models["claude-opus-5-5"];
        let unknown = &tree.models[UNKNOWN_MODEL];
        assert_eq!(tree.models.len(), 2);
        assert_eq!(
            (opus.jobs, opus.output_tokens, opus.cost_usd),
            (2, 8, Some(0.145_196))
        );
        assert_eq!(
            (unknown.jobs, unknown.cache_write_tokens, unknown.cost_usd),
            (1, 12236, None)
        );
        assert_eq!(
            (
                tree.jobs,
                tree.uncosted_jobs,
                tree.output_tokens,
                tree.cost_usd
            ),
            (3, 1, 13, Some(0.145_196))
        );
        assert_eq!(
            tree.cache_write_tokens,
            opus.cache_write_tokens + unknown.cache_write_tokens
        );
        // Past the cap, models share one entry and the sums still hold.
        let mut wide = TreeUsage::default();
        for index in 0..=MAX_MODELS {
            wide.add_usage(&Usage {
                model: Some(format!("m{index:02}")),
                output_tokens: 1,
                ..Usage::default()
            });
        }
        assert_eq!(wide.models.len(), MAX_MODELS + 1);
        assert_eq!(wide.models[OTHER_MODELS].jobs, 1);
        assert_eq!(
            wide.models.values().map(|m| m.output_tokens).sum::<u64>(),
            wide.output_tokens
        );
    }

    #[test]
    fn the_capability_takes_words_and_a_known_parser() {
        let option = |config: serde_json::Value| UsageOption::from_config(&config);
        assert_eq!(
            option(serde_json::json!({"args": ["--json"], "parser": "codex-jsonl"})).unwrap(),
            UsageOption {
                args: vec!["--json".into()],
                parser: CODEX_JSONL.into(),
            }
        );
        for bad in [
            serde_json::json!({"args": [], "parser": "codex-jsonl"}),
            serde_json::json!({"args": [""], "parser": "codex-jsonl"}),
            serde_json::json!({"args": ["{{.Prompt}}"], "parser": "codex-jsonl"}),
            serde_json::json!({"args": ["--json"], "parser": "guess"}),
            serde_json::json!({"args": ["--json"]}),
        ] {
            assert!(option(bad.clone()).is_err(), "{bad}");
        }
    }
}
