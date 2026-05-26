//! SQLite layer for ai-note. Schema covers: users / sessions / invites
//! (same shape as ai-ledger so the auth module copies verbatim) + notes
//! + chat_sessions / chat_messages.
//!
//! Notes embeddings live in a BLOB column (`f32[dim]` little-endian) so a
//! single SELECT pulls everything needed for similarity search; no separate
//! vector store.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Result as SqlResult, params};
use std::path::Path;

pub struct Db {
    pub conn: Connection,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub space: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatSession {
    pub id: String,
    pub title: String,
    pub space: String,
    pub model_id: Option<String>,
    pub message_count: u32,
    #[serde(serialize_with = "ser_rfc3339")]
    pub created_at: DateTime<Utc>,
    #[serde(serialize_with = "ser_rfc3339")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub text: String,
    pub iters: Option<i64>,
    #[serde(serialize_with = "ser_rfc3339")]
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Goal {
    pub id: String,
    pub space: String,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub status: String,
    pub parent_id: Option<String>,
    pub target_date: Option<String>,
    pub review_interval_days: Option<i64>,
    pub next_review_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GoalReview {
    pub id: String,
    pub goal_id: String,
    pub progress: String,
    pub next_steps: String,
    pub created_at: String,
}

impl Db {
    pub fn open(path: &Path) -> SqlResult<Self> {
        let conn = Connection::open(path)?;
        let db = Db { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> SqlResult<()> {
        self.conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA foreign_keys=ON;

            CREATE TABLE IF NOT EXISTS users (
                id              TEXT PRIMARY KEY,
                email           TEXT NOT NULL UNIQUE,
                password_hash   TEXT NOT NULL,
                tier            TEXT NOT NULL DEFAULT 'trial',
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

            CREATE TABLE IF NOT EXISTS sessions (
                token           TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                last_seen_at    TEXT NOT NULL,
                expires_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_user ON sessions(user_id);

            -- The product table. embedding NULL = pending; embed worker
            -- fills it asynchronously. embedding_dim lets us detect a model
            -- swap (re-embed everything if it changes).
            CREATE TABLE IF NOT EXISTS notes (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                title           TEXT NOT NULL DEFAULT '',
                body            TEXT NOT NULL,
                tags            TEXT,
                space           TEXT NOT NULL DEFAULT 'life',
                embedding       BLOB,
                embedding_dim   INTEGER,
                embedding_at    TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_notes_user_updated
                ON notes(user_id, updated_at DESC);
            -- Fast scan for the background worker: pull rows that need embedding.
            CREATE INDEX IF NOT EXISTS idx_notes_pending_embed
                ON notes(user_id) WHERE embedding IS NULL;

            CREATE TABLE IF NOT EXISTS chat_sessions (
                id              TEXT PRIMARY KEY,
                user_id         TEXT NOT NULL,
                title           TEXT NOT NULL DEFAULT '新对话',
                model_id        TEXT,
                space           TEXT NOT NULL DEFAULT 'life',
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                message_count   INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_chat_sessions_user_updated
                ON chat_sessions(user_id, updated_at DESC);

            CREATE TABLE IF NOT EXISTS chat_messages (
                id              TEXT PRIMARY KEY,
                session_id      TEXT NOT NULL,
                user_id         TEXT NOT NULL,
                role            TEXT NOT NULL,
                text            TEXT NOT NULL,
                iters           INTEGER,
                created_at      TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES chat_sessions(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_chat_messages_session
                ON chat_messages(session_id, created_at);

            -- Admin audit log: who did what, when. user_id is nullable for
            -- anonymous events (e.g. failed login by email). meta_json holds
            -- a small JSON blob with extra context.
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

            -- KV table for admin-mutable provider config. Keys:
            --   deepseek_api_key, gemini_api_key, chat_model, chat_provider
            -- On startup env vars seed missing rows; runtime reads from the
            -- in-memory AppConfig that mirrors this table.
            CREATE TABLE IF NOT EXISTS provider_config (
                key         TEXT PRIMARY KEY,
                value       TEXT NOT NULL,
                updated_ms  INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS goals (
                id                   TEXT PRIMARY KEY,
                user_id              TEXT NOT NULL,
                space                TEXT NOT NULL DEFAULT 'life',
                kind                 TEXT NOT NULL DEFAULT 'goal',
                title                TEXT NOT NULL,
                detail               TEXT NOT NULL DEFAULT '',
                status               TEXT NOT NULL DEFAULT 'active',
                parent_id            TEXT,
                target_date          TEXT,
                review_interval_days INTEGER,
                next_review_at       TEXT,
                created_at           TEXT NOT NULL,
                updated_at           TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_goals_user_space_status ON goals(user_id, space, status);
            CREATE INDEX IF NOT EXISTS idx_goals_due ON goals(user_id, next_review_at);
            CREATE INDEX IF NOT EXISTS idx_goals_parent ON goals(parent_id);

            CREATE TABLE IF NOT EXISTS goal_reviews (
                id          TEXT PRIMARY KEY,
                goal_id     TEXT NOT NULL,
                user_id     TEXT NOT NULL,
                progress    TEXT NOT NULL,
                next_steps  TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_goal_reviews_goal ON goal_reviews(goal_id, created_at);
            "#,
        )?;
        // ── idempotent migrations (existing DBs) ──
        self.ensure_column("notes", "space", "TEXT NOT NULL DEFAULT 'life'")?;
        self.ensure_column("chat_sessions", "space", "TEXT NOT NULL DEFAULT 'life'")?;
        Ok(())
    }

    /// Idempotent `ALTER TABLE … ADD COLUMN`. Swallows the "duplicate column"
    /// error so re-running on an already-migrated DB is a no-op.
    fn ensure_column(&self, table: &str, col: &str, decl: &str) -> SqlResult<()> {
        let sql = format!("ALTER TABLE {table} ADD COLUMN {col} {decl}");
        match self.conn.execute(&sql, []) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    // ───── user / auth (same shape as ai-ledger) ─────

    pub fn insert_user(&self, u: &crate::auth::User) -> SqlResult<()> {
        self.conn.execute(
            "INSERT INTO users(id, email, password_hash, tier, invited_by,
                               invite_code_used, created_at, preferred_model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                u.id,
                u.email,
                u.password_hash,
                u.tier,
                u.invited_by,
                u.invite_code_used,
                u.created_at.to_rfc3339(),
                u.preferred_model,
            ],
        )?;
        Ok(())
    }

    pub fn get_user_by_email(&self, email: &str) -> SqlResult<Option<crate::auth::User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, email, password_hash, tier, invited_by, invite_code_used,
                    created_at, preferred_model
             FROM users WHERE email = ?1",
        )?;
        stmt.query_row(params![email], Self::row_to_user).optional()
    }

    pub fn get_user_by_id(&self, id: &str) -> SqlResult<Option<crate::auth::User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, email, password_hash, tier, invited_by, invite_code_used,
                    created_at, preferred_model
             FROM users WHERE id = ?1",
        )?;
        stmt.query_row(params![id], Self::row_to_user).optional()
    }

    fn row_to_user(r: &rusqlite::Row<'_>) -> SqlResult<crate::auth::User> {
        let c: String = r.get(6)?;
        Ok(crate::auth::User {
            id: r.get(0)?,
            email: r.get(1)?,
            password_hash: r.get(2)?,
            tier: r.get(3)?,
            invited_by: r.get(4)?,
            invite_code_used: r.get(5)?,
            created_at: parse_rfc3339(&c),
            preferred_model: r.get(7).ok().flatten(),
        })
    }

    pub fn count_users(&self) -> SqlResult<u32> {
        self.conn
            .query_row("SELECT COUNT(*) FROM users", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u32)
    }

    pub fn update_user_password(&self, user_id: &str, new_hash: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2",
            params![new_hash, user_id],
        )? as u32)
    }

    pub fn update_user_model(&self, user_id: &str, model: Option<&str>) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE users SET preferred_model = ?2 WHERE id = ?1",
            params![user_id, model],
        )? as u32)
    }

    pub fn delete_other_sessions(&self, user_id: &str, keep_token: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token != ?2",
            params![user_id, keep_token],
        )? as u32)
    }

    // ───── sessions ─────

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

    // ───── invites ─────

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

    // ───── notes ─────

    pub fn create_note(
        &self,
        user_id: &str,
        title: &str,
        body: &str,
        tags: &[String],
        space: &str,
    ) -> SqlResult<Note> {
        let id = random_id();
        let now = Utc::now();
        let tag_str = tags.join(",");
        self.conn.execute(
            "INSERT INTO notes(id, user_id, title, body, tags, space,
                               embedding, embedding_dim, embedding_at,
                               created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL, NULL, ?7, ?7)",
            params![id, user_id, title, body, tag_str, space, now.to_rfc3339()],
        )?;
        Ok(Note {
            id,
            title: title.to_string(),
            body: body.to_string(),
            tags: tags.to_vec(),
            space: space.to_string(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn get_note(&self, user_id: &str, id: &str) -> SqlResult<Option<Note>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, body, tags, space, created_at, updated_at
             FROM notes WHERE user_id = ?1 AND id = ?2",
        )?;
        stmt.query_row(params![user_id, id], row_to_note).optional()
    }

    /// Update title / body / tags. Any field passed as `None` is left as-is.
    /// Always clears the embedding (so the worker re-embeds next pass).
    pub fn update_note(
        &self,
        user_id: &str,
        id: &str,
        title: Option<&str>,
        body: Option<&str>,
        tags: Option<&[String]>,
    ) -> SqlResult<u32> {
        let now = Utc::now();
        let tag_str = tags.map(|t| t.join(","));
        let n = self.conn.execute(
            "UPDATE notes
             SET title = COALESCE(?3, title),
                 body  = COALESCE(?4, body),
                 tags  = COALESCE(?5, tags),
                 embedding = NULL, embedding_dim = NULL, embedding_at = NULL,
                 updated_at = ?6
             WHERE user_id = ?1 AND id = ?2",
            params![user_id, id, title, body, tag_str, now.to_rfc3339()],
        )?;
        Ok(n as u32)
    }

    pub fn delete_note(&self, user_id: &str, id: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "DELETE FROM notes WHERE user_id = ?1 AND id = ?2",
            params![user_id, id],
        )? as u32)
    }

    pub fn list_recent_notes(
        &self,
        user_id: &str,
        space: Option<&str>,
        limit: u32,
    ) -> SqlResult<Vec<Note>> {
        let mut sql = String::from(
            "SELECT id, title, body, tags, space, created_at, updated_at
             FROM notes WHERE user_id = ?1",
        );
        let mut p: Vec<String> = vec![user_id.to_string()];
        if let Some(sp) = space {
            sql.push_str(&format!(" AND space = ?{}", p.len() + 1));
            p.push(sp.to_string());
        }
        sql.push_str(&format!(" ORDER BY updated_at DESC LIMIT ?{}", p.len() + 1));
        p.push((limit as i64).to_string());
        let mut stmt = self.conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> =
            p.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params_dyn.as_slice(), row_to_note)?;
        rows.collect()
    }

