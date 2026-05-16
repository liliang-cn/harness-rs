//! A tiny name-keyed tool registry used by `AgentLoop`.

use harness_core::{Action, Tool, ToolError, ToolResult, ToolSchema, World};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self { Self::default() }

    pub fn insert(&mut self, t: Arc<dyn Tool>) {
        self.tools.insert(t.name().to_string(), t);
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema().clone()).collect()
    }

    pub async fn dispatch(
        &self,
        action: &Action,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .tools
            .get(&action.tool)
            .ok_or_else(|| ToolError::NotFound { name: action.tool.clone() })?
            .clone();
        tool.invoke(action.args.clone(), world).await
    }

    pub fn len(&self) -> usize { self.tools.len() }
    pub fn is_empty(&self) -> bool { self.tools.is_empty() }
}
