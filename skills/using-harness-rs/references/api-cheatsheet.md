# harness-rs API cheatsheet

Every trait, every macro, every important method. Use as a reference when wiring an agent.

## Core traits — `harness-rs-core`

```rust
#[async_trait]
pub trait Model: Send + Sync + 'static {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError>;
    async fn stream(&self, ctx: &Context) -> Result<BoxStream<'_, ModelDelta>, ModelError>;
    fn info(&self) -> ModelInfo;
}

#[async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn schema(&self) -> &ToolSchema;
    fn risk(&self) -> ToolRisk;                 // ReadOnly | Idempotent | Destructive | Network
    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError>;
}

#[async_trait]
pub trait Guide: Send + Sync + 'static {
    fn id(&self) -> GuideId;
    fn kind(&self) -> Execution;                // Computational | Inferential
    fn scope(&self) -> GuideScope;              // Always | TaskMatches(...)
    async fn apply(&self, ctx: &mut Context, w: &World) -> Result<(), GuideError>;
}

#[async_trait]
pub trait Sensor: Send + Sync + 'static {
    fn id(&self) -> SensorId;
    fn kind(&self) -> Execution;
    fn stage(&self) -> Stage;                   // PreAction | SelfCorrect | PreCommit | PostIntegrate | Continuous
    async fn observe(&self, action: &Action, w: &World) -> Result<Vec<Signal>, SensorError>;
}

pub trait Hook: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn matches(&self, ev: &Event<'_>) -> bool;
    fn fire(&self, ev: &Event<'_>, w: &mut World) -> HookOutcome;  // Allow | Deny | Inject | Mutate
}

#[async_trait]
pub trait Compactor: Send + Sync + 'static {
    fn budget(&self, ctx: &Context) -> Budget;
    async fn compact(&self, stage: CompactionStage, ctx: &mut Context) -> Result<(), CompactError>;
}

pub trait Skill: Send + Sync + 'static {
    fn manifest(&self) -> &SkillManifest;
    fn body(&self) -> Cow<'_, str>;
    fn resources(&self) -> &[Resource] { &[] }
    fn handler(&self) -> Option<SkillHandler> { None }
}
```

## Signal — feedback structure

```rust
pub struct Signal {
    pub severity:   Severity,        // Block | Warn | Hint
    pub origin:     SensorId,
    pub message:    String,          // human-readable
    pub agent_hint: Option<String>,  // required when severity=Block; the LLM correction prompt
    pub auto_fix:   Option<FixPatch>,// ReplaceFile | UnifiedDiff | RunCommand — applied without the model
    pub location:   Option<CodeSpan>,
}
```

## The 27 `Event<'a>` variants (HookBus)

```rust
SessionStart { source: SessionSource }    // Startup | Resume | Clear | Compact
SessionEnd
Heartbeat        { iter: u32 }
PreModel         { ctx: &'a Context }
PostModel        { out: &'a ModelOutput }
PreToolUse       { action: &'a Action }
PostToolUse      { action: &'a Action, result: &'a ToolResult }
PermissionRequest{ action: &'a Action }
PreGuide         { guide: GuideId }
PostGuide        { guide: GuideId }
PreSensor        { sensor: SensorId }
PostSensor       { sensor: SensorId, signals: &'a [Signal] }
PreCompact       { stage: CompactionStage }
PostCompact      { stage: CompactionStage }
SubagentStart    { spec: &'a SubagentSpec }
SubagentReport   { status: SubagentStatus }   // Done | DoneWithConcerns | Blocked | NeedsContext
FileChanged      { path: &'a Path }
CwdChanged       { from: &'a Path, to: &'a Path }
BlueprintNodeEnter { node: NodeId }
BlueprintNodeExit  { node: NodeId, transition: &'a Transition }
TaskCompleted
BudgetWarning    { ratio: f32 }
Notification     { kind: NotificationKind }
Error            { err: &'a HarnessError }
Stop
Custom           { name: &'a str, data: &'a Value }
```

