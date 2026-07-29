//! Multi-session serving core for harness-rs agents.
//!
//! harness-rs gives you an agent loop, tools, memory, and hooks; this crate adds
//! the layer between "a loop you call in `main`" and "a service your whole
//! company talks to": **pluggable auth**, **per-conversation session history**,
//! **per-request caller identity**, and a **wired audit trail** — the pieces a
//! single-machine SMB deployment (process / policy / BI assistants) needs before
//! it can face real users.
//!
//! The heart is [`ChatService`]: one `async fn chat(token, session_id, message)`
//! that authenticates, loads history, stamps identity + routing flags into
//! `Context.metadata`, runs the agent with a fresh audit hook, persists the
//! exchange, and returns the answer. It's transport-agnostic — call it from a
//! CLI or IM bot, or enable the `http` feature for a ready axum router.
//!
//! ```ignore
//! use harness_serve::{ChatService, Actor, StaticTokenAuth, InMemorySessions};
//! use harness_hooks::JsonlAuditSink;
//! use harness_models::{ApiKind, ModelRouter, KEEP_LOCAL_KEY};
//! use harness_redact::Redactor;
//! use std::sync::Arc;
//!
//! // Local-first model with cloud fallback (see harness_models::ModelRouter).
//! let local = ApiKind::OpenAI.build("http://localhost:11434/v1", "qwen2.5:14b", "ollama");
//! let cloud = ApiKind::Anthropic.build("https://api.anthropic.com", "claude-opus-4-8", key);
//! let model = Arc::new(ModelRouter::new(local).with_fallback(cloud));
//!
//! // Tokens → actors; HR is pinned to the local model.
//! let auth = Arc::new(StaticTokenAuth::new()
//!     .with_token("tok-alice", Actor::new("alice@sales"))
//!     .with_token("tok-bob", Actor::new("bob@hr").with_flag(KEEP_LOCAL_KEY, true)));
//!
//! let service = ChatService::new(model, auth, Arc::new(InMemorySessions::new()), "/var/lib/app")
//!     .with_audit(Arc::new(JsonlAuditSink::new("/var/lib/app/audit.jsonl").unwrap()))
//!     .with_audit_redaction(Redactor::new);
//!
//! // Then: harness_serve::http::router(Arc::new(service)) behind axum::serve.
//! ```

pub mod auth;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "http")]
pub mod http;
pub mod service;
pub mod session;

