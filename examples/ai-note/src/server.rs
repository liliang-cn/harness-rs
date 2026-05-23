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

fn open_db_state(s: &AppState) -> Result<Db, ApiError> {
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
        .route("/api/notes/search", get(search_handler))
        .route("/api/chat", post(chat_handler))
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
        .ok_or_else(|| ApiError::Unauthorized(AuthError::BadCredentials.to_string()))?;
    if !verify_password(&req.password, &user.password_hash) {
        return Err(ApiError::Unauthorized(AuthError::BadCredentials.to_string()));
    }
    let session = new_session(&user.id);
    db.insert_session(&session)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "token": session.token, "user": &user })))
}

async fn logout_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let _ = auth.user.id;
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
    let notes = db
        .list_recent_notes(&auth.user.id, q.limit.unwrap_or(50).min(500))
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
    let note = db
        .create_note(&auth.user.id, &req.title, &req.body, &req.tags)
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
    let hits = crate::search::semantic_search(&s.db_path, &auth.user.id, &s.embedder, &qs.q, top_k)
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

    let task_desc = build_task_description(&req.message, &req.history);
    let task = Task {
        description: task_desc,
        source: None,
        deadline: None,
    };
    let outcome = loop_
        .run_with_max_iters(task, &mut world, s.max_iters)
        .await
        .map_err(|e| ApiError::Internal(format!("agent: {e}")))?;
    let (reply, iters, ok) = match outcome {
        Outcome::Done { text, iters, .. } => (text.unwrap_or_default(), iters, true),
        Outcome::BudgetExhausted { iters, last_text, .. } => (
            last_text.unwrap_or_else(|| "(budget exhausted)".into()),
            iters,
            false,
        ),
    };
    Ok(Json(json!({ "reply": reply, "iters": iters, "ok": ok })))
}

fn build_task_description(message: &str, history: &[ChatTurn]) -> String {
    if history.is_empty() {
        return message.to_string();
    }
    let mut s = String::new();
    s.push_str("--- conversation so far ---\n");
    for t in history.iter().take(20) {
        s.push_str(&format!("[{}] {}\n", t.role, t.text));
    }
    s.push_str("\n--- new message ---\n");
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
";

fn random_user_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}
