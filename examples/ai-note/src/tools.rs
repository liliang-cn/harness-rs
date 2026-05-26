//! NL tools exposed to the agent loop. Each `#[harness::tool]` registers
//! via inventory; `iter_macro_tools()` in main picks them up.
//!
//! Per-user scope: every handler reads `user_id` from
//! `world.profile.extra::<String>("user_id")`, which the HTTP layer plants
//! before launching the loop.

use chrono::{Local, Utc};
use harness::ToolError;
use harness::prelude::*;
use serde_json::{Value, json};
use std::path::PathBuf;

fn uid_of(w: &World) -> Result<String, ToolError> {
    w.profile
        .extra::<String>("user_id")
        .ok_or_else(|| ToolError::Exec("no user_id on world".into()))
}

fn db_path_of(w: &World) -> Result<PathBuf, ToolError> {
    let s = w
        .profile
        .extra::<String>("db_path")
        .ok_or_else(|| ToolError::Exec("no db_path on world".into()))?;
    Ok(PathBuf::from(s))
}

fn open_db(w: &World) -> Result<crate::db::Db, ToolError> {
    let p = db_path_of(w)?;
    crate::db::Db::open(&p).map_err(|e| ToolError::Exec(format!("db open: {e}")))
}

fn tier_of(w: &World) -> String {
    w.profile
        .extra::<String>("tier")
        .unwrap_or_else(|| "trial".into())
}

fn space_of(w: &World) -> String {
    w.profile
        .extra::<String>("space")
        .filter(|s| s == "work" || s == "life")
        .unwrap_or_else(|| "life".into())
}

fn embedder() -> Result<std::sync::Arc<dyn harness_core::Embedder>, ToolError> {
    crate::embed_slot::get().ok_or_else(|| ToolError::Exec("embedder not configured".into()))
}

// ───── tools ─────

/// Get the current wall-clock time. ALWAYS call this BEFORE interpreting any
/// relative date in the user's message ("今天" / "yesterday" / "上周" / "last
/// month" / "tomorrow" / "前天" / "next Friday" etc). Returns ISO timestamps
/// in both UTC and the user's local timezone, plus weekday and human format.
#[harness::tool(
    name = "current_time",
    risk = "read-only",
    schema = r#"{ "type": "object", "properties": {} }"#
)]
async fn current_time(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let now_utc = Utc::now();
    let tz_name = w.profile.tz.clone();
    let (iso_local, weekday, human, tz_source) = match tz_name
        .as_deref()
        .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
    {
        Some(tz) => {
            let local = now_utc.with_timezone(&tz);
            (
                local.to_rfc3339(),
                local.format("%A").to_string(),
                local.format("%Y-%m-%d %H:%M %Z").to_string(),
                format!("profile.tz={}", tz_name.as_deref().unwrap_or("?")),
            )
        }
        None => {
            let local = now_utc.with_timezone(&Local);
            (
                local.to_rfc3339(),
                local.format("%A").to_string(),
                local.format("%Y-%m-%d %H:%M %Z").to_string(),
                "system-clock".into(),
            )
        }
    };
    Ok(ToolResult {
        ok: true,
        content: json!({
            "iso_utc": now_utc.to_rfc3339(),
            "iso_local": iso_local,
            "weekday": weekday,
            "human": human,
            "timezone": tz_source,
        }),
        trace: None,
    })
}

/// Create a new note. Always extract the user's full intent into `body` —
/// don't summarise. `title` should be 4-15 chars capturing the gist; leave
/// empty if unsure. `tags` is comma-separated keywords (e.g. "work,refactor").
#[harness::tool(
    name = "create_note",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "title": { "type": "string", "description": "Short headline, ≤ 15 chars. Empty if unsure." },
            "body":  { "type": "string", "description": "The full note text from the user." },
            "tags":  { "type": "string", "description": "Comma-separated tags, optional." }
        },
        "required": ["body"]
    }"#
)]
async fn create_note(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "create_note".into(),
            reason: "body required".into(),
        })?;
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let space = space_of(w);
    let db = open_db(w)?;
    if tier_of(w) == "trial" {
        let used = db
            .count_notes(&uid, Some(&space))
            .map_err(|e| ToolError::Exec(format!("count: {e}")))?;
        let cap = crate::server::TRIAL_MAX_NOTES;
        if used >= cap {
            // Structured payload so the agent can phrase the upgrade nudge naturally.
            return Ok(ToolResult {
                ok: false,
                content: json!({
                    "error": "trial_limit",
                    "used": used,
                    "limit": cap,
                    "hint": "trial 用户最多 {limit} 条笔记。删几条腾空间，或升级到 paid（找个邀请码注册）。"
                        .replace("{limit}", &cap.to_string()),
                }),
                trace: Some(format!("trial cap hit ({used}/{cap})")),
            });
        }
    }
    let note = db
        .create_note(&uid, title, body, &tags, &space)
        .map_err(|e| ToolError::Exec(format!("insert: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "id": note.id,
            "title": note.title,
            "tags": note.tags,
            "embedding_status": "pending — search will use grep fallback until the worker fills it (~5s)"
        }),
        trace: Some(format!("created note {} ({} chars)", note.id, note.body.len())),
    })
}

