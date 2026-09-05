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
