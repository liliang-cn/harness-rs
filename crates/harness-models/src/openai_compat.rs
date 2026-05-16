//! OpenAI Chat Completions–compatible model adapter, with tool-calling.

use crate::LlmConfig;
use async_trait::async_trait;
use harness_core::{
    Block, Context, Model, ModelError, ModelInfo, ModelOutput, StopReason, ToolCall, TurnRole,
    Usage,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct OpenAiCompat {
    cfg:            LlmConfig,
    client:         reqwest::Client,
    context_window: u32,
}

impl OpenAiCompat {
    pub fn new(cfg: LlmConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client builds");
        Self { cfg, client, context_window: 128_000 }
    }

    pub fn with_context_window(mut self, window: u32) -> Self {
        self.context_window = window;
        self
    }

    pub fn config(&self) -> &LlmConfig { &self.cfg }
}

// ----------------------------------------------------------------
// Wire format — OpenAI v1 chat completions, including tool-calling
// ----------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model:    &'a str,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools:    Vec<ToolDecl>,
    stream:   bool,
}

/// `ChatMessage` is intentionally **lenient** — providers add fields
/// (DeepSeek's `reasoning_content`, OpenAI's `refusal`, etc.). We capture
/// `reasoning_content` because DeepSeek thinking mode demands it be echoed
/// back; anything else we don't recognise is ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    role:    String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireToolCall {
    id:    String,
    #[serde(rename = "type", default = "default_function_type")]
    kind:  String,
    function: WireToolFunction,
}

fn default_function_type() -> String { "function".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireToolFunction {
    name:      String,
    /// OpenAI sends arguments as a JSON-encoded string (not an object).
    arguments: String,
}

#[derive(Debug, Serialize)]
struct ToolDecl {
    #[serde(rename = "type")]
    kind:     &'static str,
    function: ToolDeclFunction,
}

#[derive(Debug, Serialize)]
struct ToolDeclFunction {
    name:        String,
    description: String,
    parameters:  serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage:   ChatUsage,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message:       ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens:     u32,
    #[serde(default)]
    completion_tokens: u32,
}

// ----------------------------------------------------------------
// Model impl
// ----------------------------------------------------------------

#[async_trait]
impl Model for OpenAiCompat {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        let messages = build_messages(ctx);
        let tools = ctx
            .tools
            .iter()
            .map(|t| ToolDecl {
                kind: "function",
                function: ToolDeclFunction {
                    name:        t.name.clone(),
                    description: t.description.clone(),
                    parameters:  t.input.clone(),
                },
            })
            .collect();

        let req = ChatRequest {
            model:      &self.cfg.model,
            messages,
            max_tokens: Some(ctx.policy.max_output_tokens),
            tools,
            stream:     false,
        };

        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        tracing::debug!(?req, "openai-compat request");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.cfg.api_key)
            .json(&req)
            .send()
            .await
            .map_err(|e| ModelError::Transport(format!("send: {e}")))?;

        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ModelError::Transport(format!("body: {e}")))?;
        if !status.is_success() {
            return Err(ModelError::Transport(format!(
                "HTTP {status}: {}",
                String::from_utf8_lossy(&bytes)
            )));
        }
        let parsed: ChatResponse = serde_json::from_slice(&bytes).map_err(|e| {
            ModelError::Invalid(format!(
                "parse: {e}; body: {}",
                String::from_utf8_lossy(&bytes)
            ))
        })?;

        let first = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ModelError::Invalid("response has no choices".into()))?;

        let tool_calls = first
            .message
            .tool_calls
            .into_iter()
            .map(|w| {
                let args: serde_json::Value =
                    serde_json::from_str(&w.function.arguments).unwrap_or_else(|_| {
                        serde_json::Value::String(w.function.arguments.clone())
                    });
                ToolCall { id: w.id, name: w.function.name, args }
            })
            .collect::<Vec<_>>();

        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolUse
        } else {
            match first.finish_reason.as_deref() {
                Some("stop")           => StopReason::EndTurn,
                Some("length")         => StopReason::MaxTokens,
                Some("tool_calls")     => StopReason::ToolUse,
                Some("content_filter") => StopReason::Other,
                _                      => StopReason::EndTurn,
            }
        };

        Ok(ModelOutput {
            text:       first.message.content,
            tool_calls,
            usage: Usage {
                input_tokens:        parsed.usage.prompt_tokens,
                output_tokens:       parsed.usage.completion_tokens,
                cached_input_tokens: 0,
            },
            stop_reason,
            reasoning: first.message.reasoning_content,
        })
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle:                                  self.cfg.name.clone(),
            provider:                                provider_from_base_url(&self.cfg.base_url),
            model:                                   self.cfg.model.clone(),
            context_window:                          self.context_window,
            input_cost_usd_per_million_tokens:       None,
            output_cost_usd_per_million_tokens:      None,
            supports_tool_use:                       true,
            supports_streaming:                      true,
        }
    }
}

