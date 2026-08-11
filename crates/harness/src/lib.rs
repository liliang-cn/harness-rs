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
//! use harness::tool;
//! use harness_core::{Task, ToolError, ToolResult, World};
//! use harness_loop::AgentLoop;
//! use harness_models::OpenAiCompat;
//! use harness_context::default_world;
//! use serde_json::json;
//!
//! /// Add two integers.
//! #[tool(name = "add", risk = "read-only")]
//! async fn add(args: serde_json::Value, _world: &mut World) -> Result<ToolResult, ToolError> {
//!     let sum = args["a"].as_i64().unwrap_or(0) + args["b"].as_i64().unwrap_or(0);
//!     Ok(ToolResult { ok: true, content: json!({ "sum": sum }), trace: None })
//! }
//!
//! # async fn run() -> anyhow::Result<()> {
//! let model = OpenAiCompat::with_key(
//!     "https://api.deepseek.com",
//!     "deepseek-chat",
//!     std::env::var("DEEPSEEK_API_KEY")?,
//! );
//! // `#[tool]` registers via `inventory`; `with_macro_hooks` is not needed for
//! // tools — collect them with `harness_core::iter_macro_tools()`.
//! let mut loop_ = AgentLoop::new(model);
//! for t in harness_core::iter_macro_tools() {
//!     loop_ = loop_.with_tool(t);
//! }
//! let mut world = default_world(std::env::current_dir()?);
//! let outcome = loop_
//!     .run(
//!         Task { description: "What is 2 + 3?".into(),
//!                source: None, deadline: None },
//!         &mut world,
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
//! - `harness_sandbox` — `WorktreeSandbox`, `ContainerSandbox`, and the
//!   `Sandbox` trait for deployment-owned isolation backends.
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
