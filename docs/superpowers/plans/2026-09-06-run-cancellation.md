# Run-Level Cancellation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a caller stop a running `AgentLoop` from outside — mid-tool, mid-generation, or between iterations — and get back an `Outcome::Cancelled` carrying the partial work, without the loop making one more model call.

**Architecture:** A `tokio_util::sync::CancellationToken` lives on `AgentLoop` (default: a token nobody cancels, so existing callers are untouched). The loop checks it at the top of every iteration, and races it against the two things that block — the model step (`complete` or `stream`) and tool dispatch — with `tokio::select!` so an in-flight HTTP request or tool future is dropped rather than awaited. Cancellation is an **outcome, not an error**: the run returns `Ok(Outcome::Cancelled { .. })`, fires a new `Event::Cancelled` then `Event::SessionEnd`, and never runs `force_final_synthesis`. `Session::turn` inherits the loop's token for free because it calls the same run path.

**Tech Stack:** Rust, tokio (`select!`, `time`), `tokio-util` 0.7 (`CancellationToken`), `futures` (`StreamExt`, `stream::unfold`), existing `MockModel` / `Hook` test fixtures.

**Why not an error variant:** `Stuck` and `BudgetExhausted` are already outcomes carrying `last_text`/`tools_called`/`usage` so a caller can salvage partial work. Cancellation is the same shape — the user pressed Esc, they still want what was done. An `Err` would throw that away.

**Repo facts the implementer needs (verified 2026-09-06 at commit `bf304a1`):**
- The loop body is `AgentLoop::run_with_seed_history` in `crates/harness-loop/src/lib.rs`; the iteration is `for iter in 0..ctx.policy.max_iters { … }` starting near line 1330. The model is called once per iteration at ~line 1368:
  ```rust
  let out = if self.streaming {
      self.complete_via_stream(&ctx, world).await?
  } else {
      self.model.complete(&ctx).await?
  };
  ```
- Tools are dispatched through `async fn dispatch_bounded(&self, action: &Action, world: &mut World) -> ToolResult` (~line 2052), which already wraps the future in `tokio::time::timeout(self.tool_timeout)`. It is called from two places: a parallel read-only prefetch (`futures::future::join_all` at ~line 1567, on a cloned `World`) and the sequential per-call path (~line 1620).
- Early exits look like this (the stuck detector, ~line 1767) — mirror it:
  ```rust
  self.hooks.fire(&Event::SessionEnd, world);
  return Ok(Outcome::Stuck { reason, repeated: repeat_count, iters: iter + 1, last_text, tools_called, usage: total_usage });
  ```
  `last_text: Option<String>`, `tools_called: u32`, `total_usage: harness_core::Usage` are locals of the run body.
- `Event<'a>` in `crates/harness-core/src/event.rs` is `#[derive(Debug)] #[non_exhaustive]`; every variant has a line in `Event::name()` (~line 205-227). The doc comment on line 7 says "All 29 lifecycle events".
- `Outcome` is at ~line 488 of lib.rs; variants are `#[non_exhaustive]`.
- `AgentLoop::new` (~line 641) initialises every field literally, e.g. `streaming: false,`.
- Builder methods are `pub fn with_x(mut self, …) -> Self` (see `with_streaming` ~line 762).
- Integration tests live in `crates/harness-loop/tests/*.rs`; `tests/agent_loop.rs` lines 1–58 define the `tmp_workspace()` / `TestDir` / `task()` fixtures. **Copy those fixtures into the new test file** — test files are separate crates and cannot share them.
- `MockModel::new().script(MockResponse::text("x"))`, `.script(MockResponse::tool_call("name", json!({..})))`, `model.call_count()`.
- `Model` trait (`crates/harness-core/src/model.rs:108`): `async fn complete(&self,&Context)->Result<ModelOutput,ModelError>`; `async fn stream(&self,&Context)->Result<BoxStream<'static,Result<ModelDelta,ModelError>>,ModelError>` (has a default); `fn info(&self)->ModelInfo`. It is `#[async_trait]`.
- `Tool` trait (`crates/harness-core/src/tool.rs:40`): `fn name(&self)->&str; fn schema(&self)->&ToolSchema; fn risk(&self)->ToolRisk; async fn invoke(&self, args: Value, world: &mut World)->Result<ToolResult,ToolError>`. `ToolSchema { name, description, input }`. `ToolRisk::{ReadOnly, Idempotent, Destructive, Network}`. `ToolResult { ok, content, trace }`.
- `Hook` trait: `fn name(&self)->&str; fn matches(&self,&Event<'_>)->bool; fn fire(&self,&Event<'_>,&mut World)->HookOutcome` (`HookOutcome::Allow`).

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `Cargo.toml` (workspace root) | modify `[workspace.dependencies]` | pin `tokio-util` once for the workspace |
| `crates/harness-loop/Cargo.toml` | modify `[dependencies]` | pull `tokio-util` into the loop crate |
| `crates/harness-core/src/event.rs` | modify | add `Event::Cancelled`; fix the "29" doc; name mapping |
| `crates/harness-loop/src/lib.rs` | modify | `cancel` field, `with_cancellation`, `Outcome::Cancelled`, the three check/race points, skip synthesis |
| `crates/harness-loop/tests/cancellation.rs` | **create** | all behavioural tests for this feature |
| `CHANGELOG.md` | modify | one entry under Unreleased |

