//! Trajectory checking: did the agent take a *correct path*?
//!
//! [`SessionRecorder`](crate::SessionRecorder) records what a run did and
//! `harness trace` shows it, but neither says whether the run was right. A
//! final answer can look fine while the agent skipped reading the file it
//! edited, ran `git add -A`, or retried the same failing command ten times.
//! This module judges the path itself, offline, from the JSONL log:
//!
//! - [`Trajectory`] flattens the raw event stream into model turns and tool
//!   calls (a `PreTool` joined to its `PostTool`), which is the shape rules
//!   actually want to ask questions of.
//! - [`Rule`] / [`TrajectoryCheck`] / [`RuleSet`] state what a correct path
//!   looks like: required tools, ordering, forbidden calls, argument shapes,
//!   budgets, loops, and a clean finish.
//! - [`Trajectory::compare_to_golden`] measures structural drift from a known
//!   good run. Only tool names are compared, never model text, because the
//!   text changes on every run while the shape of a good run does not.
//!
//! Everything here is deterministic and needs no model, so it can gate CI.

use crate::replay::{SessionEvent, read_session};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;

/// One step of a run, in the order it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// The model answered once.
    Model {
        /// Whether the turn carried non-empty text. A turn with only tool
        /// calls has none; the final turn of a finished run must.
        has_text: bool,
        /// How many tool calls this turn asked for.
        tool_calls: usize,
        input_tokens: u32,
        output_tokens: u32,
    },
    /// A tool was called.
    Tool {
        id: String,
        name: String,
        args: Value,
        ok: bool,
        /// Why it failed. For a call that never got a result this is
        /// `"no result"`, so a crashed run still reads as a failed call.
        error: Option<String>,
    },
}

/// A run flattened into ordered [`Step`]s plus totals.
#[derive(Debug, Clone, Default)]
pub struct Trajectory {
    pub steps: Vec<Step>,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// An `End` event was seen. A log without one was cut off.
    pub ended: bool,
}

impl Trajectory {
    /// Build the normalized view from recorded events.
    ///
    /// A `PreTool` starts a tool step already marked failed ("no result"); the
    /// matching `PostTool` (same `call_id`) overwrites that. Doing it in this
    /// order means a run that died mid-call is visible as a failure instead of
    /// the call silently vanishing.
    pub fn from_events(events: &[SessionEvent]) -> Self {
        let mut t = Trajectory::default();
        let mut pending: HashMap<String, usize> = HashMap::new();
        for e in events {
            match e {
                SessionEvent::PostModel { output, .. } => {
                    t.model_calls += 1;
                    t.input_tokens += output.usage.input_tokens;
                    t.output_tokens += output.usage.output_tokens;
                    t.steps.push(Step::Model {
                        has_text: output.text.as_deref().is_some_and(|s| !s.trim().is_empty()),
                        tool_calls: output.tool_calls.len(),
                        input_tokens: output.usage.input_tokens,
                        output_tokens: output.usage.output_tokens,
                    });
                }
                SessionEvent::PreTool { action, .. } => {
                    t.tool_calls += 1;
                    pending.insert(action.call_id.clone(), t.steps.len());
                    t.steps.push(Step::Tool {
                        id: action.call_id.clone(),
                        name: action.tool.clone(),
                        args: action.args.clone(),
                        ok: false,
                        error: Some("no result".into()),
                    });
                }
                SessionEvent::PostTool {
                    call_id, result, ..
                } => {
                    if let Some(&i) = pending.get(call_id)
                        && let Step::Tool { ok, error, .. } = &mut t.steps[i]
                    {
                        *ok = result.ok;
                        *error = if result.ok {
                            None
                        } else {
                            Some(failure_reason(result))
                        };
                    }
                }
                SessionEvent::End { .. } => t.ended = true,
                _ => {}
            }
        }
        t
    }

