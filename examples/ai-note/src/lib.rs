// lib target — exposes db module for `cargo test --lib`

// db.rs refers to crate::auth::{User, Session, Invite}. We provide minimal
// versions here (no axum extractor, no server dependency) so the lib compiles
// without dragging in server.rs.
pub mod auth {
    use chrono::{DateTime, Utc};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct User {
        pub id: String,
        pub email: String,
        pub password_hash: String,
        pub tier: String,
        pub invited_by: Option<String>,
        pub invite_code_used: Option<String>,
        pub created_at: DateTime<Utc>,
        pub preferred_model: Option<String>,
    }

    #[derive(Debug, Clone)]
    pub struct Session {
        pub token: String,
        pub user_id: String,
        pub created_at: DateTime<Utc>,
        pub last_seen_at: DateTime<Utc>,
        pub expires_at: DateTime<Utc>,
    }

    #[derive(Debug, Clone)]
    pub struct Invite {
        pub code: String,
        pub created_by: String,
        pub uses_remaining: i32,
        pub expires_at: Option<DateTime<Utc>>,
        pub created_at: DateTime<Utc>,
    }
}

pub mod db;
