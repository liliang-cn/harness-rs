//! OpenAI Chat Completions–compatible model adapter, with tool-calling.

use crate::LlmConfig;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use harness_core::{
    Block, Context, Model, ModelDelta, ModelError, ModelInfo, ModelOutput, StopReason, ToolCall,
    TurnRole, Usage,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct OpenAiCompat {
    cfg: LlmConfig,
    client: reqwest::Client,
    context_window: u32,
}

/// Per-request HTTP timeout. Defaults to 120s; override via
/// `HARNESS_HTTP_TIMEOUT_SECS` for slow local backends (e.g. large Ollama
/// models whose first-token latency can exceed two minutes).
fn http_timeout() -> Duration {
    let secs = std::env::var("HARNESS_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(120);
    Duration::from_secs(secs)
}

/// Optional JSON object merged into every request body, from
/// `HARNESS_OPENAI_EXTRA_BODY`. Lets callers pass provider-specific knobs the
/// typed request doesn't model — e.g. disable Qwen3 thinking on Ollama with
/// `HARNESS_OPENAI_EXTRA_BODY='{"chat_template_kwargs":{"enable_thinking":false}}'`.
/// Invalid / non-object JSON is ignored.
fn extra_body() -> Option<serde_json::Map<String, serde_json::Value>> {
    let raw = std::env::var("HARNESS_OPENAI_EXTRA_BODY").ok()?;
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(serde_json::Value::Object(m)) => Some(m),
        _ => {
            tracing::warn!("HARNESS_OPENAI_EXTRA_BODY is not a JSON object; ignoring");
            None
        }
    }
}

/// Serialize a request and fold in `extra_body()` (extra keys win).
fn request_body<T: Serialize>(req: &T) -> serde_json::Value {
    let mut v = serde_json::to_value(req).unwrap_or_else(|_| serde_json::json!({}));
    if let (Some(obj), Some(extra)) = (v.as_object_mut(), extra_body()) {
        for (k, val) in extra {
            obj.insert(k, val);
        }
    }
    v
}

impl OpenAiCompat {
    pub fn new(cfg: LlmConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(http_timeout())
            .build()
            .expect("reqwest client builds");
        Self {
            cfg,
            client,
            context_window: 128_000,
        }
    }

    /// Convenience: 3-arg construction without writing out an `LlmConfig`.
    ///
    /// ```ignore
    /// use harness_models::{OpenAiCompat, providers::DEEPSEEK};
    /// let m = OpenAiCompat::with_key(DEEPSEEK, "deepseek-v4-pro", api_key);
    /// ```
    pub fn with_key(
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let model = model.into();
        Self::new(LlmConfig::new(
            format!("openai-compat:{model}"),
            base_url,
            api_key,
            model,
        ))
    }

    pub fn with_context_window(mut self, window: u32) -> Self {
        self.context_window = window;
        self
    }

    pub fn config(&self) -> &LlmConfig {
        &self.cfg
    }
}

// ----------------------------------------------------------------
// Wire format — OpenAI v1 chat completions, including tool-calling
// ----------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDecl>,
    stream: bool,
    /// Without `stream_options.include_usage: true`, OpenAI-compatible
    /// providers (DeepSeek, OpenAI proper, Groq, Together…) DO NOT emit a
    /// final usage chunk during streaming. We always want it so the agent
    /// loop can populate `Outcome::Done.usage` and downstream consumers
    /// (audit, billing, dashboards) get real token counts. Omitted when
    /// `stream=false` since non-streaming responses always include usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    /// OpenAI-style structured output. `None` ⇒ free-form text.
    /// `{type: "json_object"}` ⇒ any-JSON mode (DeepSeek-compatible).
    /// `{type: "json_schema", json_schema: {name, schema, strict}}` ⇒ schema-
    /// constrained mode (OpenAI proper). See `build_response_format`.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

/// Translate `Context.response_format` into the OpenAI request body. Returns
/// `(response_format_json, schema_hint_for_system_prompt)`:
/// - For providers that fully support `json_schema` (OpenAI proper), the
///   schema rides in `response_format` and the system-prompt hint is empty.
/// - For providers that only support `json_object` (DeepSeek as of Dec
///   2025), the schema is injected into the system prompt as a fallback so
///   the model still has something to conform to, and `response_format` is
///   the loose `json_object` mode.
///
/// The DeepSeek detection is host-based — anything not pointing at OpenAI's
/// `api.openai.com` is assumed to be a compat-shim with limited surface.
fn build_response_format(
    fmt: &harness_core::ResponseFormat,
    base_url: &str,
) -> (Option<serde_json::Value>, Option<String>) {
    use harness_core::ResponseFormat;
    let supports_json_schema = base_url.contains("api.openai.com");
    match fmt {
        ResponseFormat::Free => (None, None),
        ResponseFormat::JsonObject => (Some(serde_json::json!({"type": "json_object"})), None),
        ResponseFormat::JsonSchema { name, schema } => {
            if supports_json_schema {
                (
                    Some(serde_json::json!({
                        "type": "json_schema",
                        "json_schema": {
                            "name": name,
                            "schema": schema,
                            "strict": true,
                        }
                    })),
                    None,
                )
            } else {
                // Compat shim: json_object mode + schema injected into system
                // prompt as a hint. Less strict than json_schema mode but
                // works against DeepSeek / Groq / others.
                let hint = format!(
                    "Respond ONLY with a single JSON object matching this schema (no markdown fences, no prose):\n{}",
                    serde_json::to_string(schema).unwrap_or_else(|_| "{}".into())
                );
                (Some(serde_json::json!({"type": "json_object"})), Some(hint))
            }
        }
        // ResponseFormat is `#[non_exhaustive]`; future variants get safest
        // default (free-form text, no response_format header).
        _ => (None, None),
    }
}

/// `ChatMessage` is intentionally **lenient** — providers add fields
/// (DeepSeek's `reasoning_content`, OpenAI's `refusal`, etc.). We capture
/// `reasoning_content` because DeepSeek thinking mode demands it be echoed
/// back; Ollama exposes the same thing under `reasoning`. Anything else we
/// don't recognise is ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    reasoning_content: Option<String>,
    /// Ollama's thinking-mode channel. Deserialize-only (we never send it
    /// back; the echo, when needed, rides `reasoning_content`).
    #[serde(skip_serializing, default)]
    reasoning: Option<String>,
}

