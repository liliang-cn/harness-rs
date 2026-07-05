//! A tiny name-keyed tool registry used by `AgentLoop`.

use harness_core::{Action, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, t: Arc<dyn Tool>) {
        self.tools.insert(t.name().to_string(), t);
    }

    /// Tool schemas in a **stable, name-sorted order**. Deterministic ordering
    /// keeps the request's `tools` block byte-identical across turns, which is
    /// what lets a provider's prefix cache (e.g. DeepSeek) hit — a `HashMap`'s
    /// arbitrary iteration order would silently break it.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let mut v: Vec<ToolSchema> = self.tools.values().map(|t| t.schema().clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub async fn dispatch(
        &self,
        action: &Action,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(&action.tool)
            .ok_or_else(|| ToolError::NotFound {
                name: action.tool.clone(),
            })?
            .clone();
        tool.invoke(action.args.clone(), world).await
    }

    /// The risk class of a tool by name (used to decide parallel-safe dispatch).
    pub fn risk(&self, name: &str) -> Option<ToolRisk> {
        self.tools.get(name).map(|t| t.risk())
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
