//! The tools an agent reaches for, in one crate.
//!
//! These were twelve crates once. Every one of them depended on nothing a
//! caller would want to avoid — `serde`, `tokio`, `async-trait` — so the
//! boundaries between them bought a consumer nothing while costing every
//! release twelve more publishes and every host twelve more version pins.
//! The tools that DO carry a heavy dependency of their own are still separate
//! crates, and that is the rule: a crate boundary here has to name a
//! dependency you might not want.
pub mod agents;
pub mod browser;
pub mod datetime;
pub mod fs;
pub mod mcp;
pub mod memory;
pub mod recall;
pub mod sensors_common;
pub mod sensors_rust;
pub mod shell;
pub mod skills;
pub mod tasks;
