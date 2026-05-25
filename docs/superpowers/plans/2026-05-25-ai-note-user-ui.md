# ai-note user-ui (ledger parity + 工作/生活) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give ai-note a modern user SPA matching ai-ledger's `user-ui` (Vite + React + shadcn/Radix + Tailwind v4 + i18n + streaming chat with persisted sessions + chat FAB), and add a 工作/生活 (work/life) `space` dimension that scopes notes, search, chat, and the agent.

**Architecture:** Copy `ai-ledger/user-ui` wholesale, swap the domain pages, embed the built `dist` at `/`. Backend gains a `space` column on `notes` + `chat_sessions`, chat session CRUD + SSE streaming (ported from ledger, minus memory/attachments), a per-request model picker, and space-aware tools. `space` is ambient context planted on `profile.extra` and injected into the agent task description.

**Tech Stack:** Rust (axum 0.7, rusqlite, harness-rs), React 19 + Vite + shadcn/ui + Radix + Tailwind v4 + react-i18next, SSE over `fetch`.

**Reference sources (read these — you'll port from them):**
- Backend chat/SSE: `examples/ai-ledger/src/server.rs` — `ChannelHook` (1487-1544), session handlers (1625-1684), `session_stream_handler` (1714-1946).
- Frontend: the entire `examples/ai-ledger/user-ui/` tree.
- SSE event contract (the reused `stream.ts` parses these): `{"type":"start"}`, `{"type":"iter","iter":N}`, `{"type":"token","text"}`, `{"type":"thought","text"}`, `{"type":"tool_start","name","args"}`, `{"type":"tool_end","name","ok","preview"}`, `{"type":"error","message"}`, `{"type":"done","ok","iters","reply","warning"?}`.

**Working dir for all paths below:** `examples/ai-note/`.

---

## Task 1: db.rs — `space` column on notes + `ensure_column` + space-filtered queries

**Files:**
- Modify: `examples/ai-note/src/db.rs`

- [ ] **Step 1: Add the `ensure_column` migration helper**

In `impl Db`, right after `fn init`, add:

```rust
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
```

- [ ] **Step 2: Call the migrations at the end of `init`**

In `fn init`, just before the final `Ok(())`, add:

```rust
        // ── idempotent migrations (existing DBs) ──
        self.ensure_column("notes", "space", "TEXT NOT NULL DEFAULT 'life'")?;
        self.ensure_column("chat_sessions", "space", "TEXT NOT NULL DEFAULT 'life'")?;
```

Also add `space TEXT NOT NULL DEFAULT 'life'` to the `CREATE TABLE IF NOT EXISTS notes (...)` and `chat_sessions (...)` blocks (after the existing columns) so fresh DBs get it directly.

- [ ] **Step 3: Add `space` to the `Note` struct**

Change the `Note` struct to add the field (place after `tags`):

```rust
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub space: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

- [ ] **Step 4: Thread `space` through `create_note` and all SELECT paths**

Replace `create_note` to accept and store `space`:

```rust
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
```

Update `row_to_note` (bottom of file) to read `space`. The SELECTs below add `space` as the last selected column, so `row_to_note` reads index 6 (and shifts created/updated to 7/8). Rewrite `row_to_note`:

```rust
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
```

Now update every SELECT that feeds `row_to_note` to select `space` in position 4 (between `tags` and `created_at`). In `get_note`, `list_recent_notes`, `list_notes_in_range`, change the column list from:
`SELECT id, title, body, tags, created_at, updated_at`
to:
`SELECT id, title, body, tags, space, created_at, updated_at`

- [ ] **Step 5: Add `space` filter to `list_recent_notes`, `list_notes_in_range`, `count_notes`, `list_embeddings`**

Change `list_recent_notes` signature to `(&self, user_id: &str, space: Option<&str>, limit: u32)`:

```rust
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
```

Change `list_notes_in_range` signature to `(&self, user_id: &str, space: Option<&str>, since: Option<&str>, until: Option<&str>, limit: u32)` (space is the 2nd param — its only caller in `tools.rs` passes it there). Insert the filter right after the `user_id = ?1` clause, before the `since`/`until` ones (the existing clauses use `p.len() + 1` so indices adapt automatically). Concretely, after `let mut p: Vec<String> = vec![user_id.to_string()];` add:

```rust
        if let Some(sp) = space {
            sql.push_str(&format!(" AND space = ?{}", p.len() + 1));
            p.push(sp.to_string());
        }
```

Add `space: Option<&str>` to `count_notes`:

```rust
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
```

Add `space: Option<&str>` to `list_embeddings` — add the same `WHERE … AND space = ?` filter. Its SELECT already lists `tags` but builds `NoteEmbedding` inline (not via `row_to_note`), so also add `space` to its column list and set it on the inline `Note { … }`. Change the SELECT to `SELECT id, title, body, tags, space, embedding, embedding_dim, created_at, updated_at`, shift the blob/dim indices to 5/6 and created/updated to 7/8, read `space` at index 4, and add `space` to the inline `Note`.

- [ ] **Step 6: Write unit tests for space filtering**

Append to `db.rs`:

```rust
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
}
```

- [ ] **Step 7: Run tests**

Run: `cargo test -p ai-note db::tests`
Expected: PASS (this task only compiles once Task 2's callers are fixed; if `cargo test` fails to compile due to *other* call sites of the changed signatures, that's expected — fix them in Steps below by updating callers in `search.rs`, `server.rs`, `tools.rs` which happen in Tasks 3 & 5. To validate Task 1 in isolation, temporarily run `cargo test -p ai-note --lib db::tests::notes_are_space_scoped 2>&1 | head`; if call-site errors block compilation, proceed to Task 3/5 and run the suite at the end of Task 5.)

- [ ] **Step 8: Commit**

```bash
git add examples/ai-note/src/db.rs
git commit -m "feat(ai-note): add space column + space-filtered note queries"
```

---

## Task 2: db.rs — chat session/message helpers + structs

**Files:**
- Modify: `examples/ai-note/src/db.rs`

- [ ] **Step 1: Add `ChatSession` / `ChatMessage` structs**

Near the other structs (e.g. after `Note`), add:

```rust
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
```

- [ ] **Step 2: Add the session helpers in `impl Db`**

```rust
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
```

- [ ] **Step 3: Add the `row_to_session` free function**

Near `row_to_note`:

```rust
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
```

- [ ] **Step 4: Add a test**

Add to the `tests` mod:

```rust
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
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p ai-note db::tests::chat_sessions_scoped_and_counted`
Expected: PASS (subject to the same cross-task compile caveat as Task 1 Step 7).

- [ ] **Step 6: Commit**

```bash
git add examples/ai-note/src/db.rs
git commit -m "feat(ai-note): chat session/message db helpers (per-space)"
```

---

## Task 3: search.rs + tools.rs — space-aware agent

**Files:**
- Modify: `examples/ai-note/src/search.rs`, `examples/ai-note/src/tools.rs`, `examples/ai-note/src/server.rs` (SYSTEM_PROMPT + search_handler caller)

- [ ] **Step 1: Thread `space` through `semantic_search`**

In `search.rs`, change the signature to add `space: Option<&str>`:

```rust
pub async fn semantic_search(
    db_path: &Path,
    user_id: &str,
    embedder: &Arc<dyn Embedder>,
    query: &str,
    top_k: usize,
    space: Option<&str>,
) -> anyhow::Result<Vec<Hit>> {
```

Update the two db calls inside:
- `db.list_embeddings(user_id)?` → `db.list_embeddings(user_id, space)?`
- `db.list_recent_notes(user_id, 5000)?` → `db.list_recent_notes(user_id, space, 5000)?`

- [ ] **Step 2: Add a `space_of` helper + thread it through tools**

In `tools.rs`, after `tier_of`, add:

```rust
fn space_of(w: &World) -> String {
    w.profile
        .extra::<String>("space")
        .filter(|s| s == "work" || s == "life")
        .unwrap_or_else(|| "life".into())
}
```

In `create_note`: change the trial cap count to `db.count_notes(&uid, Some(&space_of(w)))` and the insert to pass space. Concretely, before the `let db = open_db(w)?;` line add `let space = space_of(w);`, then change `db.count_notes(&uid)` → `db.count_notes(&uid, Some(&space))` and `db.create_note(&uid, title, body, &tags)` → `db.create_note(&uid, title, body, &tags, &space)`.

In `search_notes`: pass space. Change the call to:
`crate::search::semantic_search(&path, &uid, &emb, q, top_k, Some(&space_of(w)))`.

In `list_recent_notes`: pass space. Change both branches:
- `db.list_notes_in_range(&uid, since, until, limit)` → `db.list_notes_in_range(&uid, Some(&space_of(w)), since, until, limit)`
- `db.list_recent_notes(&uid, limit)` → `db.list_recent_notes(&uid, Some(&space_of(w)), limit)`

(Compute `let sp = space_of(w);` once at the top of `list_recent_notes` and reuse, to avoid borrow churn.)

- [ ] **Step 3: Add the space rule to SYSTEM_PROMPT**

In `server.rs`, append to the `SYSTEM_PROMPT` string a new rule (before the closing quote):

```
10. **Space scope.** Every note operation is scoped to the user's current \
   space, given on a `[system] space: work|life` line at the top of the task. \
   New notes go in that space; searches and listings only see that space. \
   Never move a note across spaces unless the user explicitly asks.\n\
```

- [ ] **Step 4: Fix the `search_handler` caller in server.rs**

In `search_handler`, change the `semantic_search(...)` call to pass the requested space (added in Task 5; for now pass `None` to compile, Task 5 wires `?space=`). To avoid a temporary, add `None` as the final arg here and replace it in Task 5.

```rust
    let hits = crate::search::semantic_search(&s.db_path, &auth.user.id, &s.embedder, &qs.q, top_k, None)
```

- [ ] **Step 5: Fix the one-shot `chat_handler` to plant a default space**

In `chat_handler` (server.rs), after planting `tier`, add a default-space plant so the one-shot path keeps working:

```rust
    profile.extra.insert("space".into(), serde_json::Value::String("life".into()));
```

- [ ] **Step 6: Build**

Run: `cargo build -p ai-note 2>&1 | tail -20`
Expected: compiles (export handlers still call `list_recent_notes(user_id, None, …)` — fix those call sites in `export_all_zip_handler` and `list_notes_handler` by inserting `None` for the new `space` param; the compiler will point them out).

- [ ] **Step 7: Run the full db test suite**

Run: `cargo test -p ai-note`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add examples/ai-note/src/search.rs examples/ai-note/src/tools.rs examples/ai-note/src/server.rs
git commit -m "feat(ai-note): space-aware tools + search + system prompt"
```

---

## Task 4: server.rs — model picker (build_model_for + /api/me/model)

**Files:**
- Modify: `examples/ai-note/src/server.rs`, `examples/ai-note/src/main.rs`

- [ ] **Step 1: Add the model allowlist + builder on AppState**

In `server.rs`, add near the top:

```rust
/// Chat models a paid/admin user may pick. id → (provider, model).
pub const ALLOWED_MODELS: &[(&str, &str)] = &[
    ("deepseek-v4-flash", "openai-compat"),
    ("deepseek-v4-pro", "openai-compat"),
    ("gemini-3.5-flash", "gemini"),
];

pub fn is_allowed_model(id: &str) -> bool {
    ALLOWED_MODELS.iter().any(|(m, _)| *m == id)
}
```

In `impl AppState`, add:

```rust
    /// Build a fresh chat model for a given allowlisted id, using keys from
    /// the hot config. Used per chat request so users can switch models.
    pub fn build_model_for(&self, model_id: &str) -> anyhow::Result<Arc<dyn Model>> {
        let cfg = self.cfg();
        let provider = ALLOWED_MODELS
            .iter()
            .find(|(m, _)| *m == model_id)
            .map(|(_, p)| *p)
            .ok_or_else(|| anyhow::anyhow!("model `{model_id}` not allowed"))?;
        match provider {
            "gemini" => {
                let key = cfg.gemini_key.clone()
                    .ok_or_else(|| anyhow::anyhow!("no gemini key configured"))?;
                Ok(Arc::new(harness_models::GeminiNative::with_key(model_id, key)))
            }
            _ => {
                let key = cfg.deepseek_key.clone()
                    .ok_or_else(|| anyhow::anyhow!("no deepseek key configured"))?;
                Ok(Arc::new(harness_models::OpenAiCompat::with_key(
                    harness_models::providers::DEEPSEEK.to_string(),
                    model_id,
                    key,
                )))
            }
        }
    }

    /// The model id this user should chat with: their preferred_model if it's
    /// allowlisted, else the configured default.
    pub fn effective_model_for(&self, user: &crate::auth::User) -> String {
        match &user.preferred_model {
            Some(m) if is_allowed_model(m) => m.clone(),
            _ => self.cfg().chat_model.clone(),
        }
    }
```

Add `use std::sync::Arc;` and `use harness_core::Model;` if not already imported at the top of `server.rs` (check existing imports first).

- [ ] **Step 2: Add `update_user_model` to db.rs**

In `db.rs` `impl Db`:

```rust
    pub fn update_user_model(&self, user_id: &str, model: Option<&str>) -> SqlResult<u32> {
        Ok(self.conn.execute(
            "UPDATE users SET preferred_model = ?2 WHERE id = ?1",
            params![user_id, model],
        )? as u32)
    }
```

- [ ] **Step 3: Add the `/api/me/model` handler + route**

Handler in `server.rs`:

```rust
#[derive(Deserialize)]
struct SetModelReq {
    model: Option<String>,
}

async fn set_model_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SetModelReq>,
) -> Result<Json<Value>, ApiError> {
    if auth.user.tier == "trial" {
        return Err(ApiError::Forbidden(
            "trial 用户不能切换模型 — 升级到 paid 后可选".into(),
        ));
    }
    if let Some(m) = &req.model {
        if !is_allowed_model(m) {
            return Err(ApiError::BadRequest(format!("model `{m}` not allowed")));
        }
    }
    let db = open_db_state(&s)?;
    db.update_user_model(&auth.user.id, req.model.as_deref())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "ok": true, "model": req.model })))
}
```

Add the route in `serve()` next to the other `/api/me/*` routes:

```rust
        .route("/api/me/password", post(change_password_handler))
        .route("/api/me/model", post(set_model_handler))
```

- [ ] **Step 4: Expose allowed models on `/api/info` (for the picker UI)**

In `info_handler`, add to the JSON:

```rust
        "allowed_models": ALLOWED_MODELS.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
```

- [ ] **Step 5: Build**

Run: `cargo build -p ai-note 2>&1 | tail -20`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add examples/ai-note/src/server.rs examples/ai-note/src/db.rs
git commit -m "feat(ai-note): per-user model picker (deepseek flash/pro, gemini flash)"
```

---

## Task 5: server.rs — chat session CRUD + SSE streaming + ?space on notes

**Files:**
- Modify: `examples/ai-note/src/server.rs`

- [ ] **Step 1: Add the SSE + channel imports**

At the top of `server.rs`, extend the axum `use` block and add the stream imports (mirror ledger):

```rust
use axum::response::{Html, Sse, sse::Event as SseEvent, sse::KeepAlive};
use std::convert::Infallible;
use tokio::sync::mpsc;
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};
use futures::Stream;
use harness_core::{Event, Hook, HookOutcome, World as CoreWorld};
```

(Adjust to merge with the existing `use axum::{...}` block — keep `Json, Router, extract::*, http::StatusCode, routing::*`.)

- [ ] **Step 2: Port `ChannelHook`**

Copy `ChannelHook` (struct + `impl Hook`) verbatim from `ai-ledger/src/server.rs:1487-1544`. It needs `Event`, `Hook`, `HookOutcome`, `CoreWorld` (aliased above) and `mpsc::UnboundedSender<Value>`. No changes required — the event shapes already match `stream.ts`.

- [ ] **Step 3: Add `?space=` to note list + search handlers**

Change `ListQuery` and `SearchQuery` to carry space:

```rust
#[derive(Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    space: Option<String>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<u32>,
    space: Option<String>,
}
```

In `list_notes_handler`, change `db.list_recent_notes(&auth.user.id, q.limit.unwrap_or(50).min(500))` to pass the space filter:

```rust
    let notes = db
        .list_recent_notes(&auth.user.id, q.space.as_deref(), q.limit.unwrap_or(50).min(500))
```

In `search_handler`, replace the `None` from Task 3 Step 4 with `qs.space.as_deref()`.

In `create_note_handler`, add a `space` field to `CreateNoteReq` (`#[serde(default = "default_space")] space: String` with `fn default_space() -> String { "life".into() }`), validate it's `work`/`life`, and pass it to `db.create_note(&auth.user.id, &req.title, &req.body, &req.tags, &req.space)`. Also change the trial `count_notes(&auth.user.id)` to `count_notes(&auth.user.id, Some(&req.space))`.

- [ ] **Step 4: Add session CRUD handlers + `random_session_id`**

Port from ledger (`ai-ledger/src/server.rs:1625-1683`), adapting to ai-note's `open_db_state(&s)` pattern and adding `space`:

```rust
fn random_session_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 6];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[derive(Deserialize)]
struct CreateSessionReq {
    #[serde(default = "default_space")]
    space: String,
}

async fn create_chat_session_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateSessionReq>,
) -> Result<Json<Value>, ApiError> {
    let id = random_session_id();
    let model = s.effective_model_for(&auth.user);
    let db = open_db_state(&s)?;
    db.create_chat_session(&auth.user.id, &id, Some(&model), &req.space)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let sess = db
        .get_chat_session(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::Internal("session vanished".into()))?;
    Ok(Json(json!({ "session": sess })))
}

#[derive(Deserialize)]
struct SessionsQuery {
    #[serde(default = "default_space")]
    space: String,
}

async fn list_chat_sessions_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<SessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let sessions = db
        .list_chat_sessions(&auth.user.id, &q.space)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "count": sessions.len(), "sessions": sessions })))
}

async fn get_chat_session_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let session = db
        .get_chat_session(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest(format!("no session `{id}`")))?;
    let messages = db
        .get_chat_messages(&auth.user.id, &id, 500)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "session": session, "messages": messages })))
}

async fn delete_chat_session_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let n = db
        .delete_chat_session(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest(format!("no session `{id}`")));
    }
    Ok(Json(json!({ "deleted": id })))
}
```

Add `fn default_space() -> String { "life".into() }` once (module-level), used by both `CreateNoteReq` and the session requests.

- [ ] **Step 5: Add `space` to `build_task_description`**

Change the signature to inject the space line:

```rust
fn build_task_description(message: &str, history: &[ChatTurn], space: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("[system] space: {space}\n\n"));
    if !history.is_empty() {
        s.push_str("--- conversation so far ---\n");
        for t in history.iter().take(20) {
            s.push_str(&format!("[{}] {}\n", t.role, t.text));
        }
        s.push_str("\n--- new message ---\n");
    }
    s.push_str(message);
    s
}
```

Update the existing one-shot `chat_handler` caller to `build_task_description(&req.message, &req.history, "life")` (one-shot has no space context). The `ChatTurn` history there is `&[ChatTurn]`; the streaming handler builds its own `Vec<ChatTurn>` from DB rows — see next step.

- [ ] **Step 6: Add `session_stream_handler` (SSE)**

Port from `ai-ledger/src/server.rs:1714-1946`, **removing** the memory/synthesizer block (lines ~1795-1849) and the attachment handling, adapting model build + history. Full target:

```rust
#[derive(Deserialize)]
struct SessionStreamReq {
    message: String,
    #[serde(default)]
    lang: Option<String>,
}

async fn session_stream_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(session_id): Path<String>,
    Json(req): Json<SessionStreamReq>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    if req.message.trim().is_empty() {
        return Err(ApiError::BadRequest("message must not be empty".into()));
    }
    let (tx, rx) = mpsc::unbounded_channel::<Value>();

    let db = open_db_state(&s)?;
    let session = db
        .get_chat_session(&auth.user.id, &session_id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest(format!("no session `{session_id}`")))?;
    let space = session.space.clone();

    db.append_chat_message(&auth.user.id, &session_id, "user", &req.message, None)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let history_msgs = db
        .get_chat_messages(&auth.user.id, &session_id, 80)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let history: Vec<ChatTurn> = history_msgs
        .iter()
        .filter(|m| !(m.role == "user" && m.text == req.message))
        .map(|m| ChatTurn { role: m.role.clone(), text: m.text.clone() })
        .collect();
    let mut task_desc = build_task_description(&req.message, &history, &space);
    if let Some(lang) = req.lang.as_deref() {
        task_desc = format!("[system] reply_language: {lang}\n\n{task_desc}");
    }
    drop(db);

    let user_id = auth.user.id.clone();
    let user_tier = auth.user.tier.clone();
    let model_id = s.effective_model_for(&auth.user);
    let tx_done = tx.clone();
    let sid = session_id.clone();
    let uid = user_id.clone();
    let mid = model_id.clone();
    let space_for_task = space.clone();
    let db_path = s.db_path.to_string_lossy().into_owned();
    let user_tz = s.user_tz.clone();
    let max_iters = s.max_iters;

    tokio::spawn(async move {
        let model = match s.build_model_for(&mid) {
            Ok(m) => m,
            Err(e) => {
                let _ = tx_done.send(json!({"type":"error","message": e.to_string()}));
                let _ = tx_done.send(json!({"type":"done","ok":false,"iters":0,"reply":""}));
                return;
            }
        };
        let mut profile = harness_core::UserProfile::default();
        profile.extra.insert("user_id".into(), Value::String(uid.clone()));
        profile.extra.insert("db_path".into(), Value::String(db_path));
        profile.extra.insert("tier".into(), Value::String(user_tier));
        profile.extra.insert("space".into(), Value::String(space_for_task));
        profile.extra.insert("__embedder_slot".into(), Value::Bool(true));
        if let Some(tz) = user_tz {
            profile.tz = Some(tz);
        }
        let mut world = harness_context::with_profile(".", profile);

        let mut loop_ = AgentLoop::new(crate::AnyModelHandle(model))
            .with_streaming(true)
            .with_guide(Arc::new(SystemPromptGuide));
        for t in harness_core::iter_macro_tools() {
            loop_ = loop_.with_tool(t);
        }
        loop_ = loop_.with_hook(Arc::new(ChannelHook { tx: tx.clone() }));

        let task = Task { description: task_desc, source: None, deadline: None };
        let _ = tx_done.send(json!({"type":"start"}));
        match loop_.run_with_max_iters(task, &mut world, max_iters).await {
            Ok(Outcome::Done { text, iters, usage, .. }) => {
                let reply = text.unwrap_or_default();
                if let Ok(db) = open_db_state(&s) {
                    let _ = db.append_chat_message(&uid, &sid, "asst", &reply, Some(iters));
                    let _ = db.update_chat_session_model(&uid, &sid, &mid);
                    let _ = db.insert_audit(
                        Some(&uid), "chat_message", Some(&sid),
                        Some(&json!({"iters": iters, "model": &mid}).to_string()),
                        usage.input_tokens as i64, usage.output_tokens as i64,
                    );
                }
                let _ = tx_done.send(json!({"type":"done","ok":true,"iters":iters,"reply":reply}));
            }
            Ok(Outcome::BudgetExhausted { iters, last_text, usage, .. }) => {
                let reply = last_text.unwrap_or_else(|| "(budget exhausted)".into());
                if let Ok(db) = open_db_state(&s) {
                    let _ = db.append_chat_message(&uid, &sid, "asst", &reply, Some(iters));
                    let _ = db.insert_audit(
                        Some(&uid), "chat_message", Some(&sid),
                        Some(&json!({"iters": iters, "warning":"budget_exhausted"}).to_string()),
                        usage.input_tokens as i64, usage.output_tokens as i64,
                    );
                }
                let _ = tx_done.send(json!({"type":"done","ok":false,"iters":iters,"reply":reply,"warning":"budget_exhausted"}));
            }
            Err(e) => {
                let _ = tx_done.send(json!({"type":"error","message": format!("agent: {e}")}));
                let _ = tx_done.send(json!({"type":"done","ok":false,"iters":0,"reply":""}));
            }
        }
    });

    let stream = UnboundedReceiverStream::new(rx)
        .map(|v| Ok::<_, Infallible>(SseEvent::default().data(v.to_string())));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
```

Note: this uses `AgentLoop`, `Outcome`, `Task`, `SystemPromptGuide`, `harness_context::with_profile` — all already imported/defined in `server.rs`. `crate::AnyModelHandle` wraps the `Arc<dyn Model>` so `.stream()` routes correctly.

- [ ] **Step 7: Register the chat routes**

In `serve()`, after the existing `/api/chat` route, add:

```rust
        .route("/api/chat/sessions", get(list_chat_sessions_handler).post(create_chat_session_handler))
        .route("/api/chat/sessions/:id", get(get_chat_session_handler).delete(delete_chat_session_handler))
        .route("/api/chat/sessions/:id/stream", post(session_stream_handler))
```

(Import `axum::routing::delete` is not needed — `.delete()` is a method on the `get(...)` builder.)

- [ ] **Step 8: Build**

Run: `cargo build -p ai-note 2>&1 | tail -30`
Expected: compiles. Fix any leftover `list_recent_notes` / `count_notes` call sites the compiler flags (pass `None` for export-all).

- [ ] **Step 9: Smoke test the stream locally**

Run (port 6755 default):
```bash
DEEPSEEK_API_KEY=$DEEPSEEK_API_KEY GEMINI_API_KEY=$GEMINI_API_KEY \
  cargo run -p ai-note -- --port 6755 &
# register, create a session, hit the stream — or just confirm it boots:
sleep 3 && curl -s localhost:6755/api/info | head; kill %1
```
Expected: `/api/info` returns JSON incl. `allowed_models`.

- [ ] **Step 10: Commit**

```bash
git add examples/ai-note/src/server.rs
git commit -m "feat(ai-note): chat session CRUD + SSE streaming + ?space on notes"
```

---

## Task 6: Scaffold user-ui (copy ledger, strip attachments, re-point api)

**Files:**
- Create: `examples/ai-note/user-ui/` (copied tree)

- [ ] **Step 1: Copy the ledger user-ui tree (excluding build/deps)**

```bash
cd examples/ai-note
rsync -a --exclude node_modules --exclude dist \
  ../ai-ledger/user-ui/ user-ui/
```

- [ ] **Step 2: Rename the package + strip attachment files**

Edit `user-ui/package.json`: change `"name"` to `"ai-note-user-ui"`.
Delete files ai-note won't use:

```bash
cd examples/ai-note/user-ui
rm -f src/components/chat/attachment-button.tsx
rm -f src/components/portfolio/allocation-pie.tsx src/components/portfolio/positions-list.tsx src/components/portfolio/trades-list.tsx
rm -f src/components/transactions/txn-filters.tsx src/components/transactions/txn-list.tsx
rm -f src/components/budgets/budgets-list.tsx src/components/subscriptions/subs-list.tsx
rm -f src/components/loans/loans-list.tsx src/components/currency-picker.tsx
rm -f src/components/memory/memory-list.tsx src/components/memory/memory-sheet.tsx
rm -f src/components/profile/account-card.tsx src/components/profile/model-picker.tsx src/components/profile/password-form.tsx
rm -f src/pages/Dashboard.tsx src/pages/Ledger.tsx src/pages/Portfolio.tsx src/pages/Profile.tsx
rmdir src/components/portfolio src/components/transactions src/components/budgets \
  src/components/subscriptions src/components/loans src/components/memory 2>/dev/null || true
```

(Keep `src/components/profile/` — we'll re-add note-specific files in Task 9. Keep `chat/`, `ui/`, `lib/`, `pages/{Marketing,Login}.tsx`, `app-shell.tsx`.)

- [ ] **Step 3: Install deps**

```bash
cd examples/ai-note/user-ui && npm install
```

Expected: completes (lockfile copied from ledger; same deps).

- [ ] **Step 4: Replace `src/lib/api.ts` with the ai-note surface**

Overwrite `user-ui/src/lib/api.ts` with note-domain types + helpers. Keep the auth/token plumbing identical to ledger (`getToken`/`setToken`/`ledger-user-token` localStorage key — rename to `ai-note-token`). Full file:

```ts
const TOKEN_KEY = 'ai-note-token';
export function getToken(): string | null { return localStorage.getItem(TOKEN_KEY); }
export function setToken(t: string | null) {
  if (t) localStorage.setItem(TOKEN_KEY, t); else localStorage.removeItem(TOKEN_KEY);
}

export type Space = 'work' | 'life';

export interface Note {
  id: string; title: string; body: string; tags: string[];
  space: Space; created_at: string; updated_at: string;
}
export interface SearchHit extends Note { score: number; via_grep: boolean; }
export interface ChatSession {
  id: string; title: string; space: Space; model_id?: string;
  message_count: number; created_at: string; updated_at: string;
}
export interface ChatMessage {
  id: string; session_id: string; role: string; text: string;
  created_at: string; truncated?: boolean;
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(path, {
    ...init,
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${getToken() ?? ''}`,
      ...(init?.headers ?? {}),
    },
  });
  if (!resp.ok) {
    let msg = `HTTP ${resp.status}`;
    try { const j = await resp.json(); msg = j.error || j.message || msg; } catch { /* */ }
    throw new Error(msg);
  }
  return resp.json() as Promise<T>;
}

