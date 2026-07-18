//! OTLP export for the agent's `tracing` telemetry (feature `otel`).
//!
//! `harness-rs-loop` instruments with plain `tracing` (see [`crate::telemetry`]).
//! This module is the last mile: it turns those spans into OpenTelemetry and
//! ships them over OTLP to any compatible backend — Jaeger, Grafana Tempo,
//! SigNoz, Logfire, Langfuse. Because [`TelemetryHook`](crate::TelemetryHook)
//! already tags spans with the GenAI semantic conventions (`gen_ai.*`), token
//! counts, model, and finish reason light up in those UIs with no extra mapping.
//!
//! The library never hard-depends on OpenTelemetry: enable the `otel` feature in
//! the *binary* to pull this in, and one instrumentation feeds many backends.
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//! // Ship every agent span to an OTLP collector on localhost:4317, then run the
//! // loop with a `TelemetryHook` as usual. `_otel` flushes on drop.
//! let _otel = harness_loop::otel::init_tracing_with_otlp(
//!     "my-agent",
//!     "http://localhost:4317",
//! )?;
//! # Ok(()) }
//! ```

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::prelude::*;

/// Boxed error so this surface doesn't churn with the OTLP crates' error types.
type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Flushes and shuts the OTLP pipeline down when dropped. Hold it for the life of
/// the program (bind it in `main`); dropping it early stops export.
pub struct OtelGuard {
    provider: SdkTracerProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        // Best-effort flush; nothing actionable if the collector is already gone.
        let _ = self.provider.shutdown();
    }
}

/// Build an OTLP-backed tracer provider exporting to `endpoint` (gRPC, e.g.
/// `http://localhost:4317`), tagging spans with `service.name = service_name`.
/// Returns the OpenTelemetry tracer plus a guard that flushes on drop. Use this
/// when you compose your own subscriber; otherwise reach for
/// [`init_tracing_with_otlp`].
pub fn otlp_tracer(
    service_name: impl Into<String>,
    endpoint: impl Into<String>,
) -> Result<(opentelemetry_sdk::trace::Tracer, OtelGuard), BoxError> {
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.into())
        .build()?;
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            Resource::builder()
                .with_service_name(service_name.into())
                .build(),
        )
        .build();
    let tracer = provider.tracer("harness-rs-loop");
    Ok((tracer, OtelGuard { provider }))
}

/// One-call setup: install a global `tracing` subscriber that both prints to
/// stderr (`fmt`, filtered by `RUST_LOG`) and exports every span over OTLP.
/// Returns a guard that flushes the pipeline on drop — bind it in `main` for the
/// program's lifetime.
///
/// For custom subscriber composition, use [`otlp_tracer`] and add your own
/// [`tracing_opentelemetry::OpenTelemetryLayer`].
pub fn init_tracing_with_otlp(
    service_name: impl Into<String>,
    endpoint: impl Into<String>,
) -> Result<OtelGuard, BoxError> {
    let (tracer, guard) = otlp_tracer(service_name, endpoint)?;
    let otel_layer = tracing_opentelemetry::OpenTelemetryLayer::new(tracer);
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer())
        .with(otel_layer)
        .try_init()?;
    Ok(guard)
}
