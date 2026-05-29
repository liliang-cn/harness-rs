# Daily Digest Push — Design

**Status:** Approved (brainstorming) → ready for plan
**Date:** 2026-05-29
**Product:** Dashboard (`examples/dashboard`)

## Goal

A proactive daily digest the user opts into. At a user-chosen local time, the
dashboard generates a short digest (yesterday's spending, the user's wealth
movement, and a shared market take on gold / bitcoin / a stock index) and
delivers it via **in-app notification** and/or **email** (Resend).

## Scope decisions (from brainstorming)

- **Channels:** both in-app + email. User picks per their settings.
- **Email transport:** Resend HTTP API (`api.resend.com/emails`) over HTTPS via
  the existing `reqwest` dep. One secret (`RESEND_API_KEY`) + a verified
  from-address. No SMTP, no new crate.
- **Sections (all four):** yesterday's spending, the user's portfolio day,
  net-worth delta, market conclusions.
- **Timezone:** per-user IANA timezone (add `chrono-tz`). User picks send time
  (HH:MM) + timezone.
- **Scheduling:** in-process tokio cron at server startup (mirrors
  `net_worth::spawn_snapshot_cron`), ticking every ~15 min. Not an external
  daemon — the digest needs live DB access and writes in-app notifications.
- **Market brief:** generated once per UTC day, cached in a table, reused by
  every user's digest that day (global-daily + per-user split).

## Defaults (adjustable)

- Opt-in: `digest_enabled` defaults **off**.
- Tiers: available to **all** logged-in users (market generation is one
  Gemini-grounding call per day globally — negligible cost).
