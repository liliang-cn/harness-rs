use crate::auth::{
    AuthCtx, AuthError, Invite, User, hash_password, is_trial, new_session,
    random_invite_code, random_user_id, validate_email, verify_password,
};
use crate::db::{Db, today_year_month};
use crate::portfolio::model::build_positions;
use crate::portfolio::quotes;
use crate::tools::ledger_path;
use crate::{SYSTEM_PROMPT, build_task_description_with_lang, collect_tools};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, State},
    http::StatusCode,
    response::{Html, Sse, sse::Event as SseEvent, sse::KeepAlive},
    routing::{get, post},
};
use chrono::{TimeZone, Utc};
use futures::stream::Stream;
use harness::prelude::*;
use harness_context::with_profile;
use harness_core::{Event, Hook, HookOutcome, UserProfile, World as CoreWorld};
use harness_loop::{AgentLoop, Outcome, ProfileGuide};
use harness_permissions::{PermissionHook, PermissionMode, PermissionRules};
use harness_tools_tasks::{TaskStore, make_tools as make_task_tools};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;

/// Vite-built admin SPA, embedded into the binary so deploys stay
/// single-artifact. Built by `cd admin-ui && npm run build`.
static ADMIN_DIST: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/admin-ui/dist");

/// Vite-built user-facing SPA (dashboard, ledger, portfolio, profile,
/// chat, login). The new SPA fully replaces the old hand-written
/// `index.html` — there is no `/legacy/` mount anymore. Built by
/// `cd user-ui && npm run build`.
static USER_UI_DIST: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/user-ui/dist");

/// Domain-specific guidance prepended to `MemorySynthesizer`'s prompt. Tells
/// the synth model what counts as a durable fact in the personal-accounting
/// context — preferences, habits, repeated patterns — and what to skip:
/// individual transactions (already in the `transactions` table) and PII
/// (GuardedMemory will block these post-hoc, but it's cleaner to not emit
/// them in the first place).
const LEDGER_MEMORY_INSTRUCTIONS: &str = "\
This is a personal-accounting + investment-tracking agent. Single \
transactions ('用户花了 ¥199 火锅', 'user bought 100 AAPL') are ALREADY \
stored in the transactions/trades tables — DO NOT re-store them as memory \
facts; that's noise.\n\
\n\
ONLY emit facts in these categories:\n\
- **stable preferences**: payment habits ('用户偏好用微信支付餐饮'), \
  category-naming conventions, report-format preferences\n\
- **repeated patterns** (≥2 mentions in transcript or implied long-term): \
  '用户每月有 Claude Code Max 订阅' (the subscription tool already covers \
   this, but a higher-level pattern like '用户偏好按月订阅 SaaS 而非买断' \
   is genuinely durable)\n\
- **long-term decisions**: investment policies, budget philosophies, \
  account-naming schemes\n\
\n\
NEVER emit facts containing: specific amounts (¥X, USD X, account balances), \
account numbers, email addresses, phone numbers, addresses. If a fact \
requires citing a specific number to make sense, it's transient — skip it.\n\
\n\
If the session was just routine logging with no observable preference, \
return [].\
";

/// One row in the model picker. `available=false` rows render greyed-out
/// so the user knows why a model isn't selectable (server missing the key).
#[derive(Clone, Debug, Serialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub available: bool,
}

/// Admin-mutable runtime config. Kept inside `AppState` behind an `RwLock`
/// so PATCH /api/admin/config can swap keys/models without a restart.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub default_model_id: String,
    pub available_models: Vec<ModelOption>,
    pub deepseek_key: Option<String>,
    pub gemini_key: Option<String>,
    /// Per-model token pricing card. Persisted as JSON under provider_config
    /// key `pricing_rate_card`; edited via PATCH /api/admin/config.
    pub pricing: crate::pricing::RateCard,
}

#[derive(Clone)]
pub struct AppState {
    pub profile: UserProfile,
    pub max_iters: u32,
    /// Shared task store. Per-user filtering lives in the tools themselves
    /// (they pick up `world.profile.extra["user_id"]`).
    pub task_store: Arc<dyn TaskStore>,
    /// Hot-reloadable provider config. Read briefly; never hold the guard
    /// across an `.await`.
    pub config: Arc<std::sync::RwLock<AppConfig>>,
    /// Active embedder for note semantic search. Also stashed in
    /// `embed_slot` so the background embed worker + tools can reach it.
    pub embedder: std::sync::Arc<dyn harness_core::Embedder>,
}

impl AppConfig {
    /// `available_models` flag derives from key presence, so recompute it
    /// after any credential change.
    pub fn refresh_availability(&mut self) {
        for m in &mut self.available_models {
            m.available = match m.provider.as_str() {
                "deepseek" => self.deepseek_key.is_some(),
                "gemini" => self.gemini_key.is_some(),
                _ => false,
            };
        }
        // If the current default became unavailable, pick the first
        // available; otherwise leave it.
        if !self
            .available_models
            .iter()
            .any(|m| m.id == self.default_model_id && m.available)
            && let Some(first) = self.available_models.iter().find(|m| m.available)
        {
            self.default_model_id = first.id.clone();
        }
    }
}

impl AppState {
    /// Snapshot the config under a brief read-lock. Callers should NOT hold
    /// the guard across an `.await`.
    pub fn cfg(&self) -> AppConfig {
        self.config.read().expect("AppConfig RwLock poisoned").clone()
    }

    /// Resolve a model id to the AnyModel adapter, picking the right
    /// credential by provider. Returns `Err(reason)` if the model id isn't
    /// recognised or the corresponding credential is missing.
    pub fn build_model_for(&self, model_id: &str) -> Result<crate::AnyModel, String> {
        let cfg = self.cfg();
        let opt = cfg
            .available_models
            .iter()
            .find(|m| m.id == model_id)
            .ok_or_else(|| format!("unknown model `{model_id}`"))?;
        if !opt.available {
            return Err(format!("model `{model_id}` is configured but missing API key"));
        }
        match opt.provider.as_str() {
            "deepseek" => {
                let key = cfg
                    .deepseek_key
                    .clone()
                    .ok_or_else(|| "DEEPSEEK_API_KEY not set on server".to_string())?;
                Ok(crate::AnyModel::OpenAi(harness_models::OpenAiCompat::with_key(
                    harness_models::providers::DEEPSEEK.to_string(),
                    model_id,
                    key,
                )))
            }
            "gemini" => {
                let key = cfg
                    .gemini_key
                    .clone()
                    .ok_or_else(|| "GEMINI_API_KEY not set on server".to_string())?;
                Ok(crate::AnyModel::Gemini(
                    harness_models::GeminiNative::with_key(model_id, key)
                        .with_search_grounding(true),
                ))
            }
            other => Err(format!("unsupported provider `{other}`")),
        }
    }

    /// Effective model id for a user — their preference if valid + available,
    /// else the server default. Trial users always get the default
    /// (per-user preference is ignored).
    pub fn effective_model_for(&self, user: &User) -> String {
        let cfg = self.cfg();
        if user.tier == "trial" {
            return cfg.default_model_id.clone();
        }
        let want = match user.preferred_model.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => return cfg.default_model_id.clone(),
        };
        if cfg
            .available_models
            .iter()
            .any(|m| m.id == want && m.available)
        {
            want.to_string()
        } else {
            cfg.default_model_id.clone()
        }
    }
}

impl AppState {
    /// Token → User. Touches `last_seen_at`. Used by `AuthCtx` extractor.
    pub fn resolve_session(&self, token: &str) -> Result<User, String> {
        let db = open_db().map_err(|_| "db".to_string())?;
        let session = db
            .get_session(token)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "invalid or expired session".to_string())?;
        if session.expires_at < Utc::now() {
            let _ = db.delete_session(token);
            return Err("session expired".into());
        }
        let user = db
            .get_user_by_id(&session.user_id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "user not found".to_string())?;
        let _ = db.touch_session(token, Utc::now());
        Ok(user)
    }
}

