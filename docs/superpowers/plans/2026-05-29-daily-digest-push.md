# Daily Digest Push Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An opt-in daily digest (yesterday's spending + wealth movement + a shared gold/BTC/index market take) that fires at each user's chosen local time and is delivered in-app and/or via Resend email.

**Architecture:** A new `digest` module owns generation, scheduling, and delivery. An in-process tokio cron (`spawn_digest_cron`, mirroring `net_worth::spawn_snapshot_cron`) ticks every 15 min, and for each opted-in user checks a pure `is_due` predicate against their IANA timezone. The shared market brief is generated once per UTC day via Gemini grounding and cached in SQLite. Per-user sections come from existing ledger + net-worth-snapshot data. Settings live in a dedicated `digest_settings` table; in-app digests live in a `notifications` table surfaced by a header bell.

**Tech Stack:** Rust, axum 0.7, rusqlite (bundled SQLite), chrono + chrono-tz (already deps), reqwest (already a dep), React 19 + Vite + shadcn + react-i18next, sonner.

**Spec:** `docs/superpowers/specs/2026-05-29-daily-digest-push-design.md`

**Conventions to follow:**
- DB tests use `Db::open_in_memory().unwrap()` (see `db.rs` test module) and the `tmp_db()` helper; tests are inline `#[cfg(test)] mod tests`.
- Run Rust tests with: `cargo test -p dashboard <filter>`.
- DB migrations: add `CREATE TABLE IF NOT EXISTS` to `Db::init` and/or `ensure_column` calls in the migration block near `db.rs:401`.
- Handlers: `Db` is opened per-request via `open_db()?` (`server.rs:1498`); auth via the `AuthCtx` extractor (`auth.user.id`, `auth.user.tier`, `auth.user.base_currency`, `auth.user.email`).
- Cron tasks open their own `Db::open(&db_path)` per tick (rusqlite `Connection` is `!Send` across awaits).
- Gemini grounding precedent: `portfolio/quotes.rs:432 gemini_grounded_price` + `gemini_api_key()` (env `GEMINI_API_KEY`).

---

### Task 1: DB layer — `digest_settings`, `notifications`, `daily_market_brief`

**Files:**
- Modify: `examples/dashboard/src/db.rs` (add tables to `init` near line 92; add migration calls near line 409; add structs near line 24; add methods)

- [ ] **Step 1: Add the three tables to `Db::init`**

In `src/db.rs`, inside the `execute_batch` string in `init` (after the `quote_cache` table block, around line 198), add:

```sql
-- Per-user daily-digest config. One row per user, created on first PATCH.
-- A missing row means "disabled with defaults".
CREATE TABLE IF NOT EXISTS digest_settings (
    user_id          TEXT PRIMARY KEY,
    enabled          INTEGER NOT NULL DEFAULT 0,
    send_time        TEXT NOT NULL DEFAULT '08:00',
    timezone         TEXT NOT NULL DEFAULT 'UTC',
    channel          TEXT NOT NULL DEFAULT 'in_app',
    last_digest_date TEXT,
    updated_at       INTEGER NOT NULL
);

-- In-app notification inbox. `body` is a JSON Digest payload.
CREATE TABLE IF NOT EXISTS notifications (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL,
    kind        TEXT NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    read_at     INTEGER
);
CREATE INDEX IF NOT EXISTS idx_notifications_user ON notifications(user_id, read_at);

-- Shared per-UTC-day market brief (gold/BTC/index). Generated once daily.
CREATE TABLE IF NOT EXISTS daily_market_brief (
    day         TEXT PRIMARY KEY,
    body        TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
```

- [ ] **Step 2: Add the `DigestSettings` and `NotificationRow` structs**

In `src/db.rs` near the other public structs (after `NetWorthSnapshot`, ~line 31), add:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DigestSettings {
    pub enabled: bool,
    pub send_time: String,        // "HH:MM"
    pub timezone: String,         // IANA
    pub channel: String,          // "in_app" | "email" | "both"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_digest_date: Option<String>,
}