No new source files in `lib.rs`'s neighbourhood: the loop crate keeps its one-big-file convention and this change is ~80 lines of it.

---

### Task 1: The dependency and the event

**Files:**
- Modify: `Cargo.toml` — the `[workspace.dependencies]` block (tokio is at ~line 31)
- Modify: `crates/harness-loop/Cargo.toml` — `[dependencies]`
- Modify: `crates/harness-core/src/event.rs` — enum (~line 13 onward) and `name()` (~line 205-227)

- [ ] **Step 1: Write the failing test** — append to the very end of `crates/harness-core/src/event.rs`:

```rust
#[cfg(test)]
mod cancellation_event {
    use super::Event;

    // Cancellation is its own lifecycle event, not a `Stop` or an `Error`:
    // a hook that pages on `Error` must not fire when a user pressed Esc, and
    // a hook that bills on `Stop` must know this run did not finish.
    #[test]
    fn cancelled_is_a_named_lifecycle_event() {
        assert_eq!(Event::Cancelled.name(), "Cancelled");
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p harness-rs-core --lib cancelled_is_a_named_lifecycle_event`
Expected: compile error `no variant or associated item named `Cancelled` found for enum `Event``

- [ ] **Step 3: Add the dependency**

In the workspace `Cargo.toml`, inside `[workspace.dependencies]`, directly after the `tokio = { … }` line, add:

```toml
tokio-util = { version = "0.7", default-features = false }
```

In `crates/harness-loop/Cargo.toml`, inside `[dependencies]`, directly after the `tokio = { workspace = true, features = ["signal"] }` line, add:

```toml
# CancellationToken for stopping a run from outside (src/lib.rs `cancel`).
tokio-util       = { workspace = true }
```

- [ ] **Step 4: Add the variant**

In `crates/harness-core/src/event.rs`, change the doc on line 7 from `All 29 lifecycle events` to `All 30 lifecycle events`.

In the `Event` enum, immediately **before** the `Stop,` variant (~line 149), add:

```rust
    /// The run was stopped from outside through the loop's
    /// `CancellationToken`, before the model reached a natural end. Fires
    /// once, then `SessionEnd`. Distinct from `Stop` (the run finished) and
    /// `Error` (something broke): the work was fine, the caller ended it.
    Cancelled,
```

In `Event::name()`, immediately before the `Event::Stop => "Stop",` arm, add:

```rust
            Event::Cancelled => "Cancelled",
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p harness-rs-core --lib cancelled_is_a_named_lifecycle_event`
Expected: `test result: ok. 1 passed`

- [ ] **Step 6: Make sure nothing else pattern-matched exhaustively on Event**

