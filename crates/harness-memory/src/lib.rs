//! Every memory and recall backend behind one door.
//!
//! The interfaces are [`harness_core::Memory`] (curated facts) and
//! [`harness_core::RecallStore`] (raw transcripts). This crate holds the
//! implementations and one way to pick between them: a URL.
//!
//! | URL                        | store                                  | feature          | memory | recall |
//! |----------------------------|----------------------------------------|------------------|--------|--------|
//! | `file://<path>`            | JSONL on disk (harness-context)        | none             | ✓      | ✓      |
//! | `sqlite://<file>`          | SQLite, FTS5 + trigram                 | `sqlite`         |        | ✓      |
//! | `mem://`                   | SurrealDB in-process, not persisted    | `surreal`        | ✓      | ✓      |
//! | `surrealkv://<dir>`        | SurrealDB in-process, on disk          | `surreal`        | ✓      | ✓      |
//! | `ws://host:port`           | a running `surreal start`              | `surreal-remote` | ✓      | ✓      |
//! | `cortexdb+http://<url>`    | CortexDB over MCP                      | `cortexdb`       | ✓      |        |
//! | `cortexdb://host:port`     | CortexDB over gRPC                     | `cortexdb-grpc`  | ✓      |        |
//!
//! ```no_run
//! # async fn demo() -> Result<(), harness_core::MemoryError> {
//! let memory = harness_memory::open_memory("surrealkv:///var/lib/myapp/memory").await?;
//! memory.write(harness_core::MemoryEntry::new("user prefers dark mode")).await?;
//! # Ok(()) }
//! ```
//!
//! A URL for a backend that was not compiled in is an error naming the
//! feature to enable, not a silent fallback to the file store: the operator
//! asked for a specific store and should get it or hear why not.
//!
//! Each backend is also a public module, for callers that want the concrete
//! type — [`surreal::SurrealMemory`] has `compact()`, `sqlite::SqliteRecall`
//! opens in memory, and so on.

use harness_core::{Embedder, Memory, MemoryError, RecallError, RecallStore};
use std::sync::Arc;

#[cfg(feature = "sqlite")]
pub mod sqlite;
#[cfg(feature = "surreal")]
pub mod surreal;
/// The CortexDB backends, re-exported so one crate is enough to name them.
#[cfg(feature = "cortexdb")]
pub mod cortexdb {
    pub use harness_cortexdb::*;
}

/// What a memory URL resolved to. Parsing needs no feature; opening does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backend {
    /// `file://<path>`. A `.jsonl` file for memory; a directory for recall.
    File(String),
    /// `sqlite://<file>` or `sqlite://:memory:`.
    Sqlite(String),
    /// `mem://`, `surrealkv://<dir>`, `ws://…`, `wss://…` — kept whole,
    /// SurrealDB's own connector reads the scheme.
    Surreal(String),
    /// `cortexdb+http://<url>` — the MCP endpoint, scheme prefix stripped.
    CortexdbHttp(String),
    /// `cortexdb://host:port` — a gRPC endpoint, `http://` prepended.
    CortexdbGrpc(String),
}

impl Backend {
    /// Which URL scheme names which store. A bare path is a file path.
    pub fn parse(url: &str) -> Result<Self, MemoryError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(MemoryError::Backend("empty memory url".into()));
        }
        let Some((scheme, rest)) = url.split_once("://") else {
            return Ok(Self::File(url.to_string()));
        };
        Ok(match scheme {
            "file" => Self::File(rest.to_string()),
            "sqlite" => Self::Sqlite(rest.to_string()),
            "mem" | "surrealkv" | "ws" | "wss" => Self::Surreal(url.to_string()),
            "cortexdb+http" => Self::CortexdbHttp(format!("http://{rest}")),
            "cortexdb+https" => Self::CortexdbHttp(format!("https://{rest}")),
            "cortexdb" => Self::CortexdbGrpc(format!("http://{rest}")),
            other => {
                return Err(MemoryError::Backend(format!(
                    "unknown memory backend `{other}://` (know: file, sqlite, mem, surrealkv, ws, wss, cortexdb, cortexdb+http)"
                )));
            }
        })
    }

    /// The feature that compiles this backend in, if it is not always on.
    pub fn feature(&self) -> Option<&'static str> {
        match self {
            Self::File(_) => None,
            Self::Sqlite(_) => Some("sqlite"),
            Self::Surreal(u) if u.starts_with("ws") => Some("surreal-remote"),
            Self::Surreal(_) => Some("surreal"),
            Self::CortexdbHttp(_) => Some("cortexdb"),
            Self::CortexdbGrpc(_) => Some("cortexdb-grpc"),
        }
    }
}

