//! `skill_read` — load a named skill's procedure on demand.

use async_trait::async_trait;
use harness_core::{Skill, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::json;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Load the full step-by-step body (SKILL.md) of a skill by name.
pub struct SkillRead {
    pub dir: PathBuf,
}
static SKILL_READ_SCHEMA: OnceLock<ToolSchema> = OnceLock::new();

#[async_trait]
impl Tool for SkillRead {
    fn name(&self) -> &str {
        "skill_read"
    }
    fn schema(&self) -> &ToolSchema {
        SKILL_READ_SCHEMA.get_or_init(|| ToolSchema {
            name: "skill_read".into(),
            description: "Load the full step-by-step body of a skill by name (from the skills \
                          catalogue). Read the relevant skill before performing that kind of task."
                .into(),
            input: json!({
                "type": "object",
                "properties": { "name": {"type": "string"} },
                "required": ["name"]
            }),
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }
    async fn invoke(
        &self,
        args: serde_json::Value,
        _w: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let name = args["name"].as_str().unwrap_or_default();
        let skills = harness_skills::scan_skills_root(&self.dir).unwrap_or_default();
        match skills.iter().find(|s| s.manifest().name == name) {
            Some(sk) => Ok(ToolResult {
                ok: true,
                content: json!({
                    "name": name,
                    "description": sk.manifest().description,
                    "body": sk.body().to_string(),
                }),
                trace: None,
            }),
            None => {
                let avail: Vec<String> = skills.iter().map(|s| s.manifest().name.clone()).collect();
                Err(ToolError::Exec(format!(
                    "no skill named `{name}`. Available: {}",
                    if avail.is_empty() {
                        "(none)".into()
                    } else {
                        avail.join(", ")
                    }
                )))
            }
        }
    }
}
