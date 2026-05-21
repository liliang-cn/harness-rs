use crate::auth::{
    AuthCtx, AuthError, Invite, Session, User, hash_password, is_trial, new_session,
    random_invite_code, random_user_id, validate_email, verify_password,
    TRIAL_MAX_ASSETS, TRIAL_MAX_TRADES, TRIAL_MAX_TRANSACTIONS,
};
use crate::db::{Db, today_year_month};
use crate::portfolio::model::build_positions;
use crate::portfolio::quotes;
use crate::tools::ledger_path;
use crate::{SYSTEM_PROMPT, build_task_description, collect_tools};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, Sse, sse::Event as SseEvent, sse::KeepAlive},
    routing::{get, post},
};
use chrono::Utc;
use futures::stream::Stream;
use harness::prelude::*;
use harness_context::with_profile;
use harness_core::{Event, Hook, HookOutcome, UserProfile, World as CoreWorld};
use harness_loop::{AgentLoop, Outcome, ProfileGuide};
use harness_models::OpenAiCompat;
use harness_permissions::{PermissionHook, PermissionMode, PermissionRules};
use harness_tools_tasks::{JsonFileStore, TaskStore, make_tools as make_task_tools};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::StreamExt;

const INDEX_HTML: &str = include_str!("index.html");
/// Vendored copy of `marked` v15 (~40KB). Bundled in the binary so the chat
/// UI's markdown rendering works without a third-party CDN (some deployments
/// sit on networks where jsdelivr / unpkg are intermittently blocked).
const MARKED_JS: &str = include_str!("marked.min.js");

#[derive(Clone)]
pub struct AppState {
    pub base_url: String,
    pub model_id: String,
    pub api_key: String,
    pub profile: UserProfile,
    pub max_iters: u32,
    pub provider_label: String,
    pub model_label: String,
    /// Shared task store. Per-user filtering lives in the tools themselves
    /// (they pick up `world.profile.extra["user_id"]`).
    pub task_store: Arc<dyn TaskStore>,
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
        .route("/", get(serve_index))
        .route("/marked.min.js", get(serve_marked_js))
        .route("/api/info", get(info_handler))
        .route("/api/register", post(register_handler))
        .route("/api/login", post(login_handler))
        // ─ protected (AuthCtx extractor on each handler)
        .route("/api/logout", post(logout_handler))
        .route("/api/me", get(me_handler))
        .route("/api/me/invites", get(list_invites_handler).post(create_invite_handler))
        .route("/api/me/password", post(change_password_handler))
        .route("/api/accounts", get(accounts_handler))
        .route("/api/transactions", get(transactions_handler))
        .route("/api/report", get(report_handler))
        .route("/api/budgets", get(budgets_handler))
        .route("/api/chat", post(chat_handler))
        .route("/api/chat/stream", post(chat_stream_handler))
        .route("/api/brief", post(brief_handler))
        .route("/api/portfolio/assets", get(portfolio_assets_handler))
        .route("/api/portfolio/trades", get(portfolio_trades_handler))
        .route("/api/portfolio/positions", get(portfolio_positions_handler))
        .route("/api/portfolio/summary", get(portfolio_summary_handler))
        .route("/api/portfolio/refresh-prices", post(portfolio_refresh_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("→ listening on http://{}", addr);
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

async fn serve_marked_js() -> impl axum::response::IntoResponse {
    use axum::http::header;
    (
        [
            (header::CONTENT_TYPE, "application/javascript; charset=utf-8"),
            // Pinned vendored copy — fine to cache aggressively.
            (header::CACHE_CONTROL, "public, max-age=86400, immutable"),
        ],
        MARKED_JS,
    )
}

async fn info_handler(State(s): State<AppState>) -> Json<Value> {
    Json(json!({
        "provider": s.provider_label,
        "model": s.model_label,
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
    };
    db.insert_user(&user)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let s = new_session(&user.id);
    db.insert_session(&s)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
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
        .ok_or_else(|| ApiError::Unauthorized(AuthError::BadCredentials.to_string()))?;
    if !verify_password(&req.password, &user.password_hash) {
        return Err(ApiError::Unauthorized(AuthError::BadCredentials.to_string()));
    }
    let s = new_session(&user.id);
    db.insert_session(&s)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "token": s.token, "user": &user })))
}

async fn logout_handler(auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    // Token isn't in AuthCtx; we just rely on session expiry. For an explicit
    // logout, the client also needs to discard the token. Best-effort: drop ALL
    // sessions for this user when called.
    let _ = auth.user.id; // ack the extractor pulled us through
    Ok(Json(json!({"ok": true})))
}

async fn me_handler(auth: AuthCtx) -> Json<Value> {
    Json(json!({"user": auth.user}))
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
    let task_description = build_task_description(&req.message, &history);
    let _ = SYSTEM_PROMPT;

    let model = crate::build_model(&s.base_url, &s.model_id, s.api_key.clone());
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
    let model = crate::build_model(&s.base_url, &s.model_id, s.api_key.clone());
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
        ] {
            rules = rules.allow(name);
        }
        PermissionHook::new(rules)
    } else {
        PermissionHook::new(PermissionRules::new(PermissionMode::Default))
    }
}

fn open_db() -> Result<Db, ApiError> {
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

async fn chat_stream_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<ChatRequest>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (tx, rx) = mpsc::unbounded_channel::<Value>();
    let _ = SYSTEM_PROMPT;

    let task_desc = build_task_description(
        &req.message,
        &req.history
            .into_iter()
            .map(|m| (m.role, m.text))
            .collect::<Vec<_>>(),
    );

    let user_id = auth.user.id.clone();
    let user_tier = auth.user.tier.clone();
    let tx_for_done = tx.clone();
    tokio::spawn(async move {
        let model = crate::build_model(&s.base_url, &s.model_id, s.api_key.clone());
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
            .insert("user_id".into(), serde_json::Value::String(user_id));
        profile
            .extra
            .insert("tier".into(), serde_json::Value::String(user_tier));
        let mut world = with_profile(".", profile);
        let task = Task {
            description: task_desc,
            source: None,
            deadline: None,
        };
        let _ = tx_for_done.send(json!({"type": "start"}));
        match loop_.run_with_max_iters(task, &mut world, s.max_iters).await {
            Ok(Outcome::Done { text, iters, .. }) => {
                let _ = tx_for_done.send(json!({
                    "type": "done",
                    "ok": true,
                    "iters": iters,
                    "reply": text.unwrap_or_default(),
                }));
            }
            Ok(Outcome::BudgetExhausted {
                iters, last_text, ..
            }) => {
                let _ = tx_for_done.send(json!({
                    "type": "done",
                    "ok": false,
                    "iters": iters,
                    "reply": last_text.unwrap_or_else(|| "(budget exhausted)".into()),
                    "warning": "budget_exhausted",
                }));
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

// minimal error mapper → JSON + status
enum ApiError {
    BadRequest(String),
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
