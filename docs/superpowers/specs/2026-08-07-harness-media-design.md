# harness media — Image & Speech Generation (Framework Capability)

**Status:** Implemented — `feat/media-image-speech`, workspace 0.0.38
**Date:** 2026-08-07
**Layer:** harness-rs framework (NOT the picture-book product)
**Driver:** an English children's picture-book backend needs to generate
illustrations and narration. Neither capability exists in the framework today,
and one of them is actively broken (see "The silent bug" below).

## Goal

Give any harness-rs application two new opt-in capabilities, in the same shape
as the existing `Embedder`:

```rust
let img: Arc<dyn ImageModel>  = Arc::new(ChatImageModel::with_key(CPA, "gemini-3.1-flash-image", key));
let tts: Arc<dyn SpeechModel> = Arc::new(DashScopeSpeech::with_key(MAAS, "qwen3-tts-flash", key));

let pages = img.generate(&ImageRequest::new(prompt).with_references(vec![cover_ref])).await?;
let wav   = tts.synthesize(&SpeechRequest::new(line).voice("Cherry")).await?;
```

Plus fix `ModelOutput` so chat models that emit images stop having those images
silently discarded.

## Empirical findings (all measured 2026-08-07, not from docs)

Every design decision below traces to one of these. Recorded because they are
expensive to rediscover and contradict the vendors' own documentation.

### Three providers, three wire formats, three transports

| Provider | Capability | Path | Response shape |
|---|---|---|---|
| CPA gateway (`cpa.superleo.app/v1`) | image | **`/chat/completions`** | `choices[0].message.images[0].image_url.url` = `data:image/jpeg;base64,…` (~1.1 MB) |
| Aliyun MaaS (`…maas.aliyuncs.com`) | speech | **`/api/v1/services/aigc/multimodal-generation/generation`** (DashScope native) | `output.audio.url` = signed OSS URL, **expires in 24 h** |
| OpenAI (reference shape) | image | `/v1/images/generations` | `data[0].b64_json` or `data[0].url` |
| OpenAI (reference shape) | speech | `/v1/audio/speech` | raw audio bytes in the body |

Three transports for the same logical operation: inline base64, expiring remote
URL, raw body. **This is exactly what the framework should absorb.**

### Traps hit while probing

1. **MaaS `/compatible-mode/v1` is a partial shim.** Only `/chat/completions`
   and `/models` respond. `/audio/speech` and `/images/generations` are **404**.
   TTS *must* use the DashScope-native path.
2. **`qwen3-tts-flash` over `/chat/completions` cannot be made to work.** Every
   variant (`audio.voice`, top-level `voice`, `modalities`, content-as-array,
   `language_type`, stream on/off) returns `Due to invalid text, invalid audio
   was returned.` The sibling `qwen-tts-2025-05-22` leaks the reason:
   `Field required: input.text` — the shim never maps `messages` → `input.text`.
   Native path with `{"input":{"text":…,"voice":…}}` works first try.
3. **`gpt-image-1.5` is advertised but unroutable** on the CPA gateway —
   `/v1/images/generations` returns `unknown provider for model gpt-image-1.5`,
   and it is absent from `/v1/models`. Only `gemini-3.1-flash-image` works, and
   only via `/chat/completions`.
4. **MaaS image models are quota-locked** on the current key:
   `Throttling.RateQuota` returned in ~0.14 s (a hard quota, not transient
   load), and async submission is refused outright with
   `current user api does not support asynchronous calls`. Retry-with-backoff
   is mandatory, not optional, for this class of call.
5. **DashScope audio URLs expire.** `expires_at` is ~24 h out. An adapter that
   returns the URL to the caller is handing them a time bomb.

### Verified working, end to end

- Image: `gemini-3.1-flash-image` via CPA `/chat/completions` → real illustration.
- Speech: `qwen3-tts-flash` via MaaS native path → 24 kHz mono PCM WAV,
  5.28 s for one sentence, 248 KB.

## The silent bug

`harness-models/src/openai_compat.rs` handles images as **input** (vision:
`ContentPart::ImageUrl`, `crates/harness-models/src/openai_compat.rs:269-290`)
but its response structs (`ChatMessage`, `crates/harness-models/src/openai_compat.rs:297`)
deserialize only `content`, `reasoning_content`, `reasoning`, `tool_calls`,
`tool_call_id`.

Serde drops unknown fields. So when a chat model returns `message.images[]`,
**harness-rs throws the image away without a warning** and hands the caller a
`ModelOutput` whose `text` is `None`. The caller sees an empty response and has
no way to learn why.

This is not just a picture-book concern: any agentic image-editing or
chart-drawing turn through a modern chat model loses its output today.

## Design principles

- **Normalise to bytes at the adapter boundary.** `GeneratedImage` and
  `SpeechAudio` carry `Vec<u8>` + a `mime`. Base64 decoding and remote-URL
  fetching happen *inside* the adapter. Callers never learn which of the three
  transports their provider used, and never hold an expiring URL. This is the
  single most valuable thing the framework does here.
