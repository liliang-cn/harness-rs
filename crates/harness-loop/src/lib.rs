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

use harness_compactor::DefaultCompactor;
use harness_core::{
    Action, Block, Compactor, Context, Event, Guide, HarnessError, HookOutcome, Model, Sensor,
    SessionSource, SignalSet, Stage, Task, ToolResult, Turn, TurnRole, World,
};
use harness_hooks::HookBus;
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
    pub model:      M,
    pub tools:      ToolRegistry,
    pub guides:     Vec<Arc<dyn Guide>>,
    pub sensors:    Vec<Arc<dyn Sensor>>,
    pub hooks:      HookBus,
    pub compactor:  Arc<dyn Compactor>,
}

impl<M: Model> AgentLoop<M> {
    pub fn new(model: M) -> Self {
        Self {
            model,
            tools:     ToolRegistry::new(),
            guides:    Vec::new(),
            sensors:   Vec::new(),
            hooks:     HookBus::new(),
            compactor: Arc::new(DefaultCompactor::new()),
        }
    }

    pub fn with_compactor(mut self, c: Arc<dyn Compactor>) -> Self {
        self.compactor = c;
        self
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

    pub fn with_hook(mut self, h: Arc<dyn harness_core::Hook>) -> Self {
        self.hooks.register(h);
        self
    }

    /// Pull in every `#[hook]`-registered hook.
    pub fn with_macro_hooks(mut self) -> Self {
        self.hooks = self.hooks.with_macro_hooks_take();
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
        ctx.tools = self.tools.schemas();

        self.hooks.fire(&Event::SessionStart { source: SessionSource::Startup }, world);

        for g in &self.guides {
            if g.scope().matches(&ctx.task) {
                self.hooks.fire(&Event::PreGuide { guide: g.id() }, world);
                g.apply(&mut ctx, world).await?;
                self.hooks.fire(&Event::PostGuide { guide: g.id() }, world);
            }
        }

        ctx.history.push(Turn {
            role:   TurnRole::User,
            blocks: vec![Block::Text(ctx.task.description.clone())],
        });

        for iter in 0..ctx.policy.max_iters {
            self.hooks.fire(&Event::Heartbeat { iter }, world);

            // Compaction: run every stage required by current budget.
            let stages = self.compactor.budget(&ctx).required_stages();
            for stage in stages {
                self.hooks.fire(&Event::PreCompact { stage }, world);
                self.compactor.compact(stage, &mut ctx).await?;
                self.hooks.fire(&Event::PostCompact { stage }, world);
            }

            self.hooks.fire(&Event::PreModel { ctx: &ctx }, world);
            let out = self.model.complete(&ctx).await?;
            self.hooks.fire(&Event::PostModel { out: &out }, world);
            ctx.push_model_output(&out);

            if out.tool_calls.is_empty() {
                self.hooks.fire(&Event::TaskCompleted, world);
                self.hooks.fire(&Event::SessionEnd, world);
                return Ok(Outcome::Done { text: out.text, iters: iter + 1 });
            }

            for call in &out.tool_calls {
                let action = Action {
                    tool:    call.name.clone(),
                    call_id: call.id.clone(),
                    args:    call.args.clone(),
                };

                // PreToolUse hook can deny destructive actions
                if let HookOutcome::Deny { reason } =
                    self.hooks.fire(&Event::PreToolUse { action: &action }, world)
                {
                    ctx.history.push(Turn {
                        role: TurnRole::Tool,
                        blocks: vec![Block::ToolResult {
                            call_id: action.call_id.clone(),
                            content: serde_json::json!({
                                "ok": false,
                                "denied_by_hook": reason,
                            }),
                        }],
                    });
                    continue;
                }

                let result = match self.tools.dispatch(&action, world).await {
                    Ok(r) => r,
                    Err(e) => ToolResult {
                        ok: false,
                        content: serde_json::json!({"error": e.to_string()}),
                        trace: None,
                    },
                };
                self.hooks.fire(&Event::PostToolUse { action: &action, result: &result }, world);

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
                    self.hooks.fire(&Event::PreSensor { sensor: s.id() }, world);
                    let sigs = s.observe(&action, world).await.unwrap_or_else(|e| {
                        tracing::warn!(?e, "sensor failed");
                        Vec::new()
                    });
                    self.hooks.fire(&Event::PostSensor { sensor: s.id(), signals: &sigs }, world);
                    all_signals.extend(sigs);
                }
                if !all_signals.is_empty() {
                    let bundle = SignalSet::new(all_signals);
                    let (patches, remaining) = bundle.partition_auto_fix();

                    let applied = apply_patches(&patches, world).await;
                    if !applied.is_empty() {
                        ctx.push_feedback(vec![harness_core::Signal {
                            severity:   harness_core::Severity::Hint,
                            origin:     "auto-fix".into(),
                            message:    format!("applied {} auto-fix patch(es): {applied:?}", applied.len()),
                            agent_hint: Some("re-check the affected files before continuing".into()),
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
        self.hooks.fire(&Event::SessionEnd, world);
        Ok(Outcome::BudgetExhausted { iters: ctx.policy.max_iters })
    }
}

/// Apply auto-fix patches; return short descriptions of those that succeeded.
async fn apply_patches(
    patches: &[harness_core::FixPatch],
    world: &mut World,
) -> Vec<String> {
    use harness_core::FixPatch;
    let mut applied = Vec::new();
    for p in patches {
        match p {
            FixPatch::ReplaceFile { path, content } => {
                let abs = world.repo.root.join(path);
                if let Some(parent) = abs.parent() {
                    let _ = tokio::fs::create_dir_all(parent).await;
                }
                if tokio::fs::write(&abs, content).await.is_ok() {
                    applied.push(format!("replaced {}", path.display()));
                }
            }
            FixPatch::UnifiedDiff { diff } => {
                // Naive: pipe diff through `patch -p0` via the world runner.
                // Anything more sophisticated belongs in a dedicated patch-engine crate.
                use tokio::io::AsyncWriteExt;
                let tmp = world
                    .repo
                    .root
                    .join(format!(".harness-patch-{}.diff", world.clock.now_ms()));
                if let Ok(mut f) = tokio::fs::File::create(&tmp).await {
                    if f.write_all(diff.as_bytes()).await.is_ok()
                        && let Ok(out) = world
                            .runner
                            .exec(
                                "patch",
                                &["-p0", "-i", tmp.to_string_lossy().as_ref()],
                                Some(world.repo.root.as_path()),
                            )
                            .await
                        && out.status == 0
                    {
                        applied.push("unified diff applied".into());
                    }
                    let _ = tokio::fs::remove_file(&tmp).await;
                }
            }
            FixPatch::RunCommand { program, args, cwd } => {
                let cwd_ref = cwd.as_deref().unwrap_or(world.repo.root.as_path());
                let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
                if let Ok(out) = world.runner.exec(program, &args_ref, Some(cwd_ref)).await
                    && out.status == 0
                {
                    applied.push(format!("ran `{program} {}`", args.join(" ")));
                }
            }
        }
    }
    applied
}
