//! HTTP layer for ai-note.
//!
//! Routes:
//! - public  : /api/info  /api/register  /api/login
//! - me      : /api/me*  /api/me/invites  /api/me/password
//! - notes   : /api/notes (GET list + POST create), /api/notes/:id (GET PATCH DELETE),
//!             /api/notes/search?q=&limit=
//! - chat    : /api/chat (one-shot agent loop, returns final reply + tool trace)

use crate::auth::{AuthCtx, AuthError, hash_password, new_session, verify_password, validate_email};
use crate::db::Db;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use chrono::Utc;
use harness::Task;
use harness_core::{Block, Embedder, Model};
use harness_loop::{AgentLoop, Outcome};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("index.html");
const MARKED_JS: &str = include_str!("marked.min.js");

/// Vite-built admin SPA, embedded into the binary so deploys stay
/// single-artifact. Built by `cd admin-ui && npm run build`.
static ADMIN_DIST: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/admin-ui/dist");

/// Max notes a `tier == "trial"` user may have at once. Paid / admin are
/// unbounded. Edit / delete don't count — only the inserts.
pub const TRIAL_MAX_NOTES: u32 = 30;

/// Admin-mutable runtime config (mirrors ai-ledger's pattern).
/// Provider keys / chat provider+model live here and are reflected to the
/// DB so they survive restart. The actual chat-model adapter is still
/// built at startup; mid-flight key changes require a restart.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub deepseek_key: Option<String>,
    pub gemini_key: Option<String>,
    pub chat_provider: String,
    pub chat_model: String,
    /// Per-model token pricing card. Seeded from `pricing::default_rate_card()`
    /// on first launch; persisted as JSON under provider_config key
    /// `pricing_rate_card`. Edited via PATCH /api/admin/config.
    pub pricing: crate::pricing::RateCard,
}

#[derive(Clone)]
pub struct AppState {
    pub db_path: PathBuf,
    pub model: Arc<dyn Model>,
    pub embedder: Arc<dyn Embedder>,
    pub max_iters: u32,
    pub model_handle: String,
    /// IANA tz id (e.g. "Asia/Shanghai"). Planted on the agent's
    /// profile.tz so `current_time` returns the right local clock.
    pub user_tz: Option<String>,
    /// Hot-readable provider config. Admin endpoints write through this
    /// under `RwLock` so reads from the user-facing endpoints (info / chat)
    /// see updates without a restart.
    pub config: Arc<std::sync::RwLock<AppConfig>>,
}

impl AppState {
    pub fn cfg(&self) -> AppConfig {
        self.config.read().expect("config lock poisoned").clone()
    }
}

impl AppState {
    pub fn resolve_session(&self, token: &str) -> Result<crate::auth::User, String> {
        let db = open_db_state(self).map_err(|e| e.to_string())?;
        let s = db
            .get_session(token)
            .map_err(|e| e.to_string())?
            .ok_or("session not found")?;
        let now = Utc::now();
        if s.expires_at < now {
            let _ = db.delete_session(token);
            return Err("session expired".into());
        }
        let _ = db.touch_session(token, now);
        let user = db
            .get_user_by_id(&s.user_id)
            .map_err(|e| e.to_string())?
            .ok_or("user gone")?;
        Ok(user)
    }
}

pub(crate) fn open_db_state(s: &AppState) -> Result<Db, ApiError> {
    if let Some(parent) = s.db_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    Db::open(&s.db_path).map_err(|e| ApiError::Internal(format!("db: {e}")))
}

pub(crate) enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    Internal(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::BadRequest(s)
            | ApiError::Unauthorized(s)
            | ApiError::Forbidden(s)
            | ApiError::Internal(s) => write!(f, "{s}"),
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (s, m) = match self {
            ApiError::BadRequest(s) => (StatusCode::BAD_REQUEST, s),
            ApiError::Unauthorized(s) => (StatusCode::UNAUTHORIZED, s),
            ApiError::Forbidden(s) => (StatusCode::FORBIDDEN, s),
            ApiError::Internal(s) => (StatusCode::INTERNAL_SERVER_ERROR, s),
        };
        (s, Json(json!({ "error": m }))).into_response()
    }
}

