//! `harness` — facade re-exporting the public surface of the framework.
//!
//! Users typically depend only on this crate.

pub use harness_core::*;
pub use harness_macros::*;

pub mod skills {
    //! agentskills.io-compliant skill loading.
    pub use harness_skills::*;
}

pub mod prelude {
    pub use harness_core::{
        Action, Block, Compactor, Context, Event, Execution, Guide, GuideScope, Hook, Model,
        ModelOutput, Policy, Sensor, Signal, Severity, Skill, SkillManifest, Stage, Task, Tool,
        ToolResult, ToolRisk, ToolSchema, World, HarnessError, Result,
    };
}

/// Crate version for diagnostic logging.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