/// OpenAI-compat requires a tool call's `arguments` to be a JSON-object–shaped
/// string. No-arg calls can surface as `""`, `null`, or a non-object value;
/// strict backends (Ollama) reject those with `HTTP 400 invalid tool call
/// arguments`. Normalise anything that isn't a valid JSON object to `"{}"`.
fn normalize_tool_args(args: &serde_json::Value) -> String {
    use serde_json::Value;
    let s = match args {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    match serde_json::from_str::<Value>(&s) {
        Ok(Value::Object(_)) => s,
        _ => "{}".to_string(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type", default = "default_function_type")]
    kind: String,
    function: WireToolFunction,
}

fn default_function_type() -> String {
    "function".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireToolFunction {
    name: String,
    /// OpenAI sends arguments as a JSON-encoded string (not an object).
    arguments: String,
}

#[derive(Debug, Serialize)]
struct ToolDecl {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolDeclFunction,
}

#[derive(Debug, Serialize)]
struct ToolDeclFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: ChatUsage,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

// ----------------------------------------------------------------
// Model impl
// ----------------------------------------------------------------

#[async_trait]
impl Model for OpenAiCompat {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        let (response_format, schema_hint) =
            build_response_format(&ctx.response_format, &self.cfg.base_url);
        let mut messages = build_messages(ctx);
        if let Some(hint) = schema_hint {
            inject_schema_hint(&mut messages, &hint);
        }
        let tools = ctx
            .tools
            .iter()
            .map(|t| ToolDecl {
                kind: "function",
                function: ToolDeclFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input.clone(),
                },
            })
            .collect();

        let req = ChatRequest {
            model: &self.cfg.model,
            messages,
            max_tokens: Some(ctx.policy.max_output_tokens),
            tools,
            stream: false,
            stream_options: None,
            response_format,
        };

        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        tracing::debug!(?req, "openai-compat request");
        let body = request_body(&req);
        let bytes = crate::retry::with_retry("openai-compat:complete", || async {
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.cfg.api_key)
                .json(&body)
                .send()
                .await
                .map_err(|e| crate::retry::Retryable::transient(format!("send: {e}")))?;
            let status = resp.status();
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| crate::retry::Retryable::transient(format!("body: {e}")))?;
            if !status.is_success() {
                // 5xx + 429 → retryable, other 4xx → permanent
                let body = String::from_utf8_lossy(&bytes).to_string();
                let msg = format!("HTTP {status}: {body}");
                return Err(
                    if status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        crate::retry::Retryable::transient(msg)
                    } else {
                        crate::retry::Retryable::permanent(msg)
                    },
                );
            }
            Ok(bytes)
        })
        .await
        .map_err(ModelError::Transport)?;
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
                let args: serde_json::Value = serde_json::from_str(&w.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(w.function.arguments.clone()));
                ToolCall {
                    id: w.id,
                    name: w.function.name,
                    args,
                }
            })
            .collect::<Vec<_>>();

        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolUse
        } else {
            match first.finish_reason.as_deref() {
                Some("stop") => StopReason::EndTurn,
                Some("length") => StopReason::MaxTokens,
                Some("tool_calls") => StopReason::ToolUse,
                Some("content_filter") => StopReason::Other,
                _ => StopReason::EndTurn,
            }
        };

        // DeepSeek puts thinking in `reasoning_content`; Ollama in `reasoning`.
        let reasoning = first.message.reasoning_content.or(first.message.reasoning);

        // Thinking models (e.g. Qwen3 via Ollama) sometimes emit the entire
        // answer into the reasoning channel and return empty `content`. When
        // there's no content and no tool call, surface the reasoning so the
        // turn isn't blank.
        let mut text = first.message.content;
        if tool_calls.is_empty()
            && text.as_deref().map(str::trim).unwrap_or("").is_empty()
            && let Some(r) = &reasoning
            && !r.trim().is_empty()
        {
            text = Some(r.clone());
        }

        Ok(ModelOutput {
            text,
            tool_calls,
            usage: Usage {
                input_tokens: parsed.usage.prompt_tokens,
                output_tokens: parsed.usage.completion_tokens,
                cached_input_tokens: 0,
            },
            stop_reason,
            reasoning,
        })
    }

    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle: self.cfg.name.clone(),
            provider: provider_from_base_url(&self.cfg.base_url),
            model: self.cfg.model.clone(),
            context_window: self.context_window,
            input_cost_usd_per_million_tokens: None,
            output_cost_usd_per_million_tokens: None,
            supports_tool_use: true,
            supports_streaming: true,
        }
    }

    async fn stream(
        &self,
        ctx: &Context,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let (response_format, schema_hint) =
            build_response_format(&ctx.response_format, &self.cfg.base_url);
        let mut messages = build_messages(ctx);
        if let Some(hint) = schema_hint {
            inject_schema_hint(&mut messages, &hint);
        }
        let tools = ctx
            .tools
            .iter()
            .map(|t| ToolDecl {
                kind: "function",
                function: ToolDeclFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input.clone(),
                },
            })
            .collect();
        let req = ChatRequest {
            model: &self.cfg.model,
            messages,
            max_tokens: Some(ctx.policy.max_output_tokens),
            tools,
            stream: true,
            stream_options: Some(StreamOptions {
                include_usage: true,
            }),
            response_format,
        };
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.cfg.api_key)
            .json(&request_body(&req))
            .send()
            .await
            .map_err(|e| ModelError::Transport(format!("send: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ModelError::Transport(format!("HTTP {status}: {body}")));
        }
        let byte_stream = resp.bytes_stream();
        let delta_stream = parse_sse_stream(byte_stream);
        Ok(delta_stream.boxed())
    }
}

