//! Core traits and types for the **harness** agent framework.
//!
//! This crate is intentionally **dependency-light** and **runtime-agnostic** so every
//! upper-layer crate can share a single source of truth for the framework's vocabulary.
//!
//! See `DESIGN.md` at the workspace root for architectural intent.

/// Re-exports used by the procedural macros in `harness-macros`. Not part of
/// the stable public API — users should never need to reference these directly.
#[doc(hidden)]
pub mod __export {
    pub use async_trait::async_trait;
    pub use futures;
    pub use inventory;
    pub use serde_json;
}

pub mod compactor;
pub mod context;
pub mod embed;
pub mod error;
pub mod event;
pub mod guide;
pub mod hook;
pub mod memory;
pub mod model;
pub mod profile;
pub mod recall;
pub mod sensor;
pub mod signal;
pub mod skill;
pub mod tool;
pub mod world;

pub use compactor::*;
pub use context::*;
pub use embed::*;
pub use error::*;
pub use event::*;
pub use guide::*;
pub use hook::*;
pub use memory::*;
pub use model::*;
pub use profile::*;
pub use recall::*;
pub use sensor::*;
pub use signal::*;
pub use skill::*;
pub use tool::*;
pub use world::*;

/// How a control-plane component executes:
/// - **Computational**: deterministic, cheap, fast (CPU-bound: linter, type checker, AST tools).
/// - **Inferential**: model-driven, slower, probabilistic (review agent, semantic dup detection).
///
/// This dichotomy is from Böckeler (2026); see DESIGN.md §3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Execution {
    Computational,
    Inferential,
}
