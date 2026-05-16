//! ReAct agent loop with self-correction.
//!
//! Minimal v0.0.1 implementation:
//! - Applies guides once at the start.
//! - Sends `Context` (with `tools`) to the model.
//! - Dispatches each returned tool call via [`ToolRegistry`].
//! - Runs `Sensor::SelfCorrect` sensors after each action; auto-fix patches are
//!   applied directly to the world, blocking signals are fed back to the model.
//! - Stops when the model returns no tool calls, or when `policy.max_iters` is hit.

pub mod registry;

pub use registry::*;

use harness_core::{
    Action, Block, Context, Guide, HarnessError, Model, Sensor, SignalSet, Stage, Task, ToolResult,
    Turn, TurnRole, World,
};
use std::sync::Arc;

/// Where a run finished.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Model returned text with no tool calls.
    Done { text: Option<String>, iters: u32 },
    /// Policy budget exhausted.
    BudgetExhausted { iters: u32 },
}

/// The agent loop.
pub struct AgentLoop<M: Model> {
    pub model:   M,
    pub tools:   ToolRegistry,
    pub guides:  Vec<Arc<dyn Guide>>,
    pub sensors: Vec<Arc<dyn Sensor>>,
}

impl<M: Model> AgentLoop<M> {
    pub fn new(model: M) -> Self {
        Self { model, tools: ToolRegistry::new(), guides: Vec::new(), sensors: Vec::new() }
    }

    pub fn with_tool(mut self, t: Arc<dyn harness_core::Tool>) -> Self {
        self.tools.insert(t);
        self
    }

    pub fn with_guide(mut self, g: Arc<dyn Guide>) -> Self {
        self.guides.push(g);
        self
    }

    pub fn with_sensor(mut self, s: Arc<dyn Sensor>) -> Self {
        self.sensors.push(s);
        self
    }

    pub async fn run(&self, task: Task, world: &mut World) -> Result<Outcome, HarnessError> {
        let max = harness_core::Policy::default().max_iters;
        self.run_with_max_iters(task, world, max).await
    }

    pub async fn run_with_max_iters(
        &self,
        task: Task,
        world: &mut World,
        max_iters: u32,
    ) -> Result<Outcome, HarnessError> {
        let mut ctx = Context::new(task);
        ctx.policy.max_iters = max_iters;

        // Attach every registered tool's schema so the model knows what's callable.
        ctx.tools = self.tools.schemas();

        // Apply guides.
        for g in &self.guides {
            if g.scope().matches(&ctx.task) {
                g.apply(&mut ctx, world).await?;
            }
        }

        // The user's task becomes the first user message — exit after pushing.
        ctx.history.push(Turn {
            role:   TurnRole::User,
            blocks: vec![Block::Text(ctx.task.description.clone())],
        });

        for iter in 0..ctx.policy.max_iters {
            tracing::debug!(iter, "agent loop step");
            let out = self.model.complete(&ctx).await?;
            ctx.push_model_output(&out);

            if out.tool_calls.is_empty() {
                return Ok(Outcome::Done { text: out.text, iters: iter + 1 });
            }

            for call in &out.tool_calls {
                let action = Action {
                    tool:    call.name.clone(),
                    call_id: call.id.clone(),
                    args:    call.args.clone(),
                };
                let result = match self.tools.dispatch(&action, world).await {
                    Ok(r) => r,
                    Err(e) => ToolResult {
                        ok: false,
                        content: serde_json::json!({"error": e.to_string()}),
                        trace: None,
                    },
                };

                // tool result back into history
                ctx.history.push(Turn {
                    role:   TurnRole::Tool,
                    blocks: vec![Block::ToolResult {
                        call_id: action.call_id.clone(),
                        content: result.content.clone(),
                    }],
                });

                // run self-correct sensors
                let mut all_signals = Vec::new();
                for s in &self.sensors {
                    if s.stage() != Stage::SelfCorrect { continue; }
                    let sigs = s.observe(&action, world).await.unwrap_or_else(|e| {
                        tracing::warn!(?e, "sensor failed");
                        Vec::new()
                    });
                    all_signals.extend(sigs);
                }
                if !all_signals.is_empty() {
                    let bundle = SignalSet::new(all_signals);
                    let (patches, remaining) = bundle.partition_auto_fix();
                    // For now: surface patches to the model as text feedback
                    // (real apply happens once we have a working tree-aware patcher).
                    if !patches.is_empty() {
                        let summary = format!("auto-fix patches available: {patches:?}");
                        ctx.push_feedback(vec![harness_core::Signal {
                            severity:   harness_core::Severity::Hint,
                            origin:     "auto-fix".into(),
                            message:    summary,
                            agent_hint: None,
                            auto_fix:   None,
                            location:   None,
                        }]);
                    }
                    if remaining.has_blocking() {
                        ctx.push_feedback(remaining.signals);
                    }
                }
            }
        }
        Ok(Outcome::BudgetExhausted { iters: ctx.policy.max_iters })
    }
}