pub async fn serve(state: AppState, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/", get(serve_index))
        .route("/marked.min.js", get(serve_marked_js))
        // Admin SPA: GET /admin or /admin/ → index.html; /admin/* → asset
        // file, with SPA fallback to index.html for client-side routes.
        .route("/admin", get(serve_admin_index))
        .route("/admin/", get(serve_admin_index))
        .route("/admin/*path", get(serve_admin_asset))
        .route("/api/info", get(info_handler))
        .route("/api/register", post(register_handler))
        .route("/api/login", post(login_handler))
        .route("/api/logout", post(logout_handler))
        .route("/api/me", get(me_handler))
        .route(
            "/api/me/invites",
            get(list_invites_handler).post(create_invite_handler),
        )
        .route("/api/me/password", post(change_password_handler))
        .route(
            "/api/notes",
            get(list_notes_handler).post(create_note_handler),
        )
        .route(
            "/api/notes/:id",
            get(get_note_handler).patch(update_note_handler).delete(delete_note_handler),
        )
        .route("/api/notes/:id/export.md", get(export_note_md_handler))
        .route("/api/notes/export.zip", get(export_all_zip_handler))
        .route("/api/notes/search", get(search_handler))
        .route("/api/chat", post(chat_handler));

    // Admin endpoints — gated by tier == "admin" in the handlers.
    let app = crate::admin::register_routes(app)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(addr = %addr, "ai-note listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_index() -> impl axum::response::IntoResponse {
    use axum::http::header;
    (
        [(header::CACHE_CONTROL, "no-cache, must-revalidate")],
        Html(INDEX_HTML),
    )
}

async fn serve_admin_index() -> impl axum::response::IntoResponse {
    use axum::http::header;
    let body = ADMIN_DIST
        .get_file("index.html")
        .and_then(|f| f.contents_utf8())
        .unwrap_or("<h1>admin UI not built</h1>");
    (
        [(header::CACHE_CONTROL, "no-cache, must-revalidate")],
        Html(body),
    )
}

async fn serve_admin_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    // First try the literal asset path inside dist/. If absent, fall through
    // to index.html (SPA fallback for client-side routes like /admin/users).
    if let Some(file) = ADMIN_DIST.get_file(&path) {
        let mime = mime_for(&path);
        return (
            [
                (header::CONTENT_TYPE, mime),
                // Vite hashes filenames in /assets, so long-cache them. Other
                // static files (favicon.svg etc.) get a short cache only.
                (
                    header::CACHE_CONTROL,
                    if path.starts_with("assets/") {
                        "public, max-age=31536000, immutable"
                    } else {
                        "no-cache"
                    },
                ),
            ],
            Body::from(file.contents()),
        )
            .into_response();
    }
    if let Some(idx) = ADMIN_DIST
        .get_file("index.html")
        .and_then(|f| f.contents_utf8())
    {
        return (
            [(header::CACHE_CONTROL, "no-cache, must-revalidate")],
            Html(idx),
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, "admin asset not found").into_response()
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "json" => "application/json",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

async fn serve_marked_js() -> impl axum::response::IntoResponse {
    use axum::http::header;
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        MARKED_JS,
    )
}

async fn info_handler(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "model": s.model_handle,
        "embedder": s.embedder.handle(),
        "embedder_dim": s.embedder.dim(),
    }))
}

// ───── auth handlers ─────

#[derive(Deserialize)]
struct RegisterReq {
    email: String,
    password: String,
    invite_code: Option<String>,
}

