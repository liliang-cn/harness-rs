# Agent-loop observability — structured chat logging

**Date:** 2026-05-28
**Status:** design approved, pending implementation plan

## Goal

Make dashboard chat runs diagnosable from the server log. Today a chat request
produces almost no output (the chat handler logs one line, only on a memory-open
failure), so a silent failure — e.g. `gemini-3.5-flash` returning an empty reply
with no tool call, no text, and no error — leaves **zero trace** in
`journalctl -u dashboard`. This adds always-on structured `tracing` along the
chat path so every run leaves a readable trail.

## Approach

Add `tracing` calls in `examples/dashboard/src/server.rs` only — the live chat
path (`session_stream_handler` + its spawned task) and the `ChannelHook`.
`tracing` is already initialised in `main.rs:683` (RUST_LOG-driven, default
`info`, to stderr → journald). **No new deps, no files, no disk management.**

## What gets logged (per chat request)

1. **Request start** — INFO when the spawned task begins:
   `user`, `model`, `session`, `msg_len`. (Short ids.)
2. **Tool calls** — in `ChannelHook::fire`:
   - `PreToolUse` → INFO `tool start name=…`
   - `PostToolUse` → INFO `tool end name=… ok=…`, or **WARN when `ok == false`**.
   (No per-tool duration in v1 — the hook is `&self`; interior-mutability timing
   isn't worth it yet.)
3. **Outcome** — after `loop_.run_with_max_iters(...)`:
   - `Outcome::Done` with a non-empty reply OR ≥1 artifact → INFO
     `chat done iters=… in_tokens=… out_tokens=… reply_len=… artifacts=…`.
   - `Outcome::Done` with **empty reply AND no artifacts → WARN
     `chat empty reply`** (model + iters + token counts) — today's silent case.
   - `Outcome::BudgetExhausted` → WARN `chat budget exhausted` (+ iters/tokens).
   - model-build error / run `Err` → ERROR with the reason.
4. **Correlation** — wrap the run in a `tracing` span (`info_span!("chat", session=…)`)
   entered for the spawned task so all lines for one request group together.

## Deeper debugging lever (documented, not code)

To chase *why* a model returned empty (e.g. gemini safety / empty candidate /
malformed chunk), run with `RUST_LOG=info,harness_models=debug`. The
`harness_models` layer already emits some tracing (the observed
`WARN gemini bytes chunk not utf-8` came from there); debug level surfaces more
of the raw response / finish reason. This is an ops lever — document it in the
deploy notes; no code change.

## Testability

Extract a small pure helper so the classification is unit-testable without
asserting on log output:

```rust
enum ChatLog { Done, Empty, Budget }
/// Decide how a finished chat turn should be logged.
fn classify_outcome(reply: &str, artifact_count: usize, budget_exhausted: bool) -> ChatLog {
    if budget_exhausted { ChatLog::Budget }
    else if reply.trim().is_empty() && artifact_count == 0 { ChatLog::Empty }
    else { ChatLog::Done }
}
```

Tests: empty + 0 artifacts → `Empty`; empty + 1 artifact → `Done`; non-empty →
`Done`; budget flag → `Budget`. The handler calls `classify_outcome` then emits
the matching INFO/WARN. (`cargo test -p dashboard`.)

## Non-goals

- **No** SessionRecorder / per-session JSONL (considered and declined — adds a
  file per chat + disk management; the always-on log covers the need).
- The legacy `chat_stream_handler` is out of scope (the UI uses
  `session_stream_handler`); may get the same treatment later if needed.
- No log shipping / rotation / dashboards — journald already handles retention.

## Verification

- `cargo build -p dashboard` clean; `cargo test -p dashboard` (the
  `classify_outcome` tests pass).
- Manual: boot locally, run a chat while `journalctl`/stderr is tailed; confirm
  the request → tool → outcome lines appear, and that an empty reply logs the
  WARN line with model + token counts.

## Rollout

Backend-only (`server.rs`); no UI/dist change → no `npm run build` needed, but
still rebuild via the musl flow and `touch server.rs` is implicit (it's edited).
Deploy to qc-jp as usual.