Run: `cargo build --workspace`
Expected: clean. (`Event` is `#[non_exhaustive]`, so downstream `match`es already carry a wildcard; if this errors, the failing match is inside the workspace — add `Event::Cancelled => …` mirroring its `Stop` arm.)

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/harness-loop/Cargo.toml crates/harness-core/src/event.rs
git commit -m "feat(core): Event::Cancelled, the thirtieth lifecycle event"
```

---

### Task 2: The token, the outcome, and the cheapest check

**Files:**
- Modify: `crates/harness-loop/src/lib.rs` — `use` block (top of file), `Outcome` (~488), `AgentLoop` struct (~555-600), `new` (~641), builders (~762), run body (~1330)
- Create: `crates/harness-loop/tests/cancellation.rs`

- [ ] **Step 1: Create the test file with fixtures and the first two tests**

Create `crates/harness-loop/tests/cancellation.rs`:

```rust
//! Stopping a run from outside. Every test here drives a real `AgentLoop`
//! against `MockModel` or a hand-rolled slow model; nothing is mocked at the
//! loop level, because the behaviour under test *is* the loop's.

use harness_context::default_world;
use harness_core::{Task, World};
use harness_loop::{AgentLoop, Outcome};
use harness_models::{MockModel, MockResponse};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

// ------------------------------------------------------------------
// fixtures (copied from tests/agent_loop.rs — test crates cannot share)
// ------------------------------------------------------------------

fn tmp_workspace() -> (TestDir, World) {
    let td = TestDir::new();
    let world = default_world(td.0.clone());
    (td, world)
}

