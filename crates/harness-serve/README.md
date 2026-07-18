# harness-rs-serve

Multi-session **serving core** for [harness-rs](https://github.com/liliang-cn/harness-rs)
agents — the layer between "an agent loop you call in `main`" and "a service
your whole company talks to."

harness-rs gives you the agent loop, tools, memory, and hooks. This crate adds
what a **single-machine SMB deployment** (process / policy / BI assistants) needs
before it can face real users:

- **Pluggable auth** — an `Authenticator` maps a bearer token to an `Actor`.
  Ships `StaticTokenAuth` (token → actor) and a dev-only `OpenAuth`; implement
  the trait for real SSO.
- **Per-conversation session history** — a `SessionStore` keeps each chat's
  turns; history is seeded into `ctx.history` (compactor-visible), not glued into
  the prompt. `InMemorySessions` is the zero-config default.
- **Per-request identity + routing flags** — actor id and session id are stamped
  into `Context.metadata`, and an `Actor` can carry flags like
  `router.keep_local` to pin (say) HR to the local model.
- **Wired audit trail** — attach an `AuditSink` and every request records who
  asked what, what the AI answered, and which tools (data sources) it touched. A
  fresh audit hook per request means concurrent callers never share state.
- **Auditable & traceable** — every request gets a `request_id` (returned in
  `ChatReply` / the terminal `Done` chunk) that is stamped into the audit trail,
  the OTel trace, **and** the replay recording. One id ties all three together.
  Turn on `.with_replay(dir)` to record each run for deterministic replay, and use
  `harness_hooks::HashChainSink` for a tamper-evident (hash-chained) audit log
  that `verify_chain` can check for deletion / edits / reordering.

Two transport-agnostic entry points, same pipeline:

- `chat(token, session, message)` — awaits the full answer (unary).
- `chat_stream(token, session, message)` — yields `ChatChunk`s as tokens arrive,
  ending with a terminal `Done`. Powers HTTP SSE and server-streaming gRPC.

The heart is `ChatService`.

```rust,ignore
use harness_serve::{ChatService, Actor, StaticTokenAuth, InMemorySessions};
use harness_hooks::JsonlAuditSink;
use harness_models::{ApiKind, ModelRouter, KEEP_LOCAL_KEY};
use harness_redact::Redactor;
use std::sync::Arc;

// Local-first model with cloud fallback (harness_models::ModelRouter).
let local = ApiKind::OpenAI.build("http://localhost:11434/v1", "qwen2.5:14b", "ollama");
let cloud = ApiKind::Anthropic.build("https://api.anthropic.com", "claude-opus-4-8", key);
let model = Arc::new(ModelRouter::new(local).with_fallback(cloud));

// Tokens → actors; HR stays on the local model.
let auth = Arc::new(StaticTokenAuth::new()
    .with_token("tok-alice", Actor::new("alice@sales"))
    .with_token("tok-bob", Actor::new("bob@hr").with_flag(KEEP_LOCAL_KEY, true)));

let service = ChatService::new(model, auth, Arc::new(InMemorySessions::new()), "/var/lib/app")
    .with_audit(Arc::new(JsonlAuditSink::new("/var/lib/app/audit.jsonl")?))
    .with_audit_redaction(Redactor::new);

// Call it directly from a CLI / IM bot:
let reply = service.chat(Some("tok-alice"), "session-1", "what is our refund policy?").await?;
```

## HTTP layer (feature `http`)

Enable `http` for a ready [axum](https://docs.rs/axum) router:

```rust,ignore
let app = harness_serve::http::router(Arc::new(service));
let listener = tokio::net::TcpListener::bind("127.0.0.1:43517").await?;
axum::serve(listener, app).await?;
```

| Route | Description |
|---|---|
| `POST /chat` | Unary. JSON body `{ "session_id", "message" }` → `{ "answer", "actor" }`. |
| `POST /chat/stream` | Same body, streams the answer token-by-token over **SSE**; each frame is a JSON `ChatChunk`. |
| `GET /healthz` | Liveness. |

The bearer token comes from `Authorization: Bearer <token>`; auth failures map to
401/403.

## gRPC layer (feature `grpc`)

Enable `grpc` for a [tonic](https://docs.rs/tonic) service — unary `Say` and
**server-streaming** `SayStream` (see `proto/chat.proto`). Requires `protoc` at
build time.

```rust,ignore
tonic::transport::Server::builder()
    .add_service(harness_serve::grpc::service(Arc::new(service)))
    .serve("127.0.0.1:43518".parse()?)
    .await?;
```

The bearer token travels in gRPC request metadata (`authorization: Bearer …`);
auth failures map to `UNAUTHENTICATED` / `PERMISSION_DENIED`.

## What this crate does *not* do

It is a serving **core**, not a finished product. A UI, per-department data
visibility beyond the `keep_local` flag, and durable session/audit storage are
left to the deployment — the traits (`Authenticator`, `SessionStore`,
`AuditSink`) are the seams to plug those in.

## License

MIT OR Apache-2.0
