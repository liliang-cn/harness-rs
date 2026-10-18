//! `TelemetryHook` emits structured `tracing` events for the run. We capture
//! them with a buffer-backed subscriber and assert the shape a downstream
//! OTLP/JSON exporter would receive.

use harness_context::default_world;
use harness_core::Task;
use harness_loop::{AgentLoop, TelemetryHook};
use harness_models::{MockModel, MockResponse};
use harness_tools::fs::ReadFile;
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

/// Time to first token is the latency a person feels; total duration is what
/// the machine spent. On a streaming provider they are very different numbers,
/// and only one of them was being reported.
#[tokio::test]
async fn streaming_reports_time_to_first_token() {
    use futures::StreamExt;
    use harness_core::{
        Context, Model, ModelDelta, ModelError, ModelInfo, ModelOutput, StopReason,
    };
    use std::time::Duration;

    /// Thinks for 120ms, then streams three fragments. TTFT must reflect the
    /// think time, not the whole answer.
    struct SlowStart;
    #[async_trait::async_trait]
    impl Model for SlowStart {
        async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
            Ok(ModelOutput {
                text: Some("hello world".into()),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            })
        }
        async fn stream(
            &self,
            _ctx: &Context,
        ) -> Result<futures::stream::BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError>
        {
            let s = futures::stream::iter(vec![
                Ok(ModelDelta::Text("hel".into())),
                Ok(ModelDelta::Text("lo ".into())),
                Ok(ModelDelta::Text("world".into())),
                Ok(ModelDelta::Stop(StopReason::EndTurn)),
            ])
            .then(|d| async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                d
            });
            Ok(s.boxed())
        }
        fn info(&self) -> ModelInfo {
            ModelInfo {
                handle: "slow".into(),
                provider: "test".into(),
                model: "slow".into(),
                context_window: 8192,
                input_cost_usd_per_million_tokens: None,
                output_cost_usd_per_million_tokens: None,
                supports_tool_use: false,
                supports_streaming: true,
                supports_web_grounding: false,
            }
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
        let ws = std::env::temp_dir().join(format!("telem-ttft-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let mut world = default_world(&ws);

        AgentLoop::new(SlowStart)
            .with_streaming(true)
            .with_hook(Arc::new(TelemetryHook::new()))
            .run(task("say hello"), &mut world)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&ws);
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    };

    assert!(
        output.contains("model.first_token"),
        "a streamed call must report its first fragment:\n{output}"
    );
    assert!(
        output.contains("first_token_ms="),
        "the run summary must carry it:\n{output}"
    );
    // The first fragment lands well before the last: TTFT is not the duration.
    let ttft: u64 = output
        .split("first_token_ms=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("first_token_ms in run.end");
    let total: u64 = output
        .rsplit("duration_ms=")
        .next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .expect("duration_ms in run.end");
    assert!(
        ttft < total,
        "TTFT ({ttft}ms) must be shorter than the whole run ({total}ms)"
    );
}

/// What a trace backend sees: span names, who their parent is, and the fields
/// recorded on them. A `fmt` writer flattens all of this into text, which is
/// why the waterfall bug below went unnoticed.
#[derive(Debug, Clone)]
struct SeenSpan {
    name: String,
    parent: Option<String>,
    fields: std::collections::HashMap<String, String>,
    closed: bool,
}

#[derive(Clone, Default)]
struct SpanCapture(Arc<Mutex<std::collections::HashMap<u64, SeenSpan>>>);

struct FieldVisitor<'a>(&'a mut std::collections::HashMap<String, String>);
impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            format!("{value:?}").trim_matches('"').to_string(),
        );
    }
}

impl<S> tracing_subscriber::Layer<S> for SpanCapture
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let parent = ctx
            .span(id)
            .and_then(|s| s.parent())
            .map(|p| p.name().to_string());
        let mut fields = std::collections::HashMap::new();
        attrs.record(&mut FieldVisitor(&mut fields));
        self.0.lock().unwrap().insert(
            id.into_u64(),
            SeenSpan {
                name: attrs.metadata().name().to_string(),
                parent,
                fields,
                closed: false,
            },
        );
    }
    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(s) = self.0.lock().unwrap().get_mut(&id.into_u64()) {
            values.record(&mut FieldVisitor(&mut s.fields));
        }
    }
    fn on_close(&self, id: tracing::span::Id, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if let Some(s) = self.0.lock().unwrap().get_mut(&id.into_u64()) {
            s.closed = true;
        }
    }
}