- Digest body language: **Chinese** (the operator's language). Market
  conclusions are prompted to Gemini in Chinese.
- Cadence: **daily** only (no weekly in v1).

## Architecture

```
                          spawn_digest_cron (every 15 min)
                                     |
                 for each digest_enabled user:
                   now_utc -> user IANA tz -> local time/date
                   is_due(now_utc, tz, digest_time, last_digest_date)?
                                     | yes
              ensure today's market brief (lazy gen + cache)
                                     |
                        build_digest(user) -> Digest
                                     |
              +----------------------+----------------------+
              | channel in {in_app, both}    channel in {email, both}
        deliver_in_app                          deliver_email
        (insert notifications row)        (render HTML -> Resend POST)
                                     |
                     set users.last_digest_date = local_date
```

`build_digest` is pure generation; the two `deliver_*` functions are thin
adapters, so generation is testable without delivery.

## Data model

All migrations use the existing `ensure_column` / `CREATE TABLE IF NOT EXISTS`
pattern in `db.rs`.

### `digest_settings` — per-user digest config (dedicated table)
One row per user, created lazily on first PATCH. A missing row means "disabled
with defaults" — keeps the `users` table and `row_to_user` untouched.
```sql
CREATE TABLE IF NOT EXISTS digest_settings (
  user_id          TEXT PRIMARY KEY,
  enabled          INTEGER NOT NULL DEFAULT 0,   -- opt-in
  send_time        TEXT NOT NULL DEFAULT '08:00',-- HH:MM, user-local
  timezone         TEXT NOT NULL DEFAULT 'UTC',  -- IANA, e.g. Asia/Shanghai
  channel          TEXT NOT NULL DEFAULT 'in_app',-- in_app | email | both
  last_digest_date TEXT,                          -- user-local YYYY-MM-DD already sent (dedup)
  updated_at       INTEGER NOT NULL
);
```

### `notifications` — in-app inbox
```sql
CREATE TABLE IF NOT EXISTS notifications (
  id          TEXT PRIMARY KEY,
  user_id     TEXT NOT NULL,
  kind        TEXT NOT NULL,        -- 'digest'
  title       TEXT NOT NULL,
  body        TEXT NOT NULL,        -- JSON Digest payload (for rich render)
  created_at  INTEGER NOT NULL,     -- epoch seconds
  read_at     INTEGER               -- NULL = unread
);
CREATE INDEX IF NOT EXISTS idx_notifications_user ON notifications(user_id, read_at);
```

### `daily_market_brief` — shared per-day market take
```sql
CREATE TABLE IF NOT EXISTS daily_market_brief (
  day         TEXT PRIMARY KEY,     -- UTC YYYY-MM-DD
  body        TEXT NOT NULL,        -- JSON: { gold, btc, index: {price, conclusion}, summary }
  created_at  INTEGER NOT NULL
);
```

## Cron logic

`spawn_digest_cron(db, cfg)` — spawned once at server startup in `--serve`.

- Loop: run a tick, then `sleep(15 min)`. Also run one tick shortly after boot.
- Each tick:
  1. `now_utc = Utc::now()`.
  2. Load opted-in users (`digest_enabled = 1`).
  3. For each user (with a `digest_settings` row, `enabled = 1`):
     - Parse `timezone` as `chrono_tz::Tz`; on parse error fall back to `UTC`
       and log WARN.
     - `local = now_utc.with_timezone(&tz)`; `local_date = local.date()`.
     - `is_due` = `local_date != last_digest_date` AND
       `local.time() >= parse(send_time)`. (Catch-up: a server that was down
       at the exact minute still sends later the same local day.)
     - If due: ensure today's market brief (UTC day), `build_digest`, deliver
       per channel, set `last_digest_date = local_date`.

`is_due(now_utc, tz, send_time, last_digest_date) -> bool` is extracted as a
pure function for unit testing.

## Generation — `build_digest(user) -> Digest`

`Digest` struct (serializable; stored as `notifications.body` JSON and rendered
to email HTML):

```rust
struct Digest {
    date: String,                 // user-local date the digest covers
    spending: SpendingSection,    // yesterday
    wealth: WealthSection,
    market: Option<MarketBrief>,  // None if generation failed today
}
struct SpendingSection { total: f64, currency: String, by_category: Vec<(String, f64)> }
struct WealthSection { net_worth: f64, net_delta: f64, cash: f64, investments: f64, investments_delta: f64, debt: f64, currency: String }
struct MarketBrief { gold: Quote, btc: Quote, index: Quote, summary: String }
struct Quote { name: String, price: String, conclusion: String }
```

- **Spending (yesterday):** `list_transactions(user, from, to)` where `[from,to)`
  is the user-local *previous* day converted to epoch. Aggregate expenses by
  category in the user's `base_currency`. No LLM.
- **Wealth:** computed entirely from the two latest `net_worth_snapshots` rows
  (already computed daily). `net_worth`, `cash`, `investments`, `debt` from the
  latest; `net_delta` and `investments_delta` = latest minus the prior row.
  This covers both "net-worth delta" and "portfolio's day" with zero recompute.
  Per-asset "biggest mover" / cost-basis unrealized needs intraday history we
  don't store — deferred.
- **Market:** read from `daily_market_brief`.

## Market brief generation

`ensure_market_brief(db, cfg, utc_day) -> Option<MarketBrief>`:
- If a row for `utc_day` exists, return it.
- Else call Gemini grounding (reuse the `gemini_grounded_*` path in
  `portfolio/quotes.rs`) with a Chinese prompt asking for: gold (Au9999 ¥/克 or
  spot), bitcoin (USD), a stock index (Nasdaq), each with current level + a
  one-line trend conclusion, plus a 1–2 sentence overall summary. Parse into
  `MarketBrief`, store, return.
- On failure: log WARN, return `None` (digest still sends with 3 sections).

Gemini cannot combine grounding + JSON-schema output in one call, so parse the
grounded text response into the struct (tolerant parsing; on parse failure treat
as generation failure).

## Delivery

- `deliver_in_app(db, user, &digest)`: insert a `notifications` row
  (`kind='digest'`, `title` = localized e.g. "今日简报", `body` = `Digest` JSON,
  `created_at = now`, `read_at = NULL`).
- `deliver_email(cfg, user, &digest)`: render `Digest` to an HTML template
  (inline-styled, mobile-friendly), POST to `https://api.resend.com/emails`
  with `Authorization: Bearer {resend_api_key}` and body
  `{ from: digest_from, to: [user.email], subject, html }`. Non-2xx → log WARN.

Email secrets are read from env, mirroring how `quotes.rs::gemini_api_key()`
reads `GEMINI_API_KEY` (deploy-time secrets, not admin-hot-reload material):
`RESEND_API_KEY` and `DIGEST_FROM` (e.g. `"Dashboard <digest@superleo.app>"`).
If `RESEND_API_KEY` is unset/empty, email is skipped with a WARN; in-app
delivery is unaffected.

## API

- `PATCH /api/me/digest-settings` — body
  `{ enabled, time, timezone, channel }`; validates time `HH:MM`, timezone is a
  known IANA name, channel in the enum. Persists to `users`.
- `GET /api/me` — extend response with the digest settings so the UI can
  hydrate the form.
- `GET /api/me/notifications?unread=bool` — list this user's notifications
  (newest first, capped, e.g. 50).
- `POST /api/me/notifications/read` — body `{ ids?: [..] }`; if `ids` omitted,
  mark all of the user's notifications read. Sets `read_at = now`.

## Frontend

- **Profile page** — new "Daily digest" card (`profile/digest-card.tsx`):
  enable `Switch`, time picker (`<input type=time>` or HH:MM select), timezone
  `Select` (common IANA list with a "detect from browser" default), channel
  `Select`. Saves via `PATCH /api/me/digest-settings`; toast on success.
- **App-shell header** — bell icon (`components/notifications/bell.tsx`) with an
  unread-count badge. Click opens a popover/sheet listing notifications
  (rendered from the `Digest` JSON); opening calls `POST .../read` to clear the
  badge. Poll `GET .../notifications?unread=true` on app load + a light interval
  (e.g. every 5 min) or on window focus.
- i18n: add `digest.*` and `notifications.*` keys to `en.json` / `zh.json`.

## Error handling

| Failure | Behavior |
|---|---|
| Bad `digest_timezone` | fall back to UTC, log WARN, still send |
| Market brief gen fails | digest sends with 3 sections, `market = None` |
| Resend non-2xx / network error | log WARN, in-app unaffected, still set `last_digest_date` (no retry storm) |
| `resend_api_key` absent | skip email with WARN; in-app still delivered |
| No snapshot yet (new user) | wealth Δ shown as 0 / "建立中" |

## Testing

- `is_due(now_utc, tz, digest_time, last_digest_date)` — pure fn: before time,
  after time, already-sent-today, tz rollover, bad tz → UTC.
- yesterday spending aggregation — date-range + category fold in user tz.
- `ensure_market_brief` — generates once, second call hits cache (no second
  Gemini call); parse-failure path returns None and stores nothing.
- dedup — second tick same local day does not re-send (`last_digest_date`).
- Resend payload shape — `deliver_email` builds correct JSON body without
  hitting the network (inject a transport seam or assert on the constructed
  request).
- Settings PATCH — validation (bad time, unknown tz, bad channel) → 400;
  valid → persisted and reflected in `GET /api/me`.
- Frontend: digest card persists + hydrates; bell badge clears on read.

## Out of scope (v1)

- Weekly / custom cadences.
- Browser Web Push / native push.
- Per-asset "biggest mover" (needs intraday history).
- Unsubscribe link in email (opt-out is the in-app toggle; revisit if needed).
- Per-user digest language selection (fixed Chinese in v1).
