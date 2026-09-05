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
- `Outcome` is at ~line 488 of lib.rs. Its **variants** are `#[non_exhaustive]` (so new *fields* don't break `..` destructuring) but the **enum itself is not** — it is `#[derive(Debug, Clone)]` only. So adding the `Cancelled` variant makes every exhaustive `match` on `Outcome` in every *other* crate of the workspace a compile error (`harness-serve`, `harness-cli`, most `examples/*`). This was missed when the plan was written and discovered by Task 2's implementer; Task 2b below is the sweep. Matches inside `harness-loop` itself (`run_typed_with_max_iters`, `Session::turn`, `subagent.rs`, `tests/prefix_cache_live.rs`) were fixed as part of Task 2 because the crate could not compile otherwise.
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
| `crates/harness-loop/src/hooks/broadcast.rs` | modify | project `Cancelled` onto the SSE feed (Task 5) |
| `crates/harness-loop/src/telemetry.rs` | modify | record `run.cancelled` inside the run span (Task 5) |
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

In `crates/harness-core/src/event.rs`, line 7 says `All 29 lifecycle events the framework emits (DESIGN.md §10).` — and the number is wrong (the enum has 32 variants; five places in the repo disagree on the count). **Drop the number** rather than correct it: make the line `/// All lifecycle events the framework emits (DESIGN.md §10).` Do the same in `crates/harness-core/Cargo.toml`'s `description` (line 9): replace `and 29 lifecycle events` with `and the lifecycle events`. (Executed as commits `f5bd669` + `97c4b95`; the second is the code-review fix that made this decision.)

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

- [ ] **Step 6: Sanity-build the workspace**

Run: `cargo build --workspace`
Expected: clean, no edits needed. This was checked on 2026-09-06: no `match` on `Event` anywhere lists `Event::Stop` as an arm — every consumer uses `matches!`/`if let` or already ends in `_ =>` — so a new variant cannot make any match non-exhaustive. If this build *does* error, stop and report `BLOCKED` with the error; do not add arms on your own.

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

No `[dev-dependencies]` change is needed: `tokio-util` became a normal dependency in Task 1, and integration tests under `tests/` can use any normal dependency of the crate.

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

`last_text`, `tools_called`, `total_usage` are the run body's existing locals (used the same way by the `Outcome::Stuck` return); `last_text` is declared at ~line 1252, above the loop.

**Also required for the crate to compile (discovered in execution, commit `4a192fe`):** four exhaustive matches on `Outcome` *inside* `harness-loop` needed a `Cancelled` arm — `run_typed_with_max_iters`'s text extraction and `Session::turn`'s reply (both `lib.rs`, fold into the existing `last_text` arm), `Subagent::run`'s report (`src/subagent.rs`, mirror the `BudgetExhausted` arm: text → `DoneWithConcerns`, none → `Blocked`), and `tests/prefix_cache_live.rs`'s usage closure. These are the minimal folds, not new behaviour.

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

### Task 2b: Every consumer of `Outcome` learns to say "cancelled"

**Why this task exists.** `Outcome` is not `#[non_exhaustive]` at the enum level (only its variants are), so Task 2's new variant broke every exhaustive `match` on `Outcome` in every other crate of the workspace — 18 sites in 11 files. `cargo build --workspace --all-targets` currently fails with `error[E0004]`. Each site needs a real `Cancelled` arm. **The rule:** where a site already treats `BudgetExhausted` and `Stuck` identically through an or-pattern, fold `Cancelled` in (it binds the same four fields). Where a site distinguishes them — a status string, a label, a message — `Cancelled` gets its own arm with its own honest word. **Never** route a cancel through a wildcard `_ =>`: that hides the next variant too, and a UI that says "stuck" for a run the user stopped is lying.

**Files (all Modify):**
- `crates/harness-serve/src/service.rs` — `answer_of` (~line 466-474)
- `crates/harness-cli/src/main.rs` — JSON-mode match (~521-549), human-mode match (~562-585), REPL match (~809-830), replay summary (~1156-1169)
- `examples/ai-note/src/server.rs` — sync chat match (~1331-1357), SSE chat match (~1593-1658)
- `examples/cap/src/bin/cap.rs` — one-shot match (~270-287), REPL match (~362-373)
- `examples/cap/src/bin/cap-tui.rs` — (~288-296)
- `examples/investor-bot/src/main.rs` — (~580-620), (~719-760)
- `examples/personal-assistant/src/main.rs` — (~874-899), (~991-1010)
- `examples/eval-bench/src/main.rs` — (~135-151)
- `examples/eval-bench/src/bench_suite.rs` — outcome match (~728-743) **and** status match (~754-760)
- `examples/crate-keeper/src/main.rs` — (~150-158)
- `examples/deepseek-caps-e2e/src/main.rs` — `outcome_text` (~75-82)

- [ ] **Step 1: Confirm the failing gate**

Run: `cargo build --workspace --all-targets --message-format=short 2>&1 | grep -c E0004`
Expected: `10` (cargo stops at the first failing crate in each dependency chain, so this undercounts the 18 sites; the grep in Step 3 is the full list).

- [ ] **Step 2: `harness-serve` — the answer helper**

In `crates/harness-serve/src/service.rs`, replace the whole `answer_of` function (its doc comment included) with:

```rust
/// Best-effort answer text from any terminal [`Outcome`] — a partial answer from
/// a budget-exhausted, stuck or cancelled run beats an empty reply.
fn answer_of(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Done { text, .. } => text.clone().unwrap_or_default(),
        Outcome::BudgetExhausted { last_text, .. } => last_text.clone().unwrap_or_default(),
        Outcome::Stuck { last_text, .. } => last_text.clone().unwrap_or_default(),
        Outcome::Cancelled { last_text, .. } => last_text.clone().unwrap_or_default(),
    }
}
```

- [ ] **Step 3: Get the full site list now that `harness-serve` compiles**

Run: `grep -rn "Outcome::Stuck" crates examples --include='*.rs' | grep -v "crates/harness-loop/"`
Expected: exactly these 17 lines (line numbers ±2):
`crates/harness-cli/src/main.rs:535`, `:574`, `:817`, `:1166`; `crates/harness-serve/src/service.rs:472`; `examples/ai-note/src/server.rs:1346`, `:1632`; `examples/cap/src/bin/cap.rs:278`, `:368`; `examples/cap/src/bin/cap-tui.rs:294`; `examples/investor-bot/src/main.rs:597`, `:736`; `examples/personal-assistant/src/main.rs:887`, `:1001`; `examples/eval-bench/src/main.rs:145`; `examples/eval-bench/src/bench_suite.rs:735`; `examples/crate-keeper/src/main.rs:154`; `examples/deepseek-caps-e2e/src/main.rs:78`. Every one of these is a match that lists `Stuck` and therefore must now list `Cancelled`. If you find a line not in this list, it still gets the same treatment — report it in your summary.

- [ ] **Step 4: `harness-cli` — four sites, each distinguishes outcomes, so each gets its own arm**

(a) JSON mode. After the `Outcome::Stuck { … } => ( "stuck", … ),` arm (~line 535-548) add:

```rust
            Outcome::Cancelled {
                last_text,
                iters,
                tools_called,
                usage,
                ..
            } => (
                "cancelled",
                last_text.clone(),
                *iters,
                *tools_called,
                usage.input_tokens,
                usage.output_tokens,
            ),
```

(b) Human mode. After the `Outcome::Stuck { last_text, iters, reason, .. } => { eprintln!("(stuck after {iters} iters: {reason})"); … }` arm (~line 574-584) add:

```rust
            Outcome::Cancelled {
                last_text, iters, ..
            } => {
                eprintln!("(cancelled after {iters} iters)");
                if let Some(t) = last_text {
                    println!("{}", t.trim());
                }
            }
```

(c) `harness code` REPL. After the `Ok(Outcome::Stuck { … }) => { eprintln!("\x1b[33m(stuck after {iters} iters: {reason})\x1b[0m"); last_text.unwrap_or_default() }` arm (~line 817-825) add:

```rust
            Ok(Outcome::Cancelled {
                last_text, iters, ..
            }) => {
                eprintln!("\x1b[33m(cancelled after {iters} iters)\x1b[0m");
                last_text.unwrap_or_default()
            }
```

(d) Replay summary. After the `Outcome::Stuck { iters, reason, .. } => { println!("  outcome:       Stuck after {iters} iter(s): {reason}"); }` arm (~line 1166-1168) add:

```rust
        Outcome::Cancelled { iters, .. } => {
            println!("  outcome:       Cancelled after {iters} iter(s)");
        }
```

- [ ] **Step 5: `ai-note` — two sites, both carry a `warning` string the UI shows, so each gets its own**

(a) Sync chat (~line 1331-1357). After the `Outcome::Stuck { iters, last_text, usage, .. } => ( last_text.unwrap_or_else(|| "(stuck)".into()), iters, false, usage, ),` arm and before the closing `};`, add:

```rust
        Outcome::Cancelled {
            iters,
            last_text,
            usage,
            ..
        } => (
            last_text.unwrap_or_else(|| "(cancelled)".into()),
            iters,
            false,
            usage,
        ),
```

(b) SSE chat (~line 1593-1658). After the `Ok(Outcome::Stuck { … }) => { … "warning":"stuck" … }` arm closes (~line 1653) and before `Err(e) => {`, add — it is the `Stuck` arm with the word changed:

```rust
            Ok(Outcome::Cancelled {
                iters,
                last_text,
                usage,
                ..
            }) => {
                let reply = last_text.unwrap_or_else(|| "(cancelled)".into());
                if let Ok(db) = open_db_state(&s) {
                    let _ = db.append_chat_message(&uid, &sid, "asst", &reply, Some(iters));
                    let _ = db.insert_audit(
                        Some(&uid),
                        "chat_message",
                        Some(&sid),
                        Some(&json!({"iters": iters, "warning":"cancelled"}).to_string()),
                        usage.input_tokens as i64,
                        usage.output_tokens as i64,
                    );
                }
                let _ = tx_done.send(
                    json!({"type":"done","ok":false,"iters":iters,"reply":reply,"warning":"cancelled"}),
                );
            }
```

- [ ] **Step 6: `cap` and `cap-tui`**

(a) `cap.rs` one-shot (~line 270-287) distinguishes with a coloured line, so after the `Outcome::Stuck { … } => { eprintln!("\x1b[33m(stuck after {iters} iters: {reason})\x1b[0m"); last_text.clone().unwrap_or_default() }` arm add:

```rust
            Outcome::Cancelled {
                last_text, iters, ..
            } => {
                eprintln!("\x1b[33m(cancelled after {iters} iters)\x1b[0m");
                last_text.clone().unwrap_or_default()
            }
```

(b) `cap.rs` REPL (~line 366-368) and (c) `cap-tui.rs` (~line 292-294) both fold `BudgetExhausted` and `Stuck` into one or-pattern. In **each**, replace

```rust
            Ok(Outcome::BudgetExhausted { last_text, .. })
            | Ok(Outcome::Stuck { last_text, .. }) => last_text.unwrap_or_default(),
```
with
```rust
            Ok(Outcome::BudgetExhausted { last_text, .. })
            | Ok(Outcome::Stuck { last_text, .. })
            | Ok(Outcome::Cancelled { last_text, .. }) => last_text.unwrap_or_default(),
```

- [ ] **Step 7: `investor-bot` — two or-patterns that bind all four fields; fold**

(a) ~line 590-603: the head currently reads
```rust
        Outcome::BudgetExhausted {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        }
        | Outcome::Stuck {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        } => {
```
Insert a third alternative so it reads
```rust
        Outcome::BudgetExhausted {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        }
        | Outcome::Stuck {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        }
        | Outcome::Cancelled {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        } => {
```
The body ("✗ stopped after {iters} iter(s), …") is accurate for a cancel and stays as is.

(b) ~line 729-742: identical shape wrapped in `Ok(…)`. Insert `| Ok(Outcome::Cancelled { iters, last_text, tools_called, usage, .. })` as the third alternative, formatted like its neighbours.

- [ ] **Step 8: `personal-assistant` — two or-patterns; fold**

(a) ~line 884-889: replace
```rust
        Outcome::BudgetExhausted {
            iters, last_text, ..
        }
        | Outcome::Stuck {
            iters, last_text, ..
        } => {
```
with
```rust
        Outcome::BudgetExhausted {
            iters, last_text, ..
        }
        | Outcome::Stuck {
            iters, last_text, ..
        }
        | Outcome::Cancelled {
            iters, last_text, ..
        } => {
```
(b) ~line 998-1003: same, with each alternative wrapped in `Ok(…)`.

- [ ] **Step 9: `eval-bench` — the runner folds; the suite gets its own status**

(a) `main.rs` ~line 139-150: add a third alternative to the or-pattern:
```rust
        | Outcome::Cancelled {
            last_text,
            iters,
            usage,
            ..
        } => (last_text.clone().unwrap_or_default(), *iters, usage.clone()),
```
(i.e. insert `| Outcome::Cancelled { last_text, iters, usage, .. }` between the `Stuck` alternative and the `=>`, formatted like the others).

(b) `bench_suite.rs`: this is a measurement tool, and a cancelled task is not a "wrong" one — the model never got to finish. After the `Ok(Ok(Outcome::Stuck { iters, usage, .. })) => { ("stuck", …) }` arm (~line 735-737) add:
```rust
        Ok(Ok(Outcome::Cancelled { iters, usage, .. })) => {
            ("cancelled", iters, usage.input_tokens, usage.output_tokens)
        }
```
and in the `let status = match (status_run, verified) { … }` block (~line 754-760), add a line **before** the final `(_, false) => "wrong",`:
```rust
        ("cancelled", false) => "cancelled",
```

- [ ] **Step 10: `crate-keeper`**

After the `Outcome::Stuck { iters, reason, .. } => { println!("\n✗ stuck after {iters} iteration(s): {reason}"); std::process::exit(2); }` arm (~line 154-157) add:
```rust
        Outcome::Cancelled { iters, .. } => {
            println!("\n✗ cancelled after {iters} iteration(s)");
            std::process::exit(2);
        }
```

- [ ] **Step 11: `deepseek-caps-e2e`**

In `outcome_text` (~line 75-82), replace
```rust
        Outcome::BudgetExhausted { last_text, .. } | Outcome::Stuck { last_text, .. } => {
            last_text.as_deref()
        }
```
with
```rust
        Outcome::BudgetExhausted { last_text, .. }
        | Outcome::Stuck { last_text, .. }
        | Outcome::Cancelled { last_text, .. } => last_text.as_deref(),
```

- [ ] **Step 12: Verify the gate passes and nothing regressed**

Run, in order:
```bash
cargo build --workspace --all-targets            # expected: clean, zero E0004
cargo clippy --workspace --all-targets -- -D warnings   # expected: clean
cargo fmt --all -- --check                       # expected: clean (run `cargo fmt --all` first if it reports your new arms)
cargo test -p harness-rs-loop 2>&1 | grep -E "^test result|FAILED"   # expected: all ok, unchanged counts
```
Then `cargo test --workspace 2>&1 | grep -E "FAILED|^test result: FAILED"`. Expected: **only** the three pre-existing doctest failures in `harness-core/src/redact/mod.rs`, `harness-tools/src/datetime/mod.rs`, `harness-tools/src/browser/policy.rs` (they fail on `main` too and are being fixed separately). Anything else failing is yours.

- [ ] **Step 13: Commit**

```bash
git add crates/harness-serve/src/service.rs crates/harness-cli/src/main.rs examples/ai-note/src/server.rs examples/cap/src/bin/cap.rs examples/cap/src/bin/cap-tui.rs examples/investor-bot/src/main.rs examples/personal-assistant/src/main.rs examples/eval-bench/src/main.rs examples/eval-bench/src/bench_suite.rs examples/crate-keeper/src/main.rs examples/deepseek-caps-e2e/src/main.rs
git commit -m "feat: every consumer of Outcome says \"cancelled\" rather than failing to build"
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

`async-trait` is already in `[dev-dependencies]` of `crates/harness-loop/Cargo.toml` (line 73); nothing to add.

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

Find the sequential tool loop, `for call in &out.tool_calls {`. Inside it (~line 1617-1622) the result is obtained and counted like this:

```rust
                let result = if let Some(r) = prefetched.remove(&action.call_id) {
                    r
                } else {
                    self.dispatch_bounded(&action, world).await
                };
                tools_called += 1;
```

Insert the block below **directly after `tools_called += 1;`** and before the `let result = ToolResult { content: self.shape_result(…` that follows it. (On a cancel the prefetch map is empty — Step 5 makes it so — so control always reaches `dispatch_bounded`, which returns immediately with the cancelled result, and `tools_called` has already been incremented, which is what the test asserts.)

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

### Task 5: The event fires exactly once, then SessionEnd — and reaches the feed and the trace

**Files:**
- Modify: `crates/harness-loop/tests/cancellation.rs`
- Modify (only if Step 2 fails): `crates/harness-loop/src/lib.rs`
- Modify: `crates/harness-loop/src/hooks/broadcast.rs` — `project()` (~line 120-160)
- Modify: `crates/harness-loop/src/telemetry.rs` — the `match ev` in `fire()` (~line 139-340)

**Why the last two are here.** Code review of Task 1 found that firing `Event::Cancelled` is not enough on its own: two consumers drop it on the floor. `BroadcastHook::project()` ends in `_ => return None`, and `matches()` is `project(ev).is_some()`, so an SSE client watching a run (`harness-serve`, `examples/ai-note`) would see the feed simply stop, never told the run was cancelled. And `TelemetryHook` has no end-state arm at all — `SessionEnd` writes `run.end` regardless of how the run ended — so a cancelled run's trace is indistinguishable from a completed one. Both are three-line fixes and both belong with the event, not in a later cleanup.

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

- [ ] **Step 4: Write the failing broadcast test**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 7. A cancel reaches the broadcast feed
// ------------------------------------------------------------------

use harness_loop::hooks::broadcast::BroadcastHook;

/// The SSE feed is how a UI watches a run. Before this, `project()` had no
/// arm for `Cancelled`, so a client saw the stream stop with no reason —
/// the exact failure the event's own doc comment warns against.
#[tokio::test]
async fn a_cancel_reaches_the_broadcast_feed() {
    let (_td, mut world) = tmp_workspace();
    let model = MockModel::new()
        .script(MockResponse::tool_call("slow", json!({})))
        .script(MockResponse::text("unreachable"));
    // Subscribe before the hook is handed to the loop: `subscribe` takes
    // `&self`, and the receiver outlives the hook independently.
    let hook = BroadcastHook::new(64);
    let mut rx = hook.subscribe();
    let token = CancellationToken::new();
    let fire = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        fire.cancel();
    });

    let _ = AgentLoop::new(model)
        .with_tool(Arc::new(SlowTool::new()))
        .with_hook(Arc::new(hook))
        .with_cancellation(token)
        .run_with_max_iters(task("call the slow tool"), &mut world, 5)
        .await
        .unwrap();

    // Drain what the feed carried. `try_recv` returns `Err(Empty)` once the
    // buffer is exhausted, which ends the loop.
    let mut names = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        names.push(ev.event);
    }
    let cancelled = names.iter().position(|n| *n == "Cancelled");
    let ended = names.iter().position(|n| *n == "SessionEnd");
    assert!(cancelled.is_some(), "Cancelled never reached the feed: {names:?}");
    assert!(ended.is_some(), "SessionEnd never reached the feed: {names:?}");
    assert!(cancelled < ended, "Cancelled must precede SessionEnd on the feed: {names:?}");
}
```

- [ ] **Step 5: Run it to verify it fails**

Run: `cargo test -p harness-rs-loop --test cancellation a_cancel_reaches_the_broadcast_feed`
Expected: FAIL with `Cancelled never reached the feed: [...]` — the list will contain `"SessionEnd"` but not `"Cancelled"`, because `project()` returns `None` for it and `matches()` therefore never lets it through.

- [ ] **Step 6: Project the event**

In `crates/harness-loop/src/hooks/broadcast.rs`, inside `fn project`, directly after the line `Event::SessionEnd => json!({}),` (~line 144), add:

```rust
        // No fields: the outcome carries the partial work, the feed only
        // needs to know the run was ended from outside rather than finished.
        Event::Cancelled => json!({}),
```

Nothing else changes: `matches()` is `project(ev).is_some()`, so this one arm is what makes the hook both match and forward the event.

- [ ] **Step 7: Run it to verify it passes**

Run: `cargo test -p harness-rs-loop --test cancellation a_cancel_reaches_the_broadcast_feed`
Expected: PASS.

- [ ] **Step 8: Write the failing telemetry test**

Append to `crates/harness-loop/tests/cancellation.rs`:

```rust
// ------------------------------------------------------------------
// 8. A cancel is recorded on the run trace
// ------------------------------------------------------------------

use std::io::Write;
use tracing_subscriber::fmt::MakeWriter;

/// A `tracing` writer that appends everything into a shared buffer.
/// (Copied from tests/telemetry.rs — test crates cannot share fixtures.)
#[derive(Clone)]
struct BufWriter(Arc<Mutex<Vec<u8>>>);
impl Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for BufWriter {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> BufWriter {
        self.clone()
    }
}

/// `SessionEnd` writes `run.end` however the run ended, so without its own
/// line a cancelled run's trace reads exactly like a finished one.
#[tokio::test]
async fn a_cancel_is_recorded_on_the_run_trace() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufWriter(buf.clone()))
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .finish();

    let output = {
        let _guard = tracing::subscriber::set_default(subscriber);
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

        let _ = AgentLoop::new(model)
            .with_tool(Arc::new(SlowTool::new()))
            .with_hook(Arc::new(harness_loop::TelemetryHook::new()))
            .with_cancellation(token)
            .run_with_max_iters(task("call the slow tool"), &mut world, 5)
            .await
            .unwrap();

        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    };

    assert!(output.contains("run.cancelled"), "missing run.cancelled:\n{output}");
    assert!(output.contains("run.end"), "run.end must still close the trace:\n{output}");
    let cancelled = output.find("run.cancelled").unwrap();
    let ended = output.find("run.end").unwrap();
    assert!(cancelled < ended, "run.cancelled must be recorded before run.end:\n{output}");
}
```

`tracing-subscriber` is already a dev-dependency of the loop crate (used by `tests/telemetry.rs`), and `tracing` is a normal dependency, so nothing is added to `Cargo.toml`.

- [ ] **Step 9: Run it to verify it fails**

Run: `cargo test -p harness-rs-loop --test cancellation a_cancel_is_recorded_on_the_run_trace`
Expected: FAIL with `missing run.cancelled:` followed by the captured output, which will contain `run.start` and `run.end` but no `run.cancelled`.

- [ ] **Step 10: Record the cancel on the span**

In `crates/harness-loop/src/telemetry.rs`, inside `fn fire`'s `match ev`, immediately **before** the `Event::SessionEnd => {` arm (~line 312), add:

```rust
            Event::Cancelled => self.in_run(|| {
                // Warn, not info: a cancel is the person deciding the run was
                // not worth finishing, which is worth seeing in a trace that
                // would otherwise look like any other run.end.
                tracing::warn!(target: "harness.telemetry", event = "run.cancelled");
            }),
```

`in_run` scopes the event to the run's span (if any), the same way `BudgetWarning` does two arms above.

- [ ] **Step 11: Run the tests to verify they pass**

Run: `cargo test -p harness-rs-loop --test cancellation`
Expected: `test result: ok. 8 passed`.

- [ ] **Step 12: Run the whole loop crate**

Run: `cargo test -p harness-rs-loop`
Expected: green. `tests/telemetry.rs` asserts on the shape of `run.start`/`run.end` and is unaffected by an extra line; the broadcast hook's own unit tests (in `hooks/broadcast.rs`) count projected events for specific inputs and do not include `Cancelled`.

- [ ] **Step 13: Commit**

```bash
git add crates/harness-loop/src/hooks/broadcast.rs crates/harness-loop/src/telemetry.rs crates/harness-loop/tests/cancellation.rs
git commit -m "feat(loop): a cancel reaches the broadcast feed and the run trace"
```

---

### Task 6: Changelog

**Files:**
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add the entry**

Open `CHANGELOG.md`. There is **no** `## Unreleased` section yet — the file goes straight from its intro paragraph to `## 0.0.62` (line 6). Insert the following directly above the `## 0.0.62` line, leaving one blank line after the intro paragraph and one before `## 0.0.62`:

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