pub async fn serve(state: AppState, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    // Sanity check: tool registry must be populated. We touch it once at startup.
    let _ = collect_tools();

    let app = Router::new()
        // ─ public
        // New user-facing SPA at site root (vite+react+antd+i18next).
        // SPA fallback under /*ui_path so /login etc. serve the same HTML.
        .route("/", get(serve_user_ui_index))
        .route("/assets/*path", get(serve_user_ui_asset))
        .route("/favicon.svg", get(serve_user_ui_asset_root))
        .route("/icons.svg", get(serve_user_ui_asset_root))
        .route("/login", get(serve_user_ui_index))
        // SPA client-side routes: /ledger, /portfolio, /profile, /app, /...
        // Direct GETs (refresh / bookmark / share) must return index.html so
        // React Router can resolve the path. We enumerate each to avoid
        // colliding with the explicit /api, /admin, /assets,
        // /robots.txt, /sitemap.xml, /llms*.txt routes above.
        .route("/ledger", get(serve_user_ui_index))
        .route("/portfolio", get(serve_user_ui_index))
        .route("/profile", get(serve_user_ui_index))
        .route("/app", get(serve_user_ui_index))
        .route("/app/", get(serve_user_ui_index))
        .route("/app/*rest", get(serve_user_ui_index))
        // SEO + GEO static text. robots/sitemap for Google/Bing;
        // llms.txt + llms-full.txt for ChatGPT/Claude/Perplexity per
        // the llmstxt.org convention.
        .route("/robots.txt", get(crate::seo::robots))
        .route("/sitemap.xml", get(crate::seo::sitemap))
        .route("/llms.txt", get(crate::seo::llms))
        .route("/llms-full.txt", get(crate::seo::llms_full))
        // Admin SPA: GET /admin → index.html; GET /admin/* → matching asset
        // from the bundled dist, with SPA fallback to index.html.
        .route("/admin", get(serve_admin_index))
        .route("/admin/", get(serve_admin_index))
        .route("/admin/*path", get(serve_admin_asset))
        .route("/api/info", get(info_handler))
        .route("/api/register", post(register_handler))
        .route("/api/login", post(login_handler))
        // ─ protected (AuthCtx extractor on each handler)
        .route("/api/logout", post(logout_handler))
        .route("/api/me", get(me_handler))
        .route("/api/me/invites", get(list_invites_handler).post(create_invite_handler))
        .route("/api/me/password", post(change_password_handler))
        .route("/api/me/model", post(set_model_handler))
        .route(
            "/api/me/memories",
            get(list_memories_handler).delete(delete_all_memories_handler),
        )
        .route("/api/me/memories/:id", axum::routing::delete(delete_memory_handler))
        .route("/api/accounts", get(accounts_handler))
        .route("/api/transactions", get(transactions_handler))
        .route("/api/report", get(report_handler))
        .route("/api/budgets", get(budgets_handler))
        .route("/api/subscriptions", get(subscriptions_handler))
        .route("/api/subscriptions/:id/cancel", post(subscription_cancel_handler))
        .route("/api/voice/transcribe", post(transcribe_handler))
        .route("/api/me/export/transactions.csv", get(export_transactions_csv))
        .route("/api/me/export/trades.csv", get(export_trades_csv))
        .route("/api/me/export/subscriptions.csv", get(export_subscriptions_csv))
        .route("/api/chat", post(chat_handler))
        .route("/api/chat/stream", post(chat_stream_handler))
        // Receipt / PDF uploads from the chat composer. The upload route gets
        // its own 20 MB body-limit override (axum default is 2 MB, which
        // breaks for any modern phone receipt photo). serve is auth-gated and
        // scoped to the caller's user_id.
        .route(
            "/api/chat/attachments",
            post(crate::attachments::upload_handler)
                .layer(DefaultBodyLimit::max(20 * 1024 * 1024)),
        )
        .route(
            "/api/chat/attachments/:id",
            get(crate::attachments::serve_handler),
        )
        // Session-aware chat: each conversation is persisted in the DB so
        // the user can leave a session and return to continue.
        .route("/api/chat/sessions", get(list_chat_sessions_handler).post(create_chat_session_handler))
        .route("/api/chat/sessions/:id", get(get_chat_session_handler).delete(delete_chat_session_handler))
        .route("/api/chat/sessions/:id/stream", post(session_stream_handler))
        .route("/api/brief", post(brief_handler))
        .route("/api/portfolio/assets", get(portfolio_assets_handler))
        .route("/api/portfolio/trades", get(portfolio_trades_handler))
        .route("/api/portfolio/positions", get(portfolio_positions_handler))
        // Net-worth dashboard endpoints. /net-worth returns the latest
        // snapshot (or recomputes on the fly if none exists yet);
        // /series feeds the trend chart; /refresh forces a recompute
        // for users who just added an account and want immediate feedback.
        .route("/api/me/net-worth", get(net_worth_handler))
        .route("/api/me/net-worth/series", get(net_worth_series_handler))
        .route("/api/me/net-worth/refresh", post(net_worth_refresh_handler))
        .route("/api/me/base-currency", post(set_base_currency_handler))
        .route("/api/me/loans", get(list_loans_handler))
        .route("/api/me/loans/:id/retire", post(retire_loan_handler))
        .route("/api/portfolio/summary", get(portfolio_summary_handler))
        .route("/api/portfolio/allocation", get(portfolio_allocation_handler))
        .route("/api/portfolio/refresh-prices", post(portfolio_refresh_handler))
        // ─ projects
        .route("/api/projects", get(list_projects_handler).post(create_project_handler))
        .route(
            "/api/projects/:id",
            get(get_project_handler)
                .patch(update_project_handler)
                .delete(delete_project_handler),
        )
        .route("/api/projects/:id/reviews", post(add_project_review_handler))
        // ─ notes
        .route("/api/notes", get(list_notes_handler).post(create_note_handler))
        .route(
            "/api/notes/:id",
            get(get_note_handler)
                .patch(update_note_handler)
                .delete(delete_note_handler),
        )
        .route("/api/notes/:id/export.md", get(export_note_md_handler))
        .route("/api/notes/export.zip", get(export_all_zip_handler))
        .route("/api/notes/search", get(search_notes_handler));

    // Mount admin endpoints — all gated by `require_admin` in the handlers.
    let app = crate::admin::register_routes(app)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("→ listening on http://{}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

// ── user-facing SPA serve ──

async fn serve_user_ui_index() -> impl axum::response::IntoResponse {
    use axum::http::header;
    let body = USER_UI_DIST
        .get_file("index.html")
        .and_then(|f| f.contents_utf8())
        .unwrap_or("<h1>user-ui not built — run `cd user-ui && npm run build`</h1>");
    (
        [(header::CACHE_CONTROL, "no-cache, must-revalidate")],
        Html(body),
    )
}

async fn serve_user_ui_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::header;
    use axum::response::IntoResponse;
    let full = format!("assets/{path}");
    if let Some(file) = USER_UI_DIST.get_file(&full) {
        let mime = mime_for(&full);
        return (
            [
                (header::CONTENT_TYPE, mime),
                // Vite hashes filenames in /assets, long-cache.
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable",
                ),
            ],
            Body::from(file.contents()),
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, "asset not found").into_response()
}

/// Serves `/favicon.svg`, `/icons.svg`, etc — files at the top of `dist/`
/// rather than under `dist/assets/`. Looks at the request path stripped of
/// the leading `/` and bails if not found.
async fn serve_user_ui_asset_root(
    req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::header;
    use axum::response::IntoResponse;
    let path = req.uri().path().trim_start_matches('/');
    if let Some(file) = USER_UI_DIST.get_file(path) {
        let mime = mime_for(path);
        return (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "public, max-age=3600"),
            ],
            Body::from(file.contents()),
        )
            .into_response();
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
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
    // SPA fallback: client-side route, return index.html so the React app
    // can resolve it.
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

async fn info_handler(State(s): State<AppState>) -> Json<Value> {
    // Public endpoint — shown by the auth overlay before login. Return the
    // catalogue so the model picker can render even pre-login.
    let cfg = s.cfg();
    let default_provider = cfg
        .available_models
        .iter()
        .find(|m| m.id == cfg.default_model_id)
        .map(|m| m.provider.clone())
        .unwrap_or_default();
    Json(json!({
        "provider": default_provider,
        "model": cfg.default_model_id,
        "default_model_id": cfg.default_model_id,
        "available_models": cfg.available_models,
    }))
}

// ───── auth handlers ─────

#[derive(Deserialize)]
struct RegisterReq {
    email: String,
    password: String,
    #[serde(default)]
    invite_code: Option<String>,
}

#[derive(Deserialize)]
struct LoginReq {
    email: String,
    password: String,
}

async fn register_handler(Json(req): Json<RegisterReq>) -> Result<Json<Value>, ApiError> {
    validate_email(&req.email).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let db = open_db()?;
    if db
        .get_user_by_email(&req.email)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .is_some()
    {
        return Err(ApiError::BadRequest(AuthError::EmailExists.to_string()));
    }
    let pw_hash = hash_password(&req.password)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    // Bootstrap: very first registered user becomes admin (no invite needed).
    let total_users = db
        .count_users()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let (tier, invited_by, invite_used) = if total_users == 0 {
        ("admin".to_string(), None, None)
    } else {
        // Invite: empty/None → trial; valid+available → paid; provided-but-invalid → 400.
        match req
            .invite_code
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
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
    let user = User {
        id: random_user_id(),
        email: req.email.trim().to_string(),
        password_hash: pw_hash,
        tier,
        invited_by,
        invite_code_used: invite_used,
        created_at: Utc::now(),
        preferred_model: None,
        // Default to USD; users change it later in profile. The DB column
        // has the same default so this is belt-and-braces.
        base_currency: "USD".into(),
    };
    db.insert_user(&user)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let s = new_session(&user.id);
    db.insert_session(&s)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let _ = db.insert_audit(
        Some(&user.id),
        "register",
        None,
        Some(&json!({"email": user.email, "tier": user.tier}).to_string()),
        0,
        0,
    );
    Ok(Json(json!({
        "token": s.token,
        "user": &user,
    })))
}

async fn login_handler(Json(req): Json<LoginReq>) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let user = db
        .get_user_by_email(&req.email)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| {
            let _ = open_db().map(|db| {
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
    let s = new_session(&user.id);
    db.insert_session(&s)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let _ = db.insert_audit(Some(&user.id), "login", None, None, 0, 0);
    Ok(Json(json!({ "token": s.token, "user": &user })))
}

async fn logout_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    // Token isn't in AuthCtx; we just rely on session expiry. For an explicit
    // logout, the client also needs to discard the token. Best-effort: drop ALL
    // sessions for this user when called.
    let _ = open_db().map(|db| db.insert_audit(Some(&auth.user.id), "logout", None, None, 0, 0));
    Ok(Json(json!({"ok": true})))
}

async fn me_handler(State(s): State<AppState>, auth: AuthCtx) -> Json<Value> {
    let effective = s.effective_model_for(&auth.user);
    Json(json!({
        "user": auth.user,
        "effective_model_id": effective,
    }))
}

#[derive(Deserialize)]
struct SetModelReq {
    /// Either a model id from `/api/info.available_models[].id`, or null
    /// to clear the preference (falls back to server default).
    model: Option<String>,
}

async fn set_model_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<SetModelReq>,
) -> Result<Json<Value>, ApiError> {
    if auth.user.tier == "trial" {
        return Err(ApiError::Forbidden(
            "trial 账户使用默认模型；升级到 paid 后可自选模型".into(),
        ));
    }
    if let Some(want) = req.model.as_deref() {
        let cfg = s.cfg();
        let ok = cfg
            .available_models
            .iter()
            .any(|m| m.id == want && m.available);
        if !ok {
            return Err(ApiError::BadRequest(format!(
                "model `{want}` not in available_models or missing API key"
            )));
        }
    }
    let db = open_db()?;
    db.set_user_preferred_model(&auth.user.id, req.model.as_deref())
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // Echo back the now-effective model (after the update).
    let mut user_after = auth.user.clone();
    user_after.preferred_model = req.model.clone();
    let effective = s.effective_model_for(&user_after);
    Ok(Json(json!({
        "preferred_model": req.model,
        "effective_model_id": effective,
    })))
}

#[derive(Deserialize)]
struct ChangePasswordReq {
    old_password: String,
    new_password: String,
}

async fn change_password_handler(
    auth: AuthCtx,
    Json(req): Json<ChangePasswordReq>,
) -> Result<Json<Value>, ApiError> {
    if !verify_password(&req.old_password, &auth.user.password_hash) {
        return Err(ApiError::Unauthorized("当前密码不正确".into()));
    }
    if req.new_password == req.old_password {
        return Err(ApiError::BadRequest("新密码不能跟旧密码相同".into()));
    }
    let new_hash =
        hash_password(&req.new_password).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let db = open_db()?;
    db.update_user_password(&auth.user.id, &new_hash)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // Kick every other device out; current session stays alive.
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
    Ok(Json(json!({
        "ok": true,
        "other_sessions_dropped": dropped,
    })))
}

async fn list_invites_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    if is_trial(&auth.user.tier) {
        return Err(ApiError::Forbidden(
            "trial users can't invite — get a paid account first".into(),
        ));
    }
    let db = open_db()?;
    let invites = db
        .list_invites_by_creator(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"invites": invites})))
}

