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

// ------------------------------------------------------------------
// 7. A cancel mid-stream stops generation
// ------------------------------------------------------------------

use futures::stream::BoxStream;
use harness_core::{
    Context, Event, Hook, HookOutcome, Model, ModelDelta, ModelError, ModelInfo, ModelOutput,
};
use std::sync::atomic::AtomicU32;

/// Streams one character every 100ms for five seconds, and announces the first
/// chunk through `entered` so a test can cancel the instant the stream is
/// provably mid-flight. Delegates everything that is not streaming to a
/// `MockModel` so `info()` needs no hand-built `ModelInfo`.
struct SlowStreamModel {
    inner: MockModel,
    entered: Arc<Notify>,
}
#[async_trait]
impl Model for SlowStreamModel {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.inner.complete(ctx).await
    }
    async fn stream(
        &self,
        _ctx: &Context,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let entered = self.entered.clone();
        let s = futures::stream::unfold(0u32, move |i| {
            let entered = entered.clone();
            async move {
                if i >= 50 {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                // Announce as the first chunk goes out. The loop consumes it
                // and fires ModelTokenDelta within the same poll, so the
                // cancel that this triggers lands on the *next* await.
                if i == 0 {
                    entered.notify_one();
                }
                Some((Ok(ModelDelta::Text("x".into())), i + 1))
            }
        });
        Ok(Box::pin(s))
    }
    fn info(&self) -> ModelInfo {
        let mut info = self.inner.info();
        info.supports_streaming = true;
        info
    }
}

/// A non-streaming model whose `complete` takes ten seconds and announces
/// its entry. This is the loop's **default** path — `streaming` is off unless
/// a caller opts in — so a cancel that only worked mid-stream would miss most
/// real runs.
struct SlowCompleteModel {
    inner: MockModel,
    entered: Arc<Notify>,
}
#[async_trait]
impl Model for SlowCompleteModel {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.entered.notify_one();
        tokio::time::sleep(Duration::from_secs(10)).await;
        self.inner.complete(ctx).await
    }
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }
}

/// Counts `ModelTokenDelta` events, so the test can prove the stream was
/// running when it was cut, not finished before the cancel landed.
struct DeltaCounter(Arc<AtomicU32>);
impl Hook for DeltaCounter {
    fn name(&self) -> &str {
        "delta-counter"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::ModelTokenDelta { .. })
    }
    fn fire(&self, _ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        HookOutcome::Allow
    }
}

#[tokio::test]
async fn cancelling_mid_stream_stops_generation() {
    let (_td, mut world) = tmp_workspace();
    let entered = Arc::new(Notify::new());
    let model = SlowStreamModel {
        inner: MockModel::new().script(MockResponse::text("unused")),
        entered: entered.clone(),
    };
    let deltas = Arc::new(AtomicU32::new(0));
    let token = CancellationToken::new();
    cancel_on_entry(entered, token.clone());

    let started = Instant::now();
    let outcome = AgentLoop::new(model)
        .with_streaming(true)
        .with_hook(Arc::new(DeltaCounter(deltas.clone())))
        .with_cancellation(token)
        .run_with_max_iters(task("stream something long"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, Outcome::Cancelled { iters: 1, .. }),
        "got {outcome:?}"
    );
    let seen = deltas.load(Ordering::SeqCst);
    assert!(
        seen >= 1,
        "the stream had started delivering before the cancel"
    );
    assert!(
        seen < 50,
        "the stream was cut short, not drained: saw {seen} of 50"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel must drop the stream: took {elapsed:?}"
    );
}

/// The default, non-streaming path: `complete()` is one HTTP round trip that
/// returns nothing until the whole answer exists. A cancel has to drop that
/// request, not wait for it.
#[tokio::test]
async fn cancelling_mid_completion_drops_the_request() {
    let (_td, mut world) = tmp_workspace();
    let entered = Arc::new(Notify::new());
    let model = SlowCompleteModel {
        inner: MockModel::new().script(MockResponse::text("never returned")),
        entered: entered.clone(),
    };
    let token = CancellationToken::new();
    cancel_on_entry(entered, token.clone());

    let started = Instant::now();
    let outcome = AgentLoop::new(model)
        .with_cancellation(token)
        .run_with_max_iters(task("think for a long time"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, Outcome::Cancelled { iters: 1, .. }),
        "got {outcome:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel must drop the in-flight completion: took {elapsed:?}"
    );
}

