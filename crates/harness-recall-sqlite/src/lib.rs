//! SQLite (FTS5) backend for [`harness_core::RecallStore`]. Faithful port of
//! Hermes Agent's recall storage: FTS5 BM25 + `snippet()`, a trigram FTS table
//! for CJK, and a LIKE fallback for short CJK queries. Owner scoping is a SQL
//! `WHERE owner = ?`, so cross-tenant leakage is structurally impossible.
//!
//! `rusqlite` runs synchronously behind a `Mutex<Connection>`; the async
//! trait methods lock and run the SQL inline (recall writes are small + fast).

use async_trait::async_trait;
use harness_core::{RecallError, RecallMessage, RecallStore, SessionHit, SessionMeta};
use rusqlite::{params, Connection};
use std::sync::Mutex;

pub struct SqliteRecall {
    conn: Mutex<Connection>,
}

impl SqliteRecall {
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, RecallError> {
        let conn = Connection::open(path).map_err(|e| RecallError::Backend(e.to_string()))?;
        Self::init(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self, RecallError> {
        let conn = Connection::open_in_memory().map_err(|e| RecallError::Backend(e.to_string()))?;
        Self::init(&conn)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn init(conn: &Connection) -> Result<(), RecallError> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS recall_sessions (
                owner         TEXT NOT NULL,
                session_id    TEXT NOT NULL,
                title         TEXT,
                source        TEXT,
                started_at    INTEGER NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (owner, session_id)
            );
            CREATE INDEX IF NOT EXISTS idx_recall_sessions_owner
                ON recall_sessions(owner, started_at DESC);

            CREATE TABLE IF NOT EXISTS recall_messages (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                owner       TEXT NOT NULL,
                session_id  TEXT NOT NULL,
                role        TEXT NOT NULL,
                content     TEXT,
                tool_name   TEXT,
                tool_calls  TEXT,
                ts_ms       INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_recall_messages_session
                ON recall_messages(owner, session_id, id);

            CREATE VIRTUAL TABLE IF NOT EXISTS recall_messages_fts USING fts5(content);
            CREATE VIRTUAL TABLE IF NOT EXISTS recall_messages_fts_trigram
                USING fts5(content, tokenize='trigram');

            CREATE TRIGGER IF NOT EXISTS recall_fts_insert AFTER INSERT ON recall_messages BEGIN
                INSERT INTO recall_messages_fts(rowid, content)
                    VALUES (new.id, COALESCE(new.content,'') || ' ' || COALESCE(new.tool_name,'') || ' ' || COALESCE(new.tool_calls,''));
                INSERT INTO recall_messages_fts_trigram(rowid, content)
                    VALUES (new.id, COALESCE(new.content,'') || ' ' || COALESCE(new.tool_name,'') || ' ' || COALESCE(new.tool_calls,''));
            END;
            CREATE TRIGGER IF NOT EXISTS recall_fts_delete AFTER DELETE ON recall_messages BEGIN
                DELETE FROM recall_messages_fts WHERE rowid = old.id;
                DELETE FROM recall_messages_fts_trigram WHERE rowid = old.id;
            END;
            "#,
        )
        .map_err(|e| RecallError::Backend(e.to_string()))?;
        Ok(())
    }
}

fn row_to_msg(r: &rusqlite::Row<'_>) -> rusqlite::Result<RecallMessage> {
    let mut m = RecallMessage::new(
        r.get::<_, String>("role")?,
        r.get::<_, Option<String>>("content")?.unwrap_or_default(),
        r.get("ts_ms")?,
    );
    m.id = r.get("id")?;
    m.tool_name = r.get("tool_name")?;
    m.tool_calls = r.get("tool_calls")?;
    Ok(m)
}

fn row_to_meta_indexed(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMeta> {
    // columns: 0 session_id, 1 title, 2 source, 3 started_at, 4 message_count
    let mut m = SessionMeta::new(r.get::<_, String>(0)?, r.get(3)?);
    m.title = r.get(1)?;
    m.source = r.get(2)?;
    m.message_count = r.get(4)?;
    Ok(m)
}

fn count_cjk(s: &str) -> usize {
    s.chars().filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c)).count()
}

impl SqliteRecall {
    fn read_window(
        conn: &Connection,
        owner: &str,
        session_id: &str,
        lo: i64,
        hi: i64,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, tool_name, tool_calls, ts_ms FROM recall_messages
                 WHERE owner=?1 AND session_id=?2 AND id BETWEEN ?3 AND ?4 ORDER BY id",
            )
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map(params![owner, session_id, lo, hi], row_to_msg)
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))
    }

    fn read_first(conn: &Connection, owner: &str, session_id: &str, n: i64) -> Result<Vec<RecallMessage>, RecallError> {
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, tool_name, tool_calls, ts_ms FROM recall_messages
                 WHERE owner=?1 AND session_id=?2 ORDER BY id ASC LIMIT ?3",
            )
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map(params![owner, session_id, n], row_to_msg)
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))
    }

    fn read_last(conn: &Connection, owner: &str, session_id: &str, n: i64) -> Result<Vec<RecallMessage>, RecallError> {
        let mut stmt = conn
            .prepare(
                "SELECT id, role, content, tool_name, tool_calls, ts_ms FROM recall_messages
                 WHERE owner=?1 AND session_id=?2 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map(params![owner, session_id, n], row_to_msg)
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        let mut v = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))?;
        v.reverse(); // back to chronological order
        Ok(v)
    }

    fn meta_of(conn: &Connection, owner: &str, session_id: &str) -> Option<SessionMeta> {
        conn.query_row(
            "SELECT session_id, title, source, started_at, message_count FROM recall_sessions WHERE owner=?1 AND session_id=?2",
            params![owner, session_id],
            row_to_meta_indexed,
        )
        .ok()
    }
}