async fn create_invite_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    if is_trial(&auth.user.tier) {
        return Err(ApiError::Forbidden(
            "trial users can't invite — get a paid account first".into(),
        ));
    }
    let db = open_db()?;
    let inv = Invite {
        code: random_invite_code(),
        created_by: auth.user.id.clone(),
        uses_remaining: 1,
        expires_at: None,
        created_at: Utc::now(),
    };
    db.insert_invite(&inv)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"invite": inv})))
}

// ───── business handlers ─────

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<u64>,
}

async fn accounts_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let accs = db
        .list_accounts(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"count": accs.len(), "accounts": accs})))
}

async fn transactions_handler(
    auth: AuthCtx,
    Query(q): Query<LimitQuery>,
) -> Result<Json<Value>, ApiError> {
    let limit = q.limit.unwrap_or(50).min(500) as usize;
    let now = chrono::Utc::now();
    let from = now - chrono::Duration::days(365);
    let db = open_db()?;
    let mut all = db
        .list_transactions(&auth.user.id, from, now, None, None)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let total = all.len();
    all.truncate(limit);
    Ok(Json(json!({
        "total_matched": total,
        "returned": all.len(),
        "transactions": all,
    })))
}

#[derive(Deserialize)]
struct MonthQuery {
    year: Option<i32>,
    month: Option<u32>,
}

async fn report_handler(
    auth: AuthCtx,
    Query(q): Query<MonthQuery>,
) -> Result<Json<Value>, ApiError> {
    let (cy, cm) = today_year_month();
    let year = q.year.unwrap_or(cy);
    let month = q.month.unwrap_or(cm);
    let db = open_db()?;
    let totals = db
        .monthly_totals(&auth.user.id, year, month)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut grand: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for t in &totals {
        let cur = t.currency.clone();
        let prev = grand
            .get(&cur)
            .and_then(|s| rust_decimal::Decimal::from_str_exact(s).ok())
            .unwrap_or(rust_decimal::Decimal::ZERO);
        grand.insert(cur, (prev + t.total).to_string());
    }
    Ok(Json(json!({
        "year": year,
        "month": month,
        "by_category": totals,
        "grand_total_by_currency": grand,
    })))
}

async fn budgets_handler(
    auth: AuthCtx,
    Query(q): Query<MonthQuery>,
) -> Result<Json<Value>, ApiError> {
    let (cy, cm) = today_year_month();
    let year = q.year.unwrap_or(cy);
    let month = q.month.unwrap_or(cm);
    let db = open_db()?;
    let statuses = db
        .budget_status(&auth.user.id, year, month)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let over = statuses.iter().filter(|s| s.over_budget).count();
    Ok(Json(json!({
        "year": year,
        "month": month,
        "budgets": statuses,
        "over_count": over,
    })))
}

async fn subscriptions_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let subs = db
        .list_subscriptions(&auth.user.id, true)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // Roughly normalise to "monthly burn" per currency — purely informational,
    // no FX conversion, just `amount × periods_per_month`.
    use std::collections::HashMap;
    let mut monthly: HashMap<String, rust_decimal::Decimal> = HashMap::new();
    for s in &subs {
        let per_month = match s.frequency {
            crate::model::Frequency::Weekly => s.amount * rust_decimal::Decimal::new(43, 1), // 4.3
            crate::model::Frequency::Monthly => s.amount,
            crate::model::Frequency::Quarterly => s.amount / rust_decimal::Decimal::from(3),
            crate::model::Frequency::Yearly => s.amount / rust_decimal::Decimal::from(12),
        };
        *monthly.entry(s.currency.clone()).or_insert(rust_decimal::Decimal::ZERO) += per_month;
    }
    let monthly_str: serde_json::Map<String, Value> = monthly
        .into_iter()
        .map(|(k, v)| (k, json!(v.round_dp(2).to_string())))
        .collect();
    Ok(Json(json!({
        "count": subs.len(),
        "subscriptions": subs,
        "monthly_burn_by_currency": monthly_str,
    })))
}

async fn subscription_cancel_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let n = db
        .cancel_subscription(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest(format!("no active subscription `{id}`")));
    }
    Ok(Json(json!({"cancelled": id})))
}

// ───── voice → text (Gemini audio inlineData) ─────
//
// Client (web / miniprogram) uploads a small audio clip as base64.
// We hand it to Gemini's generateContent with a "transcribe this" prompt
// and return the plain text. Used by the 小程序 long-press mic flow —
// in the web we already have Web Speech API in-browser, but this is the
// fallback path if it lands there too.

#[derive(Deserialize)]
struct TranscribeReq {
    audio_base64: String,
    /// e.g. "audio/mp3", "audio/wav", "audio/webm". Defaults to mp3.
    #[serde(default)]
    mime_type: Option<String>,
}

async fn transcribe_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<TranscribeReq>,
) -> Result<Json<Value>, ApiError> {
    let cfg = s.cfg();
    let key = cfg
        .gemini_key
        .clone()
        .ok_or_else(|| ApiError::Internal("GEMINI_API_KEY not configured".into()))?;
    let mime = req.mime_type.as_deref().unwrap_or("audio/mp3");
    // Strip the small base64 max length cap — Gemini accepts up to ~20MB
    // inline. Empirically 30-second m4a is well under that.
    if req.audio_base64.is_empty() {
        return Err(ApiError::BadRequest("audio_base64 is empty".into()));
    }

    let body = json!({
        "contents": [{
            "parts": [
                {"text": "请把这段录音转写成文字。直接返回文字本身，不要加引号或者其他说明。如果是中文就返回中文，英文就返回英文，混合则按实际语言混合。"},
                {"inlineData": { "mimeType": mime, "data": req.audio_base64 }}
            ]
        }],
        "generationConfig": {
            "temperature": 0.0,
            "maxOutputTokens": 2048
        }
    });

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.5-flash:generateContent?key={key}"
    );
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| ApiError::Internal(format!("gemini request: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| ApiError::Internal(format!("gemini body: {e}")))?;
    if !status.is_success() {
        return Err(ApiError::Internal(format!(
            "gemini HTTP {status}: {}",
            text.chars().take(400).collect::<String>()
        )));
    }
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| ApiError::Internal(format!("gemini json: {e}")))?;
    let transcript = v
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    // Audit it; tokens_in/out unknown from this call shape.
    if let Ok(db) = open_db() {
        let _ = db.insert_audit(
            Some(&auth.user.id),
            "transcribe",
            None,
            Some(&json!({"chars": transcript.chars().count()}).to_string()),
            0,
            0,
        );
    }

    Ok(Json(json!({ "text": transcript })))
}

// ───── CSV exports ─────
//
// Three endpoints, one per domain (transactions, trades, subscriptions).
// UTF-8 BOM is prepended so Excel/Numbers on macOS opens Chinese columns
// without the "garbled mojibake" first-launch experience. account_id and
// asset_id columns are joined to human-readable names via in-memory maps
// so the CSV is useful without consulting the app.

fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

fn csv_line(cols: &[&str]) -> String {
    let mut s = String::with_capacity(cols.iter().map(|c| c.len() + 1).sum::<usize>() + 1);
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&csv_escape(c));
    }
    s.push('\n');
    s
}

fn csv_response(body: String, filename: &str) -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    // BOM so Excel auto-detects UTF-8.
    let mut payload = String::from("\u{FEFF}");
    payload.push_str(&body);
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        payload,
    )
        .into_response()
}