struct TestDir(PathBuf);
static TD_SEQ: AtomicU64 = AtomicU64::new(0);
impl TestDir {
    fn new() -> Self {
        let pid = std::process::id();
        let n = TD_SEQ.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("harness-cancel-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        TestDir(p)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn task(desc: &str) -> Task {
    Task {
        description: desc.into(),
        source: None,
        deadline: None,
    }
}

// ------------------------------------------------------------------
// 1. A token cancelled before the run starts costs nothing
// ------------------------------------------------------------------

#[tokio::test]
async fn a_token_cancelled_before_start_never_calls_the_model() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new().script(MockResponse::text("never seen"));
    let token = CancellationToken::new();
    token.cancel();

    let agent = AgentLoop::new(model).with_cancellation(token);
    let outcome = agent
        .run_with_max_iters(task("anything"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Cancelled { iters, tools_called, .. } => {
            assert_eq!(iters, 0, "no iteration should have started");
            assert_eq!(tools_called, 0);
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert_eq!(agent.model.call_count(), 0, "a cancelled run must not spend a model call");
}

// ------------------------------------------------------------------
// 2. A token nobody cancels changes nothing
// ------------------------------------------------------------------

#[tokio::test]
async fn an_uncancelled_token_changes_nothing() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new().script(MockResponse::text("hello"));

    let outcome = AgentLoop::new(model)
        .with_cancellation(CancellationToken::new())
        .run_with_max_iters(task("say hi"), &mut world, 5)
        .await
        .unwrap();

    match outcome {
        Outcome::Done { text, iters, .. } => {
            assert_eq!(text.as_deref(), Some("hello"));
            assert_eq!(iters, 1);
        }
        other => panic!("expected Done, got {other:?}"),
    }
}
```

Add `tokio-util` to the loop crate's **dev-dependencies** too, since the test file names the type. In `crates/harness-loop/Cargo.toml`, under `[dev-dependencies]` (create the section if it does not exist, after `[dependencies]`):

```toml
tokio-util = { workspace = true }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: compile errors — `no method named `with_cancellation`` and `no variant named `Cancelled``.

- [ ] **Step 3: Add the import, the outcome variant, the field, the builder**

At the top of `crates/harness-loop/src/lib.rs`, with the other `use` lines, add:

```rust
use tokio_util::sync::CancellationToken;
```

In `pub enum Outcome`, after the closing `},` of the `Stuck { … }` variant (the last one, ~line 551), add:

```rust
    /// The caller stopped the run through [`AgentLoop::with_cancellation`]
    /// before the model reached a natural end. Carries partial work like
    /// `Stuck` does, because the user who pressed Esc still wants what was
    /// done. The loop makes **no** further model call after this — not even
    /// the forced final synthesis the other early exits perform.
    #[non_exhaustive]
    Cancelled {
        /// Iterations that had *started* when the token fired. `0` means the
        /// token was already cancelled on entry.
        iters: u32,
        last_text: Option<String>,
        tools_called: u32,
        usage: harness_core::Usage,
    },
```

In `pub struct AgentLoop<M>`, directly after the `pub streaming: bool,` field (~line 578), add:

```rust
    /// Stops the run from outside. Checked at the top of every iteration and
    /// raced against the model step and every tool dispatch, so a cancel
    /// lands within one await point rather than at the next iteration
    /// boundary. Defaults to a token nobody holds, which never fires.
    pub cancel: CancellationToken,
```

In `AgentLoop::new`, directly after `streaming: false,`, add:

```rust
            cancel: CancellationToken::new(),
```

Directly after the `with_streaming` builder, add:

```rust
    /// Hand the run a token the caller can cancel. Cloning a
    /// `CancellationToken` shares it, so keep one and pass a clone here:
    ///
    /// ```ignore
    /// let token = CancellationToken::new();
    /// let agent = AgentLoop::new(model).with_cancellation(token.clone());
    /// // … later, from anywhere:
    /// token.cancel();
    /// ```
    pub fn with_cancellation(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }
```

- [ ] **Step 4: Add the check at the top of the iteration**

In the run body, find the line `for iter in 0..ctx.policy.max_iters {`. Insert as the **first statement inside** the loop body:

```rust
            if self.cancel.is_cancelled() {
                tracing::info!(iter, "run cancelled by caller");
                self.hooks.fire(&Event::Cancelled, world);
                self.hooks.fire(&Event::SessionEnd, world);
                return Ok(Outcome::Cancelled {
                    iters: iter,
                    last_text,
                    tools_called,
                    usage: total_usage,
                });
            }
```

`last_text`, `tools_called`, `total_usage` are the run body's existing locals (used the same way by the `Outcome::Stuck` return). If `last_text` is declared *after* this point in the body, move its declaration (`let mut last_text: Option<String> = None;`) above the `for` loop.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: `test result: ok. 2 passed`

- [ ] **Step 6: Run the existing loop tests to make sure the default token is inert**

Run: `cargo test -p harness-rs-loop`
Expected: all green, same count as before plus 2.

- [ ] **Step 7: Commit**

```bash
git add crates/harness-loop/Cargo.toml crates/harness-loop/src/lib.rs crates/harness-loop/tests/cancellation.rs
git commit -m "feat(loop): a CancellationToken on AgentLoop, checked per iteration"
```

---

### Task 3: A cancel during a tool call does not wait for the tool

**Files:**
- Modify: `crates/harness-loop/src/lib.rs` — `dispatch_bounded` (~2052), the sequential tool loop (~1620), the prefetch block (~1560-1575)
- Modify: `crates/harness-loop/tests/cancellation.rs`

- [ ] **Step 1: Add a slow tool and two tests**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 3. A cancel during a tool call abandons the tool
// ------------------------------------------------------------------

use async_trait::async_trait;
use harness_core::{ToolError, ToolResult, ToolRisk, ToolSchema};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A tool that takes ten seconds. If cancellation waits for it, the test
/// below takes ten seconds and fails its elapsed-time assertion.
struct SlowTool {
    schema: ToolSchema,
}
impl SlowTool {
    fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "slow".into(),
                description: "sleeps for ten seconds".into(),
                input: json!({"type": "object", "properties": {}}),
            },
        }
    }
}
#[async_trait]
impl harness_core::Tool for SlowTool {
    fn name(&self) -> &str {
        "slow"
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Idempotent
    }
    async fn invoke(&self, _args: serde_json::Value, _world: &mut World) -> Result<ToolResult, ToolError> {
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok(ToolResult { ok: true, content: json!("slept"), trace: None })
    }
}

#[tokio::test]
async fn cancelling_during_a_slow_tool_returns_promptly() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    let token = CancellationToken::new();
    let fire = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        fire.cancel();
    });

    let started = Instant::now();
    let agent = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new()))
        .with_cancellation(token);
    let outcome = agent
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    match outcome {
        Outcome::Cancelled { iters, tools_called, .. } => {
            assert_eq!(iters, 1, "the first iteration had started");
            assert_eq!(tools_called, 1, "the tool was dispatched before the cancel");
        }
        other => panic!("expected Cancelled, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "cancel must drop the in-flight tool, not await it: took {elapsed:?}"
    );
}

// ------------------------------------------------------------------
// 4. No model call after a cancel — not even the final synthesis
// ------------------------------------------------------------------

#[tokio::test]
async fn cancellation_does_not_force_a_final_synthesis() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("a synthesis that must never be requested"));
    let token = CancellationToken::new();
    let fire = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        fire.cancel();
    });

    let agent = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new()))
        .with_cancellation(token);
    let _ = agent
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    // Exactly one call: the turn that asked for the tool. `BudgetExhausted`
    // and the monotony exit both call the model once more to write a final
    // answer; a cancel is the user saying stop, and stop means stop.
    assert_eq!(agent.model.call_count(), 1);
}
```

Add `async-trait` to `[dev-dependencies]` in `crates/harness-loop/Cargo.toml` if it is not already there (it is a workspace dep):

```toml
async-trait = { workspace = true }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p harness-rs-loop --test cancellation cancelling_during_a_slow_tool_returns_promptly`
Expected: FAIL on the elapsed assertion (`took 10.0…s`) — the tool ran to completion, the cancel was only noticed at the next iteration boundary. If instead it reports `Done`, the cancel landed after the loop finished; either way it is a failure of the same cause.

- [ ] **Step 3: Race tool dispatch against the token**

Replace the whole body of `dispatch_bounded` with:

```rust
    async fn dispatch_bounded(&self, action: &Action, world: &mut World) -> ToolResult {
        let fut = self.tools.dispatch(action, world);
        let bounded = async {
            match self.tool_timeout {
                Some(deadline) => match tokio::time::timeout(deadline, fut).await {
                    Ok(r) => r,
                    Err(_) => {
                        tracing::warn!(
                            target: "harness.telemetry",
                            event = "tool.deadline",
                            "gen_ai.tool.name" = %action.tool,
                            seconds = deadline.as_secs(),
                        );
                        return Ok(ToolResult {
                            ok: false,
                            content: serde_json::json!({
                                "error": format!(
                                    "tool call exceeded its {}s deadline and was cancelled; \
                                     the operation may be too broad — narrow it or try a \
                                     different approach",
                                    deadline.as_secs()
                                ),
                                "timeout": true,
                            }),
                            trace: None,
                        });
                    }
                },
                None => fut.await,
            }
        };
        // `biased` so that a token already cancelled wins even when the tool
        // future is also ready — the caller said stop, and a result produced
        // after that would be work the run is about to throw away anyway.
        let dispatched = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                tracing::info!("gen_ai.tool.name" = %action.tool, "tool call dropped: run cancelled");
                return ToolResult {
                    ok: false,
                    content: serde_json::json!({"error": "run cancelled", "cancelled": true}),
                    trace: None,
                };
            }
            r = bounded => r,
        };
        dispatched.unwrap_or_else(|e| ToolResult {
            ok: false,
            content: serde_json::json!({"error": e.to_string()}),
            trace: None,
        })
    }
```

Note the deadline arm now returns `Ok(ToolResult { .. })` (wrapped) because it lives inside the `bounded` block whose type is `Result<ToolResult, _>`; the outer `unwrap_or_else` is unchanged.

- [ ] **Step 4: Exit the iteration once a tool reports the cancel**

Find the sequential tool loop, `for call in &out.tool_calls {`. After the line that obtains the result — it reads `self.dispatch_bounded(&action, world).await` (~line 1620) and its value is bound to a local (call it `r` or whatever the code names it) — and **before** that result is pushed into `ctx`/history, insert:

```rust
                if self.cancel.is_cancelled() {
                    tracing::info!(iter, "run cancelled during tool dispatch");
                    self.hooks.fire(&Event::Cancelled, world);
                    self.hooks.fire(&Event::SessionEnd, world);
                    return Ok(Outcome::Cancelled {
                        iters: iter + 1,
                        last_text,
                        tools_called,
                        usage: total_usage,
                    });
                }
```

`tools_called` must already have been incremented for this call at that point (the test asserts `tools_called == 1`). If the increment happens after the push, move the increment to directly after the dispatch line.

- [ ] **Step 5: Cover the parallel prefetch path the same way**

Find the prefetch block (`if lead.len() > 1 { let futs = … ; for (id, r) in futures::future::join_all(futs).await { prefetched.insert(id, r); } }`). Replace the `for (id, r) in futures::future::join_all(futs).await { … }` with:

```rust
                    let results = tokio::select! {
                        biased;
                        _ = self.cancel.cancelled() => Vec::new(),
                        rs = futures::future::join_all(futs) => rs,
                    };
                    for (id, r) in results {
                        prefetched.insert(id, r);
                    }
```

An empty `prefetched` on cancel is safe: the sequential loop that follows re-dispatches anything not prefetched, and its own cancel check (Step 4) exits on the first one.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: `test result: ok. 4 passed`, and the slow-tool test finishes in well under a second.

- [ ] **Step 7: Run the whole loop crate**

Run: `cargo test -p harness-rs-loop`
Expected: green. `tests/tool_result_cap.rs`, `tests/parallel_dispatch.rs` and `tests/stalled_turn.rs` exercise `dispatch_bounded` and the prefetch path; they are the ones that would catch a broken refactor.

- [ ] **Step 8: Commit**

```bash
git add crates/harness-loop/Cargo.toml crates/harness-loop/src/lib.rs crates/harness-loop/tests/cancellation.rs
git commit -m "feat(loop): a cancel drops the in-flight tool instead of awaiting it"
```

---

### Task 4: A cancel mid-generation drops the request

**Files:**
- Modify: `crates/harness-loop/src/lib.rs` — the model call (~1368) and `complete_via_stream` (~1946)
- Modify: `crates/harness-loop/tests/cancellation.rs`

- [ ] **Step 1: Add a slow streaming model and the test**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 5. A cancel mid-stream stops generation
// ------------------------------------------------------------------

use futures::stream::BoxStream;
use harness_core::{Context, Event, Hook, HookOutcome, Model, ModelDelta, ModelError, ModelInfo, ModelOutput};
use std::sync::atomic::AtomicU32;

/// Streams one character every 100ms for five seconds. Delegates everything
/// that is not streaming to a `MockModel` so `info()` needs no hand-built
/// `ModelInfo`.
struct SlowStreamModel {
    inner: MockModel,
}
#[async_trait]
impl Model for SlowStreamModel {
    async fn complete(&self, ctx: &Context) -> Result<ModelOutput, ModelError> {
        self.inner.complete(ctx).await
    }
    async fn stream(
        &self,
        _ctx: &Context,
    ) -> Result<BoxStream<'static, Result<ModelDelta, ModelError>>, ModelError> {
        let s = futures::stream::unfold(0u32, |i| async move {
            if i >= 50 {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            Some((Ok(ModelDelta::Text("x".into())), i + 1))
        });
        Ok(Box::pin(s))
    }
    fn info(&self) -> ModelInfo {
        let mut info = self.inner.info();
        info.supports_streaming = true;
        info
    }
}

/// Counts `ModelTokenDelta` events, so the test can prove the stream was
/// running when it was cut, not finished before the cancel landed.
struct DeltaCounter(Arc<AtomicU32>);
impl Hook for DeltaCounter {
    fn name(&self) -> &str {
        "delta-counter"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::ModelTokenDelta { .. })
    }
    fn fire(&self, _ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        HookOutcome::Allow
    }
}

#[tokio::test]
async fn cancelling_mid_stream_stops_generation() {
    let (_td, mut world) = tmp_workspace();
    let model = SlowStreamModel { inner: MockModel::new().script(MockResponse::text("unused")) };
    let deltas = Arc::new(AtomicU32::new(0));
    let token = CancellationToken::new();
    let fire = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(350)).await;
        fire.cancel();
    });

