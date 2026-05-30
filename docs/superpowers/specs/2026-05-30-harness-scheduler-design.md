# harness-scheduler — In-Process Agent Scheduling + Delivery (Framework Capability)

**Status:** Approved (brainstorming) → ready for plan
**Date:** 2026-05-30
**Layer:** harness-rs framework (capability C of the Hermes-port roadmap; B=recall, A=learning-loop done)

## Goal

Let any harness-rs app run **scheduled agent jobs that deliver their output to a
channel** — the framework generalization of the dashboard's daily-digest cron.
An agent can also schedule jobs for itself via a `cronjob` tool. Concretely:

```rust
let sched = Scheduler::new(job_store, model)
    .with_tool(/* … */)
    .with_channel(Arc::new(StdoutChannel::new()))
    .with_channel(Arc::new(EmailChannel::from_env()));
sched.spawn(); // ticks; runs due jobs as agent turns; delivers output
```

A job is `{ schedule, prompt, channel }`: at its time, the scheduler runs the
prompt as an agent turn and sends the result to the channel.

## Decisions (from brainstorming)

| Question | Decision |
|---|---|
| Form | **NEW optional crate `harness-scheduler`** (in-process), NOT extending the subprocess-runner `harness-daemon` |
| Schedule parsing | **reuse `harness_daemon::Schedule`** (`parse`/`next_after`) — daily/weekly/interval; no new cron-expr parser |
| Job storage | `JobStore` trait + `FileJobStore` (JSON file) default |
| Delivery | `Channel` trait + built-in `StdoutChannel` + `EmailChannel` (Resend); Telegram reserved (app-provided) |
| Job execution | scheduler runs the job prompt as a **`Subagent` turn** (scheduler-level model + tools) |
| NL scheduling | the **agent supplies a schedule string** via the `cronjob` tool (validated by `Schedule::parse`) — NOT an LLM-parse step (matches Hermes) |
| Silent suppression | a `[SILENT]` job output suppresses delivery (Hermes pattern) |
| Per-job model/tools | deferred (scheduler-level) |

## Crate plan

New optional crate `crates/harness-scheduler` (`[package] name = harness-rs-scheduler`,
`[lib] name = harness_scheduler`), added to workspace members. Deps: harness-core,
harness-loop (AgentLoop/Subagent), harness-daemon (`Schedule`), reqwest (email),
tokio, chrono, serde, serde_json, async-trait, thiserror. Like harness-mcp/daemon,
nothing else in the framework depends on it.

| Component | File |
|---|---|
| `Job` + `JobStore` trait + `FileJobStore` | `src/store.rs` |
| `Channel` trait + `StdoutChannel` + `EmailChannel` + `ChannelRegistry` | `src/channel.rs` |
| `Scheduler` (tick loop + run + deliver) | `src/scheduler.rs` |
| `CronjobTool` | `src/tool.rs` |
| re-exports | `src/lib.rs` |

## `Job` + `JobStore` (src/store.rs)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub schedule: String,          // "daily 08:00" | "weekly mon 09:30" | "every 15m"
    pub prompt: String,            // run as the agent task
    pub channel: String,          // channel key, e.g. "stdout" | "email"
    #[serde(default)]
    pub target: Option<String>,    // channel-specific recipient (email addr, chat id…)
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run_ms: Option<i64>,
    #[serde(default)]
    pub next_run_ms: Option<i64>,
    pub created_ms: i64,
}

#[async_trait]
pub trait JobStore: Send + Sync + 'static {
    async fn add(&self, job: &Job) -> Result<(), JobError>;
    async fn list(&self) -> Result<Vec<Job>, JobError>;
    async fn get(&self, id: &str) -> Result<Option<Job>, JobError>;
    async fn remove(&self, id: &str) -> Result<bool, JobError>;
    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, JobError>;
    /// Record a completed run + its next fire time.
    async fn record_run(&self, id: &str, last_run_ms: i64, next_run_ms: Option<i64>) -> Result<(), JobError>;
}

