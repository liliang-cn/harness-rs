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

#[tokio::test]
async fn distinct_calls_do_not_trip_detector() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    // Each round asks for a *different* path, then finishes — never a repeat.
    let model = MockModel::new()
        .script(MockResponse::tool_call("read_file", json!({"path": "a.txt"})))
        .script(MockResponse::tool_call("read_file", json!({"path": "b.txt"})))
        .script(MockResponse::tool_call("read_file", json!({"path": "c.txt"})))
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
