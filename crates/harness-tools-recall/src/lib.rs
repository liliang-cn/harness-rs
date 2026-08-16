//! LLM-facing cross-session recall — letting an agent 翻旧账 on itself.
//!
//! [`harness_core::RecallStore`] already keeps the raw transcript of every
//! session, and `harness-recall-sqlite` already indexes it (FTS5 BM25 + a
//! trigram table so CJK actually matches). What was missing is the piece that
//! makes any of it reachable *by the model*: a tool it can decide to call.
//!
//! This crate is that piece.
//!
//! - **[`SearchPastSessions`]** — the primary interface. One tool,
//!   `search_past_sessions`, holding an `Arc<dyn RecallStore>` so it works with
//!   the file backend, the SQLite backend, or anything an app writes itself.
//! - **[`RecallGuide`]** — opt-in. Injects the top 1-2 relevant past-session
//!   snippets into the prompt every turn, for deployments that would rather pay
//!   a few hundred tokens unconditionally than rely on the model noticing it
//!   should search. Kept a separate type precisely so that it is a deliberate
//!   choice: the tool is what you wire by default.
//!
//! # Why a tool and not just a guide
//!
//! Proactive injection is a tax on every turn and a guess about relevance made
//! before the model has thought about the question. A tool is paid only when
//! the model believes there is something to find, and the query it writes is
//! informed by the whole conversation, not just the last user message. Guides
//! are the fallback for models too weak to reach for the tool on their own.
//!
//! # Chinese queries
//!
//! The SQLite backend's trigram index matches contiguous substrings, which
//! handles "支付服务" but not "上次说的咖啡" — the phrasing a user actually
//! reaches for. This crate decomposes a missed han query into chunks before
//! giving up; see `search_with_cjk_fallback` in this module for why that
//! belongs here rather than in the store.
//!
//! # Owner scoping
//!
//! Every `RecallStore` method is owner-scoped, and the owner is read from
//! `World.profile.extra["recall_owner"]` — the *same* key `AgentLoop::with_recall`
//! writes under when it captures the conversation. Reading it from anywhere
//! else would search a different partition than the one the capture path wrote
//! to, and the failure mode would be a silent, permanent "nothing found".
//! [`SearchPastSessions::with_owner`] overrides it for single-tenant hosts that
//! never populate a profile.
//!
//! # Example
//!
//! ```ignore
//! let store: Arc<dyn RecallStore> = Arc::new(SqliteRecall::open(path)?);
//! let mut loop_ = AgentLoop::new(model)
//!     .with_recall(store.clone())                              // capture
//!     .with_tool(Arc::new(SearchPastSessions::new(store.clone()))); // retrieval
//! // Optional, for weaker models:
//! // loop_ = loop_.with_guide(Arc::new(RecallGuide::new(store)));
//! ```

mod guide;

pub use guide::RecallGuide;

use async_trait::async_trait;
use harness_core::{
    RecallError, RecallStore, SessionHit, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World,
};
use serde_json::{Value, json};
use std::sync::Arc;

/// Profile key holding the recall owner. Must stay identical to the key the
/// capture side (`AgentLoop::with_recall`) writes, or reads and writes land in
/// different partitions.
pub const RECALL_OWNER_KEY: &str = "recall_owner";

/// Hits returned when the model doesn't say. Small on purpose: a past-session
/// lookup is a lead, not a corpus dump, and five is already more than a model
/// will meaningfully use in one turn.
pub const DEFAULT_LIMIT: usize = 5;

/// Hard ceiling on `limit`. Anything above this is a model that has mistaken
/// recall for grep.
pub const MAX_LIMIT: usize = 20;

/// Per-hit snippet budget, in **characters**.
///
/// The SQLite backend's `snippet(..., 12)` is already short, but the LIKE and
/// trigram fallback paths hand back raw message content, which can be an entire
/// pasted file. 240 chars is roughly two sentences with the `>>>match<<<`
/// markers still intact — enough for the model to judge relevance and decide
/// whether to follow up, which is the only job a snippet has here.
pub const SNIPPET_CHARS: usize = 240;