fn provider_from_base_url(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("openai-compat")
        .to_string()
}

fn build_messages(ctx: &Context) -> Vec<ChatMessage> {
    let mut out = Vec::new();

    // 1. system: concat system + guide blocks
    let mut system_buf = String::new();
    for b in ctx.system.iter().chain(ctx.guides.iter()) {
        push_block_text(&mut system_buf, b);
    }
    if !system_buf.trim().is_empty() {
        out.push(ChatMessage {
            role: "system".into(),
            content: Some(system_buf),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
        });
    }

    // 2. history turns
    for turn in &ctx.history {
        translate_turn(turn, &mut out);
    }

    // 3. fallback ONLY when caller forgot to push the task: if no user-shaped
    //    message exists at all, surface task.description as one. Do NOT re-append
    //    the task whenever history just happens to end on a tool turn — the loop
    //    pushes the task once at the start and that's the canonical placement.
    let has_user = out.iter().any(|m| m.role == "user");
    if !has_user {
        out.push(ChatMessage {
            role:    "user".into(),
            content: Some(ctx.task.description.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
        });
    }
    out
}

fn translate_turn(turn: &harness_core::Turn, out: &mut Vec<ChatMessage>) {
    let role = match turn.role {
        TurnRole::User      => "user",
        TurnRole::Assistant => "assistant",
        TurnRole::Tool      => "tool",
        TurnRole::System    => "system",
    };

    // Tool results become individual `tool` messages keyed by call_id.
    if matches!(turn.role, TurnRole::Tool) {
        for b in &turn.blocks {
            if let Block::ToolResult { call_id, content } = b {
                let s = match content {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.push(ChatMessage {
                    role:    "tool".into(),
                    content: Some(s),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call_id.clone()),
                    reasoning_content: None,
                });
            } else if let Block::Feedback(signals) = b {
                let mut s = String::new();
                for sig in signals {
                    s.push_str(&format!(
                        "[feedback:{}] {}\n",
                        sig.origin,
                        sig.agent_hint.as_deref().unwrap_or(&sig.message)
                    ));
                }
                out.push(ChatMessage {
                    role:    "user".into(),
                    content: Some(s),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    reasoning_content: None,
                });
            }
        }
        return;
    }

    // Assistant turn: split text content from tool_calls into the proper wire shape.
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut reasoning: Option<String> = None;
    for b in &turn.blocks {
        match b {
            Block::Text(s) => { text.push_str(s); text.push('\n'); }
            Block::ToolCall { call_id, name, args } => {
                tool_calls.push(WireToolCall {
                    id:   call_id.clone(),
                    kind: "function".into(),
                    function: WireToolFunction {
                        name: name.clone(),
                        arguments: args.to_string(),
                    },
                });
            }
            Block::Reasoning(r) => {
                // Echo back what the model said it was thinking. DeepSeek
                // requires this; OpenAI ignores unknown fields.
                reasoning = Some(reasoning.map(|prev| format!("{prev}\n{r}")).unwrap_or_else(|| r.clone()));
            }
            Block::ToolResult { .. } | Block::Feedback(_) => {
                // shouldn't appear in assistant/user turns; ignore
            }
            other => push_block_text(&mut text, other),
        }
    }
    out.push(ChatMessage {
        role:    role.into(),
        content: if text.trim().is_empty() { None } else { Some(text) },
        tool_calls,
        tool_call_id: None,
        reasoning_content: reasoning,
    });
}

fn push_block_text(buf: &mut String, b: &Block) {
    match b {
        Block::Text(s) => { buf.push_str(s); buf.push('\n'); }
        Block::Skill { name, body } => {
            buf.push_str(&format!("\n[skill:{name}]\n{body}\n"));
        }
        Block::FileRef { path, excerpt, .. } => {
            buf.push_str(&format!("\n[file:{path}]\n"));
            if let Some(e) = excerpt {
                buf.push_str(e);
                buf.push('\n');
            }
        }
        Block::ToolCall { .. }
        | Block::ToolResult { .. }
        | Block::Feedback(_)
        | Block::Reasoning(_) => {
            // handled in translate_turn (Reasoning becomes `reasoning_content`)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Policy, Task};
    use std::collections::BTreeMap;

    #[test]
    fn build_messages_emits_system_and_user() {
        let ctx = Context {
            system:   vec![Block::Text("you are a helpful agent".into())],
            guides:   vec![Block::Text("always be concise".into())],
            history:  vec![],
            task:     Task { description: "say hi".into(), source: None, deadline: None },
            policy:   Policy::default(),
            metadata: BTreeMap::new(),
            tools:    Vec::new(),
        };
        let msgs = build_messages(&ctx);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert!(msgs[0].content.as_deref().unwrap().contains("helpful agent"));
        assert!(msgs[0].content.as_deref().unwrap().contains("be concise"));
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content.as_deref(), Some("say hi"));
    }
}
