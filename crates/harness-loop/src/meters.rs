//! Metrics — what runs cost and how they behave, as numbers a dashboard can
//! aggregate across thousands of runs. Traces answer "what happened in this
//! run"; these answer "what is p95 tool latency this week" and "which model is
//! eating the token budget", which no amount of per-run spans can.
//!
//! Emitted through the [`metrics`] facade, the way spans go through `tracing`:
//! the loop records, the *binary* installs a recorder — Prometheus via
//! `harness-rs-serve`'s `prometheus` feature, or any other `metrics` exporter
//! (OpenTelemetry, StatsD, …). With no recorder installed every call is a
//! no-op, so the library pays nothing for a host that does not care.
//!
//! | name                               | kind      | unit      | labels |
//! |------------------------------------|-----------|-----------|--------|
//! | `gen_ai.client.operation.duration` | histogram | seconds   | `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `error.type` (on failure) |
//! | `gen_ai.client.token.usage`        | histogram | tokens    | `gen_ai.token.type` (`input` / `output`), `gen_ai.provider.name`, `gen_ai.request.model` |
//! | `harness.tool.duration`            | histogram | seconds   | `gen_ai.tool.name`, `ok` |
//! | `harness.run.duration`             | histogram | seconds   | `outcome` (`done`, `budget_exhausted`, `stuck`, `cancelled`, `error`) |
//! | `harness.compaction.tokens_saved`  | counter   | tokens    | `stage` |
//!
//! The two `gen_ai.client.*` metrics are the OpenTelemetry GenAI semantic
//! conventions, so backends that know them chart cost and latency unmapped.
//! There is no separate call counter: a histogram's count *is* the number of
//! calls. Prometheus exporters rewrite the dots to underscores
//! (`gen_ai_client_operation_duration_seconds_bucket`, …).
//!
//! Labels are bounded on purpose — model, provider, tool name, outcome, stage.
//! Never a request id or a user: those belong on the trace, where one value per
//! run is the point rather than a cardinality explosion.

use harness_core::{CompactionStage, ModelInfo, Usage};
use std::time::Duration;

pub const OPERATION_DURATION: &str = "gen_ai.client.operation.duration";
pub const TOKEN_USAGE: &str = "gen_ai.client.token.usage";
pub const TOOL_DURATION: &str = "harness.tool.duration";
pub const RUN_DURATION: &str = "harness.run.duration";
pub const TOKENS_SAVED: &str = "harness.compaction.tokens_saved";

/// Register units and help text with the installed recorder. Call once, after
/// installing it; the metrics work without this, they just export bare.
pub fn describe() {
    use metrics::{Unit, describe_counter, describe_histogram};
    describe_histogram!(
        OPERATION_DURATION,
        Unit::Seconds,
        "Duration of one model call, cancelled and failed calls included."
    );
    describe_histogram!(
        TOKEN_USAGE,
        Unit::Count,
        "Tokens per model call, by gen_ai.token.type."
    );
    describe_histogram!(
        TOOL_DURATION,
        Unit::Seconds,
        "Duration of one tool call, deadline and cancellation included."
    );
    describe_histogram!(
        RUN_DURATION,
        Unit::Seconds,
        "Duration of one agent run, by how it ended."
    );
    describe_counter!(
        TOKENS_SAVED,
        Unit::Count,
        "Context tokens removed by compaction, by stage."
    );
}

/// How a model call ended, for its duration's `error.type`.
pub(crate) enum ModelCallEnd<'a> {
    Done(&'a Usage),
    Failed,
    Cancelled,
}

pub(crate) fn model_call(info: &ModelInfo, elapsed: Duration, end: ModelCallEnd<'_>) {
    let provider = info.provider.clone();
    let model = info.model.clone();
    let duration = elapsed.as_secs_f64();
    match end {
        ModelCallEnd::Done(usage) => {
            metrics::histogram!(
                OPERATION_DURATION,
                "gen_ai.operation.name" => "chat",
                "gen_ai.provider.name" => provider.clone(),
                "gen_ai.request.model" => model.clone(),
            )
            .record(duration);
            for (kind, n) in [
                ("input", usage.input_tokens),
                ("output", usage.output_tokens),
            ] {
                metrics::histogram!(
                    TOKEN_USAGE,
                    "gen_ai.token.type" => kind,
                    "gen_ai.provider.name" => provider.clone(),
                    "gen_ai.request.model" => model.clone(),
                )
                .record(n as f64);
            }
        }
        ModelCallEnd::Failed | ModelCallEnd::Cancelled => {
            let error = if matches!(end, ModelCallEnd::Failed) {
                "model_error"
            } else {
                "cancelled"
            };
            metrics::histogram!(
                OPERATION_DURATION,
                "gen_ai.operation.name" => "chat",
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => model,
                "error.type" => error,
            )
            .record(duration);
        }
    }
}

pub(crate) fn tool_call(tool: &str, ok: bool, elapsed: Duration) {
    metrics::histogram!(
        TOOL_DURATION,
        "gen_ai.tool.name" => tool.to_string(),
        "ok" => if ok { "true" } else { "false" },
    )
    .record(elapsed.as_secs_f64());
}

pub(crate) fn run(outcome: &'static str, elapsed: Duration) {
    metrics::histogram!(RUN_DURATION, "outcome" => outcome).record(elapsed.as_secs_f64());
}

pub(crate) fn compaction(stage: CompactionStage, before: u32, after: u32) {
    metrics::counter!(TOKENS_SAVED, "stage" => format!("{stage:?}"))
        .increment(before.saturating_sub(after) as u64);
}
