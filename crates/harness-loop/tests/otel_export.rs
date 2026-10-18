//! What actually reaches an OTLP backend. `tests/telemetry.rs` checks the
//! `tracing` spans; this checks them after `tracing-opentelemetry` has turned
//! them into OpenTelemetry spans — names, parent links, status — which is what
//! Jaeger / Tempo / Langfuse draw the waterfall from.
#![cfg(feature = "otel")]

use harness_context::default_world;
use harness_core::Task;
use harness_loop::AgentLoop;
use harness_models::{MockModel, MockResponse};
use harness_tools::fs::ReadFile;
use opentelemetry::trace::{Status, TracerProvider as _};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
use serde_json::json;
use std::sync::Arc;
use tracing_subscriber::prelude::*;

#[tokio::test]
async fn the_trace_is_a_waterfall_of_model_and_tool_calls() {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    // The layer the OTLP setup installs, filter included — so this also checks
    // that the trace is exported with no `RUST_LOG` in sight.
    let subscriber = tracing_subscriber::registry()
        .with(harness_loop::otel::telemetry_layer(provider.tracer("test")));

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        // Somebody else's span: the trace is the agent's, not the whole process's.
        let _noise = tracing::info_span!(target: "some_app", "noise").entered();
        drop(_noise);
        let ws = std::env::temp_dir().join(format!("otel-export-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "a").unwrap();
        let mut world = default_world(&ws);
        let model = MockModel::new()
            .script(MockResponse::tool_calls(vec![
                ("read_file".into(), json!({"path": "a.txt"})),
                ("read_file".into(), json!({"path": "missing.txt"})),
            ]))
            .script(MockResponse::text("done"));
        AgentLoop::new(model)
            .with_tool(Arc::new(ReadFile))
            .run(
                Task {
                    description: "read".into(),
                    source: None,
                    deadline: None,
                },
                &mut world,
            )
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&ws);
    }
    provider.force_flush().unwrap();

    let spans = exporter.get_finished_spans().unwrap();
    let names: Vec<&str> = spans.iter().map(|s| s.name.as_ref()).collect();
    assert!(!names.contains(&"noise"), "{names:?}");
    let run = spans
        .iter()
        .find(|s| s.name == "agent_run")
        .unwrap_or_else(|| panic!("no run span in {names:?}"));
    let trace = run.span_context.trace_id();

    // One trace: everything shares the run's trace id and hangs off the run.
    for s in spans.iter().filter(|s| s.name != "agent_run") {
        assert_eq!(
            s.span_context.trace_id(),
            trace,
            "{} in another trace",
            s.name
        );
        assert_eq!(
            s.parent_span_id,
            run.span_context.span_id(),
            "{} not under the run",
            s.name
        );
    }

    // `otel.name` renames the span, so backends group calls per model / tool.
    assert_eq!(
        names.iter().filter(|n| n.starts_with("chat ")).count(),
        2,
        "{names:?}"
    );
    let tools: Vec<_> = spans
        .iter()
        .filter(|s| s.name == "execute_tool read_file")
        .collect();
    assert_eq!(tools.len(), 2, "{names:?}");
    assert_eq!(
        tools
            .iter()
            .filter(|s| matches!(s.status, Status::Error { .. }))
            .count(),
        1,
        "the missing file is the one red bar"
    );
}
