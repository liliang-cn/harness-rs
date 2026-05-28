# Investor bot — "Market & Advice" panel (Spec 2)

**Date:** 2026-05-28
**Status:** ⛔ **SHELVED at spec review (2026-05-28) — YAGNI.** Not building this.

> **Decision:** the chat agent already has Gemini **search grounding**, so it can
> answer macro/investment questions ("给我宏观投资建议", "现在 CPI?", "Buffett 最近
> 持仓?") **live, today, with zero new code**. The entire subsystem below (daily
> cron + grounded research + structuring pass + `macro_snapshots` table +
> per-user advice cache + dashboard panel + `get_macro_brief` tool) exists only
> to make it **proactive** (an always-on home panel) and to save tokens/accumulate
> history. We decided that **reactive (just ask in chat, live-grounded)** is
> enough for now. This doc is kept as the rationale for *not* building it, and as
> a ready design if we later want the proactive home panel.
>
> **What we'll do instead (minimal):** optionally a one-line SYSTEM_PROMPT nudge so
> the agent reliably grounds for macro/investment questions + factors in the
> user's portfolio, and a "今日宏观建议" chat-prefill button (reuses `openChatWith`).
> No backend, no cron, no tables.

**Part of:** the Dashboard product. Integrates into the dashboard **home** (`/app`).
Builds on the AI-artifact work only loosely — this is a **fixed panel**, not an
ad-hoc artifact.

## Vision

On the dashboard home, show a daily **market briefing + personalized advice**:
the macro backdrop (US CPI, Fed funds, 2Y/10Y treasury yields, Nasdaq/S&P) plus
**"smart-money" moves** (Berkshire/Buffett 13F changes, other notable fund moves,
1–2 market headlines), then — given the user's own holdings — what it may mean
for them. Research comes from **Gemini search grounding** (no external macro
API); everything is **cached in SQLite** (no Redis).

## Decisions locked (from brainstorming)

1. **Surface:** a fixed React panel on the dashboard home (`Dashboard.tsx`), below
   net-worth/composition. NOT an AI artifact (the home stays stable). Carries a
   persistent "仅供参考，非投资建议" disclaimer.
2. **Data source:** Gemini grounding only; cached in SQLite (reuse `quote_cache`).
3. **Compute model:** **global-daily snapshot + per-user-lazy advice** (below) +
   a manual refresh button. No per-visit live regen.
4. **Output:** narrative + key-number **cards** (no historical charts — grounding
   is unreliable for time-series).

## Two-layer architecture

### Layer 1 — Global macro snapshot (shared, daily)
A daily cron (reuse the `fx::spawn_refresher` / `net_worth::spawn_snapshot_cron`
startup-cron pattern in `main.rs:883`) produces ONE shared snapshot per day:

- **Pass A — grounded research (Gemini):** a free-form **grounded** query (search
  grounding ON, like `gemini_grounded_price` in `quotes.rs:432` but free-form
  text) asks for the macro numbers + smart-money moves + headlines. Returns raw
  text + source URLs.
- **Pass B — structuring (cheap model, no grounding):** a second LLM call reshapes
  Pass A's text into the `MacroSnapshot` JSON (cards + bullets). Two passes
  because **Gemini cannot combine `google_search` grounding with JSON-schema /
  structured-output mode in one call** — so we ground as text, then structure
  separately.

**Persisted (not just TTL-cached)** so the data is durable + queryable from chat,
and so daily rows **accumulate a real history over time**. New table:

```sql
CREATE TABLE IF NOT EXISTS macro_snapshots (
    snapshot_date TEXT PRIMARY KEY,   -- 'YYYY-MM-DD' (one row/day)
    json          TEXT NOT NULL,      -- serialized MacroSnapshot
    created_at    TEXT NOT NULL
);
```
The cron upserts today's row; the panel + chat tool read the latest row. "Stale"
= today's row missing (the cron, or a `GET` fallback, fills it). Old rows are
kept (cheap; future trend views can read the series).

```rust
struct MacroSnapshot {
    as_of: String,
    indicators: Vec<Indicator>,   // { label, value, note }
    smart_money: Vec<String>,     // bullet highlights
    sources: Vec<String>,         // URLs from grounding
}
struct Indicator { label: String, value: String, note: String }
```

### Layer 2 — Per-user advice (personalized, lazy)
Generated when a user opens the dashboard and their advice is stale (>1 day):
an LLM synthesis (cheap model, no grounding) combining the cached
`MacroSnapshot` + the user's holdings (`positions_with_prices`, server.rs:2234) +
net worth (`latest_net_worth_snapshot`, db.rs:655) → a short markdown advice
(e.g. "your ~86% gold/commodity tilt + rising 10Y → …"). Cached per-user under
`macro:advice:<uid>` (1-day TTL). **Reuses the global snapshot — no re-grounding
per user.**