export const noteApi = {
  login: (email: string, password: string) =>
    req<{ token: string; user: any }>('/api/login', { method: 'POST', body: JSON.stringify({ email, password }) }),
  register: (email: string, password: string, invite_code?: string) =>
    req<{ token: string; user: any }>('/api/register', { method: 'POST', body: JSON.stringify({ email, password, invite_code }) }),
  me: () => req<{ user: any }>('/api/me'),
  info: () => req<{ model: string; allowed_models: string[] }>('/api/info'),
  changePassword: (old_password: string, new_password: string) =>
    req<{ ok: boolean }>('/api/me/password', { method: 'POST', body: JSON.stringify({ old_password, new_password }) }),
  setModel: (model: string | null) =>
    req<{ ok: boolean; model: string | null }>('/api/me/model', { method: 'POST', body: JSON.stringify({ model }) }),

  notes: (space: Space) => req<{ notes: Note[] }>(`/api/notes?space=${space}&limit=200`),
  createNote: (space: Space, title: string, body: string, tags: string[]) =>
    req<{ note: Note }>('/api/notes', { method: 'POST', body: JSON.stringify({ space, title, body, tags }) }),
  updateNote: (id: string, patch: Partial<Pick<Note, 'title' | 'body' | 'tags'>>) =>
    req<{ ok: boolean }>(`/api/notes/${id}`, { method: 'PATCH', body: JSON.stringify(patch) }),
  deleteNote: (id: string) => req<{ ok: boolean }>(`/api/notes/${id}`, { method: 'DELETE' }),
  search: (space: Space, q: string) =>
    req<{ hits: SearchHit[] }>(`/api/notes/search?space=${space}&q=${encodeURIComponent(q)}&limit=20`),

  chatSessions: (space: Space) => req<{ sessions: ChatSession[] }>(`/api/chat/sessions?space=${space}`),
  createChatSession: (space: Space) =>
    req<{ session: ChatSession }>('/api/chat/sessions', { method: 'POST', body: JSON.stringify({ space }) }),
  getChatSession: (id: string) =>
    req<{ session: ChatSession; messages: ChatMessage[] }>(`/api/chat/sessions/${id}`),
  deleteChatSession: (id: string) =>
    req<{ deleted: string }>(`/api/chat/sessions/${id}`, { method: 'DELETE' }),
};
```

- [ ] **Step 5: Fix `stream.ts` token key + drop attachments arg**

Edit `user-ui/src/components/chat/stream.ts`: change `import { getToken } from '@/lib/api'` (still valid). Remove the `attachment_ids` parameter from `streamSession` and from the POST body (ai-note has no attachments). Keep everything else identical.

- [ ] **Step 6: Get the build green with placeholder pages**

The copied `App.tsx`, `app-shell.tsx`, `pages/Marketing.tsx`, `pages/Login.tsx`, `components/chat/*` still import `ledgerApi` and deleted files. To compile now, do a global rename and stub the routes:
- In every `user-ui/src/**/*.tsx|ts`, replace `ledgerApi` → `noteApi` and `from '@/lib/api'` import lists to only pull names that exist. (The chat components import `ledgerApi`, `ChatSession`, `ChatMessage`, `fetchAttachmentBlob` — drop `fetchAttachmentBlob` usages here; they're addressed in Task 10.)
- Temporarily simplify `App.tsx` to route `/` → Login, `/app` → a `<div>ok</div>` placeholder so the build passes. (Real routing lands in Task 8.)

Run: `cd examples/ai-note/user-ui && npm run build 2>&1 | tail -30`
Expected: build succeeds, emitting `dist/`. Fix remaining type errors by stubbing/removing ledger-only imports until green. **The goal of this task is only a green build + a `dist/` for `include_dir!` — pages get real content in Tasks 8-10.**

- [ ] **Step 7: Commit**

```bash
git add examples/ai-note/user-ui
git commit -m "chore(ai-note): scaffold user-ui from ledger (stripped, api re-pointed)"
```

---

## Task 7: server.rs — serve user-ui dist at / + SPA fallback + retire legacy

**Files:**
- Modify: `examples/ai-note/src/server.rs`

- [ ] **Step 1: Embed the user-ui dist**

Near the `ADMIN_DIST` static, add:

```rust
static USER_DIST: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/user-ui/dist");
```

- [ ] **Step 2: Replace the `/` + legacy serving with the SPA**

Remove `const INDEX_HTML` + `const MARKED_JS` and `serve_index` / `serve_marked_js`. Replace the `/` and `/marked.min.js` routes. Add a user-SPA index server + asset server mirroring the admin ones:

```rust
async fn serve_user_index() -> impl axum::response::IntoResponse {
    use axum::http::header;
    let body = USER_DIST
        .get_file("index.html")
        .and_then(|f| f.contents_utf8())
        .unwrap_or("<h1>user UI not built</h1>");
    ([(header::CACHE_CONTROL, "no-cache, must-revalidate")], Html(body))
}