    /// Read a JSONL session log and normalize it.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        Ok(Self::from_events(&read_session(path)?))
    }

    /// Tool names in call order: the structure of the run.
    pub fn tool_sequence(&self) -> Vec<String> {
        self.tool_steps().map(|(_, s)| s.name.to_string()).collect()
    }

    /// Compare this run's tool sequence with a known good one.
    pub fn compare_to_golden(&self, golden: &Trajectory) -> GoldenDiff {
        let a = self.tool_sequence();
        let g = golden.tool_sequence();
        let distance = edit_distance(&a, &g);
        let longest = a.len().max(g.len());
        GoldenDiff {
            distance,
            golden_len: g.len(),
            actual_len: a.len(),
            ratio: if longest == 0 {
                0.0
            } else {
                distance as f64 / longest as f64
            },
        }
    }

    fn tool_steps(&self) -> impl Iterator<Item = (usize, ToolStep<'_>)> {
        self.steps.iter().enumerate().filter_map(|(i, s)| match s {
            Step::Tool {
                name,
                args,
                ok,
                error,
                ..
            } => Some((
                i,
                ToolStep {
                    name,
                    args,
                    ok: *ok,
                    error: error.as_deref(),
                },
            )),
            _ => None,
        })
    }
}

/// Borrowed view of a [`Step::Tool`], so rules do not re-match the enum.
struct ToolStep<'a> {
    name: &'a str,
    args: &'a Value,
    ok: bool,
    error: Option<&'a str>,
}

/// Best human-readable reason a tool failed: a structured `error`/`message`
/// field first, then a bare string payload, then the trace, then a stub.
fn failure_reason(r: &harness_core::ToolResult) -> String {
    for key in ["error", "message"] {
        if let Some(s) = r.content.get(key).and_then(Value::as_str) {
            return s.to_string();
        }
    }
    if let Some(s) = r.content.as_str().filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    r.trace
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tool failed".into())
}

/// Levenshtein distance over whole tool names (one-row dynamic programming).
fn edit_distance(a: &[String], b: &[String]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if x == y {
                prev
            } else {
                1 + prev.min(row[j]).min(cur)
            };
            prev = cur;
        }
    }
    row[b.len()]
}

/// How far a run's tool sequence is from a golden one.
#[derive(Debug, Clone, PartialEq)]
pub struct GoldenDiff {
    /// Insertions, deletions and substitutions needed to turn one sequence
    /// into the other.
    pub distance: usize,
    pub golden_len: usize,
    pub actual_len: usize,
    /// `distance` over the longer sequence: 0.0 identical, 1.0 nothing in
    /// common. Normalizing makes one threshold work for short and long runs.
    pub ratio: f64,
}

impl GoldenDiff {
    /// Drift is acceptable when the ratio does not exceed `max_ratio`.
    pub fn within(&self, max_ratio: f64) -> bool {
        self.ratio <= max_ratio
    }
}

impl fmt::Display for GoldenDiff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tool sequence drift: edit distance {} (golden {} calls, actual {} calls, ratio {:.2})",
            self.distance, self.golden_len, self.actual_len, self.ratio
        )
    }
}

/// One statement about what a correct path looks like.
///
/// Deserializes from TOML with a `rule = "<name>"` discriminator, so rule files
/// can live next to the code and be reviewed like any other config.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "rule", rename_all = "snake_case")]
pub enum Rule {
    /// The tool was called successfully at least once.
    Requires { tool: String },
    /// The first successful `before` precedes the first `after`. Passes if
    /// `after` is never called; fails if `after` runs with no earlier `before`.
    Order { before: String, after: String },
    /// The tool is never called. With `args_match`, only calls whose
    /// JSON-serialized args match the regex are violations.
    Forbids {
        tool: String,
        #[serde(default)]
        args_match: Option<String>,
    },
    /// Every call to `tool` has a string at `field` (a `/json/pointer`, or a
    /// top-level key) matching `pattern`.
    Args {
        tool: String,
        field: String,
        pattern: String,
    },
    /// Upper bounds on model calls, tool calls, and total tokens.
    MaxSteps {
        #[serde(default)]
        model_calls: Option<u32>,
        #[serde(default)]
        tool_calls: Option<u32>,
        #[serde(default)]
        tokens: Option<u32>,
    },
    /// The same call (tool and args) failed `times` or more times: a loop.
    NoRepeatedFailure { times: u32 },
    /// The run ended and its last model turn produced text.
    Ends,
}

