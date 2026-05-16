use crate::{Action, CompactionStage, Context, GuideId, ModelOutput, SensorId, Signal, ToolResult};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// All 27 lifecycle events the framework emits (DESIGN.md §10).
///
/// Lifetimes are intentionally borrowed: hooks must not own these references
/// past the call.
#[derive(Debug)]
#[non_exhaustive]
pub enum Event<'a> {
    // session
    SessionStart   { source: SessionSource },
    SessionEnd,

    // tool
    PreToolUse     { action: &'a Action },
    PostToolUse    { action: &'a Action, result: &'a ToolResult },
    PermissionRequest { action: &'a Action },

    // compaction
    PreCompact     { stage: CompactionStage },
    PostCompact    { stage: CompactionStage },

    // guides
    PreGuide       { guide: &'a GuideId },
    PostGuide      { guide: &'a GuideId },

    // sensors
    PreSensor      { sensor: &'a SensorId },
    PostSensor     { sensor: &'a SensorId, signals: &'a [Signal] },

    // model
    PreModel       { ctx: &'a Context },
    PostModel      { out: &'a ModelOutput },

    // subagents
    SubagentStart  { name: &'a str },
    SubagentReport { status: SubagentStatus },

    // filesystem
    FileChanged    { path: &'a PathBuf },
    CwdChanged     { from: &'a PathBuf, to: &'a PathBuf },

    // blueprint
    BlueprintNodeEnter { node: &'a str },
    BlueprintNodeExit  { node: &'a str },

    // misc
    TaskCompleted,
    BudgetWarning  { ratio: f32 },
    Notification   { kind: NotificationKind },
    Error          { message: &'a str },
    Stop,
    Heartbeat      { iter: u32 },
    Custom         { name: &'a str, data: &'a serde_json::Value },
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
            Event::SessionStart { .. }   => "SessionStart",
            Event::SessionEnd            => "SessionEnd",
            Event::PreToolUse { .. }     => "PreToolUse",
            Event::PostToolUse { .. }    => "PostToolUse",
            Event::PermissionRequest { .. } => "PermissionRequest",
            Event::PreCompact { .. }     => "PreCompact",
            Event::PostCompact { .. }    => "PostCompact",
            Event::PreGuide { .. }       => "PreGuide",
            Event::PostGuide { .. }      => "PostGuide",
            Event::PreSensor { .. }      => "PreSensor",
            Event::PostSensor { .. }     => "PostSensor",
            Event::PreModel { .. }       => "PreModel",
            Event::PostModel { .. }      => "PostModel",
            Event::SubagentStart { .. }  => "SubagentStart",
            Event::SubagentReport { .. } => "SubagentReport",
            Event::FileChanged { .. }    => "FileChanged",
            Event::CwdChanged { .. }     => "CwdChanged",
            Event::BlueprintNodeEnter { .. } => "BlueprintNodeEnter",
            Event::BlueprintNodeExit { .. }  => "BlueprintNodeExit",
            Event::TaskCompleted         => "TaskCompleted",
            Event::BudgetWarning { .. }  => "BudgetWarning",
            Event::Notification { .. }   => "Notification",
            Event::Error { .. }          => "Error",
            Event::Stop                  => "Stop",
            Event::Heartbeat { .. }      => "Heartbeat",
            Event::Custom { .. }         => "Custom",
        }
    }
}
