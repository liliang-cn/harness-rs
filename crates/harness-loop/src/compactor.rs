//! Five-stage progressive compaction (DESIGN.md §9), borrowed from Claude Code.
//!
//! `DefaultCompactor` is purely structural — it doesn't call a model. Stage 3
//! (Microcompact) and Stage 5 (AutoCompact) would normally invoke a cheap LLM;
//! here we collapse content into terse summaries so the framework can run
//! offline. Wire a `ModelBackedCompactor` later if you want semantic summaries.

use async_trait::async_trait;
use harness_core::{
    Block, Budget, CompactError, CompactionStage, Compactor, Context, Model, Policy, Task, Turn,
    TurnRole,
};
use std::sync::Arc;

/// `Context::metadata` key under which the loop stores a *calibration factor* —
/// the ratio of the model's real reported `input_tokens` to this compactor's
/// char-based estimate on the previous turn. `budget()` multiplies its raw
/// estimate by it, so the trigger auto-aligns to the actual model + language
/// (a fixed `tokens_per_char` badly under-counts CJK, over-counts nothing).
pub const CALIBRATION_KEY: &str = "compactor.correction";

/// Read the calibration factor from context metadata (defaults to 1.0 — i.e.
/// trust the raw estimate until the first real `input_tokens` arrives).
fn calibration(ctx: &Context) -> f64 {
    ctx.metadata
        .get(CALIBRATION_KEY)
        .and_then(|v| v.as_f64())
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(1.0)
}

/// Heuristic compactor — operates on the structure of the context only.
pub struct DefaultCompactor {
    /// Approximate tokens per char. 0.30 ≈ 3.3 chars/token (a generous English
    /// upper bound for non-Asian content). Used as the *prior*; once the loop
    /// records a real `input_tokens`, [`CALIBRATION_KEY`] corrects it.
    pub tokens_per_char: f32,
}

impl Default for DefaultCompactor {
    fn default() -> Self {
        Self {
            tokens_per_char: 0.30,
        }
    }
}

impl DefaultCompactor {
    pub fn new() -> Self {
        Self::default()
    }

    fn estimate_tokens(&self, ctx: &Context) -> u32 {
        let mut chars: usize = 0;
        for b in ctx.system.iter().chain(ctx.guides.iter()) {
            chars += block_chars(b);
        }
        for turn in &ctx.history {
            for b in &turn.blocks {
                chars += block_chars(b);
            }
        }
        chars += ctx.task.description.len();
        (chars as f32 * self.tokens_per_char) as u32
    }
}

#[async_trait]
impl Compactor for DefaultCompactor {
    fn budget(&self, ctx: &Context) -> Budget {
        let raw = self.estimate_tokens(ctx);
        let used = (raw as f64 * calibration(ctx)).round() as u32;
        Budget {
            used,
            window: ctx.policy.max_input_tokens,
        }
    }

    async fn compact(&self, stage: CompactionStage, ctx: &mut Context) -> Result<(), CompactError> {
        tracing::debug!(?stage, "compaction stage running");
        match stage {
            CompactionStage::BudgetReduce => budget_reduce(ctx),
            CompactionStage::Snip => snip_file_reads(ctx),
            CompactionStage::Microcompact => microcompact_old(ctx),
            CompactionStage::ContextCollapse => context_collapse(ctx),
            CompactionStage::AutoCompact => auto_compact(ctx),
            // Forward-compat: ignore stages this version doesn't recognise.
            _ => tracing::warn!(?stage, "unknown compaction stage — ignoring"),
        }
        Ok(())
    }
}

// ============================================================
// ModelBackedCompactor — uses a (typically cheap) Model to do real semantic
// summarisation for Microcompact and AutoCompact stages.
// ============================================================

/// Compactor that calls an LLM for the inferential stages and falls back to
/// `DefaultCompactor`'s structural strategies for the computational ones.
///
/// Typical wiring:
/// ```ignore
/// let cheap = OpenAiCompat::with_key("https://api.deepseek.com", "deepseek-v4-flash", key);
/// let compactor = ModelBackedCompactor::new(Arc::new(cheap));
/// let loop_ = AgentLoop::new(main_model).with_compactor(Arc::new(compactor));
/// ```
pub struct ModelBackedCompactor {
    pub model: Arc<dyn Model>,
    pub tokens_per_char: f32,
    /// Keep the most recent N turns intact during semantic compaction.
    pub keep_recent: usize,
    /// Hard cap on the summary length the model is asked to produce.
    pub summary_max_tokens: u32,
}