/// Parse the SSE byte stream from OpenAI chat completions into `ModelDelta`s.
///
/// Each event is a line like `data: {"choices":[{"delta":{...}}]}` and a
/// final `data: [DONE]` marker. Lines without `data:` prefix (keepalives,
/// comments) are skipped.
fn parse_sse_stream<S>(stream: S) -> impl futures::Stream<Item = Result<ModelDelta, ModelError>>
where
    S: futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + Unpin + 'static,
{
    use futures::stream::unfold;

    struct State<S> {
        upstream: S,
        buf: String,
        done: bool,
        // Partial JSON args for in-flight tool calls, keyed by call index.
        partial_tool_args: std::collections::HashMap<u32, ToolCallAccumPriv>,
    }

    let init = State {
        upstream: stream,
        buf: String::new(),
        done: false,
        partial_tool_args: std::collections::HashMap::new(),
    };

    unfold(init, |mut state| async move {
        if state.done {
            return None;
        }

        loop {
            // Try to find a complete event in the buffer first.
            if let Some(eol) = state.buf.find('\n') {
                let line = state.buf.drain(..=eol).collect::<String>();
                let line = line.trim_end_matches(['\r', '\n']).to_string();
                if let Some(rest) = line.strip_prefix("data:") {
                    let payload = rest.trim();
                    if payload == "[DONE]" {
                        state.done = true;
                        return Some((Ok(ModelDelta::Stop(StopReason::EndTurn)), state));
                    }
                    if payload.is_empty() {
                        continue;
                    }
                    let v: serde_json::Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some(delta) = decode_delta(&v, &mut state.partial_tool_args) {
                        return Some((Ok(delta), state));
                    }
                    continue;
                }
                // ignore non-data lines
                continue;
            }
            // Need more bytes.
            match state.upstream.next().await {
                Some(Ok(bytes)) => {
                    if let Ok(s) = std::str::from_utf8(&bytes) {
                        state.buf.push_str(s);
                    }
                }
                Some(Err(e)) => {
                    state.done = true;
                    return Some((Err(ModelError::Transport(format!("stream: {e}"))), state));
                }
                None => return None,
            }
        }
    })
}