- **Strictly opt-in, mirroring `Embedder`.** Nothing in `Model`, `AgentLoop`,
  `Hook`, `Guide`, `Sensor`, or `Memory` references these traits. Code that
  wants them holds an `Arc<dyn ImageModel>` explicitly.
- **Traits in `harness-core`, adapters in `harness-models`** — the exact
  `embed.rs` → `embed_gemini.rs` layering already in the tree.
- **One error enum per capability** (`ImageError`, `SpeechError`), following
  `EmbedError`'s precedent: the surfaces differ and adapters should not reach
  across modules.
- **No new framework dependencies.** `reqwest`, `serde`, `base64`, and
  `async-trait` are already in `harness-models`.
- **No silent format substitution.** If a caller asks for MP3 and the provider
  only emits WAV, return `SpeechError::Unsupported` — do not quietly hand back
  a different format. Finding out via a corrupt file downstream is worse.

## New: `harness-core/src/image.rs`

```rust
/// A reference image fed *into* generation — the mechanism behind character
/// consistency across a multi-page illustrated sequence.
pub enum ImageRef {
    Base64 { mime: String, data: String },
    Url(String),
    Bytes { mime: String, data: Vec<u8> },
}

pub struct ImageRequest {
    pub prompt: String,
    /// Provider-native size string (e.g. "1024x1024"). `None` = provider default.
    pub size: Option<String>,
    pub n: u8,
    /// Reference images. Adapters that cannot accept references MUST return
    /// `ImageError::Unsupported` rather than silently ignoring them — a
    /// dropped reference produces a plausible-looking but wrong image, which
    /// is the hardest kind of failure to notice.
    pub references: Vec<ImageRef>,
}

pub struct GeneratedImage {
    pub mime: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum ImageError {
    Transport(String),
    Provider(String),
    BadInput(String),
    /// Provider/model does not support a requested feature (reference images,
    /// n > 1, a given size).
    Unsupported(String),
    /// Rate limit / quota. Separated from `Provider` because callers should
    /// back off and retry rather than fail the task. Measured: MaaS image
    /// models return this in 0.14 s.
    RateLimited(String),
}

#[async_trait]
pub trait ImageModel: Send + Sync + 'static {
    /// Adapters MUST return exactly `req.n` images, or an error. Bytes are
    /// fully materialised — no base64, no URLs, no expiry.
    async fn generate(&self, req: &ImageRequest) -> Result<Vec<GeneratedImage>, ImageError>;

    /// e.g. `"cpa:gemini-3.1-flash-image"`. For logs and cache keys.
    fn handle(&self) -> &str;
}
```

## New: `harness-core/src/speech.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AudioFormat { Wav, Mp3, Opus }

pub struct SpeechRequest {
    pub text: String,
    /// Provider-native voice id (e.g. "Cherry", "alloy").
    pub voice: String,
    /// Provider-native language hint (e.g. "English"). `None` = auto-detect.
    pub language: Option<String>,
    pub format: AudioFormat,
}

pub struct SpeechAudio {
    pub mime: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum SpeechError {
    Transport(String),
    Provider(String),
    BadInput(String),
    Unsupported(String),
    RateLimited(String),
}

#[async_trait]
pub trait SpeechModel: Send + Sync + 'static {
    /// Adapters MUST return audio in `req.format` or `Unsupported`. Adapters
    /// whose provider returns a URL MUST fetch it before returning — callers
    /// never receive an expiring handle.
    async fn synthesize(&self, req: &SpeechRequest) -> Result<SpeechAudio, SpeechError>;

    /// e.g. `"maas:qwen3-tts-flash"`. For logs, and so callers can build a
    /// content-addressed cache key over (handle, voice, format, text).
    fn handle(&self) -> &str;
}
```

`handle()` exists on both traits for the same reason `Embedder::handle()` does:
a stored artifact must record what produced it, so a model swap is detectable.

## Change: `ModelOutput.images`

```rust
pub struct ModelOutput {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    pub reasoning: Option<String>,
    /// Images emitted by the model in this turn. Empty for text-only models.
    /// Populated from OpenAI-compat `message.images[]` (Gemini image models)
    /// and Anthropic/Gemini native image parts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<GeneratedImage>,
}
```

`openai_compat.rs`'s `ChatMessage` gains a deserialize-only `images` field,
mapped through on the response path. `GeneratedImage` needs
`Serialize`/`Deserialize` for this (bytes as base64) — session recording and
replay must round-trip it.

**Cost:** `ModelOutput` is built as a struct literal in **28 places across 14
files** (`harness-loop` ×6, `harness-models` ×4, `harness-experience`,
`harness-hooks`, `harness-core`, `harness-scheduler`). All need the new field.
Mechanical, but it is a breaking change to every downstream literal, hence the
minor version bump.

## Adapters (`harness-models`)

| File | Type | Notes |
|---|---|---|
| `image_chat.rs` | `ChatImageModel` | Any OpenAI-compat `/chat/completions` returning `message.images[]`. **Primary path** — verified against CPA `gemini-3.1-flash-image`. `references` encode as vision `image_url` parts, reusing the existing request builder. |
| `image_openai.rs` | `OpenAiImage` | `/v1/images/generations`; handles both `b64_json` and `url` responses. |
| `image_dashscope.rs` | `DashScopeImage` | Native multimodal-generation. Ships now, unusable until the account quota is lifted; drop-in swap behind the trait. |
| `speech_dashscope.rs` | `DashScopeSpeech` | Native path. **Fetches the OSS URL and returns bytes.** Verified. |
| `speech_openai.rs` | `OpenAiSpeech` | `/v1/audio/speech`, raw body. |

All adapters route through the existing `harness-models/src/retry.rs`, with
`RateLimited` as a retryable class. Finding 4 makes this a correctness
requirement, not a nicety.

## Testing

- **Unit, no network:** fixture-driven deserialisation for each of the three
  wire shapes, captured verbatim from today's probes. Specifically: a CPA
  `message.images[]` response, a DashScope `output.audio.url` response, and an
  OpenAI `b64_json` response.
- **Regression for the silent bug:** feed `openai_compat` a response whose
  `message` has `content: null` and a populated `images[]`; assert
  `ModelOutput.images.len() == 1` and that bytes decode to a valid JPEG header.
  This test fails on today's `main`.
- **Error mapping:** assert `Throttling.RateQuota` → `ImageError::RateLimited`
  (not `Provider`), so retry actually engages.
- **URL-fetch behaviour:** a mock HTTP server returns a URL response; assert
  the adapter performs the fetch and the caller receives bytes.
- **Live tests** are `#[ignore]`d and key-gated, in the existing style.

