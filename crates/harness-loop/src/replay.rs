//! Session record + replay (DESIGN.md §15 v0.2+).
//!
//! Two halves:
//! - [`SessionRecorder`] is a [`Hook`] that captures every lifecycle event
//!   to a JSONL file. Wire it via `AgentLoop::with_hook` and you get a
//!   complete trace of what the agent did.
//! - [`read_session`] + [`replay_as_mock`] reconstruct a deterministic
//!   `MockModel` from a recorded log so you can replay the run offline,
//!   verify changes, or debug failures without rerunning against a real LLM.

use harness_core::{
    Action, CompactionStage, Event, Hook, HookOutcome, ModelOutput, ToolResult, World,
};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// One event in the recorded session. Owned (no borrows) so it round-trips
/// through serde without lifetime gymnastics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    Start {
        ts_ms: i64,
        source: String,
    },
    PreModel {
        ts_ms: i64,
        history_len: usize,
        tools_count: usize,
    },
    PostModel {
        ts_ms: i64,
        output: ModelOutput,
    },
    PreTool {
        ts_ms: i64,
        action: Action,
    },
    PostTool {
        ts_ms: i64,
        call_id: String,
        result: ToolResult,
    },
    Sensor {
        ts_ms: i64,
        id: String,
        signals: usize,
    },
    PreCompact {
        ts_ms: i64,
        stage: CompactionStage,
    },
    PostCompact {
        ts_ms: i64,
        stage: CompactionStage,
    },
    Heartbeat {
        ts_ms: i64,
        iter: u32,
    },
    End {
        ts_ms: i64,
    },
}

/// Hook that serialises every relevant lifecycle event into a JSONL file.
///
/// Failures (locked mutex, I/O errors) are logged via `tracing::warn` but
/// never panic — recording is a best-effort observability layer, not a
/// correctness path.
pub struct SessionRecorder {
    file: Mutex<std::fs::File>,
}

impl SessionRecorder {
    /// Open the file for append (creating it if needed).
    pub fn new(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(f),
        })
    }

    fn write(&self, ev: &SessionEvent) {
        let Ok(mut f) = self.file.lock() else {
            return;
        };
        match serde_json::to_string(ev) {
            Ok(s) => {
                if let Err(e) = writeln!(f, "{s}") {
                    tracing::warn!(error=%e, "session recorder write failed");
                }
            }
            Err(e) => tracing::warn!(error=%e, "session recorder serialize failed"),
        }
    }
}

impl Hook for SessionRecorder {
    fn name(&self) -> &str {
        "session-recorder"
    }
    fn matches(&self, _ev: &Event<'_>) -> bool {
        true
    }

    fn fire(&self, ev: &Event<'_>, world: &mut World) -> HookOutcome {
        let ts = world.clock.now_ms();
        let session_ev = match ev {
            Event::SessionStart { source } => Some(SessionEvent::Start {
                ts_ms: ts,
                source: format!("{source:?}"),
            }),
            Event::PreModel { ctx } => Some(SessionEvent::PreModel {
                ts_ms: ts,
                history_len: ctx.history.len(),
                tools_count: ctx.tools.len(),
            }),
            Event::PostModel { out } => Some(SessionEvent::PostModel {
                ts_ms: ts,
                output: (*out).clone(),
            }),
            Event::PreToolUse { action } => Some(SessionEvent::PreTool {
                ts_ms: ts,
                action: (*action).clone(),
            }),
            Event::PostToolUse { action, result } => Some(SessionEvent::PostTool {
                ts_ms: ts,
                call_id: action.call_id.clone(),
                result: (*result).clone(),
            }),
            Event::PostSensor { sensor, signals } => Some(SessionEvent::Sensor {
                ts_ms: ts,
                id: (*sensor).clone(),
                signals: signals.len(),
            }),
            Event::PreCompact { stage } => Some(SessionEvent::PreCompact {
                ts_ms: ts,
                stage: *stage,
            }),
            Event::PostCompact { stage } => Some(SessionEvent::PostCompact {
                ts_ms: ts,
                stage: *stage,
            }),
            Event::Heartbeat { iter } => Some(SessionEvent::Heartbeat {
                ts_ms: ts,
                iter: *iter,
            }),
            Event::SessionEnd => Some(SessionEvent::End { ts_ms: ts }),
            _ => None,
        };
        if let Some(e) = session_ev {
            self.write(&e);
        }
        HookOutcome::Allow
    }
}