/// Move a proposed split to a point where cutting the history is *legal*.
///
/// Every compaction stage keeps `history[split..]` and replaces the prefix with
/// a summary turn. Choosing that index by counting turns alone will eventually
/// cut between an assistant turn that requested a tool and the turn carrying
/// that tool's result — and the providers enforce the pairing. Gemini answers
/// such a history with a hard 400 ("function call turn comes immediately after
/// a user turn or after a function response turn"), which is permanent: the
/// retry sends the same broken history and dies the same way, so the run is
/// over. It only shows up once the context actually fills, which is to say
/// only on the long runs that can least afford to lose their work.
///
/// A cut is safe directly before a `User` turn: that is where an exchange
/// begins, so nothing behind it is owed an answer. Prefer the nearest such
/// point at or before `want` (keeping slightly more history is harmless);
/// failing that, take the nearest one after it. With no user turn anywhere,
/// report `None` and let the caller skip this stage rather than corrupt the
/// conversation.
fn safe_split(history: &[Turn], want: usize) -> Option<usize> {
    if want == 0 || want >= history.len() {
        return None;
    }
    // Walk outward from the wanted point: back first, since keeping slightly
    // more history costs nothing, then forward.
    (0..=want)
        .rev()
        .chain(want + 1..history.len())
        .filter(|&i| i > 0)
        .find(|&i| starts_an_exchange(&history[i]))
}

/// Whether the kept history may *begin* at this turn.
///
/// After a cut the first kept turn follows the summary, which is a **user**
/// turn — so a turn that requests a tool is fine there, exactly as it is after
/// any user message. What is never fine is a tool *result*, because the
/// request that earned it was just dropped.
///
/// Getting this wrong is easy in both directions and neither shows up except
/// on a long run. Too loose and the provider rejects the conversation for the
/// rest of the run; too strict and every stage declines and compaction quietly
/// stops working. The first version cut anywhere and broke pairs; the second
/// demanded a user turn, of which an agent run has exactly one, at index 0,
/// where a cut drops nothing; a real agent history is a bare alternation of
/// tool requests and results, so it offered nowhere to land at all.
///
/// The providers' rule is that a turn requesting a tool must follow a user
/// turn or a tool result. After a cut the first kept turn follows only the
/// summary, so it may not be a tool request — and it may not be a tool
/// *result* either, since the request that earned it was just dropped.
/// Anything else is a fresh start: a user turn, or an assistant turn that only
/// speaks.
///
/// Requiring a user turn here (as this first did) is too strict to be useful:
/// an agent run has exactly one, at index 0, and index 0 cuts nothing — so
/// every stage declined and compaction silently stopped compacting. That is
/// the failure this predicate exists to avoid on both sides.
fn starts_an_exchange(turn: &Turn) -> bool {
    turn.role != TurnRole::Tool
}

impl ModelBackedCompactor {
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self {
            model,
            tokens_per_char: 0.30,
            keep_recent: 6,
            summary_max_tokens: 600,
        }
    }
}

#[async_trait]
impl Compactor for ModelBackedCompactor {
    fn budget(&self, ctx: &Context) -> Budget {
        DefaultCompactor {
            tokens_per_char: self.tokens_per_char,
        }
        .budget(ctx)
    }

    async fn compact(&self, stage: CompactionStage, ctx: &mut Context) -> Result<(), CompactError> {
        match stage {
            CompactionStage::BudgetReduce => {
                budget_reduce(ctx);
                Ok(())
            }
            CompactionStage::Snip => {
                snip_file_reads(ctx);
                Ok(())
            }
            CompactionStage::ContextCollapse => {
                context_collapse(ctx);
                Ok(())
            }
            CompactionStage::Microcompact => {
                self.model_summarise(ctx, "microcompact-summary").await
            }
            CompactionStage::AutoCompact => self.model_summarise(ctx, "auto-compact-summary").await,
            _ => Ok(()),
        }
    }
}

