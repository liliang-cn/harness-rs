//! Five-stage progressive compaction (DESIGN.md §9), borrowed from Claude Code.
//!
//! `DefaultCompactor` is purely structural — it doesn't call a model. Stage 3
//! (Microcompact) and Stage 5 (AutoCompact) would normally invoke a cheap LLM;
//! here we collapse content into terse summaries so the framework can run
//! offline. Wire a `ModelBackedCompactor` later if you want semantic summaries.

use async_trait::async_trait;
use harness_core::{
    Block, Budget, CompactError, CompactionStage, Compactor, Context, Turn, TurnRole,
};

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
        }
        Ok(())
    }
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
    async fn budget_required_stages_escalates() {
        // 95% triggers ALL five stages.
        let b = Budget { used: 95, window: 100 };
        assert_eq!(b.required_stages().len(), 4);
        // 99% triggers all 5
        let b = Budget { used: 99, window: 100 };
        assert_eq!(b.required_stages().len(), 5);
    }
}