## Macros — `harness-rs-macros`

### `#[skill]`

```rust
#[harness_rs::skill(
    name        = "format-rust",        // REQUIRED — agentskills.io regex: [a-z0-9-]+
    description = "...",                // REQUIRED if no /// doc-comment
    license     = "Apache-2.0",         // optional
    compatibility = "...",              // optional
    allowed_tools = "Bash(cargo:fmt)",  // optional, experimental in spec
    harness(kind = "computational",     // optional sub-tree — emitted under metadata.harness in SKILL.md
            risk = "read-only"),
)]
async fn format_rust(ctx: &mut Context, w: &mut World) -> Result<(), SkillError> { ... }
```

Auto-registers via `inventory`. `harness skills export <dir>` materialises a spec-compliant `<dir>/<name>/SKILL.md` for external agents to consume.

### `#[tool]`

```rust
#[harness_rs::tool(
    name        = "ripgrep",            // REQUIRED
    risk        = "read-only",          // read-only | idempotent | destructive | network
    description = "...",                // optional; falls back to /// doc-comment
    schema      = r#"{"type":"object","properties":{...}}"#,  // REQUIRED JSON Schema (string)
)]
async fn ripgrep(args: serde_json::Value, w: &mut World) -> Result<ToolResult, ToolError> { ... }
```

### `#[guide]`

```rust
#[harness_rs::guide(
    id    = "rust-conventions",         // optional; defaults to fn name
    scope = "always",                   // "always" | r#"files:src/api/**"# | r#"task:matches\(/axum/\)"#
    kind  = "inferential",              // optional; default "inferential"
)]
async fn rust_conventions(ctx: &mut Context, w: &World) -> Result<(), GuideError> { ... }
```

### `#[sensor]`

```rust
#[harness_rs::sensor(
    id    = "cargo-fmt-check",
    stage = "self-correct",             // pre-action | self-correct | pre-commit | post-integrate | continuous
    kind  = "computational",
)]
async fn cargo_fmt_check(action: &Action, w: &World) -> Result<Vec<Signal>, SensorError> { ... }
```

### `#[hook]`

```rust
#[harness_rs::hook(
    event = "PreToolUse",               // any of the 27 event variant names
    name  = "audit-shell",              // optional
)]
fn audit_shell(ev: &Event<'_>, w: &mut World) -> HookOutcome { ... }   // SYNC, not async
```

## `AgentLoop` API — `harness-rs-loop`

```rust
let outcome = AgentLoop::new(model)
    .with_tool(Arc::new(ReadFile))            // chained
    .with_tools(vec![Arc::new(WriteFile), Arc::new(ListDir)])
    .with_guide(Arc::new(my_guide))
    .with_sensor(Arc::new(my_sensor))
    .with_hook(Arc::new(my_hook))
    .with_compactor(Arc::new(custom_compactor))   // defaults to DefaultCompactor
    .run(task, &mut world).await?;                 // uses Policy::default().max_iters = 50
    // or:
    .run_with_max_iters(task, &mut world, 20).await?;

// Outcome:
match outcome {
    Outcome::Done { text, iters }          => println!("done in {iters} iter(s): {text:?}"),
    Outcome::BudgetExhausted { iters }     => eprintln!("budget out after {iters}"),
}
```

`Task` is the only required argument:

```rust
let task = Task {
    description: "Translate README to English".into(),
    source: None,             // optional structured origin
    deadline: None,           // optional wall-clock deadline
};
// Or:
let task = Task::from("Translate README to English");
```

## Model adapters — `harness-rs-models`

