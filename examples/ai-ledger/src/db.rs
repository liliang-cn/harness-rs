use crate::model::*;
use crate::portfolio::model::{Asset, AssetClass, PriceQuote, Trade, TradeKind};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, Result as SqlResult, params};
use rust_decimal::Decimal;
use std::path::Path;
use std::str::FromStr;

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct CachedQuote {
    pub cache_key: String,
    pub price: Decimal,
    pub currency: String,
    pub source: String,
    pub fetched_at: DateTime<Utc>,
}

/// One row of the per-user daily net-worth journal. Amounts are in
/// `base_currency` (already FX-converted at write time).
#[derive(Debug, Clone, serde::Serialize)]
pub struct NetWorthSnapshot {
    pub snapshot_date: String,    // YYYY-MM-DD
    pub base_currency: String,
    pub cash_amt: f64,
    pub investments_amt: f64,
    pub debt_amt: f64,
    pub net_amt: f64,
}

/// One row of the loans table. Used by the loans API and agent tools.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LoanRecord {
    pub account_id: String,
    pub user_id: String,
    pub counterparty: String,
    pub principal: String,
    pub apr: String,
    pub term_months: Option<i64>,
    pub monthly_payment: Option<String>,
    pub start_date: String,
    pub last_accrued_date: String,
    pub status: String,
    pub note: Option<String>,
}

impl Db {
    pub fn open(path: &Path) -> SqlResult<Self> {
        let conn = Connection::open(path)?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> SqlResult<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> SqlResult<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id              TEXT PRIMARY KEY,
                email           TEXT NOT NULL UNIQUE COLLATE NOCASE,
                password_hash   TEXT NOT NULL,
                tier            TEXT NOT NULL DEFAULT 'trial',  -- 'trial' | 'paid' | 'admin'
                invited_by      TEXT,
                invite_code_used TEXT,
                created_at      TEXT NOT NULL,
                preferred_model TEXT
            );

