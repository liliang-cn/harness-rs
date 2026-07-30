use crate::{Context, error::ModelError};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Information about a configured model — uniform across providers.
///
/// `handle` is the user-chosen logical identifier (used in logs, metrics,
/// and `harness.toml` selectors); `model` is the wire-protocol model id
/// sent to the provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub handle: String,
    pub provider: String,
    pub model: String,
    pub context_window: u32,
    pub input_cost_usd_per_million_tokens: Option<f64>,
    pub output_cost_usd_per_million_tokens: Option<f64>,
    pub supports_tool_use: bool,
    pub supports_streaming: bool,
    /// Whether the provider will answer from live web results using its own
    /// built-in search (Gemini's `google_search`, and friends). Distinct from
    /// `supports_tool_use`: this is search the provider runs server-side, not a
    /// tool we hand it.
    #[serde(default)]
    pub supports_web_grounding: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOutput {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    /// Provider-specific reasoning trace (DeepSeek `reasoning_content`,
    /// Anthropic `thinking` blocks). Pushed back to the API verbatim on
    /// subsequent calls; required by providers that gate on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Other,
}

/// Streaming delta — incremental output from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ModelDelta {
    Text(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallArgs {
        id: String,
        partial_json: String,
    },
    ToolCallEnd {
        id: String,
    },
    Usage(Usage),
    Stop(StopReason),
    /// Provider-specific reasoning trace that must round-trip on the next
    /// request. DeepSeek thinking content, Anthropic thinking blocks, and
    /// Gemini raw `parts` (with thoughtSignatures) all flow through this —
    /// the AgentLoop folds them into the final `ModelOutput.reasoning`
    /// without surfacing them to user-visible token streams.
    Reasoning(String),
}

#[async_trait]
pub trait Model: Send + Sync + 'static {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError>;

    /// Answer `query` from live web results, using the provider's OWN search.
    ///
    /// Returns `None` when the provider has no such thing, which is the default
    /// — callers fall back to a search index. The answer comes back as prose
    /// with its sources, not as a result list, because that is what these
    /// providers return.
    ///
    /// Why it is a separate call rather than a tool the loop can pick: the
    /// providers that offer built-in search generally refuse to accept it in
    /// the same request as function declarations. Gemini answers *"Please
    /// enable tool_config.include_server_side_tool_invocations to use Built-in
    /// tools with Function calling"*, and OpenAI-compatible gateways in front
    /// of it usually drop that switch. So grounding has to happen in its own
    /// tool-free request, and hiding that here keeps every caller from
    /// rediscovering it.
    async fn search_web(&self, query: &str) -> Option<Result<String, ModelError>> {
        let _ = query;
        None
    }

    /// Streaming is optional; default implementation falls back to `complete`.
    async fn stream(
        &self,
        ctx: &Context,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError>
    {
        let out = self.complete(ctx).await?;
        let deltas: Vec<Result<ModelDelta, ModelError>> = out
            .text
            .into_iter()
            .map(|t| Ok(ModelDelta::Text(t)))
            .chain(std::iter::once(Ok(ModelDelta::Stop(out.stop_reason))))
            .collect();
        Ok(Box::pin(futures::stream::iter(deltas)))
    }

    fn info(&self) -> ModelInfo;
}

/// A concrete newtype wrapping a boxed model, so an `Arc<dyn Model>` can be used
/// where a concrete `M: Model` is required (e.g. `Subagent::new` / `AgentLoop<M>`).
///
/// We deliberately do NOT `impl Model for Arc<dyn Model>` directly. Doing so
/// changes `.stream()` method resolution on EVERY `Arc<dyn Model>` value in the
/// program (from a deref to `dyn Model` into the Arc impl's `async fn stream`
/// RPITIT), and proving that boxed streaming future is `Send` inside a `Send`
/// context (e.g. an axum handler driving the streaming loop) overflows the
/// auto-trait solver (E0275). Wrapping in this concrete newtype gives callers
/// `DynModel: Model` without touching resolution for bare `Arc<dyn Model>`.
pub struct DynModel(pub std::sync::Arc<dyn Model>);

#[async_trait]
impl Model for DynModel {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.0.complete(ctx).await
    }
    async fn stream(
        &self,
        ctx: &Context,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError>
    {
        self.0.stream(ctx).await
    }
    fn info(&self) -> ModelInfo {
        self.0.info()
    }
}

#[cfg(test)]
mod arc_model_tests {
    use super::*;
    use std::sync::Arc;

    struct Dummy;

    #[async_trait]
    impl Model for Dummy {
        async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
            Ok(ModelOutput {
                text: Some("ok".into()),
                tool_calls: vec![],
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
                reasoning: None,
            })
        }
        fn info(&self) -> ModelInfo {
            ModelInfo {
                handle: "dummy".into(),
                provider: "test".into(),
                model: "dummy".into(),
                context_window: 8192,
                input_cost_usd_per_million_tokens: None,
                output_cost_usd_per_million_tokens: None,
                supports_tool_use: false,
                supports_streaming: false,
                supports_web_grounding: false,
            }
        }
    }

    fn assert_is_model<M: Model>(_m: &M) {}

    #[tokio::test]
    async fn dyn_model_wrapper_is_a_model() {
        let m: Arc<dyn Model> = Arc::new(Dummy);
        let wrapped = DynModel(m);
        assert_is_model(&wrapped); // compiles only if DynModel: Model
        let out = wrapped
            .complete(&Context::new(crate::Task {
                description: "x".into(),
                source: None,
                deadline: None,
            }))
            .await
            .unwrap();
        assert_eq!(out.text.as_deref(), Some("ok"));
    }
}
