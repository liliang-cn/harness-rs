//! `harness-rs` — facade re-exporting the public surface of the harness-rs
//! agent framework.
//!
//! Most users depend only on this crate. It re-exports `harness-core` (traits +
//! types), the procedural macros from `harness-macros`, and exposes
//! `harness-skills` under the `skills` module. The lower-level crates remain
//! available individually for anyone who wants a minimal dependency footprint.
//!
//! # What is a harness?
//!
//! An *agent* in this framework is a `Model` + a *harness* — the surrounding
//! scaffold that decides what the model can see (`Guide`), what tools it can
//! call (`Tool`), what feedback signals come back to it (`Sensor`), what
//! policies wrap each step (`Hook`), and how its context is kept small
//! (`Compactor`). The `AgentLoop` in `harness-rs-loop` ties these together
//! in a ReAct loop with self-correction.
//!
//! See `DESIGN.md` at the workspace root for the architectural intent.
//!
//! # Quick start
//!
//! Define a tool with `#[tool]`, point the model adapter at any
//! OpenAI-compatible endpoint, and run the loop:
//!
//! ```ignore
//! use harness::{tool, ToolError};
//! use harness_loop::AgentLoop;
//! use harness_models::OpenAiCompat;
//! use harness_context::default_world;
//! use harness_core::{Policy, Task};
//! use std::sync::Arc;
//!
//! /// Add two integers.
//! #[tool(name = "add", risk = "Safe")]
//! async fn add(a: i64, b: i64) -> Result<i64, ToolError> {
//!     Ok(a + b)
//! }
//!
//! # async fn run() -> anyhow::Result<()> {
//! let model = OpenAiCompat::with_key(
//!     "https://api.deepseek.com",
//!     "deepseek-chat",
//!     std::env::var("DEEPSEEK_API_KEY")?,
//! );
//! let mut loop_ = AgentLoop::builder()
//!     .model(Arc::new(model))
//!     .tool(Arc::new(add()))
//!     .build();
//! let mut world = default_world(std::env::current_dir()?);
//! let outcome = loop_
//!     .run(
//!         Task::new("What is 2 + 3?"),
//!         &mut world,
//!         Policy::default(),
//!     )
//!     .await?;
//! println!("{outcome:?}");
//! # Ok(()) }
//! ```
//!
//! # Examples
//!
//! Worked examples live at <https://github.com/liliang-cn/harness-rs/tree/main/examples>:
//!
//! - `deepseek-hello` — smallest possible Hello-world.
//! - `crate-keeper` — `MockModel` smoke test (no network).
//! - `personal-assistant` — scheduling agent with `UserProfile`, REPL, brief mode.
//! - `investor-bot` — autonomous web research with multi-engine search + retry.
//!
//! # Crate map
//!
//! - [`harness_core`] — `Model` / `Tool` / `Guide` / `Sensor` / `Hook` /
//!   `Compactor` / `Skill` traits, `World`, `Context`, `Event`, error types.
//! - [`harness_macros`] — `#[skill]` / `#[tool]` / `#[guide]` / `#[sensor]` /
//!   `#[hook]` proc-macros.
//! - `harness_loop` — `AgentLoop` ReAct executor with auto-fix sensors.
//! - `harness_hooks` — `HookBus` over 27 lifecycle events.
//! - `harness_blueprint` — hybrid deterministic + agent state machine.
//! - `harness_compactor` — five-stage progressive context compaction.
//! - `harness_sandbox` — `WorktreeSandbox` (default) + container/VM stubs.
//! - `harness_models` — `OpenAiCompat` / `AnthropicNative` / `MockModel`.
//! - `harness_mcp` — MCP stdio JSON-RPC server.
//! - [`skills`] — agentskills.io-compliant skill loader + validator.
//! - `harness_tools_fs` / `harness_tools_shell` — built-in toolsets.
//! - `harness_sensors_rust` / `harness_sensors_common` — built-in sensors.

pub use harness_core::*;
pub use harness_macros::*;

pub mod skills {
    //! agentskills.io-compliant skill loading.
    pub use harness_skills::*;
}

pub mod prelude {
    pub use harness_core::{
        Action, Block, Compactor, Context, Event, Execution, Guide, GuideScope, HarnessError, Hook,
        Model, ModelOutput, Policy, Result, Sensor, Severity, Signal, Skill, SkillManifest, Stage,
        Task, Tool, ToolResult, ToolRisk, ToolSchema, World,
    };
}

/// Crate version for diagnostic logging.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