/// Read a recorded JSONL session log back into memory.
///
/// Tolerates malformed lines (logged, skipped) so a partially-corrupted log
/// still yields usable replay material.
pub fn read_session(path: &Path) -> std::io::Result<Vec<SessionEvent>> {
    let content = std::fs::read_to_string(path)?;
    let mut events = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(e) => events.push(e),
            Err(err) => tracing::warn!(line=i+1, error=%err, "session log line skipped"),
        }
    }
    Ok(events)
}

/// Build a [`harness_models::MockModel`] that returns each recorded
/// `PostModel` output in order. Pair with a fresh `AgentLoop` to replay the
/// run.
pub fn replay_as_mock(events: &[SessionEvent]) -> harness_models::MockModel {
    use harness_models::{MockModel, MockResponse};
    let mut m = MockModel::new().with_name("replay");
    for e in events {
        if let SessionEvent::PostModel { output, .. } = e {
            m = m.script(MockResponse {
                text: output.text.clone(),
                tool_calls: output.tool_calls.clone(),
                stop_reason: output.stop_reason,
                input_tokens: output.usage.input_tokens,
                output_tokens: output.usage.output_tokens,
                reasoning: output.reasoning.clone(),
            });
        }
    }
    m
}

/// Backwards-compatible alias.
pub fn replay_as_mock_via_events(events: &[SessionEvent]) -> harness_models::MockModel {
    replay_as_mock(events)
}

/// Stats from a single session — handy summary for the `harness trace` CLI.
#[derive(Debug, Clone, Default)]
pub struct SessionStats {
    pub events: usize,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub iters: u32,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub stages_run: usize,
    pub duration_ms: i64,
}

impl SessionStats {
    pub fn from(events: &[SessionEvent]) -> Self {
        let mut s = Self {
            events: events.len(),
            ..Default::default()
        };
        let mut first_ts: Option<i64> = None;
        let mut last_ts: Option<i64> = None;
        for e in events {
            let ts = match e {
                SessionEvent::Start { ts_ms, .. }
                | SessionEvent::PreModel { ts_ms, .. }
                | SessionEvent::PostModel { ts_ms, .. }
                | SessionEvent::PreTool { ts_ms, .. }
                | SessionEvent::PostTool { ts_ms, .. }
                | SessionEvent::Sensor { ts_ms, .. }
                | SessionEvent::PreCompact { ts_ms, .. }
                | SessionEvent::PostCompact { ts_ms, .. }
                | SessionEvent::Heartbeat { ts_ms, .. }
                | SessionEvent::End { ts_ms } => *ts_ms,
            };
            if first_ts.is_none() {
                first_ts = Some(ts);
            }
            last_ts = Some(ts);

            match e {
                SessionEvent::PostModel { output, .. } => {
                    s.model_calls += 1;
                    s.input_tokens += output.usage.input_tokens;
                    s.output_tokens += output.usage.output_tokens;
                }
                SessionEvent::PreTool { .. } => s.tool_calls += 1,
                SessionEvent::PostCompact { .. } => s.stages_run += 1,
                SessionEvent::Heartbeat { iter, .. } => s.iters = s.iters.max(*iter + 1),
                _ => {}
            }
        }
        s.duration_ms = match (first_ts, last_ts) {
            (Some(a), Some(b)) => b - a,
            _ => 0,
        };
        s
    }
}

/// Multi-line, content-rich version of [`format_event_short`].
///
/// Surfaces what `format_event_short` hides: model text, full tool args,
/// tool result preview, and failure reasons. Used by `harness trace --verbose`
/// and by [`LiveProgressHook`] so operators can actually see what their agent
/// is doing instead of guessing from `ok=false`.
pub fn format_event_verbose(e: &SessionEvent) -> String {
    match e {
        SessionEvent::Start { source, .. } => format!("session start ({source})"),
        SessionEvent::Heartbeat { iter, .. } => format!("iter {iter}"),
        SessionEvent::PreModel {
            history_len,
            tools_count,
            ..
        } => format!("→ model (history={history_len}, tools={tools_count})"),
        SessionEvent::PostModel { output, .. } => {
            let mut out = format!(
                "← model: {} tool_call(s) [{}/{} tok, stop={:?}]",
                output.tool_calls.len(),
                output.usage.input_tokens,
                output.usage.output_tokens,
                output.stop_reason,
            );
            if let Some(text) = output.text.as_deref().filter(|s| !s.is_empty()) {
                out.push_str("\n  text: ");
                out.push_str(&truncate(text, 400));
            }
            if let Some(reasoning) = output.reasoning.as_deref().filter(|s| !s.is_empty()) {
                out.push_str("\n  reasoning: ");
                out.push_str(&truncate(reasoning, 200));
            }
            out
        }
        SessionEvent::PreTool { action, .. } => {
            let args = action.args.to_string();
            format!("  → tool {} args={}", action.tool, truncate(&args, 240))
        }
        SessionEvent::PostTool {
            call_id, result, ..
        } => {
            let preview = preview_tool_result(result);
            format!(
                "  ← tool {} ok={} {}",
                call_id,
                result.ok,
                if preview.is_empty() {
                    String::new()
                } else {
                    format!("\n      {preview}")
                }
            )
        }
        SessionEvent::Sensor { id, signals, .. } => {
            format!("  ⚑ sensor {id}: {signals} signal(s)")
        }
        SessionEvent::PreCompact { stage, .. } => format!("  ⇩ pre-compact {stage:?}"),
        SessionEvent::PostCompact { stage, .. } => format!("  ⇧ post-compact {stage:?}"),
        SessionEvent::End { .. } => "session end".into(),
    }
}