```rust
// OpenAI-compatible: 3-arg with_key, or full LlmConfig
OpenAiCompat::with_key(base_url, model, api_key)
OpenAiCompat::new(LlmConfig::new("my-name", base_url, api_key, model))
    .with_context_window(64_000)

// Anthropic Messages API: URL hardcoded
AnthropicNative::with_key(model, api_key)
AnthropicNative::with_key(model, api_key)
    .with_context_window(200_000)
    .with_api_version("2023-06-01")

// Mock for tests
MockModel::new()
    .with_name("test-mock")
    .script(MockResponse::text("hello"))
    .script(MockResponse::tool_call("read_file", json!({"path":"x"})))
    .script(MockResponse::text("done"))
```

`providers::` constants — pass to `OpenAiCompat::with_key`:
```rust
ANTHROPIC | OPENAI | DEEPSEEK | GROQ | TOGETHER | OLLAMA
```

## Sandbox — `harness-rs-sandbox`

```rust
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn spawn(&self, blueprint: &()) -> Result<SandboxHandle, SandboxError>;
    fn fs_policy(&self)  -> FsPolicy;       // describes isolation strength
    fn net_policy(&self) -> NetPolicy;
}

pub struct SandboxHandle {
    pub world:   World,
    pub keep:    bool,         // call .keep() to skip Drop cleanup
}

// Implementations:
WorktreeSandbox::new(repo_root, branch_name)
ContainerSandbox::new(image, source_root).with_network(false)
NullSandbox                                 // identity — for tests
```

## Blueprint — `harness-rs-blueprint`

```rust
let bp = Blueprint::new(sandbox)
    .add("read_status", Node::deterministic(|w| Box::pin(async move {
        w.runner.exec("git", &["status"], Some(w.repo.root.as_path())).await?;
        Ok(NodeOutput { transition: Transition::Next, data: json!({}) })
    })))
    .add("write_code", Node::Agent {
        guides:  vec![],
        tools:   tool_registry(),
        sensors: SensorBus::new(),
        budget:  AgentBudget { max_iters: 8, max_tokens: 50_000 },
    })
    .edge("read_status", "write_code")
    .branch_on_failure("write_code", FailurePolicy { retry_cap: 2, fallback: None });

let outcome = bp.run(task).await?;
```

## Session record + replay — `harness-rs-loop`

```rust
use harness_rs_loop::{SessionRecorder, read_session, replay_as_mock, SessionStats};

// record:
let rec = SessionRecorder::new("session.jsonl")?;
AgentLoop::new(model).with_hook(Arc::new(rec)).run(task, &mut world).await?;

// stats:
let events = read_session("session.jsonl")?;
let stats  = SessionStats::from(&events);
println!("{} iters, {} tool calls, {}/{} tokens",
    stats.iters, stats.tool_calls, stats.input_tokens, stats.output_tokens);

// deterministic replay (no network):
let mock = replay_as_mock(&events);
AgentLoop::new(mock).run(task, &mut world2).await?;   // reproduces original bit-for-bit
```

## MCP — `harness-rs-mcp`

```rust
use harness_rs_mcp::McpServer;

let server = McpServer::new("my-tools", "0.0.1")
    .with_tool(Arc::new(ReadFile))
    .with_tool(Arc::new(my_custom_tool));
server.serve_stdio(&mut world).await?;
```

Speaks JSON-RPC 2.0 over stdin/stdout. Methods: `initialize` · `ping` · `tools/list` · `tools/call`.

## CLI surface

```
harness new <name> --local            # scaffold a starter agent (auto [patch.crates-io])
harness new <name> --workspace <path> # explicit local-checkout path
harness skills validate <skill-dir>   # strict agentskills.io check
harness skills list <skills-root>     # list all skills under dir
harness skills lint <skills-root>     # description quality / duplicates
harness skills export <out> --from <src> # round-trip to spec-compliant <out>/<name>/SKILL.md
harness trace <session.jsonl>         # pretty per-event view + summary
harness trace <session.jsonl> --summary
harness mcp serve --workspace <path>  # JSON-RPC over stdio
```
