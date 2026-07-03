//! `ExperienceGuide` — before each run (and each model turn), recall similar
//! past episodes and inject them so the model can reuse what worked before.
//!
//! Mirrors `harness-loop`'s `MemoryGuide` conventions: a marker-prefixed text
//! block in `ctx.guides`, stripped and refreshed on every iteration so the
//! injection tracks the latest query without growing unbounded.

use crate::store::ExperienceStore;
use async_trait::async_trait;
use harness_core::{Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, World};
use std::sync::{Arc, OnceLock};

const MARKER: &str = "[experience-recall]\n";

/// Guide that injects recalled experience episodes into the prompt.
pub struct ExperienceGuide {
    store: Arc<ExperienceStore>,
    top_k: usize,
}

static GUIDE_ID: OnceLock<GuideId> = OnceLock::new();
static GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

impl ExperienceGuide {
    pub fn new(store: Arc<ExperienceStore>) -> Self {
        Self { store, top_k: 3 }
    }

    /// How many past episodes to recall per query (default 3). Keep it small —
    /// each recalled episode spends prompt tokens.
    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }

    async fn block(&self, query: &str) -> Option<String> {
        let eps = self.store.recall(query, self.top_k).await;
        if eps.is_empty() {
            return None;
        }
        let mut s = String::from(MARKER);
        s.push_str("Similar past experience — how you handled comparable situations before:");
        for (i, ep) in eps.iter().enumerate() {
            let tools = if ep.tools.is_empty() {
                "(none)".to_string()
            } else {
                ep.tools.join(", ")
            };
            s.push_str(&format!(
                "\n  {}. situation: {}\n     tools used: {}\n     outcome: {}",
                i + 1,
                ep.situation.trim(),
                tools,
                ep.outcome.trim(),
            ));
        }
        Some(s)
    }

    fn remove_previous(ctx: &mut Context) {
        ctx.guides
            .retain(|b| !matches!(b, Block::Text(t) if t.starts_with(MARKER)));
    }
}

fn last_user_text(ctx: &Context) -> Option<String> {
    use harness_core::{Block as B, TurnRole};
    for turn in ctx.history.iter().rev() {
        if turn.role != TurnRole::User {
            continue;
        }
        for block in turn.blocks.iter().rev() {
            if let B::Text(t) = block
                && !t.trim().is_empty()
            {
                return Some(t.clone());
            }
        }
    }
    None
}

#[async_trait]
impl Guide for ExperienceGuide {
    fn id(&self) -> &GuideId {
        GUIDE_ID.get_or_init(|| "experience-recall".into())
    }
    fn kind(&self) -> Execution {
        Execution::Computational
    }
    fn scope(&self) -> &GuideScope {
        GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        Self::remove_previous(ctx);
        if let Some(block) = self.block(&ctx.task.description).await {
            ctx.guides.push(Block::Text(block));
        }
        Ok(())
    }
    async fn apply_before_iter(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        let query = last_user_text(ctx).unwrap_or_else(|| ctx.task.description.clone());
        Self::remove_previous(ctx);
        if let Some(block) = self.block(&query).await {
            ctx.guides.push(Block::Text(block));
        }
        Ok(())
    }
}
