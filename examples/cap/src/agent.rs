//! Agent wiring: assemble the `AgentLoop` from CAP's tools, guides, hooks, and
//! sensors, plus small endpoint/home helpers.

use crate::guides::{CapGuide, SkillCatalog};
use crate::sensor::LspSensor;
use crate::tools::{HashEdit, HashRead, SkillRead, TaskTool};
use harness_core::{DynModel, Guide, Hook, Tool};
use harness_loop::AgentLoop;
use harness_tools_fs::{Glob, Grep, ListDir, WriteFile};
use harness_tools_skills::SkillManageTool;
use std::path::PathBuf;
use std::sync::Arc;

/// Everything the loop needs beyond the model, assembled by each front-end.
/// `ui_hook` is the front-end's own hook (CLI streaming+gate, or the TUI's
/// channel bridge) — that's the only piece the `cap` and `cap-tui` binaries
/// differ by.
pub struct LoopParts {
    pub ui_hook: Arc<dyn Hook>,
    pub task_tool: TaskTool,
    pub trace_hook: Arc<dyn Hook>,
    pub exp_guide: Arc<dyn Guide>,
    pub lsp: Option<LspSensor>,
    pub mcp_tools: Vec<Arc<dyn Tool>>,
    pub skills_dir: PathBuf,
}

/// Build the fully-wired agent loop for a given (planner) model.
pub fn build_loop(model: DynModel, parts: LoopParts) -> AgentLoop<DynModel> {
    let mut l = AgentLoop::new(model)
        .with_streaming(true)
        .with_guide(Arc::new(CapGuide))
        .with_guide(parts.exp_guide)
        .with_guide(Arc::new(SkillCatalog {
            dir: parts.skills_dir.clone(),
        }))
        .with_tool(Arc::new(HashRead))
        .with_tool(Arc::new(HashEdit))
        .with_tool(Arc::new(WriteFile))
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(Grep))
        .with_tool(Arc::new(Glob))
        .with_tool(Arc::new(parts.task_tool))
        .with_tool(Arc::new(SkillRead {
            dir: parts.skills_dir.clone(),
        }))
        .with_tool(Arc::new(SkillManageTool::new(parts.skills_dir.clone())))
        .with_hook(parts.ui_hook)
        .with_hook(parts.trace_hook);
    for t in parts.mcp_tools {
        l = l.with_tool(t);
    }
    if let Some(sensor) = parts.lsp {
        l = l.with_sensor(Arc::new(sensor));
    }
    l
}

/// `~/.cap`, created if missing — where local experience and skills live.
pub fn cap_home() -> PathBuf {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let d = base.join(".cap");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Resolve (base_url, model, api_key) from the `HARNESS_*` / `DEEPSEEK_API_KEY`
/// env vars, with DeepSeek defaults.
pub fn resolve_endpoint() -> anyhow::Result<(String, String, String)> {
    let key = std::env::var("HARNESS_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .map_err(|_| anyhow::anyhow!("set HARNESS_API_KEY (or DEEPSEEK_API_KEY)"))?;
    let base =
        std::env::var("HARNESS_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".into());
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "deepseek-chat".into());
    Ok((base, model, key))
}
