//! OpenTelemetry tracing hook.
//!
//! Builds a parent span at `SessionStart`, child spans around each
//! `PreModel→PostModel`, `PreToolUse→PostToolUse`, `PreCompact→PostCompact`,
//! and `PreSensor→PostSensor` pair. Token usage is recorded as span
//! attributes. Closes the parent at `SessionEnd`.
//!
//! Gated behind the `otel` feature so the framework keeps a zero-dependency
//! default for users who don't need OTel.
//!
//! Usage (after configuring a global TracerProvider as per the
//! `opentelemetry-sdk` docs):
//!
//! ```ignore
//! let otel = Arc::new(OtelHook::new("my-agent"));
//! let loop_ = AgentLoop::new(model).with_hook(otel);
//! ```

use harness_core::{Event, Hook, HookOutcome, World};
use opentelemetry::{
    KeyValue,
    global::{self, BoxedSpan},
    trace::{Span, SpanKind, Status, Tracer, TracerProvider},
};
use std::sync::Mutex;

pub struct OtelHook {
    tracer_name: String,
    state: Mutex<OtelState>,
}

#[derive(Default)]
struct OtelState {
    parent: Option<BoxedSpan>,
    in_flight_model: Option<BoxedSpan>,
    in_flight_tool: Option<BoxedSpan>,
    in_flight_sensor: Option<BoxedSpan>,
    in_flight_compact: Option<BoxedSpan>,
}

impl OtelHook {
    pub fn new(tracer_name: impl Into<String>) -> Self {
        Self {
            tracer_name: tracer_name.into(),
            state: Mutex::new(OtelState::default()),
        }
    }

    fn tracer(&self) -> opentelemetry::global::BoxedTracer {
        global::tracer_provider().tracer(self.tracer_name.clone())
    }
}

impl Hook for OtelHook {
    fn name(&self) -> &str {
        "otel-tracer"
    }
    fn matches(&self, _ev: &Event<'_>) -> bool {
        true
    }

    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        let tracer = self.tracer();
        let Ok(mut state) = self.state.lock() else {
            return HookOutcome::Allow;
        };

        match ev {
            Event::SessionStart { source } => {
                let mut span = tracer
                    .span_builder("harness.session")
                    .with_kind(SpanKind::Internal)
                    .with_attributes(vec![KeyValue::new("source", format!("{source:?}"))])
                    .start(&tracer);
                span.set_attribute(KeyValue::new("framework", "harness"));
                state.parent = Some(span);
            }
            Event::SessionEnd => {
                if let Some(mut p) = state.parent.take() {
                    p.set_status(Status::Ok);
                    p.end();
                }
            }
            Event::PreModel { ctx } => {
                let mut s = tracer
                    .span_builder("harness.model.complete")
                    .with_kind(SpanKind::Client)
                    .with_attributes(vec![
                        KeyValue::new("history.len", ctx.history.len() as i64),
                        KeyValue::new("tools.count", ctx.tools.len() as i64),
                    ])
                    .start(&tracer);
                s.set_attribute(KeyValue::new("phase", "request"));
                state.in_flight_model = Some(s);
            }
            Event::PostModel { out } => {
                if let Some(mut s) = state.in_flight_model.take() {
                    s.set_attribute(KeyValue::new("tokens.input", out.usage.input_tokens as i64));
                    s.set_attribute(KeyValue::new(
                        "tokens.output",
                        out.usage.output_tokens as i64,
                    ));
                    s.set_attribute(KeyValue::new("tool_calls", out.tool_calls.len() as i64));
                    s.set_attribute(KeyValue::new(
                        "stop_reason",
                        format!("{:?}", out.stop_reason),
                    ));
                    s.set_status(Status::Ok);
                    s.end();
                }
            }
            Event::PreToolUse { action } => {
                let mut s = tracer
                    .span_builder(format!("harness.tool.{}", action.tool))
                    .with_kind(SpanKind::Internal)
                    .with_attributes(vec![
                        KeyValue::new("tool.name", action.tool.clone()),
                        KeyValue::new("tool.call_id", action.call_id.clone()),
                    ])
                    .start(&tracer);
                s.set_attribute(KeyValue::new("phase", "invoke"));
                state.in_flight_tool = Some(s);
            }
            Event::PostToolUse { action, result } => {
                if let Some(mut s) = state.in_flight_tool.take() {
                    s.set_attribute(KeyValue::new("tool.ok", result.ok));
                    s.set_status(if result.ok {
                        Status::Ok
                    } else {
                        Status::error(format!("tool {} failed", action.tool))
                    });
                    s.end();
                }
            }
            Event::PreSensor { sensor } => {
                let s = tracer
                    .span_builder(format!("harness.sensor.{sensor}"))
                    .with_kind(SpanKind::Internal)
                    .start(&tracer);
                state.in_flight_sensor = Some(s);
            }
            Event::PostSensor { signals, .. } => {
                if let Some(mut s) = state.in_flight_sensor.take() {
                    s.set_attribute(KeyValue::new("signals.count", signals.len() as i64));
                    s.set_status(Status::Ok);
                    s.end();
                }
            }
            Event::PreCompact { stage } => {
                let s = tracer
                    .span_builder(format!("harness.compact.{stage:?}"))
                    .with_kind(SpanKind::Internal)
                    .start(&tracer);
                state.in_flight_compact = Some(s);
            }
            Event::PostCompact { .. } => {
                if let Some(mut s) = state.in_flight_compact.take() {
                    s.set_status(Status::Ok);
                    s.end();
                }
            }
            _ => {} // silent on the rest
        }
        HookOutcome::Allow
    }
}