/// Total snippet budget across *all* hits in one result, in characters.
///
/// Worst case without this cap is `MAX_LIMIT * SNIPPET_CHARS` = 4800 chars. For
/// CJK that is ~4800 tokens on most tokenizers (roughly one token per han
/// character), which is an unacceptable bill for a speculative lookup. 4000
/// caps the payload at ~4k tokens worst case (CJK) and ~1.2k typical (ASCII).
/// Hits past the budget keep their metadata — session id, timestamp, anchor —
/// and lose only the snippet, so the model still learns *that* the sessions
/// exist and can re-run with a narrower query.
pub const TOTAL_SNIPPET_CHARS: usize = 4000;

/// Resolve the recall owner for this call: the profile key, else `"default"`.
///
/// Deliberately does *not* fall back to `World.session.actor`: the capture path
/// doesn't, and a retrieval side that guessed a different owner than the write
/// side would look like an empty store rather than a misconfiguration.
pub fn recall_owner(world: &World) -> String {
    world
        .profile
        .extra
        .get(RECALL_OWNER_KEY)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string()
}

/// Collapse transcript whitespace and clip to `max` characters.
///
/// Char-based, never byte-based: clipping a multi-byte han character in half
/// would panic on the slice, and the whole point of this crate is that CJK
/// works. Returns `(text, was_truncated)`.
pub(crate) fn clip(text: &str, max: usize) -> (String, bool) {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return (flat, false);
    }
    let mut out: String = flat.chars().take(max).collect();
    out.push('…');
    (out, true)
}

// ───── CJK query decomposition ────────────────────────────────────────────
//
// The SQLite backend routes han-heavy queries to an FTS5 `trigram` table, which
// matches *contiguous substrings*. That is exactly right for "支付服务" and
// exactly wrong for the way a user actually talks: "上次说的咖啡" is two ideas
// glued together and appears verbatim in no transcript, even though the session
// that says 我们上次说的那家咖啡馆 is obviously the one wanted. Chinese has no
// spaces, so the store cannot tell where one idea ends.
//
// Splitting the query is the tool's job, not the store's — the store is a
// faithful index and shouldn't guess. So: if the verbatim query misses and it
// contains han text, retry with non-overlapping character chunks of it, longest
// first, stopping at the first chunk size that finds anything. No dictionary,
// no segmenter, no per-language data to maintain.

/// Chunk sizes tried after a verbatim CJK miss, in order. 4 first because a
/// 4-character contiguous match in Chinese is nearly always meaningful.
///
/// **2 was here and was removed.** The backend routes to its trigram index only
/// at `count_cjk >= 3`; a 2-character chunk falls through to
/// `content LIKE '%…%'`, which is a full table scan — ~6.5 ms per 50 000 rows.
/// Because 2-char chunks are also the most numerous, a 12-han-character query
/// issued five such scans and cost 35 ms, against 170 µs for a miss that
/// stopped at width 3. Benchmarked, not guessed: `MAX_CJK_PROBES` bounds the
/// *number* of probes, which turns out to be nearly uncorrelated with cost —
/// what mattered was whether a probe could use the index at all. Dropping the
/// one width that never can removes the entire worst case, and it was the width
/// with the worst precision anyway.
const CJK_CHUNK_SIZES: [usize; 2] = [4, 3];

/// Ceiling on extra store round-trips per tool call. One user question must not
/// turn into a scan just because it was long.
///
/// Note this bounds round-trips, *not* time — see [`CJK_CHUNK_SIZES`] for why
/// that distinction bit us once already.
const MAX_CJK_PROBES: usize = 12;