impl ModelBackedCompactor {
    /// Ask the model to produce a tight summary of the older history; replace
    /// `0..split` with the resulting [`Block::Text`] in a synthetic system turn.
    async fn model_summarise(&self, ctx: &mut Context, tag: &str) -> Result<(), CompactError> {
        if ctx.history.len() <= self.keep_recent {
            return Ok(());
        }
        let Some(split) = safe_split(&ctx.history, ctx.history.len() - self.keep_recent) else {
            return Ok(());
        };
        let mut dump = String::new();
        for turn in ctx.history.iter().take(split) {
            dump.push_str(&format_turn_for_summary(turn));
        }
        if dump.trim().is_empty() {
            return Ok(());
        }

        let prompt = format!(
            "You are compacting an in-progress agent conversation for downstream replay. \
             Produce a terse summary (≤ 200 words) of the conversation below. Preserve: \
             concrete file paths, decisions made, sensor outcomes, and the current goal. \
             Drop: chit-chat, redundant tool reads, verbose stack traces.\n\n\
             ---- TRANSCRIPT ----\n{dump}\n---- END ----\n\n\
             Reply with the summary text only, no preamble."
        );

        let mut summary_ctx = Context::new(Task {
            description: prompt,
            source: None,
            deadline: None,
        });
        summary_ctx.policy = Policy {
            max_iters: 1,
            max_input_tokens: 100_000,
            max_output_tokens: self.summary_max_tokens,
            self_correct_rounds: 0,
        };
        summary_ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(summary_ctx.task.description.clone())],
        });

        let out = self
            .model
            .complete(&summary_ctx)
            .await
            .map_err(|e| CompactError::Failed {
                stage: tag.into(),
                reason: format!("model: {e}"),
            })?;

        let summary = out.text.unwrap_or_else(|| "(empty summary)".into());
        let mut new_history = vec![Turn {
            // See `starts_an_exchange`: a user turn keeps a following tool
            // request legal.
            role: TurnRole::User,
            blocks: vec![Block::Text(format!("[{tag}]\n{summary}"))],
        }];
        new_history.extend(ctx.history.drain(split..));
        ctx.history = new_history;
        Ok(())
    }
}

fn format_turn_for_summary(turn: &Turn) -> String {
    let role = match turn.role {
        TurnRole::User => "user",
        TurnRole::Assistant => "assistant",
        TurnRole::Tool => "tool",
        TurnRole::System => "system",
        _ => "unknown",
    };
    let mut s = format!("[{role}]\n");
    for b in &turn.blocks {
        match b {
            Block::Text(t) => {
                s.push_str(t);
                s.push('\n');
            }
            Block::ToolCall { name, args, .. } => {
                s.push_str(&format!("(tool-call {name} {args})\n"));
            }
            Block::ToolResult { call_id, content } => {
                let preview = content.to_string();
                let preview = preview.chars().take(160).collect::<String>();
                s.push_str(&format!("(tool-result {call_id}: {preview}…)\n"));
            }
            Block::FileRef { path, .. } => {
                s.push_str(&format!("(file-ref {path})\n"));
            }
            _ => {}
        }
    }
    s.push('\n');
    s
}

fn block_chars(b: &Block) -> usize {
    match b {
        Block::Text(s) => s.len(),
        Block::FileRef {
            path,
            hash: _,
            excerpt,
        } => path.len() + excerpt.as_ref().map_or(0, String::len),
        Block::Skill { name, body } => name.len() + body.len(),
        Block::ToolCall {
            call_id,
            name,
            args,
        } => call_id.len() + name.len() + args.to_string().len(),
        Block::ToolResult { call_id, content } => call_id.len() + content.to_string().len(),
        Block::Feedback(signals) => signals
            .iter()
            .map(|s| s.message.len() + s.agent_hint.as_ref().map_or(0, String::len))
            .sum(),
        Block::Reasoning(s) => s.len(),
        // Base64 image payloads are large; count them so the budget doesn't
        // wildly under-estimate a context carrying an image.
        Block::Image { base64, .. } => base64.len(),
        _ => 0,
    }
}

// ---------- Stage 1: BudgetReduce ----------