async fn register_handler(
    State(s): State<AppState>,
    Json(req): Json<RegisterReq>,
) -> Result<Json<Value>, ApiError> {
    validate_email(&req.email).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let db = open_db_state(&s)?;
    if db
        .get_user_by_email(&req.email)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .is_some()
    {
        return Err(ApiError::BadRequest(AuthError::EmailExists.to_string()));
    }
    let pw_hash = hash_password(&req.password).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    // Bootstrap: first user is admin (no invite needed).
    let total = db.count_users().map_err(|e| ApiError::Internal(e.to_string()))?;
    let (tier, invited_by, code_used) = if total == 0 {
        ("admin".to_string(), None, None)
    } else {
        match req.invite_code.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(code) => {
                let inv = db
                    .get_invite(code)
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
                match inv {
                    Some(i) if i.uses_remaining > 0 => {
                        db.consume_invite(&i.code)
                            .map_err(|e| ApiError::Internal(e.to_string()))?;
                        ("paid".to_string(), Some(i.created_by), Some(i.code))
                    }
                    _ => return Err(ApiError::BadRequest(AuthError::BadInvite.to_string())),
                }
            }
            None => ("trial".to_string(), None, None),
        }
    };
    let user = crate::auth::User {
        id: random_user_id(),
        email: req.email.trim().to_string(),
        password_hash: pw_hash,
        tier,
        invited_by,
        invite_code_used: code_used,
        created_at: Utc::now(),
        preferred_model: None,
    };
    db.insert_user(&user)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let session = new_session(&user.id);
    db.insert_session(&session)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let _ = db.insert_audit(
        Some(&user.id),
        "register",
        None,
        Some(&json!({"email": user.email, "tier": user.tier}).to_string()),
        0,
        0,
    );
    Ok(Json(json!({ "token": session.token, "user": &user })))
}

#[derive(Deserialize)]
struct LoginReq {
    email: String,
    password: String,
}

async fn login_handler(
    State(s): State<AppState>,
    Json(req): Json<LoginReq>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let user = db
        .get_user_by_email(&req.email)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| {
            let _ = open_db_state(&s).map(|db| {
                db.insert_audit(
                    None,
                    "login_failed",
                    None,
                    Some(&json!({"email": req.email, "reason": "no_such_user"}).to_string()),
                    0,
                    0,
                )
            });
            ApiError::Unauthorized(AuthError::BadCredentials.to_string())
        })?;
    if !verify_password(&req.password, &user.password_hash) {
        let _ = db.insert_audit(
            Some(&user.id),
            "login_failed",
            None,
            Some(&json!({"reason": "bad_password"}).to_string()),
            0,
            0,
        );
        return Err(ApiError::Unauthorized(AuthError::BadCredentials.to_string()));
    }
    let session = new_session(&user.id);
    db.insert_session(&session)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let _ = db.insert_audit(Some(&user.id), "login", None, None, 0, 0);
    Ok(Json(json!({ "token": session.token, "user": &user })))
}

async fn logout_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<Value>, ApiError> {
    let _ = open_db_state(&s).map(|db| db.insert_audit(Some(&auth.user.id), "logout", None, None, 0, 0));
    Ok(Json(json!({ "ok": true })))
}

async fn me_handler(auth: AuthCtx) -> Json<Value> {
    Json(json!({ "user": auth.user }))
}

async fn list_invites_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<Value>, ApiError> {
    if auth.user.tier == "trial" {
        return Err(ApiError::Forbidden(
            "trial users can't invite — get a paid account first".into(),
        ));
    }
    let db = open_db_state(&s)?;
    let invites = db
        .list_invites_by_creator(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "invites": invites })))
}

async fn create_invite_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<Value>, ApiError> {
    if auth.user.tier == "trial" {
        return Err(ApiError::Forbidden("trial users can't invite".into()));
    }
    let db = open_db_state(&s)?;
    let inv = crate::auth::Invite {
        code: random_user_id(),
        created_by: auth.user.id.clone(),
        uses_remaining: 1,
        expires_at: None,
        created_at: Utc::now(),
    };
    db.insert_invite(&inv)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "invite": inv })))
}

#[derive(Deserialize)]
struct ChangePw {
    old_password: String,
    new_password: String,
}

async fn change_password_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<ChangePw>,
) -> Result<Json<Value>, ApiError> {
    if !verify_password(&req.old_password, &auth.user.password_hash) {
        return Err(ApiError::Unauthorized("当前密码不正确".into()));
    }
    if req.new_password == req.old_password {
        return Err(ApiError::BadRequest("新密码不能跟旧密码相同".into()));
    }
    let new_hash =
        hash_password(&req.new_password).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let db = open_db_state(&s)?;
    db.update_user_password(&auth.user.id, &new_hash)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let dropped = db
        .delete_other_sessions(&auth.user.id, &auth.token)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let _ = db.insert_audit(
        Some(&auth.user.id),
        "password_change",
        None,
        Some(&json!({"other_sessions_dropped": dropped}).to_string()),
        0,
        0,
    );
    Ok(Json(json!({ "ok": true, "other_sessions_dropped": dropped })))
}