impl Rule {
    /// Short stable name used in reports.
    pub fn name(&self) -> &'static str {
        match self {
            Rule::Requires { .. } => "requires",
            Rule::Order { .. } => "order",
            Rule::Forbids { .. } => "forbids",
            Rule::Args { .. } => "args",
            Rule::MaxSteps { .. } => "max_steps",
            Rule::NoRepeatedFailure { .. } => "no_repeated_failure",
            Rule::Ends => "ends",
        }
    }

    /// Reject a rule whose regex does not compile. Loading a rule file calls
    /// this so a typo fails up front instead of at the end of a long check.
    pub fn validate(&self) -> Result<(), String> {
        let pat = match self {
            Rule::Forbids {
                args_match: Some(p),
                ..
            } => p,
            Rule::Args { pattern, .. } => pattern,
            _ => return Ok(()),
        };
        Regex::new(pat)
            .map(|_| ())
            .map_err(|e| format!("{}: invalid regex {pat:?}: {e}", self.name()))
    }

    fn check(&self, t: &Trajectory, out: &mut Vec<Violation>) {
        let name = self.name();
        let mut fail = |step: Option<usize>, message: String| {
            out.push(Violation {
                rule: name.into(),
                message,
                step,
            })
        };
        match self {
            Rule::Requires { tool } => {
                if !t.tool_steps().any(|(_, s)| s.name == tool && s.ok) {
                    fail(None, format!("`{tool}` was never called successfully"));
                }
            }
            Rule::Order { before, after } => {
                let first_after = t.tool_steps().find(|(_, s)| s.name == after);
                let first_before = t.tool_steps().find(|(_, s)| s.name == before && s.ok);
                if let Some((ai, _)) = first_after {
                    match first_before {
                        Some((bi, _)) if bi < ai => {}
                        Some(_) => fail(
                            Some(ai),
                            format!("`{after}` was called before the first successful `{before}`"),
                        ),
                        None => fail(
                            Some(ai),
                            format!("`{after}` was called without a prior successful `{before}`"),
                        ),
                    }
                }
            }
            Rule::Forbids { tool, args_match } => {
                let re = args_match.as_deref().map(Regex::new);
                let re = match re {
                    Some(Ok(r)) => Some(r),
                    Some(Err(e)) => {
                        fail(None, format!("invalid regex: {e}"));
                        return;
                    }
                    None => None,
                };
                for (i, s) in t.tool_steps().filter(|(_, s)| s.name == tool) {
                    let args = s.args.to_string();
                    match &re {
                        None => fail(Some(i), format!("forbidden tool `{tool}` was called")),
                        Some(r) if r.is_match(&args) => fail(
                            Some(i),
                            format!("forbidden call to `{tool}` with args {args}"),
                        ),
                        Some(_) => {}
                    }
                }
            }
            Rule::Args {
                tool,
                field,
                pattern,
            } => {
                let re = match Regex::new(pattern) {
                    Ok(r) => r,
                    Err(e) => {
                        fail(None, format!("invalid regex: {e}"));
                        return;
                    }
                };
                for (i, s) in t.tool_steps().filter(|(_, s)| s.name == tool) {
                    let v = if field.starts_with('/') {
                        s.args.pointer(field)
                    } else {
                        s.args.get(field.as_str())
                    };
                    match v.and_then(Value::as_str) {
                        None => fail(
                            Some(i),
                            format!("`{tool}` has no string at `{field}` (args {})", s.args),
                        ),
                        Some(text) if !re.is_match(text) => fail(
                            Some(i),
                            format!("`{tool}` `{field}` = {text:?} does not match /{pattern}/"),
                        ),
                        Some(_) => {}
                    }
                }
            }
            Rule::MaxSteps {
                model_calls,
                tool_calls,
                tokens,
            } => {
                if let Some(max) = model_calls
                    && t.model_calls > *max as usize
                {
                    fail(
                        None,
                        format!("{} model calls exceeds the limit of {max}", t.model_calls),
                    );
                }
                if let Some(max) = tool_calls
                    && t.tool_calls > *max as usize
                {
                    fail(
                        None,
                        format!("{} tool calls exceeds the limit of {max}", t.tool_calls),
                    );
                }
                if let Some(max) = tokens {
                    let total = u64::from(t.input_tokens) + u64::from(t.output_tokens);
                    if total > u64::from(*max) {
                        fail(None, format!("{total} tokens exceeds the limit of {max}"));
                    }
                }
            }
            Rule::NoRepeatedFailure { times } => {
                let limit = (*times).max(1) as usize;
                let mut seen: HashMap<(String, String), usize> = HashMap::new();
                for (i, s) in t.tool_steps().filter(|(_, s)| !s.ok) {
                    let n = seen
                        .entry((s.name.to_string(), s.args.to_string()))
                        .or_insert(0);
                    *n += 1;
                    if *n == limit {
                        fail(
                            Some(i),
                            format!(
                                "`{}` failed {limit} times with the same args {} ({})",
                                s.name,
                                s.args,
                                s.error.unwrap_or("no error")
                            ),
                        );
                    }
                }
            }
            Rule::Ends => {
                if !t.ended {
                    fail(None, "no End event: the run was cut off".into());
                }
                let last_model = t
                    .steps
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, s)| matches!(s, Step::Model { .. }));
                match last_model {
                    Some((_, Step::Model { has_text: true, .. })) => {}
                    Some((i, _)) => fail(Some(i), "the last model turn produced no text".into()),
                    None => fail(None, "the run has no model turn".into()),
                }
            }
        }
    }
}

