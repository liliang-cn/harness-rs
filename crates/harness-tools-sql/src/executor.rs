//! The driver-pluggable seam.
//!
//! "How you read data" from an ERP/MES is deployment-specific: a MySQL/PostgreSQL
//! pool, a SQL Server connection (via a separate driver — sqlx dropped MSSQL),
//! a vendor REST API, or a nightly export into an intermediate table. A
//! [`SqlExecutor`] is that seam — implement it once per customer backend, and the
//! [`SqlQueryTool`](crate::SqlQueryTool) (guard + LIMIT + redaction + the agent
//! contract) rides on top unchanged.

use async_trait::async_trait;
use serde_json::{Map, Value};

/// A single result row as a JSON object (column name → value).
pub type Row = Map<String, Value>;

/// Failure from a backend query.
#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    #[error("query failed: {0}")]
    Query(String),
    #[error("backend unavailable: {0}")]
    Backend(String),
}

/// Executes already-validated read-only SQL against some backend.
#[async_trait]
pub trait SqlExecutor: Send + Sync + 'static {
    /// Run `sql` (guaranteed read-only + bounded by the tool) and return rows.
    async fn query(&self, sql: &str) -> Result<Vec<Row>, SqlError>;

    /// Human-readable schema documentation (tables, columns, semantics) injected
    /// into the tool description so the model writes valid SQL on the first try.
    /// This is the per-deployment knowledge that makes NL2SQL accurate.
    fn schema_doc(&self) -> &str;
}
