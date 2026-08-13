//! A single tool result must not be able to eat the context window.
//!
//! Measured on a real run before this existed: "search these files for a word"
//! cost 53,487 input tokens, because one `read_file` returned a lock file. The
//! model then re-paid for that blob on every following turn, and compaction
//! began discarding real history to fit it. Tools cannot all be trusted to
//! bound themselves — a third-party MCP tool is outside the framework entirely —
//! so the ceiling belongs at the point where a result enters the context.

use async_trait::async_trait;
use harness_context::default_world;
use harness_core::{
    Event, Hook, HookOutcome, Task, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, TurnRole,
    World,
};
use harness_loop::{AgentLoop, Outcome, ToolResultPolicy};
use harness_models::{MockModel, MockResponse};
use serde_json::json;
use std::sync::{Arc, Mutex};

/// Captures the context as the model sees it — the bytes actually billed.
#[derive(Default)]
struct SeenContext(Mutex<String>);

impl Hook for SeenContext {
    fn name(&self) -> &str {
        "seen-context"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreModel { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        if let Event::PreModel { ctx } = ev {
            let tool_turns: String = ctx
                .history
                .iter()
                .filter(|t| t.role == TurnRole::Tool)
                .map(|t| format!("{:?}", t.blocks))
                .collect();
            if !tool_turns.is_empty() {
                *self.0.lock().unwrap() = tool_turns;
            }
        }
        HookOutcome::Allow
    }
}

/// Returns a payload far larger than any window budget — the lock-file shape.
struct Firehose;

#[async_trait]
impl Tool for Firehose {
    fn name(&self) -> &str {
        "firehose"
    }
    fn schema(&self) -> &ToolSchema {
        static S: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        S.get_or_init(|| ToolSchema {
            name: "firehose".into(),
            description: "returns a lot".into(),
            input: json!({"type": "object"}),
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        _args: serde_json::Value,
        _world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult {
            ok: true,
            content: json!({ "body": "x".repeat(200_000) }),
            trace: None,
        })
    }
}

fn task() -> Task {
    Task {
        description: "go".into(),
        source: None,
        deadline: None,
    }
}

async fn run_with(policy: ToolResultPolicy) -> String {
    let ws = std::env::temp_dir().join(format!(
        "tool-cap-{}-{:?}",
        std::process::id(),
        policy.max_bytes
    ));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);

    let seen = Arc::new(SeenContext::default());
    let outcome = AgentLoop::new(
        MockModel::new()
            .script(MockResponse::tool_call("firehose", json!({})))
            .script(MockResponse::text("done")),
    )
    .with_tool(Arc::new(Firehose))
    .with_tool_result_policy(policy)
    .with_hook(seen.clone())
    .run(task(), &mut world)
    .await
    .unwrap();
    assert!(matches!(outcome, Outcome::Done { .. }));

