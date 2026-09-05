//! Stopping a run from outside. Every test here drives a real `AgentLoop`
//! against `MockModel` or a hand-rolled slow model; nothing is mocked at the
//! loop level, because the behaviour under test *is* the loop's.

use harness_context::default_world;
use harness_core::{Task, World};
use harness_loop::{AgentLoop, Outcome};
use harness_models::{MockModel, MockResponse};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

// ------------------------------------------------------------------
// fixtures (copied from tests/agent_loop.rs — test crates cannot share)
// ------------------------------------------------------------------

fn tmp_workspace() -> (TestDir, World) {
    let td = TestDir::new();
    let world = default_world(td.0.clone());
    (td, world)
}

struct TestDir(PathBuf);
static TD_SEQ: AtomicU64 = AtomicU64::new(0);
impl TestDir {
    fn new() -> Self {
        let pid = std::process::id();
        let n = TD_SEQ.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("harness-cancel-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        TestDir(p)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn task(desc: &str) -> Task {
    Task {
        description: desc.into(),
        source: None,
        deadline: None,
    }
}

// ------------------------------------------------------------------
// 1. A token cancelled before the run starts costs nothing
// ------------------------------------------------------------------

#[tokio::test]
async fn a_token_cancelled_before_start_never_calls_the_model() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new().script(MockResponse::text("never seen"));
    let token = CancellationToken::new();
    token.cancel();

    let agent = AgentLoop::new(model).with_cancellation(token);
    let outcome = agent
        .run_with_max_iters(task("anything"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Cancelled {
            iters,
            tools_called,
            ..
        } => {
            assert_eq!(iters, 0, "no iteration should have started");
            assert_eq!(tools_called, 0);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert_eq!(
        agent.model.call_count(),
        0,
        "a cancelled run must not spend a model call"
    );
}

// ------------------------------------------------------------------
// 2. A token nobody cancels changes nothing
// ------------------------------------------------------------------

#[tokio::test]
async fn an_uncancelled_token_changes_nothing() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new().script(MockResponse::text("hello"));

    let outcome = AgentLoop::new(model)
        .with_cancellation(CancellationToken::new())
        .run_with_max_iters(task("say hi"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Done { text, iters, .. } => {
            assert_eq!(text.as_deref(), Some("hello"));
            assert_eq!(iters, 1);
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// 3. A cancel during a tool call abandons the tool
// ------------------------------------------------------------------

use async_trait::async_trait;
use harness_core::{ToolError, ToolResult, ToolRisk, ToolSchema};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// A tool that takes ten seconds and announces the moment it is entered.
///
/// The announcement is what makes the cancel tests deterministic: they cancel
/// *on* entry rather than after a sleep, so the token fires while the tool is
/// provably mid-flight, on any machine, at any load. If cancellation waited
/// for the tool, each test would take ten seconds and fail its elapsed bound.
struct SlowTool {
    schema: ToolSchema,
    risk: ToolRisk,
    entered: Arc<Notify>,
}
impl SlowTool {
    fn new(name: &str, risk: ToolRisk, entered: Arc<Notify>) -> Self {
        Self {
            schema: ToolSchema {
                name: name.into(),
                description: "sleeps for ten seconds".into(),
                input: json!({"type": "object", "properties": {}}),
            },
            risk,
            entered,
        }
    }
}
#[async_trait]
impl harness_core::Tool for SlowTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        self.risk
    }
    async fn invoke(
        &self,
        _args: serde_json::Value,
        _world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        // `notify_one` stores a permit if nobody is waiting yet, so the test's
        // `notified()` returns whether it registered before or after this line.
        self.entered.notify_one();
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok(ToolResult {
            ok: true,
            content: json!("slept"),
            trace: None,
        })
    }
}

/// Cancel the token the moment a `SlowTool` reports it has been entered.
fn cancel_on_entry(entered: Arc<Notify>, token: CancellationToken) {
    tokio::spawn(async move {
        entered.notified().await;
        token.cancel();
    });
}

#[tokio::test]
async fn cancelling_during_a_slow_tool_returns_promptly() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    let entered = Arc::new(Notify::new());
    let token = CancellationToken::new();
    cancel_on_entry(entered.clone(), token.clone());

    let started = Instant::now();
    let agent = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new(
            "slow",
            ToolRisk::Idempotent,
            entered,
        )))
        .with_cancellation(token);
    let outcome = agent
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    match outcome {
        Outcome::Cancelled {
            iters,
            tools_called,
            ..
        } => {
            assert_eq!(iters, 1, "the first iteration had started");
            assert_eq!(tools_called, 1, "the tool was dispatched before the cancel");
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel must drop the in-flight tool, not await it: took {elapsed:?}"
    );
}

// ------------------------------------------------------------------
// 4. No model call after a cancel — not even the final synthesis
// ------------------------------------------------------------------

#[tokio::test]
async fn cancellation_does_not_force_a_final_synthesis() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text(
            "a synthesis that must never be requested",
        ));
    let entered = Arc::new(Notify::new());
    let token = CancellationToken::new();
    cancel_on_entry(entered.clone(), token.clone());

    let agent = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new(
            "slow",
            ToolRisk::Idempotent,
            entered,
        )))
        .with_cancellation(token);
    let _ = agent
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    // Exactly one call: the turn that asked for the tool. `BudgetExhausted`
    // and the monotony exit both call the model once more to write a final
    // answer; a cancel is the user saying stop, and stop means stop.
    assert_eq!(agent.model.call_count(), 1);
}