fn decode_delta(
    v: &serde_json::Value,
    partial: &mut std::collections::HashMap<u32, ToolCallAccumPriv>,
) -> Option<ModelDelta> {
    use serde_json::Value;

    // Usage chunk comes with `choices: []`. Handle it FIRST — otherwise the
    // `choices.first()?` below short-circuits and we silently drop the usage.
    // (Only fires when `stream_options.include_usage: true` was set on the
    // request.)
    if let Some(Value::Object(u)) = v.get("usage") {
        let usage = Usage {
            input_tokens: u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            output_tokens: u
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32,
            cached_input_tokens: 0,
        };
        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            return Some(ModelDelta::Usage(usage));
        }
    }

    let choices = v.get("choices")?.as_array()?;
    let first = choices.first()?;
    let delta = first.get("delta")?;

    // Reasoning / thinking-mode content. DeepSeek streams `reasoning_content`
    // alongside (and BEFORE) `content` when the model is in thinking mode.
    // We MUST capture it because the next request to the same conversation
    // is required to echo it back — otherwise DeepSeek returns
    //   400: "The `reasoning_content` in the thinking mode must be passed
    //         back to the API."
    // OpenAI proper doesn't send this field; the guard below makes it a
    // safe no-op for non-DeepSeek streams.
    if let Some(Value::String(r)) = delta
        .get("reasoning_content")
        .or_else(|| delta.get("reasoning"))
        && !r.is_empty()
    {
        return Some(ModelDelta::Reasoning(r.clone()));
    }

    // Plain text content
    if let Some(Value::String(t)) = delta.get("content")
        && !t.is_empty()
    {
        return Some(ModelDelta::Text(t.clone()));
    }
    // Tool calls
    if let Some(Value::Array(tcs)) = delta.get("tool_calls") {
        for tc in tcs {
            let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let acc = partial.entry(idx).or_default();
            if let Some(Value::String(id)) = tc.get("id")
                && acc.id.is_none()
            {
                acc.id = Some(id.clone());
                return Some(ModelDelta::ToolCallStart {
                    id: id.clone(),
                    name: tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
            if let Some(Value::String(args)) = tc.get("function").and_then(|f| f.get("arguments")) {
                acc.args.push_str(args);
                return Some(ModelDelta::ToolCallArgs {
                    id: acc.id.clone().unwrap_or_default(),
                    partial_json: args.clone(),
                });
            }
        }
    }
    // Usage (final chunk on some providers)
    if let Some(Value::Object(u)) = v.get("usage") {
        let usage = Usage {
            input_tokens: u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            output_tokens: u
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32,
            cached_input_tokens: 0,
        };
        return Some(ModelDelta::Usage(usage));
    }
    // Finish reason
    if let Some(Value::String(reason)) = first.get("finish_reason") {
        let r = match reason.as_str() {
            "stop" => StopReason::EndTurn,
            "length" => StopReason::MaxTokens,
            "tool_calls" => StopReason::ToolUse,
            "content_filter" => StopReason::Other,
            _ => StopReason::EndTurn,
        };
        return Some(ModelDelta::Stop(r));
    }
    None
}

#[derive(Default)]
struct ToolCallAccumPriv {
    id: Option<String>,
    args: String,
}

fn provider_from_base_url(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("openai-compat")
        .to_string()
}

/// Append a schema hint to the system message (creating one if necessary).
/// Used when the provider only supports `json_object` mode — we put the
/// schema where the model can see it. Two newlines to separate from
/// existing system text.
fn inject_schema_hint(messages: &mut Vec<ChatMessage>, hint: &str) {
    if let Some(sys) = messages.iter_mut().find(|m| m.role == "system") {
        let existing = sys.content.take().unwrap_or_default();
        let joined = if existing.trim().is_empty() {
            hint.to_string()
        } else {
            format!("{existing}\n\n{hint}")
        };
        sys.content = Some(joined);
    } else {
        messages.insert(
            0,
            ChatMessage {
                role: "system".into(),
                content: Some(hint.to_string()),
                tool_calls: Vec::new(),
                tool_call_id: None,
                reasoning_content: None,
                reasoning: None,
            },
        );
    }
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
            reasoning: None,
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
            role: "user".into(),
            content: Some(ctx.task.description.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            reasoning_content: None,
            reasoning: None,
        });
    }
    out
}

