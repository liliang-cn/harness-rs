use crate::{
    Action, CompactionStage, Context, FixPatch, GuideId, ModelOutput, SensorId, Signal, ToolResult,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// All 29 lifecycle events the framework emits (DESIGN.md §10).
///
/// Lifetimes are intentionally borrowed: hooks must not own these references
/// past the call.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event<'a> {
    // session
    SessionStart {
        source: SessionSource,
    },
    SessionEnd,

    /// One acceptance check answered.
    ///
    /// Emitted per check, pass or fail. Until this existed the verdict reached
    /// the caller in the outcome and reached nobody else: an audit trail could
    /// show every tool the agent called and not whether anything ever agreed
    /// the work was done.
    AcceptanceChecked {
        name: &'a str,
        passed: bool,
        /// The check's own words on a failure; empty on a pass.
        reason: &'a str,
    },

    /// A sealed acceptance contract changed while the run was in flight.
    ///
    /// Separate from a failed [`Event::AcceptanceChecked`] because they warrant
    /// different responses: a failed check is work not finished, this is the
    /// measuring instrument having been moved by the party being measured. It
    /// is the one event in this enum a host may reasonably want to page on.
    SealBreached {
        detail: &'a str,
    },

    // tool
    PreToolUse {
        action: &'a Action,
    },
    PostToolUse {
        action: &'a Action,
        result: &'a ToolResult,
    },
    PermissionRequest {
        action: &'a Action,
    },

    // compaction
    PreCompact {
        stage: CompactionStage,
    },
    PostCompact {
        stage: CompactionStage,
        /// Estimated context tokens before this stage ran.
        before: u32,
        /// …and after. The difference is what the stage actually bought, which
        /// is otherwise unknowable from outside: compaction is the component
        /// whose whole job is to spend less, and a `stage` label alone says it
        /// happened, not whether it worked.
        after: u32,
    },

    // guides
    PreGuide {
        guide: &'a GuideId,
    },
    PostGuide {
        guide: &'a GuideId,
    },

    // sensors
    PreSensor {
        sensor: &'a SensorId,
    },
    PostSensor {
        sensor: &'a SensorId,
        signals: &'a [Signal],
    },

    // auto-fix patches (audit #7: sensor-emitted RunCommand etc. were applied
    // silently — hooks can now intercept and Deny per-patch).
    PreAutoFix {
        patch: &'a FixPatch,
    },
    PostAutoFix {
        patch: &'a FixPatch,
        applied: bool,
    },

    // model
    PreModel {
        ctx: &'a Context,
    },
    PostModel {
        out: &'a ModelOutput,
    },
    /// Streaming-only: a text fragment arrived from `Model::stream()`. Fires
    /// 0..N times between `PreModel` and `PostModel` when the AgentLoop is
    /// in streaming mode. `text` is the new fragment (not the accumulator).
    /// Tool-call deltas are NOT surfaced here — the loop assembles those
    /// and emits the final `PostModel` with full `tool_calls`.
    ModelTokenDelta {
        text: &'a str,
    },

    // subagents
    SubagentStart {
        name: &'a str,
    },
    SubagentReport {
        status: SubagentStatus,
    },

    // filesystem
    FileChanged {
        path: &'a PathBuf,
    },
    CwdChanged {
        from: &'a PathBuf,
        to: &'a PathBuf,
    },

    // blueprint
    BlueprintNodeEnter {
        node: &'a str,
    },
    BlueprintNodeExit {
        node: &'a str,
    },

    // misc
    TaskCompleted,
    BudgetWarning {
        ratio: f32,
    },
    Notification {
        kind: NotificationKind,
    },
    Error {
        message: &'a str,
    },
    Stop,
    Heartbeat {
        iter: u32,
    },
    Custom {
        name: &'a str,
        data: &'a serde_json::Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum SessionSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

/// Subagent self-report (Superpowers convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SubagentStatus {
    Done,
    DoneWithConcerns,
    Blocked,
    NeedsContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum NotificationKind {
    PermissionPrompt,
    IdlePrompt,
    AuthSuccess,
    ElicitationDialog,
    ElicitationComplete,
    ElicitationResponse,
}

impl<'a> Event<'a> {
    /// Stable string discriminant for matchers and serialization.
    pub fn name(&self) -> &'static str {
        match self {
            Event::SessionStart { .. } => "SessionStart",
            Event::SessionEnd => "SessionEnd",
            Event::AcceptanceChecked { .. } => "AcceptanceChecked",
            Event::SealBreached { .. } => "SealBreached",
            Event::PreToolUse { .. } => "PreToolUse",
            Event::PostToolUse { .. } => "PostToolUse",
            Event::PermissionRequest { .. } => "PermissionRequest",
            Event::PreCompact { .. } => "PreCompact",
            Event::PostCompact { .. } => "PostCompact",
            Event::PreGuide { .. } => "PreGuide",
            Event::PostGuide { .. } => "PostGuide",
            Event::PreSensor { .. } => "PreSensor",
            Event::PostSensor { .. } => "PostSensor",
            Event::PreAutoFix { .. } => "PreAutoFix",
            Event::PostAutoFix { .. } => "PostAutoFix",
            Event::PreModel { .. } => "PreModel",
            Event::PostModel { .. } => "PostModel",
            Event::ModelTokenDelta { .. } => "ModelTokenDelta",
            Event::SubagentStart { .. } => "SubagentStart",
            Event::SubagentReport { .. } => "SubagentReport",
            Event::FileChanged { .. } => "FileChanged",
            Event::CwdChanged { .. } => "CwdChanged",
            Event::BlueprintNodeEnter { .. } => "BlueprintNodeEnter",
            Event::BlueprintNodeExit { .. } => "BlueprintNodeExit",
            Event::TaskCompleted => "TaskCompleted",
            Event::BudgetWarning { .. } => "BudgetWarning",
            Event::Notification { .. } => "Notification",
            Event::Error { .. } => "Error",
            Event::Stop => "Stop",
            Event::Heartbeat { .. } => "Heartbeat",
            Event::Custom { .. } => "Custom",
        }
    }
}
