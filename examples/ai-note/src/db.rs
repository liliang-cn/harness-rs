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
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
            "#,
        )?;
        Ok(())
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

    pub fn create_note(&self, user_id: &str, title: &str, body: &str, tags: &[String]) -> SqlResult<Note> {
        let id = random_id();
        let now = Utc::now();
        let tag_str = tags.join(",");
        self.conn.execute(
            "INSERT INTO notes(id, user_id, title, body, tags,
                               embedding, embedding_dim, embedding_at,
                               created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL, ?6, ?6)",
            params![id, user_id, title, body, tag_str, now.to_rfc3339()],
        )?;
        Ok(Note {
            id,
            title: title.to_string(),
            body: body.to_string(),
            tags: tags.to_vec(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn get_note(&self, user_id: &str, id: &str) -> SqlResult<Option<Note>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, body, tags, created_at, updated_at
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

    pub fn list_recent_notes(&self, user_id: &str, limit: u32) -> SqlResult<Vec<Note>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, body, tags, created_at, updated_at
             FROM notes WHERE user_id = ?1
             ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![user_id, limit as i64], row_to_note)?;
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
    pub fn list_embeddings(&self, user_id: &str) -> SqlResult<Vec<NoteEmbedding>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, body, tags, embedding, embedding_dim,
                    created_at, updated_at
             FROM notes
             WHERE user_id = ?1 AND embedding IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![user_id], |r| {
            let blob: Vec<u8> = r.get(4)?;
            let dim: i64 = r.get(5)?;
            let dim = dim as usize;
            let mut vec = Vec::with_capacity(dim);
            for chunk in blob.chunks_exact(4) {
                vec.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            let tags_s: Option<String> = r.get(3)?;
            let tags = tags_s
                .map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect())
                .unwrap_or_default();
            let c: String = r.get(6)?;
            let u: String = r.get(7)?;
            Ok(NoteEmbedding {
                note: Note {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    body: r.get(2)?,
                    tags,
                    created_at: parse_rfc3339(&c),
                    updated_at: parse_rfc3339(&u),
                },
                embedding: vec,
            })
        })?;
        rows.collect()
    }

    pub fn count_notes(&self, user_id: &str) -> SqlResult<u32> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM notes WHERE user_id = ?1",
                params![user_id],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as u32)
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

fn row_to_note(r: &rusqlite::Row<'_>) -> SqlResult<Note> {
    let tags_s: Option<String> = r.get(3)?;
    let tags = tags_s
        .map(|s| s.split(',').filter(|x| !x.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let c: String = r.get(4)?;
    let u: String = r.get(5)?;
    Ok(Note {
        id: r.get(0)?,
        title: r.get(1)?,
        body: r.get(2)?,
        tags,
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
