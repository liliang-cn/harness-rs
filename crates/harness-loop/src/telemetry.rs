//! `TelemetryHook` — maps the agent's lifecycle [`Event`] stream onto
//! structured `tracing` spans and events, so a run becomes observable in any
//! `tracing` subscriber.
//!
//! Why `tracing` and not a hard OpenTelemetry dependency? Because `tracing` is
//! the idiomatic Rust instrumentation seam: the library emits spans + events,
//! and the *binary* chooses the exporter. Attach
//! [`tracing-opentelemetry`](https://docs.rs/tracing-opentelemetry) with an
//! OTLP pipeline and every span below is exported to Jaeger / Tempo / any OTLP
//! backend with **zero changes here**; attach `tracing_subscriber::fmt().json()`
//! and you get newline-delimited JSON for log pipelines. One instrumentation,
//! many backends.
//!
//! Field names follow the OpenTelemetry **GenAI semantic conventions**
//! (`gen_ai.*`), so any OTel-aware backend — Logfire, SigNoz, Langfuse via OTLP,
//! Grafana — recognizes token counts, model, and finish reason automatically and
//! computes cost/latency with zero mapping. The pre-convention flat names
//! (`input_tokens`, `tool`, …) are kept alongside as aliases for existing
//! consumers.
//!
//! Span/event shape (target `harness.telemetry`):
//!
//! ```text
//! agent_run (span, fields: source, gen_ai.operation.name=invoke_agent)
//!   ├─ run.start
//!   ├─ iter            (iter)
//!   ├─ model.complete  (gen_ai.operation.name=chat,
//!   │                   gen_ai.usage.input_tokens, gen_ai.usage.output_tokens,
//!   │                   gen_ai.usage.cached_input_tokens,
//!   │                   gen_ai.response.finish_reasons
//!   │                   + aliases: input_tokens, output_tokens,
//!   │                     cached_input_tokens, tool_calls, stop)
//!   ├─ tool.call       (gen_ai.operation.name=execute_tool, gen_ai.tool.name,
//!   │                   ok, duration_ms + alias: tool)
//!   ├─ sensor          (sensor, signals)
//!   ├─ compact         (stage, tokens_before, tokens_after, tokens_saved)
//!   ├─ budget.warning  (ratio)
//!   └─ run.end         (gen_ai.usage.*, total_tokens, model_calls, tool_calls,
//!                       tool_failures, compactions, tokens_saved, duration_ms)
//! ```
//!
//! To export over OTLP, enable the crate's `otel` feature and call
//! [`crate::otel::init_tracing_with_otlp`] from your binary; see that module.
//!
//! Wire it like any hook:
//! ```ignore
//! let loop_ = AgentLoop::new(model).with_hook(std::sync::Arc::new(TelemetryHook::new()));
//! ```

use harness_core::{Event, Hook, HookOutcome, World};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Emits a span per run and a structured event per model call, tool call,
/// sensor, compaction, and budget warning. See the module docs for the OTLP
/// bridge.
pub struct TelemetryHook {
    /// The current run's span. Events are recorded inside it so an OTLP exporter
    /// nests them under one trace.
    run: Mutex<Option<tracing::Span>>,
    /// `call_id -> dispatch start`, so `tool.call` can report a duration.
    tool_starts: Mutex<HashMap<String, Instant>>,
    /// When the current model call was handed off, so the run summary can say
    /// how much of the wall clock was spent waiting on the provider.
    model_start: Mutex<Option<Instant>>,
    /// Running totals for the whole run, so `run.end` can answer "what did this
    /// cost?" without the reader summing per-turn lines by hand.
    totals: Mutex<RunTotals>,
}

/// What a run added up to, accumulated across its turns.
#[derive(Default)]
struct RunTotals {
    started: Option<Instant>,
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    model_calls: u64,
    tool_calls: u64,
    tool_failures: u64,
    compactions: u64,
    tokens_saved: u64,
    /// Wall-clock spent inside model calls, and the summed duration of tool
    /// calls. Tools dispatched in parallel overlap, so `tool_ms` can exceed the
    /// wall clock it occupied — it is a cost, not a span.
    model_ms: u64,
    tool_ms: u64,
    /// Read-only calls that exactly repeated an earlier one — wasted rounds the
    /// stuck-detector cannot see, because they are not consecutive.
    repeats: u64,
}

impl TelemetryHook {
    pub fn new() -> Self {
        Self {
            run: Mutex::new(None),
            tool_starts: Mutex::new(HashMap::new()),
            model_start: Mutex::new(None),
            totals: Mutex::new(RunTotals::default()),
        }
    }

    /// Run `f` inside the current run span (if any), so its events attach to the
    /// run's trace. Falls back to the ambient subscriber if no run is active.
    fn in_run<F: FnOnce()>(&self, f: F) {
        let guard = self.run.lock().unwrap();
        match &*guard {
            Some(span) => span.in_scope(f),
            None => f(),
        }
    }
}