fn is_han(c: char) -> bool {
    // Same range the SQLite backend uses to decide it should hit the trigram
    // table. Diverging here would mean chunking queries the store then routes
    // through a tokenizer that can't use them.
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

/// Non-overlapping `size`-char chunks taken from each maximal run of han
/// characters. Chunks shorter than two characters are dropped: a single han
/// character matches almost everything.
fn cjk_chunks(query: &str, size: usize) -> Vec<String> {
    let mut out = Vec::new();
    for run in query.split(|c: char| !is_han(c)) {
        let chars: Vec<char> = run.chars().collect();
        if chars.len() < 2 {
            continue;
        }
        for window in chars.chunks(size) {
            if window.len() >= 2 {
                let chunk: String = window.iter().collect();
                if chunk != query && !out.contains(&chunk) {
                    out.push(chunk);
                }
            }
        }
    }
    out
}

/// `RecallStore::search`, plus the CJK decomposition fallback described above.
/// Shared by the tool and the guide so both behave identically.
pub(crate) async fn search_with_cjk_fallback(
    store: &Arc<dyn RecallStore>,
    owner: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SessionHit>, RecallError> {
    // The verbatim query first: when it works it is strictly more precise than
    // anything a chunking heuristic produces. Its error is the one we surface —
    // it is the call that proves whether the store is reachable at all.
    let hits = store.search(owner, query, limit).await?;
    if !hits.is_empty() || !query.chars().any(is_han) {
        return Ok(hits);
    }

    let mut probes_left = MAX_CJK_PROBES;
    for size in CJK_CHUNK_SIZES {
        let mut merged: Vec<SessionHit> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for chunk in cjk_chunks(query, size) {
            if probes_left == 0 || merged.len() >= limit {
                break;
            }
            probes_left -= 1;
            // A single bad chunk must not sink the whole fallback; the store is
            // known healthy from the verbatim call above.
            for hit in store.search(owner, &chunk, limit).await.unwrap_or_default() {
                if seen.insert(hit.session.session_id.clone()) {
                    merged.push(hit);
                    if merged.len() >= limit {
                        break;
                    }
                }
            }
        }
        if !merged.is_empty() {
            return Ok(merged);
        }
        if probes_left == 0 {
            break;
        }
    }
    Ok(Vec::new())
}

/// Render "how long ago" without pulling a date library into a tool crate.
/// Coarse on purpose — the model needs "roughly when", not a timestamp it will
/// only misquote back to the user.
pub(crate) fn ago(then_ms: i64, now_ms: i64) -> String {
    let delta = (now_ms - then_ms).max(0);
    let mins = delta / 60_000;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 48 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
}

// ───── search_past_sessions ───────────────────────────────────────────────

/// Tool: search the agent's own past conversations.
///
/// Holds an `Arc<dyn RecallStore>` rather than a concrete backend, so a host
/// can swap FTS5 for a file store, a remote service, or a per-tenant store
/// without this tool knowing. State-bearing, so it is constructed at wiring
/// time rather than declared with the `#[tool]` macro.
pub struct SearchPastSessions {
    store: Arc<dyn RecallStore>,
    schema: ToolSchema,
    /// Set for single-tenant hosts that never populate `profile.extra`.
    owner: Option<String>,
    snippet_chars: usize,
    total_snippet_chars: usize,
}

impl SearchPastSessions {
    pub fn new(store: Arc<dyn RecallStore>) -> Self {
        Self {
            store,
            owner: None,
            snippet_chars: SNIPPET_CHARS,
            total_snippet_chars: TOTAL_SNIPPET_CHARS,
            schema: ToolSchema {
                name: "search_past_sessions".into(),
                description:
                    "Search YOUR OWN past conversations with this user and return matching \
                     excerpts. Reach for this whenever the user points at something outside \
                     the current conversation: \"上次我们说的…\" / \"我之前让你…\" / \"你还记得…\" / \
                     \"as we discussed\" / \"like last time\" / \"the thing I mentioned before\"; \
                     or when you need a decision, preference, name, number or file path that \
                     was established in an earlier session and is not in the context you can \
                     see. Query with the user's own words — the index handles Chinese. \
                     Returns session ids, timestamps and short snippets, best match first. \
                     An empty result is a real answer: it means nothing was recorded. Do not \
                     call this again with the same query; either ask the user or say you \
                     don't have it."
                        .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "What to look for, in the user's own wording. Keywords work better than full questions."
                        },
                        "limit": {
                            "type": "integer",
                            "default": DEFAULT_LIMIT,
                            "minimum": 1,
                            "maximum": MAX_LIMIT,
                            "description": "Max sessions to return. Leave at the default unless you are surveying."
                        }
                    },
                    "required": ["query"]
                }),
            },
        }
    }

    /// Pin the owner instead of reading it from the world profile. For
    /// single-user desktop/CLI hosts where there is no profile to populate.
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    /// Override the per-hit and total snippet budgets. Raise them only if you
    /// know the context window can absorb 20 hits at the new size; see
    /// [`SNIPPET_CHARS`] / [`TOTAL_SNIPPET_CHARS`] for the arithmetic.
    pub fn with_snippet_budget(mut self, per_hit: usize, total: usize) -> Self {
        self.snippet_chars = per_hit;
        self.total_snippet_chars = total;
        self
    }

    fn owner_for(&self, world: &World) -> String {
        self.owner.clone().unwrap_or_else(|| recall_owner(world))
    }

    /// Shape one hit for the model, spending from the shared snippet budget.
    ///
    /// `RecallStore` gives no relevance score, so `rank` (1-based, best first)
    /// is the only ordering signal we can honestly expose — see the crate docs.
    fn render_hit(&self, rank: usize, hit: &SessionHit, now_ms: i64, budget: &mut usize) -> Value {
        // The anchor's own timestamp is when the thing was actually said; the
        // session's start can be days earlier in a long-running conversation.
        let matched_at_ms = hit
            .around
            .iter()
            .find(|m| m.id == hit.anchor_id)
            .map(|m| m.ts_ms)
            .unwrap_or(hit.session.started_at_ms);
        let role = hit
            .around
            .iter()
            .find(|m| m.id == hit.anchor_id)
            .map(|m| m.role.clone());

        let allow = self.snippet_chars.min(*budget);
        let (snippet, clipped) = if allow == 0 {
            (String::new(), true)
        } else {
            clip(&hit.snippet, allow)
        };
        *budget = budget.saturating_sub(snippet.chars().count());

        json!({
            "rank": rank,
            "session_id": hit.session.session_id,
            "title": hit.session.title,
            "source": hit.session.source,
            "started_at_ms": hit.session.started_at_ms,
            "matched_at_ms": matched_at_ms,
            "when": ago(matched_at_ms, now_ms),
            "message_count": hit.session.message_count,
            "anchor_id": hit.anchor_id,
            "anchor_role": role,
            "snippet": snippet,
            "snippet_truncated": clipped,
        })
    }
}

