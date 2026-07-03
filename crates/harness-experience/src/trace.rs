//! `ToolTraceHook` — records which tools a run calls, in order.
//!
//! Install it on an `AgentLoop` (`.with_hook(...)`); it appends each tool name
//! to a shared buffer on every `PreToolUse`. After the run, drain the buffer to
//! learn "how it was handled" — the tool sequence that solved the situation.

use harness_core::{Event, Hook, HookOutcome, World};
use std::sync::{Arc, Mutex};

/// Shared, cloneable capture buffer of tool names (in call order).
#[derive(Clone, Default)]
pub struct ToolTrace {
    tools: Arc<Mutex<Vec<String>>>,
}

impl ToolTrace {
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot of the tools captured so far.
    pub fn snapshot(&self) -> Vec<String> {
        self.tools.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Take the captured tools and reset the buffer (call after a run).
    pub fn drain(&self) -> Vec<String> {
        self.tools
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    /// Build the `Hook` to install on the loop. Shares this buffer.
    pub fn hook(&self) -> Arc<dyn Hook> {
        Arc::new(ToolTraceHook {
            tools: self.tools.clone(),
        })
    }
}

struct ToolTraceHook {
    tools: Arc<Mutex<Vec<String>>>,
}

impl Hook for ToolTraceHook {
    fn name(&self) -> &str {
        "experience-tool-trace"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        if let Event::PreToolUse { action } = ev
            && let Ok(mut g) = self.tools.lock()
        {
            g.push(action.tool.clone());
        }
        HookOutcome::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Action;
    use serde_json::json;

    fn action(tool: &str) -> Action {
        Action {
            tool: tool.into(),
            call_id: "c".into(),
            args: json!({}),
        }
    }

    #[tokio::test]
    async fn captures_tool_names_in_order() {
        let trace = ToolTrace::new();
        let hook = trace.hook();
        let mut world = harness_context::default_world(std::env::temp_dir());
        for t in ["read_file", "shell", "read_file"] {
            let a = action(t);
            hook.fire(&Event::PreToolUse { action: &a }, &mut world);
        }
        assert_eq!(trace.snapshot(), vec!["read_file", "shell", "read_file"]);
        assert_eq!(trace.drain().len(), 3);
        assert!(trace.snapshot().is_empty(), "drain resets");
    }
}
