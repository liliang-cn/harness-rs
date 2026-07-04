//! `task` — subagent fan-out.

use async_trait::async_trait;
use harness_context::default_world;
use harness_core::{
    DynModel, Model, Task, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World,
};
use harness_loop::{Subagent, SubagentSpec};
use serde_json::json;
use std::sync::{Arc, OnceLock};

/// Spawn one isolated read-only subagent per subtask, concurrently, and return a
/// structured report array. Each subagent gets its own `World` (same repo root)
/// so they run in parallel without aliasing, and a read-only toolset so a
/// fan-out can explore but not mutate.
pub struct TaskTool {
    pub model: Arc<dyn Model>,
    pub tools: Vec<Arc<dyn Tool>>,
}
static TASK_SCHEMA: OnceLock<ToolSchema> = OnceLock::new();

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn schema(&self) -> &ToolSchema {
        TASK_SCHEMA.get_or_init(|| ToolSchema {
            name: "task".into(),
            description: "Fan out to isolated sub-agents that run concurrently, each in its own \
                          context with read-only tools (hash_read, grep, glob, list_dir). Use for \
                          parallel investigation (\"where is X used\", \"summarize module Y\"). \
                          Returns one structured report per subtask — no prose to parse."
                .into(),
            input: json!({
                "type": "object",
                "properties": {
                    "subtasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name":   {"type": "string", "description": "Short label for the report."},
                                "prompt": {"type": "string", "description": "What this sub-agent should find out."}
                            },
                            "required": ["prompt"]
                        }
                    }
                },
                "required": ["subtasks"]
            }),
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let root = world.repo.root.clone();
        let subs = args["subtasks"]
            .as_array()
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "task".into(),
                reason: "`subtasks` must be an array".into(),
            })?;
        let mut futs = Vec::new();
        for s in subs {
            let name = s["name"].as_str().unwrap_or("subagent").to_string();
            let prompt = s["prompt"].as_str().unwrap_or_default().to_string();
            let model = self.model.clone();
            let tools = self.tools.clone();
            let root = root.clone();
            futs.push(async move {
                let mut w = default_world(root);
                let mut spec = SubagentSpec::new(
                    name.clone(),
                    Task {
                        description: prompt,
                        source: None,
                        deadline: None,
                    },
                )
                .with_max_iters(10);
                for t in tools {
                    spec = spec.with_tool(t);
                }
                match Subagent::new(DynModel(model), spec).run(&mut w).await {
                    Ok(rep) => json!({
                        "name": name,
                        "status": format!("{:?}", rep.status),
                        "iters": rep.iters,
                        "result": rep.text.unwrap_or_default(),
                    }),
                    Err(e) => json!({ "name": name, "status": "error", "result": e.to_string() }),
                }
            });
        }
        let results = futures::future::join_all(futs).await;
        eprintln!(
            "\n  \x1b[2m⚙ task — {} subagent(s) done\x1b[0m",
            results.len()
        );
        Ok(ToolResult {
            ok: true,
            content: json!({ "subagents": results }),
            trace: None,
        })
    }
}