/// Model and tool calls must be child spans of the run, one per call, so a
/// trace backend draws a waterfall. As events on a single run span they were
/// invisible as durations, and two parallel reads looked like zero-length dots.
#[tokio::test]
async fn model_and_tool_calls_are_child_spans_of_the_run() {
    use tracing_subscriber::prelude::*;
    let capture = SpanCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let ws = std::env::temp_dir().join(format!("telem-spans-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "a").unwrap();
        std::fs::write(ws.join("b.txt"), "b").unwrap();
        let mut world = default_world(&ws);

        // Two read-only calls in one turn: the loop dispatches them in parallel.
        let model = MockModel::new()
            .script(
                MockResponse::tool_calls(vec![
                    ("read_file".into(), json!({"path": "a.txt"})),
                    ("read_file".into(), json!({"path": "b.txt"})),
                ])
                .with_usage(100, 20),
            )
            .script(MockResponse::text("done").with_usage(50, 10));

        // No TelemetryHook: the spans come from the loop itself, so a host
        // that only installs an OTLP layer still gets the waterfall.
        AgentLoop::new(model)
            .with_tool(Arc::new(ReadFile))
            .run(task("read two files"), &mut world)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&ws);
    }

    let spans: Vec<SeenSpan> = capture.0.lock().unwrap().values().cloned().collect();
    let runs: Vec<_> = spans.iter().filter(|s| s.name == "agent_run").collect();
    assert_eq!(runs.len(), 1, "one run span:\n{spans:#?}");
    assert_eq!(runs[0].parent, None, "the run is the root:\n{spans:#?}");

    let chats: Vec<_> = spans.iter().filter(|s| s.name == "chat").collect();
    assert_eq!(chats.len(), 2, "one span per model call:\n{spans:#?}");
    for c in &chats {
        assert_eq!(c.parent.as_deref(), Some("agent_run"));
        assert!(c.fields.contains_key("gen_ai.usage.input_tokens"), "{c:#?}");
    }

    let tools: Vec<_> = spans.iter().filter(|s| s.name == "execute_tool").collect();
    assert_eq!(tools.len(), 2, "one span per tool call:\n{spans:#?}");
    let mut ids: Vec<_> = tools
        .iter()
        .map(|t| {
            t.fields
                .get("gen_ai.tool.call.id")
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    ids.dedup();
    assert_eq!(ids.len(), 2, "each parallel call has its own span: {ids:?}");
    for t in &tools {
        assert_eq!(t.parent.as_deref(), Some("agent_run"));
        assert_eq!(
            t.fields.get("gen_ai.tool.name").map(String::as_str),
            Some("read_file")
        );
        assert_eq!(
            t.fields.get("otel.name").map(String::as_str),
            Some("execute_tool read_file")
        );
        assert_eq!(
            t.fields.get("ok").map(String::as_str),
            Some("true"),
            "{t:#?}"
        );
    }
    assert!(
        spans.iter().all(|s| s.closed),
        "every span is closed:\n{spans:#?}"
    );
}

/// A failed tool call marks its span as an error, which is what turns it red
/// in Jaeger / Tempo / Langfuse.
#[tokio::test]
async fn a_failed_tool_call_marks_its_span_as_error() {
    use tracing_subscriber::prelude::*;
    let capture = SpanCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        let ws = std::env::temp_dir().join(format!("telem-err-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let mut world = default_world(&ws);
        let model = MockModel::new()
            .script(MockResponse::tool_call(
                "read_file",
                json!({"path": "missing.txt"}),
            ))
            .script(MockResponse::text("done"));
        AgentLoop::new(model)
            .with_tool(Arc::new(ReadFile))
            .run(task("read a missing file"), &mut world)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&ws);
    }
    let spans: Vec<SeenSpan> = capture.0.lock().unwrap().values().cloned().collect();
    let tool = spans
        .iter()
        .find(|s| s.name == "execute_tool")
        .expect("tool span");
    assert_eq!(
        tool.fields.get("ok").map(String::as_str),
        Some("false"),
        "{tool:#?}"
    );
    assert_eq!(
        tool.fields.get("otel.status_code").map(String::as_str),
        Some("error"),
        "{tool:#?}"
    );
}