#[async_trait]
impl Tool for SearchPastSessions {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    /// Pure read. Same classification as `list_memories`: it touches an
    /// operator-owned store the agent already wrote, and repeating it changes
    /// nothing.
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }

    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "search_past_sessions".into(),
                reason: "query required (non-empty)".into(),
            })?
            .to_string();

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_LIMIT as u64)
            .clamp(1, MAX_LIMIT as u64) as usize;

        let owner = self.owner_for(world);
        let now_ms = world.clock.now_ms();

        let hits = match search_with_cjk_fallback(&self.store, &owner, &query, limit).await {
            Ok(h) => h,
            // A backend failure is a fact about the world, not a bad call. Hand
            // it back as ok:false so the agent can route around it instead of
            // having its turn aborted by a ToolError.
            Err(e) => {
                tracing::warn!(target: "harness::recall", error = %e, "recall search failed");
                return Ok(ToolResult {
                    ok: false,
                    content: json!({
                        "error": e.to_string(),
                        "hint": "the recall store is unavailable; answer from what you have, or ask the user",
                    }),
                    trace: Some(format!("search_past_sessions({query}) → error: {e}")),
                });
            }
        };

        // A genuine miss is a valid answer. Returning ok:false here is what
        // makes models retry-loop on the same query, so: ok:true, an explicit
        // `found: false`, and an instruction not to try again.
        if hits.is_empty() {
            return Ok(ToolResult {
                ok: true,
                content: json!({
                    "query": query,
                    "found": false,
                    "count": 0,
                    "hits": [],
                    "note": "No past session mentions this. Nothing was recorded — this is a \
                             complete answer, not a failure. Do not search again for the same \
                             thing; ask the user, or tell them you have no record of it.",
                }),
                trace: Some(format!("search_past_sessions({query}) → no hits")),
            });
        }

        let mut budget = self.total_snippet_chars;
        let rendered: Vec<Value> = hits
            .iter()
            .enumerate()
            .map(|(i, h)| self.render_hit(i + 1, h, now_ms, &mut budget))
            .collect();
        let elided = rendered
            .iter()
            .filter(|h| h["snippet"].as_str().is_some_and(str::is_empty))
            .count();

        let trace = {
            let heads: Vec<String> = rendered
                .iter()
                .take(3)
                .map(|h| {
                    format!(
                        "{} ({})",
                        h["session_id"].as_str().unwrap_or("?"),
                        h["when"].as_str().unwrap_or("?")
                    )
                })
                .collect();
            format!(
                "search_past_sessions({query}) → {} session(s): {}",
                rendered.len(),
                heads.join(", ")
            )
        };

        let mut content = json!({
            "query": query,
            "found": true,
            "count": rendered.len(),
            "hits": rendered,
        });
        if elided > 0 {
            content["note"] = json!(format!(
                "{elided} lower-ranked hit(s) had their snippet dropped to stay inside the \
                 context budget; re-run with a narrower query to see them."
            ));
        }

        Ok(ToolResult {
            ok: true,
            content,
            trace: Some(trace),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_context::default_world;
    use harness_core::{RecallMessage, SessionMeta};
    use harness_recall_sqlite::SqliteRecall;

    async fn store_with(msgs: &[(&str, &str, &str)]) -> Arc<dyn RecallStore> {
        let store: Arc<dyn RecallStore> = Arc::new(SqliteRecall::open_in_memory().unwrap());
        let mut ts = 1_000_000;
        for (session, role, content) in msgs {
            store
                .ensure_session("alice", session, &SessionMeta::new(*session, ts))
                .await
                .unwrap();
            store
                .append("alice", session, &RecallMessage::new(*role, *content, ts))
                .await
                .unwrap();
            ts += 1_000;
        }
        store
    }

    fn world_for(owner: &str) -> World {
        let mut w = default_world(".");
        w.profile
            .extra
            .insert(RECALL_OWNER_KEY.into(), json!(owner));
        w
    }

    #[tokio::test]
    async fn a_hit_carries_session_id_snippet_and_when() {
        let store = store_with(&[
            ("s1", "user", "let's use postgres for the billing service"),
            ("s2", "user", "unrelated chatter about the weather"),
        ])
        .await;
        let tool = SearchPastSessions::new(store);

        let out = tool
            .invoke(
                json!({"query": "postgres billing"}),
                &mut world_for("alice"),
            )
            .await
            .unwrap();

        assert!(out.ok);
        assert_eq!(out.content["found"], true);
        assert_eq!(out.content["count"], 1);
        let hit = &out.content["hits"][0];
        assert_eq!(hit["session_id"], "s1");
        assert_eq!(hit["rank"], 1);
        assert_eq!(hit["anchor_role"], "user");
        assert!(hit["snippet"].as_str().unwrap().contains("postgres"));
        assert!(hit["when"].as_str().unwrap().ends_with("ago"));
        assert!(out.trace.unwrap().contains("s1"));
    }

    #[tokio::test]
    async fn a_miss_is_ok_true_with_an_explicit_nothing_found_payload() {
        let store = store_with(&[("s1", "user", "we talked about kubernetes")]).await;
        let tool = SearchPastSessions::new(store);

        let out = tool
            .invoke(
                json!({"query": "quarterly revenue forecast"}),
                &mut world_for("alice"),
            )
            .await
            .unwrap();

        // The contract that stops retry loops: not an error, explicitly empty,
        // and the payload tells the model not to come back.
        assert!(out.ok, "a genuine miss must not be an error");
        assert_eq!(out.content["found"], false);
        assert_eq!(out.content["count"], 0);
        assert_eq!(out.content["hits"].as_array().unwrap().len(), 0);
        assert!(
            out.content["note"]
                .as_str()
                .unwrap()
                .contains("Do not search again")
        );
    }

    #[tokio::test]
    async fn owner_scoping_makes_another_users_sessions_a_miss() {
        let store = store_with(&[("s1", "user", "my api key rotation policy")]).await;
        let tool = SearchPastSessions::new(store);

        let out = tool
            .invoke(json!({"query": "api key rotation"}), &mut world_for("bob"))
            .await
            .unwrap();
        assert!(out.ok);
        assert_eq!(out.content["found"], false);
    }

    #[tokio::test]
    async fn limit_is_capped_and_defaulted() {
        // 30 distinct sessions all matching the same term.
        let owned: Vec<(String, &str, String)> = (0..30)
            .map(|i| (format!("s{i}"), "user", format!("deploy step number {i}")))
            .collect();
        let msgs: Vec<(&str, &str, &str)> = owned
            .iter()
            .map(|(s, r, c)| (s.as_str(), *r, c.as_str()))
            .collect();
        let store = store_with(&msgs).await;
        let tool = SearchPastSessions::new(store);

        // Default when omitted.
        let out = tool
            .invoke(json!({"query": "deploy"}), &mut world_for("alice"))
            .await
            .unwrap();
        assert_eq!(out.content["count"], DEFAULT_LIMIT);

        // Absurd request clamps to MAX_LIMIT rather than erroring.
        let out = tool
            .invoke(
                json!({"query": "deploy", "limit": 500}),
                &mut world_for("alice"),
            )
            .await
            .unwrap();
        assert_eq!(out.content["count"], MAX_LIMIT);

        // Zero clamps up to 1 — a store call that can never return anything is
        // worse than a small one.
        let out = tool
            .invoke(
                json!({"query": "deploy", "limit": 0}),
                &mut world_for("alice"),
            )
            .await
            .unwrap();
        assert_eq!(out.content["count"], 1);
    }

    #[tokio::test]
    async fn snippets_respect_the_per_hit_and_total_budgets() {
        let long = format!("needle {}", "x".repeat(4_000));
        let store = store_with(&[("s1", "user", long.as_str())]).await;

        let tool = SearchPastSessions::new(store).with_snippet_budget(50, 50);
        let out = tool
            .invoke(json!({"query": "needle"}), &mut world_for("alice"))
            .await
            .unwrap();

        let snip = out.content["hits"][0]["snippet"].as_str().unwrap();
        // 50 chars + the single ellipsis marker.
        assert!(
            snip.chars().count() <= 51,
            "per-hit cap not applied: {snip}"
        );
        assert_eq!(out.content["hits"][0]["snippet_truncated"], true);
    }

    #[tokio::test]
    async fn the_total_budget_elides_snippets_of_lower_ranked_hits() {
        let owned: Vec<(String, &str, String)> = (0..6)
            .map(|i| {
                (
                    format!("s{i}"),
                    "user",
                    format!("checkpoint marker {i} {}", "y".repeat(300)),
                )
            })
            .collect();
        let msgs: Vec<(&str, &str, &str)> = owned
            .iter()
            .map(|(s, r, c)| (s.as_str(), *r, c.as_str()))
            .collect();
        let store = store_with(&msgs).await;

        // Room for two full snippets, then the budget is gone.
        let tool = SearchPastSessions::new(store).with_snippet_budget(100, 200);
        let out = tool
            .invoke(
                json!({"query": "checkpoint marker", "limit": 6}),
                &mut world_for("alice"),
            )
            .await
            .unwrap();

        let hits = out.content["hits"].as_array().unwrap();
        assert!(hits.len() > 2);
        // Metadata survives even when the snippet is dropped, so the model can
        // still narrow the query against a known session id.
        let last = hits.last().unwrap();
        assert_eq!(last["snippet"], "");
        assert!(last["session_id"].as_str().unwrap().starts_with('s'));
        assert!(
            out.content["note"]
                .as_str()
                .unwrap()
                .contains("snippet dropped")
        );
    }

    #[tokio::test]
    async fn cjk_queries_match_through_the_trigram_index() {
        // Han text has no spaces, so the plain FTS5 tokenizer sees one giant
        // token and the trigram table does contiguous-substring matching
        // instead. "上次说的咖啡" appears verbatim nowhere, so the verbatim
        // probe misses and the chunk fallback is what actually lands the hit on
        // "我们上次说的那家咖啡馆". This is the query shape users actually type.
        let store = store_with(&[
            ("zh1", "user", "我们上次说的那家咖啡馆在南京西路"),
            ("zh2", "assistant", "好的，明天帮你订会议室"),
        ])
        .await;
        let tool = SearchPastSessions::new(store);

        let out = tool
            .invoke(json!({"query": "上次说的咖啡"}), &mut world_for("alice"))
            .await
            .unwrap();

        assert!(out.ok);
        assert_eq!(
            out.content["found"], true,
            "CJK query must hit: {:?}",
            out.content
        );
        assert_eq!(out.content["hits"][0]["session_id"], "zh1");
        assert!(
            out.content["hits"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("咖啡")
        );
    }

    #[tokio::test]
    async fn a_contiguous_cjk_query_still_hits_verbatim() {
        // The fallback must not be load-bearing for queries the store already
        // handles — this one is a literal substring, so it comes back on the
        // first probe with no chunking at all.
        let store = store_with(&[("zh1", "user", "我们明天要上线支付服务")]).await;
        let tool = SearchPastSessions::new(store);
        let out = tool
            .invoke(json!({"query": "支付服务"}), &mut world_for("alice"))
            .await
            .unwrap();
        assert_eq!(out.content["found"], true);
    }

    #[tokio::test]
    async fn cjk_chunking_covers_the_query_without_overlap() {
        assert_eq!(cjk_chunks("上次说的咖啡", 4), vec!["上次说的", "咖啡"]);
        assert_eq!(cjk_chunks("上次说的咖啡", 3), vec!["上次说", "的咖啡"]);
        // Punctuation and latin text break runs; single-char leftovers are
        // dropped because one han character matches nearly everything.
        assert_eq!(cjk_chunks("咖啡, and 茶叶馆", 3), vec!["咖啡", "茶叶馆"]);
        // A chunk identical to the query is never re-probed — that call was
        // already made verbatim and already missed.
        assert!(cjk_chunks("咖啡", 4).is_empty());
    }

    #[tokio::test]
    async fn clipping_counts_characters_not_bytes() {
        // Byte slicing 中文 would panic; this is the regression guard.
        let (out, truncated) = clip("咖啡馆在南京西路", 3);
        assert!(truncated);
        assert_eq!(out, "咖啡馆…");
    }

    #[tokio::test]
    async fn empty_query_is_an_arg_error_not_a_full_table_scan() {
        let store = store_with(&[("s1", "user", "anything")]).await;
        let tool = SearchPastSessions::new(store);
        let err = tool
            .invoke(json!({"query": "   "}), &mut world_for("alice"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn pinned_owner_wins_over_the_profile() {
        let store = store_with(&[("s1", "user", "pinned owner check")]).await;
        let tool = SearchPastSessions::new(store).with_owner("alice");
        // World says bob; the pin says alice, and the pin is what a single-user
        // host configured on purpose.
        let out = tool
            .invoke(json!({"query": "pinned owner"}), &mut world_for("bob"))
            .await
            .unwrap();
        assert_eq!(out.content["found"], true);
    }
}
