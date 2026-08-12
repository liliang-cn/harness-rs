//! Running out of iterations is not the same as having nothing to show.
//!
//! Measured on a real run: a code-audit subagent produced a complete, correct
//! answer on its last iteration, was reported `Blocked`, and the orchestrator
//! dead-lettered it — filing finished work as an error message and cancelling
//! everything downstream of it.

use harness_context::default_world;
use harness_core::{SubagentStatus, Task};
use harness_loop::{Subagent, SubagentSpec};
use harness_models::{MockModel, MockResponse};
use serde_json::json;

fn spec(name: &str, iters: u32) -> SubagentSpec {
    SubagentSpec::new(
        name,
        Task {
            description: "do the work".into(),
            source: None,
            deadline: None,
        },
    )
    .with_max_iters(iters)
}

fn ws(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("subagent-budget-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[tokio::test]
async fn work_finished_on_the_last_iteration_is_not_thrown_away() {
    let dir = ws("done");
    let mut world = default_world(&dir);

    // Two iterations allowed; the model keeps calling a tool and answers at the
    // end — the shape of a real research/audit job that fills its budget.
    let model = MockModel::new()
        .script(MockResponse::tool_call("read_file", json!({"path": "x"})))
        .script(MockResponse::text("(A) none. (B) all nine are safe."));

    let report = Subagent::new(model, spec("audit", 2))
        .run(&mut world)
        .await
        .expect("run");
    let _ = std::fs::remove_dir_all(&dir);

    assert_ne!(
        report.status,
        SubagentStatus::Blocked,
        "a subagent holding a real answer must not report Blocked: {report:?}"
    );
    assert!(
        report.text.as_deref().unwrap_or("").contains("safe"),
        "the answer must survive: {report:?}"
    );
}

#[tokio::test]
async fn an_empty_handed_subagent_is_still_blocked() {
    let dir = ws("empty");
    let mut world = default_world(&dir);

    // Burns its single iteration on a tool call and never says anything.
    let model = MockModel::new()
        .script(MockResponse::tool_call("read_file", json!({"path": "x"})))
        .script(MockResponse::tool_call("read_file", json!({"path": "y"})));

    let report = Subagent::new(model, spec("silent", 1))
        .run(&mut world)
        .await
        .expect("run");
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        report.status,
        SubagentStatus::Blocked,
        "nothing to show is the case Blocked is for: {report:?}"
    );
}