// ───── notes CRUD ─────

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<u32>,
}

async fn list_notes_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    // Task 5 wires ?space= query param; for now pass None (return all spaces).
    let notes = db
        .list_recent_notes(&auth.user.id, None, q.limit.unwrap_or(50).min(500))
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "count": notes.len(), "notes": notes })))
}

#[derive(Deserialize)]
struct CreateNoteReq {
    #[serde(default)]
    title: String,
    body: String,
    #[serde(default)]
    tags: Vec<String>,
}

async fn create_note_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<CreateNoteReq>,
) -> Result<Json<Value>, ApiError> {
    if req.body.trim().is_empty() {
        return Err(ApiError::BadRequest("body is empty".into()));
    }
    let db = open_db_state(&s)?;
    // Task 5 wires ?space= on create_note_handler; default to "life" for now.
    let space = "life";
    // Trial cap. Edit/delete uncapped; only inserts count.
    if auth.user.tier == "trial" {
        let used = db
            .count_notes(&auth.user.id, Some(space))
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if used >= TRIAL_MAX_NOTES {
            return Err(ApiError::Forbidden(format!(
                "trial limit hit — you have {used} notes, the cap is {TRIAL_MAX_NOTES}. Delete some, or upgrade to paid."
            )));
        }
    }
    let note = db
        .create_note(&auth.user.id, &req.title, &req.body, &req.tags, space)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "note": note })))
}

async fn get_note_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let note = db
        .get_note(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("note not found".into()))?;
    Ok(Json(json!({ "note": note })))
}

#[derive(Deserialize)]
struct UpdateNoteReq {
    title: Option<String>,
    body: Option<String>,
    tags: Option<Vec<String>>,
}

