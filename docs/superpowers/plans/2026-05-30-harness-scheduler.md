# harness-scheduler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A new optional crate `harness-scheduler` that runs scheduled agent jobs in-process and delivers their output to a channel — the framework generalization of the dashboard digest cron.

**Architecture:** A `Scheduler` (tokio tick loop) reads due jobs from a `JobStore`, runs each job's prompt as a `Subagent` turn on an app-provided model + toolset, and delivers the output via a `Channel`. An agent self-schedules through a `cronjob` tool. Schedule strings are parsed by the existing `harness_daemon::Schedule` (daily/weekly/interval).

**Tech Stack:** Rust, async-trait, serde/serde_json, chrono, reqwest (email), tokio; reuses harness-core/loop/daemon. Optional crate (like harness-mcp/daemon).

**Spec:** `docs/superpowers/specs/2026-05-30-harness-scheduler-design.md`

**Conventions (verified):**
- Crate `[package] name = harness-rs-scheduler`, dir `crates/harness-scheduler`, `[lib] name = harness_scheduler`. Deps via workspace alias.
- `harness_daemon::Schedule` (pub): `Schedule::parse(&str) -> Result<Schedule, ScheduleError>`; `schedule.next_after(now: chrono::DateTime<chrono::Local>) -> chrono::DateTime<chrono::Local>`. Accepts `"daily 08:00"`, `"weekly mon 09:30"`, `"every 15m"`.
- `harness_loop::{Subagent, SubagentSpec}`: `SubagentSpec::new(name: impl Into<String>, task: Task).with_tool(Arc<dyn Tool>).with_max_iters(u32)`; `Subagent::new(model: M, spec).run(&mut World) -> Result<SubagentReport, HarnessError>` where `M: Model`. `SubagentReport { name, status, text: Option<String>, iters }`. `Arc<dyn Model>: Model` (blanket impl exists), so `Subagent::new(Arc<dyn Model>, spec)` compiles.
- `harness_context::default_world(repo_root) -> World`.
- `harness_core::{Task{description,source,deadline}, Tool, ToolError, ToolResult{ok,content,trace}, ToolRisk, ToolSchema, World, Model}`. `ToolError::InvalidArgs{name,reason}`.
- File-store pattern: read-all → modify → atomic write (sibling `.tmp` + rename), `Mutex<()>` for write serialization — see `crates/harness-context/src/memory_file.rs`.
- Resend email: POST `https://api.resend.com/emails` with `Authorization: Bearer {key}`, JSON `{from, to:[..], subject, text}` — see `examples/dashboard/src/digest`/`deliver` for the precedent.
- Run tests: `cargo test -p harness-rs-scheduler <filter>`. NO Co-Authored-By / AI attribution in commits.

---

### Task 1: crate scaffold + `Job` + `JobStore` + `FileJobStore`

**Files:** Create `crates/harness-scheduler/Cargo.toml`, `src/lib.rs`, `src/store.rs`; modify root `Cargo.toml`.

- [ ] **Step 1: Manifest**

Create `crates/harness-scheduler/Cargo.toml`:

```toml
[package]
name = "harness-rs-scheduler"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
description = "In-process agent scheduler for harness-rs: run scheduled agent jobs and deliver their output to a Channel (stdout / email). Optional."

[lib]
name = "harness_scheduler"

[dependencies]
harness-core   = { workspace = true }
harness-loop   = { workspace = true }
harness-daemon = { workspace = true }
async-trait    = { workspace = true }
serde          = { workspace = true }
serde_json     = { workspace = true }
chrono         = { workspace = true }
reqwest        = { workspace = true }
tokio          = { workspace = true }
thiserror      = { workspace = true }
tracing        = { workspace = true }

[dev-dependencies]
harness-context = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

Check a sibling (`crates/harness-daemon/Cargo.toml`) for exact inheritance keys; copy `[package]` shape if different.

In the root `Cargo.toml`:
1. add `"crates/harness-scheduler",` to `[workspace] members`;
2. **`harness-daemon` is NOT currently in `[workspace.dependencies]`** (verified) — add it so the new crate can use `harness-daemon = { workspace = true }`, copying the exact form of the existing `harness-loop` entry:
   `harness-daemon = { package = "harness-rs-daemon", path = "crates/harness-daemon", version = "0.0.4" }` (match the current crate version — `0.0.4`);
3. add a `[workspace.dependencies]` entry for the new crate itself (for future consumers):
   `harness-scheduler = { package = "harness-rs-scheduler", path = "crates/harness-scheduler", version = "0.0.4" }`.

The other deps (harness-core, harness-context, harness-loop, async-trait, serde, serde_json, chrono, reqwest, tokio, thiserror, tracing) are already in `[workspace.dependencies]` — reference them via `{ workspace = true }`.

- [ ] **Step 2: `Job` + `JobStore` trait + `FileJobStore`**

Create `crates/harness-scheduler/src/store.rs`:

```rust
//! Job persistence. `Job` is the unit of scheduled work; `JobStore` is the
//! pluggable backend (default `FileJobStore`, a single JSON-array file written
//! atomically — same posture as `harness_context::FileMemory`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

