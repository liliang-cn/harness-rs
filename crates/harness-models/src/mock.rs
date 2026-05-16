//! `MockModel` — deterministic, scriptable model for tests.
//!
//! Drives `AgentLoop` and `Subagent` without any network. The model returns
//! pre-scripted responses in order; if exhausted, returns `Done` with no text.
//!
//! Captures every call's `Context` for after-the-fact assertions about what the
//! framework actually sent.
//!
//! ```ignore
//! let model = MockModel::new()
//!     .script(MockResponse::tool_call("read_file", json!({"path": "x.rs"})))
//!     .script(MockResponse::text("done"));
//! let outcome = AgentLoop::new(model).run(task, &mut world).await?;
//! ```

use async_trait::async_trait;
use harness_core::{
    Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, ToolCall, Usage,
};
use serde_json::Value;
use std::sync::Mutex;

/// One scripted response the mock will return when `complete()` is called.
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub reasoning: Option<String>,
}

impl MockResponse {
    /// Assistant turn that just says `text` and is done.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
            input_tokens: 0,
            output_tokens: 0,
            reasoning: None,
        }
    }

    /// Assistant turn that calls a single tool with auto-generated call id.
    pub fn tool_call(name: impl Into<String>, args: Value) -> Self {
        let name = name.into();
        let id = format!("mock_call_{}", short_hash(&name, &args));
        Self {
            text: None,
            tool_calls: vec![ToolCall { id, name, args }],
            stop_reason: StopReason::ToolUse,
            input_tokens: 0,
            output_tokens: 0,
            reasoning: None,
        }
    }

    /// Multiple tool calls in one assistant turn.
    pub fn tool_calls(calls: Vec<(String, Value)>) -> Self {
        let tool_calls = calls
            .into_iter()
            .enumerate()
            .map(|(i, (name, args))| ToolCall {
                id: format!("mock_call_{i}_{}", short_hash(&name, &args)),
                name,
                args,
            })
            .collect();
        Self {
            text: None,
            tool_calls,
            stop_reason: StopReason::ToolUse,
            input_tokens: 0,
            output_tokens: 0,
            reasoning: None,
        }
    }

    pub fn with_usage(mut self, input: u32, output: u32) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

fn short_hash(name: &str, args: &Value) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    args.to_string().hash(&mut h);
    format!("{:x}", h.finish())[..8].to_string()
}

/// A scriptable, assertable model that runs entirely in-process.
pub struct MockModel {
    inner: Mutex<MockInner>,
    name: String,
}

struct MockInner {
    queue: std::collections::VecDeque<MockResponse>,
    calls: Vec<RecordedCall>,
}

/// What the framework actually sent the model on a given `complete()` call.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub tools_available: Vec<String>,
    pub history_summary: Vec<HistorySnapshot>,
    pub task_description: String,
}

#[derive(Debug, Clone)]
pub struct HistorySnapshot {
    pub role: String,
    pub kinds: Vec<&'static str>, // "text" | "tool-call" | "tool-result" | …
    pub texts: Vec<String>,
}

impl Default for MockModel {
    fn default() -> Self {
        Self::new()
    }
}

impl MockModel {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockInner {
                queue: std::collections::VecDeque::new(),
                calls: Vec::new(),
            }),
            name: "mock".into(),
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Push a response onto the script (consumed FIFO).
    pub fn script(self, resp: MockResponse) -> Self {
        self.inner.lock().unwrap().queue.push_back(resp);
        self
    }

    /// Convenience: push many responses at once.
    pub fn script_many(mut self, resps: impl IntoIterator<Item = MockResponse>) -> Self {
        for r in resps {
            self = self.script(r);
        }
        self
    }

    /// Snapshot of everything the model has been called with so far.
    pub fn calls(&self) -> Vec<RecordedCall> {
        self.inner.lock().unwrap().calls.clone()
    }

    pub fn call_count(&self) -> usize {
        self.inner.lock().unwrap().calls.len()
    }

    /// True if every scripted response has been consumed.
    pub fn script_exhausted(&self) -> bool {
        self.inner.lock().unwrap().queue.is_empty()
    }
}