impl Default for TelemetryHook {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for TelemetryHook {
    fn name(&self) -> &str {
        "telemetry"
    }
    fn matches(&self, _ev: &Event<'_>) -> bool {
        true
    }

    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        match ev {
            Event::SessionStart { source } => {
                let span = tracing::info_span!(
                    target: "harness.telemetry",
                    "agent_run",
                    "gen_ai.operation.name" = "invoke_agent",
                    source = format!("{source:?}")
                );
                span.in_scope(|| {
                    tracing::info!(target: "harness.telemetry", event = "run.start");
                });
                *self.run.lock().unwrap() = Some(span);
                *self.totals.lock().unwrap() = RunTotals {
                    started: Some(Instant::now()),
                    ..Default::default()
                };
            }
            Event::PreModel { .. } => {
                *self.model_start.lock().unwrap() = Some(Instant::now());
            }
            Event::Heartbeat { iter } => self.in_run(|| {
                tracing::info!(target: "harness.telemetry", event = "iter", iter = *iter);
            }),
            Event::PostModel { out } => self.in_run(|| {
                let waited = self
                    .model_start
                    .lock()
                    .unwrap()
                    .take()
                    .map(|s| s.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                {
                    let mut t = self.totals.lock().unwrap();
                    t.model_calls += 1;
                    t.model_ms += waited;
                    t.input_tokens += out.usage.input_tokens as u64;
                    t.output_tokens += out.usage.output_tokens as u64;
                    t.cached_input_tokens += out.usage.cached_input_tokens as u64;
                }
                let stop = format!("{:?}", out.stop_reason);
                tracing::info!(
                    target: "harness.telemetry",
                    event = "model.complete",
                    // OTel GenAI semantic conventions:
                    "gen_ai.operation.name" = "chat",
                    "gen_ai.usage.input_tokens" = out.usage.input_tokens,
                    "gen_ai.usage.output_tokens" = out.usage.output_tokens,
                    "gen_ai.usage.cached_input_tokens" = out.usage.cached_input_tokens,
                    "gen_ai.response.finish_reasons" = %stop,
                    // pre-convention aliases:
                    input_tokens = out.usage.input_tokens,
                    output_tokens = out.usage.output_tokens,
                    cached_input_tokens = out.usage.cached_input_tokens,
                    tool_calls = out.tool_calls.len(),
                    stop = %stop,
                    duration_ms = waited,
                );
            }),
            Event::PreToolUse { action } => {
                self.tool_starts
                    .lock()
                    .unwrap()
                    .insert(action.call_id.clone(), Instant::now());
            }
            Event::PostToolUse { action, result } => {
                let duration_ms = self
                    .tool_starts
                    .lock()
                    .unwrap()
                    .remove(&action.call_id)
                    .map(|s| s.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let repeat = result
                    .content
                    .get("repeat_of_earlier_call")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                {
                    let mut t = self.totals.lock().unwrap();
                    t.tool_calls += 1;
                    t.tool_ms += duration_ms;
                    if repeat {
                        t.repeats += 1;
                    }
                    if !result.ok {
                        t.tool_failures += 1;
                    }
                }
                self.in_run(|| {
                    tracing::info!(
                        target: "harness.telemetry",
                        event = "tool.call",
                        "gen_ai.operation.name" = "execute_tool",
                        "gen_ai.tool.name" = %action.tool,
                        ok = result.ok,
                        duration_ms,
                        tool = %action.tool, // alias
                    );
                });
            }
            Event::PostSensor { sensor, signals } => self.in_run(|| {
                tracing::debug!(
                    target: "harness.telemetry",
                    event = "sensor",
                    sensor = %sensor,
                    signals = signals.len(),
                );
            }),
            Event::PostCompact {
                stage,
                before,
                after,
            } => self.in_run(|| {
                let saved = before.saturating_sub(*after);
                {
                    let mut t = self.totals.lock().unwrap();
                    t.compactions += 1;
                    t.tokens_saved += saved as u64;
                }
                // At info, not debug: compaction is what keeps a long run
                // affordable, and "it ran" without "it saved 12k" is not an
                // observation anyone can act on.
                tracing::info!(
                    target: "harness.telemetry",
                    event = "compact",
                    stage = format!("{stage:?}"),
                    tokens_before = *before,
                    tokens_after = *after,
                    tokens_saved = saved,
                );
            }),
            Event::BudgetWarning { ratio } => self.in_run(|| {
                tracing::warn!(
                    target: "harness.telemetry",
                    event = "budget.warning",
                    ratio = *ratio,
                );
            }),
            Event::SessionEnd => {
                let t = std::mem::take(&mut *self.totals.lock().unwrap());
                self.in_run(|| {
                    // One line with the whole bill. Per-turn events answer "what
                    // happened"; this answers "what did it cost", which is the
                    // question asked after every run and previously required
                    // adding the turns up by hand.
                    tracing::info!(
                        target: "harness.telemetry",
                        event = "run.end",
                        "gen_ai.usage.input_tokens" = t.input_tokens,
                        "gen_ai.usage.output_tokens" = t.output_tokens,
                        "gen_ai.usage.cached_input_tokens" = t.cached_input_tokens,
                        total_tokens = t.input_tokens + t.output_tokens,
                        model_calls = t.model_calls,
                        tool_calls = t.tool_calls,
                        tool_failures = t.tool_failures,
                        compactions = t.compactions,
                        tokens_saved = t.tokens_saved,
                        model_ms = t.model_ms,
                        tool_ms = t.tool_ms,
                        repeat_calls = t.repeats,
                        duration_ms = t.started.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0),
                    );
                });
                *self.run.lock().unwrap() = None;
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}
