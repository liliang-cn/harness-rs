//! ReAct agent loop with self-correction.
//!
//! Minimal v0.0.1 implementation:
//! - Applies guides once at the start.
//! - Sends `Context` (with `tools`) to the model.
//! - Dispatches each returned tool call via [`ToolRegistry`].
//! - Runs `Sensor::SelfCorrect` sensors after each action; auto-fix patches are
//!   applied directly to the world, blocking signals are fed back to the model.
//! - Stops when the model returns no tool calls, or when `policy.max_iters` is hit.

pub mod memory_layer;
pub mod profile_guide;
pub mod registry;
pub mod replay;
pub mod subagent;

pub use memory_layer::*;
pub use profile_guide::*;
pub use registry::*;
pub use replay::*;
pub use subagent::*;

use harness_compactor::DefaultCompactor;
use harness_core::{
    Action, Block, Compactor, Context, Event, Guide, HarnessError, HookOutcome, Model, Sensor,
    SessionSource, SignalSet, Stage, Task, ToolResult, Turn, TurnRole, World,
};
use harness_hooks::HookBus;
use std::sync::Arc;

/// Where a run finished. Marked `#[non_exhaustive]` so future fields don't break
/// downstream matches — always include `..` when destructuring.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Model returned text with no tool calls (natural end).
    #[non_exhaustive]
    Done {
        text: Option<String>,
        iters: u32,
        tools_called: u32,
        usage: harness_core::Usage,
    },
    /// Policy budget exhausted before the model stopped requesting tools.
    /// Carries everything we know so the caller can recover partial work
    /// (saved notes, files written by tools, the last assistant text, etc.)
    /// instead of seeing a single bare "budget out" string.
    #[non_exhaustive]
    BudgetExhausted {
        iters: u32,
        last_text: Option<String>,
        tools_called: u32,
        usage: harness_core::Usage,
    },
}

/// The agent loop.
pub struct AgentLoop<M: Model> {
    pub model: M,
    pub tools: ToolRegistry,
    pub guides: Vec<Arc<dyn Guide>>,
    pub sensors: Vec<Arc<dyn Sensor>>,
    pub hooks: HookBus,
    pub compactor: Arc<dyn Compactor>,
}

