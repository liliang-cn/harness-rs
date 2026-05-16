use crate::{World, error::ToolError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Risk class for a tool — drives sandbox / permission decisions.
///
/// Names are MCP-aligned (readOnlyHint, destructiveHint, idempotentHint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ToolRisk {
    /// No side effects.
    ReadOnly,
    /// Side effects exist but repeating is safe (e.g. `cargo fmt`).
    Idempotent,
    /// Modifies state in a way that may not be safely repeated.
    Destructive,
    /// Talks to network. Independent of read/write risk.
    Network,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool input.
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub ok: bool,
    pub content: serde_json::Value,
    /// Output sized for `tracing` / replay logs but not necessarily for the model.
    pub trace: Option<String>,
}

#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn schema(&self) -> &ToolSchema;
    fn risk(&self) -> ToolRisk;
    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError>;
}

/// `inventory` slot for compile-time tool registration via `#[tool]`.
pub struct ToolEntry {
    pub factory: fn() -> Arc<dyn Tool>,
}

inventory::collect!(ToolEntry);

pub fn iter_macro_tools() -> impl Iterator<Item = Arc<dyn Tool>> {
    inventory::iter::<ToolEntry>().map(|e| (e.factory)())
}