/// The chunks a stream delivered before the cancel were already shown to the
/// user through `ModelTokenDelta`; the outcome has to carry them too, or the
/// conversation the model sees next turn disagrees with the one the person
/// just watched. Task 3 established this for tools; a stream is no different.
#[tokio::test]
async fn streamed_partial_text_survives_a_cancel() {
    let (_td, mut world) = tmp_workspace();
    let entered = Arc::new(Notify::new());
    let model = SlowStreamModel {
        inner: MockModel::new().script(MockResponse::text("unused")),
        entered: entered.clone(),
    };
    let token = CancellationToken::new();
    cancel_on_entry(entered, token.clone());

    let outcome = AgentLoop::new(model)
        .with_streaming(true)
        .with_cancellation(token)
        .run_with_max_iters(task("stream something long"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Cancelled { last_text, .. } => {
            let text = last_text.expect("the streamed chunks must reach the outcome");
            assert!(!text.is_empty());
            assert!(
                text.chars().all(|c| c == 'x'),
                "only the model's own chunks, nothing else: {text:?}"
            );
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

// ------------------------------------------------------------------
// 6. Cancelled fires once, and SessionEnd follows it
// ------------------------------------------------------------------

use std::sync::Mutex;

struct EventLog(Arc<Mutex<Vec<&'static str>>>);
impl Hook for EventLog {
    fn name(&self) -> &str {
        "event-log"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(
            ev,
            Event::Cancelled | Event::SessionEnd | Event::Stop | Event::Error { .. }
        )
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        self.0.lock().unwrap().push(ev.name());
        HookOutcome::Allow
    }
}

#[tokio::test]
async fn cancelled_fires_once_then_session_end() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    let log = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(Notify::new());
    let token = CancellationToken::new();
    cancel_on_entry(entered.clone(), token.clone());

    let _ = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new(
            "slow",
            ToolRisk::Idempotent,
            entered,
        )))
        .with_hook(Arc::new(EventLog(log.clone())))
        .with_cancellation(token)
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    let seen = log.lock().unwrap().clone();
    assert_eq!(seen, vec!["Cancelled", "SessionEnd"], "got {seen:?}");
}

// ------------------------------------------------------------------
// 7. A cancel reaches the broadcast feed
// ------------------------------------------------------------------

use harness_loop::hooks::broadcast::BroadcastHook;

/// The SSE feed is how a UI watches a run. Before this, `project()` had no
/// arm for `Cancelled`, so a client saw the stream stop with no reason —
/// the exact failure the event's own doc comment warns against.
#[tokio::test]
async fn a_cancel_reaches_the_broadcast_feed() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    // Subscribe before the hook is handed to the loop: `subscribe` takes
    // `&self`, and the receiver outlives the hook independently.
    let hook = BroadcastHook::new(64);
    let mut rx = hook.subscribe();
    let entered = Arc::new(Notify::new());
    let token = CancellationToken::new();
    cancel_on_entry(entered.clone(), token.clone());

    let _ = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new(
            "slow",
            ToolRisk::Idempotent,
            entered,
        )))
        .with_hook(Arc::new(hook))
        .with_cancellation(token)
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    // Drain what the feed carried. `try_recv` returns `Err(Empty)` once the
    // buffer is exhausted, which ends the loop.
    let mut names = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        names.push(ev.event);
    }
    let cancelled = names.iter().position(|n| *n == "Cancelled");
    let ended = names.iter().position(|n| *n == "SessionEnd");
    assert!(
        cancelled.is_some(),
        "Cancelled never reached the feed: {names:?}"
    );
    assert!(
        ended.is_some(),
        "SessionEnd never reached the feed: {names:?}"
    );
    assert!(
        cancelled < ended,
        "Cancelled must precede SessionEnd on the feed: {names:?}"
    );
}

// ------------------------------------------------------------------
// 8. A cancel is recorded on the run trace
// ------------------------------------------------------------------

use std::io::Write;
use tracing_subscriber::fmt::MakeWriter;

/// A `tracing` writer that appends everything into a shared buffer.
/// (Copied from tests/telemetry.rs — test crates cannot share fixtures.)
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