// ------------------------------------------------------------------
// 5. The partial text survives a cancel
// ------------------------------------------------------------------

/// `Cancelled` carries `last_text` because the user who pressed Esc still
/// wants what was done. No other test destructures it — they all write
/// `..` — so nothing else would notice if the field were always `None`.
#[tokio::test]
async fn partial_text_survives_a_cancel() {
    let (_td, mut world) = tmp_workspace();
    // Text and a tool call in the same turn: the loop records the text,
    // then dispatches the (slow) tool, during which the cancel lands.
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})).with_text("so far so good"))
        .script(MockResponse::text("unreachable"));
    let entered = Arc::new(Notify::new());
    let token = CancellationToken::new();
    cancel_on_entry(entered.clone(), token.clone());

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new(
            "slow",
            ToolRisk::Idempotent,
            entered,
        )))
        .with_cancellation(token)
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Cancelled { last_text, .. } => {
            assert_eq!(last_text.as_deref(), Some("so far so good"));
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// 6. A cancel during the parallel read-only prefetch
// ------------------------------------------------------------------

/// Two leading read-only calls take the concurrent prefetch path, where the
/// `join_all` is raced against the token. On a cancel the prefetch yields
/// nothing, the sequential loop re-dispatches the first call, `dispatch_bounded`
/// returns the cancelled result at once, and the loop exits — a chain no other
/// test walks. A future change that returned *partial* prefetch results would
/// break it silently.
#[tokio::test]
async fn cancelling_during_the_parallel_prefetch_returns_promptly() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_calls(vec![
            ("slow_a".to_string(), json!({})),
            ("slow_b".to_string(), json!({})),
        ]))
        .script(MockResponse::text("unreachable"));
    // One signal shared by both tools: whichever enters first cancels the run.
    let entered = Arc::new(Notify::new());
    let token = CancellationToken::new();
    cancel_on_entry(entered.clone(), token.clone());

    let started = Instant::now();
    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new(
            "slow_a",
            ToolRisk::ReadOnly,
            entered.clone(),
        )))
        .with_tool(Arc::new(SlowTool::new(
            "slow_b",
            ToolRisk::ReadOnly,
            entered,
        )))
        .with_cancellation(token)
        .run_with_max_iters(task("call both slow tools"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    match outcome {
        Outcome::Cancelled {
            iters,
            tools_called,
            ..
        } => {
            assert_eq!(iters, 1);
            // The sequential loop dispatched exactly one call (the cancelled
            // re-dispatch of the first) before exiting.
            assert_eq!(tools_called, 1);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel must drop the prefetch, not await it: took {elapsed:?}"
    );
}
