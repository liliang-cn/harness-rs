# harness-rs-models

[![crates.io](https://img.shields.io/crates/v/harness-rs-models.svg)](https://crates.io/crates/harness-rs-models)

**LLM model adapters** for harness-rs. Every provider is configured the same
way — `base_url + model + api_key` — and there are exactly **three protocol
families**. No hardcoded vendor URLs, no provider menu: you always pass the
endpoint yourself.

| Family | `ApiKind` | Speaks | Works with |
|---|---|---|---|
| OpenAI-compatible | `ApiKind::OpenAI` | `/chat/completions` | OpenAI, DeepSeek, Groq, Together, Ollama, DashScope, vLLM, Aliyun MaaS, … |
| Anthropic-native | `ApiKind::Anthropic` | Messages API | Anthropic |
| Gemini-native | `ApiKind::Gemini` | generateContent | Google Gemini |

## One entry point

```rust,ignore
use harness_models::ApiKind;

// kind + base_url + model + key → Arc<dyn Model>
let model = ApiKind::OpenAI.build(
    "https://api.deepseek.com", "deepseek-chat",
    std::env::var("DEEPSEEK_API_KEY")?,
);
```

Or construct an adapter directly when you want the concrete type:

```rust,ignore
use harness_models::{OpenAiCompat, AnthropicNative, GeminiNative};

let ds  = OpenAiCompat::with_key("https://api.deepseek.com", "deepseek-chat", ds_key);
let cl  = AnthropicNative::with_key("https://api.anthropic.com", "claude-sonnet-5", an_key);
let gem = GeminiNative::with_key("https://generativelanguage.googleapis.com", "gemini-2.5-pro", g_key);
```

Any adapter implements `harness_core::Model`, so it drops straight into
`AgentLoop::new(model)`, a `Subagent`, the orchestrator, or the scheduler.

## Config

All adapters share the 4-field `LlmConfig`:

- `name` — your logical handle (e.g. `"prod-fast"`), appears in traces
- `base_url` — endpoint root
- `api_key` — bearer credential
- `model` — wire-protocol model id

`with_key(base_url, model, key)` is the shorthand that fills `name` for you.

## Also here

- **`MockModel`** — deterministic canned responses for tests, no network.
- **Embeddings** — `OllamaEmbed`, Gemini embeddings (`text-embedding-*`).
- **Retry** — transport errors retry with backoff; permanent failures (4xx like
  a 401) fail fast and are logged at `ERROR`.

## License

MIT OR Apache-2.0.
