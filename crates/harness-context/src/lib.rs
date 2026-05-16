//! Runtime defaults and context-assembly helpers.
//!
//! `harness-core` defines the abstract `World`/`Clock`/`ProcessRunner`/`KvStore`
//! traits; this crate provides the concrete implementations the framework
//! actually runs on.

pub mod runtime;

pub use runtime::*;

use harness_core::{Action, Block, Context, Task, ToolResult, Turn, TurnRole, World};

/// Convenience constructors and mutators for `Context`.
pub trait ContextExt {
    fn for_task(task: Task) -> Context;
    fn push_user_text(&mut self, text: impl Into<String>);
    fn push_assistant_text(&mut self, text: impl Into<String>);
    fn push_tool_call(&mut self, call_id: &str, tool: &str, args: &serde_json::Value);
    fn push_tool_result(&mut self, action: &Action, result: &ToolResult);
}

impl ContextExt for Context {
    fn for_task(task: Task) -> Context {
        Context::new(task)
    }

    fn push_user_text(&mut self, text: impl Into<String>) {
        self.history.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(text.into())],
        });
    }

    fn push_assistant_text(&mut self, text: impl Into<String>) {
        self.history.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::Text(text.into())],
        });
    }

    fn push_tool_call(&mut self, call_id: &str, tool: &str, args: &serde_json::Value) {
        self.history.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::ToolCall {
                call_id: call_id.into(),
                name:    tool.into(),
                args:    args.clone(),
            }],
        });
    }

    fn push_tool_result(&mut self, action: &Action, result: &ToolResult) {
        self.history.push(Turn {
            role: TurnRole::Tool,
            blocks: vec![Block::ToolResult {
                call_id: action.call_id.clone(),
                content: result.content.clone(),
            }],
        });
    }
}

/// Quick way to construct a `World` rooted at the given path with the default
/// runtime impls (tokio process runner, system clock, in-memory kv).
pub fn default_world(repo_root: impl Into<std::path::PathBuf>) -> World {
    use std::sync::Arc;
    World {
        repo:   harness_core::RepoView { root: repo_root.into() },
        runner: Arc::new(TokioRunner),
        clock:  Arc::new(SystemClock),
        kv:     Arc::new(InMemoryKv::new()),
    }
}