impl<M: Model> AgentLoop<M> {
    pub fn new(model: M) -> Self {
        Self {
            model,
            tools: ToolRegistry::new(),
            guides: Vec::new(),
            sensors: Vec::new(),
            hooks: HookBus::new(),
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
        self.run_with_seed_history(task, Vec::new(), world, max_iters)
            .await
    }

    /// Like `run_with_max_iters` but seeds `ctx.history` with `seed` **before**
    /// the current user task is appended. Use this for multi-turn REPLs so
    /// prior conversation lives in `ctx.history` (where the Compactor can see
    /// it) instead of being concatenated into `task.description` (where it
    /// previously bypassed compaction entirely — see audit #2).
    pub async fn run_with_seed_history(
        &self,
        task: Task,
        seed: Vec<Turn>,
        world: &mut World,
        max_iters: u32,
    ) -> Result<Outcome, HarnessError> {
        let mut ctx = Context::new(task);
        ctx.policy.max_iters = max_iters;
        ctx.tools = self.tools.schemas();
        ctx.history = seed;

        self.hooks.fire(
            &Event::SessionStart {
                source: SessionSource::Startup,
            },
            world,
        );

        for g in &self.guides {
            if g.scope().matches(&ctx.task) {
                self.hooks.fire(&Event::PreGuide { guide: g.id() }, world);
                g.apply(&mut ctx, world).await?;
                self.hooks.fire(&Event::PostGuide { guide: g.id() }, world);
            }
        }

        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(ctx.task.description.clone())],
        });

        // Running totals — surface to caller even on BudgetExhausted.
        let mut tools_called: u32 = 0;
        let mut total_usage = harness_core::Usage::default();
        let mut last_text: Option<String> = None;

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
            // Accumulate usage even if the run later exhausts budget.
            total_usage.input_tokens += out.usage.input_tokens;
            total_usage.output_tokens += out.usage.output_tokens;
            total_usage.cached_input_tokens += out.usage.cached_input_tokens;
            if let Some(t) = &out.text {
                last_text = Some(t.clone());
            }
            ctx.push_model_output(&out);

            if out.tool_calls.is_empty() {
                self.hooks.fire(&Event::TaskCompleted, world);
                self.hooks.fire(&Event::SessionEnd, world);
                return Ok(Outcome::Done {
                    text: out.text,
                    iters: iter + 1,
                    tools_called,
                    usage: total_usage,
                });
            }

            for call in &out.tool_calls {
                let action = Action {
                    tool: call.name.clone(),
                    call_id: call.id.clone(),
                    args: call.args.clone(),
                };

                // PreToolUse hook can deny destructive actions
                if let HookOutcome::Deny { reason } = self
                    .hooks
                    .fire(&Event::PreToolUse { action: &action }, world)
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
                tools_called += 1;
                self.hooks.fire(
                    &Event::PostToolUse {
                        action: &action,
                        result: &result,
                    },
                    world,
                );

                ctx.history.push(Turn {
                    role: TurnRole::Tool,
                    blocks: vec![Block::ToolResult {
                        call_id: action.call_id.clone(),
                        content: result.content.clone(),
                    }],
                });

                // run self-correct sensors
                let mut all_signals = Vec::new();
                for s in &self.sensors {
                    if s.stage() != Stage::SelfCorrect {
                        continue;
                    }
                    self.hooks.fire(&Event::PreSensor { sensor: s.id() }, world);
                    let sigs = s.observe(&action, world).await.unwrap_or_else(|e| {
                        tracing::warn!(?e, "sensor failed");
                        Vec::new()
                    });
                    self.hooks.fire(
                        &Event::PostSensor {
                            sensor: s.id(),
                            signals: &sigs,
                        },
                        world,
                    );
                    all_signals.extend(sigs);
                }
                if !all_signals.is_empty() {
                    let bundle = SignalSet::new(all_signals);
                    let (patches, remaining) = bundle.partition_auto_fix();

                    // audit #7: each patch goes through PreAutoFix.
                    // Hooks can Deny (skip silently). Default safelist on
                    // RunCommand catches the obvious misuses with no hook.
                    let approved: Vec<harness_core::FixPatch> = patches.into_iter().filter(|p| {
                        if !is_default_safe_fix(p) {
                            tracing::warn!(?p, "auto-fix rejected by default safelist (use PreAutoFix hook to override)");
                            self.hooks.fire(&Event::PostAutoFix { patch: p, applied: false }, world);
                            return false;
                        }
                        match self.hooks.fire(&Event::PreAutoFix { patch: p }, world) {
                            HookOutcome::Deny { reason } => {
                                tracing::warn!(?p, %reason, "auto-fix denied by hook");
                                self.hooks.fire(&Event::PostAutoFix { patch: p, applied: false }, world);
                                false
                            }
                            _ => true,
                        }
                    }).collect();

                    let applied = apply_patches(&approved, world).await;
                    // Emit PostAutoFix for each approved patch with the application result.
                    for (i, p) in approved.iter().enumerate() {
                        self.hooks.fire(
                            &Event::PostAutoFix {
                                patch: p,
                                applied: i < applied.len(),
                            },
                            world,
                        );
                    }
                    if !applied.is_empty() {
                        ctx.push_feedback(vec![harness_core::Signal {
                            severity: harness_core::Severity::Hint,
                            origin: "auto-fix".into(),
                            message: format!(
                                "applied {} auto-fix patch(es): {applied:?}",
                                applied.len()
                            ),
                            agent_hint: Some(
                                "re-check the affected files before continuing".into(),
                            ),
                            auto_fix: None,
                            location: None,
                        }]);
                    }
                    if remaining.has_blocking() {
                        ctx.push_feedback(remaining.signals);
                    }
                }
            }
        }
        // ── Budget exhausted ─────────────────────────────────────────
        // Force a final synthesis pass with tools DISABLED. Otherwise the
        // model often spins on tool calls right up to the budget cap and
        // never emits a text conclusion, leaving the caller with nothing
        // but `last_text` from some earlier intermediate turn (or None).
        //
        // The synthesis call is "free" — it costs one extra model call
        // beyond max_iters but doesn't count toward `iters`. The result
        // lands in `last_text` so callers display it as the answer.
        let synthesised = self
            .force_final_synthesis(&mut ctx, world, &mut total_usage)
            .await;
        if let Some(t) = synthesised {
            last_text = Some(t);
        }

        self.hooks.fire(&Event::SessionEnd, world);
        Ok(Outcome::BudgetExhausted {
            iters: ctx.policy.max_iters,
            last_text,
            tools_called,
            usage: total_usage,
        })
    }

    /// One final model call with tools removed, asking it to write the
    /// best-effort conclusion from whatever it has already gathered.
    ///
    /// Errors from the model are swallowed — observability is best-effort
    /// here, and a transport blip during synthesis should not turn a
    /// near-complete run into a hard failure.
    async fn force_final_synthesis(
        &self,
        ctx: &mut Context,
        world: &mut World,
        total_usage: &mut harness_core::Usage,
    ) -> Option<String> {
        const SYNTHESIS_PROMPT: &str = "[system: iteration budget exhausted] \
            You have run out of tool-calling iterations. Write your final answer \
            NOW using only the tool results already in this conversation. Do not \
            request more tools. Mark facts you could not verify as UNKNOWN. \
            Include source URLs for every claim that is not UNKNOWN.";

        // Signal to any observer (LiveProgressHook, SessionRecorder, custom
        // hooks) that we've used 100% of the budget and are about to force
        // synthesis. Pre-existing `BudgetWarning` event was unused; this is
        // its natural home.
        self.hooks.fire(&Event::BudgetWarning { ratio: 1.0 }, world);

        // Snapshot + clear tool schemas so the model has no choice but text.
        let saved_tools = std::mem::take(&mut ctx.tools);
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(SYNTHESIS_PROMPT.into())],
        });

        self.hooks.fire(&Event::PreModel { ctx }, world);
        let result = self.model.complete(ctx).await;
        ctx.tools = saved_tools;

        match result {
            Ok(out) => {
                self.hooks.fire(&Event::PostModel { out: &out }, world);
                total_usage.input_tokens += out.usage.input_tokens;
                total_usage.output_tokens += out.usage.output_tokens;
                total_usage.cached_input_tokens += out.usage.cached_input_tokens;
                ctx.push_model_output(&out);
                out.text
            }
            Err(_) => None,
        }
    }
}

