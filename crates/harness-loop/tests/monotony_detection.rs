//! Monotony-detector tests: a model that works *one* tool round after round —
//! a different argument every time, so nothing ever repeats — is a distinct
//! failure from the byte-identical spiral `StuckPolicy` catches, and needs its
//! own signal.
//!
//! The shape being reproduced here was measured on a real run: asked which
//! platforms a product integrates with, an agent that could not retrieve the
//! anchor entity began guessing candidates and looking them up one at a time,
//! twelve calls to one tool across twenty rounds, none of them useful, and
//! never once emitting text without a tool call. `MockModel` ignores context,
//! so a script of distinct calls to one tool models exactly that agent —
//! including its refusal to take the nudge.

use harness_context::default_world;
use harness_core::{Model, Task};
use harness_loop::{AgentLoop, MonotonyPolicy, Outcome, StuckPolicy};
use harness_models::{MockModel, MockResponse};
use harness_tools::fs::{Grep, ReadFile};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

fn tmp_workspace() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("monotony-test-{}-{nanos}", std::process::id()));
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

fn on() -> MonotonyPolicy {
    MonotonyPolicy {
        enabled: true,
        ..Default::default()
    }
}

/// `n` rounds of one tool, a different argument every round — the guessing
/// spiral. No two rounds are byte-identical, so `StuckPolicy` never sees a
/// repeat, which is the whole point.
fn guessing_script(n: usize) -> Vec<MockResponse> {
    (0..n)
        .map(|i| MockResponse::tool_call("read_file", json!({ "path": format!("guess-{i}.txt") })))
        .collect()
}

/// Keeps a handle on the model after `AgentLoop` has taken ownership.
struct Shared(Arc<MockModel>);

#[async_trait::async_trait]
impl Model for Shared {
    async fn complete(
        &self,
        ctx: &harness_core::Context,
    ) -> Result<harness_core::ModelOutput, harness_core::ModelError> {
        self.0.complete(ctx).await
    }
    fn info(&self) -> harness_core::ModelInfo {
        self.0.info()
    }
}

#[tokio::test]
async fn a_guessing_spiral_over_one_tool_is_caught_although_no_two_calls_repeat() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    // 16 distinct guesses, then the reply to the forced synthesis call.
    let mut script = guessing_script(16);
    script.push(MockResponse::text("I could not determine the platforms."));

    let outcome = AgentLoop::new(MockModel::new().script_many(script))
        .with_tool(Arc::new(ReadFile))
        .with_monotony_policy(on())
        .run_with_max_iters(
            task("which platforms does it integrate with"),
            &mut world,
            40,
        )
        .await
        .unwrap();

    match outcome {
        Outcome::Stuck {
            reason,
            repeated,
            iters,
            last_text,
            ..
        } => {
            assert_eq!(repeated, 16, "aborts at the monotony abort_after threshold");
            assert_eq!(iters, 16, "and on that round, not later");
            assert!(
                reason.contains("read_file") && reason.contains("without using any other tool"),
                "the reason must name the *other* detector's finding, not a repeat: {reason}"
            );
            // The exit the model could not find: tools stripped, one question.
            assert_eq!(
                last_text.as_deref(),
                Some("I could not determine the platforms."),
                "aborting a model that will not stop calling tools has to force its \
                 final answer, or the caller gets the same empty partial answer"
            );
        }
        other => panic!("expected Outcome::Stuck, got {other:?}"),
    }
}

/// The nudge is the half of the policy that is meant to do the work — early
/// enough to save most of the budget, cheap enough to be wrong about.
#[tokio::test]
async fn the_model_is_nudged_long_before_it_is_terminated() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let model = Arc::new(MockModel::new().script_many(guessing_script(9)));

    let _ = AgentLoop::new(Shared(model.clone()))
        .with_tool(Arc::new(ReadFile))
        .with_monotony_policy(on())
        .run_with_max_iters(task("guess"), &mut world, 9)
        .await
        .unwrap();

    // `MockModel` renders a feedback block as a count, not its text, and this
    // run produces no other feedback — so counting them locates the nudge.
    let feedbacks = |call: &harness_models::RecordedCall| -> usize {
        call.history_summary
            .iter()
            .flat_map(|h| h.kinds.iter())
            .filter(|k| **k == "feedback")
            .count()
    };
    let calls = model.calls();
    assert_eq!(
        feedbacks(&calls[7]),
        0,
        "nothing should be said to a model seven rounds in — that is still \
         within what a survey does"
    );
    assert_eq!(
        feedbacks(&calls[8]),
        1,
        "the eighth single-tool round must nudge, with the budget mostly unspent"
    );
}

