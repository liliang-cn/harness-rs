//! [`ChatService`] — the transport-agnostic heart of the serving layer.
//!
//! It ties together everything the SMB deployment needs behind one call:
//! authenticate the caller, load their conversation, stamp per-request identity
//! and routing flags into `Context.metadata`, run the agent with a fresh
//! per-request audit hook, persist the exchange, and return the answer.
//!
//! Two entry points, same pipeline:
//! - [`chat`](ChatService::chat) — awaits the full answer (unary).
//! - [`chat_stream`](ChatService::chat_stream) — yields [`ChatChunk`]s as tokens
//!   arrive, ending with a terminal `Done`. Powers HTTP SSE (`http` feature) and
//!   server-streaming gRPC (`grpc` feature).
//!
//! The HTTP/gRPC layers are thin shells over these; a CLI or IM bot can call
//! them directly.

use crate::auth::{Actor, AuthError, Authenticator};
use crate::session::SessionStore;
use harness_core::{DynModel, Event, Hook, HookOutcome, Model, Task, Tool, World};
use harness_hooks::{ACTOR_KEY, AuditHook, AuditSink, REQUEST_KEY, SESSION_KEY, new_request_id};
use harness_loop::{AgentLoop, Outcome, SessionRecorder};
use harness_redact::Redactor;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Builds a fresh [`Redactor`] per request (the type isn't `Clone`, and the
/// audit hook is per-request).
type RedactorFactory = Arc<dyn Fn() -> Redactor + Send + Sync>;

/// The answer plus the resolved caller, returned from [`ChatService::chat`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatReply {
    pub answer: String,
    /// The authenticated actor id (echoed for the caller / logs).
    pub actor: String,
    /// Correlation id for this request — look it up in the audit trail, the
    /// OTel trace, or (if replay is on) the recording to reconstruct the run.
    pub request_id: String,
}

/// A streamed piece of a chat response (from [`ChatService::chat_stream`]).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatChunk {
    /// An incremental text fragment — append it to what you've shown so far.
    Token { text: String },
    /// Terminal chunk: the full answer, resolved actor, and correlation id.
    /// Always emitted last.
    Done {
        answer: String,
        actor: String,
        request_id: String,
    },
}

/// Failure from a chat request.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("agent error: {0}")]
    Agent(String),
}

/// Shared, long-lived service. Construct once, wrap in an `Arc`, hand to the
/// transport. Cloning per request is cheap (everything inside is `Arc`/`Vec` of
/// `Arc`).
pub struct ChatService {
    model: Arc<dyn Model>,
    tools: Vec<Arc<dyn Tool>>,
    auth: Arc<dyn Authenticator>,
    sessions: Arc<dyn SessionStore>,
    audit: Option<Arc<dyn AuditSink>>,
    redactor_factory: Option<RedactorFactory>,
    replay_dir: Option<PathBuf>,
    instruction: Option<String>,
    data_dir: PathBuf,
    max_iters: u32,
}