    let started = Instant::now();
    let outcome = AgentLoop::new(model)
        .with_streaming(true)
        .with_hook(Arc::new(DeltaCounter(deltas.clone())))
        .with_cancellation(token)
        .run_with_max_iters(task("stream something long"), &mut world, 5)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert!(matches!(outcome, Outcome::Cancelled { iters: 1, .. }), "got {outcome:?}");
    let seen = deltas.load(Ordering::SeqCst);
    assert!(seen >= 1, "the stream had started delivering before the cancel");
    assert!(seen < 50, "the stream was cut short, not drained: saw {seen} of 50");
    assert!(elapsed < Duration::from_secs(2), "cancel must drop the stream: took {elapsed:?}");
}
```

Check the exact builder name for adding a hook: run `grep -n "pub fn with_hook" crates/harness-loop/src/lib.rs`. If it is named differently, use that name in the test.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p harness-rs-loop --test cancellation cancelling_mid_stream_stops_generation`
Expected: FAIL — either `saw 50 of 50` (stream drained) or the elapsed assertion at ~5s. Both mean the same thing: the stream was awaited to the end.

- [ ] **Step 3: Race the model step against the token**

Replace the model call at ~line 1368:

```rust
            let out = if self.streaming {
                self.complete_via_stream(&ctx, world).await?
            } else {
                self.model.complete(&ctx).await?
            };
```