/// `SessionEnd` writes `run.end` however the run ended, so without its own
/// line a cancelled run's trace reads exactly like a finished one.
#[tokio::test]
async fn a_cancel_is_recorded_on_the_run_trace() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufWriter(buf.clone()))
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .finish();

    let output = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let (_td, mut world) = tmp_workspace();
        let model = MockModel::new()
            .script(MockResponse::tool_call("slow", json!({})))
            .script(MockResponse::text("unreachable"));
        let entered = Arc::new(Notify::new());
        let token = CancellationToken::new();
        cancel_on_entry(entered.clone(), token.clone());

        let _ = AgentLoop::new(model)
            .with_tool(Arc::new(SlowTool::new(
                "slow",
                ToolRisk::Idempotent,
                entered,
            )))
            .with_hook(Arc::new(harness_loop::TelemetryHook::new()))
            .with_cancellation(token)
            .run_with_max_iters(task("call the slow tool"), &mut world, 5)
            .await
            .unwrap();

        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    };

    assert!(
        output.contains("run.cancelled"),
        "missing run.cancelled:\n{output}"
    );
    assert!(
        output.contains("run.end"),
        "run.end must still close the trace:\n{output}"
    );
    let cancelled = output.find("run.cancelled").unwrap();
    let ended = output.find("run.end").unwrap();
    assert!(
        cancelled < ended,
        "run.cancelled must be recorded before run.end:\n{output}"
    );
    // Scoped to the run.end line: model.complete renders a `tool_calls` field
    // of its own (calls requested), and a whole-output search would match that.
    let end = output
        .lines()
        .find(|l| l.contains("run.end"))
        .expect("run.end line");
    assert!(
        end.contains("tool_calls=1"),
        "run.end must count the cancelled dispatch, as Outcome::Cancelled does:\n{output}"
    );
}

// ------------------------------------------------------------------
// 9. A denied call from an earlier run is not billed to a later cancel
// ------------------------------------------------------------------

/// Denies the first tool call it sees and allows everything after it.
struct DenyOnce(AtomicU32);
impl Hook for DenyOnce {
    fn name(&self) -> &str {
        "deny-once"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. })
    }
    fn fire(&self, _ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            HookOutcome::Deny {
                reason: "not this one".into(),
            }
        } else {
            HookOutcome::Allow
        }
    }
}

/// A denied tool call fires PreToolUse and never PostToolUse, so its entry
/// stays in the telemetry hook's map. One `AgentLoop` serves many runs, and
/// so does its hook: a cancel in a *later* run must not sweep that stale
/// entry into its own `tool_calls`, or `run.end` disagrees with the outcome.
#[tokio::test]
async fn a_denied_call_from_an_earlier_run_is_not_billed_to_a_cancel() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufWriter(buf.clone()))
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .finish();

    let output = {
        let _guard = tracing::subscriber::set_default(subscriber);
        let (_td, mut world) = tmp_workspace();
        // Different args on purpose: MockModel derives a call's id from its
        // name and args, and the telemetry map is keyed by that id. Identical
        // calls would share a key and the second would silently overwrite the
        // leaked first — hiding exactly the bug this test is for.
        let model = MockModel::new()
            .script(MockResponse::tool_call("slow", json!({"n": 1})))
            .script(MockResponse::text("done"))
            .script(MockResponse::tool_call("slow", json!({"n": 2})))
            .script(MockResponse::text("unreachable"));
        let entered = Arc::new(Notify::new());
        // Telemetry registered *before* the denier, so it sees the PreToolUse
        // that the denier then stops.
        let mut agent = AgentLoop::new(model)
            .with_hook(Arc::new(harness_loop::TelemetryHook::new()))
            .with_hook(Arc::new(DenyOnce(AtomicU32::new(0))))
            .with_tool(Arc::new(SlowTool::new(
                "slow",
                ToolRisk::Idempotent,
                entered.clone(),
            )));

        // Run 1: the call is denied, the model then answers, the run is Done.
        let first = agent
            .run_with_max_iters(task("first"), &mut world, 5)
            .await
            .unwrap();
        assert!(matches!(first, Outcome::Done { .. }), "got {first:?}");

        // Run 2 on the same loop and hook: the call is allowed, entered, and
        // cancelled. A fresh token, since the field is one-way.
        let token = CancellationToken::new();
        agent.cancel = token.clone();
        cancel_on_entry(entered, token);
        let second = agent
            .run_with_max_iters(task("second"), &mut world, 5)
            .await
            .unwrap();
        assert!(
            matches!(
                second,
                Outcome::Cancelled {
                    tools_called: 1,
                    ..
                }
            ),
            "got {second:?}"
        );

        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    };

    let ends: Vec<&str> = output.lines().filter(|l| l.contains("run.end")).collect();
    assert_eq!(ends.len(), 2, "one run.end per run:\n{output}");
    assert!(
        ends[0].contains("tool_calls=0"),
        "a denied call is not a dispatch:\n{}",
        ends[0]
    );
    assert!(
        ends[1].contains("tool_calls=1"),
        "the cancel counts its own dispatch and nothing leaked from run 1:\n{}",
        ends[1]
    );
}