/// Semantic search across the user's notes. Pass a natural-language query
/// (English or Chinese). Returns top_k notes ranked by cosine similarity, or
/// substring matches if embeddings aren't ready yet. Use this whenever the
/// user asks "did I write about X" / "关于 X 的笔记" / "find my note on Y".
#[harness::tool(
    name = "search_notes",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "query": { "type": "string", "description": "The user's question/topic verbatim." },
            "top_k": { "type": "integer", "description": "Max results, default 8.", "minimum": 1, "maximum": 50 }
        },
        "required": ["query"]
    }"#
)]
async fn search_notes(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let q = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "search_notes".into(),
            reason: "query required".into(),
        })?;
    let top_k = args
        .get("top_k")
        .and_then(|v| v.as_u64())
        .unwrap_or(8) as usize;

    let emb = embedder()?;
    let path = db_path_of(w)?;
    let hits = crate::search::semantic_search(&path, &uid, &emb, q, top_k, Some(&space_of(w)))
        .await
        .map_err(|e| ToolError::Exec(format!("search: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "count": hits.len(),
            "hits": hits,
            "mode": if hits.iter().any(|h| h.via_grep) { "grep" } else { "semantic" }
        }),
        trace: Some(format!("search '{q}' → {} hits", hits.len())),
    })
}

/// List the user's notes by updated_at, optionally filtered by date range.
/// Use for time-scoped queries ("今天写了什么" / "notes from last week" /
/// "what did I capture in 2025"). `since` and `until` are RFC3339 UTC
/// timestamps; resolve relative dates by calling `current_time` first then
/// computing the window yourself (e.g. for "今天" use today's 00:00 in the
/// user's local TZ converted to UTC).
#[harness::tool(
    name = "list_recent_notes",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "description": "Default 10, max 200.", "minimum": 1, "maximum": 200 },
            "since": { "type": "string", "description": "RFC3339 UTC, inclusive lower bound on updated_at." },
            "until": { "type": "string", "description": "RFC3339 UTC, inclusive upper bound on updated_at." }
        }
    }"#
)]
async fn list_recent_notes(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10).min(200) as u32;
    let since = args.get("since").and_then(|v| v.as_str());
    let until = args.get("until").and_then(|v| v.as_str());
    let sp = space_of(w);
    let db = open_db(w)?;
    let notes = if since.is_some() || until.is_some() {
        db.list_notes_in_range(&uid, Some(&sp), since, until, limit)
            .map_err(|e| ToolError::Exec(format!("list: {e}")))?
    } else {
        db.list_recent_notes(&uid, Some(&sp), limit)
            .map_err(|e| ToolError::Exec(format!("list: {e}")))?
    };
    Ok(ToolResult {
        ok: true,
        content: json!({
            "count": notes.len(),
            "notes": notes,
            "filter": { "since": since, "until": until }
        }),
        trace: None,
    })
}

/// Update an existing note's title / body / tags by id. Each field is optional;
/// only provided ones are changed. Embedding clears + re-pending on any touch.
/// Get the id first via search_notes / list_recent_notes.
#[harness::tool(
    name = "update_note",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "id":    { "type": "string" },
            "title": { "type": "string" },
            "body":  { "type": "string" },
            "tags":  { "type": "string", "description": "Comma-separated, optional." }
        },
        "required": ["id"]
    }"#
)]
async fn update_note(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "update_note".into(),
            reason: "id required".into(),
        })?;
    let title = args.get("title").and_then(|v| v.as_str());
    let body = args.get("body").and_then(|v| v.as_str());
    let tags: Option<Vec<String>> = args.get("tags").and_then(|v| v.as_str()).map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    });

    let db = open_db(w)?;
    let n = db
        .update_note(&uid, id, title, body, tags.as_deref())
        .map_err(|e| ToolError::Exec(format!("update: {e}")))?;
    if n == 0 {
        return Err(ToolError::Exec(format!("note `{id}` not found")));
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "id": id, "updated": n, "embedding_status": "re-pending" }),
        trace: None,
    })
}

/// Delete a note by id. Confirm with the user before calling — no soft-delete.
/// Get the id first via search_notes / list_recent_notes.
#[harness::tool(
    name = "delete_note",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": { "id": { "type": "string" } },
        "required": ["id"]
    }"#
)]
async fn delete_note(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "delete_note".into(),
            reason: "id required".into(),
        })?;
    let db = open_db(w)?;
    let n = db
        .delete_note(&uid, id)
        .map_err(|e| ToolError::Exec(format!("delete: {e}")))?;
    if n == 0 {
        return Err(ToolError::Exec(format!("note `{id}` not found")));
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "deleted": id }),
        trace: None,
    })
}

// ───── goal tools ─────

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
