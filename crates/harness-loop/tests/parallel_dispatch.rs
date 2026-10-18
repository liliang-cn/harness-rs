//! Parallel read-only dispatch: when one model response emits several read-only
//! tool calls, the loop dispatches them concurrently (a mutating call would be a
//! serial barrier). We prove it by making the tool *slow* — 3 × 150ms run in
//! ~150ms wall-clock, not ~450ms.

use async_trait::async_trait;
use harness_core::{
    Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, Tool, ToolCall, ToolError,
    ToolResult, ToolRisk, ToolSchema, World,
};
use harness_loop::{AgentLoop, Outcome};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

fn mi() -> ModelInfo {
    ModelInfo {
        handle: "mock".into(),
        provider: "mock".into(),
        model: "mock".into(),
        context_window: 8192,
        input_cost_usd_per_million_tokens: None,
        output_cost_usd_per_million_tokens: None,
        supports_tool_use: true,
        supports_streaming: false,
        supports_web_grounding: false,
    }
}

/// A read-only tool that sleeps, so concurrency shows up as wall-clock savings.
struct SlowRead {
    schema: ToolSchema,
}
impl SlowRead {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "slow_read".into(),
                description: "sleeps then echoes".into(),
                input: json!({"type": "object"}),
            },
        }
    }
}
#[async_trait]
impl Tool for SlowRead {
    fn name(&self) -> &str {
        "slow_read"
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        args: serde_json::Value,
        _w: &mut World,
    ) -> Result<ToolResult, ToolError> {
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(ToolResult {
            ok: true,
            content: json!({ "echo": args["n"] }),
            trace: None,
        })
    }
}

/// One response with three read-only tool calls, then a terminal text turn.
struct ThreeReads {
    turn: AtomicU32,
}
#[async_trait]
impl Model for ThreeReads {
    async fn complete(&self, _c: &Context) -> Result<ModelOutput, ModelError> {
        let t = self.turn.fetch_add(1, Ordering::SeqCst);
        if t == 0 {
            Ok(ModelOutput {
                text: None,
                tool_calls: (0..3)
                    .map(|i| ToolCall {
                        id: format!("c{i}"),
                        name: "slow_read".into(),
                        args: json!({ "n": i }),
                    })
                    .collect(),
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            })
        } else {
            Ok(ModelOutput {
                text: Some("done".into()),
                ..Default::default()
            })
        }
    }
    fn info(&self) -> ModelInfo {
        mi()
    }
}

#[tokio::test]
async fn read_only_calls_dispatch_concurrently() {
    let mut world = harness_context::default_world(".");
    let loop_ = AgentLoop::new(ThreeReads {
        turn: AtomicU32::new(0),
    })
    .with_tool(Arc::new(SlowRead::new()));
    let task = harness_core::Task {
        description: "go".into(),
        source: None,
        deadline: None,
    };

    let start = Instant::now();
    let out = loop_.run_with_max_iters(task, &mut world, 4).await.unwrap();
    let elapsed = start.elapsed();

    // Sequential would be ~450ms (3 × 150). Concurrent is ~150ms. Assert we're
    // comfortably under the sequential figure.
    assert!(
        elapsed < Duration::from_millis(350),
        "read-only tools did not run concurrently — took {elapsed:?}"
    );
    match out {
        Outcome::Done { tools_called, .. } => assert_eq!(tools_called, 3),
        other => panic!("expected Done, got {other:?}"),
    }
}

/// Every `ToolTimed` the loop fires, as `(call_id, started_ms, finished_ms)`.
struct TimingWitness(std::sync::Mutex<Vec<(String, i64, i64)>>);
impl harness_core::Hook for TimingWitness {
    fn name(&self) -> &str {
        "timing-witness"
    }
    fn matches(&self, ev: &harness_core::Event<'_>) -> bool {
        matches!(ev, harness_core::Event::ToolTimed { .. })
    }
    fn fire(&self, ev: &harness_core::Event<'_>, _w: &mut World) -> harness_core::HookOutcome {
        if let harness_core::Event::ToolTimed {
            action,
            started_ms,
            finished_ms,
        } = ev
        {
            self.0
                .lock()
                .unwrap()
                .push((action.call_id.clone(), *started_ms, *finished_ms));
        }
        harness_core::HookOutcome::Allow
    }
}

/// Parallel calls are dispatched before their `PreToolUse` fires, so the gap
/// between `PreToolUse` and `PostToolUse` says nothing about how long they ran.
/// `ToolTimed` carries when each one really ran: about 150ms each, overlapping.
#[tokio::test]
async fn each_call_reports_when_it_really_ran() {
    let mut world = harness_context::default_world(".");
    let witness = Arc::new(TimingWitness(Default::default()));
    AgentLoop::new(ThreeReads {
        turn: AtomicU32::new(0),
    })
    .with_tool(Arc::new(SlowRead::new()))
    .with_hook(witness.clone())
    .run_with_max_iters(
        harness_core::Task {
            description: "go".into(),
            source: None,
            deadline: None,
        },
        &mut world,
        4,
    )
    .await
    .unwrap();

    let timed = witness.0.lock().unwrap().clone();
    assert_eq!(timed.len(), 3, "{timed:?}");
    for (id, start, end) in &timed {
        assert!(end - start >= 140, "{id} ran {}ms, not ~150", end - start);
    }
    let latest_start = timed.iter().map(|t| t.1).max().unwrap();
    let earliest_end = timed.iter().map(|t| t.2).min().unwrap();
    assert!(
        latest_start < earliest_end,
        "the three calls overlap in time: {timed:?}"
    );
}

/// `TelemetryHook`'s per-call duration and the run's `tool_ms` come from the
/// same timing, so three parallel 150ms reads cost ~450ms of tool time, not ~0.
#[tokio::test]
async fn telemetry_times_parallel_calls_by_when_they_ran() {
    use std::io::Write;
    #[derive(Clone)]
    struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = Buf(Default::default());
    let writer = buf.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    {
        let _g = tracing::subscriber::set_default(subscriber);
        let mut world = harness_context::default_world(".");
        AgentLoop::new(ThreeReads {
            turn: AtomicU32::new(0),
        })
        .with_tool(Arc::new(SlowRead::new()))
        .with_hook(Arc::new(harness_loop::TelemetryHook::new()))
        .run_with_max_iters(
            harness_core::Task {
                description: "go".into(),
                source: None,
                deadline: None,
            },
            &mut world,
            4,
        )
        .await
        .unwrap();
    }
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    let tool_ms: u64 = out
        .split("tool_ms=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no tool_ms in run.end:\n{out}"));
    assert!(tool_ms >= 420, "tool_ms={tool_ms}, expected ~450:\n{out}");
}