async fn update_note_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
    Json(req): Json<UpdateNoteReq>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let tags_owned = req.tags;
    let n = db
        .update_note(
            &auth.user.id,
            &id,
            req.title.as_deref(),
            req.body.as_deref(),
            tags_owned.as_deref(),
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest("note not found".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

/// Export a single note as a markdown file. Body is stored verbatim, so
/// any markdown the user (or agent) wrote round-trips. We prepend a
/// minimal YAML-front-matter block with id / dates / tags so the download
/// is self-describing — useful for re-import / cross-tool sharing.
async fn export_note_md_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let db = open_db_state(&s)?;
    let note = db
        .get_note(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("note not found".into()))?;

    let title_line = if note.title.trim().is_empty() {
        // Fall back to the first 32 chars of body as a heading so the file
        // isn't headless.
        let head: String = note.body.chars().take(32).collect();
        head
    } else {
        note.title.clone()
    };
    let tags_yaml = if note.tags.is_empty() {
        String::new()
    } else {
        let joined = note
            .tags
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(", ");
        format!("tags: [{joined}]\n")
    };
    let body = format!(
        "---\n\
         id: {}\n\
         created_at: {}\n\
         updated_at: {}\n\
         {}\
         ---\n\
         \n\
         # {}\n\
         \n\
         {}\n",
        note.id,
        note.created_at.to_rfc3339(),
        note.updated_at.to_rfc3339(),
        tags_yaml,
        title_line,
        note.body,
    );

    let pretty = build_md_filename(&title_line, &note.id);
    // RFC 6266: `filename="..."` must be ASCII (technically Latin-1, but
    // putting raw UTF-8 there made Chrome render mojibake on CJK titles).
    // The pretty CJK name rides in `filename*=UTF-8''…` only; every modern
    // browser prefers it when both are present.
    let ascii_fallback = format!("note-{}.md", note.id);
    Ok((
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{}",
                    percent_encode(&pretty)
                ),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response())
}

/// Export every note the caller owns as a single .zip archive.
/// Layout:
///   notes/<title>-<id8>.md   — one file per note, same YAML-front-matter
///                              shape as the single-note export
///   index.md                 — human-readable table of contents
/// Filename: `notes-YYYYMMDD-<id8>.zip`. We zip in-memory because the
/// per-user cap (30 trial / unbounded paid) and short note bodies keep
/// archives well under a few MB; if that stops being true, swap to a
/// streaming `body_with_io_writer`.
async fn export_all_zip_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse;
    use std::io::Write;

    let db = open_db_state(&s)?;
    // 10k cap — well above the 30-note trial limit; paid users with more
    // than 10k notes can ask for a streaming export later.
    // Export all spaces (None = no space filter).
    let notes = db
        .list_recent_notes(&auth.user.id, None, 10_000)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let cursor = std::io::Cursor::new(buf);
    let mut zip = zip::ZipWriter::new(cursor);
    let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let mut idx = String::from("# Notes Index\n\n");
    idx.push_str(&format!("Exported {} notes for {}\n\n", notes.len(), auth.user.email));
    idx.push_str("| Date | Title | Tags | File |\n|---|---|---|---|\n");

    let mut used_names = std::collections::HashSet::<String>::new();
    for note in &notes {
        let title_line = if note.title.trim().is_empty() {
            note.body.chars().take(32).collect::<String>()
        } else {
            note.title.clone()
        };
        let base = build_md_filename(&title_line, &note.id);
        // Dedupe in the rare case build_md_filename collides (same title +
        // same id is impossible, but be defensive).
        let mut fname = format!("notes/{base}");
        let mut n = 1;
        while !used_names.insert(fname.clone()) {
            fname = format!("notes/{base}.{n}");
            n += 1;
        }

        let tags_yaml = if note.tags.is_empty() {
            String::new()
        } else {
            let joined = note
                .tags
                .iter()
                .map(|t| format!("\"{}\"", t.replace('"', "\\\"")))
                .collect::<Vec<_>>()
                .join(", ");
            format!("tags: [{joined}]\n")
        };
        let body = format!(
            "---\n\
             id: {}\n\
             created_at: {}\n\
             updated_at: {}\n\
             {}\
             ---\n\
             \n\
             # {}\n\
             \n\
             {}\n",
            note.id,
            note.created_at.to_rfc3339(),
            note.updated_at.to_rfc3339(),
            tags_yaml,
            title_line,
            note.body,
        );

        zip.start_file(&fname, opts)
            .map_err(|e| ApiError::Internal(format!("zip: {e}")))?;
        zip.write_all(body.as_bytes())
            .map_err(|e| ApiError::Internal(format!("zip write: {e}")))?;

        let tags_disp = if note.tags.is_empty() { "—".into() } else { note.tags.join(", ") };
        let date = note.created_at.format("%Y-%m-%d").to_string();
        // Escape pipe so it doesn't break the markdown table.
        let title_esc = title_line.replace('|', "\\|");
        idx.push_str(&format!(
            "| {date} | {title_esc} | {tags_disp} | [{base}](./{fname}) |\n"
        ));
    }

    zip.start_file("index.md", opts)
        .map_err(|e| ApiError::Internal(format!("zip: {e}")))?;
    zip.write_all(idx.as_bytes())
        .map_err(|e| ApiError::Internal(format!("zip write: {e}")))?;

    let cursor = zip
        .finish()
        .map_err(|e| ApiError::Internal(format!("zip finish: {e}")))?;
    let bytes = cursor.into_inner();

    let stamp = Utc::now().format("%Y%m%d").to_string();
    let id_short: String = auth.user.id.chars().take(8).collect();
    let ascii_fallback = format!("notes-{stamp}-{id_short}.zip");

    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{ascii_fallback}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
}

/// Build a filesystem-safe filename from a title + id. Keeps CJK characters
/// (they're fine in modern filesystems) but strips path separators and
/// other shell-hostile bytes. Always ends with `-<id>.md` so siblings stay
/// distinct even with duplicate titles.
fn build_md_filename(title: &str, id: &str) -> String {
    let bad: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|', '\n', '\r', '\t'];
    let clean: String = title
        .chars()
        .map(|c| if bad.contains(&c) || (c as u32) < 0x20 { '-' } else { c })
        .collect();
    let stem = clean.trim().trim_matches('-');
    let mut stem: String = stem.chars().take(40).collect();
    if stem.is_empty() {
        stem.push_str("note");
    }
    format!("{stem}-{id}.md")
}