async fn export_transactions_csv(auth: AuthCtx) -> Result<axum::response::Response, ApiError> {
    let db = open_db()?;
    let accounts = db
        .list_accounts(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let name_by_id: std::collections::HashMap<String, String> = accounts
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();

    // All time. SQLite handles RFC3339 string comparison sensibly between
    // these bounds.
    let from = chrono::Utc.with_ymd_and_hms(1970, 1, 1, 0, 0, 0).unwrap();
    let to = chrono::Utc.with_ymd_and_hms(2999, 12, 31, 23, 59, 59).unwrap();
    let txns = db
        .list_transactions(&auth.user.id, from, to, None, None)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut body = String::new();
    body.push_str(&csv_line(&[
        "id",
        "类型",
        "金额",
        "币种",
        "账户",
        "对方账户",
        "分类",
        "备注",
        "发生时间",
        "创建时间",
    ]));
    for t in &txns {
        let kind = match t.kind {
            crate::model::TxnKind::Expense => "支出",
            crate::model::TxnKind::Income => "收入",
            crate::model::TxnKind::Transfer => "转账",
        };
        let acct = name_by_id
            .get(&t.account_id)
            .cloned()
            .unwrap_or_else(|| t.account_id.clone());
        let counter = t
            .counter_account_id
            .as_ref()
            .and_then(|id| name_by_id.get(id).cloned().or(Some(id.clone())))
            .unwrap_or_default();
        body.push_str(&csv_line(&[
            &t.id,
            kind,
            &t.amount.to_string(),
            &t.currency,
            &acct,
            &counter,
            t.category.as_deref().unwrap_or(""),
            t.note.as_deref().unwrap_or(""),
            &t.occurred_at.to_rfc3339(),
            &t.created_at.to_rfc3339(),
        ]));
    }

    let _ = db.insert_audit(
        Some(&auth.user.id),
        "export",
        Some("transactions"),
        Some(&json!({"rows": txns.len()}).to_string()),
        0,
        0,
    );

    let stamp = chrono::Utc::now().format("%Y%m%d");
    Ok(csv_response(body, &format!("transactions-{stamp}.csv")))
}

async fn export_trades_csv(auth: AuthCtx) -> Result<axum::response::Response, ApiError> {
    let db = open_db()?;
    let assets = db
        .list_assets(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let asset_by_id: std::collections::HashMap<String, crate::portfolio::model::Asset> = assets
        .into_iter()
        .map(|a| (a.id.clone(), a))
        .collect();

    let trades = db
        .all_trades(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut body = String::new();
    body.push_str(&csv_line(&[
        "id",
        "标的",
        "标的名称",
        "类型",
        "数量",
        "单价",
        "币种",
        "手续费",
        "金额合计",
        "交易时间",
        "备注",
        "创建时间",
    ]));
    for tr in &trades {
        let symbol = asset_by_id
            .get(&tr.asset_id)
            .map(|a| a.symbol.clone())
            .unwrap_or_else(|| tr.asset_id.clone());
        let asset_name = asset_by_id
            .get(&tr.asset_id)
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let kind = match tr.kind {
            crate::portfolio::model::TradeKind::Buy => "买入",
            crate::portfolio::model::TradeKind::Sell => "卖出",
            crate::portfolio::model::TradeKind::Opening => "建仓基线",
        };
        let total = (tr.qty * tr.price_per_unit + tr.fees).to_string();
        body.push_str(&csv_line(&[
            &tr.id,
            &symbol,
            &asset_name,
            kind,
            &tr.qty.to_string(),
            &tr.price_per_unit.to_string(),
            &tr.currency,
            &tr.fees.to_string(),
            &total,
            &tr.occurred_at.to_rfc3339(),
            tr.note.as_deref().unwrap_or(""),
            &tr.created_at.to_rfc3339(),
        ]));
    }

    let _ = db.insert_audit(
        Some(&auth.user.id),
        "export",
        Some("trades"),
        Some(&json!({"rows": trades.len()}).to_string()),
        0,
        0,
    );

    let stamp = chrono::Utc::now().format("%Y%m%d");
    Ok(csv_response(body, &format!("trades-{stamp}.csv")))
}

async fn export_subscriptions_csv(auth: AuthCtx) -> Result<axum::response::Response, ApiError> {
    let db = open_db()?;
    let accounts = db
        .list_accounts(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let name_by_id: std::collections::HashMap<String, String> = accounts
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();

    // Include cancelled rows too — the user is asking for an export, not a
    // dashboard view.
    let subs = db
        .list_subscriptions(&auth.user.id, false)
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut body = String::new();
    body.push_str(&csv_line(&[
        "id",
        "名称",
        "金额",
        "币种",
        "频率",
        "下次扣款",
        "扣款账户",
        "分类",
        "支付渠道",
        "备注",
        "状态",
        "创建时间",
        "取消时间",
    ]));
    for s in &subs {
        let freq = match s.frequency {
            crate::model::Frequency::Weekly => "每周",
            crate::model::Frequency::Monthly => "每月",
            crate::model::Frequency::Quarterly => "每季度",
            crate::model::Frequency::Yearly => "每年",
        };
        let acct = name_by_id
            .get(&s.account_id)
            .cloned()
            .unwrap_or_else(|| s.account_id.clone());
        body.push_str(&csv_line(&[
            &s.id,
            &s.name,
            &s.amount.to_string(),
            &s.currency,
            freq,
            &s.next_charge_date.to_string(),
            &acct,
            s.category.as_deref().unwrap_or(""),
            s.pay_channel.as_deref().unwrap_or(""),
            s.note.as_deref().unwrap_or(""),
            &s.status,
            &s.created_at.to_rfc3339(),
            s.cancelled_at.map(|d| d.to_rfc3339()).as_deref().unwrap_or(""),
        ]));
    }

    let _ = db.insert_audit(
        Some(&auth.user.id),
        "export",
        Some("subscriptions"),
        Some(&json!({"rows": subs.len()}).to_string()),
        0,
        0,
    );

    let stamp = chrono::Utc::now().format("%Y%m%d");
    Ok(csv_response(body, &format!("subscriptions-{stamp}.csv")))
}

#[derive(Deserialize)]
struct ChatMsg {
    role: String,
    text: String,
}

#[derive(Deserialize)]
struct ChatRequest {
    message: String,
    #[serde(default)]
    history: Vec<ChatMsg>,
    /// Optional BCP-47-ish locale ("en", "zh-CN", ...). Sets the agent's
    /// default reply language. If absent, the system prompt's mixed
    /// EN/ZH content lets the model match the user's input language.
    #[serde(default)]
    lang: Option<String>,
    /// IDs of `chat_attachments` rows the user attached to this turn.
    /// Planted on `profile.extra.attachment_ids` so the `extract_receipt`
    /// tool can resolve them. Ownership is verified inside the tool.
    #[serde(default)]
    attachment_ids: Vec<String>,
}

async fn chat_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<ChatRequest>,
) -> Result<Json<Value>, ApiError> {
    if req.message.trim().is_empty() {
        return Err(ApiError::BadRequest("message must not be empty".into()));
    }
    let history: Vec<(String, String)> = req
        .history
        .into_iter()
        .map(|m| (m.role, m.text))
        .collect();
    let task_description = build_task_description_with_lang(
        &req.message,
        &history,
        req.lang.as_deref(),
        &req.attachment_ids,
    );
    let _ = SYSTEM_PROMPT;

    let model_id = s.effective_model_for(&auth.user);
    let model = s.build_model_for(&model_id).map_err(ApiError::Internal)?;
    let mut all_tools = collect_tools();
    all_tools.extend(make_task_tools(s.task_store.clone()));
    let mut loop_ = AgentLoop::new(model)
        .with_guide(Arc::new(ProfileGuide))
        .with_hook(Arc::new(permission_hook_for_tier(&auth.user.tier, &all_tools)));
    if let Ok(g) = crate::SkillsCatalogueGuide::new() {
        loop_ = loop_.with_guide(Arc::new(g));
    }
    for t in all_tools {
        loop_ = loop_.with_tool(t);
    }
    let mut profile = s.profile.clone();
    profile
        .extra
        .insert("user_id".into(), serde_json::Value::String(auth.user.id.clone()));
    profile
        .extra
        .insert("tier".into(), serde_json::Value::String(auth.user.tier.clone()));
    if !req.attachment_ids.is_empty() {
        profile.extra.insert(
            "attachment_ids".into(),
            serde_json::Value::Array(
                req.attachment_ids
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    let mut world = with_profile(".", profile);
    let task = Task {
        description: task_description,
        source: None,
        deadline: None,
    };
    match loop_.run_with_max_iters(task, &mut world, s.max_iters).await {
        Ok(Outcome::Done { text, iters, .. }) => Ok(Json(json!({
            "reply": text.unwrap_or_default(),
            "iters": iters,
            "ok": true,
        }))),
        Ok(Outcome::BudgetExhausted {
            iters, last_text, ..
        }) => Ok(Json(json!({
            "reply": last_text.unwrap_or_else(|| "(budget exhausted, no synthesis)".into()),
            "iters": iters,
            "ok": false,
            "warning": "budget_exhausted",
        }))),
        Err(e) => Err(ApiError::Internal(format!("agent: {e}"))),
    }
}

// ─── /api/brief — typed structured-output demo ────────────────────────────
//
// Demonstrates `AgentLoop::run_typed::<T>()` end-to-end:
// 1. `BriefReport` (below) carries `#[derive(JsonSchema)]` — schemars
//    auto-generates the JSON Schema at compile time.
// 2. `run_typed` installs that schema into `Context.response_format` for
//    one run, the model adapter forwards it to the provider on the wire
//    (Gemini `responseSchema`, DeepSeek `json_object` + prompt hint, etc.).
// 3. The model's terminal reply is JSON; we deserialise it into `BriefReport`
//    directly — UI consumes JSON instead of parsing markdown.

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BriefReport {
    pub year: i32,
    pub month: u32,
    /// Total spend per currency this month. Values are decimal strings, e.g. "1234.56".
    pub total_by_currency: Vec<CurrencyTotal>,
    /// Top 3 spend categories this month, in descending order.
    pub top_categories: Vec<CategoryEntry>,
    /// Categories exceeding their monthly budget. May be empty.
    pub over_budget: Vec<OverBudgetEntry>,
    /// One short observation (week-on-week trend, unusual category, etc.).
    /// Empty string if nothing notable.
    pub observation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CurrencyTotal {
    pub currency: String,
    pub total: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CategoryEntry {
    pub category: String,
    pub currency: String,
    pub total: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OverBudgetEntry {
    pub category: String,
    pub currency: String,
    pub used: String,
    pub limit: String,
    pub over_by: String,
}

const BRIEF_TYPED_PROMPT: &str = "\
Compose this user's monthly money brief as STRUCTURED JSON. Steps:\n\
1. Call `current_time` to anchor the year/month.\n\
2. Call `monthly_report` for the current month.\n\
3. Call `check_budgets` for the current month.\n\
4. Fill out a BriefReport object with:\n\
   • year, month (numbers, e.g. 2026 and 5)\n\
   • total_by_currency: one entry per currency, total as decimal string\n\
   • top_categories: at most 3 entries, descending by total\n\
   • over_budget: only categories with used > limit; over_by = used - limit\n\
   • observation: one short Chinese/English sentence — pick something \
     notable (largest jump, unusual category, near-budget). \"\" if nothing.\n\
5. Reply with ONLY the JSON object — no markdown fences, no prose.";

async fn brief_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<BriefReport>, ApiError> {
    let model_id = s.effective_model_for(&auth.user);
    let model = s.build_model_for(&model_id).map_err(ApiError::Internal)?;
    let mut all_tools = collect_tools();
    all_tools.extend(make_task_tools(s.task_store.clone()));
    let mut loop_ = AgentLoop::new(model)
        .with_guide(Arc::new(ProfileGuide))
        .with_hook(Arc::new(permission_hook_for_tier(&auth.user.tier, &all_tools)));
    if let Ok(g) = crate::SkillsCatalogueGuide::new() {
        loop_ = loop_.with_guide(Arc::new(g));
    }
    for t in all_tools {
        loop_ = loop_.with_tool(t);
    }
    let mut profile = s.profile.clone();
    profile
        .extra
        .insert("user_id".into(), serde_json::Value::String(auth.user.id.clone()));
    profile
        .extra
        .insert("tier".into(), serde_json::Value::String(auth.user.tier.clone()));
    let mut world = with_profile(".", profile);
    let task = Task {
        description: BRIEF_TYPED_PROMPT.into(),
        source: None,
        deadline: None,
    };
    let report: BriefReport = loop_
        .run_typed_with_max_iters::<BriefReport>(task, &mut world, s.max_iters)
        .await
        .map_err(|e| ApiError::Internal(format!("brief: {e}")))?;
    Ok(Json(report))
}

/// Per-tier permission policy. Returned as a ready-to-attach Hook.
///
/// - **trial** → `PermissionMode::Plan` with an allowlist of write tools
///   (CRUD for accounts / txns / portfolio + refresh_prices). Everything
///   destructive that isn't allowlisted (the `delete_*` family,
///   `apply_category_merge`) is denied for trial agents — keeps untrusted
///   chat sessions from silently deleting state. UI buttons that hit DELETE
///   endpoints directly still work; this hook only gates LLM-driven calls.
/// - **paid / admin / anything else** → `PermissionMode::Default` (no
///   additional gating beyond existing soft quotas + sandbox).
fn permission_hook_for_tier(
    tier: &str,
    tools: &[Arc<dyn harness_core::Tool>],
) -> PermissionHook {
    if tier == "trial" {
        let mut rules = PermissionRules::new(PermissionMode::Plan).with_tools(tools);
        for name in [
            "add_account",
            "log_transaction",
            "record_transfer",
            "set_budget",
            "add_asset",
            "record_trade",
            "update_price",
            "refresh_prices",
            "add_subscription",
            "record_subscription_charge",
        ] {
            rules = rules.allow(name);
        }
        PermissionHook::new(rules)
    } else {
        PermissionHook::new(PermissionRules::new(PermissionMode::Default))
    }
}

pub(crate) fn open_db() -> Result<Db, ApiError> {
    let p = ledger_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ApiError::Internal(e.to_string()))?;
    }
    Db::open(&p).map_err(|e| ApiError::Internal(format!("db: {e}")))
}

/// Hook that forwards a curated subset of lifecycle events into an mpsc
/// channel so the SSE stream can show live progress.
struct ChannelHook {
    tx: mpsc::UnboundedSender<Value>,
}

impl Hook for ChannelHook {
    fn name(&self) -> &str {
        "sse_channel"
    }
    fn matches(&self, _ev: &Event<'_>) -> bool {
        true
    }
    fn fire(&self, ev: &Event<'_>, _world: &mut CoreWorld) -> HookOutcome {
        let payload: Option<Value> = match ev {
            Event::Heartbeat { iter } => Some(json!({"type": "iter", "iter": iter})),
            Event::PreToolUse { action } => Some(json!({
                "type": "tool_start",
                "name": action.tool,
                "args": &action.args,
            })),
            Event::PostToolUse { action, result } => {
                let mut preview = result.content.clone();
                let s = serde_json::to_string(&preview).unwrap_or_default();
                if s.len() > 280 {
                    preview = json!(format!("{}…", &s[..280]));
                }
                Some(json!({
                    "type": "tool_end",
                    "name": action.tool,
                    "ok": result.ok,
                    "preview": preview,
                }))
            }
            Event::PostModel { out } => {
                if let Some(text) = &out.text {
                    if !text.is_empty() {
                        return {
                            let _ = self.tx.send(json!({"type":"thought","text": text}));
                            HookOutcome::Allow
                        };
                    }
                }
                None
            }
            Event::ModelTokenDelta { text } => {
                if !text.is_empty() {
                    let _ = self.tx.send(json!({"type": "token", "text": text}));
                }
                None
            }
            Event::Error { message } => Some(json!({"type": "error", "message": message})),
            _ => None,
        };
        if let Some(v) = payload {
            let _ = self.tx.send(v);
        }
        HookOutcome::Allow
    }
}

// ─── memory inspection (AI 记得我什么) ───────────────────────────────────

async fn list_memories_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let path = memory_path_for(&auth.user.id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(ApiError::Internal(format!("memory read: {e}"))),
    };
    let mut entries: Vec<serde_json::Value> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            entries.push(v);
        }
    }
    // Newest first.
    entries.sort_by(|a, b| {
        b.get("created_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .cmp(&a.get("created_ms").and_then(|v| v.as_i64()).unwrap_or(0))
    });
    Ok(Json(json!({"count": entries.len(), "memories": entries})))
}

async fn delete_all_memories_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let path = memory_path_for(&auth.user.id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(Json(json!({"deleted": 0}))),
    };
    let n = raw.lines().filter(|l| !l.trim().is_empty()).count() as u32;
    std::fs::write(&path, "").map_err(|e| ApiError::Internal(format!("write: {e}")))?;
    Ok(Json(json!({"deleted": n})))
}

async fn delete_memory_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let path = memory_path_for(&auth.user.id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Err(ApiError::BadRequest("no memories file".into())),
    };
    let mut kept: Vec<String> = Vec::new();
    let mut removed = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let entry_id = serde_json::from_str::<serde_json::Value>(trimmed)
            .ok()
            .and_then(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
            .unwrap_or_default();
        if entry_id == id {
            removed = true;
            continue;
        }
        kept.push(line.to_string());
    }
    if !removed {
        return Err(ApiError::BadRequest(format!("no memory `{id}`")));
    }
    let mut new_content = kept.join("\n");
    if !new_content.is_empty() {
        new_content.push('\n');
    }
    std::fs::write(&path, new_content).map_err(|e| ApiError::Internal(format!("write: {e}")))?;
    Ok(Json(json!({"deleted": id})))
}

// ─── persisted chat sessions ─────────────────────────────────────────────

async fn create_chat_session_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<Value>, ApiError> {
    let id = random_session_id();
    let model = s.effective_model_for(&auth.user);
    let db = open_db()?;
    db.create_chat_session(&auth.user.id, &id, Some(&model))
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let sess = db
        .get_chat_session(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::Internal("session vanished after insert".into()))?;
    Ok(Json(json!({ "session": sess })))
}

async fn list_chat_sessions_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let sessions = db
        .list_chat_sessions(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "count": sessions.len(), "sessions": sessions })))
}

