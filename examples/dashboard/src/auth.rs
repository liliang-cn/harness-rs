//! Authentication primitives for ai-ledger:
//! - `User` / `Session` / `Invite` data models
//! - argon2id password hashing
//! - random session token generation
//! - `AuthCtx` axum extractor that pulls the bearer token from
//!   `Authorization: Bearer ...`, validates the session, and returns the
//!   resolved `User` to the handler.

use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::{OsRng as ArgonOsRng, RngCore};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    /// `trial` | `paid` | `admin`
    pub tier: String,
    pub invited_by: Option<String>,
    pub invite_code_used: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Per-user model preference; `None` falls back to the server's default.
    /// Trial users can't set this — the field stays None and they always get
    /// the default. Paid/admin users may pick from `AppState.available_models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_model: Option<String>,
    /// User's display / aggregation currency for the net-worth dashboard
    /// and the AI financial-manager reports. ISO 4217 code (e.g. "USD",
    /// "CNY", "JPY"). Defaults to "USD" via DB column default; users can
    /// change it from settings.
    #[serde(default = "default_base_currency")]
    pub base_currency: String,
}

fn default_base_currency() -> String {
    "USD".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub token: String,
    pub user_id: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    pub code: String,
    pub created_by: String,
    pub uses_remaining: i32,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub const SESSION_DAYS: i64 = 7;

/// Sliding session: `expires_at = now + 7d` on every successful touch.
pub fn new_session(user_id: &str) -> Session {
    let now = Utc::now();
    Session {
        token: random_token(),
        user_id: user_id.into(),
        created_at: now,
        last_seen_at: now,
        expires_at: now + Duration::days(SESSION_DAYS),
    }
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    ArgonOsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn random_invite_code() -> String {
    let mut bytes = [0u8; 8];
    ArgonOsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn random_user_id() -> String {
    let mut bytes = [0u8; 8];
    ArgonOsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("password hashing failed: {0}")]
    Hash(String),
    #[error("invalid email or password")]
    BadCredentials,
    #[error("email already registered")]
    EmailExists,
    #[error("invite code invalid or used up")]
    BadInvite,
    #[error("password too short (min 6 chars)")]
    PasswordTooShort,
    #[error("email looks invalid")]
    BadEmail,
}

pub fn hash_password(plain: &str) -> Result<String, AuthError> {
    if plain.len() < 6 {
        return Err(AuthError::PasswordTooShort);
    }
    let salt = SaltString::generate(&mut ArgonOsRng);
    let argon = Argon2::default();
    argon
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AuthError::Hash(e.to_string()))
}

pub fn verify_password(plain: &str, hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok()
}

pub fn validate_email(email: &str) -> Result<(), AuthError> {
    // Cheap structural check — no SMTP probing.
    let trimmed = email.trim();
    if !trimmed.contains('@') || trimmed.len() < 5 || trimmed.len() > 256 {
        return Err(AuthError::BadEmail);
    }
    Ok(())
}

/// Axum extractor: pulls the `Authorization: Bearer <token>` header, looks
/// the session up in the DB, returns the resolved `User`. 401 on miss /
/// expiry; bumps `last_seen_at` on hit.
pub struct AuthCtx {
    pub user: User,
    /// The bearer token this request authenticated with — useful when we
    /// need to keep just THIS session alive (e.g. on password change we drop
    /// every other device).
    pub token: String,
}

impl<S> FromRequestParts<S> for AuthCtx
where
    S: Send + Sync,
    crate::server::AppState: axum::extract::FromRef<S>,
{
    type Rejection = (StatusCode, axum::Json<serde_json::Value>);

    fn from_request_parts<'a, 'b, 'c>(
        parts: &'a mut Parts,
        state: &'b S,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self, Self::Rejection>> + Send + 'c>,
    >
    where
        'a: 'c,
        'b: 'c,
    {
        Box::pin(async move {
            let token = parts
                .headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .map(|s| s.trim().to_string())
                .ok_or_else(|| reject(StatusCode::UNAUTHORIZED, "missing bearer token"))?;

            let _ = state; // hold the ref alive
            let app: crate::server::AppState =
                axum::extract::FromRef::from_ref(state);
            let user = app
                .resolve_session(&token)
                .map_err(|e| reject(StatusCode::UNAUTHORIZED, &e))?;
            Ok(AuthCtx { user, token })
        })
    }
}

fn reject(code: StatusCode, msg: &str) -> (StatusCode, axum::Json<serde_json::Value>) {
    (
        code,
        axum::Json(serde_json::json!({ "error": msg })),
    )
}

// ─── trial-tier quotas ───

pub const TRIAL_MAX_TRANSACTIONS: u32 = 50;

pub fn is_trial(tier: &str) -> bool {
    tier == "trial"
}