fn not_compiled(b: &Backend) -> MemoryError {
    MemoryError::Backend(format!(
        "{b:?} needs feature `{}` of harness-rs-memory",
        b.feature().unwrap_or("?")
    ))
}

/// Opening a store, with the knobs that some backends take. `open_memory`
/// and `open_recall` are the no-knob shortcuts.
#[derive(Clone)]
pub struct Open {
    url: String,
    embedder: Option<Arc<dyn Embedder>>,
    root: Option<(String, String)>,
    namespace: Option<String>,
    database: Option<String>,
}

impl Open {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            embedder: None,
            root: None,
            namespace: None,
            database: None,
        }
    }

    /// Vector recall for the stores that can index vectors (SurrealDB).
    /// Others ignore it.
    pub fn embedder(mut self, embedder: Arc<dyn Embedder>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Credentials for a remote server that wants them.
    pub fn root(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.root = Some((username.into(), password.into()));
        self
    }

    /// Namespace within a multi-tenant store (SurrealDB namespace, CortexDB
    /// namespace). Each backend has its own default.
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    /// Database within the namespace, where the store has that level.
    pub fn database(mut self, db: impl Into<String>) -> Self {
        self.database = Some(db.into());
        self
    }

    pub fn backend(&self) -> Result<Backend, MemoryError> {
        Backend::parse(&self.url)
    }

    /// The facts store.
    pub async fn memory(self) -> Result<Arc<dyn Memory>, MemoryError> {
        let backend = self.backend()?;
        match &backend {
            Backend::File(path) => Ok(Arc::new(harness_context::FileMemory::open(path.as_str())?)),
            Backend::Sqlite(_) => Err(MemoryError::Backend(
                "sqlite:// stores transcripts (open_recall), not facts; use file://, surrealkv:// or cortexdb:// for memory".into(),
            )),
            #[cfg(feature = "surreal")]
            Backend::Surreal(_) => Ok(Arc::new(self.surreal(&backend).await?)),
            #[cfg(feature = "cortexdb")]
            Backend::CortexdbHttp(url) => {
                let mut m = harness_cortexdb::CortexdbMemory::connect_http(url)
                    .await
                    .map_err(|e| MemoryError::Backend(format!("cortexdb {url}: {e}")))?;
                if let Some(ns) = &self.namespace {
                    m = m.with_namespace(ns.clone());
                }
                Ok(Arc::new(m))
            }
            #[cfg(feature = "cortexdb-grpc")]
            Backend::CortexdbGrpc(endpoint) => {
                let mut m = harness_cortexdb::CortexdbGrpcMemory::connect(endpoint.clone())
                    .await
                    .map_err(|e| MemoryError::Backend(format!("cortexdb {endpoint}: {e}")))?;
                if let Some(ns) = &self.namespace {
                    m = m.with_namespace(ns.clone());
                }
                Ok(Arc::new(m))
            }
            #[allow(unreachable_patterns)]
            other => Err(not_compiled(other)),
        }
    }

    /// The transcript store.
    pub async fn recall(self) -> Result<Arc<dyn RecallStore>, RecallError> {
        let backend = self.backend().map_err(recall_err)?;
        match &backend {
            Backend::File(path) => Ok(Arc::new(harness_context::FileRecall::open(path.as_str())?)),
            #[cfg(feature = "sqlite")]
            Backend::Sqlite(path) => Ok(Arc::new(if path == ":memory:" {
                sqlite::SqliteRecall::open_in_memory()?
            } else {
                sqlite::SqliteRecall::open(path)?
            })),
            #[cfg(feature = "surreal")]
            Backend::Surreal(_) => Ok(Arc::new(self.surreal(&backend).await.map_err(recall_err)?)),
            Backend::CortexdbHttp(_) | Backend::CortexdbGrpc(_) => Err(RecallError::Backend(
                "cortexdb stores facts (open_memory), not transcripts; use file://, sqlite:// or surrealkv:// for recall".into(),
            )),
            #[allow(unreachable_patterns)]
            other => Err(recall_err(not_compiled(other))),
        }
    }

    #[cfg(feature = "surreal")]
    async fn surreal(&self, backend: &Backend) -> Result<surreal::SurrealMemory, MemoryError> {
        let Backend::Surreal(url) = backend else {
            unreachable!("caller matched Backend::Surreal")
        };
        #[cfg(not(feature = "surreal-remote"))]
        if url.starts_with("ws") {
            return Err(not_compiled(backend));
        }
        let mut cfg = surreal::SurrealConfig::new(url.clone());
        if let Some(ns) = &self.namespace {
            cfg = cfg.namespace(ns.clone());
        }
        if let Some(db) = &self.database {
            cfg = cfg.database(db.clone());
        }
        if let Some((u, p)) = &self.root {
            cfg = cfg.root(u.clone(), p.clone());
        }
        if let Some(e) = &self.embedder {
            cfg = cfg.embedder(e.clone());
        }
        surreal::SurrealMemory::open(cfg).await
    }
}

