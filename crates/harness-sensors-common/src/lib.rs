//! Language-agnostic sensors (placeholder).
//!
//! This crate is reserved for sensors that don't bind to a specific
//! language toolchain — e.g. a `GitDirty` sensor that flags uncommitted
//! changes after a task, an `LspDiagnostics` sensor that surfaces editor
//! warnings, or a `RipgrepTodo` sensor that finds new `TODO`/`FIXME`
//! markers left behind by the agent.
//!
//! As of 0.0.x none of these are implemented yet — the crate exists so the
//! `harness-rs-sensors-common` name is reserved on crates.io and so
//! workspace-level dependency wiring is stable. The companion crate
//! `harness-rs-sensors-rust` ships the working Rust-specific sensors today.
//!
//! Writing your own sensor is a one-function affair via `#[sensor]`; see the
//! `harness-rs-macros` crate.