with:

```rust
            let Some(out) = self.model_step(&ctx, world).await? else {
                tracing::info!(iter, "run cancelled during model step");
                self.hooks.fire(&Event::Cancelled, world);
                self.hooks.fire(&Event::SessionEnd, world);
                return Ok(Outcome::Cancelled {
                    iters: iter + 1,
                    last_text,
                    tools_called,
                    usage: total_usage,
                });
            };
```

Then add this method to `impl<M: Model> AgentLoop<M>`, directly **above** `complete_via_stream`:

```rust
    /// One model call, racing the run's cancellation token.
    ///
    /// `None` means the token fired first. Dropping the un-awaited future is
    /// what cancels the underlying HTTP request (reqwest aborts on drop) or
    /// the SSE stream, so the model stops generating rather than finishing
    /// into a void. `biased` so a token already cancelled wins a tie.
    async fn model_step(
        &self,
        ctx: &Context,
        world: &mut World,
    ) -> Result<Option<ModelOutput>, HarnessError> {
        if self.streaming {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Ok(None),
                r = self.complete_via_stream(ctx, world) => r.map(Some),
            }
        } else {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Ok(None),
                r = self.model.complete(ctx) => r
                    .map(Some)
                    .map_err(harness_core::HarnessError::Model),
            }
        }
    }
```