async fn serve_user_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::header;
    use axum::response::IntoResponse;
    if let Some(file) = USER_DIST.get_file(&path) {
        let mime = mime_for(&path);
        return (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, if path.starts_with("assets/") {
                    "public, max-age=31536000, immutable"
                } else { "no-cache" }),
            ],
            Body::from(file.contents()),
        ).into_response();
    }
    // SPA fallback → index.html for client routes (/app, /login, …)
    if let Some(idx) = USER_DIST.get_file("index.html").and_then(|f| f.contents_utf8()) {
        return ([(header::CACHE_CONTROL, "no-cache, must-revalidate")], Html(idx)).into_response();
    }
    (axum::http::StatusCode::NOT_FOUND, "not found").into_response()
}
```

- [ ] **Step 3: Wire the routes**

In `serve()`, replace the `.route("/", get(serve_index))` and `.route("/marked.min.js", …)` lines with:

```rust
        .route("/", get(serve_user_index))
        .route("/login", get(serve_user_index))
        .route("/app", get(serve_user_index))
        .route("/app/*rest", get(serve_user_index))
        .route("/assets/*path", get(serve_user_asset))
        .route("/favicon.svg", get(serve_user_asset))
        .route("/robots.txt", get(serve_user_asset))
```

Keep all `/admin*` and `/api/*` routes unchanged. Order matters: `/api/*` and `/admin*` are registered explicitly so they win over the SPA catch-alls.

- [ ] **Step 4: Build**

Run: `cargo build -p ai-note 2>&1 | tail -20`
Expected: compiles (the `user-ui/dist` from Task 6 satisfies `include_dir!`).

- [ ] **Step 5: Verify the SPA boots**

```bash
DEEPSEEK_API_KEY=$DEEPSEEK_API_KEY GEMINI_API_KEY=$GEMINI_API_KEY cargo run -p ai-note -- --port 6755 &
sleep 3 && curl -s localhost:6755/ | grep -o '<title>[^<]*' ; curl -s -o /dev/null -w "%{http_code}\n" localhost:6755/admin ; kill %1
```
Expected: `/` returns the SPA HTML; `/admin` still returns 200.

- [ ] **Step 6: Commit**

```bash
git add examples/ai-note/src/server.rs
git commit -m "feat(ai-note): serve user-ui SPA at / (retire legacy index.html)"
```

---

## Task 8: Frontend — SpaceContext, AppShell, routing, i18n

**Files:**
- Create: `user-ui/src/components/space-context.tsx`
- Modify: `user-ui/src/App.tsx`, `user-ui/src/components/app-shell.tsx`, `user-ui/src/locales/{en,zh}.json`

- [ ] **Step 1: Create `space-context.tsx`**

```tsx
import { createContext, useContext, useState, type ReactNode } from 'react';
import type { Space } from '@/lib/api';

const KEY = 'ai-note-space';
function initial(): Space {
  const v = localStorage.getItem(KEY);
  return v === 'work' || v === 'life' ? v : 'life';
}

interface Ctx { space: Space; setSpace: (s: Space) => void; }
const SpaceCtx = createContext<Ctx>({ space: 'life', setSpace: () => {} });

export function SpaceProvider({ children }: { children: ReactNode }) {
  const [space, setSpaceState] = useState<Space>(initial);
  const setSpace = (s: Space) => { localStorage.setItem(KEY, s); setSpaceState(s); };
  return <SpaceCtx.Provider value={{ space, setSpace }}>{children}</SpaceCtx.Provider>;
}

export function useSpace(): Ctx { return useContext(SpaceCtx); }
```

- [ ] **Step 2: Rewrite `App.tsx`**

```tsx
import { Routes, Route, Navigate } from 'react-router-dom';
import { getToken } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Marketing } from '@/pages/Marketing';
import { Notes } from '@/pages/Notes';
import { Search } from '@/pages/Search';
import { Profile } from '@/pages/Profile';
import { AppShell } from '@/components/app-shell';
import { SpaceProvider } from '@/components/space-context';

function RequireAuth({ children }: { children: React.ReactNode }) {
  return getToken() ? <>{children}</> : <Navigate to="/login" replace />;
}

export default function App() {
  return (
    <Routes>
      <Route path="/" element={<Marketing />} />
      <Route path="/login" element={<Login />} />
      <Route
        path="/app"
        element={
          <RequireAuth>
            <SpaceProvider>
              <AppShell />
            </SpaceProvider>
          </RequireAuth>
        }
      >
        <Route index element={<Notes />} />
        <Route path="search" element={<Search />} />
        <Route path="profile" element={<Profile />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
```

- [ ] **Step 3: Adapt `app-shell.tsx`**

Start from the copied ledger file. Changes:
- Replace the `NAV` array:

```tsx
import { NotebookPen, Search as SearchIcon, User, Globe, LogOut } from 'lucide-react';
const NAV = [
  { to: '/app', key: 'notes', icon: NotebookPen },
  { to: '/app/search', key: 'search', icon: SearchIcon },
  { to: '/app/profile', key: 'profile', icon: User },
] as const;
```

- Replace `ledgerApi` → `noteApi`.
- Add a 工作/生活 toggle in the header, after the brand `Link`. Add this component at the bottom of the file and render `<SpaceToggle />` inside the header `nav` area (desktop) and just below the header on mobile — simplest is to place it in the header center for both:

```tsx
import { useSpace } from '@/components/space-context';

function SpaceToggle() {
  const { t } = useTranslation();
  const { space, setSpace } = useSpace();
  return (
    <div className="bg-muted ml-4 inline-flex rounded-full p-0.5 text-xs">
      {(['work', 'life'] as const).map((s) => (
        <button
          key={s}
          type="button"
          onClick={() => setSpace(s)}
          className={cn(
            'rounded-full px-3 py-1 transition-colors',
            space === s ? 'bg-background text-foreground shadow-sm font-medium'
                        : 'text-muted-foreground',
          )}
        >
          {t(`spaces.${s}`)}
        </button>
      ))}
    </div>
  );
}
```

Render `<SpaceToggle />` immediately after the brand `<Link>` in the header `<div>`.

- [ ] **Step 4: Update i18n locale files**

In `user-ui/src/locales/en.json` and `zh.json`: keep the shared `common`, `chat`, `auth` keys. Replace `nav.*` and add `spaces`, `notes`, `search`, `editor`, `profile`. Add (en.json):

```json
{
  "brand": "ai-note",
  "nav": { "notes": "Notes", "search": "Search", "profile": "Profile" },
  "spaces": { "work": "Work", "life": "Life" },
  "notes": {
    "new": "New note", "empty": "No notes yet in this space.",
    "title": "Title", "body": "Write your note…", "tags": "tags (comma-separated)",
    "save": "Save", "delete": "Delete", "deleteConfirm": "Delete this note?", "deleted": "Deleted"
  },
  "search": { "placeholder": "Search your notes…", "empty": "No matches.", "semantic": "semantic", "grep": "text match" },
  "profile": { "model": "Chat model", "modelTrial": "Upgrade to switch models", "export": "Export all (.zip)" }
}
```

zh.json mirrors with: `"nav":{"notes":"笔记","search":"搜索","profile":"我的"}`, `"spaces":{"work":"工作","life":"生活"}`, notes/search/profile translated (e.g. `notes.new":"写笔记"`, `spaces` etc.). Keep existing `chat.*`, `common.*`, `auth.*` keys from the ledger copy.

- [ ] **Step 5: Build**

Run: `cd examples/ai-note/user-ui && npm run build 2>&1 | tail -20`
Expected: fails only on missing `pages/{Notes,Search,Profile}` — those land in Tasks 9-10. If you want a green checkpoint now, add 1-line stub components; otherwise proceed (Tasks 9-10 create them) and build at end of Task 10.

- [ ] **Step 6: Commit**

```bash
git add examples/ai-note/user-ui/src
git commit -m "feat(ai-note/ui): SpaceContext + space toggle + routing + i18n"
```

---

## Task 9: Frontend — Notes page + editor sheet, Profile page

**Files:**
- Create: `user-ui/src/pages/Notes.tsx`, `user-ui/src/components/notes/note-editor.tsx`, `user-ui/src/pages/Profile.tsx`, `user-ui/src/components/profile/model-picker.tsx`

- [ ] **Step 1: `components/notes/note-editor.tsx`**

```tsx
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { Sheet, SheetContent, SheetHeader, SheetTitle, SheetFooter } from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { noteApi, type Note, type Space } from '@/lib/api';

export function NoteEditor({
  open, onOpenChange, space, note, onSaved,
}: {
  open: boolean; onOpenChange: (v: boolean) => void;
  space: Space; note: Note | null; onSaved: () => void;
}) {
  const { t } = useTranslation();
  const [title, setTitle] = useState('');
  const [body, setBody] = useState('');
  const [tags, setTags] = useState('');
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) {
      setTitle(note?.title ?? '');
      setBody(note?.body ?? '');
      setTags(note?.tags.join(', ') ?? '');
    }
  }, [open, note]);

  async function save() {
    if (!body.trim()) { toast.error('empty'); return; }
    setBusy(true);
    const tagArr = tags.split(',').map((s) => s.trim()).filter(Boolean);
    try {
      if (note) await noteApi.updateNote(note.id, { title, body, tags: tagArr });
      else await noteApi.createNote(space, title, body, tagArr);
      onOpenChange(false);
      onSaved();
    } catch (e) { toast.error((e as Error).message); }
    finally { setBusy(false); }
  }

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="bottom" className="flex h-[90svh] flex-col">
        <SheetHeader><SheetTitle>{note ? t('notes.save') : t('notes.new')}</SheetTitle></SheetHeader>
        <div className="flex flex-1 flex-col gap-3 overflow-y-auto px-4">
          <Input placeholder={t('notes.title')} value={title} onChange={(e) => setTitle(e.target.value)} />
          <Textarea
            placeholder={t('notes.body')} value={body}
            onChange={(e) => setBody(e.target.value)} className="min-h-48 flex-1"
          />
          <Input placeholder={t('notes.tags')} value={tags} onChange={(e) => setTags(e.target.value)} />
        </div>
        <SheetFooter>
          <Button onClick={save} disabled={busy}>{t('notes.save')}</Button>
        </SheetFooter>
      </SheetContent>
    </Sheet>
  );
}
```

(If `Textarea` isn't in `components/ui`, it was copied from ledger — confirm `ui/textarea.tsx` exists; it does in the ledger tree.)

- [ ] **Step 2: `pages/Notes.tsx`**

```tsx
import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Plus, Trash2 } from 'lucide-react';
import { format, parseISO } from 'date-fns';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useSpace } from '@/components/space-context';
import { NoteEditor } from '@/components/notes/note-editor';
import { noteApi, type Note } from '@/lib/api';

export function Notes() {
  const { t } = useTranslation();
  const { space } = useSpace();
  const [notes, setNotes] = useState<Note[] | null>(null);
  const [editing, setEditing] = useState<Note | null>(null);
  const [open, setOpen] = useState(false);

  const load = useCallback(() => {
    setNotes(null);
    noteApi.notes(space).then((j) => setNotes(j.notes)).catch(() => setNotes([]));
  }, [space]);
  useEffect(load, [load]);

  async function del(e: React.MouseEvent, id: string) {
    e.stopPropagation();
    if (!confirm(t('notes.deleteConfirm'))) return;
    try { await noteApi.deleteNote(id); setNotes((c) => c?.filter((n) => n.id !== id) ?? null); toast.success(t('notes.deleted')); }
    catch (err) { toast.error((err as Error).message); }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">{t('nav.notes')}</h1>
        <Button onClick={() => { setEditing(null); setOpen(true); }}>
          <Plus className="size-4" /> {t('notes.new')}
        </Button>
      </div>
      {notes === null ? (
        <div className="space-y-2"><Skeleton className="h-20 w-full" /><Skeleton className="h-20 w-full" /></div>
      ) : notes.length === 0 ? (
        <p className="text-muted-foreground py-12 text-center text-sm">{t('notes.empty')}</p>
      ) : (
        <div className="space-y-2">
          {notes.map((n) => (
            <Card key={n.id} onClick={() => { setEditing(n); setOpen(true); }}
              className="hover:bg-accent cursor-pointer p-3">
              <div className="flex items-start justify-between gap-2">
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm font-medium">{n.title?.trim() || n.body.slice(0, 40)}</div>
                  <div className="text-muted-foreground mt-1 line-clamp-2 text-xs">{n.body}</div>
                  <div className="text-muted-foreground mt-1.5 flex flex-wrap items-center gap-1.5 text-[11px]">
                    {n.tags.map((tg) => <span key={tg} className="bg-secondary rounded px-1.5 py-0.5">{tg}</span>)}
                    <span>{format(parseISO(n.updated_at), 'yyyy-MM-dd HH:mm')}</span>
                  </div>
                </div>
                <Button variant="ghost" size="icon-sm" onClick={(e) => del(e, n.id)} aria-label="delete">
                  <Trash2 className="size-4" />
                </Button>
              </div>
            </Card>
          ))}
        </div>
      )}
      <NoteEditor open={open} onOpenChange={setOpen} space={space} note={editing} onSaved={load} />
    </div>
  );
}
```

- [ ] **Step 3: `components/profile/model-picker.tsx`**

```tsx
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select';
import { noteApi } from '@/lib/api';

export function ModelPicker({ tier, current }: { tier: string; current?: string }) {
  const { t } = useTranslation();
  const [models, setModels] = useState<string[]>([]);
  const [value, setValue] = useState(current ?? '');
  useEffect(() => { noteApi.info().then((j) => setModels(j.allowed_models)).catch(() => {}); }, []);
  const disabled = tier === 'trial';
  async function pick(m: string) {
    setValue(m);
    try { await noteApi.setModel(m); toast.success('ok'); } catch (e) { toast.error((e as Error).message); }
  }
  return (
    <div className="space-y-1">
      <div className="text-sm font-medium">{t('profile.model')}</div>
      {disabled ? (
        <p className="text-muted-foreground text-xs">{t('profile.modelTrial')}</p>
      ) : (
        <Select value={value} onValueChange={pick}>
          <SelectTrigger className="w-full"><SelectValue placeholder="—" /></SelectTrigger>
          <SelectContent>
            {models.map((m) => <SelectItem key={m} value={m}>{m}</SelectItem>)}
          </SelectContent>
        </Select>
      )}
    </div>
  );
}
```

- [ ] **Step 4: `pages/Profile.tsx`**

```tsx
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Download } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { ModelPicker } from '@/components/profile/model-picker';
import { noteApi, getToken } from '@/lib/api';

export function Profile() {
  const { t } = useTranslation();
  const [user, setUser] = useState<any>(null);
  useEffect(() => { noteApi.me().then((j) => setUser(j.user)).catch(() => {}); }, []);

  async function exportZip() {
    const resp = await fetch('/api/notes/export.zip', { headers: { Authorization: `Bearer ${getToken() ?? ''}` } });
    const blob = await resp.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob); a.download = 'notes.zip'; a.click();
    URL.revokeObjectURL(a.href);
  }

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold">{t('nav.profile')}</h1>
      <Card className="space-y-1 p-4">
        <div className="text-sm">{user?.email}</div>
        <div className="text-muted-foreground text-xs">{user?.tier}</div>
      </Card>
      <Card className="p-4">
        <ModelPicker tier={user?.tier ?? 'trial'} current={user?.preferred_model} />
      </Card>
      <Button variant="outline" onClick={exportZip}>
        <Download className="size-4" /> {t('profile.export')}
      </Button>
    </div>
  );
}
```

- [ ] **Step 5: Build**

Run: `cd examples/ai-note/user-ui && npm run build 2>&1 | tail -20`
Expected: fails only on missing `pages/Search` (Task 10). Add a 1-line stub or proceed.

- [ ] **Step 6: Commit**

```bash
git add examples/ai-note/user-ui/src
git commit -m "feat(ai-note/ui): Notes page + editor + Profile + model picker"
```

---

## Task 10: Frontend — Search page + chat wiring (per-space sessions)

**Files:**
- Create: `user-ui/src/pages/Search.tsx`
- Modify: `user-ui/src/components/chat/{chat-sheet,sessions-list,message-list,composer}.tsx`

- [ ] **Step 1: `pages/Search.tsx`**

```tsx
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { useSpace } from '@/components/space-context';
import { noteApi, type SearchHit } from '@/lib/api';

export function Search() {
  const { t } = useTranslation();
  const { space } = useSpace();
  const [q, setQ] = useState('');
  const [hits, setHits] = useState<SearchHit[] | null>(null);

  async function run(query: string) {
    if (!query.trim()) { setHits(null); return; }
    try { const j = await noteApi.search(space, query); setHits(j.hits); } catch { setHits([]); }
  }

  return (
    <div className="space-y-4">
      <Input
        placeholder={t('search.placeholder')} value={q}
        onChange={(e) => setQ(e.target.value)}
        onKeyDown={(e) => { if (e.key === 'Enter') run(q); }}
      />
      {hits === null ? null : hits.length === 0 ? (
        <p className="text-muted-foreground py-12 text-center text-sm">{t('search.empty')}</p>
      ) : (
        <div className="space-y-2">
          {hits.map((h) => (
            <Card key={h.id} className="p-3">
              <div className="flex items-center justify-between">
                <div className="truncate text-sm font-medium">{h.title?.trim() || h.body.slice(0, 40)}</div>
                <span className="text-muted-foreground text-[11px]">
                  {h.via_grep ? t('search.grep') : `${(h.score * 100).toFixed(0)}%`}
                </span>
              </div>
              <div className="text-muted-foreground mt-1 line-clamp-3 text-xs">{h.body}</div>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 2: Wire the chat sheet to space + note session endpoints**

In `chat-sheet.tsx` (copied from ledger): 
- Replace `ledgerApi` → `noteApi` and use `noteApi.chatSessions(space)`, `noteApi.createChatSession(space)`, `noteApi.getChatSession(id)`, `noteApi.deleteChatSession(id)`.
- Add `const { space } = useSpace();` (import from `@/components/space-context`) and pass `space` to session creation + list. Add `space` to the effect deps that refetch sessions, so switching space reloads the session list.
- Remove all attachment state/props: the `attachments` array, `uploadAttachment` calls, and the `attachment_ids` arg passed to `streamSession(...)` — call `streamSession(sessionId, text, onEvent, signal, lang)`.

In `sessions-list.tsx`: replace `ledgerApi` → `noteApi`; `chatSessions()` → `chatSessions(space)` (accept `space` as a prop from chat-sheet, or read `useSpace()` directly). Keep the unread-badge + filter logic.

In `composer.tsx`: remove the paperclip/attach button and `AttachmentPreview`; keep mic (if present) + textarea + send. `canSend = !busy && text.trim().length > 0`.

In `message-list.tsx`: remove `AttachmentThumb` / `AttachmentDialog` and the `attachmentIds` prop on `Bubble`; keep the truncated-marker + reload + tool lines.

- [ ] **Step 3: Final build**

Run: `cd examples/ai-note/user-ui && npm run build 2>&1 | tail -20`
Expected: PASS, clean `dist/`.

- [ ] **Step 4: Rebuild the backend (embeds fresh dist) + commit**

```bash
cargo build -p ai-note 2>&1 | tail -5
git add examples/ai-note/user-ui/src
git commit -m "feat(ai-note/ui): Search page + per-space chat wiring (attachments removed)"
```

---

## Task 11: Marketing + Login reword, manual verification, deploy

**Files:**
- Modify: `user-ui/src/pages/Marketing.tsx`, `user-ui/src/pages/Login.tsx`, `user-ui/index.html`

- [ ] **Step 1: Reword Marketing + Login + index.html for note-taking**

In `Marketing.tsx`: replace ledger finance copy with note-taking copy (hero: "你的 AI 笔记 — 说一句就记下，问一句就找到"; features: 语义搜索 / 工作·生活分区 / 流式对话捕捉). Keep the layout, JSON-LD `SoftwareApplication` block (change `name`, `description`, `applicationCategory` to "Productivity"). In `Login.tsx`: replace `ledgerApi` → `noteApi`, keep flow; on success `window.location.assign('/app')` (full reload, mirrors ledger's fix). In `index.html`: update `<title>`, meta description, og tags to ai-note.

- [ ] **Step 2: Build both**

```bash
cd examples/ai-note/user-ui && npm run build 2>&1 | tail -5 && cd .. && cargo build -p ai-note 2>&1 | tail -5
```
Expected: both clean.

- [ ] **Step 3: Manual verification (golden path)**

```bash
DEEPSEEK_API_KEY=$DEEPSEEK_API_KEY GEMINI_API_KEY=$GEMINI_API_KEY \
  cargo run -p ai-note -- --port 6755
```
In a browser at `http://localhost:6755`:
- Register (first user = admin) / login → lands on `/app` Notes.
- Toggle 工作/生活: create a note in each → confirm the list is space-filtered.
- Search: query in 工作 → only work hits.
- FAB chat in 工作: "记一下：明天和供应商对账" → watch tokens stream, a `create_note` tool line appears, reply persists; reopen sheet shows the saved transcript; the new note shows in Notes (work).
- Switch to 生活: confirm the work note + work chat sessions are hidden.
- Profile: switch model (must be paid; if trial, see the upgrade hint), export `.zip`.
- Mobile viewport (DevTools): bottom nav + space toggle + FAB clear of nav.
- Regression: `/admin` loads; `POST /api/chat` (one-shot) still answers.

Record any breakage and fix before deploy.

- [ ] **Step 4: Commit**

```bash
git add examples/ai-note/user-ui
git commit -m "feat(ai-note/ui): note-taking marketing + login; index.html meta"
```

- [ ] **Step 5: Deploy to qc-jp**

Cross-compile musl (existing ai-note build path — confirm the builder image/target), scp, install, restart:

```bash
cargo build --release --target x86_64-unknown-linux-musl -p ai-note
scp target/x86_64-unknown-linux-musl/release/ai-note qc-jp:/tmp/ai-note.new
ssh qc-jp 'sudo install -m 0755 /tmp/ai-note.new /opt/ai-note/ai-note && sudo systemctl restart ai-note'
```
(Use whatever musl build path ai-note already uses — check the ledger deploy notes / build container. ai-note binds `127.0.0.1:6755`, Caddy fronts `note.superleo.app`.)
Verify: `curl -s https://note.superleo.app/api/info` returns `allowed_models`; load the site, repeat the golden path quickly.

---

## Self-review notes (for the executor)

- **Cross-task compile:** Tasks 1-2 change db signatures that Tasks 3 & 5 + export handlers consume. The suite compiles cleanly only at the end of Task 5. Run `cargo test -p ai-note` there as the backend gate.
- **Spec coverage:** space column ✔ (T1), per-space chat sessions ✔ (T2,T5,T10), SSE streaming ✔ (T5), model picker ✔ (T4,T9), serve SPA + retire legacy ✔ (T7), Notes/Search/Profile ✔ (T9,T10), Marketing/Login ✔ (T11), attachments dropped ✔ (T6,T10), memory synthesizer omitted ✔ (T5 port note).
- **Default space `life`** is consistent across db defaults, `space_of`, `default_space()`, and `SpaceContext.initial()`.
