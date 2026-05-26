# ai-note Plans & Goals Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a first-class Plans & Goals subsystem to ai-note: capture goals/rules in natural language via the chat agent, decompose into sub-goals, and run periodic review (复盘), surfaced in a new in-app Plans page. Phase 1 = in-app only (no daemon/email).

**Architecture:** Two new SQLite tables (`goals`, `goal_reviews`), both space-scoped like notes. The chat agent is the authoring surface (new tools: create_goal / decompose_goal / update_goal / list_goals / log_review). A new `/app/plans` page browses goals + shows a "到期复盘" due list; per-goal `review_interval_days` + `next_review_at` drive the due query. A small chat-prefill store lets Plans-page buttons open the chat composer pre-filled (keeping authoring NL-first).

**Tech Stack:** Rust (axum 0.7, rusqlite, harness-rs `#[harness::tool]`), React 19 + Vite + shadcn/Radix + react-i18next, chrono.

**Spec:** `docs/superpowers/specs/2026-05-26-ai-note-plans-goals-design.md`

**Working dir for all paths:** `examples/ai-note/`. Repo root: `/Users/liliang/Things/courses/harness`.

**Conventions already in the codebase (reuse, don't reinvent):**
- `db.rs` has `random_id()`, `parse_rfc3339(&str)->DateTime<Utc>`, `ensure_column(table,col,decl)`, and a `#[cfg(test)] mod tests` with `tmp_db()`.
- `tools.rs` has `uid_of(w)`, `db_path_of(w)`, `open_db(w)`, `tier_of(w)`, `space_of(w)` (returns "work"/"life", default "life"), and `#[harness::tool(name=…, risk=…, schema=…)]`.
- `server.rs` has `AuthCtx`, `open_db_state(&s)->Result<Db,ApiError>`, `ApiError`, the route list in `serve()`.
- Tool schemas must be **Gemini-safe**: optional fields are simply omitted from `required` (do NOT use `"type": ["string","null"]`).
- Timestamps are RFC3339 UTC strings; SQLite lexicographic compare is correct for them.

---

## Task 1: db.rs — `goals` + `goal_reviews` tables, structs, helpers, tests

**Files:** Modify `examples/ai-note/src/db.rs`

- [ ] **Step 1: Add the tables to `init`**

In `fn init`, inside the `execute_batch` SQL (before the closing `"#`), add:

```sql
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
```

(No `ensure_column` calls needed — these are brand-new tables; `CREATE TABLE IF NOT EXISTS` covers fresh + existing DBs.)

- [ ] **Step 2: Add the structs**

Near the other structs (after `ChatMessage`), add:

```rust
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
```

- [ ] **Step 3: Add `row_to_goal` free fn**

Near `row_to_note`:

```rust
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
```

- [ ] **Step 4: Add the goal helpers in `impl Db`**

```rust
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
```

- [ ] **Step 5: Write unit tests**

Add to the `#[cfg(test)] mod tests` block:

```rust
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
```

- [ ] **Step 6: Run tests**

Run: `cd /Users/liliang/Things/courses/harness && cargo test -p ai-note 2>&1 | tail -15`
Expected: all tests pass (the new two + the pre-existing ones).

- [ ] **Step 7: Commit**

```bash
git add examples/ai-note/src/db.rs
git commit -m "feat(ai-note): goals + goal_reviews tables, db helpers (per-space, cadenced review)"
```

---

## Task 2: server.rs — Plans REST endpoints

**Files:** Modify `examples/ai-note/src/server.rs`

- [ ] **Step 1: Add request structs + handlers**

Add near the other handlers in `server.rs` (ids are generated inside `db.create_goal` via `random_id()`, so no id helper is needed here):

```rust
#[derive(Deserialize)]
struct GoalsQuery {
    #[serde(default = "default_space")]
    space: String,
    /// "active" (default) | "due" | "all"
    filter: Option<String>,
}

async fn list_goals_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<GoalsQuery>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let (status, only_due) = match q.filter.as_deref() {
        Some("all") => (None, false),
        Some("due") => (Some("active"), true),
        _ => (Some("active"), false),
    };
    let goals = db.list_goals(&auth.user.id, &q.space, status, only_due)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let due = db.count_due_goals(&auth.user.id, &q.space)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "goals": goals, "due_count": due })))
}

#[derive(Deserialize)]
struct CreateGoalReq {
    #[serde(default = "default_space")]
    space: String,
    #[serde(default = "default_kind")]
    kind: String,
    title: String,
    #[serde(default)]
    detail: String,
    parent_id: Option<String>,
    target_date: Option<String>,
    review_interval_days: Option<i64>,
}
fn default_kind() -> String { "goal".into() }

async fn create_goal_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateGoalReq>,
) -> Result<Json<Value>, ApiError> {
    if req.title.trim().is_empty() {
        return Err(ApiError::BadRequest("title is empty".into()));
    }
    if req.space != "work" && req.space != "life" {
        return Err(ApiError::BadRequest("space must be 'work' or 'life'".into()));
    }
    if req.kind != "goal" && req.kind != "rule" {
        return Err(ApiError::BadRequest("kind must be 'goal' or 'rule'".into()));
    }
    let db = open_db_state(&s)?;
    let goal = db.create_goal(
        &auth.user.id, &req.space, &req.kind, &req.title, &req.detail,
        req.parent_id.as_deref(), req.target_date.as_deref(), req.review_interval_days,
    ).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "goal": goal })))
}

async fn get_goal_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let goal = db.get_goal(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("goal not found".into()))?;
    let subgoals = db.list_subgoals(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let reviews = db.list_reviews(&auth.user.id, &id, 100)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "goal": goal, "subgoals": subgoals, "reviews": reviews })))
}

#[derive(Deserialize)]
struct UpdateGoalReq {
    status: Option<String>,
    title: Option<String>,
    detail: Option<String>,
    target_date: Option<String>,
    review_interval_days: Option<i64>,
}

async fn update_goal_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
    Json(req): Json<UpdateGoalReq>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let n = db.update_goal(
        &auth.user.id, &id, req.status.as_deref(), req.title.as_deref(),
        req.detail.as_deref(), req.target_date.as_deref(), req.review_interval_days,
    ).map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 { return Err(ApiError::BadRequest("goal not found".into())); }
    Ok(Json(json!({ "ok": true })))
}

async fn delete_goal_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let n = db.delete_goal(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 { return Err(ApiError::BadRequest("goal not found".into())); }
    Ok(Json(json!({ "deleted": id })))
}

#[derive(Deserialize)]
struct AddReviewReq {
    progress: String,
    #[serde(default)]
    next_steps: String,
    next_review_in_days: Option<i64>,
}

async fn add_review_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
    Json(req): Json<AddReviewReq>,
) -> Result<Json<Value>, ApiError> {
    if req.progress.trim().is_empty() {
        return Err(ApiError::BadRequest("progress is empty".into()));
    }
    let db = open_db_state(&s)?;
    db.get_goal(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("goal not found".into()))?;
    let review = db.add_review(&auth.user.id, &id, &req.progress, &req.next_steps,
                               req.next_review_in_days)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "review": review })))
}
```

- [ ] **Step 2: Register the routes**

In `serve()`, after the notes routes, add:

```rust
        .route("/api/goals", get(list_goals_handler).post(create_goal_handler))
        .route(
            "/api/goals/:id",
            get(get_goal_handler).patch(update_goal_handler).delete(delete_goal_handler),
        )
        .route("/api/goals/:id/reviews", post(add_review_handler))
```

- [ ] **Step 3: Build**

Run: `cd /Users/liliang/Things/courses/harness && cargo build -p ai-note 2>&1 | tail -15`
Expected: compiles (0 errors). Confirm `Query`, `Path`, `post`, `get`, `Json`, `Deserialize`, `json!`, `Value`, `default_space` are already imported in server.rs (they are, from the notes/chat handlers).

- [ ] **Step 4: Smoke test**

```bash
DEEPSEEK_API_KEY=${DEEPSEEK_API_KEY:-dummy} GEMINI_API_KEY=${GEMINI_API_KEY:-dummy} \
  HARNESS_NOTE_DB=/tmp/ainote-goals.db cargo run -p ai-note -- --port 6755 &
sleep 4
TOK=$(curl -s -X POST localhost:6755/api/register -H 'Content-Type: application/json' -d '{"email":"g@t.co","password":"pw123456"}' | python3 -c "import sys,json;print(json.load(sys.stdin)['token'])")
echo "--- create goal ---"; curl -s -X POST localhost:6755/api/goals -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' -d '{"space":"work","kind":"goal","title":"架构专家","target_date":"2026-09-30","review_interval_days":30}'
echo; echo "--- list active ---"; curl -s "localhost:6755/api/goals?space=work" -H "Authorization: Bearer $TOK"
kill %1; rm -f /tmp/ainote-goals.db*
```
Expected: create returns `{"goal":{…,"next_review_at":…}}`; list returns the goal + `due_count:0`.

- [ ] **Step 5: Commit**

```bash
git add examples/ai-note/src/server.rs
git commit -m "feat(ai-note): Plans REST endpoints (goals CRUD + reviews, space-aware)"
```

---

## Task 3: tools.rs + SYSTEM_PROMPT — agent goal tools (the NL surface)

**Files:** Modify `examples/ai-note/src/tools.rs`, `examples/ai-note/src/server.rs` (SYSTEM_PROMPT)

- [ ] **Step 1: Add the goal tools in `tools.rs`**

Append these `#[harness::tool]`s (they use the existing `uid_of`, `open_db`, `space_of`):

```rust
/// Create a goal or a standing rule. Use kind="goal" for an aspiration with an
/// optional target_date + review cadence (call current_time first to resolve
/// relative dates like "今年9月"). Use kind="rule" for a standing constraint
/// ("股票不要操作"), with no date/cadence. Pass parent_id to make it a sub-goal.
#[harness::tool(
    name = "create_goal",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "kind":  { "type": "string", "enum": ["goal", "rule"], "description": "goal = aspiration; rule = standing constraint." },
            "title": { "type": "string", "description": "Short headline." },
            "detail": { "type": "string", "description": "Optional longer description / markdown." },
            "target_date": { "type": "string", "description": "RFC3339 date for goals, e.g. 2026-09-30. Omit for rules." },
            "review_interval_days": { "type": "integer", "description": "Review cadence in days (e.g. 7, 30). Omit for rules.", "minimum": 1 },
            "parent_id": { "type": "string", "description": "If this is a sub-goal, the parent goal id." }
        },
        "required": ["kind", "title"]
    }"#
)]
async fn create_goal(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let space = space_of(w);
    let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("goal");
    if kind != "goal" && kind != "rule" {
        return Err(ToolError::InvalidArgs { name: "create_goal".into(), reason: "kind must be goal|rule".into() });
    }
    let title = args.get("title").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "create_goal".into(), reason: "title required".into() })?;
    let detail = args.get("detail").and_then(|v| v.as_str()).unwrap_or("");
    let target_date = args.get("target_date").and_then(|v| v.as_str());
    let interval = args.get("review_interval_days").and_then(|v| v.as_i64());
    let parent_id = args.get("parent_id").and_then(|v| v.as_str());
    let db = open_db(w)?;
    let goal = db.create_goal(&uid, &space, kind, title, detail, parent_id, target_date, interval)
        .map_err(|e| ToolError::Exec(format!("insert goal: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({ "id": goal.id, "kind": goal.kind, "title": goal.title,
                         "target_date": goal.target_date, "next_review_at": goal.next_review_at }),
        trace: Some(format!("created {} goal {}", goal.kind, goal.id)),
    })
}

/// Break a goal into sub-goals. Pass the parent goal id and a list of sub-goals.
/// Each sub-goal is created as kind="goal" in the same space.
#[harness::tool(
    name = "decompose_goal",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "parent_id": { "type": "string" },
            "subgoals": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "detail": { "type": "string" }
                    },
                    "required": ["title"]
                }
            }
        },
        "required": ["parent_id", "subgoals"]
    }"#
)]
async fn decompose_goal(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let space = space_of(w);
    let parent_id = args.get("parent_id").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "decompose_goal".into(), reason: "parent_id required".into() })?;
    let subs = args.get("subgoals").and_then(|v| v.as_array())
        .ok_or_else(|| ToolError::InvalidArgs { name: "decompose_goal".into(), reason: "subgoals required".into() })?;
    let db = open_db(w)?;
    // Validate parent exists + belongs to user.
    if db.get_goal(&uid, parent_id).map_err(|e| ToolError::Exec(format!("{e}")))?.is_none() {
        return Err(ToolError::Exec(format!("parent goal `{parent_id}` not found")));
    }
    let mut ids = Vec::new();
    for sg in subs {
        let title = sg.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
        if title.is_empty() { continue; }
        let detail = sg.get("detail").and_then(|v| v.as_str()).unwrap_or("");
        let g = db.create_goal(&uid, &space, "goal", title, detail, Some(parent_id), None, None)
            .map_err(|e| ToolError::Exec(format!("insert subgoal: {e}")))?;
        ids.push(g.id);
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "parent_id": parent_id, "created": ids.len(), "ids": ids }),
        trace: Some(format!("decomposed {parent_id} into {} subgoals", ids.len())),
    })
}

/// Update a goal: change status (active|done|dropped|paused), title, detail,
/// target_date (RFC3339), or review_interval_days. Get the id first via list_goals.
#[harness::tool(
    name = "update_goal",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "status": { "type": "string", "enum": ["active", "done", "dropped", "paused"] },
            "title": { "type": "string" },
            "detail": { "type": "string" },
            "target_date": { "type": "string" },
            "review_interval_days": { "type": "integer", "minimum": 1 }
        },
        "required": ["id"]
    }"#
)]
async fn update_goal(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let id = args.get("id").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "update_goal".into(), reason: "id required".into() })?;
    let db = open_db(w)?;
    let n = db.update_goal(
        &uid, id,
        args.get("status").and_then(|v| v.as_str()),
        args.get("title").and_then(|v| v.as_str()),
        args.get("detail").and_then(|v| v.as_str()),
        args.get("target_date").and_then(|v| v.as_str()),
        args.get("review_interval_days").and_then(|v| v.as_i64()),
    ).map_err(|e| ToolError::Exec(format!("update goal: {e}")))?;
    if n == 0 { return Err(ToolError::Exec(format!("goal `{id}` not found"))); }
    Ok(ToolResult { ok: true, content: json!({ "id": id, "updated": n }), trace: None })
}

/// List the user's goals in the current space. Use due_for_review=true to get
/// only goals whose review is due (for 复盘). Pass parent_id to list sub-goals.
#[harness::tool(
    name = "list_goals",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "status": { "type": "string", "enum": ["active", "done", "dropped", "paused"] },
            "due_for_review": { "type": "boolean" },
            "parent_id": { "type": "string" }
        }
    }"#
)]
async fn list_goals(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let space = space_of(w);
    let db = open_db(w)?;
    let goals = if let Some(pid) = args.get("parent_id").and_then(|v| v.as_str()) {
        db.list_subgoals(&uid, pid).map_err(|e| ToolError::Exec(format!("{e}")))?
    } else {
        let due = args.get("due_for_review").and_then(|v| v.as_bool()).unwrap_or(false);
        let status = args.get("status").and_then(|v| v.as_str()).or(Some("active"));
        db.list_goals(&uid, &space, status, due).map_err(|e| ToolError::Exec(format!("{e}")))?
    };
    Ok(ToolResult {
        ok: true,
        content: json!({ "count": goals.len(), "goals": goals }),
        trace: None,
    })
}

/// Log a review (复盘) for a goal: progress + optional next steps. Advances the
/// goal's next review by its cadence (or next_review_in_days if provided).
#[harness::tool(
    name = "log_review",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "goal_id": { "type": "string" },
            "progress": { "type": "string", "description": "What happened / self-assessment." },
            "next_steps": { "type": "string" },
            "next_review_in_days": { "type": "integer", "minimum": 1 }
        },
        "required": ["goal_id", "progress"]
    }"#
)]
async fn log_review(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let goal_id = args.get("goal_id").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "log_review".into(), reason: "goal_id required".into() })?;
    let progress = args.get("progress").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "log_review".into(), reason: "progress required".into() })?;
    let next_steps = args.get("next_steps").and_then(|v| v.as_str()).unwrap_or("");
    let override_days = args.get("next_review_in_days").and_then(|v| v.as_i64());
    let db = open_db(w)?;
    if db.get_goal(&uid, goal_id).map_err(|e| ToolError::Exec(format!("{e}")))?.is_none() {
        return Err(ToolError::Exec(format!("goal `{goal_id}` not found")));
    }
    let review = db.add_review(&uid, goal_id, progress, next_steps, override_days)
        .map_err(|e| ToolError::Exec(format!("add review: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({ "review_id": review.id, "goal_id": goal_id }),
        trace: Some(format!("logged review for {goal_id}")),
    })
}
```

- [ ] **Step 2: Extend SYSTEM_PROMPT in `server.rs`**

Append to the `SYSTEM_PROMPT` string (before the closing quote), as new rules:

```
11. **Goals & rules.** When the user states an aspiration ("我要…", "今年X月…", \
   "三个月内…成为…"), call `current_time` FIRST to resolve the relative date, then \
   `create_goal(kind=\"goal\", target_date=<RFC3339>, review_interval_days=30)` \
   (default monthly cadence unless they imply another). When the user states a \
   standing rule / 戒律 ("股票不要操作", "不要…", "每天…"), call \
   `create_goal(kind=\"rule\")` with no date or cadence.\n\
12. **Decompose.** When asked to break a goal down ("拆解一下", "分解成几步"), find \
   the goal id via `list_goals`, then call `decompose_goal` with concrete sub-goals.\n\
13. **复盘 / review.** When the user says "复盘" / "review" / "进展如何", call \
   `list_goals` with due_for_review=true, walk the due goals with the user, then \
   `log_review` for each discussed (progress + next_steps in their words). Mark a \
   finished goal `update_goal(status=\"done\")`.\n\
14. All goal operations are scoped to the user's current space (rule 10); never \
   mix spaces.\n\
```

(Renumber if the existing prompt already uses these numbers — append after the last existing rule, continuing the sequence.)

- [ ] **Step 3: Build + test**

Run: `cd /Users/liliang/Things/courses/harness && cargo build -p ai-note 2>&1 | tail -10 && cargo test -p ai-note 2>&1 | tail -5`
Expected: compiles; tests still pass. (The macro tools auto-register via inventory — no manual wiring needed; they're picked up by `iter_macro_tools()` already used in the chat handlers.)

- [ ] **Step 4: Commit**

```bash
git add examples/ai-note/src/tools.rs examples/ai-note/src/server.rs
git commit -m "feat(ai-note): agent goal tools (create/decompose/update/list/review) + prompt rules"
```

---

## Task 4: Frontend — api.ts goal helpers + chat-prefill store

**Files:** Modify `examples/ai-note/user-ui/src/lib/api.ts`; Create `examples/ai-note/user-ui/src/lib/chat-prefill.ts`; Modify `examples/ai-note/user-ui/src/components/chat/chat-sheet.tsx`, `examples/ai-note/user-ui/src/components/chat/chat-fab.tsx`

- [ ] **Step 1: Add goal types + helpers to `api.ts`**

Add to `src/lib/api.ts` (after the ChatMessage type and inside/after the `noteApi` object — add these as new properties on `noteApi`):

```ts
export type GoalKind = 'goal' | 'rule';
export type GoalStatus = 'active' | 'done' | 'dropped' | 'paused';
export interface Goal {
  id: string; space: Space; kind: GoalKind; title: string; detail: string;
  status: GoalStatus; parent_id?: string | null;
  target_date?: string | null; review_interval_days?: number | null;
  next_review_at?: string | null; created_at: string; updated_at: string;
}
export interface GoalReview {
  id: string; goal_id: string; progress: string; next_steps: string; created_at: string;
}
```

And add these methods inside the `noteApi` object literal:

```ts
  goals: (space: Space, filter: 'active' | 'due' | 'all' = 'active') =>
    req<{ goals: Goal[]; due_count: number }>(`/api/goals?space=${space}&filter=${filter}`),
  goal: (id: string) =>
    req<{ goal: Goal; subgoals: Goal[]; reviews: GoalReview[] }>(`/api/goals/${id}`),
  createGoal: (body: { space: Space; kind: GoalKind; title: string; detail?: string;
                       parent_id?: string; target_date?: string; review_interval_days?: number }) =>
    req<{ goal: Goal }>('/api/goals', { method: 'POST', body: JSON.stringify(body) }),
  updateGoal: (id: string, patch: Partial<Pick<Goal, 'status' | 'title' | 'detail' | 'target_date' | 'review_interval_days'>>) =>
    req<{ ok: boolean }>(`/api/goals/${id}`, { method: 'PATCH', body: JSON.stringify(patch) }),
  deleteGoal: (id: string) =>
    req<{ deleted: string }>(`/api/goals/${id}`, { method: 'DELETE' }),
  addReview: (id: string, body: { progress: string; next_steps?: string; next_review_in_days?: number }) =>
    req<{ review: GoalReview }>(`/api/goals/${id}/reviews`, { method: 'POST', body: JSON.stringify(body) }),
```

- [ ] **Step 2: Create the chat-prefill store**

Create `src/lib/chat-prefill.ts`:

```ts
// Tiny pub/sub so non-chat pages (e.g. Plans) can open the chat sheet with the
// composer pre-filled. The ChatFab subscribes; callers invoke openChatWith().
type Listener = (text: string) => void;
const listeners = new Set<Listener>();

export function openChatWith(text: string) {
  for (const l of listeners) l(text);
}

export function subscribeChatPrefill(l: Listener): () => void {
  listeners.add(l);
  return () => listeners.delete(l);
}
```

- [ ] **Step 3: Wire ChatFab + ChatSheet to consume prefill**

In `src/components/chat/chat-fab.tsx`, subscribe so an external `openChatWith(text)` opens the sheet and passes the prefill text down. Replace the component body with:

```tsx
import { useEffect, useState } from 'react';
import { MessageSquare } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { Button } from '@/components/ui/button';
import { ChatSheet } from './chat-sheet';
import { subscribeChatPrefill } from '@/lib/chat-prefill';

export function ChatFab() {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [prefill, setPrefill] = useState<string | undefined>();
  useEffect(() => subscribeChatPrefill((text) => {
    setPrefill(text);
    setOpen(true);
  }), []);
  return (
    <>
      <Button
        type="button"
        aria-label={t('chat.fab')}
        onClick={() => { setPrefill(undefined); setOpen(true); }}
        className="fixed right-4 bottom-20 z-20 size-14 rounded-full shadow-lg md:right-6 md:bottom-6"
      >
        <MessageSquare className="size-6" />
      </Button>
      <ChatSheet open={open} onOpenChange={setOpen} prefill={prefill} />
    </>
  );
}
```

In `src/components/chat/chat-sheet.tsx`: add an optional `prefill?: string` prop to the component's props type, and an effect that, when the sheet opens with a `prefill`, starts a new chat and sets the composer text. Concretely:
- Add `prefill` to the props destructure + type.
- Find where the composer's input text state lives (e.g. `const [input, setInput] = useState('')` or similar) and where "new chat" is triggered. Add:

```tsx
  useEffect(() => {
    if (open && prefill) {
      // start a fresh draft and seed the composer
      setActiveId(null);        // back to a new conversation (adjust to the file's state names)
      setInput(prefill);        // seed composer (adjust to the actual composer text state)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, prefill]);
```

Read `chat-sheet.tsx` first to use its actual state setter names (the names above are illustrative — match what exists). The composer (`composer.tsx`) receives its text via props/state from chat-sheet; seed that same state. If the composer holds its own internal text state, lift a `value`/`onChange` or pass an `initialText` prop through — keep the change minimal and consistent with the existing wiring.

- [ ] **Step 4: Build**

Run: `cd /Users/liliang/Things/courses/harness/examples/ai-note/user-ui && npx tsc --noEmit 2>&1 | tail -15 && npm run build 2>&1 | tail -5`
Expected: tsc clean, build green.

- [ ] **Step 5: Commit**

```bash
git add examples/ai-note/user-ui/src/lib examples/ai-note/user-ui/src/components/chat examples/ai-note/user-ui/dist
git commit -m "feat(ai-note/ui): goal api helpers + chat-prefill store (open chat pre-filled)"
```

---

## Task 5: Frontend — Plans nav + page

**Files:** Modify `examples/ai-note/user-ui/src/components/app-shell.tsx`, `examples/ai-note/user-ui/src/App.tsx`, `examples/ai-note/user-ui/src/locales/{en,zh}.json`; Create `examples/ai-note/user-ui/src/pages/Plans.tsx`

- [ ] **Step 1: Add the nav item**

In `app-shell.tsx`, add to the `NAV` array (import `Target` from lucide-react):

```tsx
import { NotebookPen, Search as SearchIcon, Target, User, Globe, LogOut } from 'lucide-react';
const NAV = [
  { to: '/app', key: 'notes', icon: NotebookPen },
  { to: '/app/plans', key: 'plans', icon: Target },
  { to: '/app/search', key: 'search', icon: SearchIcon },
  { to: '/app/profile', key: 'profile', icon: User },
] as const;
```

(Keep `Globe`/`LogOut` if already imported; merge the import line.)

- [ ] **Step 2: Add the route**

In `App.tsx`, add inside the `/app` route's children (next to `search`/`profile`):

```tsx
        <Route path="plans" element={<Plans />} />
```

And import: `import { Plans } from '@/pages/Plans';`

- [ ] **Step 3: Add i18n keys**

In `src/locales/en.json` add under the existing object (and mirror in `zh.json`):

```json
"nav": { "notes": "Notes", "plans": "Plans", "search": "Search", "profile": "Profile" },
"plans": {
  "title": "Plans",
  "due": "Due for review",
  "goals": "Goals",
  "rules": "Principles",
  "empty": "No goals yet. Tell the assistant a goal to get started.",
  "review": "Review",
  "addGoal": "New goal",
  "noDue": "Nothing due. Nice.",
  "targetDate": "Target",
  "markDone": "Mark done",
  "delete": "Delete",
  "deleteConfirm": "Delete this goal (and its sub-goals)?",
  "subgoals": "Sub-goals",
  "reviews": "Review history",
  "addSubgoal": "Break down",
  "done": "Done"
}
```

zh.json equivalents: `"nav":{…,"plans":"计划",…}`, `"plans":{"title":"计划","due":"到期复盘","goals":"目标","rules":"原则","empty":"还没有目标。直接跟助手说一个目标开始吧。","review":"复盘","addGoal":"新目标","noDue":"暂无到期，棒。","targetDate":"目标日期","markDone":"标记完成","delete":"删除","deleteConfirm":"删除这个目标（及其子目标）？","subgoals":"子目标","reviews":"复盘记录","addSubgoal":"拆解","done":"已完成"}`.

(The existing `nav` object only has notes/search/profile — replace it with the 4-key version above in both locales.)

- [ ] **Step 4: Create `pages/Plans.tsx`**

```tsx
import { lazy, Suspense, useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Target, ChevronRight, Sparkles } from 'lucide-react';
import { format, parseISO } from 'date-fns';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { useSpace } from '@/components/space-context';
import { openChatWith } from '@/lib/chat-prefill';
import { noteApi, type Goal } from '@/lib/api';

const GoalDetail = lazy(() =>
  import('@/components/plans/goal-detail').then((m) => ({ default: m.GoalDetail })),
);

export function Plans() {
  const { t } = useTranslation();
  const { space } = useSpace();
  const [goals, setGoals] = useState<Goal[] | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);

  const load = useCallback(() => {
    setGoals(null);
    noteApi.goals(space, 'all').then((j) => setGoals(j.goals)).catch(() => setGoals([]));
  }, [space]);
  useEffect(load, [load]);

  const now = Date.now();
  const active = (goals ?? []).filter((g) => g.status === 'active' && !g.parent_id);
  const due = active.filter((g) => g.kind === 'goal' && g.next_review_at && Date.parse(g.next_review_at) <= now);
  const topGoals = active.filter((g) => g.kind === 'goal');
  const rules = active.filter((g) => g.kind === 'rule');

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">{t('plans.title')}</h1>
        <Button variant="outline" onClick={() => openChatWith('我想定一个新目标：')}>
          <Sparkles className="size-4" /> {t('plans.addGoal')}
        </Button>
      </div>

      {goals === null ? (
        <div className="space-y-2"><Skeleton className="h-16 w-full" /><Skeleton className="h-16 w-full" /></div>
      ) : active.length === 0 ? (
        <p className="text-muted-foreground py-12 text-center text-sm">{t('plans.empty')}</p>
      ) : (
        <>
          <section className="space-y-2">
            <h2 className="text-muted-foreground text-xs font-medium uppercase">{t('plans.due')}</h2>
            {due.length === 0 ? (
              <p className="text-muted-foreground text-sm">{t('plans.noDue')}</p>
            ) : due.map((g) => (
              <Card key={g.id} className="flex items-center justify-between gap-2 p-3">
                <button className="min-w-0 flex-1 text-left" onClick={() => setOpenId(g.id)}>
                  <div className="truncate text-sm font-medium">{g.title}</div>
                </button>
                <Button size="sm" onClick={() => openChatWith(`复盘：${g.title}`)}>{t('plans.review')}</Button>
              </Card>
            ))}
          </section>

          <section className="space-y-2">
            <h2 className="text-muted-foreground text-xs font-medium uppercase">{t('plans.goals')}</h2>
            {topGoals.map((g) => (
              <Card key={g.id} onClick={() => setOpenId(g.id)} className="hover:bg-accent flex cursor-pointer items-center gap-2 p-3">
                <Target className="text-muted-foreground size-4 shrink-0" />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm font-medium">{g.title}</div>
                  {g.target_date && (
                    <div className="text-muted-foreground text-xs">
                      {t('plans.targetDate')}: {format(parseISO(g.target_date), 'yyyy-MM-dd')}
                    </div>
                  )}
                </div>
                <ChevronRight className="text-muted-foreground size-4" />
              </Card>
            ))}
          </section>

          {rules.length > 0 && (
            <section className="space-y-2">
              <h2 className="text-muted-foreground text-xs font-medium uppercase">{t('plans.rules')}</h2>
              {rules.map((g) => (
                <Card key={g.id} onClick={() => setOpenId(g.id)} className="hover:bg-accent cursor-pointer p-3 text-sm">
                  {g.title}
                </Card>
              ))}
            </section>
          )}
        </>
      )}

      {openId && (
        <Suspense fallback={null}>
          <GoalDetail id={openId} open={!!openId} onOpenChange={(v) => !v && setOpenId(null)} onChanged={load} />
        </Suspense>
      )}
    </div>
  );
}
```

- [ ] **Step 5: Build (will fail until Task 6 creates GoalDetail — add a stub or proceed)**

Create a temporary stub `src/components/plans/goal-detail.tsx` so this task builds green:

```tsx
export function GoalDetail(_: { id: string; open: boolean; onOpenChange: (v: boolean) => void; onChanged: () => void }) {
  return null; // real implementation in Task 6
}
```

Run: `cd /Users/liliang/Things/courses/harness/examples/ai-note/user-ui && npx tsc --noEmit 2>&1 | tail -15 && npm run build 2>&1 | tail -5`
Expected: tsc clean, build green.

- [ ] **Step 6: Commit**

```bash
git add examples/ai-note/user-ui/src examples/ai-note/user-ui/dist
git commit -m "feat(ai-note/ui): Plans page + nav (due review / goals / rules), prefilled chat actions"
```

---

## Task 6: Frontend — Goal detail sheet (subgoals + review timeline)

**Files:** Overwrite `examples/ai-note/user-ui/src/components/plans/goal-detail.tsx`

- [ ] **Step 1: Implement the detail sheet**

```tsx
import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { format, parseISO } from 'date-fns';
import { Check, Trash2, Sparkles } from 'lucide-react';
import { toast } from 'sonner';
import { Sheet, SheetContent, SheetHeader, SheetTitle } from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { renderMarkdown } from '@/lib/markdown';
import { openChatWith } from '@/lib/chat-prefill';
import { noteApi, type Goal, type GoalReview } from '@/lib/api';

export function GoalDetail({
  id, open, onOpenChange, onChanged,
}: {
  id: string; open: boolean; onOpenChange: (v: boolean) => void; onChanged: () => void;
}) {
  const { t } = useTranslation();
  const [data, setData] = useState<{ goal: Goal; subgoals: Goal[]; reviews: GoalReview[] } | null>(null);

  const load = useCallback(() => {
    setData(null);
    noteApi.goal(id).then(setData).catch(() => setData(null));
  }, [id]);
  useEffect(() => { if (open) load(); }, [open, load]);

  async function toggleSub(sg: Goal) {
    const next = sg.status === 'done' ? 'active' : 'done';
    await noteApi.updateGoal(sg.id, { status: next });
    load();
  }
  async function markDone() {
    await noteApi.updateGoal(id, { status: 'done' });
    toast.success(t('plans.done'));
    onOpenChange(false); onChanged();
  }
  async function del() {
    if (!confirm(t('plans.deleteConfirm'))) return;
    await noteApi.deleteGoal(id);
    onOpenChange(false); onChanged();
  }

  const goal = data?.goal;

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="bottom" className="flex h-[90svh] flex-col">
        <SheetHeader>
          <SheetTitle>{goal?.title ?? '…'}</SheetTitle>
        </SheetHeader>
        <div className="flex-1 space-y-5 overflow-y-auto px-4 pb-4">
          {!data ? (
            <Skeleton className="h-24 w-full" />
          ) : (
            <>
              {goal!.target_date && (
                <div className="text-muted-foreground text-xs">
                  {t('plans.targetDate')}: {format(parseISO(goal!.target_date), 'yyyy-MM-dd')}
                </div>
              )}
              {goal!.detail.trim() && (
                <div className="markdown-body text-sm"
                     dangerouslySetInnerHTML={{ __html: renderMarkdown(goal!.detail) }} />
              )}

              <section className="space-y-2">
                <div className="flex items-center justify-between">
                  <h3 className="text-sm font-medium">{t('plans.subgoals')}</h3>
                  <Button variant="ghost" size="sm" onClick={() => openChatWith(`把「${goal!.title}」拆解一下`)}>
                    <Sparkles className="size-3.5" /> {t('plans.addSubgoal')}
                  </Button>
                </div>
                {data.subgoals.length === 0 ? (
                  <p className="text-muted-foreground text-xs">—</p>
                ) : data.subgoals.map((sg) => (
                  <button key={sg.id} onClick={() => toggleSub(sg)}
                    className="hover:bg-accent flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm">
                    <span className={`flex size-4 items-center justify-center rounded border ${sg.status === 'done' ? 'bg-primary text-primary-foreground' : ''}`}>
                      {sg.status === 'done' && <Check className="size-3" />}
                    </span>
                    <span className={sg.status === 'done' ? 'text-muted-foreground line-through' : ''}>{sg.title}</span>
                  </button>
                ))}
              </section>

              <section className="space-y-2">
                <h3 className="text-sm font-medium">{t('plans.reviews')}</h3>
                {data.reviews.length === 0 ? (
                  <p className="text-muted-foreground text-xs">—</p>
                ) : data.reviews.map((rv) => (
                  <div key={rv.id} className="border-border rounded-md border p-2 text-sm">
                    <div className="text-muted-foreground mb-1 text-[11px]">
                      {format(parseISO(rv.created_at), 'yyyy-MM-dd HH:mm')}
                    </div>
                    <div className="whitespace-pre-wrap">{rv.progress}</div>
                    {rv.next_steps.trim() && (
                      <div className="text-muted-foreground mt-1 whitespace-pre-wrap">→ {rv.next_steps}</div>
                    )}
                  </div>
                ))}
              </section>

              <div className="flex gap-2 pt-2">
                <Button onClick={() => openChatWith(`复盘：${goal!.title}`)}>{t('plans.review')}</Button>
                <Button variant="outline" onClick={markDone}><Check className="size-4" /> {t('plans.markDone')}</Button>
                <Button variant="ghost" size="icon" onClick={del} aria-label={t('plans.delete')}>
                  <Trash2 className="size-4" />
                </Button>
              </div>
            </>
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}
```

- [ ] **Step 2: Build**

Run: `cd /Users/liliang/Things/courses/harness/examples/ai-note/user-ui && npx tsc --noEmit 2>&1 | tail -15 && npm run build 2>&1 | tail -5`
Expected: tsc clean, build green.

- [ ] **Step 3: Commit**

```bash
git add examples/ai-note/user-ui/src/components/plans examples/ai-note/user-ui/dist
git commit -m "feat(ai-note/ui): goal detail sheet (subgoal checkoff + review timeline)"
```

---

## Task 7: Build, manual verification, deploy

**Files:** none (integration)

- [ ] **Step 1: Full build**

```bash
cd /Users/liliang/Things/courses/harness/examples/ai-note/user-ui && npm run build 2>&1 | tail -5
cd /Users/liliang/Things/courses/harness && cargo build -p ai-note 2>&1 | tail -5 && cargo test -p ai-note 2>&1 | tail -5
```
Expected: all green.

- [ ] **Step 2: Manual golden path (local, real keys for chat)**

```bash
GEMINI_API_KEY=$GEMINI_API_KEY DEEPSEEK_API_KEY=$DEEPSEEK_API_KEY \
  HARNESS_NOTE_DB=/tmp/ainote-plans.db cargo run -p ai-note -- --port 6755
```
In a browser at `http://localhost:6755` (or via Playwright):
- Register/login → toggle **Work** → open **Plans** (empty state).
- Chat (FAB): "今年9月成为企业级高可用的架构专家" → confirm agent calls current_time + create_goal; the goal appears on Plans under 目标 with target 2026-09-30.
- Chat: "股票不要操作" → appears under 原则.
- Chat: "把架构专家这个目标拆解一下" → open the goal detail → sub-goals listed; check one off.
- Force a due review: chat "把架构专家的复盘周期改成1天" (update_goal interval) won't backdate next_review; instead verify the 复盘 flow: tap a goal's **复盘** button → chat opens prefilled "复盘：…" → send progress → log_review writes a review (visible in the goal detail timeline).
- Switch to **Life** → work goals hidden.
- Regression: notes / search / existing chat note tools still work.

- [ ] **Step 3: Deploy to qc-jp**

```bash
docker exec ai-ledger-builder bash -lc 'export PATH=/usr/local/cargo/bin:$PATH && cd /work && cargo build --release --target x86_64-unknown-linux-musl -p ai-note 2>&1 | tail -3'
cd /Users/liliang/Things/courses/harness
scp -q target-musl/x86_64-unknown-linux-musl/release/ai-note qc-jp:/tmp/ai-note.new
ssh qc-jp 'sudo install -m 0755 /tmp/ai-note.new /opt/ai-note/ai-note && sudo systemctl restart ai-note && sleep 3 && systemctl is-active ai-note && rm -f /tmp/ai-note.new'
```
Verify: `curl -s https://note.superleo.app/api/info` ok; load the site, open Plans, capture a goal via chat, confirm it appears.

---

## Self-review notes (for the executor)

- **Backend gate is end of Task 3** (`cargo test -p ai-note` + `cargo build`). Tasks 1-3 are independent of the frontend.
- **Frontend ordering:** Task 5 needs Task 4 (api + prefill); Task 6 replaces the Task-5 stub. Each task keeps `npm run build` green (Task 5 ships a `goal-detail` stub).
- **Spec coverage:** goals/goal_reviews tables ✔(T1); per-goal cadence + due query ✔(T1: next_review seeding, list_goals only_due, count_due_goals; T2 filter=due); decomposition via parent_id ✔(T1 list_subgoals, T3 decompose_goal); kind goal/rule ✔(T1/T2/T3); REST endpoints ✔(T2); agent tools + prompt ✔(T3); Plans page with 到期复盘/目标/原则 + detail (subgoal tree + review timeline) ✔(T5,T6); NL-first authoring via chat-prefill ✔(T4 store, T5/T6 buttons); space scoping ✔ throughout; in-app only / no daemon ✔ (nothing scheduled).
- **Type consistency:** `noteApi.goals/goal/createGoal/updateGoal/deleteGoal/addReview` names match between T4 (definition) and T5/T6 (use); `Goal`/`GoalReview` fields match the Rust structs' serialized names; `openChatWith`/`subscribeChatPrefill` match between T4 and T5/T6.
- **Gemini-safe schemas:** all optional tool args are omitted from `required`; no `["type","null"]` unions used.
- **chat-sheet prefill (T4 Step 3):** the state-setter names are illustrative — the executor MUST read `chat-sheet.tsx` first and match its real composer-text + active-session state. Keep the change minimal.