If `HarnessError::Model` is not the variant the existing `?` on `self.model.complete` converted into, mirror whatever `From` impl that `?` used — run `grep -n "impl From<ModelError> for HarnessError" crates/harness-core/src/*.rs` to find it.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: `test result: ok. 5 passed`, the mid-stream test finishing in well under a second.

- [ ] **Step 5: Run the whole loop crate**

Run: `cargo test -p harness-rs-loop`
Expected: green. `tests/session_replay.rs` and `tests/telemetry.rs` hook `PreModel`/`PostModel`; they are unaffected because `model_step` fires neither — the surrounding code still does.

- [ ] **Step 6: Commit**

```bash
git add crates/harness-loop/src/lib.rs crates/harness-loop/tests/cancellation.rs
git commit -m "feat(loop): a cancel drops the in-flight model request or stream"
```

---

### Task 5: The event fires exactly once, then SessionEnd

**Files:**
- Modify: `crates/harness-loop/tests/cancellation.rs`
- Modify (only if the test fails): `crates/harness-loop/src/lib.rs`

- [ ] **Step 1: Add the test**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 6. Cancelled fires once, and SessionEnd follows it
// ------------------------------------------------------------------

use std::sync::Mutex;

struct EventLog(Arc<Mutex<Vec<&'static str>>>);
impl Hook for EventLog {
    fn name(&self) -> &str {
        "event-log"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::Cancelled | Event::SessionEnd | Event::Stop | Event::Error { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        self.0.lock().unwrap().push(ev.name());
        HookOutcome::Allow
    }
}

#[tokio::test]
async fn cancelled_fires_once_then_session_end() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    let log = Arc::new(Mutex::new(Vec::new()));
    let token = CancellationToken::new();
    let fire = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        fire.cancel();
    });

    let _ = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new()))
        .with_hook(Arc::new(EventLog(log.clone())))
        .with_cancellation(token)
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    let seen = log.lock().unwrap().clone();
    assert_eq!(seen, vec!["Cancelled", "SessionEnd"], "got {seen:?}");
}
```

`ev.name()` must be `pub`; it already is (used by `SessionRecorder`). If `Event::SessionEnd` is spelled differently, `grep -n "SessionEnd" crates/harness-core/src/event.rs`.

- [ ] **Step 2: Run to verify**

Run: `cargo test -p harness-rs-loop --test cancellation cancelled_fires_once_then_session_end`
Expected: PASS on the first run — Tasks 2–4 each fire `Cancelled` then `SessionEnd` at their own exit and return immediately, so only one pair can ever be emitted. **If it fails** with a doubled `Cancelled`, one exit path is not returning after firing; find it and add the missing `return`. **If it fails** with `Stop` or `Error` present, an exit path fell through to the normal end — same fix.

- [ ] **Step 3: Commit**

```bash
git add crates/harness-loop/tests/cancellation.rs
git commit -m "test(loop): Cancelled fires once and SessionEnd follows"
```

---

### Task 6: Changelog

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add the entry**

Open `CHANGELOG.md`. Under the topmost `## Unreleased` heading (create it above the newest version heading if absent), add:

