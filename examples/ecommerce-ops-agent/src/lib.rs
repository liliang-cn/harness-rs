//! Autonomous e-commerce operations agent — shared modules.
//!
//! Composes the full harness-rs stack: `harness-orchestrator` (concurrent
//! DAG + dynamic replanning + retry/backoff + resumable state),
//! `harness-loop-engine` (maturity levels + human gates + action executors),
//! `harness-core` Memory, and custom tools over a live PostgreSQL database.

pub mod action;
pub mod actionspec;
pub mod govern;
pub mod memory;
pub mod planner;
pub mod runner;
pub mod schema;
pub mod tools;
