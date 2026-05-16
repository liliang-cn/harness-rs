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

/// Heuristic compactor — operates on the structure of the context only.
pub struct DefaultCompactor {
    /// Approximate tokens per char. 0.30 ≈ 3.3 chars/token (a generous English
    /// upper bound for non-Asian content).
    pub tokens_per_char: f32,
}

impl Default for DefaultCompactor {
    fn default() -> Self { Self { tokens_per_char: 0.30 } }
}

impl DefaultCompactor {
    pub fn new() -> Self { Self::default() }

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
        Budget {
            used:   self.estimate_tokens(ctx),
            window: ctx.policy.max_input_tokens,
        }
    }

    async fn compact(&self, stage: CompactionStage, ctx: &mut Context) -> Result<(), CompactError> {
        tracing::debug!(?stage, "compaction stage running");
        match stage {
            CompactionStage::BudgetReduce    => budget_reduce(ctx),
            CompactionStage::Snip            => snip_file_reads(ctx),
            CompactionStage::Microcompact    => microcompact_old(ctx),
            CompactionStage::ContextCollapse => context_collapse(ctx),
            CompactionStage::AutoCompact     => auto_compact(ctx),
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
/// let cheap = OpenAiCompat::with_key(providers::DEEPSEEK, "deepseek-v4-flash", key);
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
        DefaultCompactor { tokens_per_char: self.tokens_per_char }.budget(ctx)
    }

    async fn compact(&self, stage: CompactionStage, ctx: &mut Context) -> Result<(), CompactError> {
        match stage {
            CompactionStage::BudgetReduce    => { budget_reduce(ctx);     Ok(()) }
            CompactionStage::Snip            => { snip_file_reads(ctx);   Ok(()) }
            CompactionStage::ContextCollapse => { context_collapse(ctx);  Ok(()) }
            CompactionStage::Microcompact    => {
                self.model_summarise(ctx, "microcompact-summary").await
            }
            CompactionStage::AutoCompact     => {
                self.model_summarise(ctx, "auto-compact-summary").await
            }
            _ => Ok(()),
        }
    }
}

impl ModelBackedCompactor {
    /// Ask the model to produce a tight summary of the older history; replace
    /// `0..split` with the resulting [`Block::Text`] in a synthetic system turn.
    async fn model_summarise(&self, ctx: &mut Context, tag: &str) -> Result<(), CompactError> {
        if ctx.history.len() <= self.keep_recent { return Ok(()); }
        let split = ctx.history.len() - self.keep_recent;
        let mut dump = String::new();
        for turn in ctx.history.iter().take(split) {
            dump.push_str(&format_turn_for_summary(turn));
        }
        if dump.trim().is_empty() { return Ok(()); }

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

        let out = self.model.complete(&summary_ctx).await.map_err(|e| {
            CompactError::Failed { stage: tag.into(), reason: format!("model: {e}") }
        })?;

        let summary = out.text.unwrap_or_else(|| "(empty summary)".into());
        let mut new_history = vec![Turn {
            role: TurnRole::System,
            blocks: vec![Block::Text(format!("[{tag}]\n{summary}"))],
        }];
        new_history.extend(ctx.history.drain(split..));
        ctx.history = new_history;
        Ok(())
    }
}

