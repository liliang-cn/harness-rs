//! Compaction tuning: hysteresis (start over high-water, stop at target) and
//! real-token calibration. A `StubCompactor` gives deterministic budget ratios
//! so we can assert the loop's escalation/stop behavior exactly.

use async_trait::async_trait;
use harness_compactor::CALIBRATION_KEY;
use harness_context::default_world;
use harness_core::{Budget, CompactError, CompactionStage, Compactor, Context, Task};
use harness_loop::{AgentLoop, CompactPolicy};
use harness_models::{MockModel, MockResponse};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Reports a controllable budget ratio; each `compact()` drops `used` by a
/// fixed step. Also records the calibration factor it sees in metadata, so we
/// can prove the loop writes it back.
struct StubCompactor {
    used: Mutex<u32>,
    window: u32,
    drop_per_stage: u32,
    compactions: AtomicUsize,
    seen_correction: Mutex<f64>,
}

impl StubCompactor {
    fn new(used: u32, window: u32, drop_per_stage: u32) -> Arc<Self> {
        Arc::new(Self {
            used: Mutex::new(used),
            window,
            drop_per_stage,
            compactions: AtomicUsize::new(0),
            seen_correction: Mutex::new(1.0),
        })
    }
}

#[async_trait]
impl Compactor for StubCompactor {
    fn budget(&self, ctx: &Context) -> Budget {
        if let Some(f) = ctx.metadata.get(CALIBRATION_KEY).and_then(|v| v.as_f64()) {
            *self.seen_correction.lock().unwrap() = f;
        }
        Budget {
            used: *self.used.lock().unwrap(),
            window: self.window,
        }
    }
    async fn compact(
        &self,
        _stage: CompactionStage,
        _ctx: &mut Context,
    ) -> Result<(), CompactError> {
        self.compactions.fetch_add(1, Ordering::SeqCst);
        let mut u = self.used.lock().unwrap();
        *u = u.saturating_sub(self.drop_per_stage);
        Ok(())
    }
}

fn task(d: &str) -> Task {
    Task {
        description: d.into(),
        source: None,
        deadline: None,
    }
}