impl ChatService {
    /// Minimal service: a model, an authenticator, and a session store. Add
    /// tools with [`with_tool`](Self::with_tool) and an audit trail with
    /// [`with_audit`](Self::with_audit).
    ///
    /// `data_dir` roots the per-request [`World`](harness_core::World) (repo view
    /// / scratch space for tools).
    pub fn new(
        model: Arc<dyn Model>,
        auth: Arc<dyn Authenticator>,
        sessions: Arc<dyn SessionStore>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            model,
            tools: Vec::new(),
            auth,
            sessions,
            audit: None,
            redactor_factory: None,
            replay_dir: None,
            instruction: None,
            data_dir: data_dir.into(),
            max_iters: harness_core::Policy::default().max_iters,
        }
    }

    /// A system instruction applied to every request (via the loop's
    /// `Context.system`) — how the agent should behave (e.g. "only answer via the
    /// governed tools; never claim you can't access data; never invent numbers").
    /// Essential for small local models, which otherwise sometimes refuse or
    /// hallucinate instead of calling a tool.
    pub fn with_instruction(mut self, s: impl Into<String>) -> Self {
        self.instruction = Some(s.into());
        self
    }

    /// Register a tool the agent may call (policy search, SQL, MCP bridge, …).
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// Attach an audit sink. Every request then records who asked what, what the
    /// AI answered, and which tools (data sources) it touched. A fresh
    /// [`AuditHook`] is created per request so concurrent callers never share
    /// identity state.
    pub fn with_audit(mut self, sink: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(sink);
        self
    }

    /// Scrub PII out of audit records. The closure builds a [`Redactor`] per
    /// request (e.g. `|| Redactor::new()`); no effect without
    /// [`with_audit`](Self::with_audit).
    pub fn with_audit_redaction(
        mut self,
        factory: impl Fn() -> Redactor + Send + Sync + 'static,
    ) -> Self {
        self.redactor_factory = Some(Arc::new(factory));
        self
    }

    /// Cap the agent's tool-calling iterations per request. Defaults to the
    /// core [`Policy`](harness_core::Policy) budget.
    pub fn with_max_iters(mut self, n: u32) -> Self {
        self.max_iters = n;
        self
    }

    /// Record each served run so it can be **replayed** deterministically. Every
    /// request is written to `<dir>/<session_id>/<request_id>.jsonl`, readable
    /// with [`harness_loop::read_session`] and re-runnable via
    /// [`harness_loop::replay_as_mock`]. The `request_id` is the same one that
    /// appears in the audit trail and OTel trace, so a single id ties all three
    /// together.
    pub fn with_replay(mut self, dir: impl Into<PathBuf>) -> Self {
        self.replay_dir = Some(dir.into());
        self
    }

    /// Per-request identity + routing metadata, seeded into `Context.metadata`.
    fn make_metadata(
        &self,
        actor: &Actor,
        session_id: &str,
        request_id: &str,
    ) -> BTreeMap<String, Value> {
        let mut metadata = actor.flags.clone();
        metadata.insert(ACTOR_KEY.to_string(), actor.id.clone().into());
        metadata.insert(SESSION_KEY.to_string(), session_id.to_string().into());
        metadata.insert(REQUEST_KEY.to_string(), request_id.to_string().into());
        metadata
    }

    /// A per-request loop: shared model + tools, a fresh audit hook, and (if
    /// enabled) a replay recorder for this exact request. The caller adds
    /// streaming / a forward hook on top.
    fn build_agent(&self, session_id: &str, request_id: &str) -> AgentLoop<DynModel> {
        let mut agent = AgentLoop::new(DynModel(self.model.clone()));
        if let Some(system) = &self.instruction {
            agent = agent.with_system(system.clone());
        }
        for tool in &self.tools {
            agent = agent.with_tool(tool.clone());
        }
        if let Some(sink) = &self.audit {
            let mut hook = AuditHook::new(sink.clone());
            if let Some(factory) = &self.redactor_factory {
                hook = hook.with_redactor(factory());
            }
            agent = agent.with_hook(Arc::new(hook));
        }
        if let Some(dir) = &self.replay_dir {
            let path = dir.join(session_id).join(format!("{request_id}.jsonl"));
            match SessionRecorder::new(&path) {
                Ok(rec) => agent = agent.with_hook(Arc::new(rec)),
                // Recording is best-effort — a disk error must not fail the chat.
                Err(e) => tracing::error!(
                    target: "harness.serve",
                    error = %e, path = %path.display(),
                    "failed to open replay recording; continuing without it",
                ),
            }
        }
        agent
    }

    /// Handle one turn of a conversation: authenticate `token`, run `message`
    /// against `session_id`'s history, persist the exchange, return the answer.
    pub async fn chat(
        &self,
        token: Option<&str>,
        session_id: &str,
        message: &str,
    ) -> Result<ChatReply, ServeError> {
        let actor = self.auth.authenticate(token)?;
        let request_id = new_request_id();
        let metadata = self.make_metadata(&actor, session_id, &request_id);
        let agent = self.build_agent(session_id, &request_id);

        let seed = self.sessions.history(session_id);
        let mut world = harness_context::default_world(&self.data_dir);
        let task = Task {
            description: message.to_string(),
            source: None,
            deadline: None,
        };

        let outcome = agent
            .run_with_seed_and_metadata(task, seed, metadata, &mut world, self.max_iters)
            .await
            .map_err(|e| ServeError::Agent(e.to_string()))?;

        let answer = answer_of(&outcome);
        self.sessions.append(session_id, message, &answer);

        Ok(ChatReply {
            answer,
            actor: actor.id,
            request_id,
        })
    }

    /// Streaming counterpart to [`chat`](Self::chat): yields [`ChatChunk::Token`]
    /// fragments as the model produces them and a final [`ChatChunk::Done`] with
    /// the full answer. Authentication happens up front (an auth failure returns
    /// `Err` here, before any stream); the agent runs on a spawned task that
    /// forwards `Event::ModelTokenDelta` into the returned stream and persists the
    /// exchange when it finishes.
    ///
    /// Providers without real token streaming still work: the loop falls back to
    /// a single delta (see [`AgentLoop::with_streaming`]).
    pub fn chat_stream(
        &self,
        token: Option<&str>,
        session_id: &str,
        message: &str,
    ) -> Result<
        impl futures::Stream<Item = Result<ChatChunk, ServeError>> + Send + 'static,
        ServeError,
    > {
        let actor = self.auth.authenticate(token)?;
        let request_id = new_request_id();
        let metadata = self.make_metadata(&actor, session_id, &request_id);

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<ChatChunk, ServeError>>();

        let agent = self
            .build_agent(session_id, &request_id)
            .with_streaming(true)
            .with_hook(Arc::new(StreamForwardHook { tx: tx.clone() }));

        // Everything the spawned run needs, owned.
        let seed = self.sessions.history(session_id);
        let data_dir = self.data_dir.clone();
        let max_iters = self.max_iters;
        let sessions = self.sessions.clone();
        let session_id = session_id.to_string();
        let message = message.to_string();
        let actor_id = actor.id;

        tokio::spawn(async move {
            let mut world = harness_context::default_world(&data_dir);
            let task = Task {
                description: message.clone(),
                source: None,
                deadline: None,
            };
            match agent
                .run_with_seed_and_metadata(task, seed, metadata, &mut world, max_iters)
                .await
            {
                Ok(outcome) => {
                    let answer = answer_of(&outcome);
                    sessions.append(&session_id, &message, &answer);
                    let _ = tx.send(Ok(ChatChunk::Done {
                        answer,
                        actor: actor_id,
                        request_id,
                    }));
                }
                Err(e) => {
                    let _ = tx.send(Err(ServeError::Agent(e.to_string())));
                }
            }
            // Dropping `tx` here closes the stream.
        });

        Ok(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }))
    }
}

/// A hook that forwards each streaming text fragment into the chat stream.
struct StreamForwardHook {
    tx: tokio::sync::mpsc::UnboundedSender<Result<ChatChunk, ServeError>>,
}

impl Hook for StreamForwardHook {
    fn name(&self) -> &str {
        "stream-forward"
    }

    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::ModelTokenDelta { .. })
    }

    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        if let Event::ModelTokenDelta { text } = ev {
            // Non-blocking; a dropped receiver (client gone) just errors — ignore.
            let _ = self.tx.send(Ok(ChatChunk::Token {
                text: text.to_string(),
            }));
        }
        HookOutcome::Allow
    }
}

/// Best-effort answer text from any terminal [`Outcome`] — a partial answer from
/// a budget-exhausted or stuck run beats an empty reply.
fn answer_of(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done { text, .. } => text.clone().unwrap_or_default(),
        Outcome::BudgetExhausted { last_text, .. } => last_text.clone().unwrap_or_default(),
        Outcome::Stuck { last_text, .. } => last_text.clone().unwrap_or_default(),
    }
}
