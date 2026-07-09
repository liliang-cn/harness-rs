//! `TelemetryHook` emits structured `tracing` events for the run. We capture
//! them with a buffer-backed subscriber and assert the shape a downstream
//! OTLP/JSON exporter would receive.

use harness_context::default_world;
use harness_core::Task;
use harness_loop::{AgentLoop, TelemetryHook};
use harness_models::{MockModel, MockResponse};
use harness_tools_fs::ReadFile;
use serde_json::json;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

/// A `tracing` writer that appends everything into a shared buffer.
#[derive(Clone)]
struct BufWriter(Arc<Mutex<Vec<u8>>>);
impl Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for BufWriter {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> BufWriter {
        self.clone()
    }
}

fn task(desc: &str) -> Task {
    Task {
        description: desc.into(),
        source: None,
        deadline: None,
    }
}

#[tokio::test]
async fn emits_structured_run_telemetry() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufWriter(buf.clone()))
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .finish();

    // Scope the default subscriber to this run.
    let output = {
        let _guard = tracing::subscriber::set_default(subscriber);

        let ws = std::env::temp_dir().join(format!("telem-test-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let mut world = default_world(&ws);

        let model = MockModel::new()
            .script(MockResponse::tool_call("read_file", json!({"path": "x.txt"})).with_usage(100, 20))
            .script(MockResponse::text("done").with_usage(50, 10));

        let outcome = AgentLoop::new(model)
            .with_tool(Arc::new(ReadFile))
            .with_hook(Arc::new(TelemetryHook::new()))
            .run(task("read a file"), &mut world)
            .await
            .unwrap();
        assert!(matches!(outcome, harness_loop::Outcome::Done { .. }));

        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    };

    // The lifecycle span + events a JSON/OTLP exporter would carry.
    assert!(output.contains("agent_run"), "missing run span:\n{output}");
    assert!(output.contains("run.start"), "missing run.start:\n{output}");
    assert!(
        output.contains("model.complete"),
        "missing model.complete:\n{output}"
    );
    assert!(
        output.contains("input_tokens=100"),
        "missing token field:\n{output}"
    );
    assert!(output.contains("tool.call"), "missing tool.call:\n{output}");
    assert!(
        output.contains("read_file"),
        "missing tool name:\n{output}"
    );
    assert!(output.contains("run.end"), "missing run.end:\n{output}");
}