async fn get_chat_session_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
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
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let n = db
        .delete_chat_session(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest(format!("no session `{id}`")));
    }
    Ok(Json(json!({ "deleted": id })))
}

fn random_session_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 6];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Per-user JSONL path for `harness-core::Memory`. The framework's default
/// FileMemory impl reads + appends to this file; one file per user gives
/// strict isolation without the trait needing to know about users.
pub(crate) fn memory_path_for(user_id: &str) -> std::path::PathBuf {
    let base = ledger_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("memory").join(format!("{user_id}.jsonl"))
}

#[derive(Deserialize)]
struct SessionStreamReq {
    message: String,
    /// BCP-47 locale for default reply language (mirrors ChatRequest.lang).
    #[serde(default)]
    lang: Option<String>,
    /// IDs of `chat_attachments` rows the user attached to this turn.
    /// Mirrors ChatRequest.attachment_ids — planted on profile.extra so
    /// the `extract_receipt` tool can resolve them.
    #[serde(default)]
    attachment_ids: Vec<String>,
}

/// Per-session streaming chat handler — replaces the old session-less
/// `chat_stream_handler` (which is kept around as a fallback for the
/// previous UI build). Builds history from the DB, persists the user's
/// message synchronously, and saves the assistant's final reply when the
/// stream completes.
async fn session_stream_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Json(req): Json<SessionStreamReq>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    if req.message.trim().is_empty() {
        return Err(ApiError::BadRequest("message must not be empty".into()));
    }
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    let _ = SYSTEM_PROMPT;

    let db = open_db()?;
    // Validate the session belongs to the user before doing any work.
    let _ = db
        .get_chat_session(&auth.user.id, &session_id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest(format!("no session `{session_id}`")))?;

    // Persist the user message NOW (so a network hiccup mid-stream still
    // leaves the transcript intact). Also makes `message_count` + `title`
    // updates land before any reply is computed.
    db.append_chat_message(
        &auth.user.id,
        &session_id,
        "user",
        &req.message,
        None,
        &req.attachment_ids,
        None,
    )
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Build agent history from the persisted message log — last 40 turns
    // is plenty given the compactor will further squash later.
    let history_msgs = db
        .get_chat_messages(&auth.user.id, &session_id, 80)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let history: Vec<(String, String)> = history_msgs
        .iter()
        // Skip the just-appended user message; the loop appends task.description for it.
        .filter(|m| !(m.role == "user" && m.text == req.message))
        .map(|m| (m.role.clone(), m.text.clone()))
        .collect();
    let task_desc = build_task_description_with_lang(
        &req.message,
        &history,
        req.lang.as_deref(),
        &req.attachment_ids,
    );
    drop(db);

    let user_id = auth.user.id.clone();
    let user_tier = auth.user.tier.clone();
    let model_id = s.effective_model_for(&auth.user);
    let tx_for_done = tx.clone();
    let session_id_for_task = session_id.clone();
    let user_id_for_task = user_id.clone();
    let model_id_for_task = model_id.clone();
    // Move attachment_ids into the spawned future so the extract_receipt
    // tool can see them on profile.extra below.
    let attachment_ids = req.attachment_ids.clone();

    tokio::spawn(async move {
        let model = match s.build_model_for(&model_id_for_task) {
            Ok(m) => m,
            Err(reason) => {
                let _ = tx_for_done.send(json!({"type": "error", "message": reason}));
                let _ = tx_for_done.send(json!({"type": "done", "ok": false, "iters": 0, "reply": ""}));
                return;
            }
        };
        let mut all_tools = collect_tools();
        all_tools.extend(make_task_tools(s.task_store.clone()));
        let mut loop_ = AgentLoop::new(model)
            .with_streaming(true)
            .with_guide(Arc::new(ProfileGuide))
            .with_hook(Arc::new(permission_hook_for_tier(&user_tier, &all_tools)));
        if let Ok(g) = crate::SkillsCatalogueGuide::new() {
            loop_ = loop_.with_guide(Arc::new(g));
        }

        // ─── Long-term memory: per-user FileMemory + write-time guards ───
        // Each user gets their own JSONL — strict file-level isolation
        // (`harness-core::Memory` trait knows nothing about users; we
        // partition by giving each user a different file).
        //
        // `GuardedMemory` adds (a) dedup against near-identical existing
        // entries, (b) regex blocklist for sensitive content (credit cards,
        // emails, ¥/$ amounts — which belong in the txns table not memory).
        let mem_path = memory_path_for(&user_id_for_task);
        if let Ok(file_mem) = harness_context::FileMemory::open(&mem_path) {
            let file_arc = Arc::new(file_mem);
            let guarded: Arc<dyn harness_core::Memory> = Arc::new(
                harness_context::GuardedMemory::new(file_arc.clone())
                    .with_dedup_threshold(0.6),
            );
            loop_ = loop_.with_guide(Arc::new(
                harness_loop::MemoryGuide::new(guarded.clone())
                    .with_top_k(5)
                    // Drop entries with weak keyword overlap so chit-chat
                    // doesn't pull in unrelated facts.
                    .with_min_score(0.25)
                    // synth-raw = fallback when distill failed to parse JSON;
                    // those are noisy. transient = anything an app explicitly
                    // tagged as ephemeral.
                    .with_excluded_tags(["synth-raw", "transient"]),
            ));
            // Three LLM-facing memory tools wired to this user's store.
            // remember_this lets the user explicitly say "记住 X" and
            // bypass synth's judgment; list/forget surface + clean up.
            loop_ = loop_
                .with_tool(Arc::new(harness_tools_memory::RememberThisTool::with_source(
                    guarded.clone(),
                    format!("ai-ledger/user-{user_id_for_task}/explicit"),
                )))
                .with_tool(Arc::new(harness_tools_memory::ListMemoriesTool::new(
                    guarded.clone(),
                )))
                .with_tool(Arc::new(harness_tools_memory::ForgetMemoryTool::new(
                    file_arc.clone() as Arc<dyn harness_tools_memory::MemoryDelete>,
                )));
            // Synth model: prefer deepseek-v4-flash for cheapness; if it's
            // not configured, skip the synthesizer entirely (chat still
            // works, just no auto-distillation).
            if let Ok(synth_model) = s.build_model_for("deepseek-v4-flash") {
                let synth_arc: Arc<dyn harness_core::Model> = Arc::new(synth_model);
                loop_ = loop_.with_hook(Arc::new(
                    harness_loop::MemorySynthesizer::new(guarded.clone(), synth_arc)
                        .with_source(format!("ai-ledger/user-{}", user_id_for_task))
                        .with_max_facts(3)
                        .with_extra_instructions(LEDGER_MEMORY_INSTRUCTIONS),
                ));
            }
        } else {
            tracing::warn!(path = %mem_path.display(), "memory open failed; chat will not persist facts");
        }

        for t in all_tools {
            loop_ = loop_.with_tool(t);
        }
        loop_ = loop_.with_hook(Arc::new(ChannelHook { tx: tx.clone() }));
        let mut profile = s.profile.clone();
        profile.extra.insert("user_id".into(), serde_json::Value::String(user_id_for_task.clone()));
        profile.extra.insert("tier".into(), serde_json::Value::String(user_tier.clone()));
        if !attachment_ids.is_empty() {
            profile.extra.insert(
                "attachment_ids".into(),
                serde_json::Value::Array(
                    attachment_ids
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        let mut world = with_profile(".", profile);
        let task = Task {
            description: task_desc,
            source: None,
            deadline: None,
        };
        let _ = tx_for_done.send(json!({"type": "start"}));
        match loop_.run_with_max_iters(task, &mut world, s.max_iters).await {
            Ok(Outcome::Done { text, iters, usage, .. }) => {
                let reply = text.unwrap_or_default();
                // Persist the assistant reply + update session model_id.
                if let Ok(db) = open_db() {
                    let _ = db.append_chat_message(
                        &user_id_for_task,
                        &session_id_for_task,
                        "asst",
                        &reply,
                        Some(iters),
                        &[],
                        None,
                    );
                    let _ = db.update_chat_session_model(
                        &user_id_for_task,
                        &session_id_for_task,
                        &model_id_for_task,
                    );
                    let _ = db.insert_audit(
                        Some(&user_id_for_task),
                        "chat_message",
                        Some(&session_id_for_task),
                        Some(&json!({"iters": iters, "model": &model_id_for_task}).to_string()),
                        usage.input_tokens as i64,
                        usage.output_tokens as i64,
                    );
                }
                let _ = tx_for_done.send(json!({
                    "type": "done", "ok": true, "iters": iters, "reply": reply,
                }));
            }
            Ok(Outcome::BudgetExhausted { iters, last_text, usage, .. }) => {
                let reply = last_text.unwrap_or_else(|| "(budget exhausted)".into());
                if let Ok(db) = open_db() {
                    let _ = db.append_chat_message(
                        &user_id_for_task,
                        &session_id_for_task,
                        "asst",
                        &reply,
                        Some(iters),
                        &[],
                        None,
                    );
                    let _ = db.insert_audit(
                        Some(&user_id_for_task),
                        "chat_message",
                        Some(&session_id_for_task),
                        Some(&json!({"iters": iters, "warning": "budget_exhausted"}).to_string()),
                        usage.input_tokens as i64,
                        usage.output_tokens as i64,
                    );
                }
                let _ = tx_for_done.send(json!({
                    "type": "done", "ok": false, "iters": iters,
                    "reply": reply, "warning": "budget_exhausted",
                }));
            }
            Err(e) => {
                let _ = tx_for_done.send(json!({
                    "type": "error", "message": format!("agent: {e}"),
                }));
                let _ = tx_for_done.send(json!({"type": "done", "ok": false, "iters": 0, "reply": ""}));
            }
        }
    });

    let stream = UnboundedReceiverStream::new(rx).map(|v| {
        Ok::<_, Infallible>(SseEvent::default().data(v.to_string()))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn chat_stream_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<ChatRequest>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    let _ = SYSTEM_PROMPT;

    let task_desc = build_task_description_with_lang(
        &req.message,
        &req.history
            .iter()
            .map(|m| (m.role.clone(), m.text.clone()))
            .collect::<Vec<_>>(),
        req.lang.as_deref(),
        &req.attachment_ids,
    );

    let user_id = auth.user.id.clone();
    let user_tier = auth.user.tier.clone();
    // Resolve the model BEFORE spawning so we can surface configuration
    // errors as a sync 500 instead of an SSE `error` event the client
    // might never reach.
    let model_id = s.effective_model_for(&auth.user);
    let tx_for_done = tx.clone();
    // Move attachment_ids into the spawned future so the extract_receipt
    // tool can see them on profile.extra below.
    let attachment_ids = req.attachment_ids.clone();
    tokio::spawn(async move {
        let model = match s.build_model_for(&model_id) {
            Ok(m) => m,
            Err(reason) => {
                let _ = tx_for_done.send(json!({"type": "error", "message": reason}));
                let _ = tx_for_done.send(json!({"type": "done", "ok": false, "iters": 0, "reply": ""}));
                return;
            }
        };
        let mut all_tools = collect_tools();
        all_tools.extend(make_task_tools(s.task_store.clone()));
        let mut loop_ = AgentLoop::new(model)
            .with_streaming(true)
            .with_guide(Arc::new(ProfileGuide))
            .with_hook(Arc::new(permission_hook_for_tier(&user_tier, &all_tools)));
        if let Ok(g) = crate::SkillsCatalogueGuide::new() {
            loop_ = loop_.with_guide(Arc::new(g));
        }
        for t in all_tools {
            loop_ = loop_.with_tool(t);
        }
        loop_ = loop_.with_hook(Arc::new(ChannelHook { tx: tx.clone() }));
        let mut profile = s.profile.clone();
        profile
            .extra
            .insert("user_id".into(), serde_json::Value::String(user_id.clone()));
        profile
            .extra
            .insert("tier".into(), serde_json::Value::String(user_tier));
        if !attachment_ids.is_empty() {
            profile.extra.insert(
                "attachment_ids".into(),
                serde_json::Value::Array(
                    attachment_ids
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
        }
        let mut world = with_profile(".", profile);
        let task = Task {
            description: task_desc,
            source: None,
            deadline: None,
        };
        let _ = tx_for_done.send(json!({"type": "start"}));
        match loop_.run_with_max_iters(task, &mut world, s.max_iters).await {
            Ok(Outcome::Done { text, iters, usage, .. }) => {
                let _ = tx_for_done.send(json!({
                    "type": "done",
                    "ok": true,
                    "iters": iters,
                    "reply": text.unwrap_or_default(),
                }));
                if let Ok(db) = open_db() {
                    let _ = db.insert_audit(
                        Some(&user_id),
                        "chat_message",
                        None,
                        Some(&json!({"iters": iters, "sessionless": true}).to_string()),
                        usage.input_tokens as i64,
                        usage.output_tokens as i64,
                    );
                }
            }
            Ok(Outcome::BudgetExhausted {
                iters, last_text, usage, ..
            }) => {
                let _ = tx_for_done.send(json!({
                    "type": "done",
                    "ok": false,
                    "iters": iters,
                    "reply": last_text.unwrap_or_else(|| "(budget exhausted)".into()),
                    "warning": "budget_exhausted",
                }));
                if let Ok(db) = open_db() {
                    let _ = db.insert_audit(
                        Some(&user_id),
                        "chat_message",
                        None,
                        Some(&json!({"iters": iters, "warning": "budget_exhausted", "sessionless": true}).to_string()),
                        usage.input_tokens as i64,
                        usage.output_tokens as i64,
                    );
                }
            }
            Err(e) => {
                let _ = tx_for_done.send(json!({
                    "type": "error",
                    "message": format!("agent: {e}"),
                }));
            }
        }
        // Drop sender → receiver completes → stream ends.
    });

    let stream = UnboundedReceiverStream::new(rx).map(|v| {
        let payload = serde_json::to_string(&v).unwrap_or_else(|_| "{}".into());
        Ok::<_, Infallible>(SseEvent::default().data(payload))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ───── portfolio handlers ─────

async fn portfolio_assets_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let assets = db
        .list_assets(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut enriched = Vec::with_capacity(assets.len());
    for a in &assets {
        let latest = db
            .latest_price(&auth.user.id, &a.id)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        enriched.push(json!({"asset": a, "latest_price": latest}));
    }
    Ok(Json(json!({"count": assets.len(), "assets": enriched})))
}

#[derive(Deserialize)]
struct TradesQuery {
    asset_symbol: Option<String>,
    limit: Option<u64>,
}

async fn portfolio_trades_handler(
    auth: AuthCtx,
    Query(q): Query<TradesQuery>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let limit = q.limit.unwrap_or(50).min(500) as usize;
    let asset_id = match &q.asset_symbol {
        Some(sym) => db
            .get_asset_by_symbol(&auth.user.id, sym)
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .map(|a| a.id),
        None => None,
    };
    let trades = db
        .list_trades(&auth.user.id, asset_id.as_deref(), limit)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"count": trades.len(), "trades": trades})))
}

fn positions_with_prices(
    db: &Db,
    user_id: &str,
) -> Result<Vec<crate::portfolio::model::Position>, ApiError> {
    use std::collections::HashMap;
    let assets = db
        .list_assets(user_id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let trades = db
        .all_trades(user_id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut prices: HashMap<String, crate::portfolio::model::PriceQuote> = HashMap::new();
    for a in &assets {
        if let Some(p) = db
            .latest_price(user_id, &a.id)
            .map_err(|e| ApiError::Internal(e.to_string()))?
        {
            prices.insert(a.id.clone(), p);
        }
    }
    Ok(build_positions(&assets, &trades, |aid| prices.get(aid).cloned()))
}

async fn portfolio_positions_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let positions = positions_with_prices(&db, &auth.user.id)?;
    Ok(Json(json!({"count": positions.len(), "positions": positions})))
}

// ───── net-worth dashboard ─────

/// Latest snapshot for the user. If no row exists yet (e.g. brand-new
/// account before the first cron fires), compute one inline so the
/// dashboard never shows a blank.
async fn net_worth_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let snap = match db.latest_net_worth_snapshot(&auth.user.id).map_err(api_err)? {
        Some(s) => s,
        None => crate::net_worth::snapshot_now(&db, &auth.user.id, &auth.user.base_currency)
            .map_err(|e| ApiError::Internal(e.to_string()))?,
    };
    Ok(Json(json!({"snapshot": snap})))
}

#[derive(serde::Deserialize)]
struct SeriesQuery {
    /// Inclusive ISO date (YYYY-MM-DD). Defaults to 12 months ago.
    from: Option<String>,
    /// Inclusive ISO date. Defaults to today.
    to: Option<String>,
}

async fn net_worth_series_handler(
    auth: AuthCtx,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let default_from = (Utc::now() - chrono::Duration::days(365))
        .format("%Y-%m-%d")
        .to_string();
    let from = q.from.unwrap_or(default_from);
    let to = q.to.unwrap_or(today);
    let series = db.net_worth_series(&auth.user.id, &from, &to).map_err(api_err)?;
    Ok(Json(json!({
        "from": from,
        "to": to,
        "count": series.len(),
        "series": series,
    })))
}

/// Force a recompute now. Mounted as POST so it can't be cached or
/// CSRF'd via a stray <img>. Used by the dashboard "refresh" button.
async fn net_worth_refresh_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let snap = crate::net_worth::snapshot_now(&db, &auth.user.id, &auth.user.base_currency)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"snapshot": snap})))
}

