//! CAP core — a hashline coding agent on harness-rs, shared by the `cap` (CLI)
//! and `cap-tui` (ratatui) front-ends. Everything except the terminal UX lives
//! here, so a front-end is just: build a model, assemble [`agent::LoopParts`]
//! with its own UI hook, and drive [`agent::build_loop`].

pub mod agent;
pub mod commands;
pub mod guides;
pub mod hashline;
pub mod jail;
pub mod lsp;
pub mod sensor;
pub mod session;
pub mod tools;
pub mod ui;
