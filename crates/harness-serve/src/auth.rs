//! Pluggable authentication for the serving layer.
//!
//! An [`Authenticator`] turns an opaque bearer token into an [`Actor`] — the
//! identity that flows into the audit trail and, via [`Actor::flags`], into the
//! model router (e.g. a department that must stay on the local model). The
//! framework ships a dev-only [`OpenAuth`] and a [`StaticTokenAuth`]; a real
//! deployment implements the trait against its own directory / SSO.

use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;

/// The authenticated caller. `id` names them for the audit trail; `flags` are
/// merged into `Context.metadata`, so an authenticator can, say, pin an HR user
/// to the local model by setting `router.keep_local = true`.
#[derive(Debug, Clone)]
pub struct Actor {
    pub id: String,
    pub flags: BTreeMap<String, Value>,
}

impl Actor {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            flags: BTreeMap::new(),
        }
    }

    /// Attach a metadata flag (merged into the request's `Context.metadata`).
    pub fn with_flag(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.flags.insert(key.into(), value.into());
        self
    }
}

/// Authentication failure. Serving code maps this onto a transport status
/// (401 / 403 for the HTTP layer).
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing or invalid credentials")]
    Unauthorized,
    #[error("authenticated but not permitted")]
    Forbidden,
}

/// Turns a bearer token (or its absence) into an [`Actor`]. Transport-agnostic:
/// the HTTP layer extracts the token from `Authorization: Bearer …`, but a CLI
/// or IPC front-end can call [`ChatService::chat`](crate::ChatService::chat)
/// directly.
pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(&self, token: Option<&str>) -> Result<Actor, AuthError>;
}

/// **Dev only.** Accepts everyone as a single fixed actor. Never deploy this in
/// front of real company data — it performs no authentication.
pub struct OpenAuth {
    actor_id: String,
}

impl OpenAuth {
    pub fn new(actor_id: impl Into<String>) -> Self {
        Self {
            actor_id: actor_id.into(),
        }
    }
}

impl Default for OpenAuth {
    fn default() -> Self {
        Self::new("anonymous")
    }
}

impl Authenticator for OpenAuth {
    fn authenticate(&self, _token: Option<&str>) -> Result<Actor, AuthError> {
        Ok(Actor::new(self.actor_id.clone()))
    }
}

/// Maps a static set of bearer tokens to actors. Suitable for a handful of
/// internal users / service accounts on a single machine; graduate to real SSO
/// when the user set grows.
pub struct StaticTokenAuth {
    tokens: HashMap<String, Actor>,
}

impl StaticTokenAuth {
    pub fn new() -> Self {
        Self {
            tokens: HashMap::new(),
        }
    }

    /// Register `token -> actor`.
    pub fn with_token(mut self, token: impl Into<String>, actor: Actor) -> Self {
        self.tokens.insert(token.into(), actor);
        self
    }
}

impl Default for StaticTokenAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl Authenticator for StaticTokenAuth {
    fn authenticate(&self, token: Option<&str>) -> Result<Actor, AuthError> {
        token
            .and_then(|t| self.tokens.get(t))
            .cloned()
            .ok_or(AuthError::Unauthorized)
    }
}
