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
            .script(
                MockResponse::tool_call("read_file", json!({"path": "x.txt"})).with_usage(100, 20),
            )
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
    // OTel GenAI semantic conventions ride alongside the legacy aliases, so an
    // OTLP backend recognizes usage/finish-reason/tool without any mapping.
    assert!(
        output.contains("gen_ai.usage.input_tokens=100"),
        "missing gen_ai usage convention:\n{output}"
    );
    assert!(
        output.contains("gen_ai.tool.name=read_file"),
        "missing gen_ai.tool.name convention:\n{output}"
    );
    assert!(
        output.contains("gen_ai.operation.name=\"invoke_agent\""),
        "missing invoke_agent on run span:\n{output}"
    );
    assert!(output.contains("tool.call"), "missing tool.call:\n{output}");
    assert!(output.contains("read_file"), "missing tool name:\n{output}");
    assert!(output.contains("run.end"), "missing run.end:\n{output}");

    // `run.end` closes with the whole bill. Per-turn events say what happened;
    // the question after a run is what it cost, and adding the turns up by hand
    // is the thing every caller was doing. Scripted usage: 100/20 then 50/10.
    assert!(
        output.contains("total_tokens=180"),
        "run.end must total the turns (100+20+50+10):\n{output}"
    );
    assert!(
        output.contains("model_calls=2"),
        "run.end must count model calls:\n{output}"
    );
    // The scripted read targets a file that does not exist, so the one tool call
    // fails — and the summary has to say so. A failure count that reads 0 while
    // the run limped is worse than no summary.
    assert!(
        output.contains("tool_calls=1") && output.contains("tool_failures=1"),
        "run.end must count tool calls and failures:\n{output}"
    );
}

/// Compaction is the component whose entire job is to spend fewer tokens, and
/// until now its telemetry said only which stage ran — "it happened", never
/// "it worked". These assert the saving is on the event and in the run summary.
#[tokio::test]
async fn compaction_reports_what_it_saved() {
    use harness_core::{Budget, CompactError, CompactionStage, Compactor, Context};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Starts at 900/1000 and drops 200 tokens per stage, so the loop runs two
    /// stages to reach its 0.55 target: 900 → 700 → 500.
    struct StubCompactor {
        used: AtomicU32,
    }
    #[async_trait::async_trait]
    impl Compactor for StubCompactor {
        fn budget(&self, _ctx: &Context) -> Budget {
            Budget {
                used: self.used.load(Ordering::SeqCst),
                window: 1000,
            }
        }
        async fn compact(
            &self,
            _stage: CompactionStage,
            _ctx: &mut Context,
        ) -> Result<(), CompactError> {
            self.used.fetch_sub(200, Ordering::SeqCst);
            Ok(())
        }
    }

    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufWriter(buf.clone()))
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .finish();

    let output = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let ws = std::env::temp_dir().join(format!("telem-compact-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let mut world = default_world(&ws);

        AgentLoop::new(MockModel::new().script(MockResponse::text("done").with_usage(500, 10)))
            .with_compactor(Arc::new(StubCompactor {
                used: AtomicU32::new(900),
            }))
            .with_hook(Arc::new(TelemetryHook::new()))
            .run(task("go"), &mut world)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&ws);
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    };

    // Per-stage: the before/after pair, and the difference spelled out.
    assert!(
        output.contains("tokens_before=900") && output.contains("tokens_after=700"),
        "first stage must report its before/after:\n{output}"
    );
    assert!(
        output.contains("tokens_saved=200"),
        "each stage must report what it bought:\n{output}"
    );
    // And the run summary carries the total across stages: 900 → 500.
    assert!(
        output.contains("compactions=2") && output.contains("tokens_saved=400"),
        "run.end must total the compactions and their saving:\n{output}"
    );
}