#[derive(serde::Deserialize)]
struct SetBaseCurrency {
    currency: String,
}

/// Change the user's display / aggregation currency. Snapshot is
/// recomputed immediately so the dashboard reflects the new unit on the
/// next reload.
async fn set_base_currency_handler(
    auth: AuthCtx,
    Json(req): Json<SetBaseCurrency>,
) -> Result<Json<Value>, ApiError> {
    let c = req.currency.trim().to_uppercase();
    if c.len() != 3 || !c.chars().all(|ch| ch.is_ascii_alphabetic()) {
        return Err(ApiError::BadRequest(
            "currency must be a 3-letter ISO code like USD".into(),
        ));
    }
    let db = open_db()?;
    db.set_user_base_currency(&auth.user.id, &c).map_err(api_err)?;
    let snap = crate::net_worth::snapshot_now(&db, &auth.user.id, &c)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({"ok": true, "base_currency": c, "snapshot": snap})))
}

// ─── loans ──────────────────────────────────────────────────────────────
//
// Joins each `loans` row with its `accounts` row + a few derived fields
// the UI needs (remaining principal, payoff progress, next-due date)
// so the dashboard doesn't have to fan out to N endpoints per loan.

async fn list_loans_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    // Default include_paid_off=true here so the UI can show retired loans
    // greyed-out; the agent tool defaults the other way (active-only).
    let out = crate::loans::summarise(&db, &auth.user.id, true).map_err(api_err)?;
    Ok(Json(json!({"loans": out})))
}