/// The threshold has to sit above what honest work does. A survey — read a
/// handful of files, then answer — is the commonest single-tool run there is.
#[tokio::test]
async fn reading_a_handful_of_files_is_ordinary_work_and_is_left_alone() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let mut script = guessing_script(6);
    script.push(MockResponse::text("here is what those files say"));

    let outcome = AgentLoop::new(MockModel::new().script_many(script))
        .with_tool(Arc::new(ReadFile))
        .with_monotony_policy(on())
        .run_with_max_iters(task("read six files and summarise"), &mut world, 40)
        .await
        .unwrap();

    assert!(
        matches!(outcome, Outcome::Done { .. }),
        "six reads in a row is a survey, not a spiral, got {outcome:?}"
    );
}

/// Reaching for a second tool is movement, and movement resets the count —
/// which is why an agent that interleaves its tools can never trip this.
#[tokio::test]
async fn reaching_for_a_second_tool_resets_the_count() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    // 30 rounds, alternating between two tools: the longest single-tool run is
    // one, so the count never climbs past one however long the run goes.
    let script: Vec<MockResponse> = (0..30)
        .map(|i| {
            if i % 2 == 0 {
                MockResponse::tool_call("read_file", json!({ "path": format!("f-{i}.txt") }))
            } else {
                MockResponse::tool_call("grep", json!({ "pattern": format!("p{i}") }))
            }
        })
        .collect();

    let outcome = AgentLoop::new(MockModel::new().script_many(script))
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(Grep))
        .with_monotony_policy(on())
        .run_with_max_iters(task("interleave two tools"), &mut world, 25)
        .await
        .unwrap();

    assert!(
        matches!(outcome, Outcome::BudgetExhausted { .. }),
        "alternating tools is progress by this detector's definition, got {outcome:?}"
    );
}

/// Off by default, on purpose: an agent given one workhorse tool calls it every
/// round for as long as the budget lasts, and that is not a defect. Nobody's
/// shipped agent changes behaviour because this code exists.
#[tokio::test]
async fn a_single_workhorse_tool_runs_to_budget_while_the_policy_is_off() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);

    let outcome = AgentLoop::new(MockModel::new().script_many(guessing_script(30)))
        .with_tool(Arc::new(ReadFile))
        // No `with_monotony_policy` call at all — the default.
        .run_with_max_iters(task("one tool, twenty five rounds"), &mut world, 25)
        .await
        .unwrap();

    assert!(
        matches!(outcome, Outcome::BudgetExhausted { .. }),
        "the default must not terminate a working agent, got {outcome:?}"
    );
}

/// Both detectors exist; neither subsumes the other. When a spiral is *also*
/// byte-identical, the older, stricter policy reaches its threshold first and
/// the run is reported under its reason — so the two never blur together.
#[tokio::test]
async fn the_byte_identical_policy_still_wins_when_both_could_fire() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let script: Vec<MockResponse> = (0..20)
        .map(|_| MockResponse::tool_call("read_file", json!({"path": "same.txt"})))
        .collect();

    let outcome = AgentLoop::new(MockModel::new().script_many(script))
        .with_tool(Arc::new(ReadFile))
        .with_monotony_policy(on())
        .run_with_max_iters(task("repeat one call"), &mut world, 40)
        .await
        .unwrap();

    match outcome {
        Outcome::Stuck {
            repeated, reason, ..
        } => {
            assert_eq!(repeated, 6, "StuckPolicy::abort_after, not monotony's 16");
            assert!(
                reason.contains("repeated the same tool call"),
                "must be reported under the byte-identical reason: {reason}"
            );
        }
        other => panic!("expected Outcome::Stuck, got {other:?}"),
    }
}

/// …and the new signal stands on its own: it fires with the byte-identical
/// policy switched off entirely.
#[tokio::test]
async fn monotony_is_detected_with_the_byte_identical_policy_disabled() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let mut script = guessing_script(16);
    script.push(MockResponse::text("partial answer"));

    let outcome = AgentLoop::new(MockModel::new().script_many(script))
        .with_tool(Arc::new(ReadFile))
        .with_stuck_policy(StuckPolicy {
            enabled: false,
            ..Default::default()
        })
        .with_monotony_policy(on())
        .run_with_max_iters(task("guess"), &mut world, 40)
        .await
        .unwrap();

    match outcome {
        Outcome::Stuck { repeated, .. } => assert_eq!(repeated, 16),
        other => panic!("expected Outcome::Stuck, got {other:?}"),
    }
}

/// Thresholds are the caller's to set, like `StuckPolicy`'s.
#[tokio::test]
async fn custom_monotony_thresholds_are_honored() {
    let ws = tmp_workspace();
    let mut world = default_world(&ws);
    let mut script = guessing_script(4);
    script.push(MockResponse::text("stopping"));

    let outcome = AgentLoop::new(MockModel::new().script_many(script))
        .with_tool(Arc::new(ReadFile))
        .with_monotony_policy(MonotonyPolicy {
            enabled: true,
            nudge_after: 2,
            abort_after: 4,
        })
        .run_with_max_iters(task("guess"), &mut world, 40)
        .await
        .unwrap();

    match outcome {
        Outcome::Stuck { repeated, .. } => assert_eq!(repeated, 4),
        other => panic!("expected Outcome::Stuck, got {other:?}"),
    }
}
