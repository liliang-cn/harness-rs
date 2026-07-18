//! Server-streaming gRPC shell over [`ChatService`] (feature `grpc`).
//!
//! Mirrors the HTTP layer: `Say` is unary, `SayStream` streams
//! [`ChatChunk`](crate::ChatChunk)s. The bearer token is read from request
//! metadata (`authorization: Bearer …`). All the substance lives in
//! [`ChatService`]; this only translates gRPC ⇄ those calls.
//!
//! ```ignore
//! let svc = std::sync::Arc::new(chat_service);
//! tonic::transport::Server::builder()
//!     .add_service(harness_serve::grpc::service(svc))
//!     .serve("127.0.0.1:43518".parse()?)
//!     .await?;
//! ```

use crate::auth::AuthError;
use crate::service::{ChatChunk as SvcChunk, ChatService, ServeError};
use futures::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// Generated protobuf types (`proto/chat.proto`).
pub mod pb {
    tonic::include_proto!("harness.serve.v1");
}

use pb::chat_server::{Chat, ChatServer};
use pb::{ChatChunk, ChatReply, ChatRequest};

/// Adapts a [`ChatService`] to the generated `Chat` gRPC service.
pub struct ChatGrpc {
    service: Arc<ChatService>,
}

impl ChatGrpc {
    pub fn new(service: Arc<ChatService>) -> Self {
        Self { service }
    }
}

/// Wrap a [`ChatService`] into a tonic service ready for `Server::add_service`.
pub fn service(service: Arc<ChatService>) -> ChatServer<ChatGrpc> {
    ChatServer::new(ChatGrpc::new(service))
}

/// Extract a bearer token from gRPC request metadata.
fn bearer(meta: &tonic::metadata::MetadataMap) -> Option<String> {
    meta.get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Map a serving error onto a gRPC status.
fn to_status(e: ServeError) -> Status {
    match e {
        ServeError::Auth(AuthError::Unauthorized) => Status::unauthenticated("invalid credentials"),
        ServeError::Auth(AuthError::Forbidden) => Status::permission_denied("not permitted"),
        ServeError::Agent(msg) => Status::internal(msg),
    }
}

/// Convert a service chunk into its protobuf form.
fn to_pb_chunk(chunk: SvcChunk) -> ChatChunk {
    let kind = match chunk {
        SvcChunk::Token { text } => pb::chat_chunk::Kind::Token(text),
        SvcChunk::Done {
            answer,
            actor,
            request_id,
        } => pb::chat_chunk::Kind::Done(ChatReply {
            answer,
            actor,
            request_id,
        }),
    };
    ChatChunk { kind: Some(kind) }
}

type ChunkStream = Pin<Box<dyn futures::Stream<Item = Result<ChatChunk, Status>> + Send>>;

#[tonic::async_trait]
impl Chat for ChatGrpc {
    async fn say(&self, request: Request<ChatRequest>) -> Result<Response<ChatReply>, Status> {
        let token = bearer(request.metadata());
        let req = request.into_inner();
        let reply = self
            .service
            .chat(token.as_deref(), &req.session_id, &req.message)
            .await
            .map_err(to_status)?;
        Ok(Response::new(ChatReply {
            answer: reply.answer,
            actor: reply.actor,
            request_id: reply.request_id,
        }))
    }

    type SayStreamStream = ChunkStream;

    async fn say_stream(
        &self,
        request: Request<ChatRequest>,
    ) -> Result<Response<Self::SayStreamStream>, Status> {
        let token = bearer(request.metadata());
        let req = request.into_inner();
        // Auth failures surface here, before the stream opens.
        let stream = self
            .service
            .chat_stream(token.as_deref(), &req.session_id, &req.message)
            .map_err(to_status)?;
        let mapped = stream.map(|item| item.map(to_pb_chunk).map_err(to_status));
        Ok(Response::new(Box::pin(mapped) as ChunkStream))
    }
}