/// Trim redundant content: keep the most recent N turns intact, summarise older.
/// Conservative — only collapses big tool results, leaves text alone.
///
/// Recency protects a turn from being *summarised away*, but it cannot protect
/// one from being *too large to send*. Every stage here guards on
/// `history.len() <= keep_recent`, and a context does not blow up by turn count:
/// an agent reads one large file and two turns later there is no room. Measured
/// on that exact shape — three turns, one big tool result — all five stages
/// returned immediately and the context came out 20 tokens *larger* than it went
/// in. So a genuinely oversized result is trimmed wherever it sits, recent or
/// not, at a much higher threshold than the one used for old turns.
fn budget_reduce(ctx: &mut Context) {
    const OLD_TRIM_BYTES: usize = 800;
    /// A recent result is only touched when it is large enough to be the reason
    /// the context does not fit — far above the threshold for stale ones.
    const RECENT_TRIM_BYTES: usize = 16 * 1024;

    let keep_recent = 8;
    let split = ctx.history.len().saturating_sub(keep_recent);
    for (i, turn) in ctx.history.iter_mut().enumerate() {
        let limit = if i < split {
            OLD_TRIM_BYTES
        } else {
            RECENT_TRIM_BYTES
        };
        for b in turn.blocks.iter_mut() {
            if let Block::ToolResult { call_id, content } = b {
                let body = content.to_string();
                if body.len() > limit {
                    // Keep more of a recent result: it is probably still the
                    // thing being worked on, and the point is to make it fit,
                    // not to erase it.
                    let keep = if i < split { 200 } else { 2_000 };
                    let preview = body.chars().take(keep).collect::<String>();
                    *b = Block::Text(format!(
                        "[tool-result:{call_id} (trimmed from {} bytes)] {preview}…",
                        body.len()
                    ));
                }
            }
        }
    }
}

// ---------- Stage 2: Snip ----------

/// Replace old `Block::FileRef { excerpt }` with hash-only references.
fn snip_file_reads(ctx: &mut Context) {
    let keep_recent = 4;
    if ctx.history.len() <= keep_recent {
        return;
    }
    let split = ctx.history.len() - keep_recent;
    for turn in ctx.history.iter_mut().take(split) {
        for b in turn.blocks.iter_mut() {
            if let Block::FileRef {
                path,
                hash,
                excerpt,
            } = b
                && excerpt.is_some()
            {
                *b = Block::FileRef {
                    path: path.clone(),
                    hash: hash.clone(),
                    excerpt: None,
                };
            }
        }
    }
}

// ---------- Stage 3: Microcompact ----------

/// Summarise older conversation segments. In `DefaultCompactor` we just
/// rewrite the older half of the history into a single text block tagged
/// `[microcompact-summary]`. Real provider-backed implementations should
/// replace this with a cheap-model summarisation call.
fn microcompact_old(ctx: &mut Context) {
    if ctx.history.len() < 12 {
        return;
    }
    let keep_recent = 6;
    let Some(split) = safe_split(&ctx.history, ctx.history.len() - keep_recent) else {
        return;
    };

    // Build a textual summary of `0..split`.
    let mut summary = String::from("[microcompact-summary]\n");
    for turn in ctx.history.iter().take(split) {
        let role = match turn.role {
            TurnRole::User => "user",
            TurnRole::Assistant => "assistant",
            TurnRole::Tool => "tool",
            TurnRole::System => "system",
            _ => "unknown",
        };
        summary.push_str(&format!("- {role}: "));
        for b in &turn.blocks {
            match b {
                Block::Text(t) => {
                    summary.push_str(&t.chars().take(80).collect::<String>());
                    summary.push(' ');
                }
                Block::ToolCall { name, .. } => summary.push_str(&format!("(call:{name}) ")),
                Block::ToolResult { call_id, .. } => {
                    summary.push_str(&format!("(result:{call_id}) "))
                }
                Block::FileRef { path, .. } => summary.push_str(&format!("(file:{path}) ")),
                _ => {}
            }
        }
        summary.push('\n');
    }

    let mut new_history = vec![Turn {
        // A user turn, not a system one: it is what makes the first kept turn
        // legal when that turn requests a tool, which in an agent history it
        // almost always does.
        role: TurnRole::User,
        blocks: vec![Block::Text(summary)],
    }];
    new_history.extend(ctx.history.drain(split..));
    ctx.history = new_history;
}

// ---------- Stage 4: ContextCollapse ----------

