//! A governed semantic layer: a model (entities, joins, dimensions, metrics)
//! plus a compiler that turns *"this metric by these dimensions"* into
//! fan-out/chasm-safe SQL.
//!
//! It speaks no LLM and opens no database. It produces SQL strings.
//!
//! # Why this exists
//!
//! Letting a model write raw SQL — even read-only — cannot stop it inventing a
//! wrong join, picking the wrong grain, or fanning a total out through a
//! one-to-many join into a **confident, clean, wrong number**. Those failures
//! aren't a `WHERE` clause away; they're structural, and no amount of prompt
//! makes them go away.
//!
//! So the agent doesn't get to write SQL. It asks for a *metric by dimensions*,
//! and this compiles it. The core move is one sentence long: **aggregate each
//! measure to its base grain inside its own CTE first, then join dimensions.**
//! Fan-out and chasm joins become impossible by construction rather than
//! something a reviewer has to catch.
//!
//! ```
//! use harness_semantic::{Model, Query, dialect};
//!
//! let m = Model::from_yaml(r#"
//! entities:
//!   - {name: order, table: orders, primary_key: id}
//!   - {name: store, table: stores, primary_key: id}
//! joins:
//!   - {from: order, to: store, from_key: store_id, to_key: id, cardinality: many_to_one}
//! dimensions:
//!   - {name: region, entity: store, column: region, type: categorical}
//! metrics:
//!   - {name: revenue, entity: order, agg: sum, expr: amount}
//! "#).unwrap();
//!
//! let q = Query { metrics: vec!["revenue".into()], group_by: vec!["region".into()], ..Default::default() };
//! let out = harness_semantic::compile(&m, &q, &dialect::Postgres).unwrap();
//! assert!(out.sql.contains("SUM(amount)"));
//! ```

pub mod compile;
pub mod dialect;
pub mod governance;
pub mod lint;
pub mod model;
pub mod reach;

pub use compile::{Compiled, CompileError, Filter, Query, Value, compile};
pub use dialect::Dialect;
pub use governance::{GovernanceError, Policy, Principal, RowFilter};
pub use lint::{Issue, lint};
pub use model::{
    ADDITIVE, Dimension, Entity, Join, Metric, Model, ModelError, NON_ADDITIVE, SEMI_ADDITIVE,
};