/// Audit #7: default safelist for `FixPatch::RunCommand`.
///
/// Sensors emitting `RunCommand` patches would otherwise be a silent
/// arbitrary-code-execution channel. We restrict the *program* by name to a
/// short list of well-known, side-effect-bounded formatters/fixers. Anything
/// else returns false and the patch is rejected (write your own `PreAutoFix`
/// hook returning `HookOutcome::Allow` to widen the policy).
///
/// `ReplaceFile` and `UnifiedDiff` are not restricted here — they only touch
/// files inside the workspace and are covered by the symlink-safe path
/// resolution in `harness-tools-fs`.
pub fn is_default_safe_fix(patch: &harness_core::FixPatch) -> bool {
    use harness_core::FixPatch;
    match patch {
        FixPatch::ReplaceFile { .. } | FixPatch::UnifiedDiff { .. } => true,
        FixPatch::RunCommand { program, args, .. } => match program.as_str() {
            // Cargo subcommands proven side-effect-bounded.
            "cargo" => matches!(
                args.first().map(String::as_str),
                Some("fmt" | "clippy" | "fix"),
            ),
            "rustfmt" | "gofmt" | "prettier" | "ruff" | "black" => true,
            _ => false,
        },
        // Future FixPatch variants: deny by default — review and add to the list above.
        _ => false,
    }
}

/// Monotonic counter for `.harness-patch-*.diff` temp filenames — millisecond
/// resolution alone collides under parallel agent runs.
static PATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Apply auto-fix patches; return short descriptions of those that succeeded.
///
/// Made `pub` (was `pub(crate)`) so integration tests can call it directly.
pub async fn apply_patches(patches: &[harness_core::FixPatch], world: &mut World) -> Vec<String> {
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
                if try_apply_diff(world, diff).await {
                    applied.push("unified diff applied".into());
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
            // FixPatch is `#[non_exhaustive]`; unknown variants are skipped.
            _ => tracing::warn!("apply_patches: unknown FixPatch variant — skipped"),
        }
    }
    applied
}

/// Write `diff` to a unique temp file and try `patch -p1` first, then `-p0`.
/// Returns whether either succeeded. The `-p1`-then-`-p0` order matches the
/// reality that most agent-emitted diffs are git-style (need `-p1`) but some
/// hand-rolled diffs use repo-relative paths (need `-p0`).
async fn try_apply_diff(world: &mut World, diff: &str) -> bool {
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncWriteExt;

    let seq = PATCH_SEQ.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let now = world.clock.now_ms();
    let tmp = world
        .repo
        .root
        .join(format!(".harness-patch-{pid}-{now}-{seq}.diff"));

    let mut f = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error=%e, path=%tmp.display(), "could not create patch tempfile");
            return false;
        }
    };
    if let Err(e) = f.write_all(diff.as_bytes()).await {
        tracing::warn!(error=%e, "could not write patch tempfile");
        let _ = tokio::fs::remove_file(&tmp).await;
        return false;
    }
    drop(f);

    let tmp_str = tmp.to_string_lossy().to_string();
    let mut applied = false;
    for strip in ["-p1", "-p0"] {
        match world
            .runner
            .exec(
                "patch",
                &[strip, "--silent", "-i", tmp_str.as_str()],
                Some(world.repo.root.as_path()),
            )
            .await
        {
            Ok(out) if out.status == 0 => {
                tracing::info!(strip, "patch applied");
                applied = true;
                break;
            }
            Ok(out) => {
                tracing::debug!(strip, stderr=%out.stderr, "patch failed; trying next strip level");
            }
            Err(e) => {
                tracing::warn!(error=%e, "patch command not available");
                break; // patch tool missing — no point trying other strip
            }
        }
    }
    let _ = tokio::fs::remove_file(&tmp).await;
    applied
}
