//! Admin-only HTTP endpoints. All routes gated by `require_admin`.
//! Mirrors `examples/ai-ledger/src/admin.rs` — same endpoint shape,
//! adapted to ai-note's domain (notes instead of transactions).

use crate::auth::{AuthCtx, hash_password};
use crate::db::AuditEvent;
use crate::server::{ApiError, AppState};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

fn require_admin(auth: &AuthCtx) -> Result<(), ApiError> {
    if auth.user.tier == "admin" {
        Ok(())
    } else {
        Err(ApiError::Forbidden("admin only".into()))
    }
}

pub fn register_routes(r: Router<AppState>) -> Router<AppState> {
    r.route("/api/admin/users", get(list_users))
        .route(
            "/api/admin/users/:id",
            get(get_user).patch(patch_user).delete(delete_user),
        )
        .route("/api/admin/users/:id/reset-password", post(reset_password))
        .route("/api/admin/audit", get(list_audit))
        .route("/api/admin/logs", get(get_logs))
        .route("/api/admin/config", get(get_config).patch(patch_config))
}

async fn list_users(State(s): State<AppState>, auth: AuthCtx) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = crate::server::open_db_state(&s)?;
    let users = db
        .list_users_with_stats()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // Enrich with estimated cost_usd using the currently configured chat
    // model. Approximate when the operator has swapped models — historical
    // tokens get priced at the *current* rate, not whatever produced them.
    // Exact accounting would need model_id on each audit row; v1 skips that.
    let cfg = s.cfg();
    let model_id = cfg.chat_model.clone();
    let enriched: Vec<Value> = users
        .into_iter()
        .map(|u| {
            let cost = crate::pricing::cost_usd(&cfg.pricing, &model_id, u.tokens_in, u.tokens_out);
            let mut v = serde_json::to_value(&u).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                // round to 6 decimal places — fractions of a cent are noise.
                obj.insert(
                    "cost_usd".into(),
                    json!((cost * 1_000_000.0).round() / 1_000_000.0),
                );
            }
            v
        })
        .collect();
    Ok(Json(
        json!({ "users": enriched, "priced_at_model": model_id }),
    ))
}

async fn get_user(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = crate::server::open_db_state(&s)?;
    let user = db
        .get_user_by_id(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("user not found".into()))?;
    let notes = db
        .count_notes(&id, None)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let recent = db
        .list_audit(Some(&id), None, i64::MAX, 25)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({
        "user": {
            "id": user.id,
            "email": user.email,
            "tier": user.tier,
            "created_at": user.created_at.to_rfc3339(),
            "preferred_model": user.preferred_model,
            "note_count": notes,
        },
        "recent_audit": recent,
    })))
}

#[derive(Deserialize)]
struct PatchUser {
    tier: Option<String>,
}

async fn patch_user(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
    Json(req): Json<PatchUser>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let Some(new_tier) = req.tier else {
        return Err(ApiError::BadRequest("nothing to update".into()));
    };
    if !["trial", "paid", "admin"].contains(&new_tier.as_str()) {
        return Err(ApiError::BadRequest(format!("invalid tier `{new_tier}`")));
    }
    if auth.user.id == id && new_tier != "admin" {
        return Err(ApiError::BadRequest(
            "refusing to demote yourself from admin".into(),
        ));
    }
    let db = crate::server::open_db_state(&s)?;
    let existing = db
        .get_user_by_id(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("user not found".into()))?;
    let old_tier = existing.tier.clone();
    db.update_user_tier(&id, &new_tier)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let meta = json!({ "from": old_tier, "to": new_tier, "by_email": auth.user.email }).to_string();
    let _ = db.insert_audit(
        Some(&auth.user.id),
        "tier_change",
        Some(&id),
        Some(&meta),
        0,
        0,
    );
    Ok(Json(json!({ "ok": true, "tier": new_tier })))
}

