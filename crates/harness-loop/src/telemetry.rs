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
//! Span/event shape (target `harness.telemetry`):
//!
//! ```text
//! agent_run (span, fields: source)
//!   ├─ run.start
//!   ├─ iter            (iter)
//!   ├─ model.complete  (input_tokens, output_tokens, cached_input_tokens, tool_calls, stop)
//!   ├─ tool.call       (tool, ok, duration_ms)
//!   ├─ sensor          (sensor, signals)
//!   ├─ compact         (stage)
//!   ├─ budget.warning  (ratio)
//!   └─ run.end
//! ```
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
}

impl TelemetryHook {
    pub fn new() -> Self {
        Self {
            run: Mutex::new(None),
            tool_starts: Mutex::new(HashMap::new()),
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
                    source = format!("{source:?}")
                );
                span.in_scope(|| {
                    tracing::info!(target: "harness.telemetry", event = "run.start");
                });
                *self.run.lock().unwrap() = Some(span);
            }
            Event::Heartbeat { iter } => self.in_run(|| {
                tracing::info!(target: "harness.telemetry", event = "iter", iter = *iter);
            }),
            Event::PostModel { out } => self.in_run(|| {
                tracing::info!(
                    target: "harness.telemetry",
                    event = "model.complete",
                    input_tokens = out.usage.input_tokens,
                    output_tokens = out.usage.output_tokens,
                    cached_input_tokens = out.usage.cached_input_tokens,
                    tool_calls = out.tool_calls.len(),
                    stop = format!("{:?}", out.stop_reason),
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
                self.in_run(|| {
                    tracing::info!(
                        target: "harness.telemetry",
                        event = "tool.call",
                        tool = %action.tool,
                        ok = result.ok,
                        duration_ms,
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
            Event::PostCompact { stage } => self.in_run(|| {
                tracing::debug!(
                    target: "harness.telemetry",
                    event = "compact",
                    stage = format!("{stage:?}"),
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
                self.in_run(|| {
                    tracing::info!(target: "harness.telemetry", event = "run.end");
                });
                *self.run.lock().unwrap() = None;
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}