    /// Like `list_recent_notes` but with inclusive `since` / `until` filters
    /// on `updated_at`. Either bound can be `None`. RFC3339 strings; SQLite
    /// orders them lexicographically which is correct for RFC3339 UTC.
    pub fn list_notes_in_range(
        &self,
        user_id: &str,
        space: Option<&str>,
        since: Option<&str>,
        until: Option<&str>,
        limit: u32,
    ) -> SqlResult<Vec<Note>> {
        let mut sql = String::from(
            "SELECT id, title, body, tags, space, created_at, updated_at
             FROM notes WHERE user_id = ?1",
        );
        let mut p: Vec<String> = vec![user_id.to_string()];
        if let Some(sp) = space {
            sql.push_str(&format!(" AND space = ?{}", p.len() + 1));
            p.push(sp.to_string());
        }
        if let Some(s) = since {
            sql.push_str(&format!(" AND updated_at >= ?{}", p.len() + 1));
            p.push(s.to_string());
        }
        if let Some(u) = until {
            sql.push_str(&format!(" AND updated_at <= ?{}", p.len() + 1));
            p.push(u.to_string());
        }
        sql.push_str(&format!(" ORDER BY updated_at DESC LIMIT ?{}", p.len() + 1));
        p.push((limit as i64).to_string());

        let mut stmt = self.conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> =
            p.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params_dyn.as_slice(), row_to_note)?;
        rows.collect()
    }

    // ───── embedding storage ─────

    /// Pull the next batch of notes that need embedding. Cheap because of the
    /// partial index `idx_notes_pending_embed`.
    pub fn pending_embeds(&self, batch: u32) -> SqlResult<Vec<PendingEmbed>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_id, title, body
             FROM notes WHERE embedding IS NULL
             ORDER BY updated_at ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![batch as i64], |r| {
            Ok(PendingEmbed {
                id: r.get(0)?,
                user_id: r.get(1)?,
                title: r.get(2)?,
                body: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn write_embedding(&self, id: &str, dim: usize, vector: &[f32]) -> SqlResult<u32> {
        // f32[dim] little-endian. We don't bother portable-encoding since we
        // own the DB; same machine reads it back.
        let mut bytes = Vec::with_capacity(vector.len() * 4);
        for v in vector {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let now = Utc::now().to_rfc3339();
        Ok(self.conn.execute(
            "UPDATE notes
             SET embedding = ?2, embedding_dim = ?3, embedding_at = ?4
             WHERE id = ?1",
            params![id, bytes, dim as i64, now],
        )? as u32)
    }

    /// Load all embedded notes for a user, returning the parsed vector.
    /// Used by the semantic search path; for a personal note app the per-user
    /// corpus is small enough (<10k) that linear scan is fine.
    pub fn list_embeddings(&self, user_id: &str, space: Option<&str>) -> SqlResult<Vec<NoteEmbedding>> {
        let mut sql = String::from(
            "SELECT id, title, body, tags, space, embedding, embedding_dim,
                    created_at, updated_at
             FROM notes
             WHERE user_id = ?1 AND embedding IS NOT NULL",
        );
        let mut p: Vec<String> = vec![user_id.to_string()];
        if let Some(sp) = space {
            sql.push_str(&format!(" AND space = ?{}", p.len() + 1));
            p.push(sp.to_string());
        }
        let mut stmt = self.conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> =
            p.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params_dyn.as_slice(), |r| {
            let blob: Vec<u8> = r.get(5)?;
            let dim: i64 = r.get(6)?;
            let dim = dim as usize;
            let mut vec = Vec::with_capacity(dim);
            for chunk in blob.chunks_exact(4) {
                vec.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            let tags_s: Option<String> = r.get(3)?;
            let tags = tags_s
                .map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect())
                .unwrap_or_default();
            let space: String = r.get(4)?;
            let c: String = r.get(7)?;
            let u: String = r.get(8)?;
            Ok(NoteEmbedding {
                note: Note {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    body: r.get(2)?,
                    tags,
                    space,
                    created_at: parse_rfc3339(&c),
                    updated_at: parse_rfc3339(&u),
                },
                embedding: vec,
            })
        })?;
        rows.collect()
    }

    pub fn count_notes(&self, user_id: &str, space: Option<&str>) -> SqlResult<u32> {
        let (sql, has_sp) = match space {
            Some(_) => ("SELECT COUNT(*) FROM notes WHERE user_id = ?1 AND space = ?2", true),
            None => ("SELECT COUNT(*) FROM notes WHERE user_id = ?1", false),
        };
        let n = if has_sp {
            self.conn.query_row(sql, params![user_id, space.unwrap()], |r| r.get::<_, i64>(0))?
        } else {
            self.conn.query_row(sql, params![user_id], |r| r.get::<_, i64>(0))?
        };
        Ok(n as u32)
    }

    // ───── admin: audit events ─────

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
        let id = format!(
            "{:016x}",
            (now_ms as u64).wrapping_mul(2654435761u64) ^ rand_u64()
        );
        self.conn.execute(
            "INSERT INTO audit_events(id, user_id, kind, target_id, meta_json,
                                      tokens_in, tokens_out, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id, user_id, kind, target_id, meta_json, tokens_in, tokens_out, now_ms,
            ],
        )?;
        Ok(())
    }

    /// Paged audit-events query. `before_ms` is a cursor.
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

    // ───── admin: users with aggregated stats ─────

    pub fn list_users_with_stats(&self) -> SqlResult<Vec<UserStats>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                u.id, u.email, u.tier, u.created_at,
                COALESCE((SELECT COUNT(*) FROM notes         n WHERE n.user_id = u.id), 0) AS note_count,
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
                note_count: r.get::<_, i64>(4)? as u32,
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

    pub fn update_user_tier(&self, user_id: &str, new_tier: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE users SET tier = ?1 WHERE id = ?2",
            params![new_tier, user_id],
        )? as u32)
    }

    /// Cascade-delete a user. Per-user memory / embedding stays inside the
    /// notes row, so just nuking notes is enough — no separate file cleanup.
    pub fn delete_user_cascade(&self, user_id: &str) -> SqlResult<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM notes          WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM chat_messages  WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM chat_sessions  WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM sessions       WHERE user_id = ?1", params![user_id])?;
        tx.execute("DELETE FROM invites        WHERE created_by = ?1", params![user_id])?;
        tx.execute(
            "UPDATE audit_events SET user_id = NULL WHERE user_id = ?1",
            params![user_id],
        )?;
        tx.execute("DELETE FROM users WHERE id = ?1", params![user_id])?;
        tx.commit()?;
        Ok(())
    }

    // ───── admin: provider_config KV ─────

    pub fn provider_config_all(&self) -> SqlResult<std::collections::HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM provider_config")?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.collect()
    }

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

    // ───── chat sessions ─────

    pub fn create_chat_session(
        &self,
        user_id: &str,
        id: &str,
        model_id: Option<&str>,
        space: &str,
    ) -> SqlResult<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO chat_sessions(id, user_id, title, model_id, space,
                                       created_at, updated_at, message_count)
             VALUES (?1, ?2, '新对话', ?3, ?4, ?5, ?5, 0)",
            params![id, user_id, model_id, space, now],
        )?;
        Ok(())
    }

    pub fn list_chat_sessions(&self, user_id: &str, space: &str) -> SqlResult<Vec<ChatSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, space, model_id, message_count, created_at, updated_at
             FROM chat_sessions WHERE user_id = ?1 AND space = ?2
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![user_id, space], row_to_session)?;
        rows.collect()
    }

    pub fn get_chat_session(&self, user_id: &str, id: &str) -> SqlResult<Option<ChatSession>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, space, model_id, message_count, created_at, updated_at
             FROM chat_sessions WHERE user_id = ?1 AND id = ?2",
        )?;
        stmt.query_row(params![user_id, id], row_to_session).optional()
    }

    pub fn delete_chat_session(&self, user_id: &str, id: &str) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "DELETE FROM chat_sessions WHERE user_id = ?1 AND id = ?2",
            params![user_id, id],
        )? as u32)
    }

    pub fn update_chat_session_model(
        &self,
        user_id: &str,
        id: &str,
        model_id: &str,
    ) -> SqlResult<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET model_id = ?3 WHERE user_id = ?1 AND id = ?2",
            params![user_id, id, model_id],
        )?;
        Ok(())
    }

    pub fn get_chat_messages(
        &self,
        user_id: &str,
        session_id: &str,
        limit: u32,
    ) -> SqlResult<Vec<ChatMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, text, iters, created_at
             FROM chat_messages WHERE user_id = ?1 AND session_id = ?2
             ORDER BY created_at ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![user_id, session_id, limit as i64], |r| {
            let c: String = r.get(5)?;
            Ok(ChatMessage {
                id: r.get(0)?,
                session_id: r.get(1)?,
                role: r.get(2)?,
                text: r.get(3)?,
                iters: r.get(4)?,
                created_at: parse_rfc3339(&c),
            })
        })?;
        rows.collect()
    }

    /// Append a message, bump message_count + updated_at, and set the session
    /// title from the first user message (trimmed to 40 chars).
    pub fn append_chat_message(
        &self,
        user_id: &str,
        session_id: &str,
        role: &str,
        text: &str,
        iters: Option<u32>,
    ) -> SqlResult<()> {
        let now = Utc::now().to_rfc3339();
        let id = random_id();
        self.conn.execute(
            "INSERT INTO chat_messages(id, session_id, user_id, role, text, iters, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, session_id, user_id, role, text, iters.map(|n| n as i64), now],
        )?;
        self.conn.execute(
            "UPDATE chat_sessions
             SET message_count = message_count + 1, updated_at = ?3
             WHERE user_id = ?1 AND id = ?2",
            params![user_id, session_id, now],
        )?;
        if role == "user" {
            // Set title only if still the default.
            let title: String = text.chars().take(40).collect();
            self.conn.execute(
                "UPDATE chat_sessions SET title = ?3
                 WHERE user_id = ?1 AND id = ?2 AND title = '新对话'",
                params![user_id, session_id, title],
            )?;
        }
        Ok(())
    }

    // ───── goals ─────

    #[allow(clippy::too_many_arguments)]
    pub fn create_goal(
        &self,
        user_id: &str,
        space: &str,
        kind: &str,
        title: &str,
        detail: &str,
        parent_id: Option<&str>,
        target_date: Option<&str>,
        review_interval_days: Option<i64>,
    ) -> SqlResult<Goal> {
        let id = random_id();
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        // Seed next_review_at for cadenced goals (not rules).
        let next_review_at: Option<String> = if kind == "goal" {
            review_interval_days.map(|d| (now + chrono::Duration::days(d)).to_rfc3339())
        } else {
            None
        };
        self.conn.execute(
            "INSERT INTO goals(id, user_id, space, kind, title, detail, status,
                               parent_id, target_date, review_interval_days,
                               next_review_at, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, ?10, ?11, ?11)",
            params![id, user_id, space, kind, title, detail, parent_id,
                    target_date, review_interval_days, next_review_at, now_s],
        )?;
        self.get_goal(user_id, &id).map(|o| o.expect("goal vanished after insert"))
    }

    pub fn get_goal(&self, user_id: &str, id: &str) -> SqlResult<Option<Goal>> {
        let sql = format!("SELECT {GOAL_COLS} FROM goals WHERE user_id = ?1 AND id = ?2");
        self.conn.prepare(&sql)?
            .query_row(params![user_id, id], row_to_goal).optional()
    }

    /// List goals in a space. `status` filters when Some. `only_due` keeps only
    /// goals whose next_review_at has passed (for the 到期复盘 list).
    pub fn list_goals(
        &self,
        user_id: &str,
        space: &str,
        status: Option<&str>,
        only_due: bool,
    ) -> SqlResult<Vec<Goal>> {
        let mut sql = format!("SELECT {GOAL_COLS} FROM goals WHERE user_id = ?1 AND space = ?2");
        let mut p: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(user_id.to_string()), Box::new(space.to_string())];
        if let Some(st) = status {
            sql.push_str(&format!(" AND status = ?{}", p.len() + 1));
            p.push(Box::new(st.to_string()));
        }
        if only_due {
            sql.push_str(&format!(
                " AND next_review_at IS NOT NULL AND next_review_at <= ?{}",
                p.len() + 1
            ));
            p.push(Box::new(Utc::now().to_rfc3339()));
        }
        sql.push_str(" ORDER BY COALESCE(next_review_at, target_date, created_at) ASC");
        let refs: Vec<&dyn rusqlite::ToSql> = p.iter().map(|b| b.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(refs), row_to_goal)?;
        rows.collect()
    }

    pub fn list_subgoals(&self, user_id: &str, parent_id: &str) -> SqlResult<Vec<Goal>> {
        let sql = format!(
            "SELECT {GOAL_COLS} FROM goals WHERE user_id = ?1 AND parent_id = ?2 \
             ORDER BY created_at ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![user_id, parent_id], row_to_goal)?;
        rows.collect()
    }

    pub fn count_due_goals(&self, user_id: &str, space: &str) -> SqlResult<u32> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM goals
             WHERE user_id = ?1 AND space = ?2 AND status = 'active'
               AND next_review_at IS NOT NULL AND next_review_at <= ?3",
            params![user_id, space, Utc::now().to_rfc3339()],
            |r| r.get::<_, i64>(0),
        ).map(|n| n as u32)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_goal(
        &self,
        user_id: &str,
        id: &str,
        status: Option<&str>,
        title: Option<&str>,
        detail: Option<&str>,
        target_date: Option<&str>,
        review_interval_days: Option<i64>,
    ) -> SqlResult<u32> {
        let now = Utc::now().to_rfc3339();
        let n = self.conn.execute(
            "UPDATE goals SET
               status = COALESCE(?3, status),
               title  = COALESCE(?4, title),
               detail = COALESCE(?5, detail),
               target_date = COALESCE(?6, target_date),
               review_interval_days = COALESCE(?7, review_interval_days),
               updated_at = ?8
             WHERE user_id = ?1 AND id = ?2",
            params![user_id, id, status, title, detail, target_date,
                    review_interval_days, now],
        )?;
        Ok(n as u32)
    }

    /// Delete a goal plus its direct subgoals and all its reviews.
    pub fn delete_goal(&self, user_id: &str, id: &str) -> SqlResult<u32> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM goal_reviews WHERE user_id = ?1 AND goal_id = ?2",
                   params![user_id, id])?;
        tx.execute("DELETE FROM goals WHERE user_id = ?1 AND parent_id = ?2",
                   params![user_id, id])?;
        let n = tx.execute("DELETE FROM goals WHERE user_id = ?1 AND id = ?2",
                   params![user_id, id])?;
        tx.commit()?;
        Ok(n as u32)
    }

    /// Insert a review and advance the goal's next_review_at by its interval
    /// (or by `override_days` if provided).
    pub fn add_review(
        &self,
        user_id: &str,
        goal_id: &str,
        progress: &str,
        next_steps: &str,
        override_days: Option<i64>,
    ) -> SqlResult<GoalReview> {
        let id = random_id();
        let now = Utc::now();
        let now_s = now.to_rfc3339();
        self.conn.execute(
            "INSERT INTO goal_reviews(id, goal_id, user_id, progress, next_steps, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, goal_id, user_id, progress, next_steps, now_s],
        )?;
        // Advance next_review_at: use override, else the goal's interval.
        let interval: Option<i64> = override_days.or_else(|| {
            self.conn.query_row(
                "SELECT review_interval_days FROM goals WHERE user_id = ?1 AND id = ?2",
                params![user_id, goal_id], |r| r.get(0),
            ).optional().ok().flatten()
        });
        if let Some(d) = interval {
            let next = (now + chrono::Duration::days(d)).to_rfc3339();
            self.conn.execute(
                "UPDATE goals SET next_review_at = ?3, updated_at = ?4
                 WHERE user_id = ?1 AND id = ?2",
                params![user_id, goal_id, next, now_s],
            )?;
        }
        Ok(GoalReview {
            id, goal_id: goal_id.to_string(),
            progress: progress.to_string(), next_steps: next_steps.to_string(),
            created_at: now_s,
        })
    }

    pub fn list_reviews(&self, user_id: &str, goal_id: &str, limit: u32) -> SqlResult<Vec<GoalReview>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, goal_id, progress, next_steps, created_at
             FROM goal_reviews WHERE user_id = ?1 AND goal_id = ?2
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![user_id, goal_id, limit as i64], |r| {
            Ok(GoalReview {
                id: r.get(0)?, goal_id: r.get(1)?,
                progress: r.get(2)?, next_steps: r.get(3)?, created_at: r.get(4)?,
            })
        })?;
        rows.collect()
    }

}