#[derive(Debug, thiserror::Error)]
pub enum JobError { #[error("job io: {0}")] Io(String), #[error("job serde: {0}")] Serde(String) }
```

`FileJobStore::open(path)` — all jobs in one JSON file (array). Mutations
read-all → modify → atomic rewrite (sibling tmp + rename), like `FileMemory`.
Small N (jobs), so whole-file rewrite is fine. A `Mutex<()>` serializes writes.

## `Channel` (src/channel.rs)

```rust
#[async_trait]
pub trait Channel: Send + Sync {
    /// Stable key matched against `Job.channel`.
    fn key(&self) -> &str;
    /// Deliver `output` for `job`. `job.target` is the recipient if relevant.
    async fn send(&self, output: &str, job: &Job) -> Result<(), ChannelError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError { #[error("channel: {0}")] Send(String) }
```

- **`StdoutChannel`** (`key = "stdout"`): prints `\n=== {job.name} ===\n{output}\n`.
- **`EmailChannel`** (`key = "email"`): POST `api.resend.com/emails` via reqwest,
  faithful to the dashboard digest path. Constructed with `api_key` + `from`
  (or `EmailChannel::from_env()` reading `RESEND_API_KEY` + `DIGEST_FROM`).
  Recipient = `job.target` (required; if absent → `ChannelError`). Subject =
  `job.name`. Body = the agent output as text. Non-2xx → `ChannelError`. The
  request body is built by a pure `resend_body(from,to,subject,text)` fn (unit
  tested without network).
- **`ChannelRegistry`**: `HashMap<String, Arc<dyn Channel>>`, `register` + `get`.
  Telegram is reserved — apps implement `Channel` and `register` their own.

## `Scheduler` (src/scheduler.rs)

```rust
pub struct Scheduler {
    store: Arc<dyn JobStore>,
    model: Arc<dyn Model>,
    tools: Vec<Arc<dyn Tool>>,
    channels: ChannelRegistry,
    repo_root: PathBuf,   // for the job's World
    tick: Duration,       // default 60s
    max_iters: u32,       // default 20
}
impl Scheduler {
    pub fn new(store: Arc<dyn JobStore>, model: Arc<dyn Model>) -> Self;
    pub fn with_tool(self, t: Arc<dyn Tool>) -> Self;
    pub fn with_channel(self, c: Arc<dyn Channel>) -> Self;   // registers by c.key()
    pub fn with_tick(self, d: Duration) -> Self;
    pub fn with_max_iters(self, n: u32) -> Self;
    pub fn with_repo_root(self, p: impl Into<PathBuf>) -> Self;
    /// Spawn the tick loop (tokio task). Runs forever.
    pub fn spawn(self);
    /// Run all currently-due jobs once; returns how many fired. (Testable.)
    pub async fn tick_once(&self) -> usize;
}
```

`tick_once`:
- `now = chrono::Local::now()`, `now_ms = now.timestamp_millis()`.
- For each `job` in `store.list()` where `enabled` AND
  (`next_run_ms.is_none()` OR `next_run_ms <= now_ms`):
  - Build `World` (`default_world(repo_root)`), a `SubagentSpec::new(&job.name, Task{description: job.prompt, …})` `.with_max_iters(max_iters)` + each scheduler tool.
  - `Subagent::new(model.clone(), spec)`; `Box::pin(sub.run(&mut world)).await`
    (Box::pin: same async-recursion guard as the learning loop). Output =
    `report.text.unwrap_or_default()`.
  - If `output.trim() == "[SILENT]"` or empty → skip delivery.
  - Else `channels.get(&job.channel)`:
    - present → `.send(&output, &job).await` (warn on `Err`).
    - absent → `tracing::warn!` (unknown channel).
  - `next = Schedule::parse(&job.schedule).ok().map(|s| s.next_after(now).timestamp_millis())`;
    `store.record_run(&job.id, now_ms, next).await` (warn on err).
  - **Best-effort per job:** each job runs in its own guarded block; one job's
    failure (model error, channel error, store error) logs a warning and the
    loop continues to the next job. A job failure NEVER stops the scheduler.
- Return the count of jobs that fired.

`spawn` = `tokio::spawn(async move { loop { self.tick_once().await; sleep(tick).await; } })`.

## `CronjobTool` (src/tool.rs)

Holds `Arc<dyn JobStore>`; lets the agent manage its own schedule. `name =
"cronjob"`, `risk = Destructive`. Schema:
```json
{ "action": "create|list|remove|pause|resume",
  "name": "...", "schedule": "daily 08:00", "prompt": "...",
  "channel": "stdout", "target": "x@y.com", "id": "..." }
```
- `create`: validate `Schedule::parse(schedule)` (bad → `ok:false`); generate
  `id`; `next_run_ms = parse.next_after(now)`; `created_ms = now`; default
  `channel="stdout"`, `enabled=true`; `store.add`. Returns the job id.
- `list`: `store.list` → `{jobs}`.
- `remove`: `store.remove(id)`.
- `pause`/`resume`: `store.set_enabled(id, false|true)`.
The agent supplies the schedule STRING — there is no LLM parsing step; the model
chose the string when it decided to call the tool.

## Error handling

| Situation | Behavior |
|---|---|
| One job errors (model/channel/store) | `tracing::warn!`, continue other jobs; scheduler never stops |
| Unknown `job.channel` | warn, no delivery, still `record_run` (so it doesn't hot-loop) |
| `[SILENT]` / empty output | no delivery (still `record_run`) |
| EmailChannel: no `target` / non-2xx / no key | `ChannelError` → job's delivery warns; run still recorded |
| Bad `schedule` in `cronjob create` | `ok:false` (the agent sees it, can retry) |
| Bad `schedule` at run time | `next_run = None` → job won't auto-refire; warn |

## Testing

- **FileJobStore**: add/list/get/remove/set_enabled/record_run round-trip; rewrite atomicity (no corruption on a malformed line is N/A for a single JSON array — assert add+remove leaves a valid file).
- **EmailChannel**: `resend_body(from,to,subject,text)` shape (no network); `send` with no `target` → `ChannelError`.
- **Scheduler.tick_once** (the headline): a `CapturingChannel` (test impl recording `send` calls) + a mock model returning `"hi"` + one job due now → `tick_once()==1`, the channel received `"hi"`, and the job's `next_run_ms` advanced past `now`. Variants: a `[SILENT]`-returning model → no delivery; a job whose `next_run_ms` is in the future → not fired (`tick_once()==0`); a job failing (model error) does not prevent a second due job from firing.
- **CronjobTool**: `create` with a valid schedule writes a job with a future `next_run_ms`; `create` with a bad schedule → `ok:false`; `list`/`remove`/`pause`/`resume`.
- **No-recursion / best-effort**: the job Subagent has no scheduler attached; a job that errors is isolated.

## Out of scope (v1)

- Per-job model/tool overrides (scheduler-level only).
- Telegram / other inbound-channel adapters (the `Channel` trait reserves the
  shape; apps implement their own).
- 5-field cron expressions (only `Schedule`'s daily/weekly/interval).
- Concurrency limits / parallel job execution (jobs run sequentially per tick).
- Catch-up semantics for missed ticks beyond "fire as soon as `next_run <= now`".

## Dogfood / reference consumer

The dashboard's bespoke digest cron can be reframed as a `harness-scheduler` job
(`schedule="daily HH:MM"`, `prompt=` the digest prompt, `channel="email"`,
`target=` the user's email) — proving the generalization. (Migration optional;
the existing digest keeps working.)
