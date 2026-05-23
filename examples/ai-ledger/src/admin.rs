//! Admin-only HTTP endpoints. All routes mounted by `register_routes` go
//! through `require_admin` which 403s for non-admin tiers. The handlers are
//! intentionally chunky-but-flat — most do one db call, format a JSON
//! response, return.

use crate::auth::{AuthCtx, hash_password};
use crate::db::AuditEvent;
use crate::server::{ApiError, AppConfig, AppState, ModelOption, open_db};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{delete, get, patch, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Gate: pass through if the authenticated user is `admin`; else 403.
fn require_admin(auth: &AuthCtx) -> Result<(), ApiError> {
    if auth.user.tier == "admin" {
        Ok(())
    } else {
        Err(ApiError::Forbidden("admin only".into()))
    }
}

/// Mount all `/api/admin/*` routes on the given router.
pub fn register_routes(r: Router<AppState>) -> Router<AppState> {
    r.route("/api/admin/users", get(list_users))
        .route("/api/admin/users/:id", get(get_user).patch(patch_user).delete(delete_user))
        .route(
            "/api/admin/users/:id/reset-password",
            post(reset_password),
        )
        .route("/api/admin/audit", get(list_audit))
        .route("/api/admin/logs", get(get_logs))
        .route(
            "/api/admin/config",
            get(get_config).patch(patch_config),
        )
}

// ───── handlers ─────

async fn list_users(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = open_db()?;
    let users = db
        .list_users_with_stats()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // Enrich with cost_usd using the operator's current default chat model.
    // Approximate — historical tokens get priced at the *current* rate; for
    // exact accounting we'd need model_id on each audit row.
    let cfg = s.cfg();
    let model_id = cfg.default_model_id.clone();
    let enriched: Vec<Value> = users
        .into_iter()
        .map(|u| {
            let cost = crate::pricing::cost_usd(&cfg.pricing, &model_id, u.tokens_in, u.tokens_out);
            let mut v = serde_json::to_value(&u).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "cost_usd".into(),
                    json!((cost * 1_000_000.0).round() / 1_000_000.0),
                );
            }
            v
        })
        .collect();
    Ok(Json(json!({ "users": enriched, "priced_at_model": model_id })))
}

async fn get_user(
    State(_s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = open_db()?;
    let user = db
        .get_user_by_id(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("user not found".into()))?;
    let txns = db
        .count_user_transactions(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let trades = db
        .count_user_trades(&id)
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
            "txn_count": txns,
            "trade_count": trades,
        },
        "recent_audit": recent,
    })))
}

#[derive(Deserialize)]
struct PatchUser {
    tier: Option<String>,
}

async fn patch_user(
    State(_s): State<AppState>,
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
    // Guard: don't let an admin demote themselves — would lock them out.
    if auth.user.id == id && new_tier != "admin" {
        return Err(ApiError::BadRequest(
            "refusing to demote yourself from admin".into(),
        ));
    }
    let db = open_db()?;
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
    State(_s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    if auth.user.id == id {
        return Err(ApiError::BadRequest("refusing to delete yourself".into()));
    }
    let db = open_db()?;
    let target = db
        .get_user_by_id(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("user not found".into()))?;
    db.delete_user_cascade(&id)
        .map_err(|e| ApiError::Internal(format!("cascade delete failed: {e}")))?;
    // Wipe the per-user memory JSONL file too (lives outside SQLite).
    let mem_path = crate::server::memory_path_for(&id);
    let _ = std::fs::remove_file(&mem_path);
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
    State(_s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = open_db()?;
    db.get_user_by_id(&id)
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::BadRequest("user not found".into()))?;
    let temp_password = gen_temp_password();
    let hash = hash_password(&temp_password).map_err(|e| ApiError::Internal(e.to_string()))?;
    db.update_user_password(&id, &hash)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    // Also drop every existing session for the target so they get kicked off.
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
    State(_s): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&auth)?;
    let db = open_db()?;
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
    // Fixed argv — no shell, no interpolation. Only journalctl, only our unit.
    let out = tokio::process::Command::new("journalctl")
        .args([
            "-u",
            "ai-ledger",
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
        // Return 200 + error field rather than 500 so the System page still
        // renders. journalctl is genuinely absent on dev macOS, so this
        // shouldn't be flagged as a hard error.
        Err(e) => Ok(Json(json!({
            "lines": "",
            "error": format!("journalctl unavailable: {e} (only systemd hosts have it — expected in dev)"),
        }))),
    }
}

#[derive(Serialize)]
struct ProviderConfigView {
    deepseek_key_masked: String,
    gemini_key_masked: String,
    default_model_id: String,
    available_models: Vec<ModelOption>,
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
        default_model_id: cfg.default_model_id,
        available_models: cfg.available_models,
        pricing: cfg.pricing,
    }))
}

#[derive(Deserialize)]
struct PatchConfig {
    deepseek_api_key: Option<String>,
    gemini_api_key: Option<String>,
    default_model_id: Option<String>,
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
    let db = open_db()?;
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
    if let Some(m) = req.default_model_id.as_deref() {
        let v = m.trim();
        if !v.is_empty() {
            db.provider_config_set("default_model_id", v)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            changed.push("default_model_id");
        }
    }
    if let Some(card) = req.pricing.as_ref() {
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

    // Hot-swap the in-memory config. Read everything fresh from the DB so
    // we never partially-apply if multiple admins are editing concurrently.
    let stored = db
        .provider_config_all()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    {
        let mut w = s.config.write().expect("config lock poisoned");
        let pricing = stored
            .get("pricing_rate_card")
            .and_then(|s| serde_json::from_str::<crate::pricing::RateCard>(s).ok())
            .unwrap_or_else(|| w.pricing.clone());
        let new_cfg = AppConfig {
            default_model_id: stored
                .get("default_model_id")
                .cloned()
                .unwrap_or_else(|| w.default_model_id.clone()),
            available_models: w.available_models.clone(),
            deepseek_key: stored.get("deepseek_api_key").cloned().or_else(|| w.deepseek_key.clone()),
            gemini_key: stored.get("gemini_api_key").cloned().or_else(|| w.gemini_key.clone()),
            pricing,
        };
        *w = new_cfg;
        w.refresh_availability();
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

// ───── helpers ─────

fn mask(key: Option<&str>) -> String {
    match key {
        None => String::new(),
        Some(k) if k.len() <= 8 => "*".repeat(k.len()),
        Some(k) => format!("{}…{}", &k[..4], &k[k.len() - 4..]),
    }
}

fn gen_temp_password() -> String {
    // 12-char alphanumeric (no homoglyphs). Mixes time-based + thread-id
    // entropy — strong enough for one-shot admin reset, not crypto-grade.
    use std::time::{SystemTime, UNIX_EPOCH};
    let alphabet: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789abcdefghijkmnpqrstuvwxyz";
    let mut x = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut out = String::with_capacity(12);
    for _ in 0..12 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        out.push(alphabet[((x >> 32) as usize) % alphabet.len()] as char);
    }
    out
}
