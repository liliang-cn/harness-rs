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
    Action, Block, Compactor, Context, Event, Guide, HarnessError, HookOutcome, Model,
    ModelDelta, ModelOutput, ResponseFormat, Sensor, SessionSource, SignalSet, Stage, StopReason,
    Task, ToolCall, ToolResult, Turn, TurnRole, Usage, World,
};
use harness_hooks::HookBus;
use std::collections::HashMap;
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
    /// Default response format applied to every run unless overridden by
    /// `run_typed`. See [`ResponseFormat`].
    pub response_format: ResponseFormat,
    /// When `true`, the loop drives each model turn via `Model::stream()`
    /// instead of `complete()`, firing `Event::ModelTokenDelta` for each
    /// text fragment. Tool-call deltas are still assembled inside the loop;
    /// only the terminal `ModelOutput` shape is observable downstream.
    pub streaming: bool,
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
            response_format: ResponseFormat::Free,
            streaming: false,
        }
    }

    /// Opt in to streaming the model's terminal turn token-by-token via
    /// `Model::stream()`. Hooks subscribed to `Event::ModelTokenDelta` see
    /// each fragment as it arrives; the rest of the loop is unchanged.
    pub fn with_streaming(mut self, enable: bool) -> Self {
        self.streaming = enable;
        self
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

    /// Set the default response format for all runs through this loop. See
    /// [`ResponseFormat`]. For typed deserialisation, prefer `run_typed::<T>()`.
    pub fn with_response_format(mut self, fmt: ResponseFormat) -> Self {
        self.response_format = fmt;
        self
    }

    /// Shortcut for `with_response_format(ResponseFormat::JsonSchema { name, schema })`.
    /// Accepts a raw `serde_json::Value` so callers can hand-roll the schema or
    /// pull it from `schemars::schema_for!(T)`.
    pub fn with_response_schema(
        self,
        name: impl Into<String>,
        schema: serde_json::Value,
    ) -> Self {
        self.with_response_format(ResponseFormat::JsonSchema {
            name: name.into(),
            schema,
        })
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

    /// Run the agent and deserialise the terminal reply into `T`.
    ///
    /// The schema for `T` is derived via `schemars::schema_for!(T)` and
    /// installed as `ResponseFormat::JsonSchema` for this run only — any
    /// pre-existing `self.response_format` is ignored. On success the
    /// returned `T` is parsed from `Outcome::Done.text` (or, on budget
    /// exhaustion, from `Outcome::BudgetExhausted.last_text`).
    ///
    /// Errors:
    /// - `HarnessError::Other` if the model returns no text at all
    /// - `HarnessError::Other` if `serde_json::from_str::<T>(text)` fails —
    ///   the original text is included in the message for debugging.
    pub async fn run_typed<T>(&self, task: Task, world: &mut World) -> Result<T, HarnessError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
    {
        let max = harness_core::Policy::default().max_iters;
        self.run_typed_with_max_iters::<T>(task, world, max).await
    }

    /// Like `run_typed` but with explicit `max_iters`.
    pub async fn run_typed_with_max_iters<T>(
        &self,
        task: Task,
        world: &mut World,
        max_iters: u32,
    ) -> Result<T, HarnessError>
    where
        T: serde::de::DeserializeOwned + schemars::JsonSchema + 'static,
    {
        let schema_root = schemars::schema_for!(T);
        let schema = serde_json::to_value(&schema_root)
            .map_err(|e| HarnessError::Other(format!("response schema: {e}")))?;
        let name = std::any::type_name::<T>()
            .rsplit("::")
            .next()
            .unwrap_or("response")
            .to_string();
        let fmt = ResponseFormat::JsonSchema { name, schema };
        let outcome = self
            .run_with_response_format(task, world, max_iters, fmt)
            .await?;
        let text = match outcome {
            Outcome::Done {
                text: Some(t),
                ..
            }
            | Outcome::BudgetExhausted {
                last_text: Some(t),
                ..
            } => t,
            Outcome::Done { text: None, .. } => {
                return Err(HarnessError::Other(
                    "run_typed: model returned no text".into(),
                ));
            }
            Outcome::BudgetExhausted {
                last_text: None, ..
            } => {
                return Err(HarnessError::Other(
                    "run_typed: budget exhausted with no text".into(),
                ));
            }
        };
        serde_json::from_str::<T>(&text).map_err(|e| {
            HarnessError::Other(format!(
                "run_typed: decode {} failed: {e} — raw text was: {text}",
                std::any::type_name::<T>()
            ))
        })
    }

    /// Run with a one-off `ResponseFormat` override (doesn't touch `self`).
    pub async fn run_with_response_format(
        &self,
        task: Task,
        world: &mut World,
        max_iters: u32,
        fmt: ResponseFormat,
    ) -> Result<Outcome, HarnessError> {
        // Borrow checker won't let us swap `self.response_format` because
        // `self` is `&`. Easiest workaround: hand-roll the same setup that
        // `run_with_seed_history` does, but with our `fmt`. We do this by
        // calling through a private helper.
        self.run_with_seed_history_and_format(task, Vec::new(), world, max_iters, Some(fmt))
            .await
    }

    async fn run_with_seed_history_and_format(
        &self,
        task: Task,
        seed: Vec<Turn>,
        world: &mut World,
        max_iters: u32,
        fmt_override: Option<ResponseFormat>,
    ) -> Result<Outcome, HarnessError> {
        let mut ctx = Context::new(task);
        ctx.policy.max_iters = max_iters;
        ctx.tools = self.tools.schemas();
        ctx.history = seed;
        ctx.response_format = fmt_override.unwrap_or_else(|| self.response_format.clone());
        self.run_built_context(ctx, world).await
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
        ctx.response_format = self.response_format.clone();
        self.run_built_context(ctx, world).await
    }

    /// Inner ReAct loop on an already-prepared `Context`. Use the public
    /// `run*` methods unless you need to inject a non-standard `Context`
    /// (e.g. `run_with_response_format` does to apply a one-off
    /// `ResponseFormat` without mutating `self`).
    async fn run_built_context(
        &self,
        mut ctx: Context,
        world: &mut World,
    ) -> Result<Outcome, HarnessError> {
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
            let out = if self.streaming {
                self.complete_via_stream(&ctx, world).await?
            } else {
                self.model.complete(&ctx).await?
            };
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

    /// Drive `Model::stream()` and assemble the result into a `ModelOutput`,
    /// firing `Event::ModelTokenDelta` for each text fragment along the way.
    ///
    /// Adapters that don't implement real streaming (e.g. `GeminiNative` /
    /// `AnthropicNative` today) fall back to the default trait impl, which
    /// runs `complete()` and emits the whole reply as a single delta. That
    /// works — the loop sees one big `ModelDelta::Text(...)` followed by
    /// `Stop`, fires one big `ModelTokenDelta`, and proceeds. So enabling
    /// `streaming` is safe regardless of which provider the user picked.
    async fn complete_via_stream(
        &self,
        ctx: &Context,
        world: &mut World,
    ) -> Result<ModelOutput, HarnessError> {
        use futures::StreamExt;
        let mut stream = self
            .model
            .stream(ctx)
            .await
            .map_err(harness_core::HarnessError::Model)?;
        let mut text = String::new();
        let mut reasoning_lines: Vec<String> = Vec::new();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::EndTurn;
        // Insertion-ordered map: index → (id, name, args). We can't use the
        // tool-call id as the primary key because the stream may emit args
        // chunks before the first chunk that carries the id; the OpenAI-compat
        // SSE parser already does its own buffering and surfaces `id` in
        // ToolCallStart, but be lenient with adapters that may interleave.
        let mut tool_starts: HashMap<String, (String, String)> = HashMap::new();
        let mut tool_order: Vec<String> = Vec::new();
        while let Some(item) = stream.next().await {
            let delta = item.map_err(harness_core::HarnessError::Model)?;
            match delta {
                ModelDelta::Text(t) => {
                    if !t.is_empty() {
                        self.hooks
                            .fire(&Event::ModelTokenDelta { text: &t }, world);
                        text.push_str(&t);
                    }
                }
                ModelDelta::ToolCallStart { id, name } => {
                    if !tool_starts.contains_key(&id) {
                        tool_order.push(id.clone());
                    }
                    tool_starts.entry(id).or_insert_with(|| (name, String::new()));
                }
                ModelDelta::ToolCallArgs { id, partial_json } => {
                    let entry = tool_starts
                        .entry(id.clone())
                        .or_insert_with(|| (String::new(), String::new()));
                    if !tool_order.iter().any(|k| k == &id) {
                        tool_order.push(id);
                    }
                    entry.1.push_str(&partial_json);
                }
                ModelDelta::ToolCallEnd { .. } => {}
                ModelDelta::Usage(u) => usage = u,
                ModelDelta::Stop(r) => stop_reason = r,
                ModelDelta::Reasoning(s) => {
                    if !s.is_empty() {
                        reasoning_lines.push(s);
                    }
                }
                // ModelDelta is `#[non_exhaustive]`; ignore future variants
                // we don't yet understand.
                _ => {}
            }
        }
        let tool_calls: Vec<ToolCall> = tool_order
            .into_iter()
            .filter_map(|id| {
                tool_starts.remove(&id).map(|(name, args)| {
                    let args_v = serde_json::from_str::<serde_json::Value>(&args)
                        .unwrap_or_else(|_| serde_json::Value::String(args));
                    ToolCall {
                        id,
                        name,
                        args: args_v,
                    }
                })
            })
            .collect();
        // Reconcile stop_reason with what actually came out — adapters
        // sometimes emit `Stop(EndTurn)` even after tool_calls, which would
        // confuse downstream consumers that branch on stop_reason alone.
        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolUse
        } else {
            stop_reason
        };
        Ok(ModelOutput {
            text: if text.is_empty() { None } else { Some(text) },
            tool_calls,
            usage,
            stop_reason,
            reasoning: if reasoning_lines.is_empty() {
                None
            } else {
                Some(reasoning_lines.join("\n"))
            },
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