/// One-shot status flip for "I just paid this off". Ownership is checked
/// by looking up the loan under the authenticated user_id first; if the
/// row isn't theirs (or doesn't exist), we 400 — we don't disclose
/// whether another user owns the id.
async fn retire_loan_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let row = db
        .get_loan_by_account(&auth.user.id, &id)
        .map_err(api_err)?;
    if row.is_none() {
        return Err(ApiError::BadRequest("loan not found".into()));
    }
    db.set_loan_status(&id, "paid_off").map_err(api_err)?;
    Ok(Json(json!({"ok": true})))
}

fn api_err(e: rusqlite::Error) -> ApiError {
    ApiError::Internal(e.to_string())
}

async fn portfolio_summary_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    let db = open_db()?;
    let positions = positions_with_prices(&db, &auth.user.id)?;
    let mut value_by_currency: HashMap<String, Decimal> = HashMap::new();
    let mut realized_by_currency: HashMap<String, Decimal> = HashMap::new();
    let mut unrealized_by_currency: HashMap<String, Decimal> = HashMap::new();
    let mut value_by_class: HashMap<String, Decimal> = HashMap::new();
    let mut missing_prices = Vec::new();
    for p in &positions {
        if p.qty == Decimal::ZERO && p.realized_pl == Decimal::ZERO {
            continue;
        }
        *realized_by_currency
            .entry(p.currency.clone())
            .or_insert(Decimal::ZERO) += p.realized_pl;
        if let Some(mv) = p.market_value {
            *value_by_currency
                .entry(p.currency.clone())
                .or_insert(Decimal::ZERO) += mv;
            *value_by_class
                .entry(format!("{}/{}", p.asset_class.as_str(), p.currency))
                .or_insert(Decimal::ZERO) += mv;
        } else if p.qty > Decimal::ZERO {
            missing_prices.push(p.symbol.clone());
        }
        if let Some(upl) = p.unrealized_pl {
            *unrealized_by_currency
                .entry(p.currency.clone())
                .or_insert(Decimal::ZERO) += upl;
        }
    }
    let to_json = |m: HashMap<String, Decimal>| -> serde_json::Map<String, Value> {
        m.into_iter()
            .map(|(k, v)| (k, json!(v.to_string())))
            .collect()
    };
    Ok(Json(json!({
        "market_value_by_currency":  to_json(value_by_currency),
        "realized_pl_by_currency":   to_json(realized_by_currency),
        "unrealized_pl_by_currency": to_json(unrealized_by_currency),
        "market_value_by_class_currency": to_json(value_by_class),
        "missing_prices_for": missing_prices,
        "position_count": positions.iter().filter(|p| p.qty > rust_decimal::Decimal::ZERO).count(),
    })))
}

/// Allocation pie / bar feeder: positions bucketed by asset_class with
/// all market values converted to the user's `base_currency` using the
/// cached FX rates. The old portfolio_summary_handler kept positions
/// split per native currency which made cross-currency portfolios
/// (e.g. USD stocks + CNY gold) look catastrophically skewed when
/// plotted naively.
async fn portfolio_allocation_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    use rust_decimal::Decimal;
    use rust_decimal::prelude::ToPrimitive;
    use std::collections::HashMap;
    let db = open_db()?;
    let base = auth.user.base_currency.clone();
    let positions = positions_with_prices(&db, &auth.user.id)?;
    let mut by_class: HashMap<String, f64> = HashMap::new();
    let mut total: f64 = 0.0;
    let mut missing_rate_for: Vec<String> = Vec::new();
    for p in &positions {
        if p.qty <= Decimal::ZERO {
            continue;
        }
        let Some(mv) = p.market_value else { continue };
        let mv_native = mv.to_f64().unwrap_or(0.0);
        if mv_native <= 0.0 {
            continue;
        }
        // Convert to base. fx::convert returns Some(amount) when both
        // currencies are the same OR a cached rate exists; None if the
        // pair has never been fetched. We fall back to the native amount
        // and flag the position so the UI can show a warning.
        let converted = match crate::fx::convert(&db, mv_native, &p.currency, &base) {
            Ok(Some(v)) => v,
            _ => {
                missing_rate_for.push(format!("{}:{}", p.symbol, p.currency));
                mv_native // best-effort, still shows up in the right bucket
            }
        };
        let cls = p.asset_class.as_str().to_string();
        *by_class.entry(cls).or_insert(0.0) += converted;
        total += converted;
    }
    let mut rows: Vec<Value> = by_class
        .into_iter()
        .map(|(class, value)| {
            let pct = if total > 0.0 { (value / total) * 100.0 } else { 0.0 };
            json!({"class": class, "value": value, "pct": pct})
        })
        .collect();
    rows.sort_by(|a, b| {
        let av = a["value"].as_f64().unwrap_or(0.0);
        let bv = b["value"].as_f64().unwrap_or(0.0);
        bv.partial_cmp(&av).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(json!({
        "base_currency": base,
        "total": total,
        "by_class": rows,
        "missing_rate_for": missing_rate_for,
    })))
}