fn translate_turn(turn: &harness_core::Turn, out: &mut Vec<ChatMessage>) {
    let role = match turn.role {
        TurnRole::User => "user",
        TurnRole::Assistant => "assistant",
        TurnRole::Tool => "tool",
        TurnRole::System => "system",
        _ => "user",
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
                    role: "tool".into(),
                    content: Some(s),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call_id.clone()),
                    reasoning_content: None,
                    reasoning: None,
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
                    role: "user".into(),
                    content: Some(s),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    reasoning_content: None,
                    reasoning: None,
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
            Block::Text(s) => {
                text.push_str(s);
                text.push('\n');
            }
            Block::ToolCall {
                call_id,
                name,
                args,
            } => {
                tool_calls.push(WireToolCall {
                    id: call_id.clone(),
                    kind: "function".into(),
                    function: WireToolFunction {
                        name: name.clone(),
                        arguments: normalize_tool_args(args),
                    },
                });
            }
            Block::Reasoning(r) => {
                // Echo back what the model said it was thinking. DeepSeek
                // requires this; OpenAI ignores unknown fields.
                reasoning = Some(
                    reasoning
                        .map(|prev| format!("{prev}\n{r}"))
                        .unwrap_or_else(|| r.clone()),
                );
            }
            Block::ToolResult { .. } | Block::Feedback(_) => {
                // shouldn't appear in assistant/user turns; ignore
            }
            other => push_block_text(&mut text, other),
        }
    }
    out.push(ChatMessage {
        role: role.into(),
        content: if text.trim().is_empty() {
            None
        } else {
            Some(text)
        },
        tool_calls,
        tool_call_id: None,
        reasoning_content: reasoning,
        reasoning: None,
    });
}

