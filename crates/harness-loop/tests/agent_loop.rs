//! End-to-end AgentLoop tests with `MockModel`. These verify the framework
//! integrates correctly without depending on any LLM service.

use harness_context::default_world;
use harness_core::{
    Action, Block, Context, Event, Execution, GuideId, GuideScope, HookOutcome, SensorId, Severity,
    Signal, Skill, Stage, Task, World,
};
use harness_loop::{AgentLoop, Outcome};
use harness_models::{MockModel, MockResponse};
use harness_tools_fs::{ReadFile, WriteFile};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

// ============================================================
// shared test fixtures
// ============================================================

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
        let p = std::env::temp_dir().join(format!("harness-loop-test-{pid}-{nanos}-{n}"));
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
    Task { description: desc.into(), source: None, deadline: None }
}

// ============================================================
// 1. Text-only response terminates the loop
// ============================================================

#[tokio::test]
async fn text_only_response_returns_done_immediately() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new().script(MockResponse::text("hello"));
    let outcome = AgentLoop::new(model)
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

// ============================================================
// 2. Tool call → result → followup text completes loop
// ============================================================

#[tokio::test]
async fn tool_call_then_text_takes_two_iters() {
    let (_td, mut world) = tmp_workspace();
    std::fs::write(world.repo.root.join("greeting.txt"), "hi there\n").unwrap();

    let model = MockModel::new()
        .script(MockResponse::tool_call(
            "read_file",
            json!({"path": "greeting.txt"}),
        ))
        .script(MockResponse::text("file says hi there"));

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task("read it"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Done { text, iters, .. } => {
            assert_eq!(iters, 2);
            assert!(text.as_deref().unwrap_or("").contains("hi there"));
        }
        other => panic!("expected Done, got {other:?}"),
    }
}

// ============================================================
// 3. Tool call result reaches the model's NEXT call
// ============================================================

#[tokio::test]
async fn tool_result_visible_to_model_next_iter() {
    let (_td, mut world) = tmp_workspace();
    std::fs::write(world.repo.root.join("data.txt"), "abc\n").unwrap();

    let model = Arc::new(
        MockModel::new()
            .script(MockResponse::tool_call("read_file", json!({"path": "data.txt"})))
            .script(MockResponse::text("done")),
    );
    let model_ref = model.clone();

    // Wrap Arc in a transparent Model impl so AgentLoop can own it but we keep a handle.
    struct Shared(Arc<MockModel>);
    #[async_trait::async_trait]
    impl harness_core::Model for Shared {
        async fn complete(
            &self,
            ctx: &Context,
        ) -> Result<harness_core::ModelOutput, harness_core::ModelError> {
            self.0.complete(ctx).await
        }
        fn info(&self) -> harness_core::ModelInfo { self.0.info() }
    }

    AgentLoop::new(Shared(model_ref.clone()))
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task("read"), &mut world, 5)
        .await
        .unwrap();

    let calls = model_ref.calls();
    assert_eq!(calls.len(), 2, "model should have been called twice");

    // First call: history has only the user task.
    assert_eq!(calls[0].history_summary.len(), 1);
    assert_eq!(calls[0].history_summary[0].role, "user");

    // Second call: history must include the tool-result.
    let second = &calls[1];
    let has_tool_result = second
        .history_summary
        .iter()
        .any(|h| h.role == "tool" && h.kinds.contains(&"tool-result"));
    assert!(
        has_tool_result,
        "tool result missing from second-call history: {:#?}",
        second.history_summary
    );
}

// ============================================================
// 4. PreToolUse hook Deny short-circuits the tool
// ============================================================

#[tokio::test]
async fn pre_tool_use_deny_blocks_dispatch() {
    let (_td, mut world) = tmp_workspace();
    std::fs::write(world.repo.root.join("secret.txt"), "PASSWORD\n").unwrap();

    struct DenyReadFile {
        denials: Arc<AtomicU32>,
    }
    impl harness_core::Hook for DenyReadFile {
        fn name(&self) -> &str { "deny-read-file" }
        fn matches(&self, ev: &Event<'_>) -> bool {
            matches!(ev, Event::PreToolUse { action } if action.tool == "read_file")
        }
        fn fire(&self, _ev: &Event<'_>, _w: &mut World) -> HookOutcome {
            self.denials.fetch_add(1, Ordering::SeqCst);
            HookOutcome::Deny { reason: "no secrets".into() }
        }
    }

    let denials = Arc::new(AtomicU32::new(0));
    let model = MockModel::new()
        .script(MockResponse::tool_call(
            "read_file",
            json!({"path": "secret.txt"}),
        ))
        .script(MockResponse::text("nothing read"));

    AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_hook(Arc::new(DenyReadFile { denials: denials.clone() }))
        .run_with_max_iters(task("try to read"), &mut world, 5)
        .await
        .unwrap();

    assert_eq!(denials.load(Ordering::SeqCst), 1);
}

// ============================================================
// 5. Sensor signals reach the model's next call
// ============================================================