            CREATE TABLE IF NOT EXISTS invites (
                code            TEXT PRIMARY KEY,
                created_by      TEXT NOT NULL,
                uses_remaining  INTEGER NOT NULL DEFAULT 1,
                expires_at      TEXT,
                created_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_invites_creator ON invites(created_by);

            CREATE TABLE IF NOT EXISTS sessions (
                token           TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                last_seen_at    TEXT NOT NULL,
                expires_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);

            CREATE TABLE IF NOT EXISTS accounts (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                name            TEXT NOT NULL,
                kind            TEXT NOT NULL,
                currency        TEXT NOT NULL,
                opening_balance TEXT NOT NULL,
                created_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_accounts_user ON accounts(user_id);

            CREATE TABLE IF NOT EXISTS transactions (
                id                  TEXT PRIMARY KEY,
                user_id             TEXT NOT NULL,
                kind                TEXT NOT NULL,
                amount              TEXT NOT NULL,
                currency            TEXT NOT NULL,
                account_id          TEXT NOT NULL,
                counter_account_id  TEXT,
                category            TEXT,
                note                TEXT,
                occurred_at         TEXT NOT NULL,
                created_at          TEXT NOT NULL,
                FOREIGN KEY(account_id) REFERENCES accounts(id)
            );
            CREATE INDEX IF NOT EXISTS idx_txn_user_occurred ON transactions(user_id, occurred_at);
            CREATE INDEX IF NOT EXISTS idx_txn_user_category ON transactions(user_id, category);

            CREATE TABLE IF NOT EXISTS budgets (
                user_id         TEXT NOT NULL,
                category        TEXT NOT NULL,
                currency        TEXT NOT NULL,
                monthly_limit   TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                PRIMARY KEY (user_id, category, currency)
            );

            CREATE TABLE IF NOT EXISTS assets (
                id          TEXT PRIMARY KEY,
                user_id     TEXT NOT NULL,
                symbol      TEXT NOT NULL,
                name        TEXT NOT NULL,
                asset_class TEXT NOT NULL,
                provider_id TEXT,
                currency    TEXT NOT NULL,
                created_at  TEXT NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS uniq_assets_user_symbol
                ON assets(user_id, symbol COLLATE NOCASE);

            CREATE TABLE IF NOT EXISTS trades (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                asset_id        TEXT NOT NULL,
                kind            TEXT NOT NULL,
                qty             TEXT NOT NULL,
                price_per_unit  TEXT NOT NULL,
                currency        TEXT NOT NULL,
                fees            TEXT NOT NULL DEFAULT '0',
                occurred_at     TEXT NOT NULL,
                note            TEXT,
                created_at      TEXT NOT NULL,
                FOREIGN KEY(asset_id) REFERENCES assets(id)
            );
            CREATE INDEX IF NOT EXISTS idx_trades_user_asset    ON trades(user_id, asset_id);
            CREATE INDEX IF NOT EXISTS idx_trades_user_occurred ON trades(user_id, occurred_at);

            CREATE TABLE IF NOT EXISTS prices (
                user_id     TEXT NOT NULL,
                asset_id    TEXT NOT NULL,
                price       TEXT NOT NULL,
                currency    TEXT NOT NULL,
                fetched_at  TEXT NOT NULL,
                source      TEXT NOT NULL,
                PRIMARY KEY (user_id, asset_id, fetched_at)
            );

            -- Global market-data cache (NOT per-user). Lets us amortise an
            -- expensive upstream like Gemini grounding across all users +
            -- the deterministic refresh-prices loop.
            CREATE TABLE IF NOT EXISTS quote_cache (
                cache_key   TEXT PRIMARY KEY,
                price       TEXT NOT NULL,
                currency    TEXT NOT NULL,
                source      TEXT NOT NULL,
                fetched_at  TEXT NOT NULL
            );

            -- Recurring expenses (SaaS subscriptions, rent, gym, ...).
            -- `next_charge_date` is YYYY-MM-DD; on each charge it advances by
            -- `frequency`. Status flips to 'cancelled' on user cancel — we
            -- keep the row + history rather than deleting.
            CREATE TABLE IF NOT EXISTS subscriptions (
                id                TEXT PRIMARY KEY,
                user_id           TEXT NOT NULL,
                name              TEXT NOT NULL,
                amount            TEXT NOT NULL,
                currency          TEXT NOT NULL,
                frequency         TEXT NOT NULL, -- weekly|monthly|quarterly|yearly
                next_charge_date  TEXT NOT NULL, -- YYYY-MM-DD
                account_id        TEXT NOT NULL,
                category          TEXT,
                pay_channel       TEXT,          -- "Android/Google Play" etc.
                note              TEXT,
                status            TEXT NOT NULL DEFAULT 'active', -- active|cancelled
                created_at        TEXT NOT NULL,
                cancelled_at      TEXT,
                FOREIGN KEY(account_id) REFERENCES accounts(id)
            );
            CREATE INDEX IF NOT EXISTS idx_subs_user_next
                ON subscriptions(user_id, next_charge_date)
                WHERE status = 'active';

            -- Server-side persisted chat sessions + messages. The UI's FAB
            -- modal binds to one session at a time; the 我的 → 聊天记录 page
            -- lists every session for the user. Messages survive across
            -- browsers / reloads / devices.
            CREATE TABLE IF NOT EXISTS chat_sessions (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                title           TEXT,
                model_id        TEXT,
                message_count   INTEGER NOT NULL DEFAULT 0,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_chat_sessions_user_updated
                ON chat_sessions(user_id, updated_at DESC);

            CREATE TABLE IF NOT EXISTS chat_messages (
                id          TEXT PRIMARY KEY,
                session_id  TEXT NOT NULL,
                user_id     TEXT NOT NULL,
                role        TEXT NOT NULL, -- 'user' | 'asst'
                text        TEXT NOT NULL,
                iters       INTEGER,
                created_at  TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES chat_sessions(id)
            );
            CREATE INDEX IF NOT EXISTS idx_chat_messages_session
                ON chat_messages(session_id, created_at);

            -- Admin audit log: who did what, when. user_id is nullable for
            -- anonymous events (e.g. failed login by email). meta_json holds
            -- a small JSON blob with extra context (actor email, before/after
            -- on tier_change, etc).
            CREATE TABLE IF NOT EXISTS audit_events (
                id          TEXT PRIMARY KEY,
                user_id     TEXT,
                kind        TEXT NOT NULL,
                target_id   TEXT,
                meta_json   TEXT,
                tokens_in   INTEGER NOT NULL DEFAULT 0,
                tokens_out  INTEGER NOT NULL DEFAULT 0,
                created_ms  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_audit_user_time
                ON audit_events(user_id, created_ms DESC);
            CREATE INDEX IF NOT EXISTS idx_audit_kind_time
                ON audit_events(kind, created_ms DESC);

            CREATE TABLE IF NOT EXISTS loans (
                account_id        TEXT PRIMARY KEY,
                user_id           TEXT NOT NULL,
                counterparty      TEXT NOT NULL,
                principal         TEXT NOT NULL,
                apr               TEXT NOT NULL,
                term_months       INTEGER,
                monthly_payment   TEXT,
                start_date        TEXT NOT NULL,
                last_accrued_date TEXT NOT NULL,
                status            TEXT NOT NULL DEFAULT 'active',
                note              TEXT,
                created_at        TEXT NOT NULL,
                FOREIGN KEY(account_id) REFERENCES accounts(id)
            );
            CREATE INDEX IF NOT EXISTS idx_loans_user_status ON loans(user_id, status);

            -- KV table for admin-mutable provider config. Keys:
            --   deepseek_api_key, gemini_api_key, model_for_trial, model_for_paid
            -- On startup env vars seed missing rows; runtime reads from the
            -- in-memory AppConfig that mirrors this table.
            CREATE TABLE IF NOT EXISTS provider_config (
                key         TEXT PRIMARY KEY,
                value       TEXT NOT NULL,
                updated_ms  INTEGER NOT NULL
            );

            -- FX rate cache. One row per (base, quote, fetched_date). We
            -- fetch daily mid prices from exchangerate.host and key by ISO
            -- date so historical net-worth snapshots can be reproduced.
            CREATE TABLE IF NOT EXISTS fx_rates (
                base          TEXT NOT NULL,
                quote         TEXT NOT NULL,
                rate          TEXT NOT NULL,   -- decimal as text, USD -> EUR = "0.92"
                fetched_date  TEXT NOT NULL,   -- YYYY-MM-DD (UTC)
                source        TEXT NOT NULL,   -- 'exchangerate.host' | 'manual' | ...
                PRIMARY KEY (base, quote, fetched_date)
            );
            CREATE INDEX IF NOT EXISTS idx_fx_pair_date
                ON fx_rates(base, quote, fetched_date DESC);

            -- Per-user daily net-worth snapshot. Populated by a tokio cron
            -- around 00:05 UTC. Older rows are immutable; if the user
            -- backfills a missed account today's value won't retroactively
            -- change yesterday's snapshot — which is the correct accounting
            -- behavior for a journal.
            CREATE TABLE IF NOT EXISTS net_worth_snapshots (
                user_id          TEXT NOT NULL,
                snapshot_date    TEXT NOT NULL,    -- YYYY-MM-DD (UTC)
                base_currency    TEXT NOT NULL,
                cash_amt         TEXT NOT NULL,    -- sum of cash-kind accounts, in base
                investments_amt  TEXT NOT NULL,    -- sum of (qty * latest_price), in base
                debt_amt         TEXT NOT NULL,    -- sum of liability-kind accounts (positive number), in base
                net_amt          TEXT NOT NULL,    -- cash + investments - debt
                computed_at      TEXT NOT NULL,
                PRIMARY KEY (user_id, snapshot_date)
            );
            CREATE INDEX IF NOT EXISTS idx_nws_user_date
                ON net_worth_snapshots(user_id, snapshot_date DESC);
            "#,
        )?;
        // Idempotent column adds — keeps already-migrated databases working
        // without a separate migration framework (no rusqlite_migration).
        self.ensure_column("users", "preferred_model", "TEXT")?;
        self.ensure_column("users", "base_currency", "TEXT NOT NULL DEFAULT 'USD'")?;
        Ok(())
    }

    /// Add `col` of `typ` to `table` if it doesn't already exist. Safe to
    /// call on every startup — no-op when the column is present.
    fn ensure_column(&self, table: &str, col: &str, typ: &str) -> SqlResult<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<SqlResult<Vec<_>>>()?;
        drop(stmt);
        if !existing.iter().any(|c| c == col) {
            self.conn.execute(
                &format!("ALTER TABLE {table} ADD COLUMN {col} {typ}"),
                [],
            )?;
        }
        Ok(())
    }

    // ───── global quote_cache (market data, not per user) ─────

    pub fn get_cached_quote(&self, cache_key: &str) -> SqlResult<Option<CachedQuote>> {
        let mut stmt = self.conn.prepare(
            "SELECT cache_key, price, currency, source, fetched_at FROM quote_cache
             WHERE cache_key = ?1",
        )?;
        stmt.query_row(params![cache_key], |r| {
            let price_s: String = r.get(1)?;
            let fet_s: String = r.get(4)?;
            Ok(CachedQuote {
                cache_key: r.get(0)?,
                price: Decimal::from_str(&price_s).unwrap_or(Decimal::ZERO),
                currency: r.get(2)?,
                source: r.get(3)?,
                fetched_at: parse_rfc3339(&fet_s),
            })
        })
        .optional()
    }

    pub fn put_cached_quote(
        &self,
        cache_key: &str,
        price: Decimal,
        currency: &str,
        source: &str,
        fetched_at: DateTime<Utc>,
    ) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO quote_cache(cache_key, price, currency, source, fetched_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                cache_key,
                price.to_string(),
                currency,
                source,
                fetched_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    // ───── auth: users / sessions / invites ─────

    pub fn insert_user(&self, u: &crate::auth::User) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO users(
                id, email, password_hash, tier, invited_by, invite_code_used,
                created_at, preferred_model, base_currency
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                u.id,
                u.email,
                u.password_hash,
                u.tier,
                u.invited_by,
                u.invite_code_used,
                u.created_at.to_rfc3339(),
                u.preferred_model,
                u.base_currency,
            ],
        )?;
        Ok(())
    }

    fn row_to_user(r: &rusqlite::Row<'_>) -> SqlResult<crate::auth::User> {
        let created_s: String = r.get(6)?;
        Ok(crate::auth::User {
            id: r.get(0)?,
            email: r.get(1)?,
            password_hash: r.get(2)?,
            tier: r.get(3)?,
            invited_by: r.get(4)?,
            invite_code_used: r.get(5)?,
            created_at: DateTime::parse_from_rfc3339(&created_s)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            preferred_model: r.get(7).ok().flatten(),
            base_currency: r.get::<_, Option<String>>(8).ok().flatten().unwrap_or_else(|| "USD".into()),
        })
    }

    pub fn get_user_by_email(&self, email: &str) -> SqlResult<Option<crate::auth::User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, email, password_hash, tier, invited_by, invite_code_used,
                    created_at, preferred_model, base_currency
             FROM users WHERE email = ?1 COLLATE NOCASE",
        )?;
        stmt.query_row(params![email], Self::row_to_user).optional()
    }

    pub fn get_user_by_id(&self, id: &str) -> SqlResult<Option<crate::auth::User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, email, password_hash, tier, invited_by, invite_code_used,
                    created_at, preferred_model, base_currency
             FROM users WHERE id = ?1",
        )?;
        stmt.query_row(params![id], Self::row_to_user).optional()
    }

    /// Update the user's base_currency. Validated as ISO 4217 (3 uppercase
    /// letters) by the HTTP handler.
    pub fn set_user_base_currency(&self, user_id: &str, currency: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE users SET base_currency = ?1 WHERE id = ?2",
            params![currency, user_id],
        )? as u32)
    }

    /// Set the user's preferred model (or clear it with `None`). Paid/admin
    /// tier check happens in the HTTP handler, not here.
    pub fn set_user_preferred_model(
        &self,
        user_id: &str,
        model_id: Option<&str>,
    ) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE users SET preferred_model = ?1 WHERE id = ?2",
            params![model_id, user_id],
        )? as u32)
    }

    pub fn update_user_password(&self, user_id: &str, new_hash: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2",
            params![new_hash, user_id],
        )? as u32)
    }

    /// Drop every session except the one passed in. Used after a password
    /// change to log out all *other* devices.
    pub fn delete_other_sessions(&self, user_id: &str, keep_token: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token != ?2",
            params![user_id, keep_token],
        )? as u32)
    }

    pub fn count_users(&self) -> SqlResult<u32> {
        self.conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u32)
    }

    pub fn list_all_user_ids(&self) -> SqlResult<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT id FROM users")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    // ───── FX rates ─────

    /// Look up the cached rate for (base → quote) on a specific UTC date.
    /// Returns None if not cached; caller decides whether to fetch.
    pub fn get_fx_rate(&self, base: &str, quote: &str, date: &str) -> SqlResult<Option<f64>> {
        let mut stmt = self.conn.prepare(
            "SELECT rate FROM fx_rates
             WHERE base = ?1 AND quote = ?2 AND fetched_date = ?3",
        )?;
        let row: Option<String> = stmt
            .query_row(params![base, quote, date], |r| r.get(0))
            .optional()?;
        Ok(row.and_then(|s| s.parse::<f64>().ok()))
    }

    /// Latest cached rate for the pair on or before `date`. Used when today's
    /// fetch failed but yesterday's value is good enough.
    pub fn latest_fx_rate(&self, base: &str, quote: &str, on_or_before: &str) -> SqlResult<Option<f64>> {
        let mut stmt = self.conn.prepare(
            "SELECT rate FROM fx_rates
             WHERE base = ?1 AND quote = ?2 AND fetched_date <= ?3
             ORDER BY fetched_date DESC LIMIT 1",
        )?;
        let row: Option<String> = stmt
            .query_row(params![base, quote, on_or_before], |r| r.get(0))
            .optional()?;
        Ok(row.and_then(|s| s.parse::<f64>().ok()))
    }

    pub fn insert_fx_rate(
        &self,
        base: &str,
        quote: &str,
        rate: f64,
        date: &str,
        source: &str,
    ) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO fx_rates(base, quote, rate, fetched_date, source)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(base, quote, fetched_date) DO UPDATE SET
                rate = excluded.rate, source = excluded.source",
            params![base, quote, rate.to_string(), date, source],
        )?;
        Ok(())
    }

    // ───── net-worth snapshots ─────

    /// Insert (or replace) today's snapshot. Composite key (user_id, date)
    /// means re-running the cron mid-day overwrites — that's what we want
    /// when the user backfills accounts late.
    pub fn upsert_net_worth_snapshot(
        &self,
        user_id: &str,
        snapshot_date: &str,
        base_currency: &str,
        cash: f64,
        investments: f64,
        debt: f64,
    ) -> SqlResult<()> {
        let net = cash + investments - debt;
        self.conn.execute(
            "INSERT INTO net_worth_snapshots(
                user_id, snapshot_date, base_currency,
                cash_amt, investments_amt, debt_amt, net_amt, computed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(user_id, snapshot_date) DO UPDATE SET
                base_currency = excluded.base_currency,
                cash_amt = excluded.cash_amt,
                investments_amt = excluded.investments_amt,
                debt_amt = excluded.debt_amt,
                net_amt = excluded.net_amt,
                computed_at = excluded.computed_at",
            params![
                user_id,
                snapshot_date,
                base_currency,
                cash.to_string(),
                investments.to_string(),
                debt.to_string(),
                net.to_string(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn latest_net_worth_snapshot(&self, user_id: &str) -> SqlResult<Option<NetWorthSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT snapshot_date, base_currency, cash_amt, investments_amt, debt_amt, net_amt
             FROM net_worth_snapshots
             WHERE user_id = ?1
             ORDER BY snapshot_date DESC LIMIT 1",
        )?;
        stmt.query_row(params![user_id], Self::row_to_snapshot).optional()
    }

    pub fn net_worth_series(
        &self,
        user_id: &str,
        from_date: &str,
        to_date: &str,
    ) -> SqlResult<Vec<NetWorthSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT snapshot_date, base_currency, cash_amt, investments_amt, debt_amt, net_amt
             FROM net_worth_snapshots
             WHERE user_id = ?1 AND snapshot_date BETWEEN ?2 AND ?3
             ORDER BY snapshot_date ASC",
        )?;
        let rows = stmt.query_map(
            params![user_id, from_date, to_date],
            Self::row_to_snapshot,
        )?;
        rows.collect()
    }

    fn row_to_snapshot(r: &rusqlite::Row<'_>) -> SqlResult<NetWorthSnapshot> {
        let parse_dec = |s: String| s.parse::<f64>().unwrap_or(0.0);
        Ok(NetWorthSnapshot {
            snapshot_date: r.get(0)?,
            base_currency: r.get(1)?,
            cash_amt: parse_dec(r.get(2)?),
            investments_amt: parse_dec(r.get(3)?),
            debt_amt: parse_dec(r.get(4)?),
            net_amt: parse_dec(r.get(5)?),
        })
    }

    // ───── loans ─────

    /// Insert a new loan row keyed by `account_id`. The loan's
    /// `last_accrued_date` cursor is initialized to `start_date`, and the
    /// status is `'active'`. Caller is expected to have already created the
    /// matching `accounts` row (the FK references it).
    pub fn insert_loan(
        &self,
        account_id: &str,
        user_id: &str,
        counterparty: &str,
        principal: &str,
        apr: &str,
        term_months: Option<i64>,
        monthly_payment: Option<&str>,
        start_date: &str,
        note: Option<&str>,
    ) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO loans(
                account_id, user_id, counterparty, principal, apr,
                term_months, monthly_payment, start_date, last_accrued_date,
                status, note, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', ?10, ?11)",
            params![
                account_id,
                user_id,
                counterparty,
                principal,
                apr,
                term_months,
                monthly_payment,
                start_date,
                start_date, // last_accrued_date defaults to start_date
                note,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn row_to_loan(r: &rusqlite::Row<'_>) -> SqlResult<LoanRecord> {
        Ok(LoanRecord {
            account_id: r.get(0)?,
            user_id: r.get(1)?,
            counterparty: r.get(2)?,
            principal: r.get(3)?,
            apr: r.get(4)?,
            term_months: r.get::<_, Option<i64>>(5)?,
            monthly_payment: r.get::<_, Option<String>>(6)?,
            start_date: r.get(7)?,
            last_accrued_date: r.get(8)?,
            status: r.get(9)?,
            note: r.get::<_, Option<String>>(10)?,
        })
    }

    /// List all loans for `user_id`, including non-active (paid_off / cancelled),
    /// newest first. The UI/API can filter by status.
    pub fn list_loans(&self, user_id: &str) -> SqlResult<Vec<LoanRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT account_id, user_id, counterparty, principal, apr,
                    term_months, monthly_payment, start_date, last_accrued_date,
                    status, note
             FROM loans
             WHERE user_id = ?1
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![user_id], Self::row_to_loan)?;
        rows.collect()
    }

    pub fn get_loan_by_account(
        &self,
        user_id: &str,
        account_id: &str,
    ) -> SqlResult<Option<LoanRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT account_id, user_id, counterparty, principal, apr,
                    term_months, monthly_payment, start_date, last_accrued_date,
                    status, note
             FROM loans
             WHERE user_id = ?1 AND account_id = ?2",
        )?;
        stmt.query_row(params![user_id, account_id], Self::row_to_loan)
            .optional()
    }

    /// Advance the loan's accrual cursor. Called by the daily interest cron
    /// after it has posted the interest transaction(s) up to `date`.
    pub fn set_loan_last_accrued(&self, account_id: &str, date: &str) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE loans SET last_accrued_date = ?1 WHERE account_id = ?2",
            params![date, account_id],
        )?;
        Ok(())
    }

    /// Flip status — typically `'active'` → `'paid_off'` once the balance
    /// hits zero, or `'cancelled'` when the user deletes the loan.
    pub fn set_loan_status(&self, account_id: &str, status: &str) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE loans SET status = ?1 WHERE account_id = ?2",
            params![status, account_id],
        )?;
        Ok(())
    }

    /// Current balance of an account = `opening_balance` + net of every
    /// transaction touching it. Mirrors the per-account fold inside
    /// `net_worth::snapshot_now`, but returns one f64 for callers (the
    /// daily-interest cron) that just need today's number.
    ///
    /// Sign convention: for a debt account (Loan/Mortgage/Credit) the
    /// opening_balance is stored negative, and each "expense" booked on
    /// that account further decreases the balance — so the returned f64
    /// is negative for debt and positive for cash.
    pub fn compute_account_balance(&self, user_id: &str, account_id: &str) -> SqlResult<f64> {
        use rust_decimal::prelude::ToPrimitive;
        // Opening balance.
        let mut stmt = self.conn.prepare(
            "SELECT opening_balance FROM accounts WHERE user_id = ?1 AND id = ?2",
        )?;
        let opening: Option<String> = stmt
            .query_row(params![user_id, account_id], |r| r.get::<_, String>(0))
            .optional()?;
        let Some(opening_s) = opening else { return Ok(0.0) };
        let mut bal: f64 = Decimal::from_str(&opening_s)
            .unwrap_or(Decimal::ZERO)
            .to_f64()
            .unwrap_or(0.0);

        // Fold every txn that mentions this account (own leg or counter leg).
        let mut stmt = self.conn.prepare(
            "SELECT kind, amount, account_id, counter_account_id
             FROM transactions
             WHERE user_id = ?1
               AND (account_id = ?2 OR counter_account_id = ?2)",
        )?;
        let rows = stmt.query_map(params![user_id, account_id], |r| {
            let kind: String = r.get(0)?;
            let amt: String = r.get(1)?;
            let own: String = r.get(2)?;
            let counter: Option<String> = r.get(3)?;
            Ok((kind, amt, own, counter))
        })?;
        for row in rows {
            let (kind, amt_s, own, counter) = row?;
            let amt = Decimal::from_str(&amt_s)
                .unwrap_or(Decimal::ZERO)
                .to_f64()
                .unwrap_or(0.0);
            if own == account_id {
                match kind.as_str() {
                    "income" => bal += amt,
                    "expense" => bal -= amt,
                    "transfer" => bal -= amt, // outgoing leg
                    _ => {}
                }
            } else if counter.as_deref() == Some(account_id) && kind == "transfer" {
                bal += amt; // incoming leg
            }
        }
        Ok(bal)
    }

    /// Book a system-generated interest expense transaction on a loan
    /// account. Called by the daily accrual cron — keeps the txn history
    /// honest so the existing per-account fold in `net_worth::snapshot_now`
    /// naturally arrives at the post-interest balance.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_system_interest_transaction(
        &self,
        user_id: &str,
        account_id: &str,
        currency: &str,
        amount: Decimal,
        apr: f64,
        days: i64,
        date_iso: &str,
    ) -> SqlResult<()> {
        use uuid::Uuid;
        let id = Uuid::new_v4().to_string()[..8].to_string();
        let note = format!("daily accrual {days}d @ {:.4}% APR", apr * 100.0);
        let occurred_at = format!("{date_iso}T12:00:00Z");
        let created_at = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO transactions(
                id, user_id, kind, amount, currency, account_id, counter_account_id,
                category, note, occurred_at, created_at
             ) VALUES (?1, ?2, 'expense', ?3, ?4, ?5, NULL, 'interest', ?6, ?7, ?8)",
            params![
                id,
                user_id,
                amount.to_string(),
                currency,
                account_id,
                note,
                occurred_at,
                created_at,
            ],
        )?;
        Ok(())
    }

    pub fn insert_session(&self, s: &crate::auth::Session) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO sessions(token, user_id, created_at, last_seen_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                s.token,
                s.user_id,
                s.created_at.to_rfc3339(),
                s.last_seen_at.to_rfc3339(),
                s.expires_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, token: &str) -> SqlResult<Option<crate::auth::Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT token, user_id, created_at, last_seen_at, expires_at
             FROM sessions WHERE token = ?1",
        )?;
        stmt.query_row(params![token], |r| {
            let c: String = r.get(2)?;
            let l: String = r.get(3)?;
            let e: String = r.get(4)?;
            Ok(crate::auth::Session {
                token: r.get(0)?,
                user_id: r.get(1)?,
                created_at: parse_rfc3339(&c),
                last_seen_at: parse_rfc3339(&l),
                expires_at: parse_rfc3339(&e),
            })
        })
        .optional()
    }

    pub fn touch_session(&self, token: &str, now: DateTime<Utc>) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE sessions SET last_seen_at = ?1 WHERE token = ?2",
            params![now.to_rfc3339(), token],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, token: &str) -> SqlResult<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
        Ok(())
    }

    pub fn insert_invite(&self, i: &crate::auth::Invite) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO invites(code, created_by, uses_remaining, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                i.code,
                i.created_by,
                i.uses_remaining,
                i.expires_at.map(|d| d.to_rfc3339()),
                i.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn get_invite(&self, code: &str) -> SqlResult<Option<crate::auth::Invite>> {
        let mut stmt = self.conn.prepare(
            "SELECT code, created_by, uses_remaining, expires_at, created_at
             FROM invites WHERE code = ?1",
        )?;
        stmt.query_row(params![code], |r| {
            let exp_s: Option<String> = r.get(3)?;
            let c: String = r.get(4)?;
            Ok(crate::auth::Invite {
                code: r.get(0)?,
                created_by: r.get(1)?,
                uses_remaining: r.get::<_, i64>(2)? as i32,
                expires_at: exp_s.map(|s| parse_rfc3339(&s)),
                created_at: parse_rfc3339(&c),
            })
        })
        .optional()
    }

    pub fn consume_invite(&self, code: &str) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE invites SET uses_remaining = uses_remaining - 1
             WHERE code = ?1 AND uses_remaining > 0",
            params![code],
        )?;
        Ok(())
    }

    /// Active (unused) invites only — used codes are excluded so the
    /// caller's UI shows just what's still actionable. The code→user
    /// relationship for consumed codes lives on the `users` row instead
    /// (`invited_by`, `invite_code_used`).
    pub fn list_invites_by_creator(&self, user_id: &str) -> SqlResult<Vec<crate::auth::Invite>> {
        let mut stmt = self.conn.prepare(
            "SELECT code, created_by, uses_remaining, expires_at, created_at
             FROM invites
             WHERE created_by = ?1 AND uses_remaining > 0
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![user_id], |r| {
            let exp_s: Option<String> = r.get(3)?;
            let c: String = r.get(4)?;
            Ok(crate::auth::Invite {
                code: r.get(0)?,
                created_by: r.get(1)?,
                uses_remaining: r.get::<_, i64>(2)? as i32,
                expires_at: exp_s.map(|s| parse_rfc3339(&s)),
                created_at: parse_rfc3339(&c),
            })
        })?;
        rows.collect()
    }

    // ───── usage counts (for trial quotas) ─────

    pub fn count_user_transactions(&self, user_id: &str) -> SqlResult<u32> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM transactions WHERE user_id = ?1",
                params![user_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u32)
    }

    pub fn count_user_trades(&self, user_id: &str) -> SqlResult<u32> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM trades WHERE user_id = ?1",
                params![user_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u32)
    }

    pub fn count_user_assets(&self, user_id: &str) -> SqlResult<u32> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE user_id = ?1",
                params![user_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u32)
    }

    // ───── portfolio: assets ─────

    pub fn insert_asset(&self, user_id: &str, a: &Asset) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO assets(id, user_id, symbol, name, asset_class, provider_id, currency, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                a.id,
                user_id,
                a.symbol,
                a.name,
                a.asset_class.as_str(),
                a.provider_id,
                a.currency,
                a.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn row_to_asset(r: &rusqlite::Row<'_>) -> SqlResult<Asset> {
        let class_s: String = r.get(3)?;
        let class = AssetClass::parse(&class_s).unwrap_or(AssetClass::Other);
        let created_s: String = r.get(6)?;
        Ok(Asset {
            id: r.get(0)?,
            symbol: r.get(1)?,
            name: r.get(2)?,
            asset_class: class,
            provider_id: r.get(4)?,
            currency: r.get(5)?,
            created_at: DateTime::parse_from_rfc3339(&created_s)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    }

    pub fn list_assets(&self, user_id: &str) -> SqlResult<Vec<Asset>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, symbol, name, asset_class, provider_id, currency, created_at
             FROM assets WHERE user_id = ?1 ORDER BY symbol",
        )?;
        let rows = stmt.query_map(params![user_id], Self::row_to_asset)?;
        rows.collect()
    }

    pub fn get_asset_by_symbol(&self, user_id: &str, symbol: &str) -> SqlResult<Option<Asset>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, symbol, name, asset_class, provider_id, currency, created_at
             FROM assets WHERE user_id = ?1 AND symbol = ?2 COLLATE NOCASE",
        )?;
        stmt.query_row(params![user_id, symbol], Self::row_to_asset)
            .optional()
    }

    pub fn get_asset_by_id(&self, user_id: &str, id: &str) -> SqlResult<Option<Asset>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, symbol, name, asset_class, provider_id, currency, created_at
             FROM assets WHERE user_id = ?1 AND id = ?2",
        )?;
        stmt.query_row(params![user_id, id], Self::row_to_asset)
            .optional()
    }

    // ───── portfolio: trades ─────

    /// Delete an asset and cascade-delete its trades + cached prices.
    /// Returns (trades_deleted, prices_deleted).
    pub fn delete_asset(&self, user_id: &str, asset_id: &str) -> SqlResult<(u32, u32)> {
        let trades_n = self.conn.execute(
            "DELETE FROM trades WHERE user_id = ?1 AND asset_id = ?2",
            params![user_id, asset_id],
        )? as u32;
        let prices_n = self.conn.execute(
            "DELETE FROM prices WHERE user_id = ?1 AND asset_id = ?2",
            params![user_id, asset_id],
        )? as u32;
        self.conn.execute(
            "DELETE FROM assets WHERE user_id = ?1 AND id = ?2",
            params![user_id, asset_id],
        )?;
        Ok((trades_n, prices_n))
    }

    pub fn delete_trade(&self, user_id: &str, trade_id: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "DELETE FROM trades WHERE user_id = ?1 AND id = ?2",
            params![user_id, trade_id],
        )? as u32)
    }

    pub fn delete_transaction(&self, user_id: &str, txn_id: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "DELETE FROM transactions WHERE user_id = ?1 AND id = ?2",
            params![user_id, txn_id],
        )? as u32)
    }

    pub fn insert_trade(&self, user_id: &str, t: &Trade) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO trades(
                id, user_id, asset_id, kind, qty, price_per_unit, currency, fees,
                occurred_at, note, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                t.id,
                user_id,
                t.asset_id,
                t.kind.as_str(),
                t.qty.to_string(),
                t.price_per_unit.to_string(),
                t.currency,
                t.fees.to_string(),
                t.occurred_at.to_rfc3339(),
                t.note,
                t.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn row_to_trade(r: &rusqlite::Row<'_>) -> SqlResult<Trade> {
        let kind_s: String = r.get(2)?;
        let kind = TradeKind::parse(&kind_s).unwrap_or(TradeKind::Buy);
        let qty_s: String = r.get(3)?;
        let price_s: String = r.get(4)?;
        let fees_s: String = r.get(6)?;
        let occ_s: String = r.get(7)?;
        let cre_s: String = r.get(9)?;
        Ok(Trade {
            id: r.get(0)?,
            asset_id: r.get(1)?,
            kind,
            qty: Decimal::from_str(&qty_s).unwrap_or(Decimal::ZERO),
            price_per_unit: Decimal::from_str(&price_s).unwrap_or(Decimal::ZERO),
            currency: r.get(5)?,
            fees: Decimal::from_str(&fees_s).unwrap_or(Decimal::ZERO),
            occurred_at: DateTime::parse_from_rfc3339(&occ_s)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            note: r.get(8)?,
            created_at: DateTime::parse_from_rfc3339(&cre_s)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        })
    }

    pub fn list_trades(
        &self,
        user_id: &str,
        asset_id: Option<&str>,
        limit: usize,
    ) -> SqlResult<Vec<Trade>> {
        if let Some(aid) = asset_id {
            let mut stmt = self.conn.prepare(
                "SELECT id, asset_id, kind, qty, price_per_unit, currency, fees,
                        occurred_at, note, created_at
                 FROM trades WHERE user_id = ?1 AND asset_id = ?2
                 ORDER BY occurred_at DESC LIMIT ?3",
            )?;
            stmt.query_map(params![user_id, aid, limit as i64], Self::row_to_trade)?
                .collect()
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, asset_id, kind, qty, price_per_unit, currency, fees,
                        occurred_at, note, created_at
                 FROM trades WHERE user_id = ?1
                 ORDER BY occurred_at DESC LIMIT ?2",
            )?;
            stmt.query_map(params![user_id, limit as i64], Self::row_to_trade)?
                .collect()
        }
    }

    pub fn all_trades(&self, user_id: &str) -> SqlResult<Vec<Trade>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, asset_id, kind, qty, price_per_unit, currency, fees,
                    occurred_at, note, created_at
             FROM trades WHERE user_id = ?1 ORDER BY occurred_at",
        )?;
        let rows = stmt.query_map(params![user_id], Self::row_to_trade)?;
        rows.collect()
    }

    // ───── portfolio: prices ─────

    pub fn insert_price(&self, user_id: &str, p: &PriceQuote) -> SqlResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO prices(user_id, asset_id, price, currency, fetched_at, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                user_id,
                p.asset_id,
                p.price.to_string(),
                p.currency,
                p.fetched_at.to_rfc3339(),
                p.source,
            ],
        )?;
        Ok(())
    }

    pub fn latest_price(&self, user_id: &str, asset_id: &str) -> SqlResult<Option<PriceQuote>> {
        let mut stmt = self.conn.prepare(
            "SELECT asset_id, price, currency, fetched_at, source FROM prices
             WHERE user_id = ?1 AND asset_id = ?2 ORDER BY fetched_at DESC LIMIT 1",
        )?;
        stmt.query_row(params![user_id, asset_id], |r| {
            let price_s: String = r.get(1)?;
            let fet_s: String = r.get(3)?;
            Ok(PriceQuote {
                asset_id: r.get(0)?,
                price: Decimal::from_str(&price_s).unwrap_or(Decimal::ZERO),
                currency: r.get(2)?,
                fetched_at: DateTime::parse_from_rfc3339(&fet_s)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                source: r.get(4)?,
            })
        })
        .optional()
    }

    pub fn insert_account(&self, user_id: &str, a: &Account) -> SqlResult<()> {
        let kind = serde_json::to_string(&a.kind).unwrap_or("\"other\"".into());
        let kind = kind.trim_matches('"').to_string();
        self.conn.execute(
            "INSERT INTO accounts(id, user_id, name, kind, currency, opening_balance, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                a.id,
                user_id,
                a.name,
                kind,
                a.currency,
                a.opening_balance.to_string(),
                a.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_accounts(&self, user_id: &str) -> SqlResult<Vec<Account>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, kind, currency, opening_balance, created_at FROM accounts
             WHERE user_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![user_id], |r| {
            let kind_s: String = r.get(2)?;
            let kind: AccountKind =
                serde_json::from_str(&format!("\"{kind_s}\"")).unwrap_or(AccountKind::Other);
            let bal_s: String = r.get(4)?;
            let created_s: String = r.get(5)?;
            Ok(Account {
                id: r.get(0)?,
                name: r.get(1)?,
                kind,
                currency: r.get(3)?,
                opening_balance: Decimal::from_str(&bal_s).unwrap_or(Decimal::ZERO),
                created_at: DateTime::parse_from_rfc3339(&created_s)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?;
        rows.collect()
    }

    pub fn get_account(&self, user_id: &str, id: &str) -> SqlResult<Option<Account>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, kind, currency, opening_balance, created_at FROM accounts
             WHERE user_id = ?1 AND id = ?2",
        )?;
        stmt.query_row(params![user_id, id], |r| {
            let kind_s: String = r.get(2)?;
            let kind: AccountKind =
                serde_json::from_str(&format!("\"{kind_s}\"")).unwrap_or(AccountKind::Other);
            let bal_s: String = r.get(4)?;
            let created_s: String = r.get(5)?;
            Ok(Account {
                id: r.get(0)?,
                name: r.get(1)?,
                kind,
                currency: r.get(3)?,
                opening_balance: Decimal::from_str(&bal_s).unwrap_or(Decimal::ZERO),
                created_at: DateTime::parse_from_rfc3339(&created_s)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })
        .optional()
    }

    pub fn insert_transaction(&self, user_id: &str, t: &Transaction) -> SqlResult<()> {
        let kind = serde_json::to_string(&t.kind).unwrap_or("\"expense\"".into());
        let kind = kind.trim_matches('"').to_string();
        self.conn.execute(
            "INSERT INTO transactions(
                id, user_id, kind, amount, currency, account_id, counter_account_id,
                category, note, occurred_at, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                t.id,
                user_id,
                kind,
                t.amount.to_string(),
                t.currency,
                t.account_id,
                t.counter_account_id,
                t.category,
                t.note,
                t.occurred_at.to_rfc3339(),
                t.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_transactions(
        &self,
        user_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        category: Option<&str>,
        account_id: Option<&str>,
    ) -> SqlResult<Vec<Transaction>> {
        let mut sql = String::from(
            "SELECT id, kind, amount, currency, account_id, counter_account_id,
                    category, note, occurred_at, created_at
             FROM transactions
             WHERE user_id = ?1 AND occurred_at >= ?2 AND occurred_at <= ?3",
        );
        let mut p: Vec<String> = vec![
            user_id.to_string(),
            from.to_rfc3339(),
            to.to_rfc3339(),
        ];
        if let Some(c) = category {
            let idx = p.len() + 1;
            sql.push_str(&format!(" AND category = ?{idx}"));
            p.push(c.to_string());
        }
        if let Some(a) = account_id {
            let idx = p.len() + 1;
            sql.push_str(&format!(" AND account_id = ?{idx}"));
            p.push(a.to_string());
        }
        sql.push_str(" ORDER BY occurred_at DESC");

        let mut stmt = self.conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> =
            p.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params_dyn.as_slice(), |r| {
            let kind_s: String = r.get(1)?;
            let kind: TxnKind =
                serde_json::from_str(&format!("\"{kind_s}\"")).unwrap_or(TxnKind::Expense);
            let amt_s: String = r.get(2)?;
            let occ_s: String = r.get(8)?;
            let cre_s: String = r.get(9)?;
            Ok(Transaction {
                id: r.get(0)?,
                kind,
                amount: Decimal::from_str(&amt_s).unwrap_or(Decimal::ZERO),
                currency: r.get(3)?,
                account_id: r.get(4)?,
                counter_account_id: r.get(5)?,
                category: r.get(6)?,
                note: r.get(7)?,
                occurred_at: DateTime::parse_from_rfc3339(&occ_s)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                created_at: DateTime::parse_from_rfc3339(&cre_s)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?;
        rows.collect()
    }

    pub fn monthly_totals(
        &self,
        user_id: &str,
        year: i32,
        month: u32,
    ) -> SqlResult<Vec<CategoryTotal>> {
        let (from, to) = month_bounds(year, month);
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(category, '(uncategorised)') AS cat, currency,
                    amount, kind
             FROM transactions
             WHERE user_id = ?1 AND occurred_at >= ?2 AND occurred_at < ?3
               AND kind = 'expense'",
        )?;
        let rows = stmt.query_map(
            params![user_id, from.to_rfc3339(), to.to_rfc3339()],
            |r| {
                let cat: String = r.get(0)?;
                let cur: String = r.get(1)?;
                let amt_s: String = r.get(2)?;
                Ok((cat, cur, Decimal::from_str(&amt_s).unwrap_or(Decimal::ZERO)))
            },
        )?;
        use std::collections::HashMap;
        let mut acc: HashMap<(String, String), (Decimal, u32)> = HashMap::new();
        for row in rows {
            let (cat, cur, amt) = row?;
            let e = acc.entry((cat, cur)).or_insert((Decimal::ZERO, 0));
            e.0 += amt;
            e.1 += 1;
        }
        let mut out: Vec<CategoryTotal> = acc
            .into_iter()
            .map(|((category, currency), (total, count))| CategoryTotal {
                category,
                currency,
                total,
                count,
            })
            .collect();
        out.sort_by(|a, b| b.total.cmp(&a.total));
        Ok(out)
    }

    pub fn set_budget(&self, user_id: &str, b: &Budget) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO budgets(user_id, category, currency, monthly_limit, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id, category, currency) DO UPDATE
               SET monthly_limit = excluded.monthly_limit",
            params![
                user_id,
                b.category,
                b.currency,
                b.monthly_limit.to_string(),
                b.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_budgets(&self, user_id: &str) -> SqlResult<Vec<Budget>> {
        let mut stmt = self.conn.prepare(
            "SELECT category, currency, monthly_limit, created_at FROM budgets
             WHERE user_id = ?1 ORDER BY category",
        )?;
        let rows = stmt.query_map(params![user_id], |r| {
            let lim_s: String = r.get(2)?;
            let created_s: String = r.get(3)?;
            Ok(Budget {
                category: r.get(0)?,
                currency: r.get(1)?,
                monthly_limit: Decimal::from_str(&lim_s).unwrap_or(Decimal::ZERO),
                created_at: DateTime::parse_from_rfc3339(&created_s)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            })
        })?;
        rows.collect()
    }

    pub fn budget_status(&self, user_id: &str, year: i32, month: u32) -> SqlResult<Vec<BudgetStatus>> {
        let totals = self.monthly_totals(user_id, year, month)?;
        let budgets = self.list_budgets(user_id)?;
        let mut out = Vec::new();
        for b in budgets {
            let used = totals
                .iter()
                .find(|t| t.category == b.category && t.currency == b.currency)
                .map(|t| t.total)
                .unwrap_or(Decimal::ZERO);
            let remaining = b.monthly_limit - used;
            out.push(BudgetStatus {
                category: b.category,
                currency: b.currency,
                limit: b.monthly_limit,
                used,
                remaining,
                over_budget: used > b.monthly_limit,
            });
        }
        Ok(out)
    }

    /// Rename `from` → `to` for one user's transactions and budgets.
    /// For budgets, on collision keep the existing `to` row.
    pub fn rename_category(&self, user_id: &str, from: &str, to: &str) -> SqlResult<(u32, u32)> {
        let txn_updated = self.conn.execute(
            "UPDATE transactions SET category = ?1 WHERE user_id = ?2 AND category = ?3",
            params![to, user_id, from],
        )? as u32;
        self.conn.execute(
            "INSERT OR IGNORE INTO budgets(user_id, category, currency, monthly_limit, created_at)
             SELECT user_id, ?1, currency, monthly_limit, created_at FROM budgets
             WHERE user_id = ?2 AND category = ?3",
            params![to, user_id, from],
        )?;
        let budgets_removed = self.conn.execute(
            "DELETE FROM budgets WHERE user_id = ?1 AND category = ?2",
            params![user_id, from],
        )? as u32;
        Ok((txn_updated, budgets_removed))
    }

    pub fn distinct_categories(&self, user_id: &str) -> SqlResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT category FROM transactions
             WHERE user_id = ?1 AND category IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![user_id], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    // ───── subscriptions ─────

    pub fn insert_subscription(&self, user_id: &str, s: &Subscription) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO subscriptions(
                id, user_id, name, amount, currency, frequency, next_charge_date,
                account_id, category, pay_channel, note, status, created_at, cancelled_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                s.id,
                user_id,
                s.name,
                s.amount.to_string(),
                s.currency,
                s.frequency.as_str(),
                s.next_charge_date.format("%Y-%m-%d").to_string(),
                s.account_id,
                s.category,
                s.pay_channel,
                s.note,
                s.status,
                s.created_at.to_rfc3339(),
                s.cancelled_at.map(|d| d.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    fn row_to_subscription(r: &rusqlite::Row<'_>) -> SqlResult<Subscription> {
        let amount_s: String = r.get(2)?;
        let freq_s: String = r.get(4)?;
        let next_s: String = r.get(5)?;
        let created_s: String = r.get(11)?;
        let cancelled_s: Option<String> = r.get(12)?;
        Ok(Subscription {
            id: r.get(0)?,
            name: r.get(1)?,
            amount: Decimal::from_str(&amount_s).unwrap_or(Decimal::ZERO),
            currency: r.get(3)?,
            frequency: Frequency::parse(&freq_s).unwrap_or(Frequency::Monthly),
            next_charge_date: NaiveDate::parse_from_str(&next_s, "%Y-%m-%d")
                .unwrap_or_else(|_| Utc::now().date_naive()),
            account_id: r.get(6)?,
            category: r.get(7)?,
            pay_channel: r.get(8)?,
            note: r.get(9)?,
            status: r.get(10)?,
            created_at: parse_rfc3339(&created_s),
            cancelled_at: cancelled_s.map(|s| parse_rfc3339(&s)),
        })
    }

    pub fn list_subscriptions(
        &self,
        user_id: &str,
        only_active: bool,
    ) -> SqlResult<Vec<Subscription>> {
        let sql = if only_active {
            "SELECT id, name, amount, currency, frequency, next_charge_date,
                    account_id, category, pay_channel, note, status,
                    created_at, cancelled_at
             FROM subscriptions
             WHERE user_id = ?1 AND status = 'active'
             ORDER BY next_charge_date ASC"
        } else {
            "SELECT id, name, amount, currency, frequency, next_charge_date,
                    account_id, category, pay_channel, note, status,
                    created_at, cancelled_at
             FROM subscriptions
             WHERE user_id = ?1
             ORDER BY status ASC, next_charge_date ASC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![user_id], Self::row_to_subscription)?;
        rows.collect()
    }

    pub fn get_subscription(&self, user_id: &str, id: &str) -> SqlResult<Option<Subscription>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, amount, currency, frequency, next_charge_date,
                    account_id, category, pay_channel, note, status,
                    created_at, cancelled_at
             FROM subscriptions WHERE user_id = ?1 AND id = ?2",
        )?;
        stmt.query_row(params![user_id, id], Self::row_to_subscription)
            .optional()
    }

    pub fn cancel_subscription(&self, user_id: &str, id: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE subscriptions
             SET status = 'cancelled', cancelled_at = ?3
             WHERE user_id = ?1 AND id = ?2 AND status = 'active'",
            params![user_id, id, Utc::now().to_rfc3339()],
        )? as u32)
    }

    /// Set the next-charge date explicitly. Used by `--auto-charge-subs`
    /// after catching up multiple missed cycles in one shot.
    pub fn conn_update_subscription_next_date(
        &self,
        user_id: &str,
        id: &str,
        next: NaiveDate,
    ) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE subscriptions SET next_charge_date = ?3
             WHERE user_id = ?1 AND id = ?2",
            params![user_id, id, next.format("%Y-%m-%d").to_string()],
        )? as u32)
    }

    /// Advance the next-charge date by one period. Caller decides when to
    /// call this (typically: right after `insert_transaction` for the charge).
    pub fn advance_subscription(&self, user_id: &str, id: &str) -> SqlResult<u32> {
        let sub = match self.get_subscription(user_id, id)? {
            Some(s) => s,
            None => return Ok(0),
        };
        let next = sub.frequency.advance(sub.next_charge_date);
        Ok(self.conn.execute(
            "UPDATE subscriptions
             SET next_charge_date = ?3
             WHERE user_id = ?1 AND id = ?2",
            params![user_id, id, next.format("%Y-%m-%d").to_string()],
        )? as u32)
    }

    /// Active subscriptions whose `next_charge_date <= as_of` — drive the
    /// daily `--auto-charge-subs` runner.
    pub fn due_subscriptions(
        &self,
        user_id: &str,
        as_of: NaiveDate,
    ) -> SqlResult<Vec<Subscription>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, amount, currency, frequency, next_charge_date,
                    account_id, category, pay_channel, note, status,
                    created_at, cancelled_at
             FROM subscriptions
             WHERE user_id = ?1 AND status = 'active' AND next_charge_date <= ?2
             ORDER BY next_charge_date ASC",
        )?;
        let rows = stmt.query_map(
            params![user_id, as_of.format("%Y-%m-%d").to_string()],
            Self::row_to_subscription,
        )?;
        rows.collect()
    }

    pub fn due_subscriptions_all_users(
        &self,
        as_of: NaiveDate,
    ) -> SqlResult<Vec<(String, Subscription)>> {
        let mut stmt = self.conn.prepare(
            "SELECT user_id, id, name, amount, currency, frequency, next_charge_date,
                    account_id, category, pay_channel, note, status,
                    created_at, cancelled_at
             FROM subscriptions
             WHERE status = 'active' AND next_charge_date <= ?1
             ORDER BY user_id, next_charge_date ASC",
        )?;
        let rows = stmt.query_map(
            params![as_of.format("%Y-%m-%d").to_string()],
            |r| {
                let user_id: String = r.get(0)?;
                let amount_s: String = r.get(3)?;
                let freq_s: String = r.get(5)?;
                let next_s: String = r.get(6)?;
                let created_s: String = r.get(12)?;
                let cancelled_s: Option<String> = r.get(13)?;
                let sub = Subscription {
                    id: r.get(1)?,
                    name: r.get(2)?,
                    amount: Decimal::from_str(&amount_s).unwrap_or(Decimal::ZERO),
                    currency: r.get(4)?,
                    frequency: Frequency::parse(&freq_s).unwrap_or(Frequency::Monthly),
                    next_charge_date: NaiveDate::parse_from_str(&next_s, "%Y-%m-%d")
                        .unwrap_or_else(|_| Utc::now().date_naive()),
                    account_id: r.get(7)?,
                    category: r.get(8)?,
                    pay_channel: r.get(9)?,
                    note: r.get(10)?,
                    status: r.get(11)?,
                    created_at: parse_rfc3339(&created_s),
                    cancelled_at: cancelled_s.map(|s| parse_rfc3339(&s)),
                };
                Ok((user_id, sub))
            },
        )?;
        rows.collect()
    }

    // ───── chat sessions / messages ─────

    pub fn create_chat_session(
        &self,
        user_id: &str,
        id: &str,
        model_id: Option<&str>,
    ) -> SqlResult<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO chat_sessions(
                id, user_id, title, model_id, message_count, created_at, updated_at
             ) VALUES (?1, ?2, NULL, ?3, 0, ?4, ?4)",
            params![id, user_id, model_id, now],
        )?;
        Ok(())
    }

    fn row_to_chat_session(r: &rusqlite::Row<'_>) -> SqlResult<ChatSession> {
        let created_s: String = r.get(4)?;
        let updated_s: String = r.get(5)?;
        Ok(ChatSession {
            id: r.get(0)?,
            title: r.get(1)?,
            model_id: r.get(2)?,
            message_count: r.get::<_, i64>(3)? as u32,
            created_at: parse_rfc3339(&created_s),
            updated_at: parse_rfc3339(&updated_s),
        })
    }

    pub fn list_chat_sessions(&self, user_id: &str) -> SqlResult<Vec<ChatSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model_id, message_count, created_at, updated_at
             FROM chat_sessions
             WHERE user_id = ?1
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![user_id], Self::row_to_chat_session)?;
        rows.collect()
    }

    pub fn get_chat_session(
        &self,
        user_id: &str,
        id: &str,
    ) -> SqlResult<Option<ChatSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model_id, message_count, created_at, updated_at
             FROM chat_sessions WHERE user_id = ?1 AND id = ?2",
        )?;
        stmt.query_row(params![user_id, id], Self::row_to_chat_session)
            .optional()
    }

    pub fn get_chat_messages(
        &self,
        user_id: &str,
        session_id: &str,
        limit: usize,
    ) -> SqlResult<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, text, iters, created_at
             FROM chat_messages
             WHERE user_id = ?1 AND session_id = ?2
             ORDER BY created_at ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![user_id, session_id, limit as i64],
            |r| {
                let created_s: String = r.get(5)?;
                Ok(ChatMessage {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    role: r.get(2)?,
                    text: r.get(3)?,
                    iters: r.get::<_, Option<i64>>(4)?.map(|n| n as u32),
                    created_at: parse_rfc3339(&created_s),
                })
            },
        )?;
        rows.collect()
    }

    /// Append a message and bump the session's `updated_at` + `message_count`
    /// + (on first user message) `title`. Title is the first ~40 chars of
    /// the first user message — purely cosmetic.
    pub fn append_chat_message(
        &self,
        user_id: &str,
        session_id: &str,
        role: &str,
        text: &str,
        iters: Option<u32>,
    ) -> SqlResult<String> {
        use uuid::Uuid;
        let id = Uuid::new_v4().to_string()[..8].to_string();
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO chat_messages(
                id, session_id, user_id, role, text, iters, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, session_id, user_id, role, text, iters.map(|n| n as i64), now],
        )?;
        self.conn.execute(
            "UPDATE chat_sessions
             SET updated_at = ?3,
                 message_count = message_count + 1,
                 title = COALESCE(title, CASE WHEN ?4 = 'user'
                                              THEN substr(?5, 1, 40)
                                              ELSE NULL END)
             WHERE user_id = ?1 AND id = ?2",
            params![user_id, session_id, now, role, text],
        )?;
        Ok(id)
    }

    pub fn update_chat_session_model(
        &self,
        user_id: &str,
        session_id: &str,
        model_id: &str,
    ) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE chat_sessions SET model_id = ?3
             WHERE user_id = ?1 AND id = ?2",
            params![user_id, session_id, model_id],
        )? as u32)
    }

    /// Cascade-deletes messages too.
    pub fn delete_chat_session(&self, user_id: &str, id: &str) -> SqlResult<u32> {
        self.conn.execute(
            "DELETE FROM chat_messages WHERE user_id = ?1 AND session_id = ?2",
            params![user_id, id],
        )?;
        Ok(self.conn.execute(
            "DELETE FROM chat_sessions WHERE user_id = ?1 AND id = ?2",
            params![user_id, id],
        )? as u32)
    }

    pub fn count_user_subscriptions(&self, user_id: &str) -> SqlResult<u32> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM subscriptions WHERE user_id = ?1 AND status = 'active'",
                params![user_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u32)
    }

    // ───── admin: audit events ─────

    /// Insert an audit row. `id` is generated here; caller passes the rest.
    /// `tokens_in/out` should be 0 for non-LLM events.
    pub fn insert_audit(
        &self,
        user_id: Option<&str>,
        kind: &str,
        target_id: Option<&str>,
        meta_json: Option<&str>,
        tokens_in: i64,
        tokens_out: i64,
    ) -> SqlResult<()> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        // 8-byte hex id, no external crate needed.
        let id = format!(
            "{:016x}",
            (now_ms as u64).wrapping_mul(2654435761u64) ^ rand_u64()
        );
        self.conn.execute(
            "INSERT INTO audit_events(id, user_id, kind, target_id, meta_json,
                                      tokens_in, tokens_out, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                user_id,
                kind,
                target_id,
                meta_json,
                tokens_in,
                tokens_out,
                now_ms,
            ],
        )?;
        Ok(())
    }

    /// Paged audit-events query. `before_ms` is a cursor (rows strictly older
    /// than that timestamp); pass `i64::MAX` on the first page.
    pub fn list_audit(
        &self,
        user_id_filter: Option<&str>,
        kind_filter: Option<&str>,
        before_ms: i64,
        limit: u32,
    ) -> SqlResult<Vec<AuditEvent>> {
        let mut sql = String::from(
            "SELECT id, user_id, kind, target_id, meta_json, tokens_in, tokens_out, created_ms
             FROM audit_events WHERE created_ms < ?1",
        );
        let mut p: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(before_ms)];
        if let Some(uid) = user_id_filter {
            sql.push_str(" AND user_id = ?");
            sql.push_str(&(p.len() + 1).to_string());
            p.push(Box::new(uid.to_string()));
        }
        if let Some(k) = kind_filter {
            sql.push_str(" AND kind = ?");
            sql.push_str(&(p.len() + 1).to_string());
            p.push(Box::new(k.to_string()));
        }
        sql.push_str(" ORDER BY created_ms DESC LIMIT ?");
        sql.push_str(&(p.len() + 1).to_string());
        p.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(refs), |r| {
            Ok(AuditEvent {
                id: r.get(0)?,
                user_id: r.get(1)?,
                kind: r.get(2)?,
                target_id: r.get(3)?,
                meta_json: r.get(4)?,
                tokens_in: r.get(5)?,
                tokens_out: r.get(6)?,
                created_ms: r.get(7)?,
            })
        })?;
        rows.collect()
    }

    // ───── admin: users list with stats ─────

    /// All users with aggregated activity counts. Single query with LEFT JOINs;
    /// at the user counts we care about (<10k) this is fine.
    pub fn list_users_with_stats(&self) -> SqlResult<Vec<UserStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                u.id, u.email, u.tier, u.created_at,
                COALESCE((SELECT COUNT(*) FROM transactions  t WHERE t.user_id = u.id), 0) AS txn_count,
                COALESCE((SELECT COUNT(*) FROM chat_sessions c WHERE c.user_id = u.id), 0) AS chat_count,
                COALESCE((SELECT MAX(last_seen_at) FROM sessions s WHERE s.user_id = u.id), '') AS last_seen,
                COALESCE((SELECT SUM(tokens_in)  FROM audit_events e WHERE e.user_id = u.id), 0) AS tokens_in,
                COALESCE((SELECT SUM(tokens_out) FROM audit_events e WHERE e.user_id = u.id), 0) AS tokens_out,
                u.invited_by, u.invite_code_used
             FROM users u
             ORDER BY u.created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let created_s: String = r.get(3)?;
            let last_seen_s: String = r.get(6)?;
            Ok(UserStats {
                id: r.get(0)?,
                email: r.get(1)?,
                tier: r.get(2)?,
                created_at: parse_rfc3339(&created_s),
                txn_count: r.get::<_, i64>(4)? as u32,
                chat_count: r.get::<_, i64>(5)? as u32,
                last_seen_at: if last_seen_s.is_empty() {
                    None
                } else {
                    Some(parse_rfc3339(&last_seen_s))
                },
                tokens_in: r.get::<_, i64>(7)?,
                tokens_out: r.get::<_, i64>(8)?,
                invited_by: r.get(9)?,
                invite_code_used: r.get(10)?,
            })
        })?;
        rows.collect()
    }

    /// Update a user's tier. Returns rows affected (0 = user not found).
    pub fn update_user_tier(&self, user_id: &str, new_tier: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE users SET tier = ?1 WHERE id = ?2",
            params![new_tier, user_id],
        )? as u32)
    }

    /// Cascade-delete a user and EVERYTHING they own. Per-user memory JSONL
    /// file is the caller's responsibility (lives outside SQLite).
    pub fn delete_user_cascade(&self, user_id: &str) -> SqlResult<()> {
        let tx = self.conn.unchecked_transaction()?;
        // Order matters where FKs are declared (transactions → accounts,
        // trades → assets, chat_messages → chat_sessions). Delete children
        // first.
        tx.execute("DELETE FROM transactions   WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM trades         WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM prices         WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM assets         WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM accounts       WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM budgets        WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM subscriptions  WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM chat_messages  WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM chat_sessions  WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM sessions       WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM invites        WHERE created_by = ?1", params![user_id])?;
        // Leave audit_events behind (anonymise instead) so admin can still
        // see the deletion trail — but null out the user_id link.
        tx.execute(
            "UPDATE audit_events SET user_id = NULL WHERE user_id = ?1",
            params![user_id],
        )?;
        tx.execute("DELETE FROM users WHERE id = ?1", params![user_id])?;
        tx.commit()?;
        Ok(())
    }

    // ───── admin: provider config KV ─────

    pub fn provider_config_all(&self) -> SqlResult<std::collections::HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM provider_config")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect()
    }

    /// Insert key=value only if the key is not already present. Used at
    /// startup to seed env-var-supplied defaults without overwriting any
    /// admin edits.
    pub fn provider_config_seed_if_missing(&self, key: &str, value: &str) -> SqlResult<()> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT OR IGNORE INTO provider_config(key, value, updated_ms)
             VALUES (?1, ?2, ?3)",
            params![key, value, now_ms],
        )?;
        Ok(())
    }

    /// Upsert. Used by admin PATCH /api/admin/config.
    pub fn provider_config_set(&self, key: &str, value: &str) -> SqlResult<()> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.conn.execute(
            "INSERT INTO provider_config(key, value, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_ms = excluded.updated_ms",
            params![key, value, now_ms],
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEvent {
    pub id: String,
    pub user_id: Option<String>,
    pub kind: String,
    pub target_id: Option<String>,
    pub meta_json: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub created_ms: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct UserStats {
    pub id: String,
    pub email: String,
    pub tier: String,
    #[serde(serialize_with = "ser_rfc3339")]
    pub created_at: DateTime<Utc>,
    pub txn_count: u32,
    pub chat_count: u32,
    #[serde(serialize_with = "ser_rfc3339_opt")]
    pub last_seen_at: Option<DateTime<Utc>>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// The user id of whoever's invite code was consumed to register this
    /// user. `None` for the bootstrap admin or any future open-signup.
    pub invited_by: Option<String>,
    /// The exact code that was redeemed. Lets admin trace registrations
    /// back to a specific invite link.
    pub invite_code_used: Option<String>,
}

fn ser_rfc3339<S: serde::Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&t.to_rfc3339())
}
fn ser_rfc3339_opt<S: serde::Serializer>(
    t: &Option<DateTime<Utc>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match t {
        Some(t) => s.serialize_str(&t.to_rfc3339()),
        None => s.serialize_none(),
    }
}

