//! Stuck-detector tests: a model that repeats the *same* tool call round after
//! round must be caught and terminated with `Outcome::Stuck`, not left to burn
//! the whole budget. `MockModel` ignores context, so scripting the identical
//! tool call N times simulates a model that also ignores the nudge — exactly
//! the loop we want to abort.

use harness_context::default_world;
use harness_core::Task;
use harness_loop::{AgentLoop, Outcome, StuckPolicy};
use harness_models::{MockModel, MockResponse};
use harness_tools_fs::ReadFile;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn tmp_workspace() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("stuck-test-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn task(desc: &str) -> Task {
    Task {
        description: desc.into(),
        source: None,
        deadline: None,
    }
}

/// The same tool call 20 times: the detector should abort at `abort_after`.
fn repeat_script(n: usize) -> Vec<MockResponse> {
    (0..n)
        .map(|_| MockResponse::tool_call("read_file", json!({"path": "does-not-exist.txt"})))
        .collect()
}

#[tokio::test]
async fn aborts_on_repeated_tool_call() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let model = MockModel::new().script_many(repeat_script(20));

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task("read the file forever"), &mut world, 30)
        .await
        .unwrap();

    match outcome {
        Outcome::Stuck {
            repeated, iters, ..
        } => {
            // Default policy: abort_after = 6.
            assert_eq!(repeated, 6, "should abort at the abort_after threshold");
            assert_eq!(iters, 6, "abort happens on the 6th identical round");
        }
        other => panic!("expected Outcome::Stuck, got {other:?}"),
    }
}

#[tokio::test]
async fn custom_thresholds_are_honored() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let model = MockModel::new().script_many(repeat_script(20));

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_stuck_policy(StuckPolicy {
            enabled: true,
            nudge_after: 2,
            abort_after: 3,
        })
        .run_with_max_iters(task("loop"), &mut world, 30)
        .await
        .unwrap();

    match outcome {
        Outcome::Stuck { repeated, .. } => assert_eq!(repeated, 3),
        other => panic!("expected Outcome::Stuck, got {other:?}"),
    }
}

#[tokio::test]
async fn disabled_policy_runs_to_budget() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let model = MockModel::new().script_many(repeat_script(20));

    // With detection off, the same repeated call is allowed to exhaust the
    // (small) iteration budget instead of aborting early.
    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_stuck_policy(StuckPolicy {
            enabled: false,
            ..Default::default()
        })
        .run_with_max_iters(task("loop"), &mut world, 4)
        .await
        .unwrap();

    assert!(
        matches!(outcome, Outcome::BudgetExhausted { .. }),
        "detection off → should hit the budget, got {outcome:?}"
    );
}

/// A tool that answers something different every time it is asked — a poll of
/// a thing that is moving (a build printing lines, a job whose clock ticks).
struct Ticking(std::sync::atomic::AtomicU64);

/// A tool frozen solid: the identical answer forever. Polling this really is
/// spinning, and must still be caught.
struct Frozen;

fn schema(name: &str) -> harness_core::ToolSchema {
    harness_core::ToolSchema {
        name: name.into(),
        description: "test".into(),
        input: json!({"type": "object", "properties": {}}),
    }
}

#[async_trait::async_trait]
impl harness_core::Tool for Ticking {
    fn name(&self) -> &str {
        "poll"
    }
    fn schema(&self) -> &harness_core::ToolSchema {
        static S: std::sync::OnceLock<harness_core::ToolSchema> = std::sync::OnceLock::new();
        S.get_or_init(|| schema("poll"))
    }
    fn risk(&self) -> harness_core::ToolRisk {
        harness_core::ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        _a: serde_json::Value,
        _w: &mut harness_core::World,
    ) -> Result<harness_core::ToolResult, harness_core::ToolError> {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(harness_core::ToolResult {
            ok: true,
            content: json!({ "state": "running", "elapsed_ms": n * 1000 }),
            trace: None,
        })
    }
}

#[async_trait::async_trait]
impl harness_core::Tool for Frozen {
    fn name(&self) -> &str {
        "poll"
    }
    fn schema(&self) -> &harness_core::ToolSchema {
        static S: std::sync::OnceLock<harness_core::ToolSchema> = std::sync::OnceLock::new();
        S.get_or_init(|| schema("poll"))
    }
    fn risk(&self) -> harness_core::ToolRisk {
        harness_core::ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        _a: serde_json::Value,
        _w: &mut harness_core::World,
    ) -> Result<harness_core::ToolResult, harness_core::ToolError> {
        Ok(harness_core::ToolResult {
            ok: true,
            content: json!({ "state": "running" }),
            trace: None,
        })
    }
}

