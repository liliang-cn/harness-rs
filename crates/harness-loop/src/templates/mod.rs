//! Pre-built Blueprint templates per DESIGN.md §15 (v0.1 roadmap).
//!
//! Each template ships:
//! - a `Blueprint` factory (deterministic + agent nodes wired)
//! - a `tools` helper that builds the recommended toolset
//! - a `sensors` helper for the recommended sensors
//! - a curated set of `Guide` instances installed at startup

pub mod axum_crud;
pub mod crate_keeper;
