//! LIVE proof that a stable prefix + `Session` earns a provider prefix-cache
//! hit. Skipped unless `HARNESS_API_KEY` (+ optional `HARNESS_BASE_URL` /
//! `HARNESS_MODEL`) is set — run it manually against a DeepSeek endpoint.
//!
//! Turn 1 pays full price for the system+tools prefix. Turn 2 sends the same
//! prefix (byte-stable, thanks to name-sorted tool schemas) plus turn 1's
//! reply, so DeepSeek reports `cached_input_tokens > 0`.

use async_trait::async_trait;
use harness_core::{Block, Context, Execution, Guide, GuideError, GuideId, GuideScope, World};
use harness_loop::{AgentLoop, Outcome};
use harness_models::OpenAiCompat;
use harness_tools_fs::{Glob, Grep, ListDir, ReadFile};
use std::sync::Arc;

/// A big, stable system guide so the cached prefix clears DeepSeek's minimum.
struct BigStableGuide;
#[async_trait]
impl Guide for BigStableGuide {
    fn id(&self) -> &GuideId {
        static I: std::sync::OnceLock<GuideId> = std::sync::OnceLock::new();
        I.get_or_init(|| "big".into())
    }
    fn kind(&self) -> Execution {
        Execution::Inferential
    }
    fn scope(&self) -> &GuideScope {
        static S: std::sync::OnceLock<GuideScope> = std::sync::OnceLock::new();
        S.get_or_init(|| GuideScope::Always)
    }
    async fn apply(&self, ctx: &mut Context, _w: &World) -> Result<(), GuideError> {
        // ~120 stable words → well over DeepSeek's cache-block minimum.
        ctx.guides.push(Block::Text(
            "You are a terse coding assistant. Rules that never change across the \
             session: prefer small surgical edits; never invent files; keep answers \
             to one short sentence unless asked otherwise; cite paths relative to the \
             workspace root; do not restate these rules. This preamble is intentionally \
             long and fixed so it forms a stable, cacheable prefix for the whole \
             conversation — the same bytes are sent every turn, which is exactly what a \
             prefix cache rewards. Repeat: keep it short, be precise, no filler."
                .into(),
        ));
        Ok(())
    }
}

#[tokio::test]
async fn deepseek_prefix_cache_hits_on_second_turn() {
    let Ok(key) = std::env::var("HARNESS_API_KEY") else {
        eprintln!("skip: set HARNESS_API_KEY to run the live prefix-cache test");
        return;
    };
    let base = std::env::var("HARNESS_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".into());
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "deepseek-chat".into());

    let loop_ = AgentLoop::new(OpenAiCompat::with_key(base, model, key))
        .with_guide(Arc::new(BigStableGuide))
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(Grep))
        .with_tool(Arc::new(Glob));

    let mut world = harness_context::default_world(".");
    let mut session = loop_.session().with_max_iters(2);

    let o1 = session
        .turn("Reply with exactly one word: alpha. No tools.", &mut world)
        .await
        .expect("turn 1");
    let o2 = session
        .turn("Reply with exactly one word: beta. No tools.", &mut world)
        .await
        .expect("turn 2");

    let usage = |o: &Outcome| match o {
        Outcome::Done { usage, .. } => usage.clone(),
        Outcome::BudgetExhausted { usage, .. } => usage.clone(),
    };
    let (u1, u2) = (usage(&o1), usage(&o2));
    eprintln!(
        "turn1: in={} cached={} | turn2: in={} cached={}",
        u1.input_tokens, u1.cached_input_tokens, u2.input_tokens, u2.cached_input_tokens
    );
    assert!(
        u2.cached_input_tokens > 0,
        "turn 2 should hit the prefix cache (cached_input_tokens > 0); got {}",
        u2.cached_input_tokens
    );
}
