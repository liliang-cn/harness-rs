use crate::model::*;
use crate::portfolio::model::{Asset, AssetClass, PriceQuote, Trade, TradeKind};
use chrono::{DateTime, Datelike, TimeZone, Utc};
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
                created_at      TEXT NOT NULL
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
            "#,
        )?;
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
            "INSERT INTO users(id, email, password_hash, tier, invited_by, invite_code_used, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                u.id,
                u.email,
                u.password_hash,
                u.tier,
                u.invited_by,
                u.invite_code_used,
                u.created_at.to_rfc3339(),
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
        })
    }

    pub fn get_user_by_email(&self, email: &str) -> SqlResult<Option<crate::auth::User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, email, password_hash, tier, invited_by, invite_code_used, created_at
             FROM users WHERE email = ?1 COLLATE NOCASE",
        )?;
        stmt.query_row(params![email], Self::row_to_user).optional()
    }

    pub fn get_user_by_id(&self, id: &str) -> SqlResult<Option<crate::auth::User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, email, password_hash, tier, invited_by, invite_code_used, created_at
             FROM users WHERE id = ?1",
        )?;
        stmt.query_row(params![id], Self::row_to_user).optional()
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

    pub fn list_invites_by_creator(&self, user_id: &str) -> SqlResult<Vec<crate::auth::Invite>> {
        let mut stmt = self.conn.prepare(
            "SELECT code, created_by, uses_remaining, expires_at, created_at
             FROM invites WHERE created_by = ?1 ORDER BY created_at DESC",
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