/// Collapse all FileRefs into a single inventory at the top, plus key excerpts.
fn context_collapse(ctx: &mut Context) {
    // Walk all history, collect file paths.
    let mut files = std::collections::BTreeSet::new();
    for turn in &ctx.history {
        for b in &turn.blocks {
            if let Block::FileRef { path, .. } = b {
                files.insert(path.clone());
            }
        }
    }
    if files.is_empty() {
        return;
    }

    let mut inv = String::from("[file-inventory]\n");
    for f in &files {
        inv.push_str(&format!("- {f}\n"));
    }

    // Remove file-ref blocks from history (inventory replaces them).
    for turn in ctx.history.iter_mut() {
        turn.blocks.retain(|b| !matches!(b, Block::FileRef { .. }));
    }

    // Insert inventory as the first system turn.
    ctx.history.insert(
        0,
        Turn {
            role: TurnRole::System,
            blocks: vec![Block::Text(inv)],
        },
    );
}

// ---------- Stage 5: AutoCompact ----------

/// Last resort: rewrite the whole history into a single condensed summary block.
fn auto_compact(ctx: &mut Context) {
    let keep_recent = 2;
    if ctx.history.len() <= keep_recent {
        return;
    }
    let Some(split) = safe_split(&ctx.history, ctx.history.len() - keep_recent) else {
        return;
    };
    let mut combined =
        String::from("[auto-compact-summary]\nCondensed history of earlier turns:\n");
    let mut counts = std::collections::BTreeMap::new();
    for turn in ctx.history.iter().take(split) {
        for b in &turn.blocks {
            let key = match b {
                Block::Text(_) => "text",
                Block::ToolCall { .. } => "tool_call",
                Block::ToolResult { .. } => "tool_result",
                Block::FileRef { .. } => "file_ref",
                Block::Skill { .. } => "skill",
                Block::Feedback(_) => "feedback",
                Block::Reasoning(_) => "reasoning",
                _ => "unknown",
            };
            *counts.entry(key).or_insert(0u32) += 1;
        }
    }
    for (k, v) in counts {
        combined.push_str(&format!("- {v} × {k} block(s)\n"));
    }

    let mut new_history = vec![Turn {
        // A user turn, not a system one: it is what makes the first kept turn
        // legal when that turn requests a tool, which in an agent history it
        // almost always does.
        role: TurnRole::User,
        blocks: vec![Block::Text(combined)],
    }];
    new_history.extend(ctx.history.drain(split..));
    ctx.history = new_history;
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Block, Policy, Task, Turn, TurnRole};
    use std::collections::BTreeMap;

    /// A history shaped like a real agent run: user asks, assistant calls a
    /// tool, the tool answers, repeat. Compaction must never cut between a
    /// call and its result.
    fn tool_using_history(exchanges: usize) -> Context {
        let mut ctx = mk_ctx(0);
        for i in 0..exchanges {
            ctx.history.push(Turn {
                role: TurnRole::User,
                blocks: vec![Block::Text(format!("do step {i}"))],
            });
            ctx.history.push(Turn {
                role: TurnRole::Assistant,
                blocks: vec![Block::ToolCall {
                    call_id: format!("c{i}"),
                    name: "write_file".into(),
                    args: serde_json::json!({ "path": format!("{i}.txt") }),
                }],
            });
            ctx.history.push(Turn {
                role: TurnRole::Tool,
                blocks: vec![Block::ToolResult {
                    call_id: format!("c{i}"),
                    content: serde_json::json!({ "ok": true }),
                }],
            });
        }
        ctx
    }

    /// Every `Tool` turn must still be preceded by the assistant turn that
    /// asked for it, and the kept history must not *open* with a tool call —
    /// providers reject both, and Gemini rejects the second with a permanent
    /// 400 that ends the run.
    fn assert_history_is_legal(history: &[Turn], what: &str) {
        // The summary leads as a user turn, so a tool *request* may follow it.
        // A tool *result* may not: whatever asked for it is gone.
        if let Some(first) = history.first() {
            assert!(
                first.role != TurnRole::Tool,
                "{what}: history opens with a tool result whose request was dropped"
            );
        }
        for (i, turn) in history.iter().enumerate() {
            if turn.role == TurnRole::Tool {
                let prev_is_call = i > 0
                    && history[i - 1]
                        .blocks
                        .iter()
                        .any(|b| matches!(b, Block::ToolCall { .. }));
                assert!(
                    prev_is_call,
                    "{what}: tool result at {i} lost the call that made it"
                );
            }
        }
    }

    #[test]
    fn compaction_never_splits_a_tool_call_from_its_result() {
        // Every stage that rewrites history, at every length: with 3 turns per
        // exchange and a fixed `keep_recent`, a blind split lands mid-pair on
        // two lengths out of three.
        for exchanges in 4..14 {
            for (name, stage) in [
                ("microcompact_old", microcompact_old as fn(&mut Context)),
                ("context_collapse", context_collapse as fn(&mut Context)),
                ("auto_compact", auto_compact as fn(&mut Context)),
            ] {
                let mut ctx = tool_using_history(exchanges);
                stage(&mut ctx);
                assert_history_is_legal(&ctx.history, &format!("{name} @ {exchanges} exchanges"));
            }
        }
    }

    #[test]
    fn a_split_with_nowhere_legal_to_land_is_declined() {
        // Only a tool result sits after index 0, and a kept history may not
        // begin with one. Better to skip the stage than hand the provider a
        // conversation it will refuse for the rest of the run.
        let history = vec![
            Turn {
                role: TurnRole::Assistant,
                blocks: vec![Block::ToolCall {
                    call_id: "c0".into(),
                    name: "t".into(),
                    args: serde_json::json!({}),
                }],
            },
            Turn {
                role: TurnRole::Tool,
                blocks: vec![Block::ToolResult {
                    call_id: "c0".into(),
                    content: serde_json::json!({}),
                }],
            },
        ];
        assert_eq!(safe_split(&history, 1), None);
    }

    /// The shape an agent run actually has: one user turn, then nothing but
    /// tool requests and their results. Demanding a user turn — or even an
    /// assistant turn that only speaks — finds nowhere to land here, and
    /// compaction silently stops working on exactly the runs that need it.
    #[test]
    fn a_bare_alternation_of_tool_calls_can_still_be_compacted() {
        let mut ctx = mk_ctx(0);
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text("the task".into())],
        });
        for i in 0..10 {
            ctx.history.push(Turn {
                role: TurnRole::Assistant,
                blocks: vec![Block::ToolCall {
                    call_id: format!("c{i}"),
                    name: "read_file".into(),
                    args: serde_json::json!({ "path": format!("{i}.txt") }),
                }],
            });
            ctx.history.push(Turn {
                role: TurnRole::Tool,
                blocks: vec![Block::ToolResult {
                    call_id: format!("c{i}"),
                    content: serde_json::json!({ "ok": true }),
                }],
            });
        }
        let before = ctx.history.len();
        microcompact_old(&mut ctx);
        assert!(
            ctx.history.len() < before,
            "compaction did nothing on a bare tool alternation: {before} turns in, {} out",
            ctx.history.len()
        );
        assert_history_is_legal(&ctx.history, "bare tool alternation");
    }

    /// An agent run has exactly ONE user turn — the task — and it sits at
    /// index 0, where a cut would drop nothing. Requiring a user turn to land
    /// on therefore made every stage decline, and compaction silently stopped
    /// compacting on exactly the long runs it exists for. An assistant turn
    /// that only speaks is a legal place to begin.
    #[test]
    fn a_run_with_one_user_turn_can_still_be_compacted() {
        let mut ctx = mk_ctx(0);
        ctx.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text("the task".into())],
        });
        for i in 0..8 {
            ctx.history.push(Turn {
                role: TurnRole::Assistant,
                blocks: vec![Block::ToolCall {
                    call_id: format!("c{i}"),
                    name: "read_file".into(),
                    args: serde_json::json!({}),
                }],
            });
            ctx.history.push(Turn {
                role: TurnRole::Tool,
                blocks: vec![Block::ToolResult {
                    call_id: format!("c{i}"),
                    content: serde_json::json!({ "ok": true }),
                }],
            });
            ctx.history.push(Turn {
                role: TurnRole::Assistant,
                blocks: vec![Block::Text(format!("step {i} done"))],
            });
        }
        let before = ctx.history.len();
        microcompact_old(&mut ctx);
        assert!(
            ctx.history.len() < before,
            "compaction did nothing: {before} turns in, {} out",
            ctx.history.len()
        );
        assert_history_is_legal(&ctx.history, "single-user-turn run");
    }

    fn mk_ctx(turns: usize) -> Context {
        let mut ctx = Context {
            system: vec![],
            guides: vec![],
            history: Vec::new(),
            task: Task {
                description: "t".into(),
                source: None,
                deadline: None,
            },
            policy: Policy::default(),
            metadata: BTreeMap::new(),
            tools: Vec::new(),
            response_format: harness_core::ResponseFormat::Free,
        };
        for i in 0..turns {
            ctx.history.push(Turn {
                role: if i % 2 == 0 {
                    TurnRole::User
                } else {
                    TurnRole::Assistant
                },
                blocks: vec![Block::Text(format!("turn {i}: {}", "x".repeat(50)))],
            });
        }
        ctx
    }

    #[tokio::test]
    async fn budget_reduce_keeps_recent() {
        let c = DefaultCompactor::new();
        let mut ctx = mk_ctx(20);
        // Inject big tool results in early turns
        ctx.history[0].blocks.push(Block::ToolResult {
            call_id: "c1".into(),
            content: serde_json::Value::String("y".repeat(2000)),
        });
        c.compact(CompactionStage::BudgetReduce, &mut ctx)
            .await
            .unwrap();
        // First turn's big tool result should be trimmed.
        let has_trim = ctx.history[0]
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Text(t) if t.contains("trimmed")));
        assert!(has_trim);
    }

    #[tokio::test]
    async fn microcompact_collapses_old_turns() {
        let c = DefaultCompactor::new();
        let mut ctx = mk_ctx(20);
        c.compact(CompactionStage::Microcompact, &mut ctx)
            .await
            .unwrap();
        // First turn should be the synthetic system summary.
        // The summary leads as a user turn — see `starts_an_exchange`.
        assert!(matches!(ctx.history[0].role, TurnRole::User));
        let first_text = match &ctx.history[0].blocks[0] {
            Block::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert!(first_text.starts_with("[microcompact-summary]"));
    }

    #[tokio::test]
    async fn model_backed_compactor_replaces_old_turns_with_summary() {
        use harness_models::{MockModel, MockResponse};

        let model = Arc::new(MockModel::new().script(MockResponse::text("CONCISE-SUMMARY-OK")))
            as Arc<dyn Model>;
        let c = ModelBackedCompactor::new(model);

        let mut ctx = mk_ctx(20);
        let original_len = ctx.history.len();
        c.compact(CompactionStage::Microcompact, &mut ctx)
            .await
            .unwrap();
        // First turn now the summary, total shrinks to keep_recent (6) + 1 summary = 7
        assert_eq!(ctx.history.len(), c.keep_recent + 1);
        assert!(original_len > ctx.history.len());
        let first = match &ctx.history[0].blocks[0] {
            Block::Text(t) => t.clone(),
            _ => String::new(),
        };
        assert!(first.starts_with("[microcompact-summary]"));
        assert!(first.contains("CONCISE-SUMMARY-OK"));
    }

    #[tokio::test]
    async fn model_backed_compactor_noop_when_history_short() {
        use harness_models::{MockModel, MockResponse};
        let mock = Arc::new(MockModel::new().script(MockResponse::text("never called")));
        let c = ModelBackedCompactor::new(mock.clone() as Arc<dyn Model>);
        let mut ctx = mk_ctx(4); // < keep_recent
        c.compact(CompactionStage::Microcompact, &mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.history.len(), 4);
        assert_eq!(
            mock.call_count(),
            0,
            "model must not be called when history is short"
        );
    }

    #[test]
    fn budget_applies_calibration_factor() {
        let c = DefaultCompactor::new();
        let mut ctx = mk_ctx(10);
        let raw = c.budget(&ctx).used;
        assert!(raw > 0);

        // A model that reports 2× our estimate → correction 2.0 → used doubles.
        ctx.metadata
            .insert(CALIBRATION_KEY.into(), serde_json::json!(2.0));
        let calibrated = c.budget(&ctx).used;
        assert_eq!(calibrated, (raw as f64 * 2.0).round() as u32);

        // Garbage / non-positive factors are ignored (fall back to raw).
        ctx.metadata
            .insert(CALIBRATION_KEY.into(), serde_json::json!(-1.0));
        assert_eq!(c.budget(&ctx).used, raw);
    }

    #[tokio::test]
    async fn budget_required_stages_escalates() {
        // 95% triggers ALL five stages.
        let b = Budget {
            used: 95,
            window: 100,
        };
        assert_eq!(b.required_stages().len(), 4);
        // 99% triggers all 5
        let b = Budget {
            used: 99,
            window: 100,
        };
        assert_eq!(b.required_stages().len(), 5);
    }
}