async fn delete_user(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    if auth.user.id == id {
        return Err(ApiError::BadRequest("refusing to delete yourself".into()));
    }
    let db = crate::server::open_db_state(&s)?;
    let target = db
        .get_user_by_id(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("user not found".into()))?;
    db.delete_user_cascade(&id)
        .map_err(|e| ApiError::Internal(format!("cascade: {e}")))?;
    let meta = json!({
        "deleted_email": target.email,
        "by_email": auth.user.email,
    })
    .to_string();
    let _ = db.insert_audit(
        Some(&auth.user.id),
        "delete_user",
        Some(&id),
        Some(&meta),
        0,
        0,
    );
    Ok(Json(json!({ "ok": true })))
}

async fn reset_password(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = crate::server::open_db_state(&s)?;
    db.get_user_by_id(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("user not found".into()))?;
    let temp_password = gen_temp_password();
    let hash = hash_password(&temp_password).map_err(|e| ApiError::Internal(e.to_string()))?;
    db.update_user_password(&id, &hash)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    db.delete_other_sessions(&id, "")
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let _ = db.insert_audit(
        Some(&auth.user.id),
        "password_reset",
        Some(&id),
        Some(&json!({"by_email": auth.user.email}).to_string()),
        0,
        0,
    );
    Ok(Json(json!({ "ok": true, "temp_password": temp_password })))
}

#[derive(Deserialize)]
struct AuditQuery {
    user_id: Option<String>,
    kind: Option<String>,
    before_ms: Option<i64>,
    limit: Option<u32>,
}

async fn list_audit(
    State(s): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = crate::server::open_db_state(&s)?;
    let limit = q.limit.unwrap_or(50).min(500);
    let before = q.before_ms.unwrap_or(i64::MAX);
    let rows: Vec<AuditEvent> = db
        .list_audit(q.user_id.as_deref(), q.kind.as_deref(), before, limit)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let next_cursor = rows.last().map(|r| r.created_ms);
    Ok(Json(json!({
        "events": rows,
        "next_before_ms": next_cursor,
    })))
}

#[derive(Deserialize)]
struct LogsQuery {
    lines: Option<u32>,
}

