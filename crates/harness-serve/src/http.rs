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
//! SSE frames carry each [`ChatChunk`](crate::ChatChunk) as JSON, including
//! failures: a stream error arrives as an `event: error` frame whose body is a
//! [`ChatChunk::Error`](crate::ChatChunk::Error), so one JSON parse handles every
//! frame and no failure is silently dropped.

use crate::auth::AuthError;
use crate::service::{ChatChunk, ChatService, ServeError};
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
        .route("/model", get(model))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(service)
}

/// `GET /model` — the live model's identifier, for a UI to display.
async fn model(State(service): State<Arc<ChatService>>) -> impl IntoResponse {
    Json(serde_json::json!({ "model": service.model_name() }))
}

/// CORS policy for [`router_with_cors`] (feature `cors`).
///
/// `harness-serve` exists to front a browser/aigui client, so a page served from
/// another origin must be allowed to call `/chat` and read the `/chat/stream`
/// SSE. This wraps the common cases without exposing `tower-http` to callers.
#[cfg(feature = "cors")]
#[derive(Clone, Default)]
pub struct CorsConfig {
    /// `None` → allow any origin (dev). `Some(list)` → only these origins.
    origins: Option<Vec<String>>,
}

#[cfg(feature = "cors")]
impl CorsConfig {
    /// Allow any origin, method, and header. Convenient for local dev; prefer
    /// [`allow_origins`](Self::allow_origins) in production.
    pub fn permissive() -> Self {
        Self { origins: None }
    }

    /// Restrict to an explicit allow-list of origins (e.g.
    /// `"https://bi.example.com"`). Origin strings that don't parse are skipped.
    pub fn allow_origins<I, S>(origins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            origins: Some(origins.into_iter().map(Into::into).collect()),
        }
    }

    fn layer(&self) -> tower_http::cors::CorsLayer {
        use tower_http::cors::{AllowOrigin, Any, CorsLayer};
        let base = CorsLayer::new().allow_methods(Any).allow_headers(Any);
        match &self.origins {
            None => base.allow_origin(Any),
            Some(list) => {
                let parsed = list
                    .iter()
                    .filter_map(|o| o.parse::<axum::http::HeaderValue>().ok())
                    .collect::<Vec<_>>();
                base.allow_origin(AllowOrigin::list(parsed))
            }
        }
    }
}

/// Like [`router`], but wrapped in a CORS layer so a browser page served from
/// another origin can call `/chat` and read the `/chat/stream` SSE (feature `cors`).
#[cfg(feature = "cors")]
pub fn router_with_cors(service: Arc<ChatService>, cors: &CorsConfig) -> Router {
    router(service).layer(cors.layer())
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

/// A stream failure as both a named SSE event and a JSON [`ChatChunk::Error`] frame:
/// the event name keeps older clients working, the JSON body means a client parsing
/// every `data:` as a chunk sees the reason instead of silently dropping it.
fn error_event(message: String) -> Event {
    Event::default()
        .event("error")
        .json_data(ChatChunk::Error {
            message: message.clone(),
        })
        .unwrap_or_else(|_| Event::default().event("error").data(message))
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
                    Ok(chunk) => Event::default()
                        .json_data(chunk)
                        .unwrap_or_else(|_| error_event("serialize failed".into())),
                    Err(e) => error_event(e.to_string()),
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