fn format_turn_for_summary(turn: &Turn) -> String {
    let role = match turn.role {
        TurnRole::User      => "user",
        TurnRole::Assistant => "assistant",
        TurnRole::Tool      => "tool",
        TurnRole::System    => "system",
        _                   => "unknown",
    };
    let mut s = format!("[{role}]\n");
    for b in &turn.blocks {
        match b {
            Block::Text(t) => { s.push_str(t); s.push('\n'); }
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
        Block::Text(s)                  => s.len(),
        Block::FileRef { path, hash: _, excerpt } => path.len() + excerpt.as_ref().map_or(0, String::len),
        Block::Skill { name, body }     => name.len() + body.len(),
        Block::ToolCall { call_id, name, args } => call_id.len() + name.len() + args.to_string().len(),
        Block::ToolResult { call_id, content } => call_id.len() + content.to_string().len(),
        Block::Feedback(signals) => signals.iter().map(|s| s.message.len() + s.agent_hint.as_ref().map_or(0, String::len)).sum(),
        Block::Reasoning(s) => s.len(),
        _ => 0,
    }
}

// ---------- Stage 1: BudgetReduce ----------

/// Trim redundant content: keep the most recent N turns intact, summarise older.
/// Conservative — only collapses big tool results, leaves text alone.
fn budget_reduce(ctx: &mut Context) {
    let keep_recent = 8;
    if ctx.history.len() <= keep_recent { return; }
    let split = ctx.history.len() - keep_recent;
    for turn in ctx.history.iter_mut().take(split) {
        for b in turn.blocks.iter_mut() {
            if let Block::ToolResult { call_id, content } = b
                && content.to_string().len() > 800
            {
                let preview = content.to_string().chars().take(200).collect::<String>();
                *b = Block::Text(format!(
                    "[tool-result:{call_id} (trimmed)] {preview}…"
                ));
            }
        }
    }
}

// ---------- Stage 2: Snip ----------

/// Replace old `Block::FileRef { excerpt }` with hash-only references.
fn snip_file_reads(ctx: &mut Context) {
    let keep_recent = 4;
    if ctx.history.len() <= keep_recent { return; }
    let split = ctx.history.len() - keep_recent;
    for turn in ctx.history.iter_mut().take(split) {
        for b in turn.blocks.iter_mut() {
            if let Block::FileRef { path, hash, excerpt } = b
                && excerpt.is_some()
            {
                *b = Block::FileRef {
                    path:   path.clone(),
                    hash:   hash.clone(),
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
    if ctx.history.len() < 12 { return; }
    let keep_recent = 6;
    let split = ctx.history.len() - keep_recent;

    // Build a textual summary of `0..split`.
    let mut summary = String::from("[microcompact-summary]\n");
    for turn in ctx.history.iter().take(split) {
        let role = match turn.role {
            TurnRole::User      => "user",
            TurnRole::Assistant => "assistant",
            TurnRole::Tool      => "tool",
            TurnRole::System    => "system",
            _                   => "unknown",
        };
        summary.push_str(&format!("- {role}: "));
        for b in &turn.blocks {
            match b {
                Block::Text(t) => {
                    summary.push_str(&t.chars().take(80).collect::<String>());
                    summary.push(' ');
                }
                Block::ToolCall { name, .. } => summary.push_str(&format!("(call:{name}) ")),
                Block::ToolResult { call_id, .. } => summary.push_str(&format!("(result:{call_id}) ")),
                Block::FileRef { path, .. } => summary.push_str(&format!("(file:{path}) ")),
                _ => {}
            }
        }
        summary.push('\n');
    }

    let mut new_history = vec![Turn {
        role:   TurnRole::System,
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
    if files.is_empty() { return; }

    let mut inv = String::from("[file-inventory]\n");
    for f in &files {
        inv.push_str(&format!("- {f}\n"));
    }

    // Remove file-ref blocks from history (inventory replaces them).
    for turn in ctx.history.iter_mut() {
        turn.blocks.retain(|b| !matches!(b, Block::FileRef { .. }));
    }

    // Insert inventory as the first system turn.
    ctx.history.insert(0, Turn {
        role:   TurnRole::System,
        blocks: vec![Block::Text(inv)],
    });
}

// ---------- Stage 5: AutoCompact ----------

/// Last resort: rewrite the whole history into a single condensed summary block.
fn auto_compact(ctx: &mut Context) {
    let keep_recent = 2;
    if ctx.history.len() <= keep_recent { return; }
    let split = ctx.history.len() - keep_recent;
    let mut combined = String::from("[auto-compact-summary]\nCondensed history of earlier turns:\n");
    let mut counts = std::collections::BTreeMap::new();
    for turn in ctx.history.iter().take(split) {
        for b in &turn.blocks {
            let key = match b {
                Block::Text(_)            => "text",
                Block::ToolCall { .. }    => "tool_call",
                Block::ToolResult { .. }  => "tool_result",
                Block::FileRef { .. }     => "file_ref",
                Block::Skill { .. }       => "skill",
                Block::Feedback(_)        => "feedback",
                Block::Reasoning(_)       => "reasoning",
                _                         => "unknown",
            };
            *counts.entry(key).or_insert(0u32) += 1;
        }
    }
    for (k, v) in counts {
        combined.push_str(&format!("- {v} × {k} block(s)\n"));
    }

    let mut new_history = vec![Turn {
        role:   TurnRole::System,
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

    fn mk_ctx(turns: usize) -> Context {
        let mut ctx = Context {
            system:   vec![],
            guides:   vec![],
            history:  Vec::new(),
            task:     Task { description: "t".into(), source: None, deadline: None },
            policy:   Policy::default(),
            metadata: BTreeMap::new(),
            tools:    Vec::new(),
        };
        for i in 0..turns {
            ctx.history.push(Turn {
                role: if i % 2 == 0 { TurnRole::User } else { TurnRole::Assistant },
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
        c.compact(CompactionStage::BudgetReduce, &mut ctx).await.unwrap();
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
        c.compact(CompactionStage::Microcompact, &mut ctx).await.unwrap();
        // First turn should be the synthetic system summary.
        assert!(matches!(ctx.history[0].role, TurnRole::System));
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
        c.compact(CompactionStage::Microcompact, &mut ctx).await.unwrap();
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
        c.compact(CompactionStage::Microcompact, &mut ctx).await.unwrap();
        assert_eq!(ctx.history.len(), 4);
        assert_eq!(mock.call_count(), 0, "model must not be called when history is short");
    }

    #[tokio::test]
    async fn budget_required_stages_escalates() {
        // 95% triggers ALL five stages.
        let b = Budget { used: 95, window: 100 };
        assert_eq!(b.required_stages().len(), 4);
        // 99% triggers all 5
        let b = Budget { used: 99, window: 100 };
        assert_eq!(b.required_stages().len(), 5);
    }
}