#[async_trait]
impl Model for MockModel {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        let snapshot = snapshot_history(ctx);
        let tools_available: Vec<String> = ctx.tools.iter().map(|t| t.name.clone()).collect();

        let mut guard = self.inner.lock().unwrap();
        guard.calls.push(RecordedCall {
            tools_available,
            history_summary: snapshot,
            task_description: ctx.task.description.clone(),
        });
        let resp = guard
            .queue
            .pop_front()
            .unwrap_or_else(|| MockResponse::text(""));
        drop(guard);

        Ok(ModelOutput {
            text: resp.text,
            tool_calls: resp.tool_calls,
            stop_reason: resp.stop_reason,
            usage: Usage {
                input_tokens: resp.input_tokens,
                output_tokens: resp.output_tokens,
                cached_input_tokens: 0,
            },
            reasoning: resp.reasoning.clone(),
        })
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle: self.name.clone(),
            provider: "mock".into(),
            model: "mock-1".into(),
            context_window: 200_000,
            input_cost_usd_per_million_tokens: Some(0.0),
            output_cost_usd_per_million_tokens: Some(0.0),
            supports_tool_use: true,
            supports_streaming: false,
        }
    }
}

fn snapshot_history(ctx: &Context) -> Vec<HistorySnapshot> {
    use harness_core::{Block, TurnRole};
    ctx.history
        .iter()
        .map(|t| {
            let role = match t.role {
                TurnRole::User => "user",
                TurnRole::Assistant => "assistant",
                TurnRole::Tool => "tool",
                TurnRole::System => "system",
                _ => "unknown",
            };
            let mut kinds = Vec::new();
            let mut texts = Vec::new();
            for b in &t.blocks {
                let (kind, text) = match b {
                    Block::Text(s) => ("text", s.clone()),
                    Block::ToolCall { name, .. } => ("tool-call", name.clone()),
                    Block::ToolResult { call_id, content } => {
                        ("tool-result", format!("{call_id}: {content}"))
                    }
                    Block::FileRef { path, .. } => ("file-ref", path.clone()),
                    Block::Skill { name, .. } => ("skill", name.clone()),
                    Block::Feedback(s) => ("feedback", format!("{} signal(s)", s.len())),
                    Block::Reasoning(s) => ("reasoning", s.clone()),
                    _ => ("unknown", String::new()),
                };
                kinds.push(kind);
                texts.push(text);
            }
            HistorySnapshot {
                role: role.into(),
                kinds,
                texts,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Task;
    use std::collections::BTreeMap;

    fn ctx() -> Context {
        Context {
            system: vec![],
            guides: vec![],
            history: vec![],
            task: Task {
                description: "t".into(),
                source: None,
                deadline: None,
            },
            policy: Default::default(),
            metadata: BTreeMap::new(),
            tools: vec![],
        }
    }

    #[tokio::test]
    async fn script_fifo_consumption() {
        let m = MockModel::new()
            .script(MockResponse::text("first"))
            .script(MockResponse::tool_call("foo", serde_json::json!({})))
            .script(MockResponse::text("third"));
        assert_eq!(
            m.complete(&ctx()).await.unwrap().text.as_deref(),
            Some("first")
        );
        let r2 = m.complete(&ctx()).await.unwrap();
        assert_eq!(r2.tool_calls.len(), 1);
        assert_eq!(r2.tool_calls[0].name, "foo");
        assert_eq!(
            m.complete(&ctx()).await.unwrap().text.as_deref(),
            Some("third")
        );
        assert!(m.script_exhausted());
        assert_eq!(m.call_count(), 3);
    }

    #[tokio::test]
    async fn exhausted_script_returns_empty_done() {
        let m = MockModel::new();
        let r = m.complete(&ctx()).await.unwrap();
        assert!(r.tool_calls.is_empty());
        assert_eq!(r.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn calls_record_what_framework_sent() {
        let m = MockModel::new().script(MockResponse::text("ok"));
        let mut c = ctx();
        c.tools = vec![harness_core::ToolSchema {
            name: "x".into(),
            description: "y".into(),
            input: serde_json::json!({}),
        }];
        let _ = m.complete(&c).await;
        let calls = m.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tools_available, vec!["x".to_string()]);
        assert_eq!(calls[0].task_description, "t");
    }
}