fn push_block_text(buf: &mut String, b: &Block) {
    match b {
        Block::Text(s) => {
            buf.push_str(s);
            buf.push('\n');
        }
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
        _ => {} // forward-compat: skip unknown block variants
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Policy, Task};
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn sse_stream_parses_text_then_done() {
        use futures::stream::StreamExt;

        let chunks = vec![
            br#"data: {"choices":[{"delta":{"content":"Hello"}}]}
"#
            .to_vec()
            .into(),
            br#"data: {"choices":[{"delta":{"content":" world"}}]}
"#
            .to_vec()
            .into(),
            br#"data: {"choices":[{"finish_reason":"stop"}]}
"#
            .to_vec()
            .into(),
            br#"data: [DONE]
"#
            .to_vec()
            .into(),
        ];
        // Convert Vec<Bytes> into a Stream<Result<Bytes, reqwest::Error>>.
        // We synthesise an Ok-only stream — the parser doesn't care about the
        // error type as long as it never has to construct one.
        let stream = futures::stream::iter(
            chunks
                .into_iter()
                .map::<Result<bytes::Bytes, reqwest::Error>, _>(Ok),
        );

        let mut deltas = Vec::new();
        let mut s = std::pin::pin!(parse_sse_stream(stream));
        while let Some(d) = s.next().await {
            deltas.push(d.unwrap());
        }
        let texts: Vec<String> = deltas
            .iter()
            .filter_map(|d| {
                if let ModelDelta::Text(s) = d {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(texts, vec!["Hello".to_string(), " world".to_string()]);
        let has_stop = deltas.iter().any(|d| matches!(d, ModelDelta::Stop(_)));
        assert!(has_stop, "expected a Stop delta");
    }

    #[tokio::test]
    async fn sse_stream_parses_tool_call_increments() {
        use futures::stream::StreamExt;
        let chunks = vec![
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":""}}]}}]}
"#.to_vec().into(),
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}
"#.to_vec().into(),
            br#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]}}]}
"#.to_vec().into(),
            br#"data: [DONE]
"#.to_vec().into(),
        ];
        let stream = futures::stream::iter(
            chunks
                .into_iter()
                .map::<Result<bytes::Bytes, reqwest::Error>, _>(Ok),
        );
        let mut deltas = Vec::new();
        let mut s = std::pin::pin!(parse_sse_stream(stream));
        while let Some(d) = s.next().await {
            deltas.push(d.unwrap());
        }
        let starts: usize = deltas
            .iter()
            .filter(|d| matches!(d, ModelDelta::ToolCallStart { .. }))
            .count();
        let args_count: usize = deltas
            .iter()
            .filter(|d| matches!(d, ModelDelta::ToolCallArgs { .. }))
            .count();
        assert_eq!(starts, 1);
        assert_eq!(args_count, 2);
    }

    #[test]
    fn normalize_tool_args_coerces_empty_to_object() {
        use serde_json::{Value, json};
        // No-arg calls in their various empty forms → "{}" (Ollama rejects "").
        assert_eq!(normalize_tool_args(&Value::String(String::new())), "{}");
        assert_eq!(normalize_tool_args(&Value::Null), "{}");
        assert_eq!(normalize_tool_args(&json!("not json")), "{}");
        assert_eq!(normalize_tool_args(&json!([1, 2])), "{}");
        // Real object args pass through unchanged.
        assert_eq!(normalize_tool_args(&json!({"a": 1})), r#"{"a":1}"#);
        // Object already encoded as a JSON string passes through.
        assert_eq!(
            normalize_tool_args(&Value::String(r#"{"a":1}"#.into())),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn build_messages_emits_system_and_user() {
        let ctx = Context {
            system: vec![Block::Text("you are a helpful agent".into())],
            guides: vec![Block::Text("always be concise".into())],
            history: vec![],
            task: Task {
                description: "say hi".into(),
                source: None,
                deadline: None,
            },
            policy: Policy::default(),
            metadata: BTreeMap::new(),
            tools: Vec::new(),
            response_format: harness_core::ResponseFormat::Free,
        };
        let msgs = build_messages(&ctx);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert!(
            msgs[0]
                .content
                .as_deref()
                .unwrap()
                .contains("helpful agent")
        );
        assert!(msgs[0].content.as_deref().unwrap().contains("be concise"));
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content.as_deref(), Some("say hi"));
    }
}