## Non-goals

- Video, image editing/inpainting, ASR. Not needed; add when a driver appears.
- Streaming audio. The picture book stores whole files; realtime TTS
  (`qwen3-tts-flash-realtime`) is a separate shape and can come later.
- A `MediaTool` for agent tool-calling. These are library capabilities first.
  A tool wrapper is trivial to add on top once the traits are proven.
- Caching. Content-addressed storage of generated audio is an *application*
  concern (the picture book keys `say/<hash>.wav` on text). The framework
  exposes `handle()` so apps can build correct keys, and stops there.

## Open questions

1. Should `ModelOutput.images` be feature-gated to avoid the 28-site churn for
   apps that will never use it? **Recommendation: no.** A feature flag on a
   struct field fragments the type across the workspace and the churn is
   one-time and mechanical.
2. Does `AnthropicNative` need image-output parsing too? Deferred — no verified
   provider to test against right now, and shipping untested parsing is worse
   than not shipping it.

## Version

Workspace `0.0.37` → `0.0.38`.

---

## As built — where the implementation diverged from this draft

Recorded because the draft is now the historical record and the code is the
truth.

**`mime` → `media_type`.** The draft named the field `mime`; `harness-core`
already says `media_type` on `Block::Image` / `Block::Audio`. Consistency with
the surrounding code beat consistency with this document.

**`harness-core::b64` is new, and was not planned.** `context.rs` already
carried a dependency-free base64 *encoder* with an explicit comment about
keeping the crate lean. Decoding provider data URIs needed the other half, and
three modules now share it, so it was lifted into its own module rather than
duplicated. `parse_data_uri` and a `serde_with` adapter live there too — the
latter because serde's default `Vec<u8>` encoding would inflate a 1 MB JPEG
into ~4 MB of JSON in every recorded session.

**`harness-models::media_http` is new.** Five adapters were about to carry five
copies of "is this a rate limit". That drifts, and it drifts silently: one
adapter retries `Throttling.RateQuota`, another fails the job. One classifier,
one place to fix it.

**`retry::with_retry_typed` is new.** The existing `with_retry` collapses errors
to `String`, which would have thrown away the `RateLimited` vs `Provider`
distinction the adapters had just established — leaving callers to re-derive it
by grepping an error message. The typed variant keeps the enum.

**`ModelOutput` and `StopReason` now derive `Default`.** Not in the draft. The
28-site churn was going to recur on every future field; `..Default::default()`
makes this the last time.

**`ModelDelta` was not extended.** Streaming image output has no verified
provider — the Gemini image path answers non-streamed — so `AgentLoop`'s
streaming accumulator sets `images: Vec::new()` with a comment naming what
would have to exist first.

**Open question 1 resolved as recommended:** no feature gate on
`ModelOutput.images`.

**Open question 2 resolved as deferred:** `AnthropicNative` and `gemini.rs`
native `inline_data` still return empty `images`, both with comments saying so.
No verified provider to test against.

### Verification

- 452 workspace tests pass; `cargo clippy --workspace --all-targets` clean.
- 44 new unit tests across the new modules, all fixture-driven from bodies
  captured on 2026-08-07.
- 3 live tests (`tests/live_media.rs`, `#[ignore]`, key-gated) run green
  against the real endpoints: 1,089,423 bytes of `image/jpeg`; 291,884 bytes of
  RIFF/WAVE audio; and the reference-image path returning a distinct image.

### Still open

- MaaS image quota (`Throttling.RateQuota`) — needs an account-side fix before
  `DashScopeImage`'s response parsing can be confirmed against reality.
- `OpenAiImage` / `OpenAiSpeech` have no live verification either; no key.
