//! with_learning_loop forks a review subagent at SessionEnd that writes a skill.

use async_trait::async_trait;
use harness_context::default_world;
use harness_core::{Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, Tool, ToolCall, Usage};
use harness_loop::{AgentLoop, LearningConfig};
use harness_tools_skills::SkillManageTool;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

const SKILL_MD: &str = "---\nname: learned-skill\ndescription: A thing learned.\n---\n# Learned\n1. do it\n";

fn mi() -> ModelInfo {
    ModelInfo { handle: "mock".into(), provider: "mock".into(), model: "mock".into(), context_window: 8192, input_cost_usd_per_million_tokens: None, output_cost_usd_per_million_tokens: None, supports_tool_use: true, supports_streaming: false }
}

struct MainModel { turn: AtomicU32 }
#[async_trait]
impl Model for MainModel {
    async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
        let t = self.turn.fetch_add(1, Ordering::SeqCst);
        if t < 2 {
            Ok(ModelOutput { text: Some("work".into()), tool_calls: vec![ToolCall { id: format!("c{t}"), name: "noop".into(), args: serde_json::json!({}) }], usage: Usage::default(), stop_reason: StopReason::ToolUse, reasoning: None })
        } else {
            Ok(ModelOutput { text: Some("done".into()), tool_calls: vec![], usage: Usage::default(), stop_reason: StopReason::EndTurn, reasoning: None })
        }
    }
    fn info(&self) -> ModelInfo { mi() }
}

struct ReviewModel { turn: AtomicU32, fail: bool }
#[async_trait]
impl Model for ReviewModel {
    async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
        if self.fail { return Err(ModelError::Transport("boom".into())); }
        let t = self.turn.fetch_add(1, Ordering::SeqCst);
        if t == 0 {
            Ok(ModelOutput { text: None, tool_calls: vec![ToolCall { id: "s1".into(), name: "skill_manage".into(), args: serde_json::json!({"action":"create","name":"learned-skill","content": SKILL_MD}) }], usage: Usage::default(), stop_reason: StopReason::ToolUse, reasoning: None })
        } else {
            Ok(ModelOutput { text: Some("reviewed".into()), tool_calls: vec![], usage: Usage::default(), stop_reason: StopReason::EndTurn, reasoning: None })
        }
    }
    fn info(&self) -> ModelInfo { mi() }
}

struct Noop { schema: harness_core::ToolSchema }
impl Noop { fn new() -> Self { Self { schema: harness_core::ToolSchema { name: "noop".into(), description: "noop".into(), input: serde_json::json!({"type":"object"}) } } } }
#[async_trait]
impl Tool for Noop {
    fn name(&self) -> &str { &self.schema.name }
    fn schema(&self) -> &harness_core::ToolSchema { &self.schema }
    fn risk(&self) -> harness_core::ToolRisk { harness_core::ToolRisk::ReadOnly }
    async fn invoke(&self, _a: serde_json::Value, _w: &mut harness_core::World) -> Result<harness_core::ToolResult, harness_core::ToolError> {
        Ok(harness_core::ToolResult { ok: true, content: serde_json::json!({}), trace: None })
    }
}

fn skills_dir() -> std::path::PathBuf {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("harness-learn-{}-{n}", std::process::id()))
}
fn task() -> harness_core::Task { harness_core::Task { description: "do real work".into(), source: None, deadline: None } }

#[tokio::test]
async fn review_writes_a_skill_after_enough_tool_calls() {
    let dir = skills_dir();
    let review_model: Arc<dyn Model> = Arc::new(ReviewModel { turn: AtomicU32::new(0), fail: false });
    let cfg = LearningConfig::new(review_model)
        .with_tool(Arc::new(SkillManageTool::new(&dir)))
        .with_nudge_interval(2);
    let loop_ = AgentLoop::new(MainModel { turn: AtomicU32::new(0) })
        .with_tool(Arc::new(Noop::new()))
        .with_learning_loop(cfg);
    let mut world = default_world(".");
    let outcome = loop_.run(task(), &mut world).await.unwrap();
    assert!(matches!(outcome, harness_loop::Outcome::Done { .. }));
    assert!(dir.join("learned-skill").join("SKILL.md").exists(), "review should have written the skill");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn below_threshold_does_not_review() {
    let dir = skills_dir();
    let review_model: Arc<dyn Model> = Arc::new(ReviewModel { turn: AtomicU32::new(0), fail: false });
    let cfg = LearningConfig::new(review_model)
        .with_tool(Arc::new(SkillManageTool::new(&dir)))
        .with_nudge_interval(5);
    let loop_ = AgentLoop::new(MainModel { turn: AtomicU32::new(0) })
        .with_tool(Arc::new(Noop::new()))
        .with_learning_loop(cfg);
    let mut world = default_world(".");
    let _ = loop_.run(task(), &mut world).await.unwrap();
    assert!(!dir.join("learned-skill").exists(), "no review below threshold");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn review_failure_is_best_effort() {
    let dir = skills_dir();
    let review_model: Arc<dyn Model> = Arc::new(ReviewModel { turn: AtomicU32::new(0), fail: true });
    let cfg = LearningConfig::new(review_model)
        .with_tool(Arc::new(SkillManageTool::new(&dir)))
        .with_nudge_interval(2);
    let loop_ = AgentLoop::new(MainModel { turn: AtomicU32::new(0) })
        .with_tool(Arc::new(Noop::new()))
        .with_learning_loop(cfg);
    let mut world = default_world(".");
    let outcome = loop_.run(task(), &mut world).await;
    assert!(outcome.is_ok(), "review failure must not fail the run");
    let _ = std::fs::remove_dir_all(&dir);
}