#[derive(Debug, Clone)]
pub struct PendingEmbed {
    pub id: String,
    pub user_id: String,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct NoteEmbedding {
    pub note: Note,
    pub embedding: Vec<f32>,
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
    pub note_count: u32,
    pub chat_count: u32,
    #[serde(serialize_with = "ser_rfc3339_opt")]
    pub last_seen_at: Option<DateTime<Utc>>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub invited_by: Option<String>,
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

fn rand_u64() -> u64 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    (nanos as u64) ^ (pid.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

fn row_to_goal(r: &rusqlite::Row<'_>) -> SqlResult<Goal> {
    Ok(Goal {
        id: r.get(0)?,
        space: r.get(1)?,
        kind: r.get(2)?,
        title: r.get(3)?,
        detail: r.get(4)?,
        status: r.get(5)?,
        parent_id: r.get(6)?,
        target_date: r.get(7)?,
        review_interval_days: r.get(8)?,
        next_review_at: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

const GOAL_COLS: &str =
    "id, space, kind, title, detail, status, parent_id, target_date, \
     review_interval_days, next_review_at, created_at, updated_at";

fn row_to_note(r: &rusqlite::Row<'_>) -> SqlResult<Note> {
    let tags_s: Option<String> = r.get(3)?;
    let tags = tags_s
        .map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let space: String = r.get(4)?;
    let c: String = r.get(5)?;
    let u: String = r.get(6)?;
    Ok(Note {
        id: r.get(0)?,
        title: r.get(1)?,
        body: r.get(2)?,
        tags,
        space,
        created_at: parse_rfc3339(&c),
        updated_at: parse_rfc3339(&u),
    })
}

fn row_to_session(r: &rusqlite::Row<'_>) -> SqlResult<ChatSession> {
    let c: String = r.get(5)?;
    let u: String = r.get(6)?;
    Ok(ChatSession {
        id: r.get(0)?,
        title: r.get(1)?,
        space: r.get(2)?,
        model_id: r.get(3)?,
        message_count: r.get::<_, i64>(4)? as u32,
        created_at: parse_rfc3339(&c),
        updated_at: parse_rfc3339(&u),
    })
}

pub(crate) fn parse_rfc3339(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn random_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> Db {
        let p = std::env::temp_dir().join(format!("ainote-test-{}.db", random_id()));
        Db::open(&p).unwrap()
    }

    #[test]
    fn notes_are_space_scoped() {
        let db = tmp_db();
        db.create_note("u1", "w", "work note", &[], "work").unwrap();
        db.create_note("u1", "l", "life note", &[], "life").unwrap();
        let work = db.list_recent_notes("u1", Some("work"), 50).unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].space, "work");
        let all = db.list_recent_notes("u1", None, 50).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(db.count_notes("u1", Some("life")).unwrap(), 1);
    }

    #[test]
    fn chat_sessions_scoped_and_counted() {
        let db = tmp_db();
        db.create_chat_session("u1", "s1", Some("deepseek-v4-flash"), "work").unwrap();
        db.append_chat_message("u1", "s1", "user", "hello work", None).unwrap();
        db.append_chat_message("u1", "s1", "asst", "hi", Some(1)).unwrap();
        let work = db.list_chat_sessions("u1", "work").unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].message_count, 2);
        assert_eq!(work[0].title, "hello work");
        assert!(db.list_chat_sessions("u1", "life").unwrap().is_empty());
        assert_eq!(db.get_chat_messages("u1", "s1", 10).unwrap().len(), 2);
    }

    #[test]
    fn goals_create_list_due_and_review() {
        let db = tmp_db();
        // a cadenced work goal — next_review_at seeded to now+30d (NOT due yet)
        let g = db.create_goal("u1", "work", "goal", "架构专家", "", None,
                               Some("2026-09-30"), Some(30)).unwrap();
        assert_eq!(g.space, "work");
        assert!(g.next_review_at.is_some());
        // a rule — no cadence, never due
        db.create_goal("u1", "life", "rule", "股票不要操作", "", None, None, None).unwrap();

        // space scoping + status filter
        assert_eq!(db.list_goals("u1", "work", Some("active"), false).unwrap().len(), 1);
        assert_eq!(db.list_goals("u1", "life", Some("active"), false).unwrap().len(), 1);
        // nothing due yet (30d out)
        assert_eq!(db.list_goals("u1", "work", Some("active"), true).unwrap().len(), 0);
        assert_eq!(db.count_due_goals("u1", "work").unwrap(), 0);

        // decompose: a subgoal under g
        let sub = db.create_goal("u1", "work", "goal", "打牢分布式基础", "",
                                 Some(&g.id), None, None).unwrap();
        assert_eq!(db.list_subgoals("u1", &g.id).unwrap().len(), 1);
        assert_eq!(sub.parent_id.as_deref(), Some(g.id.as_str()));

        // review advances cadence; adding a review keeps it from being due
        let r = db.add_review("u1", &g.id, "学了 raft", "下月做一次演练", None).unwrap();
        assert_eq!(r.goal_id, g.id);
        assert_eq!(db.list_reviews("u1", &g.id, 10).unwrap().len(), 1);

        // delete cascades subgoals + reviews
        assert_eq!(db.delete_goal("u1", &g.id).unwrap(), 1);
        assert!(db.get_goal("u1", &g.id).unwrap().is_none());
        assert_eq!(db.list_subgoals("u1", &g.id).unwrap().len(), 0);
        assert_eq!(db.list_reviews("u1", &g.id, 10).unwrap().len(), 0);
    }

    #[test]
    fn goals_due_when_review_past() {
        let db = tmp_db();
        let g = db.create_goal("u1", "work", "goal", "x", "", None, None, Some(7)).unwrap();
        // force next_review_at into the past
        db.conn.execute(
            "UPDATE goals SET next_review_at = ?2 WHERE id = ?1",
            rusqlite::params![g.id, "2000-01-01T00:00:00+00:00"],
        ).unwrap();
        assert_eq!(db.list_goals("u1", "work", Some("active"), true).unwrap().len(), 1);
        assert_eq!(db.count_due_goals("u1", "work").unwrap(), 1);
    }
}