/// The case that used to kill long runs: an identical call whose *result*
/// moves every round is the model watching progress, not spinning. It must be
/// allowed to run to its budget.
#[tokio::test]
async fn an_identical_call_with_a_changing_result_is_progress_not_stuck() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let model = MockModel::new().script_many(
        (0..20)
            .map(|_| MockResponse::tool_call("poll", json!({"id": 1})))
            .collect::<Vec<_>>(),
    );

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(Ticking(std::sync::atomic::AtomicU64::new(0))))
        .run_with_max_iters(task("wait for the build"), &mut world, 9)
        .await
        .unwrap();

    assert!(
        matches!(outcome, Outcome::BudgetExhausted { .. }),
        "a poll that keeps returning new information must not be judged stuck, got {outcome:?}"
    );
}

/// …and the guard still does its job when nothing actually changes.
#[tokio::test]
async fn an_identical_call_with_a_frozen_result_still_aborts() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let model = MockModel::new().script_many(
        (0..20)
            .map(|_| MockResponse::tool_call("poll", json!({"id": 1})))
            .collect::<Vec<_>>(),
    );

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(Frozen))
        .run_with_max_iters(task("poll something that never moves"), &mut world, 30)
        .await
        .unwrap();

    match outcome {
        Outcome::Stuck { repeated, .. } => assert_eq!(repeated, 6),
        other => panic!("expected Outcome::Stuck, got {other:?}"),
    }
}

/// A wall clock, not just a step count. An unattended run is bounded in hours;
/// `max_iters` cannot express that, because one iteration is a 100ms read or a
/// 20-minute build. Reaching the deadline must end the run the same graceful
/// way a spent step budget does — with a forced final answer, so the work is
/// reported rather than dropped.
#[tokio::test]
async fn a_task_deadline_stops_the_run_and_still_produces_an_answer() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    // The deadline bites before iteration 0 runs, so the ONLY model call this
    // run makes is the forced synthesis — hence a single scripted reply. (A
    // tool call here instead would prove nothing: the mock answers in order,
    // not by context.)
    let script = vec![MockResponse::text("here is what I got done")];

    let mut task = task("work until time runs out");
    // Already past: the very first iteration is over budget.
    task.deadline = Some(world.clock.now_ms() - 1);

    let outcome = AgentLoop::new(MockModel::new().script_many(script))
        .with_tool(Arc::new(Ticking(std::sync::atomic::AtomicU64::new(0))))
        .run_with_max_iters(task, &mut world, 40)
        .await
        .unwrap();

    match outcome {
        Outcome::BudgetExhausted {
            deadline_reached,
            iters,
            last_text,
            tools_called,
            ..
        } => {
            assert!(deadline_reached, "must report *why* it stopped");
            assert_eq!(iters, 0, "stopped before spending an iteration");
            assert_eq!(tools_called, 0);
            assert_eq!(
                last_text.as_deref(),
                Some("here is what I got done"),
                "a timed-out run still has to hand back its conclusion"
            );
        }
        other => panic!("expected BudgetExhausted via deadline, got {other:?}"),
    }
}

/// No deadline set → nothing changes; the step budget is still the bound.
#[tokio::test]
async fn without_a_deadline_the_step_budget_still_rules() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let mut script: Vec<MockResponse> = (0..10)
        .map(|_| MockResponse::tool_call("poll", json!({"id": 1})))
        .collect();
    script.push(MockResponse::text("done what I could"));

    let outcome = AgentLoop::new(MockModel::new().script_many(script))
        .with_tool(Arc::new(Ticking(std::sync::atomic::AtomicU64::new(0))))
        .run_with_max_iters(task("no clock"), &mut world, 4)
        .await
        .unwrap();

    match outcome {
        Outcome::BudgetExhausted {
            deadline_reached,
            iters,
            ..
        } => {
            assert!(!deadline_reached);
            assert_eq!(iters, 4, "spent the whole step budget");
        }
        other => panic!("expected BudgetExhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn distinct_calls_do_not_trip_detector() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    // Each round asks for a *different* path, then finishes — never a repeat.
    let model = MockModel::new()
        .script(MockResponse::tool_call(
            "read_file",
            json!({"path": "a.txt"}),
        ))
        .script(MockResponse::tool_call(
            "read_file",
            json!({"path": "b.txt"}),
        ))
        .script(MockResponse::tool_call(
            "read_file",
            json!({"path": "c.txt"}),
        ))
        .script(MockResponse::text("done"));

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task("read three files"), &mut world, 30)
        .await
        .unwrap();

    assert!(
        matches!(outcome, Outcome::Done { .. }),
        "distinct calls should finish normally, got {outcome:?}"
    );
}
