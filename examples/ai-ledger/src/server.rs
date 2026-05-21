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
/// Vendored copy of `marked` v15 (~40KB). Bundled in the binary so the chat
/// UI's markdown rendering works without a third-party CDN (some deployments
/// sit on networks where jsdelivr / unpkg are intermittently blocked).
const MARKED_JS: &str = include_str!("marked.min.js");

/// One row in the model picker. `available=false` rows render greyed-out
/// so the user knows why a model isn't selectable (server missing the key).
#[derive(Clone, Debug, Serialize)]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub available: bool,
}

#[derive(Clone)]
pub struct AppState {
    /// Default model id when a user hasn't set a preference. Always one of
    /// `available_models` whose `available=true`.
    pub default_model_id: String,
    pub profile: UserProfile,
    pub max_iters: u32,
    /// Catalogue of models the UI can pick from.
    pub available_models: Vec<ModelOption>,
    /// Provider credentials. Both may be set; either may be `None` if the
    /// corresponding env var wasn't provided at startup.
    pub deepseek_key: Option<String>,
    pub gemini_key: Option<String>,
    /// Shared task store. Per-user filtering lives in the tools themselves
    /// (they pick up `world.profile.extra["user_id"]`).
    pub task_store: Arc<dyn TaskStore>,
}

impl AppState {
    /// Resolve a model id to the AnyModel adapter, picking the right
    /// credential by provider. Returns `Err(reason)` if the model id isn't
    /// recognised or the corresponding credential is missing.
    pub fn build_model_for(&self, model_id: &str) -> Result<crate::AnyModel, String> {
        let opt = self
            .available_models
            .iter()
            .find(|m| m.id == model_id)
            .ok_or_else(|| format!("unknown model `{model_id}`"))?;
        if !opt.available {
            return Err(format!("model `{model_id}` is configured but missing API key"));
        }
        match opt.provider.as_str() {
            "deepseek" => {
                let key = self
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
                let key = self
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
        if user.tier == "trial" {
            return self.default_model_id.clone();
        }
        let want = match user.preferred_model.as_deref() {
            Some(s) if !s.is_empty() => s,
            _ => return self.default_model_id.clone(),
        };
        if self
            .available_models
            .iter()
            .any(|m| m.id == want && m.available)
        {
            want.to_string()
        } else {
            self.default_model_id.clone()
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
        .route("/api/chat", post(chat_handler))
        .route("/api/chat/stream", post(chat_stream_handler))
        // Session-aware chat: each conversation is persisted in the DB so
        // the user can leave a session and return to continue.
        .route("/api/chat/sessions", get(list_chat_sessions_handler).post(create_chat_session_handler))
        .route("/api/chat/sessions/:id", get(get_chat_session_handler).delete(delete_chat_session_handler))
        .route("/api/chat/sessions/:id/stream", post(session_stream_handler))
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
    // Public endpoint — shown by the auth overlay before login. Return the
    // catalogue so the model picker can render even pre-login.
    let default_provider = s
        .available_models
        .iter()
        .find(|m| m.id == s.default_model_id)
        .map(|m| m.provider.clone())
        .unwrap_or_default();
    Json(json!({
        "provider": default_provider,
        "model": s.default_model_id,
        "default_model_id": s.default_model_id,
        "available_models": s.available_models,
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
        let ok = s
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
fn memory_path_for(user_id: &str) -> std::path::PathBuf {
    let base = ledger_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("memory").join(format!("{user_id}.jsonl"))
}

#[derive(Deserialize)]
struct SessionStreamReq {
    message: String,
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
    db.append_chat_message(&auth.user.id, &session_id, "user", &req.message, None)
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
    let task_desc = build_task_description(&req.message, &history);
    drop(db);

    let user_id = auth.user.id.clone();
    let user_tier = auth.user.tier.clone();
    let model_id = s.effective_model_for(&auth.user);
    let tx_for_done = tx.clone();
    let session_id_for_task = session_id.clone();
    let user_id_for_task = user_id.clone();
    let model_id_for_task = model_id.clone();

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
        let mut world = with_profile(".", profile);
        let task = Task {
            description: task_desc,
            source: None,
            deadline: None,
        };
        let _ = tx_for_done.send(json!({"type": "start"}));
        match loop_.run_with_max_iters(task, &mut world, s.max_iters).await {
            Ok(Outcome::Done { text, iters, .. }) => {
                let reply = text.unwrap_or_default();
                // Persist the assistant reply + update session model_id.
                if let Ok(db) = open_db() {
                    let _ = db.append_chat_message(
                        &user_id_for_task,
                        &session_id_for_task,
                        "asst",
                        &reply,
                        Some(iters),
                    );
                    let _ = db.update_chat_session_model(
                        &user_id_for_task,
                        &session_id_for_task,
                        &model_id_for_task,
                    );
                }
                let _ = tx_for_done.send(json!({
                    "type": "done", "ok": true, "iters": iters, "reply": reply,
                }));
            }
            Ok(Outcome::BudgetExhausted { iters, last_text, .. }) => {
                let reply = last_text.unwrap_or_else(|| "(budget exhausted)".into());
                if let Ok(db) = open_db() {
                    let _ = db.append_chat_message(
                        &user_id_for_task,
                        &session_id_for_task,
                        "asst",
                        &reply,
                        Some(iters),
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

    let task_desc = build_task_description(
        &req.message,
        &req.history
            .into_iter()
            .map(|m| (m.role, m.text))
            .collect::<Vec<_>>(),
    );

    let user_id = auth.user.id.clone();
    let user_tier = auth.user.tier.clone();
    // Resolve the model BEFORE spawning so we can surface configuration
    // errors as a sync 500 instead of an SSE `error` event the client
    // might never reach.
    let model_id = s.effective_model_for(&auth.user);
    let tx_for_done = tx.clone();
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