async fn get_logs(
    State(_s): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<LogsQuery>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let n = q.lines.unwrap_or(200).clamp(10, 5000);
    let out = tokio::process::Command::new("journalctl")
        .args([
            "-u",
            "ai-note",
            "-n",
            &n.to_string(),
            "--no-pager",
            "--output=short-iso",
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => Ok(Json(json!({
            "lines": String::from_utf8_lossy(&o.stdout),
        }))),
        Ok(o) => Ok(Json(json!({
            "lines": String::from_utf8_lossy(&o.stdout),
            "error": String::from_utf8_lossy(&o.stderr),
        }))),
        Err(e) => Ok(Json(json!({
            "lines": "",
            "error": format!("journalctl unavailable: {e} (expected in dev)"),
        }))),
    }
}

#[derive(Serialize)]
struct ProviderConfigView {
    deepseek_key_masked: String,
    gemini_key_masked: String,
    chat_provider: String,
    chat_model: String,
    pricing: crate::pricing::RateCard,
}

async fn get_config(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<ProviderConfigView>, ApiError> {
    require_admin(&auth)?;
    let cfg = s.cfg();
    Ok(Json(ProviderConfigView {
        deepseek_key_masked: mask(cfg.deepseek_key.as_deref()),
        gemini_key_masked: mask(cfg.gemini_key.as_deref()),
        chat_provider: cfg.chat_provider,
        chat_model: cfg.chat_model,
        pricing: cfg.pricing,
    }))
}

#[derive(Deserialize)]
struct PatchConfig {
    deepseek_api_key: Option<String>,
    gemini_api_key: Option<String>,
    chat_provider: Option<String>,
    chat_model: Option<String>,
    /// Full replacement of the rate card. Caller must send the entire map;
    /// omitted entries are removed. Validated for non-negative numbers.
    pricing: Option<crate::pricing::RateCard>,
}

async fn patch_config(
    State(s): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<PatchConfig>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = crate::server::open_db_state(&s)?;
    let mut changed = Vec::<&str>::new();

    if let Some(k) = req.deepseek_api_key.as_deref() {
        let v = k.trim();
        if !v.is_empty() {
            db.provider_config_set("deepseek_api_key", v)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            changed.push("deepseek_api_key");
        }
    }
    if let Some(k) = req.gemini_api_key.as_deref() {
        let v = k.trim();
        if !v.is_empty() {
            db.provider_config_set("gemini_api_key", v)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            changed.push("gemini_api_key");
        }
    }
    if let Some(p) = req.chat_provider.as_deref() {
        let v = p.trim();
        if !v.is_empty() {
            db.provider_config_set("chat_provider", v)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            changed.push("chat_provider");
        }
    }
    if let Some(m) = req.chat_model.as_deref() {
        let v = m.trim();
        if !v.is_empty() {
            db.provider_config_set("chat_model", v)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            changed.push("chat_model");
        }
    }
    if let Some(card) = req.pricing.as_ref() {
        // Reject negative or NaN rates; empty cards are allowed (everything
        // then falls back to pricing::FALLBACK_RATE).
        for (k, v) in card {
            if !v.input.is_finite() || !v.output.is_finite() || v.input < 0.0 || v.output < 0.0 {
                return Err(ApiError::BadRequest(format!(
                    "pricing[{k}]: input/output must be finite and ≥ 0"
                )));
            }
        }
        let json = serde_json::to_string(card)
            .map_err(|e| ApiError::Internal(format!("pricing encode: {e}")))?;
        db.provider_config_set("pricing_rate_card", &json)
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        changed.push("pricing");
    }

    if changed.is_empty() {
        return Err(ApiError::BadRequest("nothing to update".into()));
    }

    // Hot-swap in-memory config. NOTE: we update the credential strings,
    // but the actual chat model adapter is built once at startup from the
    // initial keys. A full reload would require rebuilding the
    // `Arc<dyn Model>` — left for a follow-up, since the realistic flow
    // is: admin updates keys → restart service. For now the config
    // endpoint surfaces the changed values for `get_config` and the
    // embedder/key check, but model rebuild on patch is not wired.
    let stored = db
        .provider_config_all()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    {
        let mut w = s.config.write().expect("config lock poisoned");
        w.deepseek_key = stored
            .get("deepseek_api_key")
            .cloned()
            .or(w.deepseek_key.clone());
        w.gemini_key = stored
            .get("gemini_api_key")
            .cloned()
            .or(w.gemini_key.clone());
        if let Some(p) = stored.get("chat_provider") {
            w.chat_provider = p.clone();
        }
        if let Some(m) = stored.get("chat_model") {
            w.chat_model = m.clone();
        }
        if let Some(json) = stored.get("pricing_rate_card")
            && let Ok(card) = serde_json::from_str::<crate::pricing::RateCard>(json)
        {
            w.pricing = card;
        }
    }

    let _ = db.insert_audit(
        Some(&auth.user.id),
        "admin_config_change",
        None,
        Some(&json!({"fields": changed, "by_email": auth.user.email}).to_string()),
        0,
        0,
    );
    Ok(Json(json!({ "ok": true, "changed": changed })))
}

fn mask(key: Option<&str>) -> String {
    match key {
        None => String::new(),
        Some(k) if k.len() <= 8 => "*".repeat(k.len()),
        Some(k) => format!("{}…{}", &k[..4], &k[k.len() - 4..]),
    }
}

fn gen_temp_password() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let alphabet: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789abcdefghijkmnpqrstuvwxyz";
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = String::with_capacity(12);
    for _ in 0..12 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(alphabet[((x >> 32) as usize) % alphabet.len()] as char);
    }
    out
}