/// One broken rule.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    /// The rule's [`Rule::name`].
    pub rule: String,
    pub message: String,
    /// Index into [`Trajectory::steps`], when the problem has a location.
    pub step: Option<usize>,
}

/// Result of checking a trajectory against rules.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub violations: Vec<Violation>,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.violations.is_empty()
    }

    /// Panic with the full report if any rule failed. For tests, where the
    /// panic message is the diagnosis.
    #[track_caller]
    pub fn assert_ok(&self) {
        assert!(self.is_ok(), "trajectory check failed:\n{self}");
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.violations.is_empty() {
            return write!(f, "trajectory ok");
        }
        for (i, v) in self.violations.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            match v.step {
                Some(s) => write!(f, "[{}] step {}: {}", v.rule, s, v.message)?,
                None => write!(f, "[{}] {}", v.rule, v.message)?,
            }
        }
        Ok(())
    }
}

/// Builder for a set of rules, for checks written in Rust.
#[derive(Debug, Clone, Default)]
pub struct TrajectoryCheck {
    rules: Vec<Rule>,
}

impl TrajectoryCheck {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add any rule, including ones the shortcuts below do not cover.
    pub fn rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    pub fn requires(self, tool: &str) -> Self {
        self.rule(Rule::Requires { tool: tool.into() })
    }

    pub fn order(self, before: &str, after: &str) -> Self {
        self.rule(Rule::Order {
            before: before.into(),
            after: after.into(),
        })
    }

    pub fn forbids(self, tool: &str, args_match: Option<&str>) -> Self {
        self.rule(Rule::Forbids {
            tool: tool.into(),
            args_match: args_match.map(Into::into),
        })
    }

    pub fn args(self, tool: &str, field: &str, pattern: &str) -> Self {
        self.rule(Rule::Args {
            tool: tool.into(),
            field: field.into(),
            pattern: pattern.into(),
        })
    }

    pub fn max_model_calls(self, n: u32) -> Self {
        self.rule(Rule::MaxSteps {
            model_calls: Some(n),
            tool_calls: None,
            tokens: None,
        })
    }

    pub fn max_tool_calls(self, n: u32) -> Self {
        self.rule(Rule::MaxSteps {
            model_calls: None,
            tool_calls: Some(n),
            tokens: None,
        })
    }

    pub fn max_tokens(self, n: u32) -> Self {
        self.rule(Rule::MaxSteps {
            model_calls: None,
            tool_calls: None,
            tokens: Some(n),
        })
    }

    pub fn no_repeated_failure(self, times: u32) -> Self {
        self.rule(Rule::NoRepeatedFailure { times })
    }

    pub fn ends(self) -> Self {
        self.rule(Rule::Ends)
    }

    /// Run every rule; all violations are collected, not just the first.
    pub fn check(&self, t: &Trajectory) -> Report {
        check_rules(&self.rules, t)
    }
}

fn check_rules(rules: &[Rule], t: &Trajectory) -> Report {
    let mut violations = Vec::new();
    for r in rules {
        r.check(t, &mut violations);
    }
    Report { violations }
}

/// Rules loaded from a TOML file (`[[rules]]` tables).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuleSet {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// Parse and validate rules from TOML text.
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        let set: RuleSet = toml::from_str(s)?;
        for r in &set.rules {
            r.validate().map_err(anyhow::Error::msg)?;
        }
        Ok(set)
    }

    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        Self::from_toml_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
    }

    pub fn check(&self, t: &Trajectory) -> Report {
        check_rules(&self.rules, t)
    }
}