fn recall_err(e: MemoryError) -> RecallError {
    RecallError::Backend(e.to_string())
}

/// [`Open::memory`] with defaults.
pub async fn open_memory(url: &str) -> Result<Arc<dyn Memory>, MemoryError> {
    Open::new(url).memory().await
}

/// [`Open::recall`] with defaults.
pub async fn open_recall(url: &str) -> Result<Arc<dyn RecallStore>, RecallError> {
    Open::new(url).recall().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{MemoryEntry, RecallMessage, SessionMeta};

    #[test]
    fn urls_name_backends() {
        assert_eq!(
            Backend::parse("file:///tmp/m.jsonl").unwrap(),
            Backend::File("/tmp/m.jsonl".into())
        );
        assert_eq!(
            Backend::parse("/tmp/m.jsonl").unwrap(),
            Backend::File("/tmp/m.jsonl".into()),
            "a bare path is a file"
        );
        assert_eq!(
            Backend::parse("sqlite://recall.db").unwrap(),
            Backend::Sqlite("recall.db".into())
        );
        assert_eq!(
            Backend::parse("surrealkv:///data/mem").unwrap(),
            Backend::Surreal("surrealkv:///data/mem".into())
        );
        assert_eq!(
            Backend::parse("cortexdb://192.168.1.2:47821").unwrap(),
            Backend::CortexdbGrpc("http://192.168.1.2:47821".into())
        );
        assert_eq!(
            Backend::parse("cortexdb+http://brain:8080/mcp").unwrap(),
            Backend::CortexdbHttp("http://brain:8080/mcp".into())
        );
        assert!(Backend::parse("redis://x").is_err());
        assert!(Backend::parse("").is_err());
    }

    #[test]
    fn each_backend_knows_its_feature() {
        assert_eq!(Backend::parse("file://x").unwrap().feature(), None);
        assert_eq!(
            Backend::parse("sqlite://x").unwrap().feature(),
            Some("sqlite")
        );
        assert_eq!(Backend::parse("mem://").unwrap().feature(), Some("surreal"));
        assert_eq!(
            Backend::parse("ws://localhost:8000").unwrap().feature(),
            Some("surreal-remote")
        );
        assert_eq!(
            Backend::parse("cortexdb://h:1").unwrap().feature(),
            Some("cortexdb-grpc")
        );
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("harness-memory-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn file_backend_needs_no_feature() {
        let dir = scratch("file");
        let m = open_memory(&format!("file://{}", dir.join("m.jsonl").display()))
            .await
            .unwrap();
        m.write(MemoryEntry::new("likes tea")).await.unwrap();
        assert_eq!(m.recall("tea", 5).await.unwrap().len(), 1);

        let r = open_recall(&format!("file://{}", dir.join("recall").display()))
            .await
            .unwrap();
        r.ensure_session("o", "s", &SessionMeta::new("s", 1))
            .await
            .unwrap();
        r.append("o", "s", &RecallMessage::new("user", "hello", 1))
            .await
            .unwrap();
        assert_eq!(r.recent("o", 5).await.unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_backend_that_is_not_compiled_in_says_which_feature() {
        // sqlite:// never serves memory, whatever is compiled in.
        let e = open_memory("sqlite://x.db")
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("open_recall"), "{e}");

        #[cfg(not(feature = "surreal"))]
        {
            let e = open_memory("mem://").await.err().unwrap().to_string();
            assert!(e.contains("feature `surreal`"), "{e}");
        }
        #[cfg(not(feature = "sqlite"))]
        {
            let e = open_recall("sqlite://x.db")
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(e.contains("feature `sqlite`"), "{e}");
        }
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn sqlite_url_opens_recall() {
        let r = open_recall("sqlite://:memory:").await.unwrap();
        r.ensure_session("o", "s", &SessionMeta::new("s", 1))
            .await
            .unwrap();
        r.append("o", "s", &RecallMessage::new("user", "hello sqlite", 1))
            .await
            .unwrap();
        assert_eq!(r.search("o", "sqlite", 5).await.unwrap().len(), 1);
    }

    #[cfg(feature = "surreal")]
    #[tokio::test]
    async fn surreal_url_opens_both() {
        let m = open_memory("mem://").await.unwrap();
        m.write(MemoryEntry::new("surreal fact")).await.unwrap();
        assert_eq!(m.recall("surreal", 5).await.unwrap().len(), 1);
        let r = open_recall("mem://").await.unwrap();
        r.append("o", "s", &RecallMessage::new("user", "surreal line", 1))
            .await
            .unwrap();
        assert_eq!(r.search("o", "surreal", 5).await.unwrap().len(), 1);
    }
}
