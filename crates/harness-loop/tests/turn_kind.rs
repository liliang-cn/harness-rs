//! `TurnKind::of` is the loop's whole reading of a model turn. Every shape a
//! provider has actually sent is pinned here, so a new shape is a new row and
//! never a fresh bug in some branch that forgot about it.

use harness_context::default_world;
use harness_core::{ModelOutput, StopReason, Task, ToolCall};
use harness_loop::{AgentLoop, Outcome, TurnKind};
use harness_models::{MockModel, MockResponse};

fn out(text: Option<&str>, reasoning: Option<&str>, calls: usize, stop: StopReason) -> ModelOutput {
    ModelOutput {
        text: text.map(Into::into),
        tool_calls: (0..calls)
            .map(|i| ToolCall {
                id: format!("c{i}"),
                name: "read_file".into(),
                args: serde_json::json!({}),
            })
            .collect(),
        usage: Default::default(),
        stop_reason: stop,
        reasoning: reasoning.map(Into::into),
        images: Vec::new(),
    }
}

#[test]
fn every_shape_of_turn_has_one_reading() {
    use StopReason::*;
    use TurnKind::*;
    let table: &[(ModelOutput, TurnKind, &str)] = &[
        (out(None, None, 1, ToolUse), Acts, "a tool call"),
        (
            out(Some("let me look"), None, 2, ToolUse),
            Acts,
            "text alongside calls",
        ),
        (
            out(None, None, 1, MaxTokens),
            Acts,
            "calls win over the stop reason",
        ),
        (
            out(Some("the answer"), None, 0, EndTurn),
            Answers,
            "plain answer",
        ),
        (
            out(Some("the answer"), Some("why"), 0, EndTurn),
            Answers,
            "answer with reasoning",
        ),
        (
            out(Some("done"), None, 0, StopSequence),
            Answers,
            "stop sequence",
        ),
        (
            out(None, Some("8000 tokens of thinking"), 0, MaxTokens),
            Truncated,
            "9B: all reasoning, cut off",
        ),
        (
            out(Some("half an ans"), None, 0, MaxTokens),
            Truncated,
            "half an answer, cut off",
        ),
        (
            out(None, None, 0, MaxTokens),
            Truncated,
            "cut off with nothing",
        ),
        (
            out(None, Some("I'll write the docx next"), 0, EndTurn),
            Stalled,
            "only thought, then stopped",
        ),
        (
            out(Some("   \n"), None, 0, EndTurn),
            Stalled,
            "whitespace is not an answer",
        ),
        (out(None, None, 0, EndTurn), Stalled, "nothing at all"),
        (
            out(None, None, 0, Other),
            Stalled,
            "unknown stop with nothing",
        ),
    ];
    for (o, want, why) in table {
        assert_eq!(TurnKind::of(o), *want, "{why}");
    }
}

fn only_thinking() -> MockResponse {
    MockResponse {
        reasoning: Some("I'll get to it".into()),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    }
}

/// Dropping the default acceptance set must not turn a stalled turn back into
/// a successful answer: that rule is structural, not configuration.
#[tokio::test]
async fn a_stalled_turn_fails_even_with_no_acceptance_checks() {
    let model = MockModel::new().script_many(vec![only_thinking(), only_thinking()]);
    let out = AgentLoop::new(model)
        .with_acceptance_set(Vec::new())
        .run_with_max_iters(
            Task {
                description: "do it".into(),
                source: None,
                deadline: None,
            },
            &mut default_world(std::env::temp_dir()),
            8,
        )
        .await
        .unwrap();
    match out {
        Outcome::Done {
            verified, iters, ..
        } => {
            assert_eq!(iters, 2, "asked once to carry on, then reported");
            assert!(!verified.expect("a verdict").passed);
        }
        other => panic!("expected Done, got {other:?}"),
    }
}
