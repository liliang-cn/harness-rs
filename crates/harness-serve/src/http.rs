//! Thin axum HTTP shell over [`ChatService`] (feature `http`).
//!
//! Routes: `POST /chat` (unary JSON), `POST /chat/stream` (token stream over
//! Server-Sent Events), and `GET /healthz`. The bearer token is read from the
//! `Authorization` header and handed to the service's authenticator; auth
//! failures map to 401/403. Everything substantive lives in [`ChatService`];
//! this module only translates HTTP ⇄ those calls.
//!
//! ```ignore
//! let state = std::sync::Arc::new(chat_service);
//! let app = harness_serve::http::router(state);
//! let listener = tokio::net::TcpListener::bind("127.0.0.1:43517").await?;
//! axum::serve(listener, app).await?;
//! ```
//!
//! SSE frames carry each [`ChatChunk`](crate::ChatChunk) as JSON; a stream error
//! arrives as an `event: error` frame.

use crate::auth::AuthError;
use crate::service::{ChatService, ServeError};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::StreamExt;
use std::convert::Infallible;
use std::sync::Arc;

/// Request body for `POST /chat`.
#[derive(serde::Deserialize)]
pub struct ChatRequest {
    pub session_id: String,
    pub message: String,
}

/// Build the router. Bind it with `axum::serve`.
pub fn router(service: Arc<ChatService>) -> Router {
    Router::new()
        .route("/chat", post(chat))
        .route("/chat/stream", post(chat_stream))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(service)
}

/// Extract a bearer token from the `Authorization` header, if present.
fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

async fn chat(
    State(service): State<Arc<ChatService>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    let token = bearer(&headers);
    match service
        .chat(token.as_deref(), &req.session_id, &req.message)
        .await
    {
        Ok(reply) => (StatusCode::OK, Json(reply)).into_response(),
        Err(ServeError::Auth(AuthError::Unauthorized)) => StatusCode::UNAUTHORIZED.into_response(),
        Err(ServeError::Auth(AuthError::Forbidden)) => StatusCode::FORBIDDEN.into_response(),
        Err(ServeError::Agent(msg)) => {
            tracing::error!(target: "harness.serve", error = %msg, "agent run failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /chat/stream` — same body as `/chat`, but streams the answer token by
/// token over SSE. Auth failures still return 401/403 (before the stream opens).
async fn chat_stream(
    State(service): State<Arc<ChatService>>,
    headers: HeaderMap,
    Json(req): Json<ChatRequest>,
) -> Response {
    let token = bearer(&headers);
    match service.chat_stream(token.as_deref(), &req.session_id, &req.message) {
        Ok(stream) => {
            let sse = stream.map(|item| {
                let event = match item {
                    Ok(chunk) => Event::default().json_data(chunk).unwrap_or_else(|_| {
                        Event::default().event("error").data("serialize failed")
                    }),
                    Err(e) => Event::default().event("error").data(e.to_string()),
                };
                Ok::<Event, Infallible>(event)
            });
            Sse::new(sse)
                .keep_alive(KeepAlive::default())
                .into_response()
        }
        Err(ServeError::Auth(AuthError::Unauthorized)) => StatusCode::UNAUTHORIZED.into_response(),
        Err(ServeError::Auth(AuthError::Forbidden)) => StatusCode::FORBIDDEN.into_response(),
        Err(ServeError::Agent(msg)) => {
            tracing::error!(target: "harness.serve", error = %msg, "chat stream setup failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