#[async_trait]
impl RecallStore for SqliteRecall {
    async fn ensure_session(&self, owner: &str, session_id: &str, meta: &SessionMeta) -> Result<(), RecallError> {
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        conn.execute(
            "INSERT OR IGNORE INTO recall_sessions(owner, session_id, title, source, started_at, message_count)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![owner, session_id, meta.title, meta.source, meta.started_at_ms],
        )
        .map_err(|e| RecallError::Backend(e.to_string()))?;
        Ok(())
    }

    async fn append(&self, owner: &str, session_id: &str, msg: &RecallMessage) -> Result<i64, RecallError> {
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        conn.execute(
            "INSERT INTO recall_messages(owner, session_id, role, content, tool_name, tool_calls, ts_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![owner, session_id, msg.role, msg.content, msg.tool_name, msg.tool_calls, msg.ts_ms],
        )
        .map_err(|e| RecallError::Backend(e.to_string()))?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE recall_sessions SET message_count = message_count + 1 WHERE owner=?1 AND session_id=?2",
            params![owner, session_id],
        )
        .map_err(|e| RecallError::Backend(e.to_string()))?;
        Ok(id)
    }

    async fn search(&self, owner: &str, query: &str, limit: usize) -> Result<Vec<SessionHit>, RecallError> {
        if query.trim().is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        let cjk = count_cjk(query);

        // anchors: (msg_id, session_id) ranked best-first.
        let anchors: Vec<(i64, String)> = if cjk >= 3 {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.session_id FROM recall_messages_fts_trigram f
                 JOIN recall_messages m ON m.id = f.rowid
                 WHERE f.content MATCH ?1 AND m.owner = ?2
                 ORDER BY rank LIMIT ?3",
            ).map_err(|e| RecallError::Backend(e.to_string()))?;
            stmt.query_map(params![query, owner, (limit * 5) as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(|e| RecallError::Backend(e.to_string()))?
                .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))?
        } else {
            let fts: rusqlite::Result<Vec<(i64, String)>> = (|| {
                let mut stmt = conn.prepare(
                    "SELECT m.id, m.session_id FROM recall_messages_fts f
                     JOIN recall_messages m ON m.id = f.rowid
                     WHERE f.content MATCH ?1 AND m.owner = ?2
                     ORDER BY rank LIMIT ?3",
                )?;
                let v = stmt.query_map(params![query, owner, (limit * 5) as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(v)
            })();
            let v = fts.unwrap_or_default();
            if v.is_empty() {
                let like = format!("%{}%", query.trim());
                let mut stmt = conn.prepare(
                    "SELECT id, session_id FROM recall_messages
                     WHERE owner=?1 AND content LIKE ?2 ORDER BY id DESC LIMIT ?3",
                ).map_err(|e| RecallError::Backend(e.to_string()))?;
                stmt.query_map(params![owner, like, (limit * 5) as i64], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(|e| RecallError::Backend(e.to_string()))?
                    .collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))?
            } else {
                v
            }
        };

        let mut seen = std::collections::HashSet::new();
        let mut hits = Vec::new();
        for (anchor_id, session_id) in anchors {
            if !seen.insert(session_id.clone()) {
                continue;
            }
            let Some(meta) = Self::meta_of(&conn, owner, &session_id) else { continue };
            let around = Self::read_window(&conn, owner, &session_id, anchor_id - 5, anchor_id + 5)?;
            let bookend_start = Self::read_first(&conn, owner, &session_id, 3)?;
            let bookend_end = Self::read_last(&conn, owner, &session_id, 3)?;
            let snippet: String = conn
                .query_row(
                    "SELECT snippet(recall_messages_fts, 0, '>>>', '<<<', '…', 12)
                     FROM recall_messages_fts WHERE recall_messages_fts MATCH ?1 AND rowid = ?2",
                    params![query, anchor_id],
                    |r| r.get(0),
                )
                .unwrap_or_else(|_| {
                    around.iter().find(|m| m.id == anchor_id).map(|m| m.content.chars().take(80).collect()).unwrap_or_default()
                });
            hits.push(SessionHit::new(meta, snippet, anchor_id, bookend_start, around, bookend_end));
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    async fn scroll(&self, owner: &str, session_id: &str, around: i64, window: usize) -> Result<Vec<RecallMessage>, RecallError> {
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        let w = window as i64;
        Self::read_window(&conn, owner, session_id, around - w, around + w)
    }

    async fn recent(&self, owner: &str, limit: usize) -> Result<Vec<SessionMeta>, RecallError> {
        let conn = self.conn.lock().map_err(|e| RecallError::Backend(e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, title, source, started_at, message_count FROM recall_sessions
                 WHERE owner=?1 ORDER BY started_at DESC LIMIT ?2",
            )
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        let rows = stmt
            .query_map(params![owner, limit as i64], row_to_meta_indexed)
            .map_err(|e| RecallError::Backend(e.to_string()))?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e| RecallError::Backend(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fts_english_and_cjk_search() {
        let r = SqliteRecall::open_in_memory().unwrap();
        r.ensure_session("u1", "s1", &SessionMeta::new("s1", 1)).await.unwrap();
        r.append("u1", "s1", &RecallMessage::new("user", "deploy the payment service tomorrow", 1)).await.unwrap();
        r.append("u1", "s1", &RecallMessage::new("user", "我们明天要上线支付服务", 2)).await.unwrap();

        let en = r.search("u1", "payment deploy", 5).await.unwrap();
        assert_eq!(en.len(), 1);
        assert!(en[0].snippet.contains(">>>"));

        let zh = r.search("u1", "支付服务", 5).await.unwrap();
        assert_eq!(zh.len(), 1, "trigram CJK search should hit");
    }

    #[tokio::test]
    async fn bookends_correct_for_non_first_session() {
        let r = SqliteRecall::open_in_memory().unwrap();
        // Session A: 3 messages → global ids 1,2,3
        r.ensure_session("u1", "a", &SessionMeta::new("a", 1)).await.unwrap();
        for i in 0..3 {
            r.append("u1", "a", &RecallMessage::new("user", format!("a-msg-{i}"), i)).await.unwrap();
        }
        // Session B: 4 messages → global ids 4,5,6,7 (NOT 1-based)
        r.ensure_session("u1", "b", &SessionMeta::new("b", 10)).await.unwrap();
        r.append("u1", "b", &RecallMessage::new("user", "find the kraken deployment runbook", 10)).await.unwrap();
        r.append("u1", "b", &RecallMessage::new("assistant", "second b", 11)).await.unwrap();
        r.append("u1", "b", &RecallMessage::new("assistant", "third b", 12)).await.unwrap();
        r.append("u1", "b", &RecallMessage::new("assistant", "fourth b kraken", 13)).await.unwrap();

        let hits = r.search("u1", "kraken", 5).await.unwrap();
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.session.session_id, "b");
        // bookends must be non-empty and contain ONLY session b's messages
        assert!(!h.bookend_start.is_empty(), "bookend_start empty — the global-id bug");
        assert!(!h.bookend_end.is_empty(), "bookend_end empty — the global-id bug");
        assert!(
            h.bookend_start.iter().any(|m| m.content == "find the kraken deployment runbook"),
            "bookend_start must contain the first message of session b"
        );
        assert!(
            h.bookend_end.iter().any(|m| m.content == "fourth b kraken"),
            "bookend_end must contain the last message of session b"
        );
    }
}
