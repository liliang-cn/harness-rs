//! `RecallGuide` — before each run (and each model turn), search the agent's
//! own past sessions and inject the top hits so a weaker model doesn't have to
//! remember that [`crate::SearchPastSessions`] exists.
//!
//! Mirrors `harness-experience`'s `ExperienceGuide` conventions: a
//! marker-prefixed text block in `ctx.guides`, stripped and refreshed on every
//! iteration so the injection tracks the latest user message without growing
//! unbounded. Without the strip, a ten-turn conversation ends up carrying ten
//! stale recall blocks, each one paid for on every subsequent request.
//!
//! Opt-in by construction. The tool is the primary interface; this exists for
//! hosts that measured their model failing to reach for it.

use crate::{ago, clip, recall_owner, search_with_cjk_fallback};
use async_trait::async_trait;
use harness_core::{
    Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, RecallStore, World,
};
use std::sync::{Arc, OnceLock};

const MARKER: &str = "[past-sessions]\n";

/// Per-snippet cap for the injected block. Tighter than the tool's budget: the
/// tool is spent deliberately on a question the model already framed, whereas
/// this is speculative and billed on every single turn.
const GUIDE_SNIPPET_CHARS: usize = 160;

/// Guide that injects recalled past-session snippets into the prompt.
pub struct RecallGuide {
    store: Arc<dyn RecallStore>,
    top_k: usize,
    owner: Option<String>,
}

static GUIDE_ID: OnceLock<GuideId> = OnceLock::new();
static GUIDE_SCOPE: OnceLock<GuideScope> = OnceLock::new();

impl RecallGuide {
    pub fn new(store: Arc<dyn RecallStore>) -> Self {
        Self {
            store,
            top_k: 2,
            owner: None,
        }
    }

    /// How many past sessions to inject per turn (default 2). Keep it at 1-2 —
    /// this cost is paid on every model call whether or not it turns out to be
    /// relevant, which is exactly the tax the tool exists to avoid.
    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k;
        self
    }

    /// Pin the owner instead of reading `profile.extra["recall_owner"]`.
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    async fn block(&self, owner: &str, query: &str, now_ms: i64) -> Option<String> {
        let query = query.trim();
        if query.is_empty() {
            return None;
        }
        let hits = match search_with_cjk_fallback(&self.store, owner, query, self.top_k).await {
            Ok(h) => h,
            // A guide must never fail a turn: recall is an enhancement, and an
            // unavailable store simply means no enhancement this iteration.
            Err(e) => {
                tracing::debug!(target: "harness::recall", error = %e, "recall guide search failed");
                return None;
            }
        };
        if hits.is_empty() {
            return None;
        }
        let mut s = String::from(MARKER);
        s.push_str(
            "From your own past sessions with this user — possibly relevant, possibly not. \
             Use it only if it fits; call `search_past_sessions` for the full context:",
        );
        for (i, h) in hits.iter().enumerate() {
            let (snippet, _) = clip(&h.snippet, GUIDE_SNIPPET_CHARS);
            let when = ago(
                h.around
                    .iter()
                    .find(|m| m.id == h.anchor_id)
                    .map(|m| m.ts_ms)
                    .unwrap_or(h.session.started_at_ms),
                now_ms,
            );
            s.push_str(&format!(
                "\n  {}. [{} · {}] {}",
                i + 1,
                h.session.session_id,
                when,
                snippet,
            ));
        }
        Some(s)
    }

    fn remove_previous(ctx: &mut Context) {
        ctx.guides
            .retain(|b| !matches!(b, Block::Text(t) if t.starts_with(MARKER)));
    }

    fn owner_for(&self, world: &World) -> String {
        self.owner.clone().unwrap_or_else(|| recall_owner(world))
    }
}

/// The freshest user text drives the query: over a long conversation the task
/// description says less and less about what the user is asking right now.
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
impl Guide for RecallGuide {
    fn id(&self) -> &GuideId {
        GUIDE_ID.get_or_init(|| "past-sessions".into())
    }
    fn kind(&self) -> Execution {
        Execution::Computational
    }
    fn scope(&self) -> &GuideScope {
        GUIDE_SCOPE.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, w: &World) -> Result<(), GuideError> {
        let (owner, now) = (self.owner_for(w), w.clock.now_ms());
        Self::remove_previous(ctx);
        if let Some(block) = self.block(&owner, &ctx.task.description.clone(), now).await {
            ctx.guides.push(Block::Text(block));
        }
        Ok(())
    }
    async fn apply_before_iter(&self, ctx: &mut Context, w: &World) -> Result<(), GuideError> {
        let (owner, now) = (self.owner_for(w), w.clock.now_ms());
        let query = last_user_text(ctx).unwrap_or_else(|| ctx.task.description.clone());
        Self::remove_previous(ctx);
        if let Some(block) = self.block(&owner, &query, now).await {
            ctx.guides.push(Block::Text(block));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_context::default_world;
    use harness_core::{RecallMessage, SessionMeta, Task};
    use harness_recall_sqlite::SqliteRecall;
    use serde_json::json;

    async fn store() -> Arc<dyn RecallStore> {
        let s: Arc<dyn RecallStore> = Arc::new(SqliteRecall::open_in_memory().unwrap());
        s.ensure_session("alice", "s1", &SessionMeta::new("s1", 1_000))
            .await
            .unwrap();
        s.append(
            "alice",
            "s1",
            &RecallMessage::new("user", "我们上次说的那家咖啡馆在南京西路", 1_000),
        )
        .await
        .unwrap();
        s
    }

    fn world() -> World {
        let mut w = default_world(".");
        w.profile
            .extra
            .insert(crate::RECALL_OWNER_KEY.into(), json!("alice"));
        w
    }

    fn ctx(desc: &str) -> Context {
        Context::new(Task {
            description: desc.into(),
            source: None,
            deadline: None,
        })
    }

    #[tokio::test]
    async fn injects_a_marked_block_when_the_past_is_relevant() {
        let guide = RecallGuide::new(store().await);
        let mut c = ctx("上次说的咖啡");
        guide.apply(&mut c, &world()).await.unwrap();

        let texts: Vec<&String> = c
            .guides
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].starts_with(MARKER));
        assert!(texts[0].contains("咖啡"));
        assert!(texts[0].contains("s1"));
    }

    #[tokio::test]
    async fn injects_nothing_when_nothing_matches() {
        let guide = RecallGuide::new(store().await);
        let mut c = ctx("quarterly revenue forecast");
        guide.apply(&mut c, &world()).await.unwrap();
        assert!(c.guides.is_empty());
    }

    #[tokio::test]
    async fn refreshes_in_place_instead_of_stacking_blocks() {
        let guide = RecallGuide::new(store().await);
        let mut c = ctx("上次说的咖啡");
        for _ in 0..5 {
            guide.apply_before_iter(&mut c, &world()).await.unwrap();
        }
        let marked = c
            .guides
            .iter()
            .filter(|b| matches!(b, Block::Text(t) if t.starts_with(MARKER)))
            .count();
        assert_eq!(marked, 1, "guide block must be replaced, never appended");
    }
}