fn default_true() -> bool { true }

/// One scheduled agent job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Job {
    pub id: String,
    pub name: String,
    /// "daily 08:00" | "weekly mon 09:30" | "every 15m" (parsed by harness_daemon::Schedule).
    pub schedule: String,
    /// The agent task to run at fire time.
    pub prompt: String,
    /// Channel key (matched against a registered Channel), e.g. "stdout" | "email".
    pub channel: String,
    /// Channel-specific recipient (email address, chat id, …).
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run_ms: Option<i64>,
    #[serde(default)]
    pub next_run_ms: Option<i64>,
    pub created_ms: i64,
}

impl Job {
    pub fn new(name: impl Into<String>, schedule: impl Into<String>, prompt: impl Into<String>, channel: impl Into<String>, created_ms: i64) -> Self {
        Self {
            id: gen_id(created_ms),
            name: name.into(),
            schedule: schedule.into(),
            prompt: prompt.into(),
            channel: channel.into(),
            target: None,
            enabled: true,
            last_run_ms: None,
            next_run_ms: None,
            created_ms,
        }
    }
    pub fn with_target(mut self, t: Option<String>) -> Self { self.target = t; self }
    pub fn with_next_run(mut self, ms: Option<i64>) -> Self { self.next_run_ms = ms; self }
}

/// Short-ish unique id: created-ms + a process-global counter.
fn gen_id(created_ms: i64) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("job-{created_ms}-{}", SEQ.fetch_add(1, Ordering::SeqCst))
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JobError {
    #[error("job io: {0}")] Io(String),
    #[error("job serde: {0}")] Serde(String),
}

#[async_trait]
pub trait JobStore: Send + Sync + 'static {
    async fn add(&self, job: &Job) -> Result<(), JobError>;
    async fn list(&self) -> Result<Vec<Job>, JobError>;
    async fn get(&self, id: &str) -> Result<Option<Job>, JobError>;
    async fn remove(&self, id: &str) -> Result<bool, JobError>;
    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, JobError>;
    async fn record_run(&self, id: &str, last_run_ms: i64, next_run_ms: Option<i64>) -> Result<(), JobError>;
}