impl Default for DigestSettings {
    fn default() -> Self {
        DigestSettings {
            enabled: false,
            send_time: "08:00".into(),
            timezone: "UTC".into(),
            channel: "in_app".into(),
            last_digest_date: None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NotificationRow {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: serde_json::Value, // parsed from the stored JSON string
    pub created_at: i64,
    pub read_at: Option<i64>,
}
```

- [ ] **Step 3: Write the failing test for digest-settings round-trip + dedup**

In the `#[cfg(test)] mod tests` block at the bottom of `src/db.rs`, add:

```rust
#[test]
fn digest_settings_roundtrip_and_dedup() {
    let db = Db::open_in_memory().unwrap();
    // Default when no row exists.
    let d = db.get_digest_settings("u1").unwrap();
    assert!(!d.enabled);
    assert_eq!(d.send_time, "08:00");
    assert_eq!(d.timezone, "UTC");
    assert_eq!(d.channel, "in_app");
    assert!(d.last_digest_date.is_none());

    // Upsert.
    db.upsert_digest_settings("u1", true, "07:30", "Asia/Shanghai", "both").unwrap();
    let d = db.get_digest_settings("u1").unwrap();
    assert!(d.enabled);
    assert_eq!(d.send_time, "07:30");
    assert_eq!(d.timezone, "Asia/Shanghai");
    assert_eq!(d.channel, "both");

    // last_digest_date is preserved across an upsert.
    db.set_last_digest_date("u1", "2026-05-29").unwrap();
    db.upsert_digest_settings("u1", true, "09:00", "Asia/Shanghai", "email").unwrap();
    assert_eq!(db.get_digest_settings("u1").unwrap().last_digest_date.as_deref(), Some("2026-05-29"));

    // Only enabled users are listed.
    db.upsert_digest_settings("u2", false, "08:00", "UTC", "in_app").unwrap();
    let enabled: Vec<String> = db.list_digest_enabled_user_ids().unwrap();
    assert_eq!(enabled, vec!["u1".to_string()]);
}
```

- [ ] **Step 4: Run the test, verify it fails**

Run: `cargo test -p dashboard digest_settings_roundtrip_and_dedup`
Expected: FAIL — `no method named get_digest_settings`.

- [ ] **Step 5: Implement the digest-settings methods**

In `src/db.rs` (e.g. after the net-worth methods, ~line 694), add:

```rust
// ───── digest settings ─────

pub fn get_digest_settings(&self, user_id: &str) -> SqlResult<DigestSettings> {
    let mut stmt = self.conn.prepare(
        "SELECT enabled, send_time, timezone, channel, last_digest_date
         FROM digest_settings WHERE user_id = ?1",
    )?;
    let row = stmt
        .query_row(params![user_id], |r| {
            Ok(DigestSettings {
                enabled: r.get::<_, i64>(0)? != 0,
                send_time: r.get(1)?,
                timezone: r.get(2)?,
                channel: r.get(3)?,
                last_digest_date: r.get(4)?,
            })
        })
        .optional()?;
    Ok(row.unwrap_or_default())
}

/// Upsert config without touching `last_digest_date` (preserved).
pub fn upsert_digest_settings(
    &self,
    user_id: &str,
    enabled: bool,
    send_time: &str,
    timezone: &str,
    channel: &str,
) -> SqlResult<()> {
    self.conn.execute(
        "INSERT INTO digest_settings(user_id, enabled, send_time, timezone, channel, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(user_id) DO UPDATE SET
            enabled = excluded.enabled,
            send_time = excluded.send_time,
            timezone = excluded.timezone,
            channel = excluded.channel,
            updated_at = excluded.updated_at",
        params![
            user_id,
            enabled as i64,
            send_time,
            timezone,
            channel,
            Utc::now().timestamp(),
        ],
    )?;
    Ok(())
}

pub fn set_last_digest_date(&self, user_id: &str, date: &str) -> SqlResult<()> {
    self.conn.execute(
        "UPDATE digest_settings SET last_digest_date = ?2 WHERE user_id = ?1",
        params![user_id, date],
    )?;
    Ok(())
}

pub fn list_digest_enabled_user_ids(&self) -> SqlResult<Vec<String>> {
    let mut stmt = self
        .conn
        .prepare("SELECT user_id FROM digest_settings WHERE enabled = 1")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}
```

- [ ] **Step 6: Run the test, verify it passes**

Run: `cargo test -p dashboard digest_settings_roundtrip_and_dedup`
Expected: PASS.

- [ ] **Step 7: Write the failing test for notifications**

Add to the same test module:

```rust
#[test]
fn notifications_insert_list_read() {
    let db = Db::open_in_memory().unwrap();
    let body = serde_json::json!({"date": "2026-05-29", "spending": {"total": 12.5}});
    db.insert_notification("u1", "digest", "今日简报", &body).unwrap();
    db.insert_notification("u1", "digest", "今日简报", &body).unwrap();

    let all = db.list_notifications("u1", false, 50).unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0].read_at.is_none());
    assert_eq!(all[0].title, "今日简报");
    assert_eq!(all[0].body["spending"]["total"], 12.5);

    let unread = db.list_notifications("u1", true, 50).unwrap();
    assert_eq!(unread.len(), 2);

    // Mark all read clears the unread list.
    let n = db.mark_notifications_read("u1", None).unwrap();
    assert_eq!(n, 2);
    assert_eq!(db.list_notifications("u1", true, 50).unwrap().len(), 0);
    // Idempotent: a second mark-all-read affects 0 rows.
    assert_eq!(db.mark_notifications_read("u1", None).unwrap(), 0);
}
```

- [ ] **Step 8: Run the test, verify it fails**

Run: `cargo test -p dashboard notifications_insert_list_read`
Expected: FAIL — `no method named insert_notification`.

- [ ] **Step 9: Implement the notification methods**

In `src/db.rs` (after the digest-settings methods), add:

```rust
// ───── notifications (in-app inbox) ─────

pub fn insert_notification(
    &self,
    user_id: &str,
    kind: &str,
    title: &str,
    body: &serde_json::Value,
) -> SqlResult<String> {
    let id = uuid::Uuid::new_v4().to_string();
    self.conn.execute(
        "INSERT INTO notifications(id, user_id, kind, title, body, created_at, read_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
        params![
            id,
            user_id,
            kind,
            title,
            body.to_string(),
            Utc::now().timestamp(),
        ],
    )?;
    Ok(id)
}

pub fn list_notifications(
    &self,
    user_id: &str,
    unread_only: bool,
    limit: i64,
) -> SqlResult<Vec<NotificationRow>> {
    let sql = if unread_only {
        "SELECT id, kind, title, body, created_at, read_at FROM notifications
         WHERE user_id = ?1 AND read_at IS NULL ORDER BY created_at DESC LIMIT ?2"
    } else {
        "SELECT id, kind, title, body, created_at, read_at FROM notifications
         WHERE user_id = ?1 ORDER BY created_at DESC LIMIT ?2"
    };
    let mut stmt = self.conn.prepare(sql)?;
    let rows = stmt.query_map(params![user_id, limit], |r| {
        let body_s: String = r.get(3)?;
        Ok(NotificationRow {
            id: r.get(0)?,
            kind: r.get(1)?,
            title: r.get(2)?,
            body: serde_json::from_str(&body_s).unwrap_or(serde_json::Value::Null),
            created_at: r.get(4)?,
            read_at: r.get(5)?,
        })
    })?;
    rows.collect()
}

pub fn count_unread_notifications(&self, user_id: &str) -> SqlResult<i64> {
    self.conn.query_row(
        "SELECT COUNT(*) FROM notifications WHERE user_id = ?1 AND read_at IS NULL",
        params![user_id],
        |r| r.get(0),
    )
}

/// Mark notifications read. `ids = None` marks all of the user's unread.
/// Returns the number of rows updated.
pub fn mark_notifications_read(&self, user_id: &str, ids: Option<&[String]>) -> SqlResult<usize> {
    let now = Utc::now().timestamp();
    match ids {
        None => self.conn.execute(
            "UPDATE notifications SET read_at = ?2 WHERE user_id = ?1 AND read_at IS NULL",
            params![user_id, now],
        ),
        Some(ids) => {
            let mut n = 0;
            for id in ids {
                n += self.conn.execute(
                    "UPDATE notifications SET read_at = ?3
                     WHERE user_id = ?1 AND id = ?2 AND read_at IS NULL",
                    params![user_id, id, now],
                )?;
            }
            Ok(n)
        }
    }
}
```

- [ ] **Step 10: Write the failing test for the market-brief cache**

Add to the same test module:

```rust
#[test]
fn market_brief_cache_roundtrip() {
    let db = Db::open_in_memory().unwrap();
    assert!(db.get_market_brief("2026-05-29").unwrap().is_none());
    let body = serde_json::json!({"summary": "黄金走平，比特币回落"});
    db.put_market_brief("2026-05-29", &body).unwrap();
    let got = db.get_market_brief("2026-05-29").unwrap().unwrap();
    assert_eq!(got["summary"], "黄金走平，比特币回落");
}
```

- [ ] **Step 11: Run it, verify it fails, then implement the market-brief methods**

Run: `cargo test -p dashboard market_brief_cache_roundtrip` → FAIL (`no method named get_market_brief`).

In `src/db.rs` (after the notification methods), add:

```rust
// ───── shared daily market brief ─────

pub fn get_market_brief(&self, day: &str) -> SqlResult<Option<serde_json::Value>> {
    let mut stmt = self
        .conn
        .prepare("SELECT body FROM daily_market_brief WHERE day = ?1")?;
    let row = stmt
        .query_row(params![day], |r| r.get::<_, String>(0))
        .optional()?;
    Ok(row.and_then(|s| serde_json::from_str(&s).ok()))
}

pub fn put_market_brief(&self, day: &str, body: &serde_json::Value) -> SqlResult<()> {
    self.conn.execute(
        "INSERT OR REPLACE INTO daily_market_brief(day, body, created_at)
         VALUES (?1, ?2, ?3)",
        params![day, body.to_string(), Utc::now().timestamp()],
    )?;
    Ok(())
}
```

- [ ] **Step 12: Run all four new tests, verify pass, then commit**

Run: `cargo test -p dashboard digest_settings_roundtrip_and_dedup notifications_insert_list_read market_brief_cache_roundtrip`
Expected: 3 tests PASS (notifications + settings + market brief).

```bash
git add examples/dashboard/src/db.rs
git commit -m "feat(dashboard): digest DB layer — digest_settings, notifications, daily_market_brief"
```

---

### Task 2: `digest` module scaffold + model + pure `is_due` scheduler

**Files:**
- Create: `examples/dashboard/src/digest/mod.rs`
- Create: `examples/dashboard/src/digest/model.rs`
- Create: `examples/dashboard/src/digest/schedule.rs`
- Modify: `examples/dashboard/src/main.rs:24` (add `mod digest;`)

- [ ] **Step 1: Declare the module**

In `src/main.rs`, add to the module list (keep alphabetical-ish, after `mod db;` at line 18):

```rust
mod digest;
```

- [ ] **Step 2: Create the module root**

Create `src/digest/mod.rs`:

```rust
//! Daily digest: opt-in proactive summary (yesterday's spending + wealth
//! movement + a shared gold/BTC/index market take), delivered in-app and/or
//! via Resend email at each user's chosen local time.
//!
//! - `model`    — the serializable `Digest` payload.
//! - `schedule` — the pure `is_due` predicate (timezone-aware, dedup-aware).
//! - `market`   — shared per-UTC-day market brief via Gemini grounding.
//! - `build`    — assembles a `Digest` for one user from ledger + snapshots.
//! - `deliver`  — in-app (notifications row) + email (Resend) adapters.
//! - `cron`     — the in-process tokio loop.

pub mod build;
pub mod cron;
pub mod deliver;
pub mod market;
pub mod model;
pub mod schedule;
```

- [ ] **Step 3: Create the model**

Create `src/digest/model.rs`:

```rust
//! The serializable digest payload. Stored as `notifications.body` JSON and
//! rendered to email HTML.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Digest {
    pub date: String, // user-local date the digest covers (the "yesterday")
    pub spending: SpendingSection,
    pub wealth: WealthSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market: Option<MarketBrief>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendingSection {
    pub total: f64,
    pub currency: String,
    /// (category, amount), highest first, capped to a handful.
    pub by_category: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WealthSection {
    pub net_worth: f64,
    pub net_delta: f64,
    pub cash: f64,
    pub investments: f64,
    pub investments_delta: f64,
    pub debt: f64,
    pub currency: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketBrief {
    pub gold: Quote,
    pub btc: Quote,
    pub index: Quote,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub name: String,
    pub price: String,
    pub conclusion: String,
}
```

- [ ] **Step 4: Write the failing test for `is_due`**

Create `src/digest/schedule.rs`:

```rust
//! Pure scheduling predicate for the digest cron. Kept free of I/O so it can
//! be exhaustively unit-tested.

use chrono::{DateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

/// Parse "HH:MM" into a `NaiveTime`. Invalid input falls back to 08:00.
pub fn parse_send_time(s: &str) -> NaiveTime {
    let mut parts = s.splitn(2, ':');
    let h: u32 = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(8);
    let m: u32 = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
    NaiveTime::from_hms_opt(h.min(23), m.min(59), 0).unwrap_or_else(|| NaiveTime::from_hms_opt(8, 0, 0).unwrap())
}

/// Parse an IANA timezone, falling back to UTC (logging the bad value).
pub fn parse_tz(s: &str) -> Tz {
    match s.parse::<Tz>() {
        Ok(tz) => tz,
        Err(_) => {
            tracing::warn!(tz = %s, "bad digest timezone, falling back to UTC");
            Tz::UTC
        }
    }
}

/// True when a digest should be sent right now for this user:
/// it's at/after their local send-time today AND they haven't been sent today.
pub fn is_due(
    now_utc: DateTime<Utc>,
    tz: Tz,
    send_time: &str,
    last_digest_date: Option<&str>,
) -> bool {
    let local = now_utc.with_timezone(&tz);
    let local_date = local.format("%Y-%m-%d").to_string();
    if last_digest_date == Some(local_date.as_str()) {
        return false; // already sent today (user-local)
    }
    local.time() >= parse_send_time(send_time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::Asia::Shanghai;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn fires_after_local_send_time() {
        // 2026-05-29 01:00 UTC = 09:00 Shanghai. send_time 08:00 → due.
        assert!(is_due(utc(2026, 5, 29, 1, 0), Shanghai, "08:00", None));
    }

    #[test]
    fn not_yet_before_local_send_time() {
        // 2026-05-28 23:00 UTC = 07:00 Shanghai. send_time 08:00 → not due.
        assert!(!is_due(utc(2026, 5, 28, 23, 0), Shanghai, "08:00", None));
    }

    #[test]
    fn skips_if_already_sent_today_local() {
        // 09:00 Shanghai on 2026-05-29, already sent that local date.
        assert!(!is_due(utc(2026, 5, 29, 1, 0), Shanghai, "08:00", Some("2026-05-29")));
    }

    #[test]
    fn fires_new_local_day_even_if_sent_yesterday() {
        assert!(is_due(utc(2026, 5, 29, 1, 0), Shanghai, "08:00", Some("2026-05-28")));
    }

    #[test]
    fn bad_tz_falls_back_to_utc() {
        // 09:00 UTC, send 08:00, bad tz → treated as UTC → due.
        assert!(is_due(utc(2026, 5, 29, 9, 0), parse_tz("Not/AZone"), "08:00", None));
    }

    #[test]
    fn parse_send_time_handles_garbage() {
        assert_eq!(parse_send_time("7:5"), NaiveTime::from_hms_opt(7, 5, 0).unwrap());
        assert_eq!(parse_send_time("nope"), NaiveTime::from_hms_opt(8, 0, 0).unwrap());
        assert_eq!(parse_send_time("25:99"), NaiveTime::from_hms_opt(23, 59, 0).unwrap());
    }
}
```

Also create stub files so `mod.rs` compiles (they're filled in later tasks). Create `src/digest/market.rs`, `src/digest/build.rs`, `src/digest/deliver.rs`, `src/digest/cron.rs`, each containing only a doc comment line for now:

```rust
//! (filled in a later task)
```

- [ ] **Step 5: Run the scheduler tests, verify they fail then pass**

Run: `cargo test -p dashboard --lib schedule`
Expected: the 6 tests compile and PASS. If `chrono::TimeZone` unused-import warnings appear, remove the redundant `use chrono::TimeZone;` inside the test module (the `Utc.with_ymd_and_hms` needs the trait — keep whichever import makes it compile).

- [ ] **Step 6: Commit**

```bash
git add examples/dashboard/src/digest examples/dashboard/src/main.rs
git commit -m "feat(dashboard): digest module scaffold + Digest model + pure is_due scheduler"
```

---

### Task 3: Market brief — Gemini grounding (Chinese), parsed + cached once per UTC day

**Files:**
- Modify: `examples/dashboard/src/digest/market.rs`

- [ ] **Step 1: Write the failing test for the response parser**

Replace `src/digest/market.rs` contents with the parser + its test first:

```rust
//! Shared per-UTC-day market brief: gold / bitcoin / a stock index, each with
//! a current level + one-line Chinese trend conclusion, plus a short overall
//! summary. Generated once per UTC day via Gemini grounding and cached in the
//! `daily_market_brief` table so every user's digest that day reuses it.

use crate::db::Db;
use crate::digest::model::{MarketBrief, Quote};
use chrono::Utc;

/// The Chinese grounding prompt. We ask for a strict pipe-delimited format so
/// the response is trivially parseable (Gemini can't combine grounding with
/// JSON-schema output in one call).
pub const MARKET_PROMPT: &str = "\
用 Google 搜索查到当前最新行情，然后严格按下面 4 行格式输出，不要任何多余文字、不要 Markdown：\n\
黄金|<伦敦金现货美元/盎司价格数字>|<一句话中文结论>\n\
比特币|<美元价格数字>|<一句话中文结论>\n\
纳斯达克|<纳斯达克综合指数点位数字>|<一句话中文结论>\n\
总结|<1到2句中文综合点评>\n\
示例：黄金|2360.5|金价小幅走高，避险情绪升温。";

/// Parse the 4-line pipe format into a `MarketBrief`. Returns `None` if any of
/// the four expected lines is missing.
pub fn parse_market_response(text: &str) -> Option<MarketBrief> {
    let mut gold = None;
    let mut btc = None;
    let mut index = None;
    let mut summary = None;
    for line in text.lines() {
        let line = line.trim();
        let mut cols = line.splitn(3, '|');
        let tag = cols.next().unwrap_or("").trim();
        match tag {
            "黄金" => {
                let price = cols.next().unwrap_or("").trim().to_string();
                let concl = cols.next().unwrap_or("").trim().to_string();
                if !price.is_empty() {
                    gold = Some(Quote { name: "黄金".into(), price, conclusion: concl });
                }
            }
            "比特币" => {
                let price = cols.next().unwrap_or("").trim().to_string();
                let concl = cols.next().unwrap_or("").trim().to_string();
                if !price.is_empty() {
                    btc = Some(Quote { name: "比特币".into(), price, conclusion: concl });
                }
            }
            "纳斯达克" => {
                let price = cols.next().unwrap_or("").trim().to_string();
                let concl = cols.next().unwrap_or("").trim().to_string();
                if !price.is_empty() {
                    index = Some(Quote { name: "纳斯达克".into(), price, conclusion: concl });
                }
            }
            "总结" => {
                let s = cols.next().unwrap_or("").trim().to_string();
                if !s.is_empty() {
                    summary = Some(s);
                }
            }
            _ => {}
        }
    }
    Some(MarketBrief {
        gold: gold?,
        btc: btc?,
        index: index?,
        summary: summary?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_response() {
        let text = "黄金|2360.5|金价小幅走高，避险升温。\n比特币|67000|震荡回落。\n纳斯达克|17500|科技股领涨。\n总结|风险偏好整体回暖。";
        let b = parse_market_response(text).unwrap();
        assert_eq!(b.gold.price, "2360.5");
        assert_eq!(b.gold.conclusion, "金价小幅走高，避险升温。");
        assert_eq!(b.btc.price, "67000");
        assert_eq!(b.index.name, "纳斯达克");
        assert_eq!(b.summary, "风险偏好整体回暖。");
    }

    #[test]
    fn missing_line_returns_none() {
        let text = "黄金|2360.5|金价走高\n比特币|67000|回落"; // no index, no summary
        assert!(parse_market_response(text).is_none());
    }

    #[test]
    fn tolerates_blank_and_extra_lines() {
        let text = "\n这是模型的废话\n黄金|2360|稳\n比特币|67000|稳\n纳斯达克|17500|稳\n总结|稳。\n再见";
        assert!(parse_market_response(text).is_some());
    }
}
```

- [ ] **Step 2: Run the parser tests**

Run: `cargo test -p dashboard --lib market`
Expected: 3 tests PASS.

- [ ] **Step 3: Add `ensure_market_brief` (network) below the parser**

Append to `src/digest/market.rs`:

```rust
/// Return today's (UTC) market brief, generating + caching it on first call.
/// On generation/parse failure, logs a WARN and returns `None` (the digest
/// still sends without the market section).
pub async fn ensure_market_brief(db: &Db, client: &reqwest::Client) -> Option<MarketBrief> {
    let day = Utc::now().format("%Y-%m-%d").to_string();
    if let Ok(Some(v)) = db.get_market_brief(&day) {
        if let Ok(b) = serde_json::from_value::<MarketBrief>(v) {
            return Some(b);
        }
    }
    match generate_market_brief(client).await {
        Some(brief) => {
            if let Ok(v) = serde_json::to_value(&brief) {
                let _ = db.put_market_brief(&day, &v);
            }
            Some(brief)
        }
        None => {
            tracing::warn!(day = %day, "market brief generation failed; digest will omit market section");
            None
        }
    }
}

/// One grounded Gemini call → parsed `MarketBrief`. Mirrors the request shape
/// in `portfolio/quotes.rs::gemini_grounded_price` (google_search tool,
/// thinkingBudget 0). Returns `None` on any network/parse error.
async fn generate_market_brief(client: &reqwest::Client) -> Option<MarketBrief> {
    let api_key = std::env::var("GEMINI_API_KEY").ok().filter(|k| !k.is_empty())?;
    let model = std::env::var("HARNESS_QUOTE_MODEL").unwrap_or_else(|_| "gemini-3.5-flash".to_string());
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        urlencoding(&model),
        urlencoding(&api_key),
    );
    let body = serde_json::json!({
        "contents": [{ "parts": [{ "text": MARKET_PROMPT }] }],
        "tools": [{"google_search": {}}],
        "generationConfig": {
            "temperature": 0.0,
            "thinkingConfig": {"thinkingBudget": 0},
            "maxOutputTokens": 1024
        }
    });
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(40))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&resp).ok()?;
    let parts = v.pointer("/candidates/0/content/parts")?.as_array()?;
    let mut combined = String::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
            combined.push_str(t);
            combined.push('\n');
        }
    }
    parse_market_response(&combined)
}

/// Minimal percent-encoder for URL path/query segments (avoids pulling a new
/// dep; the model id + key are already URL-safe in practice but we guard `/`).
fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}
```

- [ ] **Step 4: Build to confirm it compiles**

Run: `cargo build -p dashboard`
Expected: compiles (warnings OK). `ensure_market_brief`/`generate_market_brief` may be flagged unused until Task 6 wires the cron — that's fine (they're `pub` so the unused warning is limited).

- [ ] **Step 5: Commit**

```bash
git add examples/dashboard/src/digest/market.rs
git commit -m "feat(dashboard): digest market brief — grounded Gemini call, pipe-format parser, daily cache"
```

---

### Task 4: `build_digest` — yesterday's spending + wealth from snapshots

**Files:**
- Modify: `examples/dashboard/src/digest/build.rs`

- [ ] **Step 1: Write the failing test for spending aggregation**

Replace `src/digest/build.rs` with the spending helper + its test:

```rust
//! Assemble a `Digest` for one user from existing ledger + net-worth data.
//! All generation is pure-ish (DB reads only); delivery lives in `deliver`.

use crate::db::Db;
use crate::digest::model::{Digest, MarketBrief, SpendingSection, WealthSection};
use crate::model::{Transaction, TxnKind};
use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use chrono_tz::Tz;
use rust_decimal::prelude::ToPrimitive;
use std::collections::HashMap;

/// The user-local [start, end) of "yesterday", expressed as UTC instants.
pub fn yesterday_bounds(now_utc: DateTime<Utc>, tz: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
    let local_today = now_utc.with_timezone(&tz).date_naive();
    let local_yest = local_today - Duration::days(1);
    let start_local = tz
        .from_local_datetime(&local_yest.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap();
    let end_local = tz
        .from_local_datetime(&local_today.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap();
    (start_local.with_timezone(&Utc), end_local.with_timezone(&Utc))
}

/// Aggregate expense transactions into a SpendingSection. Income/transfer are
/// ignored; categories without a label fold into "未分类". Sorted desc, top 5.
pub fn spending_from_txns(txns: &[Transaction], currency: &str) -> SpendingSection {
    let mut by_cat: HashMap<String, f64> = HashMap::new();
    let mut total = 0.0;
    for t in txns {
        if !matches!(t.kind, TxnKind::Expense) {
            continue;
        }
        let amt = t.amount.to_f64().unwrap_or(0.0);
        total += amt;
        let cat = t.category.clone().unwrap_or_else(|| "未分类".into());
        *by_cat.entry(cat).or_insert(0.0) += amt;
    }
    let mut by_category: Vec<(String, f64)> = by_cat.into_iter().collect();
    by_category.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    by_category.truncate(5);
    SpendingSection { total, currency: currency.to_string(), by_category }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Asia::Shanghai;
    use rust_decimal::Decimal;

    fn txn(kind: TxnKind, amt: i64, cat: Option<&str>) -> Transaction {
        Transaction {
            id: "t".into(),
            kind,
            amount: Decimal::new(amt, 0),
            currency: "CNY".into(),
            account_id: "a".into(),
            counter_account_id: None,
            category: cat.map(|c| c.to_string()),
            note: None,
            occurred_at: Utc::now(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn aggregates_expenses_only_top5_desc() {
        let txns = vec![
            txn(TxnKind::Expense, 100, Some("餐饮")),
            txn(TxnKind::Expense, 50, Some("餐饮")),
            txn(TxnKind::Expense, 200, Some("购物")),
            txn(TxnKind::Income, 9999, Some("工资")),
            txn(TxnKind::Expense, 30, None),
        ];
        let s = spending_from_txns(&txns, "CNY");
        assert_eq!(s.total, 380.0); // income excluded
        assert_eq!(s.by_category[0], ("购物".into(), 200.0));
        assert_eq!(s.by_category[1], ("餐饮".into(), 150.0));
        assert!(s.by_category.iter().any(|(c, _)| c == "未分类"));
    }

    #[test]
    fn yesterday_bounds_span_one_local_day() {
        // 2026-05-29 02:00 UTC = 10:00 Shanghai. Yesterday = 2026-05-28 local.
        let now = Utc.with_ymd_and_hms(2026, 5, 29, 2, 0, 0).unwrap();
        let (start, end) = yesterday_bounds(now, Shanghai);
        // 2026-05-28 00:00 +08:00 = 2026-05-27 16:00 UTC; end is +24h.
        assert_eq!(end - start, Duration::days(1));
        assert_eq!(start.with_timezone(&Shanghai).day(), 28);
        assert_eq!(start.with_timezone(&Shanghai).hour(), 0);
    }
}
```

(Add `use chrono::Timelike;` to the test if `.hour()` needs it — include it in the `use super::*` block of the test or alongside `Datelike`.)

- [ ] **Step 2: Run, verify the spending tests pass**

Run: `cargo test -p dashboard --lib build`
Expected: 2 tests PASS. If `.hour()`/`.day()` don't resolve, add `use chrono::Timelike;` and `Datelike` is already imported at top.

- [ ] **Step 3: Add `wealth_from_snapshots` + `build_digest` below the helpers**

Append to `src/digest/build.rs`:

```rust
/// WealthSection from the two latest net-worth snapshots. With <2 snapshots,
/// deltas are 0. With none, everything is 0 (new user).
pub fn wealth_from_snapshots(db: &Db, user_id: &str, base_currency: &str) -> WealthSection {
    // Pull a short recent series and take the last two rows.
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let from = (Utc::now() - Duration::days(14)).format("%Y-%m-%d").to_string();
    let series = db.net_worth_series(user_id, &from, &today).unwrap_or_default();
    let latest = series.last();
    let prev = if series.len() >= 2 { series.get(series.len() - 2) } else { None };
    match latest {
        Some(l) => WealthSection {
            net_worth: l.net_amt,
            net_delta: prev.map(|p| l.net_amt - p.net_amt).unwrap_or(0.0),
            cash: l.cash_amt,
            investments: l.investments_amt,
            investments_delta: prev.map(|p| l.investments_amt - p.investments_amt).unwrap_or(0.0),
            debt: l.debt_amt,
            currency: l.base_currency.clone(),
        },
        None => WealthSection {
            net_worth: 0.0,
            net_delta: 0.0,
            cash: 0.0,
            investments: 0.0,
            investments_delta: 0.0,
            debt: 0.0,
            currency: base_currency.to_string(),
        },
    }
}

/// Assemble the full digest for one user. `market` is the shared brief (may be
/// None). `now_utc` and `tz` define which local "yesterday" to summarize.
pub fn build_digest(
    db: &Db,
    user_id: &str,
    base_currency: &str,
    now_utc: DateTime<Utc>,
    tz: Tz,
    market: Option<MarketBrief>,
) -> anyhow::Result<Digest> {
    let (start, end) = yesterday_bounds(now_utc, tz);
    let txns = db.list_transactions(user_id, start, end, None, None)?;
    let spending = spending_from_txns(&txns, base_currency);
    let wealth = wealth_from_snapshots(db, user_id, base_currency);
    let date = (now_utc.with_timezone(&tz).date_naive() - Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    Ok(Digest { date, spending, wealth, market })
}
```

- [ ] **Step 4: Write an integration-ish test for `build_digest`**

Add to the test module in `build.rs`:

```rust
#[test]
fn build_digest_end_to_end_from_db() {
    use crate::model::Account;
    let db = Db::open_in_memory().unwrap();
    // Seed an account + a yesterday expense (Shanghai local).
    let now = Utc.with_ymd_and_hms(2026, 5, 29, 2, 0, 0).unwrap();
    let (start, _end) = yesterday_bounds(now, Shanghai);
    let acct = Account {
        id: "a1".into(),
        name: "微信".into(),
        kind: crate::model::AccountKind::Wallet,
        currency: "CNY".into(),
        opening_balance: rust_decimal::Decimal::ZERO,
        created_at: Utc::now(),
    };
    db.insert_account("u1", &acct).unwrap();
    // A transaction occurring at start+2h (inside yesterday).
    db.insert_transaction(
        "u1",
        &crate::model::Transaction {
            id: "tx1".into(),
            kind: TxnKind::Expense,
            amount: rust_decimal::Decimal::new(4200, 2), // 42.00
            currency: "CNY".into(),
            account_id: "a1".into(),
            counter_account_id: None,
            category: Some("餐饮".into()),
            note: None,
            occurred_at: start + Duration::hours(2),
            created_at: Utc::now(),
        },
    )
    .unwrap();

    let d = build_digest(&db, "u1", "CNY", now, Shanghai, None).unwrap();
    assert_eq!(d.date, "2026-05-28");
    assert_eq!(d.spending.total, 42.0);
    assert_eq!(d.spending.by_category[0], ("餐饮".into(), 42.0));
    assert_eq!(d.wealth.net_worth, 0.0); // no snapshots seeded
    assert!(d.market.is_none());
}
```

> Note: confirm the exact constructor names/signatures of `Account`,
> `Transaction`, `db.insert_account`, and `db.insert_transaction` against
> `model.rs` / `db.rs` before running; adjust field names if they differ
> (e.g. `insert_transaction` may take positional args rather than a struct).
> Use whichever the codebase exposes — the assertions are what matter.

- [ ] **Step 5: Run the build_digest test, verify pass, commit**

Run: `cargo test -p dashboard --lib build`
Expected: 3 tests PASS.

```bash
git add examples/dashboard/src/digest/build.rs
git commit -m "feat(dashboard): build_digest — yesterday spending + wealth from snapshots"
```

---

### Task 5: Delivery — in-app row + Resend email

**Files:**
- Modify: `examples/dashboard/src/digest/deliver.rs`

- [ ] **Step 1: Write the failing test for the HTML renderer + Resend payload**

Replace `src/digest/deliver.rs` with the pure renderers + tests first:

```rust
//! Two delivery adapters for a `Digest`:
//!   - `deliver_in_app` inserts a `notifications` row.
//!   - `deliver_email` renders HTML and POSTs to Resend.
//! Channel selection ("in_app" | "email" | "both") is the caller's job.

use crate::db::Db;
use crate::digest::model::Digest;

/// Localized (Chinese) email subject for a digest covering `date`.
pub fn email_subject(d: &Digest) -> String {
    format!("今日简报 · {}", d.date)
}

/// Render the digest to a simple inline-styled HTML email body.
pub fn render_email_html(d: &Digest) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "<div style=\"font-family:-apple-system,Segoe UI,Roboto,sans-serif;max-width:560px;margin:0 auto;color:#1a1a1a\">\
         <h2 style=\"margin:0 0 4px\">今日简报</h2>\
         <div style=\"color:#888;font-size:13px;margin-bottom:16px\">{}</div>",
        d.date
    ));
    // Spending
    s.push_str(&format!(
        "<h3 style=\"margin:16px 0 6px\">昨日支出</h3>\
         <div style=\"font-size:20px;font-weight:600\">{:.2} {}</div>",
        d.spending.total, d.spending.currency
    ));
    if !d.spending.by_category.is_empty() {
        s.push_str("<ul style=\"margin:6px 0;padding-left:18px;color:#444\">");
        for (cat, amt) in &d.spending.by_category {
            s.push_str(&format!("<li>{cat}: {amt:.2}</li>"));
        }
        s.push_str("</ul>");
    }
    // Wealth
    let w = &d.wealth;
    s.push_str(&format!(
        "<h3 style=\"margin:16px 0 6px\">资产</h3>\
         <div>净值 {:.2} {} （较前一日 {:+.2}）</div>\
         <div style=\"color:#444;font-size:14px\">现金 {:.2} · 投资 {:.2}（{:+.2}）· 负债 {:.2}</div>",
        w.net_worth, w.currency, w.net_delta, w.cash, w.investments, w.investments_delta, w.debt
    ));
    // Market
    if let Some(m) = &d.market {
        s.push_str("<h3 style=\"margin:16px 0 6px\">市场</h3><ul style=\"margin:6px 0;padding-left:18px;color:#444\">");
        for q in [&m.gold, &m.btc, &m.index] {
            s.push_str(&format!("<li><b>{}</b> {} — {}</li>", q.name, q.price, q.conclusion));
        }
        s.push_str("</ul>");
        s.push_str(&format!("<p style=\"color:#444\">{}</p>", m.summary));
    }
    s.push_str("<hr style=\"border:none;border-top:1px solid #eee;margin:20px 0\">\
                <div style=\"color:#aaa;font-size:12px\">来自 Dashboard · 可在「我的 → 每日简报」中关闭</div></div>");
    s
}

/// Build the JSON body for Resend's POST /emails. Pure — no network.
pub fn resend_body(from: &str, to: &str, subject: &str, html: &str) -> serde_json::Value {
    serde_json::json!({
        "from": from,
        "to": [to],
        "subject": subject,
        "html": html,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::model::*;

    fn sample() -> Digest {
        Digest {
            date: "2026-05-28".into(),
            spending: SpendingSection { total: 42.0, currency: "CNY".into(), by_category: vec![("餐饮".into(), 42.0)] },
            wealth: WealthSection { net_worth: 1000.0, net_delta: 10.0, cash: 500.0, investments: 600.0, investments_delta: 5.0, debt: 100.0, currency: "CNY".into() },
            market: Some(MarketBrief {
                gold: Quote { name: "黄金".into(), price: "2360".into(), conclusion: "走高".into() },
                btc: Quote { name: "比特币".into(), price: "67000".into(), conclusion: "回落".into() },
                index: Quote { name: "纳斯达克".into(), price: "17500".into(), conclusion: "领涨".into() },
                summary: "整体回暖".into(),
            }),
        }
    }

    #[test]
    fn subject_includes_date() {
        assert_eq!(email_subject(&sample()), "今日简报 · 2026-05-28");
    }

    #[test]
    fn html_contains_all_sections() {
        let h = render_email_html(&sample());
        assert!(h.contains("昨日支出"));
        assert!(h.contains("42.00 CNY"));
        assert!(h.contains("餐饮"));
        assert!(h.contains("净值 1000.00 CNY"));
        assert!(h.contains("黄金"));
        assert!(h.contains("整体回暖"));
    }

    #[test]
    fn html_omits_market_when_absent() {
        let mut d = sample();
        d.market = None;
        let h = render_email_html(&d);
        assert!(!h.contains("市场"));
    }

    #[test]
    fn resend_body_shape() {
        let b = resend_body("Dashboard <d@x.com>", "u@y.com", "subj", "<p>hi</p>");
        assert_eq!(b["from"], "Dashboard <d@x.com>");
        assert_eq!(b["to"][0], "u@y.com");
        assert_eq!(b["subject"], "subj");
        assert_eq!(b["html"], "<p>hi</p>");
    }
}
```

- [ ] **Step 2: Run the renderer tests**

Run: `cargo test -p dashboard --lib deliver`
Expected: 4 tests PASS.

- [ ] **Step 3: Add the two delivery functions below the renderers**

Append to `src/digest/deliver.rs`:

```rust
/// Insert the digest as an in-app notification row.
pub fn deliver_in_app(db: &Db, user_id: &str, digest: &Digest) -> anyhow::Result<()> {
    let body = serde_json::to_value(digest)?;
    db.insert_notification(user_id, "digest", "今日简报", &body)?;
    Ok(())
}

/// Send the digest by email via Resend. Reads `RESEND_API_KEY` + `DIGEST_FROM`
/// from env (deploy-time secrets). If the key is unset, logs a WARN and returns
/// Ok (email is simply skipped — in-app delivery is unaffected). A non-2xx
/// Resend response is logged WARN but not treated as fatal (no retry storm).
pub async fn deliver_email(
    client: &reqwest::Client,
    to_email: &str,
    digest: &Digest,
) -> anyhow::Result<()> {
    let api_key = match std::env::var("RESEND_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            tracing::warn!("RESEND_API_KEY unset; skipping digest email");
            return Ok(());
        }
    };
    let from = std::env::var("DIGEST_FROM")
        .unwrap_or_else(|_| "Dashboard <onboarding@resend.dev>".to_string());
    let subject = email_subject(digest);
    let html = render_email_html(digest);
    let body = resend_body(&from, to_email, &subject, &html);
    let resp = client
        .post("https://api.resend.com/emails")
        .bearer_auth(&api_key)
        .json(&body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => {
            let code = r.status();
            let txt = r.text().await.unwrap_or_default();
            tracing::warn!(status = %code, body = %txt, "Resend digest email non-2xx");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(err = %e, "Resend digest email failed");
            Ok(())
        }
    }
}
```

- [ ] **Step 4: Build, verify it compiles, commit**

Run: `cargo build -p dashboard`
Expected: compiles (unused-fn warnings for delivery fns until Task 6 wires them are OK).

```bash
git add examples/dashboard/src/digest/deliver.rs
git commit -m "feat(dashboard): digest delivery — in-app row + Resend email (HTML render + payload)"
```

---

### Task 6: Cron — `spawn_digest_cron` + wire into `main.rs`

**Files:**
- Modify: `examples/dashboard/src/digest/cron.rs`
- Modify: `examples/dashboard/src/main.rs:884` (spawn alongside the other crons)

- [ ] **Step 1: Implement the cron loop**

Replace `src/digest/cron.rs` with:

```rust
//! In-process digest cron. Mirrors `net_worth::spawn_snapshot_cron`: spawn one
//! tokio task at server startup that ticks every 15 minutes. Each tick opens
//! its own DB connection (rusqlite `Connection` is `!Send` across awaits).

use crate::db::Db;
use crate::digest::{build, deliver, market, schedule};
use chrono::Utc;
use std::path::PathBuf;
use std::time::Duration;

const TICK: Duration = Duration::from_secs(15 * 60);

pub fn spawn_digest_cron(db_path: PathBuf) {
    tokio::spawn(async move {
        // Small initial delay so startup isn't competing with first requests.
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            if let Err(e) = run_tick(&db_path).await {
                tracing::warn!(err = %e, "digest tick failed");
            }
            tokio::time::sleep(TICK).await;
        }
    });
}

async fn run_tick(db_path: &PathBuf) -> anyhow::Result<()> {
    let db = Db::open(db_path)?;
    let user_ids = db.list_digest_enabled_user_ids()?;
    if user_ids.is_empty() {
        return Ok(());
    }
    let now = Utc::now();
    let client = crate::portfolio::quotes::make_client();
    let mut market_brief: Option<market::MarketBriefCacheState> = None;

    for uid in &user_ids {
        let settings = db.get_digest_settings(uid)?;
        if !settings.enabled {
            continue;
        }
        let tz = schedule::parse_tz(&settings.timezone);
        if !schedule::is_due(now, tz, &settings.send_time, settings.last_digest_date.as_deref()) {
            continue;
        }
        let Some(user) = db.get_user_by_id(uid)? else { continue };

        // Generate the shared market brief lazily on the first due user.
        if market_brief.is_none() {
            market_brief = Some(market::MarketBriefCacheState {
                brief: market::ensure_market_brief(&db, &client).await,
            });
        }
        let brief = market_brief.as_ref().and_then(|m| m.brief.clone());

        let digest = match build::build_digest(&db, &user.id, &user.base_currency, now, tz, brief) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(user = %uid, err = %e, "build_digest failed");
                continue;
            }
        };

        let channel = settings.channel.as_str();
        if channel == "in_app" || channel == "both" {
            if let Err(e) = deliver::deliver_in_app(&db, &user.id, &digest) {
                tracing::warn!(user = %uid, err = %e, "deliver_in_app failed");
            }
        }
        if channel == "email" || channel == "both" {
            let _ = deliver::deliver_email(&client, &user.email, &digest).await;
        }

        // Mark sent (user-local date) regardless of email outcome — no retry storm.
        let local_date = now.with_timezone(&tz).format("%Y-%m-%d").to_string();
        if let Err(e) = db.set_last_digest_date(&user.id, &local_date) {
            tracing::warn!(user = %uid, err = %e, "set_last_digest_date failed");
        }
        tracing::info!(user = %uid, channel = %channel, date = %digest.date, "digest sent");
    }
    Ok(())
}
```

- [ ] **Step 2: Add the tiny `MarketBriefCacheState` holder to `market.rs`**

In `src/digest/market.rs`, add near the top (after the imports):

```rust
/// Per-tick holder so we generate the shared brief at most once per cron tick.
pub struct MarketBriefCacheState {
    pub brief: Option<MarketBrief>,
}
```

- [ ] **Step 3: Confirm `make_client` is public**

Check `portfolio/quotes.rs:38` — `pub fn make_client()`. It is `pub`. If `quotes` isn't re-exported from the `portfolio` module, add `pub mod quotes;` (it already is, since other code calls it). Verify with:

Run: `grep -n "pub mod quotes\|pub use.*quotes\|mod quotes" examples/dashboard/src/portfolio/mod.rs`
Expected: a `pub mod quotes;` (or equivalent). If it's private, change it to `pub mod quotes;`.

- [ ] **Step 4: Wire the cron into `main.rs`**

In `src/main.rs`, right after `net_worth::spawn_snapshot_cron(db_path.clone());` (line 884), add:

```rust
        digest::cron::spawn_digest_cron(db_path.clone());
```

(Place it before `loans::spawn_accrual_cron(db_path);` since that call moves `db_path`.)

- [ ] **Step 5: Build, verify it compiles**

Run: `cargo build -p dashboard`
Expected: compiles. The previously-"unused" digest functions are now all referenced.

- [ ] **Step 6: Commit**

```bash
git add examples/dashboard/src/digest/cron.rs examples/dashboard/src/digest/market.rs examples/dashboard/src/main.rs
git commit -m "feat(dashboard): spawn_digest_cron (15-min tick) + wire into serve startup"
```

---

### Task 7: API — digest settings + notifications endpoints

**Files:**
- Modify: `examples/dashboard/src/server.rs` (add handlers near the other `/api/me/*` handlers ~line 2343; register routes ~line 320)

- [ ] **Step 1: Add the settings + notifications handlers**

In `src/server.rs`, near `set_base_currency_handler` (~line 2358), add:

```rust
// ─── daily digest settings ───────────────────────────────────────────────

async fn get_digest_settings_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let s = db.get_digest_settings(&auth.user.id).map_err(api_err)?;
    Ok(Json(json!({ "settings": s })))
}

#[derive(serde::Deserialize)]
struct DigestSettingsReq {
    enabled: bool,
    time: String,     // "HH:MM"
    timezone: String, // IANA
    channel: String,  // in_app | email | both
}

async fn patch_digest_settings_handler(
    auth: AuthCtx,
    Json(req): Json<DigestSettingsReq>,
) -> Result<Json<Value>, ApiError> {
    // Validate time "HH:MM".
    let valid_time = {
        let mut p = req.time.splitn(2, ':');
        match (p.next().and_then(|h| h.parse::<u32>().ok()), p.next().and_then(|m| m.parse::<u32>().ok())) {
            (Some(h), Some(m)) => h < 24 && m < 60,
            _ => false,
        }
    };
    if !valid_time {
        return Err(ApiError::BadRequest("time must be HH:MM (24h)".into()));
    }
    // Validate timezone is a known IANA name.
    if req.timezone.parse::<chrono_tz::Tz>().is_err() {
        return Err(ApiError::BadRequest(format!("unknown timezone `{}`", req.timezone)));
    }
    // Validate channel.
    if !matches!(req.channel.as_str(), "in_app" | "email" | "both") {
        return Err(ApiError::BadRequest("channel must be in_app | email | both".into()));
    }
    let db = open_db()?;
    db.upsert_digest_settings(&auth.user.id, req.enabled, &req.time, &req.timezone, &req.channel)
        .map_err(api_err)?;
    let s = db.get_digest_settings(&auth.user.id).map_err(api_err)?;
    Ok(Json(json!({ "ok": true, "settings": s })))
}

// ─── in-app notifications ────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct NotificationsQuery {
    #[serde(default)]
    unread: bool,
}

async fn list_notifications_handler(
    auth: AuthCtx,
    axum::extract::Query(q): axum::extract::Query<NotificationsQuery>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let items = db.list_notifications(&auth.user.id, q.unread, 50).map_err(api_err)?;
    let unread = db.count_unread_notifications(&auth.user.id).map_err(api_err)?;
    Ok(Json(json!({ "notifications": items, "unread": unread })))
}

#[derive(serde::Deserialize)]
struct MarkReadReq {
    #[serde(default)]
    ids: Option<Vec<String>>,
}

async fn mark_notifications_read_handler(
    auth: AuthCtx,
    Json(req): Json<MarkReadReq>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let n = db
        .mark_notifications_read(&auth.user.id, req.ids.as_deref())
        .map_err(api_err)?;
    Ok(Json(json!({ "ok": true, "updated": n })))
}
```

- [ ] **Step 2: Register the routes**

In `src/server.rs`, in the `Router::new()` chain (after `/api/me/base-currency` at line 320), add:

```rust
        .route("/api/me/digest-settings", get(get_digest_settings_handler).patch(patch_digest_settings_handler))
        .route("/api/me/notifications", get(list_notifications_handler))
        .route("/api/me/notifications/read", post(mark_notifications_read_handler))
```

Ensure `patch` is imported: at the top of `server.rs` the routing imports likely read `use axum::routing::{get, post};` — change to `use axum::routing::{get, patch, post};`. Verify:

Run: `grep -n "use axum::routing" examples/dashboard/src/server.rs`
If `patch` is missing, add it.

- [ ] **Step 3: Build, verify it compiles**

Run: `cargo build -p dashboard`
Expected: compiles.

- [ ] **Step 4: Manual smoke (optional but recommended)**

Run the server on a random high port and exercise the endpoints with a session cookie. (Reuse the existing login flow; this step is a sanity check, not a unit test.)

```bash
HARNESS_LEDGER_DB=/tmp/digest-smoke.db cargo run -p dashboard -- --serve --port 6743 &
# register/login to get a token, then:
# curl -s -X PATCH localhost:6743/api/me/digest-settings -H 'Cookie: ...' \
#   -d '{"enabled":true,"time":"08:00","timezone":"Asia/Shanghai","channel":"both"}'
# curl -s localhost:6743/api/me/digest-settings -H 'Cookie: ...'
```

Expected: PATCH returns `{"ok":true,"settings":{...}}`; bad timezone returns 400.

- [ ] **Step 5: Commit**

```bash
git add examples/dashboard/src/server.rs
git commit -m "feat(dashboard): digest API — GET/PATCH /api/me/digest-settings, notifications list + mark-read"
```

---

### Task 8: Frontend — API client + Profile "Daily digest" card + i18n

**Files:**
- Modify: `examples/dashboard/user-ui/src/lib/api.ts` (types + 4 methods)
- Create: `examples/dashboard/user-ui/src/components/profile/digest-card.tsx`
- Modify: `examples/dashboard/user-ui/src/pages/Profile.tsx` (mount the card)
- Modify: `examples/dashboard/user-ui/src/locales/en.json` + `zh.json` (add `digest.*`)

- [ ] **Step 1: Add API types + methods**

In `user-ui/src/lib/api.ts`, add types and methods (follow the existing `ledgerApi` method style — `fetchJson`/`request` helper the file already uses):

```ts
export interface DigestSettings {
  enabled: boolean;
  send_time: string; // "HH:MM"
  timezone: string;  // IANA
  channel: 'in_app' | 'email' | 'both';
  last_digest_date?: string | null;
}

export interface NotificationItem {
  id: string;
  kind: string;
  title: string;
  body: any; // Digest JSON
  created_at: number;
  read_at: number | null;
}
```

Add to the `ledgerApi` object (mirroring how `me()` / `setModel()` are written):

```ts
  digestSettings(): Promise<{ settings: DigestSettings }> {
    return this.get('/api/me/digest-settings');
  },
  saveDigestSettings(s: { enabled: boolean; time: string; timezone: string; channel: string }): Promise<{ ok: boolean; settings: DigestSettings }> {
    return this.patch('/api/me/digest-settings', s);
  },
  notifications(unread = false): Promise<{ notifications: NotificationItem[]; unread: number }> {
    return this.get(`/api/me/notifications${unread ? '?unread=true' : ''}`);
  },
  markNotificationsRead(ids?: string[]): Promise<{ ok: boolean; updated: number }> {
    return this.post('/api/me/notifications/read', ids ? { ids } : {});
  },
```

> If `api.ts` has no `patch` helper, add one next to `post` using
> `method: 'PATCH'` (copy the `post` implementation, swap the verb).

- [ ] **Step 2: Add i18n keys**

In `user-ui/src/locales/en.json`, add a `digest` block (top-level, after `chat`):

```json
  "digest": {
    "title": "Daily digest",
    "subtitle": "A morning summary of yesterday's spending, your wealth, and the market.",
    "enable": "Enable daily digest",
    "time": "Send time",
    "timezone": "Timezone",
    "channel": "Delivery",
    "channelInApp": "In-app",
    "channelEmail": "Email",
    "channelBoth": "In-app + Email",
    "saved": "Digest settings saved",
    "saveFailed": "Couldn't save digest settings"
  },
```

In `user-ui/src/locales/zh.json`, add the matching block:

```json
  "digest": {
    "title": "每日简报",
    "subtitle": "每天早上汇总昨日支出、你的资产变化和市场行情。",
    "enable": "开启每日简报",
    "time": "推送时间",
    "timezone": "时区",
    "channel": "推送方式",
    "channelInApp": "应用内",
    "channelEmail": "邮件",
    "channelBoth": "应用内 + 邮件",
    "saved": "简报设置已保存",
    "saveFailed": "简报设置保存失败"
  },
```

- [ ] **Step 3: Create the digest card**

Create `user-ui/src/components/profile/digest-card.tsx`. Follow the existing profile-card layout (see `account-card.tsx` / the model picker). Use shadcn `Switch`, `Select`, and a native `<input type="time">`. A short curated timezone list keeps it simple:

```tsx
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Switch } from '@/components/ui/switch';
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select';
import { ledgerApi } from '@/lib/api';

const TIMEZONES = [
  'Asia/Shanghai', 'Asia/Hong_Kong', 'Asia/Tokyo', 'Asia/Singapore',
  'America/New_York', 'America/Los_Angeles', 'Europe/London', 'Europe/Paris', 'UTC',
];

export function DigestCard() {
  const { t } = useTranslation();
  const [enabled, setEnabled] = useState(false);
  const [time, setTime] = useState('08:00');
  const [timezone, setTimezone] = useState('UTC');
  const [channel, setChannel] = useState<'in_app' | 'email' | 'both'>('in_app');
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    ledgerApi.digestSettings().then((r) => {
      setEnabled(r.settings.enabled);
      setTime(r.settings.send_time);
      // default the picker to the browser tz on first enable if server is UTC
      setTimezone(r.settings.timezone);
      setChannel(r.settings.channel);
      setLoaded(true);
    }).catch(() => setLoaded(true));
  }, []);

  async function save(next: Partial<{ enabled: boolean; time: string; timezone: string; channel: string }>) {
    const payload = {
      enabled, time, timezone, channel, ...next,
    };
    try {
      await ledgerApi.saveDigestSettings(payload);
      toast.success(t('digest.saved'));
    } catch {
      toast.error(t('digest.saveFailed'));
    }
  }

  if (!loaded) return null;

  return (
    <Card>
      <CardHeader>
        <CardTitle>{t('digest.title')}</CardTitle>
        <p className="text-sm text-muted-foreground">{t('digest.subtitle')}</p>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex items-center justify-between">
          <span className="text-sm">{t('digest.enable')}</span>
          <Switch
            checked={enabled}
            onCheckedChange={(v) => { setEnabled(v); save({ enabled: v }); }}
          />
        </div>
        {enabled && (
          <>
            <div className="flex items-center justify-between gap-3">
              <span className="text-sm">{t('digest.time')}</span>
              <input
                type="time"
                value={time}
                onChange={(e) => setTime(e.target.value)}
                onBlur={() => save({ time })}
                className="h-9 rounded-md border bg-background px-2 text-sm"
              />
            </div>
            <div className="flex items-center justify-between gap-3">
              <span className="text-sm">{t('digest.timezone')}</span>
              <Select value={timezone} onValueChange={(v) => { setTimezone(v); save({ timezone: v }); }}>
                <SelectTrigger className="h-9 w-[180px] text-sm"><SelectValue /></SelectTrigger>
                <SelectContent>
                  {TIMEZONES.map((tz) => (<SelectItem key={tz} value={tz}>{tz}</SelectItem>))}
                </SelectContent>
              </Select>
            </div>
            <div className="flex items-center justify-between gap-3">
              <span className="text-sm">{t('digest.channel')}</span>
              <Select value={channel} onValueChange={(v) => { const c = v as typeof channel; setChannel(c); save({ channel: c }); }}>
                <SelectTrigger className="h-9 w-[180px] text-sm"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="in_app">{t('digest.channelInApp')}</SelectItem>
                  <SelectItem value="email">{t('digest.channelEmail')}</SelectItem>
                  <SelectItem value="both">{t('digest.channelBoth')}</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </>
        )}
      </CardContent>
    </Card>
  );
}
```

> Verify the shadcn `Switch` component exists at `@/components/ui/switch`. If
> not, add it via the project's shadcn setup or substitute a styled checkbox.

- [ ] **Step 4: Mount the card in Profile**

In `user-ui/src/pages/Profile.tsx`, import and render `<DigestCard />` in the card stack (e.g. after the model-picker card, before Memory):

```tsx
import { DigestCard } from '@/components/profile/digest-card';
// ...in the JSX stack:
<DigestCard />
```

- [ ] **Step 5: Typecheck / build the UI**

Run: `cd examples/dashboard/user-ui && npm run build`
Expected: build succeeds (no TS errors). Fix any type mismatches against the actual `api.ts` helper names.

- [ ] **Step 6: Commit**

```bash
git add examples/dashboard/user-ui/src/lib/api.ts examples/dashboard/user-ui/src/components/profile/digest-card.tsx examples/dashboard/user-ui/src/pages/Profile.tsx examples/dashboard/user-ui/src/locales/en.json examples/dashboard/user-ui/src/locales/zh.json
git commit -m "feat(dashboard): Profile daily-digest settings card + api client + i18n"
```

---

### Task 9: Frontend — notification bell in the app shell

**Files:**
- Create: `examples/dashboard/user-ui/src/components/notifications/bell.tsx`
- Modify: `examples/dashboard/user-ui/src/components/app-shell.tsx` (mount the bell in the header)
- Modify: `examples/dashboard/user-ui/src/locales/en.json` + `zh.json` (add `notifications.*`)

- [ ] **Step 1: Add i18n keys**

In both locale files add a `notifications` block:

en.json:
```json
  "notifications": {
    "title": "Notifications",
    "empty": "Nothing here yet.",
    "markAllRead": "Mark all read"
  },
```

zh.json:
```json
  "notifications": {
    "title": "通知",
    "empty": "暂时没有通知。",
    "markAllRead": "全部已读"
  },
```

- [ ] **Step 2: Create the bell component**

Create `user-ui/src/components/notifications/bell.tsx`. Use a shadcn `Popover` + the existing icon set, poll on mount + every 5 min + on window focus, and render each notification's digest body compactly:

```tsx
import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Popover, PopoverContent, PopoverTrigger,
} from '@/components/ui/popover';
import { ledgerApi, type NotificationItem } from '@/lib/api';

function DigestBody({ body }: { body: any }) {
  if (!body || typeof body !== 'object') return null;
  const s = body.spending;
  const w = body.wealth;
  const m = body.market;
  return (
    <div className="space-y-1 text-xs text-muted-foreground">
      {s && <div>昨日支出 {Number(s.total).toFixed(2)} {s.currency}</div>}
      {w && <div>净值 {Number(w.net_worth).toFixed(2)} {w.currency}（{Number(w.net_delta) >= 0 ? '+' : ''}{Number(w.net_delta).toFixed(2)}）</div>}
      {m?.summary && <div>{m.summary}</div>}
    </div>
  );
}

export function NotificationBell() {
  const { t } = useTranslation();
  const [items, setItems] = useState<NotificationItem[]>([]);
  const [unread, setUnread] = useState(0);
  const [open, setOpen] = useState(false);

  const load = useCallback(() => {
    ledgerApi.notifications(false).then((r) => {
      setItems(r.notifications);
      setUnread(r.unread);
    }).catch(() => {});
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, 5 * 60 * 1000);
    const onFocus = () => load();
    window.addEventListener('focus', onFocus);
    return () => { clearInterval(id); window.removeEventListener('focus', onFocus); };
  }, [load]);

  async function onOpenChange(next: boolean) {
    setOpen(next);
    if (next && unread > 0) {
      await ledgerApi.markNotificationsRead().catch(() => {});
      setUnread(0);
    }
  }

  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger className="relative inline-flex h-9 w-9 items-center justify-center rounded-md hover:bg-accent" aria-label={t('notifications.title')}>
        {/* bell glyph — reuse the project's icon sprite if available */}
        <span aria-hidden>🔔</span>
        {unread > 0 && (
          <span className="absolute -right-0.5 -top-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-red-500 px-1 text-[10px] font-medium text-white">
            {unread > 9 ? '9+' : unread}
          </span>
        )}
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80 p-0">
        <div className="border-b px-3 py-2 text-sm font-medium">{t('notifications.title')}</div>
        <div className="max-h-96 overflow-y-auto">
          {items.length === 0 ? (
            <div className="px-3 py-6 text-center text-sm text-muted-foreground">{t('notifications.empty')}</div>
          ) : (
            items.map((n) => (
              <div key={n.id} className="border-b px-3 py-2 last:border-b-0">
                <div className="text-sm font-medium">{n.title}{n.body?.date ? ` · ${n.body.date}` : ''}</div>
                <DigestBody body={n.body} />
              </div>
            ))
          )}
        </div>
      </PopoverContent>
    </Popover>
  );
}
```

> Replace the 🔔 emoji with the project's actual icon component if one exists
> (check how other header icons are rendered in `app-shell.tsx`). Verify the
> shadcn `Popover` exists at `@/components/ui/popover`; if not, add it.

- [ ] **Step 3: Mount the bell in the app shell header**

In `user-ui/src/components/app-shell.tsx`, import `NotificationBell` and render it in the top-bar action area (next to the existing logout / language controls — match their placement):

```tsx
import { NotificationBell } from '@/components/notifications/bell';
// ...in the header actions JSX:
<NotificationBell />
```

- [ ] **Step 4: Build the UI**

Run: `cd examples/dashboard/user-ui && npm run build`
Expected: build succeeds.

- [ ] **Step 5: Commit**

```bash
git add examples/dashboard/user-ui/src/components/notifications/bell.tsx examples/dashboard/user-ui/src/components/app-shell.tsx examples/dashboard/user-ui/src/locales/en.json examples/dashboard/user-ui/src/locales/zh.json
git commit -m "feat(dashboard): in-app notification bell (unread badge + digest popover)"
```

---

## Final verification (after all tasks)

- [ ] `cargo test -p dashboard` — all tests green (digest DB, schedule, market parse, build, deliver).
- [ ] `cargo build -p dashboard` — clean build.
- [ ] `cd examples/dashboard/user-ui && npm run build` — UI builds.
- [ ] Manual end-to-end: set `digest_settings` for a test user with `send_time` a minute ahead in their tz, `channel='both'`, set `GEMINI_API_KEY` + `RESEND_API_KEY` + `DIGEST_FROM`, run `--serve`, wait for the tick, confirm (a) a `notifications` row appears / the bell badge increments, and (b) an email arrives (or a WARN is logged if keys absent). Confirm a second tick the same local day does NOT re-send.
- [ ] Dispatch a final code-reviewer over the whole branch.

## Notes for the implementer

- **TOOL_NAMES allowlist:** this feature adds no `#[harness::tool]`, so the
  `TOOL_NAMES` allowlist in `main.rs` is irrelevant here. The market brief is a
  direct Gemini call inside the cron, NOT a chat tool.
- **`include_dir!` rebuild gotcha (deploy-time):** the embedded UI is baked in
  via `include_dir!`. When building the release/musl binary after UI changes,
  `touch examples/dashboard/src/server.rs` first so the macro re-embeds `dist`.
  (Not needed for local `cargo test`/`build`.)
- **Don't hold the `AppConfig` RwLock across `.await`** (existing rule) — the
  cron doesn't touch `AppConfig`; it reads secrets from env.
- **Ports:** if you smoke-test a server, use a random high port (e.g. 6743),
  never 8080.
