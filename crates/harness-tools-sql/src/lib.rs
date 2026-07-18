//! Safe read-only SQL for harness-rs agents — the reusable core of "connect the
//! agent to an ERP / MES / BI database" without letting it touch production data
//! it shouldn't.
//!
//! What this crate provides (the framework's 20% that's the same everywhere):
//! - a **read-only guard** ([`check_read_only`]) — SELECT/WITH only, single
//!   statement, no writes/DDL — plus [`enforce_limit`] so results are bounded;
//! - a driver-pluggable [`SqlExecutor`] seam — MySQL/PostgreSQL/SQL Server/API
//!   are each one impl; SQLite ([`SqliteExecutor`], feature `sqlite`) is the
//!   in-CI reference;
//! - a [`SqlQueryTool`] the agent calls, with result **redaction** and a
//!   schema-doc-in-description contract so NL2SQL lands.
//!
//! What it deliberately does NOT provide (the per-deployment 80%, i.e. the
//! billable consulting): the vendor schema knowledge (用友 U8/T+, 金蝶 K3/云星空,
//! and each MES differ wildly and are often undocumented), read access to the
//! customer's system, and NL2SQL accuracy tuning on that schema.
//!
//! **Primary safety control is a read-only DB account.** This guard is
//! defense-in-depth; pair it with least-privilege credentials and a read replica.
//!
//! # Caveat: this is a *fallback*, not the right tool for correctness-critical BI
//!
//! Letting a model write raw SQL — even read-only — cannot prevent it from
//! inventing a wrong join, picking the wrong grain, or fanning a total out
//! through a one-to-many join into a **confident, clean, wrong number**. Those
//! failures aren't a `WHERE` clause away; they're structural. This crate blocks
//! *writes*, not *wrong answers*.
//!
//! For BI where the number has to be right, put a **governed semantic layer**
//! between the agent and the warehouse: the agent asks for a *metric by
//! dimensions*, and a compiler produces fan-out/chasm-safe SQL. harness-rs talks
//! to such a layer (e.g. DataIntelligence) over MCP with no code here at all —
//! see `verticals/warehouse-bi-agent`. Reach for `SqlQueryTool` only when there
//! is no semantic model yet and approximate ad-hoc reads are acceptable.

pub mod executor;
pub mod guard;
pub mod tool;

#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use executor::{Row, SqlError, SqlExecutor};
pub use guard::{GuardError, check_read_only, enforce_limit};
pub use tool::SqlQueryTool;

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteExecutor;