/// Tiny non-crypto u64 — good enough for audit row ids when combined with
/// the millisecond timestamp.
fn rand_u64() -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Mix with std::process::id and a thread-local counter for non-zero entropy.
    let pid = std::process::id() as u64;
    let n = nanos as u64;
    n ^ (pid.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

pub(crate) fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

pub fn month_bounds(year: i32, month: u32) -> (DateTime<Utc>, DateTime<Utc>) {
    let from = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).unwrap();
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let to = Utc.with_ymd_and_hms(ny, nm, 1, 0, 0, 0).unwrap();
    (from, to)
}

pub fn today_year_month() -> (i32, u32) {
    let n = Utc::now();
    (n.year(), n.month())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn mk_id() -> String {
        Uuid::new_v4().to_string()[..8].to_string()
    }

    fn mk_account(name: &str, currency: &str) -> Account {
        Account {
            id: mk_id(),
            name: name.into(),
            kind: AccountKind::Debit,
            currency: currency.into(),
            opening_balance: Decimal::ZERO,
            created_at: Utc::now(),
        }
    }

    fn mk_expense(
        account_id: &str,
        amount: &str,
        currency: &str,
        category: &str,
        when: DateTime<Utc>,
    ) -> Transaction {
        Transaction {
            id: mk_id(),
            kind: TxnKind::Expense,
            amount: Decimal::from_str(amount).unwrap(),
            currency: currency.into(),
            account_id: account_id.into(),
            counter_account_id: None,
            category: Some(category.into()),
            note: None,
            occurred_at: when,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn roundtrip_account() {
        let db = Db::open_in_memory().unwrap();
        let a = mk_account("微信", "CNY");
        db.insert_account("u1", &a).unwrap();
        let fetched = db.get_account("u1", &a.id).unwrap().unwrap();
        assert_eq!(fetched.name, "微信");
        assert_eq!(fetched.currency, "CNY");
        assert_eq!(fetched.kind, AccountKind::Debit);
        let all = db.list_accounts("u1").unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn monthly_totals_aggregates_by_category_and_currency() {
        let db = Db::open_in_memory().unwrap();
        let a = mk_account("微信", "CNY");
        db.insert_account("u1", &a).unwrap();
        let when = Utc.with_ymd_and_hms(2026, 5, 10, 12, 0, 0).unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "200", "CNY", "餐饮", when))
            .unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "50", "CNY", "餐饮", when))
            .unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "1500", "CNY", "房租", when))
            .unwrap();
        let other_month = Utc.with_ymd_and_hms(2026, 4, 10, 12, 0, 0).unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "999", "CNY", "餐饮", other_month))
            .unwrap();

        let totals = db.monthly_totals("u1", 2026, 5).unwrap();
        assert_eq!(totals.len(), 2);
        let dining = totals.iter().find(|t| t.category == "餐饮").unwrap();
        assert_eq!(dining.total, Decimal::from_str("250").unwrap());
        assert_eq!(dining.count, 2);
        // sorted by total desc -> 房租 first
        assert_eq!(totals[0].category, "房租");
    }

    #[test]
    fn budget_status_tracks_used_and_over() {
        let db = Db::open_in_memory().unwrap();
        let a = mk_account("微信", "CNY");
        db.insert_account("u1", &a).unwrap();
        let when = Utc.with_ymd_and_hms(2026, 5, 10, 12, 0, 0).unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "1200", "CNY", "餐饮", when))
            .unwrap();
        db.set_budget("u1", &Budget {
            category: "餐饮".into(),
            currency: "CNY".into(),
            monthly_limit: Decimal::from_str("1000").unwrap(),
            created_at: Utc::now(),
        })
        .unwrap();

        let status = db.budget_status("u1", 2026, 5).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].category, "餐饮");
        assert_eq!(status[0].used, Decimal::from_str("1200").unwrap());
        assert_eq!(status[0].remaining, Decimal::from_str("-200").unwrap());
        assert!(status[0].over_budget);
    }

    #[test]
    fn portfolio_assets_trades_prices_roundtrip() {
        use crate::portfolio::model::{Asset, AssetClass, PriceQuote, Trade, TradeKind};
        let db = Db::open_in_memory().unwrap();
        let now = Utc::now();
        let a = Asset {
            id: "asset-aapl".into(),
            symbol: "AAPL".into(),
            name: "Apple Inc.".into(),
            asset_class: AssetClass::Stock,
            provider_id: None,
            currency: "USD".into(),
            created_at: now,
        };
        db.insert_asset("u1", &a).unwrap();
        assert_eq!(db.list_assets("u1").unwrap().len(), 1);
        assert!(db.get_asset_by_symbol("u1", "aapl").unwrap().is_some()); // case-insensitive
        assert!(db.get_asset_by_symbol("u1", "AAPL").unwrap().is_some());
        assert!(db.get_asset_by_id("u1", "asset-aapl").unwrap().is_some());

        let t = Trade {
            id: "tr-1".into(),
            asset_id: a.id.clone(),
            kind: TradeKind::Buy,
            qty: Decimal::from(100),
            price_per_unit: Decimal::from_str("190.50").unwrap(),
            currency: "USD".into(),
            fees: Decimal::from_str("1.50").unwrap(),
            occurred_at: now,
            note: Some("initial buy".into()),
            created_at: now,
        };
        db.insert_trade("u1", &t).unwrap();
        let trades = db.list_trades("u1", Some(&a.id), 50).unwrap();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].qty, Decimal::from(100));
        assert_eq!(trades[0].kind, TradeKind::Buy);

        let p = PriceQuote {
            asset_id: a.id.clone(),
            price: Decimal::from_str("198.42").unwrap(),
            currency: "USD".into(),
            fetched_at: now,
            source: "yahoo".into(),
        };
        db.insert_price("u1", &p).unwrap();
        let got = db.latest_price("u1", &a.id).unwrap().unwrap();
        assert_eq!(got.price, Decimal::from_str("198.42").unwrap());
        assert_eq!(got.source, "yahoo");
    }

    #[test]
    fn delete_asset_cascades_trades_and_prices() {
        use crate::portfolio::model::{Asset, AssetClass, PriceQuote, Trade, TradeKind};
        let db = Db::open_in_memory().unwrap();
        let now = Utc::now();
        let a = Asset {
            id: "asset-aapl".into(),
            symbol: "AAPL".into(),
            name: "Apple Inc.".into(),
            asset_class: AssetClass::Stock,
            provider_id: None,
            currency: "USD".into(),
            created_at: now,
        };
        db.insert_asset("u1", &a).unwrap();
        db.insert_trade("u1", &Trade {
            id: "t1".into(),
            asset_id: a.id.clone(),
            kind: TradeKind::Buy,
            qty: Decimal::from(50),
            price_per_unit: Decimal::from(190),
            currency: "USD".into(),
            fees: Decimal::ZERO,
            occurred_at: now,
            note: None,
            created_at: now,
        })
        .unwrap();
        db.insert_price("u1", &PriceQuote {
            asset_id: a.id.clone(),
            price: Decimal::from(200),
            currency: "USD".into(),
            fetched_at: now,
            source: "tencent".into(),
        })
        .unwrap();

        let (n_trades, n_prices) = db.delete_asset("u1", &a.id).unwrap();
        assert_eq!(n_trades, 1);
        assert_eq!(n_prices, 1);
        assert!(db.get_asset_by_id("u1", &a.id).unwrap().is_none());
        assert_eq!(db.all_trades("u1").unwrap().len(), 0);
        assert!(db.latest_price("u1", &a.id).unwrap().is_none());
    }

    #[test]
    fn rename_category_moves_transactions_and_handles_budget_collision() {
        let db = Db::open_in_memory().unwrap();
        let a = mk_account("微信", "CNY");
        db.insert_account("u1", &a).unwrap();
        let when = Utc.with_ymd_and_hms(2026, 5, 10, 12, 0, 0).unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "100", "CNY", "吃饭", when))
            .unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "50", "CNY", "吃饭", when))
            .unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "200", "CNY", "餐饮", when))
            .unwrap();
        // Pre-existing budget for the canonical name; the merge must NOT
        // overwrite it with the from-side budget.
        db.set_budget("u1", &Budget {
            category: "餐饮".into(),
            currency: "CNY".into(),
            monthly_limit: Decimal::from_str("1500").unwrap(),
            created_at: Utc::now(),
        })
        .unwrap();
        db.set_budget("u1", &Budget {
            category: "吃饭".into(),
            currency: "CNY".into(),
            monthly_limit: Decimal::from_str("999").unwrap(),
            created_at: Utc::now(),
        })
        .unwrap();

        let (txn_n, bud_n) = db.rename_category("u1", "吃饭", "餐饮").unwrap();
        assert_eq!(txn_n, 2);
        assert_eq!(bud_n, 1);

        let totals = db.monthly_totals("u1", 2026, 5).unwrap();
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].category, "餐饮");
        assert_eq!(totals[0].total, Decimal::from_str("350").unwrap());
        assert_eq!(totals[0].count, 3);

        let budgets = db.list_budgets("u1").unwrap();
        assert_eq!(budgets.len(), 1);
        assert_eq!(budgets[0].category, "餐饮");
        // The canonical (existing) budget survives.
        assert_eq!(budgets[0].monthly_limit, Decimal::from_str("1500").unwrap());
    }

    #[test]
    fn list_transactions_filters_by_range_and_category() {
        let db = Db::open_in_memory().unwrap();
        let a = mk_account("微信", "CNY");
        db.insert_account("u1", &a).unwrap();
        let when = Utc.with_ymd_and_hms(2026, 5, 10, 12, 0, 0).unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "100", "CNY", "餐饮", when))
            .unwrap();
        db.insert_transaction("u1", &mk_expense(&a.id, "200", "CNY", "交通", when))
            .unwrap();

        let (from, to) = month_bounds(2026, 5);
        let all = db.list_transactions("u1", from, to, None, None).unwrap();
        assert_eq!(all.len(), 2);
        let only_dining = db
            .list_transactions("u1", from, to, Some("餐饮"), None)
            .unwrap();
        assert_eq!(only_dining.len(), 1);
        assert_eq!(only_dining[0].category.as_deref(), Some("餐饮"));
    }
}