```markdown
### Added

- **Run-level cancellation.** `AgentLoop::with_cancellation(CancellationToken)`
  stops a run from outside. The token is checked every iteration and raced
  against the model step and every tool dispatch, so a cancel drops the
  in-flight HTTP request, SSE stream or tool future instead of awaiting it.
  The run returns `Outcome::Cancelled { iters, last_text, tools_called, usage }`
  — an outcome, not an error, because the partial work is still the caller's —
  and makes no further model call, not even the forced final synthesis the
  other early exits perform. Fires the new `Event::Cancelled` (the 30th
  lifecycle event) followed by `SessionEnd`. A `Session` inherits its loop's
  token. Default is a token nobody holds, so existing callers are unchanged.
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs: changelog for run-level cancellation"
```

---

## Final verification (controller runs this, not a task)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must be clean before `superpowers:finishing-a-development-branch`.

---

## Self-review

**Spec coverage**
- Stop from outside, between iterations → Task 2 Step 4.
- Stop mid-tool, promptly → Task 3 (sequential + prefetch paths).
- Stop mid-generation, promptly, both streaming and not → Task 4.
- Outcome carries partial work → `Outcome::Cancelled` fields mirror `Stuck` (Task 2).
- No further model call, incl. no synthesis → asserted by Task 3 test 4 (`call_count() == 1`); guaranteed structurally because every cancel exit `return`s before the synthesis sites.
- `Event::Cancelled` then `SessionEnd`, once → Task 1 + Task 5.
- `Session::turn` inherits → no code needed; it calls `run_with_seed_history` on the same `AgentLoop`. Noted in changelog.
- Existing callers unchanged → Task 2 test 2 + full-crate runs in every task.

**Placeholder scan** — none. Every code step is complete; the two "if it is named differently, grep" notes are verification instructions, not gaps.

**Type consistency** — `with_cancellation(CancellationToken)`, field `cancel: CancellationToken`, `Outcome::Cancelled { iters, last_text, tools_called, usage }`, `Event::Cancelled` (unit variant), `model_step(&ctx, world) -> Result<Option<ModelOutput>, HarnessError>` are used identically across Tasks 2–5.
