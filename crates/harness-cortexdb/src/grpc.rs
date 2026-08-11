//! `Memory` over CortexDB's own gRPC API — the protocol a running deployment
//! already serves.
//!
//! The MCP path in this crate spawns (or reaches) an MCP server. That is not
//! what an existing CortexDB is: the openclaw sidecar talks to `host:47821`
//! over gRPC, and a probe of that port answers nothing at all to HTTP. So an
//! agent that wanted to join the brain a cluster already runs had no way in —
//! `connect_http` speaks MCP Streamable HTTP, which is a different protocol on
//! a different port, and the resemblance of the two URLs hides that.
//!
//! Requires the `grpc` feature (off by default: it pulls in tonic/prost and
//! wants `protoc` at build time).
//!
//! ```ignore
//! let mem = Arc::new(CortexdbGrpcMemory::connect("http://127.0.0.1:47821").await?
//!     .with_scope("global")
//!     .with_user_id("liliang"));
//! let agent = AgentLoop::new(model).with_guide(Arc::new(MemoryGuide::new(mem)));
//! ```
//!
//! Field mapping is deliberately identical to the MCP path — see
//! [`split_tags`] — so the same entry written over either transport reads back
//! the same way. Two ways in, one brain.

use harness_core::{Memory, MemoryEntry, MemoryError};
use std::sync::atomic::{AtomicU64, Ordering};

/// Generated from `proto/cortexdb/v1/memory.proto`.
///
/// `common.proto` brings graph and tool messages along that a memory client
/// never constructs; they are part of the wire contract, not dead weight.
#[allow(dead_code)]
pub(crate) mod pb {
    tonic::include_proto!("cortexdb.v1");
}

use pb::memory_service_client::MemoryServiceClient;
use pb::{MemoryRecord, SaveMemoryRequest, SearchMemoryRequest};

/// A `Memory` backed by a CortexDB reachable over gRPC.
///
/// Cloning is cheap: `tonic`'s generated client is itself a cheap handle over a
/// shared connection, which is what lets one instance serve concurrent agents.
pub struct CortexdbGrpcMemory {
    client: MemoryServiceClient<tonic::transport::Channel>,
    scope: String,
    namespace: String,
    user_id: Option<String>,
    session_id: Option<String>,
    token: Option<String>,
    seq: AtomicU64,
}

