//! A turn that ends with neither an answer nor a tool call has stalled, not
//! finished — the loop must ask it to carry on rather than dress its own
//! planning monologue up as the result.
//!
//! Seen in the wild: a provider answers a tool result by narrating the next
//! step as reasoning and ending the turn. `text` is empty, `tool_calls` is
//! empty, and the reasoning fallback then hands that monologue back as if it
//! were the answer, so a run that did nothing reads as a run that succeeded.

use harness_context::default_world;
use harness_core::{StopReason, Task};
use harness_loop::{AgentLoop, FilesExist, Outcome};
use harness_models::{MockModel, MockResponse};

fn task(desc: &str) -> Task {
    Task {
        description: desc.into(),
        source: None,
        deadline: None,
    }
}

/// A turn with only reasoning: no text, no calls.
fn only_thinking(thought: &str) -> MockResponse {
    let mut r = MockResponse::text("");
    r.text = None;
    r.reasoning = Some(thought.into());
    r.stop_reason = StopReason::EndTurn;
    r
}

#[tokio::test]
async fn a_turn_that_only_thought_gets_asked_to_carry_on() {
    let mut world = default_world(".");
    let model = MockModel::new()
        .script(only_thinking(
            "**Initiating Document Design** — I'll write the docx next.",
        ))
        .script(MockResponse::text("Done — resume.docx is written."));

    let outcome = AgentLoop::new(model)
        .run_with_max_iters(task("convert the pdf"), &mut world, 8)
        .await
        .unwrap();

    match outcome {
        Outcome::Done {
            text,
            iters,
            verified,
            ..
        } => {
            // The correction cost one round, and the answer is the real one —
            // not the monologue.
            assert_eq!(iters, 2, "the stalled turn plus the one that followed");
            let text = text.expect("an answer");
            assert!(text.contains("resume.docx"), "got {text:?}");
            assert!(!text.contains("Initiating Document Design"));
            assert_eq!(verified.map(|v| v.passed), Some(true), "and it was checked");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn it_only_asks_once_and_then_reports_what_there_is() {
    let mut world = default_world(".");
    // Stalls every time: the loop must not spend the whole budget nudging.
    let stalls: Vec<MockResponse> = (0..6)
        .map(|_| only_thinking("still just thinking"))
        .collect();
    let model = MockModel::new().script_many(stalls);

    let outcome = AgentLoop::new(model)
        .run_with_max_iters(task("do the thing"), &mut world, 8)
        .await
        .unwrap();

    match outcome {
        Outcome::Done {
            iters,
            text,
            verified,
            ..
        } => {
            assert_eq!(
                iters, 2,
                "one correction, then it reports rather than looping"
            );
            // The reasoning fallback still applies, so the caller sees SOMETHING
            // rather than a blank turn — but the outcome no longer PRETENDS the
            // work was done: the verdict travels with it.
            assert_eq!(text.as_deref(), Some("still just thinking"));
            let v = verified.expect("a verdict");
            assert!(!v.passed, "reported as unverified, not as success");
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn a_promised_file_that_never_appeared_is_not_done() {
    let dir = std::env::temp_dir().join(format!("stalled-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut world = default_world(&dir);

    // It says it wrote the file. Twice. It never does.
    let model = MockModel::new()
        .script(MockResponse::text("Done — I converted it to 简历.docx."))
        .script(MockResponse::text("Really, it's there now."));

    let outcome = AgentLoop::new(model)
        .with_acceptance(std::sync::Arc::new(FilesExist::new(["简历.docx"])))
        .run_with_max_iters(task("convert the pdf"), &mut world, 8)
        .await
        .unwrap();

    match outcome {
        Outcome::Done { verified, .. } => {
            let v = verified.expect("checked");
            assert!(!v.passed, "claiming twice is not producing once");
            assert!(v.reason.contains("简历.docx"), "{v:?}");
        }
        other => panic!("expected Done, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