/// Minimal percent-encoder for the `filename*=UTF-8''…` parameter so
/// non-ASCII titles survive intermediaries that mangle Content-Disposition.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

async fn delete_note_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db_state(&s)?;
    let n = db
        .delete_note(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest("note not found".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<u32>,
}

async fn search_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Query(qs): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let top_k = qs.limit.unwrap_or(8).min(50) as usize;
    // Task 5 wires ?space= query param; for now pass None (export-all spaces).
    let hits = crate::search::semantic_search(&s.db_path, &auth.user.id, &s.embedder, &qs.q, top_k, None)
        .await
        .map_err(|e| ApiError::Internal(format!("search: {e}")))?;
    Ok(Json(json!({ "count": hits.len(), "hits": hits })))
}

// ───── chat (one-shot agent loop) ─────

#[derive(Deserialize)]
struct ChatReq {
    message: String,
    #[serde(default)]
    history: Vec<ChatTurn>,
}

#[derive(Deserialize)]
struct ChatTurn {
    role: String,
    text: String,
}

async fn chat_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<ChatReq>,
) -> Result<Json<Value>, ApiError> {
    if req.message.trim().is_empty() {
        return Err(ApiError::BadRequest("message empty".into()));
    }
    // Use a profile with user_id + db_path so tools see the right scope.
    let mut profile = harness_core::UserProfile::default();
    profile.extra.insert(
        "user_id".into(),
        serde_json::Value::String(auth.user.id.clone()),
    );
    profile.extra.insert(
        "db_path".into(),
        serde_json::Value::String(s.db_path.to_string_lossy().into_owned()),
    );
    profile.extra.insert(
        "tier".into(),
        serde_json::Value::String(auth.user.tier.clone()),
    );
    // Default space for one-shot chat path (Task 5 wires per-session space).
    profile.extra.insert("space".into(), serde_json::Value::String("life".into()));
    // Plant the user's tz so `current_time` resolves "今天" / "今天" / "this week"
    // in their local clock. Defaults to system tz if unset.
    if let Some(tz) = &s.user_tz {
        profile.tz = Some(tz.clone());
    }
    // Flag that an embedder is available (tools read the slot, not this).
    profile
        .extra
        .insert("__embedder_slot".into(), serde_json::Value::Bool(true));

    let mut world = harness_context::with_profile(".", profile);

    let model = crate::AnyModelHandle(s.model.clone());
    let tools = harness_core::iter_macro_tools();
    let mut loop_ = AgentLoop::new(model);
    for t in tools {
        loop_ = loop_.with_tool(t);
    }
    let loop_ = loop_.with_guide(Arc::new(SystemPromptGuide));

    let task_desc = build_task_description(&req.message, &req.history, "life");
    let task = Task {
        description: task_desc,
        source: None,
        deadline: None,
    };
    let outcome = loop_
        .run_with_max_iters(task, &mut world, s.max_iters)
        .await
        .map_err(|e| ApiError::Internal(format!("agent: {e}")))?;
    let (reply, iters, ok, usage) = match outcome {
        Outcome::Done { text, iters, usage, .. } => (text.unwrap_or_default(), iters, true, usage),
        Outcome::BudgetExhausted { iters, last_text, usage, .. } => (
            last_text.unwrap_or_else(|| "(budget exhausted)".into()),
            iters,
            false,
            usage,
        ),
    };
    if let Ok(db) = open_db_state(&s) {
        let _ = db.insert_audit(
            Some(&auth.user.id),
            "chat_message",
            None,
            Some(&json!({"iters": iters, "ok": ok}).to_string()),
            usage.input_tokens as i64,
            usage.output_tokens as i64,
        );
    }
    Ok(Json(json!({ "reply": reply, "iters": iters, "ok": ok })))
}

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

struct SystemPromptGuide;