impl CortexdbGrpcMemory {
    /// Connect to `endpoint`, e.g. `http://127.0.0.1:47821`.
    ///
    /// The scheme is required by tonic even though the transport underneath is
    /// gRPC — the openclaw config strips it, which is one reason that endpoint
    /// reads like an HTTP URL when it is not one.
    pub async fn connect(endpoint: impl Into<String>) -> anyhow::Result<Self> {
        let endpoint = endpoint.into();
        let client = MemoryServiceClient::connect(endpoint).await?;
        Ok(Self {
            client,
            scope: "global".into(),
            namespace: "default".into(),
            user_id: None,
            session_id: None,
            // A CortexDB started by the openclaw sidecar runs with
            // `auth=bearer token`, so this is not optional in practice — the
            // same variable the sidecar sets when it spawns the server.
            token: std::env::var("CORTEXDB_GRPC_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            seq: AtomicU64::new(0),
        })
    }

    /// Adapt an already-built client — for a caller that needs its own channel
    /// (TLS, interceptors, a load-balanced list of addresses).
    pub fn from_client(client: MemoryServiceClient<tonic::transport::Channel>) -> Self {
        Self {
            client,
            scope: "global".into(),
            namespace: "default".into(),
            user_id: None,
            session_id: None,
            token: std::env::var("CORTEXDB_GRPC_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            seq: AtomicU64::new(0),
        }
    }

    /// `global` (shared brain), `user`, or `session`. Default `global`.
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }

    /// Partition within the scope. Default `default`.
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    /// Whose memories these are. Required by CortexDB for `user` scope.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    /// Which conversation these memories belong to.
    ///
    /// CortexDB refuses `session` scope without one — `InvalidArgument:
    /// session_id is required for session scope` — so a memory configured for
    /// that scope and given no session is broken by construction, on recall if
    /// not on write. An individual entry can still override this with a
    /// `session:<id>` tag; this is the default the recall side uses, which a
    /// per-entry tag cannot supply.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Bearer token for a CortexDB started with authentication — which is the
    /// normal case: the server announces `auth=bearer token` at startup and
    /// answers `Unauthenticated: missing authorization metadata` without one.
    /// Defaults to `$CORTEXDB_GRPC_TOKEN`, the variable the openclaw sidecar
    /// sets when it spawns the server.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Wrap a message in a request carrying the bearer token, if there is one.
    fn authed<T>(&self, msg: T) -> tonic::Request<T> {
        let mut req = tonic::Request::new(msg);
        if let Some(t) = &self.token
            && let Ok(v) = format!("Bearer {t}").parse()
        {
            req.metadata_mut().insert("authorization", v);
        }
        req
    }

    fn next_id(&self) -> String {
        format!(
            "harness-{}-{}",
            std::process::id(),
            self.seq.fetch_add(1, Ordering::Relaxed)
        )
    }
}

/// Split harness tags into CortexDB's typed fields, exactly as the MCP path
/// does: `role:x` and `session:y` are promoted out of `tags`, the rest stay.
///
/// Shared shape rather than shared code because the two transports carry
/// different value types (`serde_json::Value` vs `prost_types::Struct`); what
/// has to match is the result, and the tests assert that it does.
pub(crate) fn split_tags(tags: &[String]) -> (Option<String>, Option<String>, Vec<String>) {
    let (mut role, mut session) = (None, None);
    let mut rest = Vec::new();
    for t in tags {
        if let Some(r) = t.strip_prefix("role:") {
            role = Some(r.to_string());
        } else if let Some(s) = t.strip_prefix("session:") {
            session = Some(s.to_string());
        } else {
            rest.push(t.clone());
        }
    }
    (role, session, rest)
}

/// `tags` + `source` into CortexDB's `metadata` struct, matching the MCP path.
fn build_metadata(tags: &[String], source: Option<&str>) -> Option<prost_types::Struct> {
    use prost_types::{ListValue, Struct, Value, value::Kind};
    let mut fields = std::collections::BTreeMap::new();
    if !tags.is_empty() {
        fields.insert(
            "tags".to_string(),
            Value {
                kind: Some(Kind::ListValue(ListValue {
                    values: tags
                        .iter()
                        .map(|t| Value {
                            kind: Some(Kind::StringValue(t.clone())),
                        })
                        .collect(),
                })),
            },
        );
    }
    if let Some(s) = source {
        fields.insert(
            "source".to_string(),
            Value {
                kind: Some(Kind::StringValue(s.to_string())),
            },
        );
    }
    if fields.is_empty() {
        None
    } else {
        Some(Struct {
            fields: fields.into_iter().collect(),
        })
    }
}

/// A CortexDB record back into a harness entry, reversing [`build_metadata`].
fn entry_from(rec: &MemoryRecord) -> MemoryEntry {
    use prost_types::value::Kind;

    let mut tags: Vec<String> = Vec::new();
    let mut source: Option<String> = None;
    if let Some(md) = &rec.metadata {
        if let Some(Kind::ListValue(list)) = md.fields.get("tags").and_then(|v| v.kind.as_ref()) {
            for v in &list.values {
                if let Some(Kind::StringValue(s)) = &v.kind {
                    tags.push(s.clone());
                }
            }
        }
        if let Some(Kind::StringValue(s)) = md.fields.get("source").and_then(|v| v.kind.as_ref()) {
            source = Some(s.clone());
        }
    }
    // Put the promoted fields back where a harness caller left them, so a
    // round-trip through CortexDB returns the tags it was given.
    if !rec.role.is_empty() {
        tags.push(format!("role:{}", rec.role));
    }
    if !rec.session_id.is_empty() {
        tags.push(format!("session:{}", rec.session_id));
    }

    let mut entry = MemoryEntry::new(rec.content.clone());
    entry.id = rec.id.clone();
    entry.tags = tags;
    entry.source = source;
    entry
}

#[async_trait::async_trait]
impl Memory for CortexdbGrpcMemory {
    async fn recall(&self, query: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        if k == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let req = SearchMemoryRequest {
            query: query.to_string(),
            user_id: self.user_id.clone().unwrap_or_default(),
            session_id: self.session_id.clone().unwrap_or_default(),
            scope: self.scope.clone(),
            namespace: self.namespace.clone(),
            top_k: k as i32,
            ..Default::default()
        };
        let resp = self
            .client
            .clone()
            .search_memory(self.authed(req))
            .await
            .map_err(|e| MemoryError::Backend(format!("SearchMemory: {e}")))?
            .into_inner();

        Ok(resp
            .results
            .iter()
            .filter_map(|hit| hit.memory.as_ref())
            .map(entry_from)
            .collect())
    }