/// Pull the most actionable text out of a [`ToolResult`] for human display.
///
/// For failures, prefer `errors`/`hint`/`message` keys if the tool returned a
/// structured JSON payload (the multi-engine search tool in `investor-bot`
/// follows this convention). Falls back to a truncated JSON dump.
fn preview_tool_result(r: &ToolResult) -> String {
    let v = &r.content;
    if !r.ok {
        // Try the common error-shape conventions first.
        if let Some(errors) = v.get("errors").and_then(|x| x.as_array()) {
            let joined: Vec<String> = errors
                .iter()
                .filter_map(|e| e.as_str().map(String::from))
                .collect();
            if !joined.is_empty() {
                let hint = v
                    .get("hint")
                    .and_then(|x| x.as_str())
                    .map(|h| format!(" | hint: {h}"))
                    .unwrap_or_default();
                return format!("errors=[{}]{hint}", truncate(&joined.join("; "), 240));
            }
        }
        if let Some(msg) = v.get("message").and_then(|x| x.as_str()) {
            return format!("message={}", truncate(msg, 240));
        }
        if let Some(err) = v.get("error").and_then(|x| x.as_str()) {
            return format!("error={}", truncate(err, 240));
        }
    }
    // Generic preview: serialize, trim, truncate.
    let s = v.to_string();
    if s == "null" || s == "{}" {
        String::new()
    } else {
        format!("preview={}", truncate(&s, 240))
    }
}

fn truncate(s: &str, max: usize) -> String {
    // Char-wise truncation so we don't bisect a multibyte sequence.
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.replace('\n', " ⏎ ")
    } else {
        let head: String = chars[..max].iter().collect();
        format!(
            "{}… ({} chars total)",
            head.replace('\n', " ⏎ "),
            chars.len()
        )
    }
}

/// Tiny helper used by the CLI: convert a single event to a single line of
/// pretty-printed text (does NOT include the timestamp prefix).
pub fn format_event_short(e: &SessionEvent) -> String {
    match e {
        SessionEvent::Start { source, .. } => format!("session start ({source})"),
        SessionEvent::Heartbeat { iter, .. } => format!("iter {iter}"),
        SessionEvent::PreModel {
            history_len,
            tools_count,
            ..
        } => {
            format!("→ model (history={history_len}, tools={tools_count})")
        }
        SessionEvent::PostModel { output, .. } => {
            let calls = output.tool_calls.len();
            let txt = output
                .text
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect::<String>();
            if calls > 0 {
                format!(
                    "← model: {} tool_call(s) [{}/{} tok]",
                    calls, output.usage.input_tokens, output.usage.output_tokens
                )
            } else {
                format!(
                    "← model: {:?} [{}/{} tok]",
                    txt, output.usage.input_tokens, output.usage.output_tokens
                )
            }
        }
        SessionEvent::PreTool { action, .. } => {
            format!("  → tool {} args={}", action.tool, action.args)
        }
        SessionEvent::PostTool {
            call_id, result, ..
        } => {
            format!("  ← tool {} ok={}", call_id, result.ok)
        }
        SessionEvent::Sensor { id, signals, .. } => format!("  ⚑ sensor {id}: {signals} signal(s)"),
        SessionEvent::PreCompact { stage, .. } => format!("  ⇩ pre-compact {stage:?}"),
        SessionEvent::PostCompact { stage, .. } => format!("  ⇧ post-compact {stage:?}"),
        SessionEvent::End { .. } => "session end".into(),
    }
}