async fn portfolio_refresh_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let assets = db
        .list_assets(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let client = quotes::make_client();
    let mut report = Vec::with_capacity(assets.len());
    let mut ok_count = 0u32;
    for a in &assets {
        match quotes::fetch_price(&client, a).await {
            Ok(q) => {
                db.insert_price(&auth.user.id, &q)
                    .map_err(|e| ApiError::Internal(e.to_string()))?;
                ok_count += 1;
                report.push(json!({
                    "symbol": a.symbol,
                    "ok": true,
                    "price": q.price.to_string(),
                    "currency": q.currency,
                    "source": q.source,
                }));
            }
            Err(e) => report.push(json!({
                "symbol": a.symbol,
                "ok": false,
                "error": e.to_string(),
            })),
        }
    }
    Ok(Json(json!({
        "refreshed": ok_count,
        "total": assets.len(),
        "results": report,
    })))
}

// ───── projects REST ─────

#[derive(Deserialize)]
struct ProjectsQuery {
    /// "active" (default) | "due" | "all"
    filter: Option<String>,
}

async fn list_projects_handler(
    auth: AuthCtx,
    Query(q): Query<ProjectsQuery>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let (status, only_due) = match q.filter.as_deref() {
        Some("all") => (None, false),
        Some("due") => (Some("active"), true),
        _ => (Some("active"), false),
    };
    let projects = db
        .list_projects(&auth.user.id, status, only_due)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let due = db
        .count_due_projects(&auth.user.id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "projects": projects, "due_count": due })))
}

#[derive(Deserialize)]
struct CreateProjectReq {
    name: String,
    #[serde(default)]
    detail: String,
    parent_id: Option<String>,
    target_date: Option<String>,
    review_interval_days: Option<i64>,
}

async fn create_project_handler(
    auth: AuthCtx,
    Json(req): Json<CreateProjectReq>,
) -> Result<Json<Value>, ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name is empty".into()));
    }
    let db = open_db()?;
    let project = db
        .create_project(
            &auth.user.id,
            &req.name,
            &req.detail,
            req.parent_id.as_deref(),
            req.target_date.as_deref(),
            req.review_interval_days,
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "project": project })))
}

async fn get_project_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let project = db
        .get_project(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("project not found".into()))?;
    let milestones = db
        .list_milestones(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let reviews = db
        .list_project_reviews(&auth.user.id, &id, 100)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "project": project, "milestones": milestones, "reviews": reviews })))
}

#[derive(Deserialize)]
struct UpdateProjectReq {
    status: Option<String>,
    name: Option<String>,
    detail: Option<String>,
    target_date: Option<String>,
    review_interval_days: Option<i64>,
}

const VALID_PROJECT_STATUSES: &[&str] = &["active", "paused", "done", "dropped"];

async fn update_project_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<UpdateProjectReq>,
) -> Result<Json<Value>, ApiError> {
    if let Some(st) = req.status.as_deref() {
        if !VALID_PROJECT_STATUSES.contains(&st) {
            return Err(ApiError::BadRequest(format!(
                "status must be one of: {}",
                VALID_PROJECT_STATUSES.join(", ")
            )));
        }
    }
    let db = open_db()?;
    let n = db
        .update_project(
            &auth.user.id,
            &id,
            req.status.as_deref(),
            req.name.as_deref(),
            req.detail.as_deref(),
            req.target_date.as_deref(),
            req.review_interval_days,
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest("project not found".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

async fn delete_project_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let n = db
        .delete_project(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest("project not found".into()));
    }
    Ok(Json(json!({ "deleted": id })))
}

#[derive(Deserialize)]
struct AddProjectReviewReq {
    progress: String,
    #[serde(default)]
    next_steps: String,
    next_review_in_days: Option<i64>,
}

async fn add_project_review_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<AddProjectReviewReq>,
) -> Result<Json<Value>, ApiError> {
    if req.progress.trim().is_empty() {
        return Err(ApiError::BadRequest("progress is empty".into()));
    }
    let db = open_db()?;
    db.get_project(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("project not found".into()))?;
    let review = db
        .add_project_review(
            &auth.user.id,
            &id,
            &req.progress,
            &req.next_steps,
            req.next_review_in_days,
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "review": review })))
}

// ───── notes REST ─────

#[derive(Deserialize)]
struct NotesListQuery {
    project_id: Option<String>,
    limit: Option<u32>,
}

async fn list_notes_handler(
    auth: AuthCtx,
    Query(q): Query<NotesListQuery>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let notes = db
        .list_recent_notes(
            &auth.user.id,
            q.project_id.as_deref(),
            q.limit.unwrap_or(50).min(500),
        )
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
    project_id: Option<String>,
}

async fn create_note_handler(
    auth: AuthCtx,
    Json(req): Json<CreateNoteReq>,
) -> Result<Json<Value>, ApiError> {
    if req.body.trim().is_empty() {
        return Err(ApiError::BadRequest("body is empty".into()));
    }
    let db = open_db()?;
    let note = db
        .create_note(
            &auth.user.id,
            req.project_id.as_deref(),
            &req.title,
            &req.body,
            &req.tags,
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "note": note })))
}

async fn get_note_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
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
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<UpdateNoteReq>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let n = db
        .update_note(
            &auth.user.id,
            &id,
            req.title.as_deref(),
            req.body.as_deref(),
            req.tags.as_deref(),
        )
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest("note not found".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

async fn delete_note_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let db = open_db()?;
    let n = db
        .delete_note(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    if n == 0 {
        return Err(ApiError::BadRequest("note not found".into()));
    }
    Ok(Json(json!({ "ok": true })))
}

/// Export a single note as a markdown file with YAML front-matter.
async fn export_note_md_handler(
    auth: AuthCtx,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let db = open_db()?;
    let note = db
        .get_note(&auth.user.id, &id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("note not found".into()))?;

    let title_line = if note.title.trim().is_empty() {
        note.body.chars().take(32).collect::<String>()
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
    let project_yaml = note
        .project_id
        .as_ref()
        .map(|pid| format!("project_id: {pid}\n"))
        .unwrap_or_default();
    let body = format!(
        "---\n\
         id: {}\n\
         created_at: {}\n\
         updated_at: {}\n\
         {}{}\
         ---\n\
         \n\
         # {}\n\
         \n\
         {}\n",
        note.id,
        note.created_at.to_rfc3339(),
        note.updated_at.to_rfc3339(),
        project_yaml,
        tags_yaml,
        title_line,
        note.body,
    );

    let pretty = build_note_md_filename(&title_line, &note.id);
    let ascii_fallback = format!("note-{}.md", note.id);
    Ok((
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!(
                    "attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{}",
                    note_percent_encode(&pretty)
                ),
            ),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response())
}

/// Export every note the caller owns as a .zip archive.
async fn export_all_zip_handler(
    auth: AuthCtx,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse;
    use std::io::Write;

    let db = open_db()?;
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
    idx.push_str(&format!(
        "Exported {} notes for {}\n\n",
        notes.len(),
        auth.user.email
    ));
    idx.push_str("| Date | Title | Tags | File |\n|---|---|---|---|\n");

    let mut used_names = std::collections::HashSet::<String>::new();
    for note in &notes {
        let title_line = if note.title.trim().is_empty() {
            note.body.chars().take(32).collect::<String>()
        } else {
            note.title.clone()
        };
        let base = build_note_md_filename(&title_line, &note.id);
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
        let project_yaml = note
            .project_id
            .as_ref()
            .map(|pid| format!("project_id: {pid}\n"))
            .unwrap_or_default();
        let content = format!(
            "---\n\
             id: {}\n\
             created_at: {}\n\
             updated_at: {}\n\
             {}{}\
             ---\n\
             \n\
             # {}\n\
             \n\
             {}\n",
            note.id,
            note.created_at.to_rfc3339(),
            note.updated_at.to_rfc3339(),
            project_yaml,
            tags_yaml,
            title_line,
            note.body,
        );

        zip.start_file(&fname, opts)
            .map_err(|e| ApiError::Internal(format!("zip: {e}")))?;
        zip.write_all(content.as_bytes())
            .map_err(|e| ApiError::Internal(format!("zip write: {e}")))?;

        let tags_disp = if note.tags.is_empty() {
            "—".into()
        } else {
            note.tags.join(", ")
        };
        let date = note.created_at.format("%Y-%m-%d").to_string();
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

    let stamp = chrono::Utc::now().format("%Y%m%d").to_string();
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

#[derive(Deserialize)]
struct NotesSearchQuery {
    q: String,
    project_id: Option<String>,
    limit: Option<u32>,
}

async fn search_notes_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Query(qs): Query<NotesSearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let top_k = qs.limit.unwrap_or(8).min(50) as usize;
    let db_path = ledger_path();
    let hits = crate::search::semantic_search(
        &db_path,
        &auth.user.id,
        &s.embedder,
        &qs.q,
        top_k,
        qs.project_id.as_deref(),
    )
    .await
    .map_err(|e| ApiError::Internal(format!("search: {e}")))?;
    Ok(Json(json!({ "count": hits.len(), "hits": hits })))
}

fn build_note_md_filename(title: &str, id: &str) -> String {
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

fn note_percent_encode(s: &str) -> String {
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

// minimal error mapper → JSON + status
pub(crate) enum ApiError {
    BadRequest(String),
    #[allow(dead_code)]
    Unauthorized(String),
    Forbidden(String),
    Internal(String),
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            ApiError::BadRequest(s) => (StatusCode::BAD_REQUEST, s),
            ApiError::Unauthorized(s) => (StatusCode::UNAUTHORIZED, s),
            ApiError::Forbidden(s) => (StatusCode::FORBIDDEN, s),
            ApiError::Internal(s) => (StatusCode::INTERNAL_SERVER_ERROR, s),
        };
        (status, Json(json!({"error": msg}))).into_response()
    }
}