    async fn write(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        let id = if entry.id.is_empty() {
            self.next_id()
        } else {
            entry.id.clone()
        };
        let (role, session, tags) = split_tags(&entry.tags);
        let req = SaveMemoryRequest {
            memory_id: id,
            user_id: self.user_id.clone().unwrap_or_default(),
            // A per-entry `session:` tag wins; otherwise the configured default.
            session_id: session
                .or_else(|| self.session_id.clone())
                .unwrap_or_default(),
            scope: self.scope.clone(),
            namespace: self.namespace.clone(),
            role: role.unwrap_or_default(),
            content: entry.content,
            metadata: build_metadata(&tags, entry.source.as_deref()),
            ..Default::default()
        };
        self.client
            .clone()
            .save_memory(self.authed(req))
            .await
            .map_err(|e| MemoryError::Backend(format!("SaveMemory: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two transports must agree on what a tag means, or an entry written
    /// over gRPC reads back differently over MCP — same brain, two dialects.
    #[test]
    fn tags_split_the_way_the_mcp_path_splits_them() {
        let tags = vec![
            "role:user".to_string(),
            "session:s1".to_string(),
            "project".to_string(),
        ];
        let (role, session, rest) = split_tags(&tags);
        assert_eq!(role.as_deref(), Some("user"));
        assert_eq!(session.as_deref(), Some("s1"));
        assert_eq!(rest, vec!["project".to_string()]);
    }

    #[test]
    fn metadata_carries_tags_and_source() {
        let md = build_metadata(&["a".into(), "b".into()], Some("session")).expect("some");
        assert!(md.fields.contains_key("tags"));
        assert!(md.fields.contains_key("source"));
        // Nothing to say means no struct at all, rather than an empty one.
        assert!(build_metadata(&[], None).is_none());
    }

    /// A record has to come back as the entry that went in — including the
    /// tags that were promoted into typed fields on the way out.
    #[test]
    fn a_record_round_trips_into_an_entry() {
        let entry = MemoryEntry::new("the deploy needs a tag push")
            .with_source("chat")
            .with_tags(["role:user", "session:s1", "release"]);
        let (role, session, rest) = split_tags(&entry.tags);
        let rec = MemoryRecord {
            id: "m1".into(),
            role: role.unwrap_or_default(),
            session_id: session.unwrap_or_default(),
            content: entry.content.clone(),
            metadata: build_metadata(&rest, entry.source.as_deref()),
            ..Default::default()
        };

        let back = entry_from(&rec);
        assert_eq!(back.content, entry.content);
        assert_eq!(back.source.as_deref(), Some("chat"));
        for t in ["release", "role:user", "session:s1"] {
            assert!(
                back.tags.contains(&t.to_string()),
                "lost {t}: {:?}",
                back.tags
            );
        }
    }
}