### Endpoint
- `GET /api/macro/brief` → returns `MacroBrief { snapshot: MacroSnapshot, advice_md: String, advice_as_of: String }`. Reads the latest `macro_snapshots` row; generates/caches the per-user advice if stale. If no snapshot row exists yet (first boot before the cron ran), trigger Pass A+B inline once (and persist).
- `POST /api/macro/brief/refresh` → force a re-run of the snapshot (if stale) + this user's advice. Backs the panel's 刷新 button.

### Layer 3 — Chat access (read the saved data)
A read-only agent tool `get_macro_brief` returns the **latest saved**
`MacroSnapshot` (indicators + smart_money + sources + as_of) from
`macro_snapshots`, so chat questions — "现在 CPI 多少？", "Buffett 最近的持仓变化？",
"给我宏观投资建议" — are answered from the **saved daily data** (cheap, consistent
with the panel) instead of re-grounding every time. The agent still has its own
Gemini grounding for follow-ups, but `get_macro_brief` is the cheap default for
"what's the current macro picture."
- Optional arg `date?` to fetch a specific past day's row (history is accumulating).
- **Must be added to the `TOOL_NAMES` allowlist in `main.rs`** — registering the
  `#[harness::tool]` is not enough; `collect_tools()` (main.rs) filters by
  `TOOL_NAMES`, so an unlisted tool never reaches the model (this bit us before —
  the whole project/note/render_artifact set was missing from it). The SYSTEM_PROMPT gets a line:
  for macro/market/Buffett/"投资建议" questions, call `get_macro_brief` first.

### Models
Grounded research (Pass A) **must** use Gemini (search grounding). Structuring
(Pass B) + advice (Layer 2) use the cheap brief model (`deepseek-v4-flash` or
the server default) — no grounding needed there.

## Frontend
- `Dashboard.tsx`: a new **`MarketAdvice`** section/component — renders the
  indicator cards, a "smart money" bullet list, the advice markdown
  (`renderMarkdown`), source links, an `as_of` timestamp, a 刷新 button, and the
  disclaimer. Loads **async** (its own fetch) so it never blocks the rest of the
  home; shows a skeleton while loading and a friendly empty/"generating" state if
  no snapshot yet.
- `lib/api.ts`: `macroBrief()` → GET; `macroBriefRefresh()` → POST.
- i18n: `market.*` keys (title, refresh, disclaimer, empty, smartMoney) in en/zh.

## Reuse map
- Gemini grounded call shape + key resolution: `quotes.rs` (`gemini_grounded_price`,
  `gemini_api_key`). Adapt to free-form text + JSON.
- Snapshot persistence: new `macro_snapshots` table (one row/day) + `Db` helpers
  `latest_macro_snapshot()` / `get_macro_snapshot(date)` / `put_macro_snapshot(date, json)`.
- Per-user advice cache: `Db::get_cached_quote`/`put_cached_quote` (db.rs:432/450),
  key `macro:advice:<uid>`, 1-day TTL (advice is ephemeral; no need to persist long-term).
- Tool registration: new `get_macro_brief` in `tools.rs` **and** in `TOOL_NAMES` (main.rs).
- Cron: the startup-cron pattern (`main.rs:883`).
- Typed output for `MacroSnapshot` structuring: the `BriefReport` /
  `run_typed_with_max_iters` pattern (server.rs:1374) — or a tolerant manual JSON
  parse (no grounding in Pass B, so structured output is fine here).
- Portfolio: `positions_with_prices`, `latest_net_worth_snapshot`.

## Error handling
- Grounding fails / no key → serve last cached snapshot + "数据可能延迟"; if none,
  the panel shows a friendly empty state. Never panic, never block the home.
- Pass B (structuring) fails → fall back to showing Pass A's raw text as the
  summary (degraded but useful).
- Advice synthesis fails → show the snapshot without the advice paragraph.
- All `quote_cache` writes are best-effort (`let _ =`).

## Testing
- `cargo test -p dashboard`: `MacroSnapshot`/`MacroBrief` (de)serialize;
  `put_macro_snapshot` → `latest_macro_snapshot`/`get_macro_snapshot(date)`
  round-trip; per-user advice cache TTL staleness; a pure
  `build_advice_prompt(snapshot, positions, net_worth) -> String` helper (asserts
  it includes the holdings + key numbers).
- Manual (real keys): run the snapshot cron once → `GET /api/macro/brief` returns
  indicators + smart-money; open the dashboard home → panel renders; 刷新 re-runs;
  a second user reuses the same snapshot (no re-grounding); in chat, ask "现在
  CPI 多少？" → agent calls `get_macro_brief` and answers from saved data.

## Non-goals
- No historical **charts** in v1 (but saved daily snapshots accumulate a real
  series, so trend charts become possible later without grounding).
- No per-visit live regeneration; no proactive push/notifications.
- No trade execution; no external macro/data-vendor API.
- Advice is daily-cached per user, not real-time.

## Rollout
Backend (cron + endpoint + cache) + frontend panel, all in `examples/dashboard`.
The daily snapshot cron runs inside `--serve` (like fx/net-worth). Standard musl
→ qc-jp deploy; touch `server.rs` + rebuild dist (UI changed). Gemini key already
configured on prod.