/// `Hook` that prints a verbose progress trace to stderr in real time.
///
/// Pair with `AgentLoop::with_hook(Arc::new(LiveProgressHook::default()))` to
/// see model calls, tool calls, and tool results as they happen — instead of
/// staring at a silent terminal for 60 seconds and then post-mortem'ing a JSONL
/// file. Independent of `SessionRecorder`; both can be installed together.
///
/// Output is structured to be greppable: `[iter=N]` prefix on every line, and
/// each line is one event. Writes go to stderr, so stdout stays clean for
/// the final answer.
#[derive(Default)]
pub struct LiveProgressHook {
    iter: std::sync::atomic::AtomicU32,
}

impl LiveProgressHook {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Hook for LiveProgressHook {
    fn name(&self) -> &str {
        "live-progress"
    }
    fn matches(&self, _ev: &Event<'_>) -> bool {
        true
    }
    fn fire(&self, ev: &Event<'_>, world: &mut World) -> HookOutcome {
        let ts = world.clock.now_ms();
        let iter = self.iter.load(std::sync::atomic::Ordering::Relaxed);
        // Reuse the recorder's projection + the verbose formatter so the
        // live output is the same format you'd see post-mortem.
        let session_ev = match ev {
            Event::SessionStart { source } => Some(SessionEvent::Start {
                ts_ms: ts,
                source: format!("{source:?}"),
            }),
            Event::PreModel { ctx } => Some(SessionEvent::PreModel {
                ts_ms: ts,
                history_len: ctx.history.len(),
                tools_count: ctx.tools.len(),
            }),
            Event::PostModel { out } => Some(SessionEvent::PostModel {
                ts_ms: ts,
                output: (*out).clone(),
            }),
            Event::PreToolUse { action } => Some(SessionEvent::PreTool {
                ts_ms: ts,
                action: (*action).clone(),
            }),
            Event::PostToolUse { action, result } => Some(SessionEvent::PostTool {
                ts_ms: ts,
                call_id: action.call_id.clone(),
                result: (*result).clone(),
            }),
            Event::Heartbeat { iter: i } => {
                self.iter.store(*i, std::sync::atomic::Ordering::Relaxed);
                Some(SessionEvent::Heartbeat {
                    ts_ms: ts,
                    iter: *i,
                })
            }
            Event::PreCompact { stage } => Some(SessionEvent::PreCompact {
                ts_ms: ts,
                stage: *stage,
            }),
            Event::PostCompact { stage } => Some(SessionEvent::PostCompact {
                ts_ms: ts,
                stage: *stage,
            }),
            Event::SessionEnd => Some(SessionEvent::End { ts_ms: ts }),
            _ => None,
        };
        if let Some(e) = session_ev {
            for line in format_event_verbose(&e).lines() {
                eprintln!("[iter={iter}] {line}");
            }
        }
        HookOutcome::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_log() -> Vec<SessionEvent> {
        vec![
            SessionEvent::Start {
                ts_ms: 0,
                source: "Startup".into(),
            },
            SessionEvent::Heartbeat { ts_ms: 1, iter: 0 },
            SessionEvent::PreModel {
                ts_ms: 2,
                history_len: 1,
                tools_count: 3,
            },
            SessionEvent::PostModel {
                ts_ms: 100,
                output: ModelOutput {
                    text: Some("hi".into()),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                    stop_reason: harness_core::StopReason::EndTurn,
                    reasoning: None,
                },
            },
            SessionEvent::End { ts_ms: 110 },
        ]
    }

    #[test]
    fn stats_compute_correctly() {
        let s = SessionStats::from(&sample_log());
        assert_eq!(s.events, 5);
        assert_eq!(s.model_calls, 1);
        assert_eq!(s.iters, 1);
        assert_eq!(s.duration_ms, 110);
    }

    #[test]
    fn round_trip_via_serde() {
        let original = sample_log();
        let json: Vec<String> = original
            .iter()
            .map(|e| serde_json::to_string(e).unwrap())
            .collect();
        let parsed: Vec<SessionEvent> = json
            .iter()
            .map(|s| serde_json::from_str::<SessionEvent>(s).unwrap())
            .collect();
        assert_eq!(parsed.len(), original.len());
        assert!(
            matches!(parsed[3], SessionEvent::PostModel { ref output, .. } if output.text.as_deref() == Some("hi"))
        );
    }
}
