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
    async fn compact(&self, _stage: CompactionStage, _ctx: &mut Context) -> Result<(), CompactError> {
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