#[async_trait::async_trait]
impl harness_core::Guide for SystemPromptGuide {
    fn id(&self) -> &harness_core::GuideId {
        static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        ID.get_or_init(|| "ai-note/system-prompt".to_string())
    }
    fn kind(&self) -> harness_core::Execution {
        harness_core::Execution::Inferential
    }
    fn scope(&self) -> &harness_core::GuideScope {
        static S: std::sync::OnceLock<harness_core::GuideScope> = std::sync::OnceLock::new();
        S.get_or_init(|| harness_core::GuideScope::Always)
    }
    async fn apply(
        &self,
        ctx: &mut harness_core::Context,
        _world: &harness_core::World,
    ) -> Result<(), harness_core::GuideError> {
        ctx.system.push(Block::Text(SYSTEM_PROMPT.to_string()));
        Ok(())
    }
}

const SYSTEM_PROMPT: &str = "\
You are a personal note-taking assistant. The user types natural-language \
messages describing what they want to capture, recall, edit, or delete.\n\
\n\
Hard rules:\n\
1. **Time awareness.** Whenever the user uses a relative date — 今天 / 昨天 / 前天 / 上周 / \
   上个月 / 去年 / today / yesterday / last week / last month / last year / next Friday / etc — \
   call `current_time` FIRST and compute the window from its `iso_local` + `timezone` fields. \
   Never guess what \"today\" is.\n\
   - For \"今天\" / \"today\": since = local midnight today (in user TZ → UTC), until = now.\n\
   - For \"昨天\" / \"yesterday\": [local midnight yesterday, local midnight today).\n\
   - For \"上周\" / \"last week\": [local midnight last Monday, local midnight this Monday).\n\
   - For \"上个月\" / \"last month\": [first day of last month 00:00 local, first day of this month 00:00 local).\n\
   - For \"去年\" / \"last year\": [Jan 1 last year 00:00 local, Jan 1 this year 00:00 local).\n\
   Pass those as RFC3339 UTC to `list_recent_notes`'s `since`/`until` args.\n\
2. When the user gives you a thought, observation, idea, todo, or anything \
   they want remembered, call `create_note` IMMEDIATELY. Don't ask for a title — \
   pick a short 4-15 char one yourself from the content. Pull tags out if natural \
   (e.g. \"work\", \"idea\", \"book\", \"todo\", \"reading\").\n\
3. When the user is searching / recalling without a time bound (\"did I write about X\" / \
   \"关于 X 的笔记\" / \"find my note on Y\" / \"what did I say about Z\"), call `search_notes` \
   with their query verbatim. Pass the user's full phrasing, NOT just keywords — the \
   embedding model handles paraphrasing.\n\
4. When the user combines a topic AND a time bound (\"上周关于 X 的笔记\" / \"yesterday's todos\"), \
   first compute the date window per rule 1, then call `list_recent_notes` with `since`/`until`. \
   If the topic still needs disambiguating after that, also call `search_notes` and intersect.\n\
5. When the user wants an overview (\"what have I been writing\", \"recent notes\"), call \
   `list_recent_notes` with no date filter.\n\
6. For `update_note` and `delete_note` you MUST first surface the matching id via \
   search_notes / list_recent_notes, then confirm with the user before mutating.\n\
7. NEVER summarise what the note will say back to the user before storing it. Store \
   first, then briefly confirm with a one-line ack (\"已记录 · 4 条今日想法\"). The user's \
   words are the canonical record; don't paraphrase them away.\n\
8. Notes are private to the user. No third-party leakage in your replies.\n\
9. **Trial limit.** If `create_note` returns `{error: \"trial_limit\", used, limit, hint}`, \
   don't retry — instead tell the user in plain Chinese that they've hit the {limit}-note \
   cap (state the exact number), suggest deleting an old note to make room (offer to \
   `list_recent_notes` so they can pick), and mention the paid upgrade path. Don't \
   apologise effusively; one sentence + actionable choices is enough.\n\
10. **Space scope.** Every note operation is scoped to the user's current \
   space, given on a `[system] space: work|life` line at the top of the task. \
   New notes go in that space; searches and listings only see that space. \
   Never move a note across spaces unless the user explicitly asks.\n\
";

fn random_user_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}
