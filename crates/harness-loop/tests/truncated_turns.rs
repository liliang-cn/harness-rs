//! A turn the provider cut off at the output-token limit is not an answer.
//! The loop sends it back with an instruction to act, and if the model keeps
//! getting cut off the run ends as `Stuck`, never as `Done`.

use harness_context::default_world;
use harness_core::{Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, Task};
use harness_loop::{AgentLoop, Outcome};
use harness_models::{MockModel, MockResponse};
use std::sync::Arc;

/// Lets the test keep a handle on the mock after the loop takes it.
struct Shared(Arc<MockModel>);

#[async_trait::async_trait]
impl Model for Shared {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.0.complete(ctx).await
    }
    fn info(&self) -> ModelInfo {
        self.0.info()
    }
}

fn cut_off(reasoning: &str) -> MockResponse {
    MockResponse {
        reasoning: Some(reasoning.into()),
        stop_reason: StopReason::MaxTokens,
        ..Default::default()
    }
}

fn task() -> Task {
    Task {
        description: "score the transactions".into(),
        source: None,
        deadline: None,
    }
}

#[tokio::test]
async fn a_cut_off_turn_is_retried_not_taken_as_the_answer() {
    let model = MockModel::new().script_many(vec![
        cut_off("let me think about R1 again..."),
        MockResponse::text("final answer"),
    ]);
    let out = AgentLoop::new(model)
        .run_with_max_iters(task(), &mut default_world(std::env::temp_dir()), 10)
        .await
        .unwrap();
    match out {
        Outcome::Done { text, iters, .. } => {
            assert_eq!(text.as_deref(), Some("final answer"));
            assert_eq!(iters, 2);
        }
        other => panic!("expected Done after the retry, got {other:?}"),
    }
}

#[tokio::test]
async fn a_model_that_keeps_getting_cut_off_ends_stuck() {
    let model = MockModel::new().script_many(vec![
        cut_off("thinking 1"),
        cut_off("thinking 2"),
        cut_off("thinking 3"),
        MockResponse::text("never reached"),
    ]);
    let out = AgentLoop::new(model)
        .run_with_max_iters(task(), &mut default_world(std::env::temp_dir()), 10)
        .await
        .unwrap();
    match out {
        Outcome::Stuck {
            reason,
            repeated,
            last_text,
            ..
        } => {
            assert_eq!(repeated, 3);
            assert!(reason.contains("output token limit"), "{reason}");
            assert!(
                last_text.is_none(),
                "reasoning must not be passed off as the answer"
            );
        }
        other => panic!("expected Stuck, got {other:?}"),
    }
}

#[tokio::test]
async fn the_retry_tells_the_model_what_happened() {
    let probe =
        Arc::new(MockModel::new().script_many(vec![cut_off("hmm"), MockResponse::text("ok")]));
    AgentLoop::new(Shared(probe.clone()))
        .run_with_max_iters(task(), &mut default_world(std::env::temp_dir()), 10)
        .await
        .unwrap();
    let calls = probe.calls();
    assert_eq!(calls.len(), 2);
    let second = format!("{:?}", calls[1].history_summary);
    assert!(second.contains("output token limit"), "{second}");
}