#[tokio::test]
async fn sensor_signals_feed_back_to_model() {
    let (_td, mut world) = tmp_workspace();

    /// Sensor that always returns one blocking signal.
    struct AlwaysComplain {
        id: SensorId,
    }
    #[async_trait::async_trait]
    impl harness_core::Sensor for AlwaysComplain {
        fn id(&self)    -> &SensorId  { &self.id }
        fn kind(&self)  -> Execution  { Execution::Computational }
        fn stage(&self) -> Stage      { Stage::SelfCorrect }
        async fn observe(
            &self,
            _: &Action,
            _: &World,
        ) -> Result<Vec<Signal>, harness_core::SensorError> {
            Ok(vec![Signal {
                severity:   Severity::Block,
                origin:     "always-complain".into(),
                message:    "this is bad".into(),
                agent_hint: Some("undo it".into()),
                auto_fix:   None,
                location:   None,
            }])
        }
    }

    let model = Arc::new(
        MockModel::new()
            .script(MockResponse::tool_call(
                "write_file",
                json!({"path": "x.txt", "content": "bad"}),
            ))
            .script(MockResponse::text("acknowledged feedback")),
    );
    let model_ref = model.clone();

    struct Shared(Arc<MockModel>);
    #[async_trait::async_trait]
    impl harness_core::Model for Shared {
        async fn complete(&self, ctx: &Context) -> Result<harness_core::ModelOutput, harness_core::ModelError> {
            self.0.complete(ctx).await
        }
        fn info(&self) -> harness_core::ModelInfo { self.0.info() }
    }

    AgentLoop::new(Shared(model.clone()))
        .with_tool(Arc::new(WriteFile))
        .with_sensor(Arc::new(AlwaysComplain { id: "always-complain".into() }))
        .run_with_max_iters(task("write bad file"), &mut world, 5)
        .await
        .unwrap();

    let calls = model_ref.calls();
    assert_eq!(calls.len(), 2);
    let feedback_seen = calls[1]
        .history_summary
        .iter()
        .any(|h| h.kinds.contains(&"feedback"));
    assert!(
        feedback_seen,
        "sensor feedback missing from second-call history: {:#?}",
        calls[1].history_summary
    );
}

// ============================================================
// 6. Guide injects content before the first model call
// ============================================================

#[tokio::test]
async fn guide_applies_before_first_model_call() {
    let (_td, mut world) = tmp_workspace();

    struct InjectGuide {
        id: GuideId,
        scope: GuideScope,
    }
    #[async_trait::async_trait]
    impl harness_core::Guide for InjectGuide {
        fn id(&self) -> &GuideId { &self.id }
        fn kind(&self) -> Execution { Execution::Inferential }
        fn scope(&self) -> &GuideScope { &self.scope }
        async fn apply(
            &self,
            ctx: &mut Context,
            _w: &World,
        ) -> Result<(), harness_core::GuideError> {
            ctx.guides.push(Block::Text("INJECTED-BY-GUIDE".into()));
            Ok(())
        }
    }

    let model = Arc::new(MockModel::new().script(MockResponse::text("ok")));
    let model_ref = model.clone();
    struct Shared(Arc<MockModel>);
    #[async_trait::async_trait]
    impl harness_core::Model for Shared {
        async fn complete(&self, ctx: &Context) -> Result<harness_core::ModelOutput, harness_core::ModelError> {
            // We can verify directly here that guide content is on `ctx`.
            let injected = ctx
                .guides
                .iter()
                .any(|b| matches!(b, Block::Text(t) if t == "INJECTED-BY-GUIDE"));
            assert!(injected, "guide content missing from ctx.guides at model.complete()");
            self.0.complete(ctx).await
        }
        fn info(&self) -> harness_core::ModelInfo { self.0.info() }
    }

    AgentLoop::new(Shared(model.clone()))
        .with_guide(Arc::new(InjectGuide {
            id: "test-guide".into(),
            scope: GuideScope::Always,
        }))
        .run_with_max_iters(task("anything"), &mut world, 3)
        .await
        .unwrap();
    assert_eq!(model_ref.call_count(), 1);
}

// ============================================================
// 7. Budget-exhausted outcome when the model keeps calling tools
// ============================================================

#[tokio::test]
async fn budget_exhausted_when_model_loops_on_tool_calls() {
    let (_td, mut world) = tmp_workspace();
    std::fs::write(world.repo.root.join("a.txt"), "x").unwrap();

    let mut model = MockModel::new();
    // 10 identical tool calls, never returns text
    for _ in 0..10 {
        model = model.script(MockResponse::tool_call(
            "read_file",
            json!({"path": "a.txt"}),
        ));
    }

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task("loop forever"), &mut world, 3)
        .await
        .unwrap();
    assert!(matches!(outcome, Outcome::BudgetExhausted { iters: 3, .. }));
}

// ============================================================
// 8. Auto-fix patch (ReplaceFile) is actually applied
// ============================================================