pub use auth::{Actor, AuthError, Authenticator, OpenAuth, StaticTokenAuth};
#[cfg(feature = "cors")]
pub use http::{CorsConfig, router_with_cors};
pub use service::{ChatChunk, ChatReply, ChatService, ServeError};
pub use session::{InMemorySessions, SessionStore};

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Model;
    use harness_models::{MockModel, MockResponse};
    use std::sync::Arc;

    fn service_with(model: Arc<dyn Model>, auth: Arc<dyn Authenticator>) -> ChatService {
        ChatService::new(
            model,
            auth,
            Arc::new(InMemorySessions::new()),
            std::env::temp_dir().join("serve-test"),
        )
    }

    #[test]
    fn stream_errors_serialize_as_json_chunks() {
        // A client parses every SSE `data:` as a ChatChunk, so a failure has to be a
        // chunk too — a bare string body is silently dropped and the stream just ends.
        let json = serde_json::to_value(ChatChunk::Error {
            message: "agent error: model error".into(),
        })
        .unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["message"], "agent error: model error");
    }

    /// A tool that records the session it was invoked under, which is the whole point of `World.session`.
    struct WitnessTool {
        seen: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        schema: harness_core::ToolSchema,
    }

    #[async_trait::async_trait]
    impl harness_core::Tool for WitnessTool {
        fn name(&self) -> &str {
            &self.schema.name
        }
        fn schema(&self) -> &harness_core::ToolSchema {
            &self.schema
        }
        fn risk(&self) -> harness_core::ToolRisk {
            harness_core::ToolRisk::ReadOnly
        }
        async fn invoke(
            &self,
            _args: serde_json::Value,
            world: &mut harness_core::World,
        ) -> Result<harness_core::ToolResult, harness_core::ToolError> {
            let session = world
                .session
                .clone()
                .expect("a served turn carries its session");
            self.seen
                .lock()
                .unwrap()
                .push((session.id.clone(), session.actor.clone()));
            Ok(harness_core::ToolResult {
                ok: true,
                content: serde_json::json!({"session": session.id}),
                trace: None,
            })
        }
    }

    #[tokio::test]
    async fn a_tool_can_tell_whose_turn_it_is_in() {
        // Tools are registered once on a long-lived service, so one object serves every caller. Without
        // the session on the world, a tool holding per-conversation state has one shared slot and two
        // simultaneous learners silently overwrite each other's.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tool = Arc::new(WitnessTool {
            seen: seen.clone(),
            schema: harness_core::ToolSchema {
                name: "witness".into(),
                description: "records the session it ran under".into(),
                input: serde_json::json!({"type": "object", "properties": {}}),
            },
        });
        let model: Arc<dyn Model> = Arc::new(
            MockModel::new()
                .script(MockResponse::tool_call("witness", serde_json::json!({})))
                .script(MockResponse::text("done"))
                .script(MockResponse::tool_call("witness", serde_json::json!({})))
                .script(MockResponse::text("done")),
        );
        let svc = ChatService::new(
            model,
            Arc::new(OpenAuth::new("ada")),
            Arc::new(InMemorySessions::new()),
            std::env::temp_dir().join("serve-session-test"),
        )
        .with_tool(tool);

        svc.chat(None, "lesson-a", "hello").await.unwrap();
        svc.chat(None, "lesson-b", "hello").await.unwrap();

        let recorded = seen.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "the tool ran once per turn");
        // Each turn saw its own conversation, not whichever one happened to be registered last.
        assert_eq!(recorded[0].0, "lesson-a");
        assert_eq!(recorded[1].0, "lesson-b");
        assert!(recorded.iter().all(|(_, actor)| actor == "ada"));
    }

    #[tokio::test]
    async fn rejects_bad_token() {
        let model: Arc<dyn Model> = Arc::new(MockModel::new().script(MockResponse::text("hi")));
        let auth = Arc::new(StaticTokenAuth::new().with_token("good", Actor::new("alice")));
        let svc = service_with(model, auth);

        let err = svc.chat(Some("bad"), "s1", "hello").await.unwrap_err();
        assert!(matches!(err, ServeError::Auth(AuthError::Unauthorized)));
    }

    #[tokio::test]
    async fn answers_and_persists_history() {
        // Two turns; the second call should see the first in seeded history.
        let model: Arc<dyn Model> = Arc::new(
            MockModel::new()
                .script(MockResponse::text("first answer"))
                .script(MockResponse::text("second answer")),
        );
        let auth = Arc::new(OpenAuth::new("tester"));
        let sessions = Arc::new(InMemorySessions::new());
        let svc = ChatService::new(
            model,
            auth,
            sessions.clone(),
            std::env::temp_dir().join("serve-test2"),
        );

        let r1 = svc.chat(None, "s1", "hello").await.unwrap();
        assert_eq!(r1.answer, "first answer");
        assert_eq!(r1.actor, "tester");

        // History now holds the first exchange (user + assistant).
        assert_eq!(sessions.history("s1").len(), 2);

        let r2 = svc.chat(None, "s1", "again").await.unwrap();
        assert_eq!(r2.answer, "second answer");
        assert_eq!(sessions.history("s1").len(), 4);
    }

    #[tokio::test]
    async fn audit_trail_captures_the_exchange() {
        use harness_hooks::{AuditRecord, AuditSink};
        use std::sync::Mutex;

        #[derive(Default)]
        struct VecSink(Mutex<Vec<AuditRecord>>);
        impl AuditSink for VecSink {
            fn record(&self, rec: &AuditRecord) {
                self.0.lock().unwrap().push(rec.clone());
            }
        }

        let model: Arc<dyn Model> = Arc::new(MockModel::new().script(MockResponse::text("42")));
        let auth = Arc::new(OpenAuth::new("carol"));
        let sink = Arc::new(VecSink::default());
        let svc = service_with(model, auth).with_audit(sink.clone());

        svc.chat(None, "s9", "what is the answer?").await.unwrap();

        let recs = sink.0.lock().unwrap();
        // Identity stamped, request + response captured.
        assert!(recs.iter().any(|r| r.kind == "request"));
        assert!(recs.iter().any(|r| r.kind == "response"));
        assert!(recs.iter().all(|r| r.actor.as_deref() == Some("carol")));
        assert!(recs.iter().all(|r| r.session.as_deref() == Some("s9")));
    }

    #[tokio::test]
    async fn chat_stream_yields_tokens_then_done() {
        use futures::StreamExt;

        let model: Arc<dyn Model> =
            Arc::new(MockModel::new().script(MockResponse::text("hello world")));
        let auth = Arc::new(OpenAuth::new("streamer"));
        let svc = service_with(model, auth);

        let stream = svc.chat_stream(None, "s1", "hi").unwrap();
        let chunks: Vec<ChatChunk> = stream.map(|r| r.unwrap()).collect().await;

        // Token(s) first, terminal Done last.
        let tokens: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChatChunk::Token { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(tokens.contains("hello world"), "tokens: {tokens:?}");
        match chunks.last() {
            Some(ChatChunk::Done {
                answer,
                actor,
                request_id,
            }) => {
                assert_eq!(answer, "hello world");
                assert_eq!(actor, "streamer");
                assert!(request_id.starts_with("req-"), "request id: {request_id}");
            }
            other => panic!("expected terminal Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_records_a_reconstructable_run() {
        let dir = std::env::temp_dir().join(format!("serve-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let model: Arc<dyn Model> =
            Arc::new(MockModel::new().script(MockResponse::text("recorded answer")));
        let auth = Arc::new(OpenAuth::new("rec"));
        let svc = ChatService::new(
            model,
            auth,
            Arc::new(InMemorySessions::new()),
            std::env::temp_dir().join("serve-replay-data"),
        )
        .with_replay(&dir);

        let reply = svc.chat(None, "sess-x", "hello").await.unwrap();
        assert!(reply.request_id.starts_with("req-"));

        // The recording lands at <dir>/<session>/<request_id>.jsonl and can be
        // read back into replayable events.
        let path = dir
            .join("sess-x")
            .join(format!("{}.jsonl", reply.request_id));
        assert!(path.exists(), "replay file missing at {}", path.display());
        let events = harness_loop::read_session(&path).unwrap();
        assert!(!events.is_empty(), "recording should hold the run's events");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn chat_stream_rejects_bad_token_before_streaming() {
        let model: Arc<dyn Model> = Arc::new(MockModel::new().script(MockResponse::text("x")));
        let auth = Arc::new(StaticTokenAuth::new().with_token("good", Actor::new("a")));
        let svc = service_with(model, auth);
        // Auth failure surfaces synchronously, not as a stream item.
        assert!(svc.chat_stream(Some("bad"), "s1", "hi").is_err());
    }
}