/// JSON-array file backend. All jobs in one file; mutations read-all → modify →
/// atomic rewrite. `Mutex<()>` serializes writers.
pub struct FileJobStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl FileJobStore {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, JobError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| JobError::Io(e.to_string()))?;
            }
        }
        Ok(Self { path, write_lock: Mutex::new(()) })
    }

    fn read_all(&self) -> Result<Vec<Job>, JobError> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).map_err(|e| JobError::Serde(e.to_string())),
            _ => Ok(Vec::new()),
        }
    }

    fn write_all(&self, jobs: &[Job]) -> Result<(), JobError> {
        let _g = self.write_lock.lock().map_err(|e| JobError::Io(e.to_string()))?;
        let buf = serde_json::to_string_pretty(jobs).map_err(|e| JobError::Serde(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, buf).map_err(|e| JobError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| JobError::Io(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl JobStore for FileJobStore {
    async fn add(&self, job: &Job) -> Result<(), JobError> {
        let mut jobs = self.read_all()?;
        jobs.push(job.clone());
        self.write_all(&jobs)
    }
    async fn list(&self) -> Result<Vec<Job>, JobError> { self.read_all() }
    async fn get(&self, id: &str) -> Result<Option<Job>, JobError> {
        Ok(self.read_all()?.into_iter().find(|j| j.id == id))
    }
    async fn remove(&self, id: &str) -> Result<bool, JobError> {
        let mut jobs = self.read_all()?;
        let before = jobs.len();
        jobs.retain(|j| j.id != id);
        let removed = jobs.len() != before;
        if removed { self.write_all(&jobs)?; }
        Ok(removed)
    }
    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<bool, JobError> {
        let mut jobs = self.read_all()?;
        let mut found = false;
        for j in &mut jobs { if j.id == id { j.enabled = enabled; found = true; } }
        if found { self.write_all(&jobs)?; }
        Ok(found)
    }
    async fn record_run(&self, id: &str, last_run_ms: i64, next_run_ms: Option<i64>) -> Result<(), JobError> {
        let mut jobs = self.read_all()?;
        for j in &mut jobs { if j.id == id { j.last_run_ms = Some(last_run_ms); j.next_run_ms = next_run_ms; } }
        self.write_all(&jobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("harness-jobstore-{}-{n}.json", std::process::id()))
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let p = tmp();
        let store = FileJobStore::open(&p).unwrap();
        let job = Job::new("digest", "daily 08:00", "summarize", "stdout", 1000).with_next_run(Some(2000));
        let id = job.id.clone();
        store.add(&job).await.unwrap();

        assert_eq!(store.list().await.unwrap().len(), 1);
        assert_eq!(store.get(&id).await.unwrap().unwrap().name, "digest");

        assert!(store.set_enabled(&id, false).await.unwrap());
        assert!(!store.get(&id).await.unwrap().unwrap().enabled);

        store.record_run(&id, 5000, Some(9000)).await.unwrap();
        let j = store.get(&id).await.unwrap().unwrap();
        assert_eq!(j.last_run_ms, Some(5000));
        assert_eq!(j.next_run_ms, Some(9000));

        assert!(store.remove(&id).await.unwrap());
        assert!(store.list().await.unwrap().is_empty());
        // file is still valid JSON (empty array) after remove
        assert!(!store.remove(&id).await.unwrap());

        let _ = std::fs::remove_file(&p);
    }
}
```

- [ ] **Step 3: lib.rs**

Create `crates/harness-scheduler/src/lib.rs`:

```rust
//! In-process agent scheduling + delivery for harness-rs. See the crate README
//! / design spec. Optional — nothing else in the framework depends on it.

pub mod channel;
pub mod scheduler;
pub mod store;
pub mod tool;

pub use channel::*;
pub use scheduler::*;
pub use store::*;
pub use tool::*;
```

(The `channel`/`scheduler`/`tool` modules are created in later tasks. To make Task 1 compile on its own, create those three files now as one-line stubs `//! (filled in a later task)` — Task 2/3/4 replace them.)

- [ ] **Step 4: Build + test + commit**

Run: `cargo test -p harness-rs-scheduler crud_roundtrip` and `cargo build -p harness-rs-scheduler`
Expected: `crud_roundtrip` PASS; clean build.

```bash
git add crates/harness-scheduler Cargo.toml
git commit -m "feat(harness-scheduler): crate scaffold + Job + JobStore + FileJobStore"
```

---

### Task 2: `Channel` trait + `StdoutChannel` + `EmailChannel` + `ChannelRegistry`

**Files:** Replace `crates/harness-scheduler/src/channel.rs`.

- [ ] **Step 1: Write the module (pure resend_body first for TDD)**

Replace `crates/harness-scheduler/src/channel.rs`:

```rust
//! Delivery channels. A `Channel` takes a job's agent output and sends it
//! somewhere. Built-ins: `StdoutChannel`, `EmailChannel` (Resend). Apps add
//! their own (Telegram, Slack, …) by implementing `Channel`.

use crate::store::Job;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ChannelError {
    #[error("channel send: {0}")] Send(String),
}

#[async_trait]
pub trait Channel: Send + Sync {
    /// Stable key matched against `Job.channel`.
    fn key(&self) -> &str;
    /// Deliver `output` for `job`. `job.target` is the recipient if relevant.
    async fn send(&self, output: &str, job: &Job) -> Result<(), ChannelError>;
}

/// Prints the output to stdout.
pub struct StdoutChannel;
impl StdoutChannel { pub fn new() -> Self { Self } }
impl Default for StdoutChannel { fn default() -> Self { Self::new() } }

#[async_trait]
impl Channel for StdoutChannel {
    fn key(&self) -> &str { "stdout" }
    async fn send(&self, output: &str, job: &Job) -> Result<(), ChannelError> {
        println!("\n=== {} ===\n{}\n", job.name, output);
        Ok(())
    }
}

/// Sends the output as an email via Resend. Recipient = `job.target`.
pub struct EmailChannel {
    api_key: String,
    from: String,
    client: reqwest::Client,
}

/// Build the Resend POST body. Pure — unit-tested without network.
pub fn resend_body(from: &str, to: &str, subject: &str, text: &str) -> serde_json::Value {
    serde_json::json!({ "from": from, "to": [to], "subject": subject, "text": text })
}

impl EmailChannel {
    pub fn new(api_key: impl Into<String>, from: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), from: from.into(), client: reqwest::Client::new() }
    }
    /// Construct from env: `RESEND_API_KEY` + `DIGEST_FROM` (falls back to a
    /// Resend test sender). Returns None if `RESEND_API_KEY` is absent.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("RESEND_API_KEY").ok().filter(|k| !k.is_empty())?;
        let from = std::env::var("DIGEST_FROM").unwrap_or_else(|_| "Scheduler <onboarding@resend.dev>".into());
        Some(Self::new(key, from))
    }
}

#[async_trait]
impl Channel for EmailChannel {
    fn key(&self) -> &str { "email" }
    async fn send(&self, output: &str, job: &Job) -> Result<(), ChannelError> {
        let to = job.target.as_deref().ok_or_else(|| ChannelError::Send("email job has no target recipient".into()))?;
        let body = resend_body(&self.from, to, &job.name, output);
        let resp = self.client
            .post("https://api.resend.com/emails")
            .bearer_auth(&self.api_key)
            .json(&body)
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| ChannelError::Send(format!("resend post: {e}")))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let code = resp.status();
            let t = resp.text().await.unwrap_or_default();
            Err(ChannelError::Send(format!("resend {code}: {t}")))
        }
    }
}

/// Maps channel keys to implementations.
#[derive(Default, Clone)]
pub struct ChannelRegistry {
    map: HashMap<String, Arc<dyn Channel>>,
}

impl ChannelRegistry {
    pub fn new() -> Self { Self::default() }
    pub fn register(&mut self, c: Arc<dyn Channel>) {
        self.map.insert(c.key().to_string(), c);
    }
    pub fn get(&self, key: &str) -> Option<&Arc<dyn Channel>> { self.map.get(key) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resend_body_shape() {
        let b = resend_body("Me <m@x.com>", "u@y.com", "Daily", "hello");
        assert_eq!(b["from"], "Me <m@x.com>");
        assert_eq!(b["to"][0], "u@y.com");
        assert_eq!(b["subject"], "Daily");
        assert_eq!(b["text"], "hello");
    }

    #[tokio::test]
    async fn email_without_target_errors() {
        let ch = EmailChannel::new("re_x", "Me <m@x.com>");
        let job = Job::new("j", "daily 08:00", "p", "email", 1);
        let err = ch.send("out", &job).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn registry_lookup() {
        let mut reg = ChannelRegistry::new();
        reg.register(Arc::new(StdoutChannel::new()));
        assert!(reg.get("stdout").is_some());
        assert!(reg.get("email").is_none());
    }
}
```

- [ ] **Step 2: Build + test + commit**

Run: `cargo test -p harness-rs-scheduler channel` (and `resend_body_shape`, `email_without_target_errors`, `registry_lookup`) + `cargo build -p harness-rs-scheduler`
Expected: 3 tests PASS.

```bash
git add crates/harness-scheduler/src/channel.rs
git commit -m "feat(harness-scheduler): Channel trait + StdoutChannel + EmailChannel(Resend) + registry"
```

---

### Task 3: `Scheduler` (tick + run job as agent turn + deliver)

**Files:** Replace `crates/harness-scheduler/src/scheduler.rs`.

- [ ] **Step 1: Write the scheduler**

Replace `crates/harness-scheduler/src/scheduler.rs`:

```rust
//! The in-process scheduler. Ticks on an interval; for each due job, runs the
//! job's prompt as a Subagent turn, then delivers the output via the job's
//! channel. Best-effort per job — one failure never stops the scheduler.

use crate::channel::ChannelRegistry;
use crate::store::JobStore;
use harness_core::{Model, Task, Tool};
use harness_daemon::Schedule;
use harness_loop::{Subagent, SubagentSpec};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct Scheduler {
    store: Arc<dyn JobStore>,
    model: Arc<dyn Model>,
    tools: Vec<Arc<dyn Tool>>,
    channels: ChannelRegistry,
    repo_root: PathBuf,
    tick: Duration,
    max_iters: u32,
}

impl Scheduler {
    pub fn new(store: Arc<dyn JobStore>, model: Arc<dyn Model>) -> Self {
        Self {
            store,
            model,
            tools: Vec::new(),
            channels: ChannelRegistry::new(),
            repo_root: PathBuf::from("."),
            tick: Duration::from_secs(60),
            max_iters: 20,
        }
    }
    pub fn with_tool(mut self, t: Arc<dyn Tool>) -> Self { self.tools.push(t); self }
    pub fn with_channel(mut self, c: Arc<dyn crate::channel::Channel>) -> Self { self.channels.register(c); self }
    pub fn with_tick(mut self, d: Duration) -> Self { self.tick = d; self }
    pub fn with_max_iters(mut self, n: u32) -> Self { self.max_iters = n; self }
    pub fn with_repo_root(mut self, p: impl Into<PathBuf>) -> Self { self.repo_root = p.into(); self }

    /// Spawn the tick loop. Runs forever.
    pub fn spawn(self) {
        tokio::spawn(async move {
            loop {
                let _ = self.tick_once().await;
                tokio::time::sleep(self.tick).await;
            }
        });
    }

    /// Run every currently-due job once. Returns how many fired.
    pub async fn tick_once(&self) -> usize {
        let now = chrono::Local::now();
        let now_ms = now.timestamp_millis();
        let jobs = match self.store.list().await {
            Ok(j) => j,
            Err(e) => { tracing::warn!(error = %e, "scheduler: list jobs failed"); return 0; }
        };
        let mut fired = 0;
        for job in jobs {
            if !job.enabled { continue; }
            let due = job.next_run_ms.map(|t| t <= now_ms).unwrap_or(true);
            if !due { continue; }
            fired += 1;

            // Run the job prompt as a Subagent turn (best-effort).
            let mut world = harness_context_default_world(&self.repo_root);
            let mut spec = SubagentSpec::new(
                job.name.clone(),
                Task { description: job.prompt.clone(), source: None, deadline: None },
            ).with_max_iters(self.max_iters);
            for t in &self.tools { spec = spec.with_tool(t.clone()); }
            let sub = Subagent::new(self.model.clone(), spec);
            let output = match sub.run(&mut world).await {
                Ok(report) => report.text.unwrap_or_default(),
                Err(e) => { tracing::warn!(job = %job.name, error = %e, "scheduler: job run failed"); String::new() }
            };

            // Deliver unless [SILENT] / empty.
            let trimmed = output.trim();
            if !trimmed.is_empty() && trimmed != "[SILENT]" {
                match self.channels.get(&job.channel) {
                    Some(ch) => { if let Err(e) = ch.send(&output, &job).await { tracing::warn!(job = %job.name, error = %e, "scheduler: delivery failed"); } }
                    None => tracing::warn!(job = %job.name, channel = %job.channel, "scheduler: unknown channel"),
                }
            }

            // Advance next_run.
            let next = Schedule::parse(&job.schedule).ok().map(|s| s.next_after(now).timestamp_millis());
            if let Err(e) = self.store.record_run(&job.id, now_ms, next).await {
                tracing::warn!(job = %job.name, error = %e, "scheduler: record_run failed");
            }
        }
        fired
    }
}

/// Tiny indirection so harness-context is only a *dev*-dep... actually we need a
/// World at runtime. harness-context IS a normal dep of harness-loop already and
/// re-exported? It is NOT — so construct the World here. harness-scheduler must
/// depend on harness-context (normal dep) for `default_world`. Add it.
fn harness_context_default_world(root: &std::path::Path) -> harness_core::World {
    harness_context::default_world(root.to_path_buf())
}
```

> NOTE: `default_world` lives in `harness-context`. Add `harness-context = { workspace = true }` to harness-scheduler's NORMAL `[dependencies]` (it's already in dev-deps from Task 1 — move/duplicate it to normal deps). Then replace the `harness_context_default_world` helper body with a direct `harness_context::default_world(self.repo_root.clone())` call inline if you prefer; the helper is just to keep the call in one place. Ensure `harness_context` resolves.

- [ ] **Step 2: Test with a mock model + capturing channel**

Append to `crates/harness-scheduler/src/scheduler.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{Channel, ChannelError};
    use crate::store::{FileJobStore, Job, JobStore};
    use async_trait::async_trait;
    use harness_core::{Context, ModelError, ModelInfo, ModelOutput, StopReason, Usage};
    use std::sync::Mutex as StdMutex;

    fn mi() -> ModelInfo {
        ModelInfo { handle: "mock".into(), provider: "mock".into(), model: "mock".into(), context_window: 8192, input_cost_usd_per_million_tokens: None, output_cost_usd_per_million_tokens: None, supports_tool_use: false, supports_streaming: false }
    }

    struct SayModel { text: String }
    #[async_trait]
    impl Model for SayModel {
        async fn complete(&self, _c: &Context) -> Result<ModelOutput, ModelError> {
            Ok(ModelOutput { text: Some(self.text.clone()), tool_calls: vec![], usage: Usage::default(), stop_reason: StopReason::EndTurn, reasoning: None })
        }
        fn info(&self) -> ModelInfo { mi() }
    }

    #[derive(Default)]
    struct CapturingChannel { sent: Arc<StdMutex<Vec<String>>> }
    #[async_trait]
    impl Channel for CapturingChannel {
        fn key(&self) -> &str { "cap" }
        async fn send(&self, output: &str, _job: &Job) -> Result<(), ChannelError> {
            self.sent.lock().unwrap().push(output.to_string());
            Ok(())
        }
    }

    fn tmp() -> std::path::PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("harness-sched-{}-{n}.json", std::process::id()))
    }

    #[tokio::test]
    async fn due_job_runs_and_delivers_and_advances() {
        let p = tmp();
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(&p).unwrap());
        // Due now: next_run_ms in the past.
        let job = Job::new("daily-brief", "daily 08:00", "write the brief", "cap", 1).with_next_run(Some(0));
        store.add(&job).await.unwrap();

        let captured = Arc::new(StdMutex::new(Vec::new()));
        let cap = CapturingChannel { sent: captured.clone() };
        let model: Arc<dyn Model> = Arc::new(SayModel { text: "the brief".into() });
        let sched = Scheduler::new(store.clone(), model)
            .with_channel(Arc::new(cap))
            .with_repo_root(".");

        let fired = sched.tick_once().await;
        assert_eq!(fired, 1);
        assert_eq!(captured.lock().unwrap().as_slice(), &["the brief".to_string()]);
        // next_run advanced into the future
        let j = store.get(&job.id).await.unwrap().unwrap();
        assert!(j.next_run_ms.unwrap() > chrono::Local::now().timestamp_millis() - 1000);
        assert!(j.last_run_ms.is_some());
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn silent_output_is_not_delivered() {
        let p = tmp();
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(&p).unwrap());
        store.add(&Job::new("j", "daily 08:00", "p", "cap", 1).with_next_run(Some(0))).await.unwrap();
        let captured = Arc::new(StdMutex::new(Vec::new()));
        let sched = Scheduler::new(store, Arc::new(SayModel { text: "[SILENT]".into() }))
            .with_channel(Arc::new(CapturingChannel { sent: captured.clone() }));
        sched.tick_once().await;
        assert!(captured.lock().unwrap().is_empty(), "[SILENT] suppresses delivery");
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn future_job_does_not_fire() {
        let p = tmp();
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(&p).unwrap());
        let future = chrono::Local::now().timestamp_millis() + 10_000_000;
        store.add(&Job::new("j", "daily 08:00", "p", "cap", 1).with_next_run(Some(future))).await.unwrap();
        let sched = Scheduler::new(store, Arc::new(SayModel { text: "x".into() }))
            .with_channel(Arc::new(CapturingChannel::default()));
        assert_eq!(sched.tick_once().await, 0);
        let _ = std::fs::remove_file(&p);
    }
}
```

- [ ] **Step 3: Build + test + commit**

Run: `cargo test -p harness-rs-scheduler scheduler` then `cargo test -p harness-rs-scheduler`
Expected: the 3 scheduler tests PASS (+ Task 1/2 tests).

```bash
git add crates/harness-scheduler/src/scheduler.rs crates/harness-scheduler/Cargo.toml
git commit -m "feat(harness-scheduler): Scheduler — tick, run job as Subagent turn, deliver, advance"
```

---

### Task 4: `CronjobTool` (agent self-scheduling)

**Files:** Replace `crates/harness-scheduler/src/tool.rs`.

- [ ] **Step 1: Write the tool**

Replace `crates/harness-scheduler/src/tool.rs`:

```rust
//! `cronjob` — lets an agent schedule its own recurring jobs. The agent supplies
//! the schedule STRING (validated by `harness_daemon::Schedule::parse`); there is
//! no LLM-parsing step.

use crate::store::{Job, JobStore};
use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use harness_daemon::Schedule;
use serde_json::{json, Value};
use std::sync::Arc;

pub struct CronjobTool {
    store: Arc<dyn JobStore>,
    schema: ToolSchema,
}

impl CronjobTool {
    pub fn new(store: Arc<dyn JobStore>) -> Self {
        Self {
            store,
            schema: ToolSchema {
                name: "cronjob".into(),
                description: "Schedule recurring agent jobs. actions: create (name, \
                    schedule, prompt, channel, target?), list, remove (id), pause (id), \
                    resume (id). schedule is a string: 'daily 08:00', 'weekly mon 09:30', \
                    or 'every 15m'. When the job fires, your `prompt` runs as a task and \
                    the result is delivered to `channel`."
                    .into(),
                input: json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["create", "list", "remove", "pause", "resume"]},
                        "name": {"type": "string"},
                        "schedule": {"type": "string", "description": "'daily 08:00' | 'weekly mon 09:30' | 'every 15m'"},
                        "prompt": {"type": "string", "description": "task to run at fire time"},
                        "channel": {"type": "string", "description": "delivery channel key (default 'stdout')"},
                        "target": {"type": "string", "description": "channel recipient (email address, etc.)"},
                        "id": {"type": "string", "description": "job id for remove/pause/resume"}
                    },
                    "required": ["action"]
                }),
            },
        }
    }

    fn s<'a>(a: &'a Value, k: &str) -> Option<&'a str> { a.get(k).and_then(|v| v.as_str()) }
}

#[async_trait]
impl Tool for CronjobTool {
    fn name(&self) -> &str { &self.schema.name }
    fn schema(&self) -> &ToolSchema { &self.schema }
    fn risk(&self) -> ToolRisk { ToolRisk::Destructive }

    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let action = Self::s(&args, "action").ok_or_else(|| ToolError::InvalidArgs { name: "cronjob".into(), reason: "action required".into() })?;
        let res: Result<Value, String> = match action {
            "create" => {
                let name = Self::s(&args, "name").unwrap_or("job");
                let schedule = Self::s(&args, "schedule").ok_or_else(|| ToolError::InvalidArgs { name: "cronjob".into(), reason: "schedule required".into() })?;
                let prompt = Self::s(&args, "prompt").ok_or_else(|| ToolError::InvalidArgs { name: "cronjob".into(), reason: "prompt required".into() })?;
                let channel = Self::s(&args, "channel").unwrap_or("stdout");
                let target = Self::s(&args, "target").map(|s| s.to_string());
                match Schedule::parse(schedule) {
                    Ok(sched) => {
                        let now = chrono::Local::now();
                        let next = sched.next_after(now).timestamp_millis();
                        let job = Job::new(name, schedule, prompt, channel, now.timestamp_millis())
                            .with_target(target)
                            .with_next_run(Some(next));
                        let id = job.id.clone();
                        self.store.add(&job).await.map_err(|e| e.to_string())
                            .map(|_| json!({"created": id, "next_run_ms": next}))
                    }
                    Err(e) => Err(format!("bad schedule `{schedule}`: {e}")),
                }
            }
            "list" => self.store.list().await.map(|jobs| json!({"jobs": jobs})).map_err(|e| e.to_string()),
            "remove" => {
                let id = Self::s(&args, "id").ok_or_else(|| ToolError::InvalidArgs { name: "cronjob".into(), reason: "id required".into() })?;
                self.store.remove(id).await.map(|r| json!({"removed": r})).map_err(|e| e.to_string())
            }
            "pause" | "resume" => {
                let id = Self::s(&args, "id").ok_or_else(|| ToolError::InvalidArgs { name: "cronjob".into(), reason: "id required".into() })?;
                let on = action == "resume";
                self.store.set_enabled(id, on).await.map(|r| json!({"updated": r, "enabled": on})).map_err(|e| e.to_string())
            }
            other => Err(format!("unknown action `{other}`")),
        };
        match res {
            Ok(content) => Ok(ToolResult { ok: true, content, trace: None }),
            Err(reason) => Ok(ToolResult { ok: false, content: json!({"error": reason}), trace: None }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::FileJobStore;
    use harness_context::default_world;

    fn tmp() -> std::path::PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("harness-cronjob-{}-{n}.json", std::process::id()))
    }

    #[tokio::test]
    async fn create_validates_and_lists_and_removes() {
        let p = tmp();
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(&p).unwrap());
        let tool = CronjobTool::new(store.clone());
        let mut w = default_world(".");

        // bad schedule → ok:false
        let out = tool.invoke(json!({"action":"create","name":"x","schedule":"nonsense","prompt":"p"}), &mut w).await.unwrap();
        assert!(!out.ok, "bad schedule must be rejected");

        // good schedule → created with future next_run
        let out = tool.invoke(json!({"action":"create","name":"brief","schedule":"daily 08:00","prompt":"write brief","channel":"stdout"}), &mut w).await.unwrap();
        assert!(out.ok);
        let id = out.content["created"].as_str().unwrap().to_string();
        assert!(out.content["next_run_ms"].as_i64().unwrap() > 0);

        // list
        let out = tool.invoke(json!({"action":"list"}), &mut w).await.unwrap();
        assert_eq!(out.content["jobs"].as_array().unwrap().len(), 1);

        // pause
        let out = tool.invoke(json!({"action":"pause","id": id}), &mut w).await.unwrap();
        assert!(out.ok);
        assert!(!store.get(&id).await.unwrap().unwrap().enabled);

        // remove
        let out = tool.invoke(json!({"action":"remove","id": id}), &mut w).await.unwrap();
        assert!(out.ok);
        assert!(store.list().await.unwrap().is_empty());

        let _ = std::fs::remove_file(&p);
    }
}
```

- [ ] **Step 2: Build + test + commit**

Run: `cargo test -p harness-rs-scheduler tool` then `cargo test -p harness-rs-scheduler` (whole crate).
Expected: cronjob test PASS; all crate tests green.

```bash
git add crates/harness-scheduler/src/tool.rs
git commit -m "feat(harness-scheduler): cronjob tool (agent self-scheduling, schedule-string validated)"
```

---

## Final verification (after all tasks)

- [ ] `cargo test -p harness-rs-scheduler` — all green (store CRUD, channel, scheduler ×3, cronjob).
- [ ] `cargo build` — workspace builds (the pre-existing `examples/deepseek-hello` `response_format` break is unrelated).
- [ ] `cargo tree -p harness-rs-core | grep -i harness-rs-scheduler` → empty (nothing in the framework depends on the new optional crate).
- [ ] Dispatch a final code-reviewer over the whole branch.

## Notes for the implementer

- **`Box::pin` is NOT needed** in `Scheduler::tick_once`: unlike the learning-loop case, `tick_once` is a standalone async fn (not called from within `AgentLoop::run_built_context`), so there is no recursive-future cycle. A plain `sub.run(&mut world).await` compiles. (If, surprisingly, you hit E0733, wrap with `Box::pin` — but you should not.)
- **harness-context is a NORMAL dep** of harness-scheduler (for `default_world` at runtime), not just a dev-dep. Make sure Task 3 moved it into `[dependencies]`.
- **Best-effort per job:** every per-job failure (model error, channel error, store error) logs a warning and the loop continues. Never `?`-propagate out of the per-job body in `tick_once`.
- **Optional crate:** nothing else in the framework should depend on harness-scheduler. It's opt-in like harness-mcp/harness-daemon.
- **Commits:** no Co-Authored-By / AI attribution.