    let _ = std::fs::remove_dir_all(&ws);
    let out = seen.0.lock().unwrap();
    out.clone()
}

#[tokio::test]
async fn an_oversized_result_is_capped_before_it_reaches_the_context() {
    let recorded = run_with(ToolResultPolicy::default()).await;

    // The 200 KB body is gone; a bounded marker took its place.
    assert!(
        recorded.len() < 64 * 1024,
        "a 200 KB result reached the context: {} bytes",
        recorded.len()
    );
    assert!(recorded.contains("truncated"), "{recorded:.400}");
    // The marker has to be actionable, or the next turn just asks again.
    assert!(recorded.contains("narrow it"), "{recorded:.400}");
    assert!(recorded.contains("bytes_total"), "{recorded:.400}");
}

#[tokio::test]
async fn a_small_result_passes_through_untouched() {
    struct Tiny;
    #[async_trait]
    impl Tool for Tiny {
        fn name(&self) -> &str {
            "tiny"
        }
        fn schema(&self) -> &ToolSchema {
            static S: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
            S.get_or_init(|| ToolSchema {
                name: "tiny".into(),
                description: String::new(),
                input: json!({"type": "object"}),
            })
        }
        fn risk(&self) -> ToolRisk {
            ToolRisk::ReadOnly
        }
        async fn invoke(
            &self,
            _a: serde_json::Value,
            _w: &mut World,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                ok: true,
                content: json!({"answer": 42}),
                trace: None,
            })
        }
    }

    let ws = std::env::temp_dir().join(format!("tool-cap-small-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);
    let seen = Arc::new(SeenContext::default());
    AgentLoop::new(
        MockModel::new()
            .script(MockResponse::tool_call("tiny", json!({})))
            .script(MockResponse::text("done")),
    )
    .with_tool(Arc::new(Tiny))
    .with_hook(seen.clone())
    .run(task(), &mut world)
    .await
    .unwrap();
    let recorded = seen.0.lock().unwrap().clone();
    let _ = std::fs::remove_dir_all(&ws);

    assert!(recorded.contains("42"), "{recorded}");
    assert!(
        !recorded.contains("truncated"),
        "an ordinary result must not be marked: {recorded}"
    );
}

/// The guard is a default, not a law — a caller who genuinely wants the whole
/// blob (a one-shot extraction into a 1M window) can turn it off.
#[tokio::test]
async fn the_cap_can_be_disabled() {
    let recorded = run_with(ToolResultPolicy {
        max_bytes: None,
        ..Default::default()
    })
    .await;
    assert!(
        recorded.len() > 100_000,
        "disabling the cap must pass the payload through: {} bytes",
        recorded.len()
    );
    assert!(!recorded.contains("truncated"), "{recorded:.200}");
}

/// A read-only call repeated later in the same run adds the same bytes to the
/// context a second time and teaches the model nothing. `StuckPolicy` cannot see
/// it — the rounds are not consecutive — so it looks like progress.
#[tokio::test]
async fn a_repeated_read_only_call_is_answered_with_a_pointer() {
    let ws = std::env::temp_dir().join(format!("tool-repeat-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);

    let seen = Arc::new(SeenContext::default());
    AgentLoop::new(
        MockModel::new()
            .script(MockResponse::tool_call("firehose", json!({"q": 1})))
            // A different call in between, so the rounds are not consecutive.
            .script(MockResponse::tool_call("firehose", json!({"q": 2})))
            .script(MockResponse::tool_call("firehose", json!({"q": 1})))
            .script(MockResponse::text("done")),
    )
    .with_tool(Arc::new(Firehose))
    // Opt in: suppression is off by default because it cost a task on the
    // completion benchmark. This test is about what it does when asked for.
    .with_tool_result_policy(ToolResultPolicy {
        dedupe_repeats: true,
        ..Default::default()
    })
    .with_hook(seen.clone())
    .with_hook(Arc::new(RepeatWatcher))
    .run(task(), &mut world)
    .await
    .unwrap();
    let recorded = seen.0.lock().unwrap().clone();
    let _ = std::fs::remove_dir_all(&ws);

    assert!(
        recorded.contains("repeat_of_earlier_call"),
        "the third call repeats the first: {recorded:.400}"
    );
    assert!(
        recorded.contains("still stands"),
        "the pointer must tell the model what to do: {recorded:.400}"
    );
    // The suppression happens before `PostToolUse`, so a hook — an audit log,
    // the telemetry summary — records what the model actually received rather
    // than a payload it never saw.
    assert!(
        seen_tool_results().contains("repeat_of_earlier_call"),
        "hooks must observe the substituted result"
    );
}

/// Captured by `RepeatWatcher` below.
static SEEN_RESULTS: Mutex<String> = Mutex::new(String::new());
fn seen_tool_results() -> String {
    SEEN_RESULTS.lock().unwrap().clone()
}

struct RepeatWatcher;
impl Hook for RepeatWatcher {
    fn name(&self) -> &str {
        "repeat-watcher"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PostToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        if let Event::PostToolUse { result, .. } = ev {
            SEEN_RESULTS
                .lock()
                .unwrap()
                .push_str(&result.content.to_string());
        }
        HookOutcome::Allow
    }
}

/// Suppression must not survive a write: after something changes the workspace,
/// re-reading is the correct move, not a repeat.
#[tokio::test]
async fn a_write_between_reads_clears_the_record() {
    struct Mutator;
    #[async_trait]
    impl Tool for Mutator {
        fn name(&self) -> &str {
            "mutate"
        }
        fn schema(&self) -> &ToolSchema {
            static S: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
            S.get_or_init(|| ToolSchema {
                name: "mutate".into(),
                description: String::new(),
                input: json!({"type": "object"}),
            })
        }
        fn risk(&self) -> ToolRisk {
            ToolRisk::Destructive
        }
        async fn invoke(
            &self,
            _a: serde_json::Value,
            _w: &mut World,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                ok: true,
                content: json!({"wrote": true}),
                trace: None,
            })
        }
    }

    let ws = std::env::temp_dir().join(format!("tool-repeat-w-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);

    let seen = Arc::new(SeenContext::default());
    AgentLoop::new(
        MockModel::new()
            .script(MockResponse::tool_call("firehose", json!({"q": 1})))
            .script(MockResponse::tool_call("mutate", json!({})))
            .script(MockResponse::tool_call("firehose", json!({"q": 1})))
            .script(MockResponse::text("done")),
    )
    .with_tool(Arc::new(Firehose))
    .with_tool(Arc::new(Mutator))
    .with_tool_result_policy(ToolResultPolicy {
        dedupe_repeats: true,
        ..Default::default()
    })
    .with_hook(seen.clone())
    .run(task(), &mut world)
    .await
    .unwrap();
    let recorded = seen.0.lock().unwrap().clone();
    let _ = std::fs::remove_dir_all(&ws);

    assert!(
        !recorded.contains("repeat_of_earlier_call"),
        "a write invalidates the earlier read: {recorded:.400}"
    );
}