async fn run_one_turn(stub: Arc<StubCompactor>) {
    let ws = std::env::temp_dir().join(format!("compact-test-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);
    // One text turn → the loop runs exactly one iteration (one compaction pass).
    let model = MockModel::new().script(MockResponse::text("done").with_usage(500, 10));
    AgentLoop::new(model)
        .with_compactor(stub)
        .run(task("go"), &mut world)
        .await
        .unwrap();
}

#[tokio::test]
async fn stops_at_target_not_all_stages() {
    // ratio 0.90; each stage drops 0.20. high_water 0.75, target 0.55.
    // 0.90 → 0.70 → 0.50 (≤ target): exactly 2 compactions, not 5.
    let stub = StubCompactor::new(900, 1000, 200);
    run_one_turn(stub.clone()).await;
    assert_eq!(stub.compactions.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn no_compaction_below_high_water() {
    // ratio 0.50 < high_water 0.75 → never compacts.
    let stub = StubCompactor::new(500, 1000, 200);
    run_one_turn(stub.clone()).await;
    assert_eq!(stub.compactions.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn custom_policy_thresholds_honored() {
    // Tighter target forces more stages: 0.90 → .8 → .7 → .6 → .5 (≤ .55) = 4.
    let stub = StubCompactor::new(900, 1000, 100);
    let ws = std::env::temp_dir().join(format!("compact-test2-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);
    let model = MockModel::new().script(MockResponse::text("done").with_usage(500, 10));
    AgentLoop::new(model)
        .with_compactor(stub.clone())
        .with_compact_policy(CompactPolicy {
            high_water: 0.85,
            target: 0.55,
        })
        .run(task("go"), &mut world)
        .await
        .unwrap();
    assert_eq!(stub.compactions.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn loop_writes_real_token_calibration() {
    // used=100 (ratio 0.1 → no compaction). Model reports input_tokens=500.
    // Correction should converge to real/used = 500/100 = 5.0, and be visible
    // to the compactor on the following turn.
    let stub = StubCompactor::new(100, 1000, 0);
    let ws = std::env::temp_dir().join(format!("calib-test-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);
    // Turn 1: a tool call (keeps the loop going), reporting 500 input tokens.
    // Turn 2: text done — its budget() call reads the calibration written in T1.
    let model = MockModel::new()
        .script(MockResponse::tool_call("noop", json!({})).with_usage(500, 10))
        .script(MockResponse::text("done").with_usage(500, 10));
    AgentLoop::new(model)
        .with_compactor(stub.clone())
        .run(task("go"), &mut world)
        .await
        .unwrap();
    let seen = *stub.seen_correction.lock().unwrap();
    assert!(
        (seen - 5.0).abs() < 1e-6,
        "expected calibration 5.0, saw {seen}"
    );
}

/// The compaction threshold has to be a fraction of the *model's* window.
///
/// It was a fixed 150,000 default that nothing connected to `ModelInfo`, so a
/// small-window model needed 112,500 tokens before the compactor would run —
/// more than it can hold. The provider rejects the request first, and the
/// mechanism meant to prevent exactly that never gets a turn.
#[tokio::test]
async fn the_budget_window_comes_from_the_model() {
    use harness_core::{
        Block, Context, Event, Hook, HookOutcome, Model, ModelError, ModelInfo, ModelOutput,
        StopReason, Turn, TurnRole, World,
    };
    use std::sync::Mutex;

    /// Declares a small window, the shape of a local 32k model.
    struct SmallWindow;
    #[async_trait::async_trait]
    impl Model for SmallWindow {
        async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
            Ok(ModelOutput {
                text: Some("done".into()),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            })
        }
        fn info(&self) -> ModelInfo {
            ModelInfo {
                handle: "small".into(),
                provider: "test".into(),
                model: "small".into(),
                context_window: 32_000,
                input_cost_usd_per_million_tokens: None,
                output_cost_usd_per_million_tokens: None,
                supports_tool_use: false,
                supports_streaming: false,
                supports_web_grounding: false,
            }
        }
    }

    /// Reads the budget the loop actually handed the compactor.
    #[derive(Default)]
    struct SeenPolicy(Mutex<u32>);
    impl Hook for SeenPolicy {
        fn name(&self) -> &str {
            "seen-policy"
        }
        fn matches(&self, ev: &Event<'_>) -> bool {
            matches!(ev, Event::PreModel { .. })
        }
        fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
            if let Event::PreModel { ctx } = ev {
                *self.0.lock().unwrap() = ctx.policy.max_input_tokens;
            }
            HookOutcome::Allow
        }
    }

    let ws = std::env::temp_dir().join(format!("budget-window-{}", std::process::id()));
    std::fs::create_dir_all(&ws).unwrap();
    let mut world = default_world(&ws);
    let seen = Arc::new(SeenPolicy::default());

    // A seeded history big enough to matter against a 32k window but nowhere
    // near the old fixed 150k default.
    let bulk = Turn {
        role: TurnRole::User,
        blocks: vec![Block::Text("x".repeat(80_000))],
    };
    AgentLoop::new(SmallWindow)
        .with_hook(seen.clone())
        .run_with_seed_history(task("go"), vec![bulk], &mut world, 2)
        .await
        .unwrap();
    let _ = std::fs::remove_dir_all(&ws);

    let budget = *seen.0.lock().unwrap();
    assert!(
        budget < 32_000,
        "the input budget must come from the model's 32k window, got {budget}"
    );
    assert!(
        budget >= 20_000,
        "and must not be needlessly small (window minus the reply allowance), got {budget}"
    );
}

/// Compaction, end to end, with the real `DefaultCompactor` — not a stub.
///
/// Every live measurement in this repo reported `compactions=0`: the default
/// budget was large enough that nothing ever crossed the high-water mark, so the
/// five-stage pipeline had never actually run against a real context. This drives
/// it with a genuinely oversized history and checks it both fires and lands under
/// the target.
#[tokio::test]
async fn a_real_oversized_context_is_compacted_under_target() {
    use harness_compactor::DefaultCompactor;
    use harness_core::{Block, Compactor, Context, Task, Turn, TurnRole};

    let compactor = DefaultCompactor::default();

    // A history shaped like a long agent run: many turns, several fat tool
    // results — the thing that actually fills a window in practice.
    let mut ctx = Context::new(Task {
        description: "summarise the work so far".into(),
        source: None,
        deadline: None,
    });
    ctx.policy.max_input_tokens = 32_000;
    for i in 0..40 {
        ctx.history.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::Text(format!("step {i}: looked at the module"))],
        });
        ctx.history.push(Turn {
            role: TurnRole::Tool,
            blocks: vec![Block::ToolResult {
                call_id: format!("c{i}"),
                content: serde_json::json!({ "content": "line of file\n".repeat(200) }),
            }],
        });
    }

    let before = compactor.budget(&ctx);
    assert!(
        before.ratio() > 0.75,
        "the fixture must actually be over the high-water mark, got {:.2}",
        before.ratio()
    );

    // Drive the same stage sequence the loop does.
    let mut budget = before;
    let mut ran = 0;
    for stage in harness_core::CompactionStage::ALL {
        if budget.ratio() <= 0.55 {
            break;
        }
        compactor.compact(stage, &mut ctx).await.unwrap();
        budget = compactor.budget(&ctx);
        ran += 1;
    }

    assert!(ran > 0, "compaction must run");
    assert!(
        budget.ratio() <= 0.55,
        "compaction must reach the target: {:.2} after {ran} stage(s)",
        budget.ratio()
    );
    // The conversation must survive: compaction shrinks content, it does not
    // empty the history.
    assert!(
        !ctx.history.is_empty(),
        "compaction must not discard the conversation outright"
    );
}

/// Compaction decides by turn count; a context blows up by size. A short
/// conversation carrying one enormous tool result is the ordinary way an agent
/// fills a window — read a large file, and two turns later there is no room —
/// and every stage guards on `history.len() <= keep_recent`, so all five bail
/// out and the run proceeds straight into a provider rejection.
#[tokio::test]
async fn a_short_history_with_one_huge_turn_is_still_compacted() {
    use harness_compactor::DefaultCompactor;
    use harness_core::{Block, Compactor, Context, Task, Turn, TurnRole};

    let compactor = DefaultCompactor::default();
    let mut ctx = Context::new(Task {
        description: "what does this file do".into(),
        source: None,
        deadline: None,
    });
    ctx.policy.max_input_tokens = 8_000;

    // Three turns — well under every stage's `keep_recent` — one of which is
    // a large file read.
    ctx.history.push(Turn {
        role: TurnRole::User,
        blocks: vec![Block::Text("read big.txt".into())],
    });
    ctx.history.push(Turn {
        role: TurnRole::Assistant,
        blocks: vec![Block::Text("reading".into())],
    });
    ctx.history.push(Turn {
        role: TurnRole::Tool,
        blocks: vec![Block::ToolResult {
            call_id: "c1".into(),
            content: serde_json::json!({ "content": "a line of source code\n".repeat(4000) }),
        }],
    });

    let before = compactor.budget(&ctx);
    assert!(
        before.ratio() > 0.75,
        "fixture must be over the high-water mark, got {:.2}",
        before.ratio()
    );

    let mut budget = before;
    for stage in harness_core::CompactionStage::ALL {
        if budget.ratio() <= 0.55 {
            break;
        }
        compactor.compact(stage, &mut ctx).await.unwrap();
        budget = compactor.budget(&ctx);
    }

    assert!(
        budget.used < before.used,
        "five stages must reduce something: {} tokens before, {} after",
        before.used,
        budget.used
    );
}