#[tokio::test]
async fn auto_fix_replace_file_writes_to_disk() {
    let (_td, mut world) = tmp_workspace();
    std::fs::write(world.repo.root.join("target.txt"), "old\n").unwrap();

    struct PatchingSensor;
    #[async_trait::async_trait]
    impl harness_core::Sensor for PatchingSensor {
        fn id(&self) -> &SensorId {
            static ID: once_cell::sync::Lazy<SensorId> =
                once_cell::sync::Lazy::new(|| "patcher".into());
            &ID
        }
        fn kind(&self)  -> Execution  { Execution::Computational }
        fn stage(&self) -> Stage      { Stage::SelfCorrect }
        async fn observe(
            &self,
            _a: &Action,
            _w: &World,
        ) -> Result<Vec<Signal>, harness_core::SensorError> {
            Ok(vec![Signal {
                severity:   Severity::Hint,
                origin:     "patcher".into(),
                message:    "applying fix".into(),
                agent_hint: None,
                auto_fix:   Some(harness_core::FixPatch::ReplaceFile {
                    path:    "target.txt".into(),
                    content: "NEW CONTENT\n".into(),
                }),
                location:   None,
            }])
        }
    }

    let model = MockModel::new()
        .script(MockResponse::tool_call(
            "write_file",
            json!({"path": "noop.txt", "content": "noop"}),
        ))
        .script(MockResponse::text("done"));

    let root_for_check = world.repo.root.clone();
    AgentLoop::new(model)
        .with_tool(Arc::new(WriteFile))
        .with_sensor(Arc::new(PatchingSensor))
        .run_with_max_iters(task("trigger sensor"), &mut world, 5)
        .await
        .unwrap();

    let contents = std::fs::read_to_string(root_for_check.join("target.txt")).unwrap();
    assert_eq!(contents, "NEW CONTENT\n");
}

// ============================================================
// 9. Compaction fires when budget is exceeded
// ============================================================

#[tokio::test]
async fn compaction_runs_at_top_of_iter_when_over_budget() {
    let (_td, mut world) = tmp_workspace();

    /// Compactor that records every stage it was asked to run.
    struct RecordingCompactor {
        triggered: Mutex<Vec<harness_core::CompactionStage>>,
    }
    #[async_trait::async_trait]
    impl harness_core::Compactor for RecordingCompactor {
        fn budget(&self, _ctx: &Context) -> harness_core::Budget {
            // Always report 99% to force every stage to fire.
            harness_core::Budget { used: 99, window: 100 }
        }
        async fn compact(
            &self,
            stage: harness_core::CompactionStage,
            _ctx: &mut Context,
        ) -> Result<(), harness_core::CompactError> {
            self.triggered.lock().unwrap().push(stage);
            Ok(())
        }
    }

    let recorder = Arc::new(RecordingCompactor { triggered: Mutex::new(Vec::new()) });
    let model = MockModel::new().script(MockResponse::text("done"));

    AgentLoop::new(model)
        .with_compactor(recorder.clone())
        .run_with_max_iters(task("anything"), &mut world, 3)
        .await
        .unwrap();

    let stages = recorder.triggered.lock().unwrap().clone();
    assert_eq!(stages.len(), 5, "expected all 5 stages at 99% budget");
    use harness_core::CompactionStage::*;
    assert_eq!(
        stages,
        vec![BudgetReduce, Snip, Microcompact, ContextCollapse, AutoCompact]
    );
}

// ============================================================
// 10. activate_skill — model decides which skill to load (manual flow)
// ============================================================
//
// (We don't have a built-in activate_skill tool yet, but exercise the
// SkillRegistry catalogue rendering since that's the surface the model would
// use to decide.)

#[tokio::test]
async fn skill_registry_catalogue_is_readable_at_session_start() {
    use harness_core::SkillManifest;
    use harness_skills::SkillRegistry;
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    struct DummySkill(SkillManifest);
    impl Skill for DummySkill {
        fn manifest(&self) -> &SkillManifest { &self.0 }
        fn body(&self) -> Cow<'_, str> { Cow::Borrowed("body") }
    }

    let mut reg = SkillRegistry::new();
    reg.insert(Arc::new(DummySkill(SkillManifest {
        name: "alpha".into(),
        description: "first skill".into(),
        license: None,
        compatibility: None,
        metadata: BTreeMap::new(),
        allowed_tools: None,
    })))
    .unwrap();
    reg.insert(Arc::new(DummySkill(SkillManifest {
        name: "beta".into(),
        description: "second skill".into(),
        license: None,
        compatibility: None,
        metadata: BTreeMap::new(),
        allowed_tools: None,
    })))
    .unwrap();

    let cat = reg.catalogue();
    assert!(cat.contains("- alpha:"));
    assert!(cat.contains("- beta:"));
    let pos_a = cat.find("- alpha:").unwrap();
    let pos_b = cat.find("- beta:").unwrap();
    assert!(pos_a < pos_b, "catalogue should be alphabetical");
}
