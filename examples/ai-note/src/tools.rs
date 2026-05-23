//! NL tools exposed to the agent loop. Each `#[harness::tool]` registers
//! via inventory; `iter_macro_tools()` in main picks them up.
//!
//! Per-user scope: every handler reads `user_id` from
//! `world.profile.extra::<String>("user_id")`, which the HTTP layer plants
//! before launching the loop.

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

fn embedder() -> Result<std::sync::Arc<dyn harness_core::Embedder>, ToolError> {
    crate::embed_slot::get().ok_or_else(|| ToolError::Exec("embedder not configured".into()))
}

// ───── tools ─────

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

    let db = open_db(w)?;
    let note = db
        .create_note(&uid, title, body, &tags)
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
    let hits = crate::search::semantic_search(&path, &uid, &emb, q, top_k)
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

/// List the user's most recent notes by updated_at. Use for overview queries
/// ("what have I been writing"). `limit` defaults to 10.
#[harness::tool(
    name = "list_recent_notes",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "limit": { "type": "integer", "description": "Default 10, max 100.", "minimum": 1, "maximum": 100 }
        }
    }"#
)]
async fn list_recent_notes(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
    let db = open_db(w)?;
    let notes = db
        .list_recent_notes(&uid, limit)
        .map_err(|e| ToolError::Exec(format!("list: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({ "count": notes.len(), "notes": notes }),
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
