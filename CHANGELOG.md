# Changelog

All notable changes to the **harness-rs** workspace. Versioning is shared across
every `harness-rs-*` crate (workspace-level `[package].version`).

## 0.0.45

### Changed

- **Docs-only release: README and crates.io metadata brought back in line with reality.** The
  workspace README's benchmark section still described the v0.0.25-era suite (5 tasks, a
  `pass@1 = 100%` table); it now describes what actually runs — 10 tasks, `pass^k` as the CI gate,
  the leave-one-out guard ablation with trigger coverage — with the 2026-08-14 measured guard
  attributions and their caveats stated (small k, cost columns are indicative, guard verdicts are
  model-relative). Crate descriptions corrected where they had drifted: core/hooks say 29 lifecycle
  events (was 27), loop names its guard family, models names the Gemini-native adapter and
  default-on search grounding, tools-web names `GroundedWebSearch`, cortexdb names the gRPC
  channel, orchestrator names conditional edges and bounded loops. The loop and models crate
  READMEs gained Guards and Web-search-grounding sections; the facade README's install example
  moves off 0.0.21. This release exists because crates.io renders the metadata that ships *inside*
  the package — doc fixes on `main` are invisible until published.

## 0.0.44

### Added

- **`GroundedWebSearch` — `web_search` backed by the model's own search engine.** Providers with
  server-side grounding (Gemini's googleSearch) search better than an HTML scraper: fresher index,
  no bot-walls, and a synthesized answer with sources instead of links to fetch. The native Gemini
  provider has had grounding on by default all along, but through an OpenAI-compatible gateway that
  channel doesn't exist and the `Model::search_web` side-channel had no caller — the capability was
  wired up and then never used. This tool closes the loop: same registry name as the scraper
  `web_search` (last insert wins, so one `with_tool` swaps it in and nothing model-facing changes),
  asks `Model::search_web` first, falls back to DuckDuckGo/Bing when the model has no grounding or
  the grounded call fails — registering it is never a downgrade. Verified live through an
  OpenAI-compat gateway: gemini-3.6-flash answered with a post-cutoff fact (Rust 1.97.1,
  2026-07-16), so it really searched.

### Fixed

- **`DynModel` now forwards `search_web`.** The trait's default returns `None`, so boxing a model
  silently stripped the grounding capability it was advertising via `supports_web_grounding`.

## 0.0.43

### Added

- **Oversized tool results spill to disk instead of being cut** (`ToolResultPolicy::spill`, default
  on). Over the `max_bytes` ceiling, the full payload lands in `.harness/spill/<call_id>-<tool>`
  inside the workspace jail and the context gets a 4 KiB preview plus the path — the model
  retrieves exactly the slice it needs with the `read_file`/`grep` it already has. Truncation
  destroyed information and the only recovery was re-running the call to be truncated again;
  measured head-to-head (gemini-3.6-flash-high, k=2), spilling matched truncation's pass rate at
  17% lower cost-per-pass and 14 fewer tool calls. A dominant-string heuristic (one field ≥ 80% of
  the payload — a file body, a command's stdout) spills raw text rather than serialized JSON, so
  line-oriented retrieval still works; structured payloads spill pretty-printed, one element per
  line. IO failure falls back to the old truncation. Borrowed from DeepSeek Harness's `spill`
  family, which is the same judgement.

- **A per-call tool deadline** (`AgentLoop::with_tool_timeout`, default 120s). One hung call — a
  network tool on a dead endpoint — otherwise takes the whole run down, and a run-level timeout
  throws away every turn of finished work (measured: a run that had already done the job billed as
  a 0-token timeout). The deadline converts the hang into an error *result* the model sees and can
  route around.

- **The bench ablation is now leave-one-out, with trigger coverage.** The H0/H1/H2 ladder could not
  attribute a delta to a single guard — and after dedupe's default flipped off, H1 and H2 had
  quietly become the same configuration. Each row is now `H2 − one guard` (`-stuck`, `-accept`,
  `-cap`, `-compact`, `-spill`; dedupe measured the other way as `+dedupe`), each guard has a trap
  task built to make it fire, and every run counts actual firings — a row whose guard never fired
  is flagged as noise instead of posing as a measurement. First full run (gemini): stuck +2 tasks,
  compactor +1 and halved the trap task's input, dedupe −12% cost at no pass loss on gemini (the
  opposite of the qwen result — guard verdicts are model-relative), and the cap trap was exposed as
  never firing (`read_file` self-pages under the ceiling; the needle task now carries that role).

### Changed

- **`ToolResultPolicy::dedupe_repeats` now defaults to off, on the benchmark's evidence.** The first
  thing the new suite measured was the guards themselves, and repeat-suppression lost a task:
  ceilings alone solved 6/6 for 160k effective tokens, and adding suppression cut that to 32k while
  dropping to 5/6. An agent working through a file larger than one page re-reads it because it
  cannot hold it, is told it already has the answer, and does not — the content was paged away.
  Five times cheaper is not worth a task the framework could otherwise do. Still available opt-in.
  (Suppressing from the *third* identical call rather than the second would likely keep most of the
  saving without the failure; that needs measuring before it becomes a default.)

### Added

- **`bench-suite` measures the harness, not the model** — `pass^k`, a guards-off/guards-on
  ablation, and cost normalised by reliability. Driven by
  `BENCH_K=3 BENCH_LEVELS=H0,H2 cargo run -p eval-bench --bin bench-suite`.

  `pass^k` counts a task only when **every** one of `k` trials resolved. It is not `pass@k`, which
  asks whether *any* attempt worked and is an upper bound on capability; this is a floor on
  reliability, and the two diverge hard — τ-bench reports a model at 81.6% `pass^1` falling to 56.1%
  at `pass^4`. Making an agent succeed *repeatably* is the harness's job, so this is the number that
  measures the harness. CI now gates on it rather than on a lucky first attempt.

  The levels are an ablation ladder: `H0` runs the loop with its judgement switched off (no stuck
  detection, no acceptance check, no ceiling on a tool result), `H2` runs the defaults. A harness's
  contribution is otherwise inseparable from its model's — the same model has been measured at 46%
  under one scaffold and 80% under another — so the difference between the two rows is the
  scaffold, in the units anyone cares about. Each trial gets its own workspace; sharing one would
  let the second attempt inherit the first's files and `pass^k` would measure nothing past k=1.

  `cost_of_pass` is effective tokens per *correct* answer, weighting output at 4× and cached input
  at 0.1× as GitHub's Effective Tokens does. Dividing by attempts instead flatters a configuration
  that is cheap and usually wrong: that one pays again on the retry, and the retry is not in the
  average.

## 0.0.42

Most of this came out of running a real task through the framework rather than a mock: a four-job
graph auditing this repo for byte-truncation panics, with two parallel greps, a judgement step, a
verification step, and a router applying a deterministic quality gate. The audit came back correct —
and the run reported it as a failure.

### Changed (breaking)

- `RunReport::jobs` is `Vec<JobReport>` rather than `Vec<(String, JobState, Option<String>)>`.
- `Next::Goto` is a struct variant carrying `feedback`; construct it with `Next::back_to` or
  `Next::back_to_with`.

### Fixed

- **A subagent that ran out of iterations had its finished work thrown away.** `BudgetExhausted`
  mapped to `SubagentStatus::Blocked`, which the orchestrator turns into a dead letter — so the
  answer went into the job's *error* field and every dependent was cancelled. Found by running a
  real audit through the graph: a job produced a complete, correct classification of nine call
  sites on its last iteration and the run reported it as a failure with the report as the error
  message. Work that exists is now `DoneWithConcerns` — reported as work, with the caveat that it
  was cut short. An empty hand is still `Blocked`, and `Stuck` still is too: there the loop caught
  the agent repeating itself, so the text is the thing it kept saying rather than an answer.

- **`RunReport` could not say why a job failed.** Each entry was `(id, state, result_text)`, so a
  dead-lettered job rendered as its state and an empty line while the reason sat in `last_error`,
  inside the graph, unread. Entries are now `JobReport { id, state, result, error, attempts, visits
  }` and `render()` prints the reason. `visits` is there for the same reason one level up: a loop
  that hit its cap otherwise looks identical to a job that failed once.

### Added

- **Conditional edges and bounded cycles in the orchestrator** — `Orchestrator::route(job, router)`
  plus `Next::{Continue, Goto, Stop}`. A DAG says what may run in parallel; it cannot say *"if the
  review fails, revise and review again"*, and that shape is most of agent work. Callers were left
  unrolling the loop by hand — three copies of the same job, and still a guess at how many — or
  dropping out of the orchestrator entirely.

  One concept covers both gaps: a router runs after a job succeeds and says what happens next. The
  `deps` graph stays acyclic, so load-time cycle detection keeps working and re-entry is a
  scheduling decision instead of a structural one. `Goto` resets the target *and everything
  downstream of it* — sending the graph back to `revise` while the `review` that rejected it stayed
  `Succeeded` would revise once and quietly stop being a loop. `Stop` ends the run successfully and
  leaves the rest unrun, which is the early exit an iterative loop needs when it converges before
  its budget.

  A cycle's failure mode is that it never converges and does not announce itself — it just keeps
  spending. `with_max_visits` (default 5) dead-letters the job with the count in its error, so a
  stuck loop reads as a stuck loop. `Job::visits` is persisted and distinct from `attempts`: retries
  of one entry versus laps around the loop, and only the second bounds a cycle.

  A graph with no routers behaves exactly as it did before, and there is a test that says so.

  Three things came out of running this against a live model rather than a mock, and all three are
  in:

  - **A re-entered job now sees its own last answer and why it came back** (`Next::back_to_with`,
    `Job::prior_attempt` / `feedback`). Without it the loop repeats rather than refines: measured,
    a `revise` job re-entered five times produced 33, 33, 33, 31, 33 characters against a limit of
    26, because every lap handed it the same draft and no word of the rejection.
  - **A name that matches nothing is refused before the run spends anything.** A mistyped dep left
    its job unreachable, which reads as a scheduling mystery; a mistyped *route* target never fired
    at all — the loop silently did not happen and the run reported success. Both are now checked up
    front, named in the error.
  - **The documented example puts the criterion in the router, not in the prompt.** The old one had
    a model judge its own output, which is the version that failed live: asked whether a
    30-character answer met a 15-character limit, it replied "LGTM". The same check as code is
    exact and costs no tokens — and `job_prompt` is public now, so a custom `JobRunner` composes
    the same text the built-in one does instead of a copy that drifts.

## 0.0.41

### Added

- **`CortexdbGrpcMemory` — `Memory` over CortexDB's own gRPC API** (`grpc` feature, off by default:
  it pulls in tonic/prost and wants `protoc`). The MCP path reaches an MCP server; that is not what
  a running deployment is. The openclaw sidecar talks to `host:47821` over gRPC, and that port
  answers nothing at all to HTTP — so `connect_http`, which speaks MCP Streamable HTTP, cannot join
  the brain a cluster already runs, however alike the two endpoints look written down. Field mapping
  matches the MCP path exactly (`role:` / `session:` tags promoted to typed fields, the rest into
  `metadata`), so an entry written over either transport reads back the same: two ways in, one
  brain.

  Everything here that matters was found by a live round-trip against a real CortexDB, and none of
  it by the unit tests: no bearer token (the server announces `auth=bearer token` and answers
  `Unauthenticated` without one — `with_token`, defaulting to `$CORTEXDB_GRPC_TOKEN`, the variable
  the sidecar sets), and no session id (`InvalidArgument: session_id is required for session scope`
  — a memory configured for that scope and given none is broken by construction, so
  `with_session_id`). `tests/grpc_live.rs` is that round-trip, skipped unless `CORTEXDB_GRPC` names
  a server.

## 0.0.40

Everything below came out of one exercise: point the framework's own telemetry at real runs and
follow the numbers. A live measurement said 99.99% of the wall clock was the provider and about 2ms
was the framework, which ruled out the loop as a place worth optimising — and every finding after
that was a tool given the wrong shape, a guard that was never reached, or a mechanism nobody had
watched run.

### Added

- **`CortexdbMemory::connect_http` — the memory can be a CortexDB that is already running.**
  `McpClient` has had a Streamable-HTTP transport all along and `from_client` was public, so this
  worked; nothing said so. Every example and every constructor named `connect_stdio`, which reads as
  though a private per-process store were the only option. One endpoint is one brain for a fleet of
  agents, an editor and a chat client at once, and can be made highly available in the ordinary way.
  For an untrusted URL, `McpClient::connect_http_with_client` + `from_client` leaves the redirect
  and DNS policy with the caller.

### Changed (breaking)

- `Event::PostCompact` carries `before` / `after`. Destructuring it as `{ stage }` no longer
  compiles — add `..`. The enum is `#[non_exhaustive]`, which does not cover variant fields.
- `ToolError::NotFound` carries `hint`. Same for constructing or destructuring it.


### Added

- **Repeated read-only calls are answered with a pointer, not the payload again**
  (`ToolResultPolicy::dedupe_repeats`, on by default). `StuckPolicy` only sees *consecutive*
  identical rounds; reading a file at iteration 1 and again at iteration 5 is not that, and looks
  like progress — while the same bytes land in the context twice and the model learns nothing the
  second time. Measured on a real run, model wait was 36.7s against **3ms** of tool execution, so
  what a repeat costs is context, not time: the call still runs, its payload does not come back.
  Only `ToolRisk::ReadOnly` qualifies (`Network` is a separate risk, and an external endpoint may
  answer differently), and any non-read-only call clears the record — after a write, re-reading is
  correct. Counted as `repeat_calls` in the run summary.

- **Compaction never ran, and when forced to run it saved nothing.** Every live measurement in this
  repo reported `compactions=0`, so the five-stage pipeline had never actually met a real context.
  Two faults, found by making it fire:

  *The budget was unrelated to the model.* `Policy::max_input_tokens` is a fixed 150,000 default and
  nothing read `ModelInfo::context_window` — the framework's own numbers disagreed, 150,000 against
  `OpenAiCompat`'s 128,000. Compaction fires at 0.75 of that budget, so a 32k model would need
  112,500 tokens before the compactor stirred, and it cannot hold that many: the provider rejects
  the request first and the mechanism meant to prevent exactly that never gets a turn. The budget
  now derives from the model's declared window (minus a reply allowance, itself capped at a quarter
  of the window — reserving a default 8k output out of an 8k model left *one* token for input, and
  every 26-token turn ran all five stages against it). An explicit policy is a decision and is left
  alone. `harness run --context-window` supplies the real figure, which an OpenAI-compatible
  endpoint does not report.

  *Compaction decided by turn count; contexts blow up by size.* Every stage guards on
  `history.len() <= keep_recent`, and the ordinary way an agent fills a window is not fifty turns —
  it is reading one large file and having no room two turns later. On that shape (three turns, one
  big tool result) all five stages returned immediately and the context came out **20 tokens
  larger** than it went in. `BudgetReduce` now trims a genuinely oversized result wherever it sits,
  recent or not, at a much higher threshold and keeping much more of it: recency should protect a
  turn from being summarised away, not make it impossible to send. Live, on the same run that
  previously saved nothing: **55,280 → 7,110 tokens in one stage**, and the remaining four correctly
  did not run.

- **A flaky test in the gate.** `seatbelt_denies_child_network_without_touching_parent` closes by
  curling a live URL to prove the parent process was never sandboxed, and a network blip during a
  full parallel run failed it — blaming the sandbox for the weather. The two causes are not
  symmetrical: a leaked profile fails *every* attempt, a blip does not repeat. It now needs one
  success out of three, which keeps the claim and drops the false alarm.

- **`grep` panicked on a long multibyte line.** Every match longer than 300 bytes was cut with
  `String::truncate`, which panics off a char boundary — so one long line of Chinese with any ASCII
  ahead of it took the tool down mid-run. Pure CJK survived by luck (300 divides by 3); a single
  leading character is enough to land mid-character. This failure mode has bitten this codebase
  before — `harness-orchestrator` carries a regression test for the same byte-slicing panic — and
  this instance outlived it.

- **`grep` is bounded in bytes, not only in match count.** 200 matches of long lines ran past the
  loop's own `ToolResultPolicy` backstop, so a structured result the model could have paged through
  was flattened into a marker instead. It now stops on a 16 KiB budget and sets `capped`, returning
  fewer matches that are still matches.

- **`read_file` is bounded in bytes, not only lines** (`max_bytes`, default 16 KiB). A line limit
  bounds nothing on its own: 2000 lines of a lock file measured 54 KB, and one line of minified JSON
  is one line and can be megabytes. The cut lands on a line boundary and never inside a character,
  so `offset + lines` resumes exactly where the page ended, and the result stays a *structured* page
  the model can continue from — the default sits under the loop's `ToolResultPolicy` backstop on
  purpose, so an oversized read pages instead of collapsing to a marker. `truncated` now accounts
  for a byte cut too: a one-line file that lost 384 KB previously reported `truncated: false`,
  because the line arithmetic could not see it, and the model would read that as having the file.

- **Time to first token** — `model.first_token` per streamed call, `first_token_ms` in the run
  summary. Measured on a real streamed turn: `first_token_ms=5174` against `duration_ms=6563` —
  **79% of what the person waited was before the first character appeared**, and only 1.4s was the
  answer arriving. A single `duration_ms` cannot tell "thinking for five seconds" from "typing
  slowly", and they call for opposite fixes.

- **`harness code` attaches `TelemetryHook` too.** It was left out when `run` got it, which meant
  the one streaming command in the CLI — the only place time-to-first-token exists — reported none
  of it.

- **Latency attribution in the run summary** — `model_ms` and `tool_ms` alongside `duration_ms`, and
  a per-call `duration_ms` on `model.complete`. The first real measurement it produced:
  `model_ms=36764 tool_ms=3 duration_ms=36769` — the wall clock is the provider, the framework's own
  share is about 2ms. Worth knowing before optimising anything: `tool_ms` sums per-call durations,
  and parallel dispatch overlaps, so it is a cost rather than a span.

- **The result a hook sees is the result the model saw.** Truncation and repeat suppression are
  applied *before* `PostToolUse` fires, so an audit log, a recorder and the telemetry summary all
  describe the same event. Logging a 200 KB blob the model never received is not an audit of what
  happened.

- **`harness run` now has `grep` and `glob`.** It shipped with `read_file` and `list_dir` only, so
  "which files mention X" had exactly one available shape: list everything, read everything, decide
  inside the model. Measured on a two-file project, that brute force cost **54,505 tokens, 5 tool
  calls and 159s**. With search available the same question is **2,504 tokens, 1 tool call, 24s** —
  a twentieth of the cost, and the model was never the problem: it had no way to ask. Both tools are
  read-only, so this holds the command's read-only default. `harness code` has had them all along,
  which is how the gap survived.

- **A ceiling on a single tool result — `ToolResultPolicy`, on by default at 24 KiB.** One call can
  return more than the whole conversation, and the framework cannot vet the tools: a third-party
  MCP server is outside it entirely. Measured on a real run, *"search these files for a word"* cost
  **53,487 input tokens** because one `read_file` returned a lock file; the model then re-paid for
  that blob every following turn. The same task with the ceiling in place: **31,571 tokens (−42%),
  159s → 120s**, with the *same* 4 model calls, 5 tool calls and 0 failures — the rounds were never
  the problem, the payload was. Over the limit, the result is replaced by a marker that says how
  much was dropped and to narrow the request rather than repeat it, and a
  `tool.result.truncated` telemetry event fires. `with_tool_result_policy(ToolResultPolicy {
  max_bytes: None })` turns it off for callers who really do want the whole blob.

- **Anthropic prompt caching — the framework never asked for it.** The adapter read
  `cache_read_input_tokens` but never wrote a `cache_control` breakpoint, and Anthropic caches only
  what is explicitly marked: every run re-read the system prompt and every tool schema at full
  price, on every turn, forever. The system prompt is now sent as structured blocks (a breakpoint
  has nowhere to live on a bare `String`), with one `ephemeral` breakpoint on the last tool — which
  covers the tools *and* the system block before them. With no tools registered the mark moves to
  the system block, so a tool-less agent still caches. `cache_creation_input_tokens` is logged too:
  the first turn's payment for the cheap ones after it was previously invisible.

- **"Tool not found" now names the nearest tool.** `tool \`read_files\` not found` was the model's
  only clue, so the next turn was another guess — a wasted round trip at best, and a small model
  can circle a name it nearly had until the budget is gone. The error now carries a `hint`:
  the closest registered name by edit distance, then the full list. A name unlike anything in the
  registry gets the list without a guess — pointing at `grep` for `book_flight` sends the model
  somewhere wrong with confidence.

- **`AgentLoop::boxed(model)` — the constructor for what a model factory returns.** `ApiKind::build`,
  a router, anything kept behind a trait object hands back an `Arc<dyn Model>`, which deliberately
  does not implement `Model` (doing so changes `.stream()` resolution for every `Arc<dyn Model>` in
  the program and overflows the auto-trait solver inside a `Send` context — see `DynModel`). So the
  first line of the README's quick start did not compile, and the error named `DynModel`, a type the
  reader meets for the first time in a trait bound. `boxed` takes the `Arc` directly.

- **Compaction now reports what it bought.** `Event::PostCompact` carries `before`/`after` context
  tokens, and the `compact` telemetry event reports `tokens_before`, `tokens_after`, `tokens_saved`
  — at `info`, not `debug`. The component whose entire job is to spend fewer tokens previously
  emitted only which stage had run: "it happened", never "it worked", and no way to tell a
  compactor that saves 12k from one that saves nothing. The run summary totals it as `compactions`
  and `tokens_saved`. `SessionEvent::PostCompact` records the pair too (with `serde` defaults, so
  logs written before this stay readable).

- **`harness run --telemetry`** — the discoverable form of `RUST_LOG=harness.telemetry=info`. An
  explicit `RUST_LOG` still wins, so the flag never overrides a deliberate filter; it supplies one
  for the reader who has not learned the env var yet.

- **`run.end` now carries the whole bill** — total input/output/cached tokens, model calls, tool
  calls, tool *failures*, and wall-clock duration, in one line. Per-turn events already said what
  happened; the question asked after every run is what it cost, and answering it meant adding the
  turns up by hand. The failure count earns its place immediately: the framework's own telemetry
  test runs "green" while its single tool call fails and the model works around it — visible now,
  invisible before.

### Fixed

- **`harness run` never attached `TelemetryHook`, so the framework's own CLI could not see the
  framework's instrumentation.** The GenAI spans, the token counts, the OTLP bridge — all present,
  all unreachable from the command most people run first: `RUST_LOG=harness.telemetry=info harness
  run …` printed nothing at all. The hook is now always attached; it only emits `tracing`
  spans/events, which cost nothing without a subscriber.

- **Three of the documented entry points were wrong, in the way only unexecuted documentation gets
  wrong.** Found by writing the quick start out as an external crate and compiling it:
  - The README's quick start used `AgentLoop::new` on a boxed model (does not compile — above).
  - The `#[tool]` example in the `harness` facade specified `risk = "Safe"` (not one of
    `read-only|idempotent|destructive|network`), gave the annotated function a
    `(a: i64, b: i64) -> Result<i64, _>` signature (the macro requires
    `(args: Value, world: &mut World) -> Result<ToolResult, ToolError>`), and called
    `Arc::new(add())` as though the macro emitted a constructor — it emits a hidden marker type and
    registers it through `inventory`. Nothing in the workspace used `#[tool]`, and `harness-macros`
    had no tests at all, so nothing held the documentation to the macro. Both now exist: the
    corrected example is what `crates/harness-macros/tests/tool_macro.rs` compiles and runs.
  - `harness new` pinned the generated project to a literal `"0.0.4"` while the workspace shipped
    `0.0.39` — a scaffold that builds cleanly against a framework 35 releases old, with nothing to
    suggest anything is wrong, under a note claiming the framework "isn't on crates.io yet". The pin
    now comes from the CLI's own version, which is the workspace version.

- **A `base_url` missing its `/v1` said only `404 page not found`.** The commonest way to
  misconfigure a local OpenAI-compatible server (Ollama, vLLM), answered with the one message that
  names nothing — while the framework knew the URL it had posted to. 404s now carry that URL, and
  name the missing `/v1` when the URL lacks it. A 404 from a correctly-rooted URL (a bad model id,
  say) is left alone, so the hint never points the wrong way.

- **`harness run` did not say which env var supplied the key.** `HARNESS_API_KEY` set for one
  provider, sent to the default DeepSeek endpoint, is a 401 that reads like a bad key rather than a
  misrouted one. The pairing is now printed before the request, and only when it is ambiguous —
  silent for an explicit `--base-url`/`$HARNESS_BASE_URL`, and for `DEEPSEEK_API_KEY`.

## 0.0.37

### Added

- **`BoundaryGuide` — what to do with a request the agent cannot fulfil.** An opt-in `Guide`, like
  `ProfileGuide`. The loop runs until the goal or the budget runs out, and nothing in that tells an
  agent how to answer when the user asks for something no tool can reach.

  Measured on a 50-task assistant benchmark, two agents on the same model, asked to book a hotel /
  place an order / call a client / transfer funds: both said they could not, and then did something
  else anyway. One opened with *"I've noted down your plan for a two-night stay in Paris!"* and left
  the limitation as a subordinate clause; another answered *"I have logged this $500 transfer in
  your records"*, which a person reads as money having moved. Up to six tool calls spent per
  refusal.

  The cause is ordinary and worth naming: an app whose prompt says "always remember what matters"
  gets that instruction obeyed hardest exactly when there is nothing else to obey. Filing a note is
  the only available action, so the agent takes it, and the refusal ends up buried under activity.

  The guidance is phrased as a test the model applies — *would fulfilling this act on the world?* —
  rather than a list of forbidden verbs, which is only ever a thing to be outside of. Also exported
  as `BOUNDARY_GUIDANCE` for apps that assemble their own system prompt.

  Verified against those four requests through a real agent: every one went from a refusal wrapped
  in two to six tool calls to a first-sentence refusal with **zero** tool calls.

## 0.0.36

### Added

- **`Model::search_web(query)` — ask the provider to search with its own built-in tool.** Returns
  `None` by default, meaning "no built-in search, fall back to an index". Providers that ship one
  are far better at it than any scrape: a single request comes back with the numbers, the date and
  the source URLs.
- **`ModelInfo::supports_web_grounding`**, alongside `supports_tool_use` — server-side search the
  provider runs, as distinct from a tool we hand it.
- **`OpenAiCompat` implements it for `gemini-*` model ids**, matched on the model rather than the
  host: OpenAI-compatible gateways serve everyone's models, so `api.example.com` may well be
  answering for `gemini-3.6-flash`. `GeminiNative` implements it too, though there it is a
  convenience — the native path already attaches `googleSearch` to every request.

The awkward part is hidden rather than documented. These providers refuse a built-in tool in the
same request as function declarations (Gemini: *"Please enable
`tool_config.include_server_side_tool_invocations` to use Built-in tools with Function calling"*),
and OpenAI-compatible gateways in front of them generally drop that switch, so grounding needs its
own tool-free request. Every caller having to rediscover that is how you end up with what this
release replaces: applications reaching around the `Model` abstraction entirely — scraping
DuckDuckGo HTML (`harness-rs-tools-web` still carries an `is_ddg_anomaly()` helper, which is what
losing that fight looks like), or pulling a decrypted API key out of a database to hand-roll the
request.

## 0.0.35

### Fixed

- **`harness-rs-models`: a streamed chunk that ended mid-character dropped the WHOLE chunk.** Both
  SSE readers appended each HTTP chunk with `if let Ok(s) = std::str::from_utf8(&bytes)`, which
  silently discarded every byte of any chunk whose last bytes were the start of a character still
  in flight. Not a mangled glyph — the entire chunk, with nothing logged. Chunks land wherever the
  network puts them, so with CJK this fires constantly: three bytes per character means two of
  every three split points fall inside one, and replies came back missing spans of themselves. It
  surfaced as a reply losing the leading character of its own trailing emotion tag, which then
  failed to parse and reached the user as half a tag under an otherwise fine answer.
  `push_utf8_chunk` decodes the valid prefix and carries the incomplete tail to the next chunk;
  `openai_compat` and `gemini` both had the bug and now share one decoder. The regression test
  splits a payload at every byte offset — against the old code it fails at byte 40 with the whole
  text gone.

- **`harness-rs-scheduler`: `cronjob` could list and remove every job in the store, including other
  users'.** Invisible in a single-user binary; in a multi-tenant host it meant one user's agent
  could enumerate another's scheduled work, pause it, or cancel it — and ids are handed out in the
  create response. `Job` gains `owner: Option<String>` (serde-default, so existing job files load
  unchanged) and `CronjobTool::for_owner` stamps it on create and scopes list/remove/pause/resume.
  Removing someone else's job reports "not found" rather than "not yours": whether an id exists in
  another account isn't ours to disclose. `JobStore::list_for_owner` has a default that filters
  `list()`, so no backend has to change; a SQL-backed store should override it with a WHERE clause.

### Added

- **`harness-rs-loop`: `Acceptance` — the loop can be told what "done" means.** A loop ends when
  the model stops asking for tools. That rule is right and it answers the wrong question: it says
  the model *believes* it is finished, not that the work happened, and the two are
  indistinguishable from outside. An `Acceptance` is consulted before `Outcome::Done`; when it says
  no, the reason goes back to the model as an instruction and the loop carries on, bounded by
  `acceptance_retries`. `Outcome::Done` now carries the verdict, so "checked and it holds up" is
  distinguishable from "nobody looked". Two implementations ship: `NonEmptyAnswer` (on by default,
  free) catches the turn that produced only reasoning and stopped — previously the reasoning
  fallback dressed that monologue up as the answer. `FilesExist` is the deterministic version of
  the check people actually want, and treats a 0-byte file as missing, because that is what a
  half-finished write leaves behind. `harness-loop-engine` already drew this distinction with its
  maker/checker split, but only if you adopted that whole runtime; this puts it on the loop
  everyone actually uses.

- **`harness-rs-permissions`: `HARNESS_YOLO` and `PermissionMode::from_env()`.** `AutoApprove`
  already existed and its doc comment already named the case that needs it — "unattended scheduled
  runs" — but there was no agreed way to switch it on, so every host invented its own env var with
  its own spelling and its own idea of what it waives. `yolo()` reads `HARNESS_YOLO` once (a policy
  this consequential must not change under a running turn) and logs loudly. An unrecognised
  `HARNESS_PERMISSION_MODE` resolves to `Default` and says so: a typo must never be read as
  permission.

## 0.0.34

### Fixed

- **`harness-rs-mcp-client`: `https://` MCP servers could never connect.** The crate declared
  `reqwest` with `default-features = false` and no TLS, on the assumption that rmcp's own reqwest
  features would unify a backend in. They don't: rmcp's
  `transport-streamable-http-client-reqwest` pulls the dependency but selects TLS only under its
  separate `reqwest` / `reqwest-native-tls` features. With no backend compiled in, reqwest rejects
  an https URL *at the connector*, before opening a socket, with `invalid URL, scheme is not http`
  — an error that points at the scheme rather than the missing feature, so the obvious reading
  ("my URL is wrong") is the wrong one. Since practically every remote MCP server is https, the
  HTTP transport failed in exactly the case it exists for. `tls-rustls` is now on by default;
  `tls-native` is available via `default-features = false, features = ["http", "tls-native"]`.
  Covered by a regression test that needs no network: it connects to a refusing loopback port and
  asserts the failure is a *connect* error rather than the scheme error.

- **`harness-rs-sandbox`: `container_sandbox_fails_cleanly_without_docker` no longer hangs the
  suite.** With the Docker CLI installed but the daemon stopped, `docker run` waits on it
  indefinitely, so the test never returned and took `cargo test --workspace` with it. The spawn is
  now bounded by a 20s timeout; a daemon that never answers counts as "nothing was spawned", which
  is what the test is really asserting.

## 0.0.32

### Fixed

- **`harness-rs-serve`: an empty answer is retried once instead of being served as a blank.** After a
  tool loop, a model occasionally returns no assistant text at all — a gateway hiccup, or the model
  deciding it is done right after the last tool result. `ChatService::chat` and `chat_stream` handed
  that straight through, and what the user saw was a blank reply after a query that had, in fact,
  executed successfully against their database.

  Both paths now re-run the turn once when the answer is empty, and return whatever the retry
  produces — including empty, if it is empty again; the failure is reported rather than papered over.
  The retry is safe in the streaming path specifically because an empty answer means no token was
  ever emitted, so nothing can be duplicated on the wire. A blank screen gives the user no way to
  tell "the model had nothing to say" from "the pipe broke", which is the worse of the two failures
  to be silent about.

## 0.0.31

### Added

- **`Block::Audio` — models that listen.** `harness-rs-core` gains
  `Block::Audio { media_type, base64 }` with a `Block::audio_bytes` constructor, rendered by the
  OpenAI-compatible adapter as an `input_audio` content part and by Gemini as `inlineData`. Audio is
  not an image with a different MIME type: OpenAI wants the payload as bare base64 with the container
  named separately (`{"data":…,"format":"wav"}`), so the data-URI shape an image uses would be
  silently unusable audio. The container name is the MIME subtype, with `audio/mpeg` spelled `mp3` the
  way the API spells it — a browser's default `audio/webm` stays `webm` rather than being forced into
  `wav`, which would hand a provider a container it cannot read while claiming otherwise.

  This is what a transcript cannot carry: intonation, stress, whether two words ran together, whether
  a vowel was long. Speech transcribed to text before the model sees it has already lost the part a
  pronunciation judgement rests on — which is why language tutoring needs the bytes, not the words.
  `Block` is `#[non_exhaustive]`, so the new variant breaks no downstream match.

## 0.0.30

### Fixed

- **`harness-rs-mcp-client`: a stdio MCP server that dies during the handshake now says why.**
  `McpClient::connect_stdio` reported rmcp's `connection closed: initialize response` and nothing
  else — no exit status, no stderr, no way to tell a broken binary from a transient start, because
  `serve` consumes the transport and the child is already reaped by the time the error is built. On
  that path it now re-runs the program once, drives the same handshake by hand while owning the
  child, and appends what actually happened: the exit status and the tail of what the server said
  ("it exited with status 1 before answering, saying: \"open cortexdb: unable to open database
  file\""), the signal that killed it ("was killed by signal 9 … and wrote nothing to stderr" — what
  a freshly copied binary looks like when macOS kills it on exec), that it hangs rather than crashes,
  or that it answers a handshake perfectly well, which says the binary is sound and the failed start
  was transient. Costs one extra spawn, on a path that has already failed.

## 0.0.29

### Added

- **`harness-rs-serve`: `GET /model`, CORS support, and tool-progress chunks.**
  `ChatService::model_name()` is surfaced at `GET /model` so a UI can show which
  model is live; `CorsConfig` + `router_with_cors` let a browser page served from
  another origin call `/chat` and read the `/chat/stream` SSE without exposing
  `tower-http` to callers; `ChatChunk::Step { label }` is emitted around each
  governed tool call so a client can show progress instead of a frozen spinner
  while the answer-token stream is silent during the tool loop.
- **`harness-rs-serve`: `ChatChunk::Error { message }`.** A stream failure now
  arrives as JSON on the same `data:` path as every other frame (still under
  `event: error` for older clients). Previously the body was a bare string, so a
  client parsing frames as JSON dropped the reason silently and the stream simply
  ended — indistinguishable from success, and the real cause never reached the UI.
- **`harness-rs-models`: multi-delta streaming decode.** One SSE chunk can decode
  to several `ModelDelta`s (e.g. `ToolCallStart` + `ToolCallArgs`); the driver now
  queues the extras instead of dropping them.

### Fixed

- **`harness-rs-models`: transport errors name their cause.** `reqwest::Error`'s
  `Display` is terse — an invalid base URL or a header value carrying a stray
  control character both surfaced as a bare `builder error`, which tells an
  operator nothing. Failures now include the target URL and the full source
  chain. The API key lives in a header, never in the URL, so nothing secret is
  added.
- **`harness-rs-mcp-client`: prune undeclared arguments for strict MCP servers.**
  Some gateways inject a sentinel key (Anthropic-via-OpenAI emits `{"_": true, …}`)
  alongside the real params, and a server whose input schema is closed
  (`additionalProperties: false`) then rejects the whole call. Undeclared
  top-level keys are dropped only for closed schemas; free-form tools pass
  through untouched.

### Changed

- **`verticals/` → `projects/datainside/`.** The DataInside vertical agents moved
  under `projects/`, joined by new ones (advisor, strategy-advisor, pharmacy,
  hotel-revenue, dental-clinic, autorepair-ops, ecommerce, edumind-tutor) and the
  `boss-briefing` project. Workspace members and docs updated accordingly.

## 0.0.28

### Fixed

- **`harness-rs-models`: tolerate `"tool_calls": null` in OpenAI-compatible
  responses.** Some providers (e.g. `cpa.superleo.app`) send `tool_calls: null`
  on a plain text answer instead of omitting the key; the non-streaming parser
  rejected it with `invalid type: null, expected a sequence`. Now decoded as an
  empty list, so `complete()` works against these endpoints.
- **`harness-rs-mcp-client`: keep the MCP session alive after `McpClient` drop;**
  add a first-class system prompt + bi-server wiring.

## 0.0.27

### Added

- **`harness-rs-serve` (new crate): multi-session serving core.** `ChatService`
  ties model + tools + audit + sessions behind one call — `chat` (unary) and
  `chat_stream` (token stream). Pluggable `Authenticator` (`StaticTokenAuth` /
  dev `OpenAuth`), `SessionStore` (`InMemorySessions`), and a wired per-request
  `AuditHook`. Optional `http` feature (an axum router: `POST /chat`,
  `POST /chat/stream` over SSE, `GET /healthz`) and `grpc` feature (a tonic
  service: unary `Say` + server-streaming `SayStream`).
- **`harness-rs-tools-sql` (new crate): safe read-only SQL tool.** SELECT-only
  guard, auto-`LIMIT`, result redaction, and a driver-pluggable `SqlExecutor`
  (optional `sqlite` backend). Documented as a *fallback* — correctness-critical
  BI should sit behind a governed semantic layer, not raw text-to-SQL.
- **`ModelRouter` (`harness-models`): local-first, cloud-fallback routing.** Itself
  a `Model`. Reads `Context.metadata`: `router.keep_local` pins a request to the
  local leg (data never leaves the intranet), `router.prefer_fallback` prefers the
  cloud leg; single-retry failover onto the other leg on error.
- **Tamper-evident audit trail (`harness-hooks`).** `AuditHook` records
  `request` / `response` / `tool_use` / `session_end` with actor/session/request
  identity and optional PII redaction, via a pluggable `AuditSink`
  (`JsonlAuditSink`, or hash-chained `HashChainSink` that `verify_chain` checks
  for deletion / edits / reordering). `new_request_id()` mints the correlation id
  that ties an audit line to its OTel trace and replay recording.
- **OTLP export (`harness-loop`, `otel` feature)** with telemetry aligned to the
  OpenTelemetry **GenAI semantic conventions** (`gen_ai.*`), so backends recognize
  token usage / model / finish reason automatically. Legacy flat field names kept
  as aliases.
- **`AgentLoop::run_with_seed_and_metadata`** — seed per-request `ctx.metadata`
  (caller identity + routing flags) into a shared, reused loop; the seam a serving
  layer uses. Session recording is wired for deterministic replay of served runs.

### Changed

- `harness-loop`'s `TelemetryHook` fields now follow the `gen_ai.*` GenAI
  conventions alongside the pre-existing flat aliases.

## 0.0.26

### Added

- **`harness-rs-redact` (new crate): PII detection + redaction.** Three
  orthogonal axes mirroring Presidio / cloud DLP — `Detector`s find `Span`s
  (`RegexDetector` + a checksum `validator`, so `luhn_valid` spares 16-digit
  order numbers), a `Policy` maps each `PiiKind` to an `Action`
  (`Label` `<EMAIL>` · `Mask` `************1111` · `Hash` stable pseudonym ·
  `Block` · `Keep`), and `Redactor::scrub` applies them, returning
  `Redaction { text, spans, blocked }`. **Redact-not-drop:** the surrounding
  fact is kept. Built-in `Policy::default()` / `Policy::memory_hygiene()`.
- **`RedactingMemory` (new, `harness-context`): redact-only `Memory` decorator.**
  Scrubs PII on `write` but *never drops* an entry — the right fit for the
  persistence boundary (transcript / experience → CortexDB). Wrap the `Memory`
  the transcript writer and episode store target and the biggest PII leak closes
  in one place. `redaction-demo` example shows the full picture.
- **Offline OCR for scanned PDFs: `harness-rs-tools-docs` feature `ocr-tesseract`.**
  A scanned, image-only PDF has no text layer, so local extraction comes back
  empty; with the feature on, `read_document` rasterises pages (`pdftoppm`) and
  recognises them (`tesseract`) — offline, deterministic, zero tokens. New
  `ocr_lang` arg (default `eng`, e.g. `eng+chi_sim`); result `source` is now
  `local` | `ocr` | `llm`. CLI-shell, so the crate still compiles everywhere;
  the two binaries are only needed at runtime.

### Changed

- **`GuardedMemory` now redacts instead of dropping.** Previously any entry
  matching a sensitivity regex was silently discarded; it now runs content
  through a `Redactor` (default `Policy::memory_hygiene`: cards masked,
  email/phone labelled, monetary amounts blocked) and stores the *redacted* text.
  Hard block-list (`with_blocked_substring` / `with_sensitivity_pattern`) still
  drops outright. Luhn-checked cards fix the old `\d{13,19}` false positives.

### Fixed

- **Scanned-PDF fallback no longer feeds binary-as-text to the model.**
  `read_document`'s LLM fallback used to hand a scanned PDF's raw bytes to the
  model as `from_utf8_lossy` garbage; it now returns an actionable error pointing
  at the `ocr-tesseract` feature instead of burning tokens.

## 0.0.25

### Added

- **Completion-rate benchmark: `eval-bench --bin bench-suite`.** A verifier-driven
  `pass@1` runner — each task carries a machine verifier (a shell assertion the
  harness runs *outside* the agent), so "resolved" is objective. Rust-native task
  set; **pass@1 = 5/5** measured with `qwen3.7-plus`. Emits a markdown table + JSON
  and exits non-zero on any failure for CI gating.
- **Stuck detection (`StuckPolicy`, `Outcome::Stuck`).** The loop fingerprints
  each round's tool calls; on repeated identical calls it injects a "change your
  approach" nudge (`nudge_after`, default 3) then terminates cleanly
  (`abort_after`, default 6) instead of burning the whole budget spinning.
  Enabled by default; `with_stuck_policy()` to tune or disable.
- **Observability: `TelemetryHook` + `harness replay` / `run --record`.**
  `TelemetryHook` maps the lifecycle event stream onto structured `tracing`
  spans (`agent_run` → `model.complete` / `tool.call` / …); attach
  `tracing-opentelemetry` to export to OTLP. `harness run --record run.jsonl`
  captures a session; `harness replay run.jsonl` re-executes it offline from the
  recorded model outputs (zero API cost), reproducing the exact Outcome — a free
  CI regression test.
- **Vision / image input: `Block::Image { media_type, base64 }`** + a
  dependency-free base64 encoder (`Block::image_bytes`). All three provider
  adapters render it: OpenAI `image_url` data-URI parts, Anthropic `image`/base64
  source, Gemini `inlineData`.
- **`harness-rs-tools-docs` (new crate): `read_document`.** Reads external
  documents — local pure-Rust extraction first (`pdf-extract` for PDF,
  `office_oxide` for docx/xlsx/pptx/doc/xls/ppt; feature `local`, on by default,
  no native deps), then an **LLM fallback** for anything local can't parse.
  Image files route to a real `Block::Image` vision request. Read-only, jailed to
  the workspace.
- **Conversation → CortexDB → knowledge graph.** `harness-experience`'s
  `TranscriptRecorder` (a sync hook → channel → background writer) persists every
  turn to any `Memory`; `CortexdbMemory::consolidate()` triggers CortexDB's
  server-side `knowledge_memory_consolidate` to distill memories into the graph
  (zero tokens on our side). `role`/`session` are promoted to `memory_save`'s
  first-class `role`/`session_id` columns.
- **Prefix-cache-friendly multi-turn: `AgentLoop::session()` → `Session`.** A
  persistent conversation that re-runs the loop each turn against a **stable
  prefix** (system + tool schemas), so a provider's prefix cache hits across
  turns. Verified live against DeepSeek: turn 2 reported **768 cached input
  tokens** (~10% price) instead of re-paying full price for the same bytes.
- **Parallel read-only tool dispatch.** When one model response emits several
  read-only tool calls, the loop now dispatches the *leading run* concurrently
  (`join_all`, each on a cheap `World` clone); a mutating tool is a serial
  barrier and all hooks/sensors/history stay ordered. `World` derives `Clone`;
  `ToolRegistry::risk(name)` added. Proven by a timing test (3 × 150ms → ~150ms).
- **`examples/cap`: tool-call storm guard** — a `StormGuard` hook fingerprints
  each `(tool, args)` call and, on an exact repeat within a sliding window,
  breaks the loop (`HookOutcome::Deny` with a "try a different approach"
  reflection) and prints a visible ⚠ marker. Kills the fast-model "call → fail →
  retry → same call" death loop and burns no extra tokens on it.

### Changed

- **Compaction tuning: real-token calibration + hysteresis (`CompactPolicy`).**
  The loop now calibrates the compactor's char-based token estimate against the
  model's real reported `input_tokens` each turn (fixes the fixed 0.30 tok/char
  heuristic badly under-counting CJK), and only compacts above a `high_water`
  mark, stopping as soon as usage is back under `target` — instead of running
  every stage over a threshold on every turn (avoids over-compacting to the lossy
  `AutoCompact` stage and needless prefix-cache invalidation).
- **Deterministic tool ordering** — `ToolRegistry::schemas()` now returns tools
  **name-sorted** instead of in `HashMap` order, so the request's `tools` block
  is byte-stable across turns (a prerequisite for prefix caching that was
  silently broken for every agent).
- **Cache tokens are surfaced** — `OpenAiCompat` now parses DeepSeek's
  `prompt_cache_hit_tokens` (and OpenAI's `prompt_tokens_details.cached_tokens`)
  into `Usage.cached_input_tokens` on both the `complete` and streaming paths;
  previously hardcoded to 0, throwing the information away.

## 0.0.24

### Added

- **`harness code --sandbox`** — shell commands run inside the OS sandbox
  (macOS Seatbelt / Linux bubblewrap): network denied, writes confined to the
  workspace. Falls back gracefully if the OS sandbox tool is missing. Closes the
  gap where the sandbox crate existed but no agent used it (verified live:
  `curl` inside the sandbox gets `CURLE_COULDNT_RESOLVE_HOST`).

### Changed

- **`harness-tools-fs` — the workspace jail is now OS-enforced.** `read_file`,
  `write_file`, `edit_file`, and `list_dir` run every operation through a
  capability directory (`cap-std` / `openat`), so `..`, absolute-path, and
  symlink escapes are rejected by the kernel instead of a lexical string check.
  A new `write_cannot_escape_workspace` test confirms a `..` write is refused
  and leaves nothing outside the root. With `--sandbox`, both side-effect
  channels are now confined: shell by the OS sandbox, files by the cap-std jail.

- **`harness-sandbox` — honest isolation.** Added an `Isolation` enum
  (`None` / `Changes` / `Process`) and a `Sandbox::isolation()` method so a
  backend reports what it *actually enforces*, not just the `FsPolicy`/`NetPolicy`
  it *requests*. Docs (crate + README + DESIGN §11) corrected: `WorktreeSandbox`
  isolates git *changes*, not capability; a sandbox wraps `runner.exec` (shell)
  only — in-process fs tools are jailed separately.

### Added

- **`SeatbeltSandbox` (macOS)** and **`BubblewrapSandbox` (Linux)** — OS-native,
  kernel-enforced isolation with **no Docker/daemon**. Each runs a command as a
  *separate* sandboxed process (`sandbox-exec` / `bwrap`), so the harness process
  is never restricted — the per-command helper model Codex CLI uses. Network
  denied by default; Bubblewrap also confines writes to the workspace
  (`--ro-bind / /` + `--bind <root>`). Verified live: Seatbelt net-deny on
  macOS (parent stays free), and the exact `bwrap` arg shape enforces net-deny +
  write-confinement in a real Linux container.
- Evaluated the ready-made cross-platform crate **`birdcage`** and **rejected**
  it here: its `spawn` applies the sandbox to the *calling* process (documented:
  "restrictions applied to the current process… single-threaded only"). Verified
  empirically — one benign sandboxed child killed the parent's network — so it's
  unusable for per-command sandboxing from a persistent, multi-threaded agent.

## 0.0.23

### Added

- **`harness code` — an interactive coding agent (opencode-style), built on the
  framework.** A multi-turn REPL with streaming output and read/write/edit/list/
  grep/glob + shell tools. Two modes: **NORMAL** (default) gates every mutating
  action — `write_file`, `edit_file`, `shell_exec` — behind a `y/N` prompt,
  surfacing a rejection back to the model (via `HookOutcome::Deny`) so it adapts;
  **`--yolo`** runs unattended. Conversation continuity across turns via
  `run_with_seed_history`; the whole terminal UX (token streaming + tool activity
  lines + approval) is one `Hook`. Verified live against `deepseek-v4-flash`
  (approve, decline, and YOLO paths).
- **`grep` and `glob` tools** in `harness-rs-tools-fs` — regex content search and
  path-glob file finding (`*`, `**`, `?`), both read-only, skipping `.git` /
  `target` / `node_modules` / `.venv`. So code search never trips the approval
  gate.
- **`examples/cap`** — a coding agent that reimplements the core of
  [oh-my-pi](https://github.com/can1357/oh-my-pi): **hashline editing**
  (content-hash line anchors instead of line numbers — stable under churn,
  batch-safe, duplicate-proof; dependency-free core with 8 unit tests). Wired on
  harness as two `Tool`s (`hash_read` / `hash_edit`), a workspace `Guide`, and a
  preview-then-approve `Hook` (`y`/`N`/`a`=always). Plus three IDE-grade
  extensions, all on framework primitives and verified live against
  `deepseek-v4-pro`:
  - **`task` subagent fan-out** — one isolated, read-only `Subagent` per
    subtask, run concurrently, returning a structured report array.
  - **Hindsight memory** — `harness-experience` records situation → tools used →
    outcome each turn; a fresh process recalls it (CortexDB semantic recall when
    available, else a local `~/.cap/experience.jsonl`).
  - **LSP diagnostics `Sensor`** — opt-in `CAP_LSP=<server>`; a **persistent**
    LSP client (its own codec tests) keeps the server warm and re-checks each
    edited file via `didChange`, feeding diagnostics back — errors block so the
    agent self-corrects (verified end-to-end with `gopls` catching a real type
    error).
  - **MCP tools** — `CAP_MCP="<command>"` connects any external MCP server via
    `harness-mcp-client` and mounts its tools into the loop; mutating MCP tools
    pass through the approval gate (verified calling `shell_read` over MCP from a
    second `harness mcp serve` process).
  - **Skills** — reusable procedures at `~/.cap/skills`: a `SkillCatalog`
    `Guide` lists them each session, `skill_read` loads one on demand, and
    `skill_manage` authors new ones — cross-session procedural memory (verified:
    one run authors a skill, a fresh run discovers and applies it).
  - **Model routing** — a strong planner (`HARNESS_MODEL`) drives the main loop;
    a fast worker (`CAP_WORKER_MODEL`) drives the `task` fan-out subagents (same
    endpoint/key). Verified with planner `deepseek-v4-pro` + worker
    `deepseek-v4-flash`.
  - **Sessions** — conversations persist to `~/.cap/sessions/<id>.json`;
    `--continue` resumes the latest for the workspace, `--resume <id|path>` a
    specific one, `--session <name>` a named one, `--sessions` lists them
    (verified: a secret told in one process is recalled in a fresh one). The CLI
    moved to **clap**.
  - **Split into a library + two front-ends** — `cap` is now a `cap` library
    crate plus two binaries sharing it: **`cap`** (CLI/REPL) and **`cap-tui`** (a
    standalone **ratatui** TUI — scrolling conversation, live streaming, tool
    feed; the agent runs on its own thread bridged to the render loop by a hook).
    The front-ends differ only by their UI hook.

## 0.0.22

### Added

- **`harness sched` — schedule agents from the CLI.** Wires `harness-scheduler`
  into the binary: `add` / `list` / `rm` / `enable` / `disable`, plus `run`
  (fire every due job once — point an OS cron at it) and `serve` (the tick loop,
  `--tick <secs>`). Jobs persist to `~/.harness/jobs.json` (`--store` to
  override); schedules are validated at `add` time. Model/endpoint resolve the
  same way as `harness run`; `stdout` delivery by default, `email` when
  `RESEND_API_KEY` is set. Verified end-to-end against a real model (fire →
  HTTP model call → channel delivery → `next_run` advance).
- **Per-crate READMEs** for `harness-rs`, `harness-rs-loop`, and
  `harness-rs-models`, so each lands with its own docs on crates.io / docs.rs
  instead of the shared workspace README.
- **`eval-bench` now reports cost** — `iters`, `input_tokens`, `output_tokens`,
  and `tool_calls` are emitted per run (previously the usage was dropped), so
  the benchmark measures tokens, not just correctness. README gains a
  **Benchmarks** section with measured numbers on `deepseek-v4-flash`.

### Changed

- **CLI installs a real tracing subscriber.** Previously the CLI set a
  `NoSubscriber`, so model 401s and scheduler delivery failures vanished
  silently. It now uses an `EnvFilter` fmt subscriber (default `warn`, override
  with `RUST_LOG`) writing to **stderr** — failures surface, and `run --json` /
  `mcp serve` keep stdout clean.

## 0.0.21

### Added

- **`harness run "<prompt>"` — run an agent from the CLI.** The headline gap:
  the CLI could scaffold, lint skills, print traces, and serve MCP, but it
  couldn't actually *run* an agent. Now it can. Model comes from
  `--model`/`--base-url` or the `HARNESS_*` / `DEEPSEEK_API_KEY` env vars.
  **Read-only by default** (`ReadFile` + `ListDir`); opt into writes with
  `--write` and the shell tool with `--shell`. Also: `--workspace`,
  `--max-iters`, `--progress` (live stderr trace), and `--json` (dump the full
  `Outcome`).

## 0.0.20

### Changed

- **README** — documents the learning layer (`harness-experience` +
  `harness-cortexdb`) and the `experience-cortexdb` example. Docs-only refresh;
  no code changes.

## 0.0.19

A **learning layer**: agents can now remember how they handled past situations
and semantically recall that experience, backed by CortexDB.

### Added

- **`harness-rs-experience` — episodic learning layer.** Records each run as an
  `Episode` (situation → tools used → outcome) and recalls similar past
  episodes before the next run to guide it. Pieces: `Episode`, `ToolTrace` (a
  `Hook` capturing the tools a run calls, in order), `ExperienceStore`
  (record/recall over any `Memory`), `ExperienceGuide` (recall + inject each
  turn), and `ExperienceRecorder` (wires them to an `AgentLoop`). Backend-
  agnostic — semantic recall comes from a semantic `Memory` backend.
- **`harness-rs-cortexdb` — CortexDB-backed `Memory`.** Implements
  `harness_core::Memory` over CortexDB's MCP server (`memory_search` for
  recall, `memory_save` for write); `tags` + `source` round-trip through
  CortexDB `metadata`. Gives any harness agent **semantic recall** and a brain
  that can be shared with Claude Code / Codex (the default `~/.cortexdb`
  store). Pair with `harness-rs-experience` for a learning layer with real
  semantic recall. See `examples/experience-cortexdb`.

## 0.0.18

### Fixed

- **`harness-rs-orchestrator` — `RunReport::render` no longer panics on
  multibyte text.** The summary truncation sliced the job result at a fixed
  byte index (`&t[..80]`), which panics when byte 80 falls inside a multi-byte
  UTF-8 char (e.g. an emoji or CJK character in a model's output). It now
  truncates on a character boundary. Regression test added.

## 0.0.17

New **orchestration** layer: run one goal as a concurrent DAG of Jobs.

### Added

- **`harness-rs-orchestrator` — single-machine async Run orchestration.** A new
  crate that fans one goal out across many concurrent, dependent Jobs — the
  durable task fabric of an agent system, kept deliberately single-machine (no
  Kafka, no worker pool, no distributed locks; just `tokio` + a state store):
  - **Concurrent Job DAG** — a `Dag` of `Job`s; the `Orchestrator` runs every
    Job whose dependencies have `Succeeded`, up to a concurrency cap, on one
    thread via `FuturesUnordered` (sub-agent futures are `!Send`). Each Job
    gets a fresh `World` from a factory for worker-style isolation.
  - **Dynamic replanning** — a `Planner` is re-invoked with the results so far
    and may merge new Jobs into the running DAG (`PlanDelta::Add`), the
    feedback edge a static plan-then-execute workflow lacks.
  - **Retry / backoff / dead-letter** — per-Job `RetryPolicy` with
    `Backoff::{None, Fixed, Exponential}`; exhausted Jobs are `DeadLettered`
    and their dependents `Cancelled`.
  - **Resumable state** — a `RunStore` (`InMemoryRunStore` / `FileRunStore`)
    persists Run + Job state after every transition; `Orchestrator::resume`
    restarts a crashed Run from its succeeded results.
  - **Run-level token budget** — `RunBudget` caps total spend across all Jobs.
  - Up-front DAG **cycle rejection**. Execution is decoupled via the
    `JobRunner` trait; the default `SubagentJobRunner` runs each Job as an
    isolated sub-agent. See DESIGN.md §11.6 and `examples/orchestrator-demo`.

## 0.0.16

New **loop-engineering** layer, plus a simplified, hardcoded-URL-free model API.

### Added

- **`harness-rs-loop-engine` — loop engineering for harness-rs.** A new crate
  that turns the existing building blocks (scheduler, sandbox, sub-agents,
  memory, MCP) into *trusted recurring loops*. It adds the orchestration
  discipline those parts lacked:
  - **`LoopLevel`** — maturity levels `L1Report` → `L2Assisted` → `L3Unattended`
    (a loop earns autonomy in stages; the level governs both write-capability
    and gate policy).
  - **`HumanGate`** — proceed-or-escalate decisions tied to the level
    (`AlwaysEscalate`, `AllowlistGate`, `CallbackGate`).
  - **`TokenBudget` / `BudgetState`** — per-round input/output/total token
    ceilings, tallied across the maker and checker sub-agents.
  - **`LoopSpec`** — an inert, serializable loop definition; its required
    `intent` field is the antidote to *intent debt*.
  - **`LoopEngine::run_once`** — one verified, budgeted, gated round: recall
    state → isolate → **maker** sub-agent → **checker** sub-agent → gate →
    record state. Never panics or returns `Err` (failures fold into
    `RoundOutcome::Failed`).
  - **`LoopScheduler`** — runs loops on their declared cadence.
  - **`patterns`** — seven ready-made production loops: `daily_triage`,
    `pr_babysitter`, `ci_sweeper`, `dependency_sweeper`, `changelog_drafter`,
    `post_merge_cleanup`, `issue_triage`.

  See DESIGN.md §11.5. Example: `examples/loop-engine-demo`.
- **`harness-rs-models` — `ApiKind` single entry point.** `ApiKind::{OpenAI,
  Anthropic, Gemini}.build(base_url, model, key) -> Arc<dyn Model>` constructs
  any of the three protocol families through one call.

### Changed

- **`harness-rs-models` — no more hardcoded provider URLs (breaking).** The
  `providers` module and its vendor URL menu (`DEEPSEEK`, `OPENAI`, `GROQ`,
  `TOGETHER`, `OLLAMA`, `ANTHROPIC`, `GEMINI`) are **removed**. There are exactly
  three protocol families and you always pass `base_url` yourself.
  `AnthropicNative::with_key` and `GeminiNative::with_key` now take
  `(base_url, model, key)` to match `OpenAiCompat::with_key` — no URL is baked
  into any adapter. Migration: replace `providers::DEEPSEEK` with the literal
  `"https://api.deepseek.com"`, etc.
- **`harness-rs-loop` — `SubagentReport` now carries `usage`.** The
  `harness_core::Usage` from the sub-agent's loop is preserved on the report
  (previously discarded), so callers can account for token spend across
  sub-agent turns. `BudgetExhausted` rounds also surface their `last_text`.
- **`harness-rs-loop-engine` — L1 now hard-filters mutating tools.** Report-only
  loops no longer rely only on prompt text for read-only behaviour: L1 maker and
  checker sub-agents receive only `ReadOnly` / `Network` tools. `Idempotent` and
  `Destructive` tools are skipped with a trace log.
- **`harness-rs-loop-engine` — action executors for approved work.**
  `LoopEngine` now has an `ActionExecutor` handoff. When a verified proposal is
  auto-approved, the executor is invoked and its `ActionReceipt` is attached to
  the `RoundReport`; executor failures become `RoundOutcome::Failed` instead of
  pretending the action landed. The safe default is `ApprovalOnlyExecutor`, and
  apps can install `CallbackActionExecutor` or their own async executor via
  `with_action_executor`.
- **`harness-rs-sandbox` — VM isolation is now explicitly deployment-owned.**
  The non-functional `VmSandbox` / Firecracker stub has been removed from the
  core crate. VM or microVM isolation should be provided by downstream
  infrastructure crates that implement the existing `Sandbox` trait.

### Tests

- Added deterministic `LoopEngine::run_once` coverage for L1 tool filtering, L3
  allowlisted auto-proceed, budget exhaustion before checker execution, and
  memory recall/writeback of the loop state spine. Added action-executor
  coverage for successful handoff and failed handoff.

## 0.0.14

Skill loading is now interop-friendly and fault-isolated. Additive, backward-compatible.

### Fixed

- **`harness-rs-skills` — one bad skill no longer hides them all.**
  `scan_skills_root` previously did `load(&p)?`, so a single malformed
  `SKILL.md` aborted the entire scan and the agent saw *zero* skills. It now
  **skips the offending skill with a `tracing::warn!`** and returns every valid
  one. Regression test `scan_skips_invalid_skill_keeps_the_rest`.

### Changed

- **`harness-rs-skills` — tolerate non-spec frontmatter fields.** Skills from
  the wider ecosystem (skills.sh, Claude Code, …) routinely carry extensions
  like `displayName` / `hidden`. The loader used to **reject** any unknown
  top-level field; it now **logs and ignores** them (the field is dropped on
  deserialize), so those skills load instead of failing. Spec guidance is
  unchanged — extensions still belong under `metadata`. Test
  `rejects_unknown_top_field` → `tolerates_unknown_top_field`.

## 0.0.13

`forget_memory` can now delete in a single tool round. Additive, backward-compatible.

### Added

- **`harness-rs-tools-memory` — one-call `forget_memory`.** `ForgetMemoryTool`
  gains `with_resolver(Arc<dyn Memory>)`: when wired, the tool accepts a natural
  language `query` (the fact in the user's own words) in addition to an exact
  `id`, recalls the single best match, and deletes it. This collapses the usual
  `list_memories` → `forget_memory` two-round dance into one call, cutting an LLM
  round-trip off every delete. Without a resolver the tool keeps its prior
  id-only behaviour; if both `id` and `query` are given, `id` wins. Added
  regression tests covering query resolution, id precedence, a no-match miss, and
  the id-only rejection path.

## 0.0.12

Security fix for the skill-management tool. Additive (no breaking changes).

### Security

- **`harness-rs-tools-skills` — `skill_manage` path-traversal hardening.** The
  `patch` action joined the user-supplied skill `name` into a path and read the
  `SKILL.md` *before* validating the name, so a crafted name like
  `../other/skill` could read a file outside the tool's skills dir (a low-severity
  existence-probe leak in multi-tenant hosts — no write, no content exfiltration).
  `validate_name` now runs up front in `SkillManageTool::invoke` for **every**
  action before any filesystem access. Added a `patch_rejects_traversal_name`
  regression test.

## 0.0.11

Security fix for the MCP HTTP client. Additive (no breaking changes).

### Security

- **`harness-rs-mcp-client` — SSRF-safe HTTP connect.** `connect_http` uses a
  default reqwest client that follows redirects and re-resolves DNS, so a
  validated URL can still be redirected (`302 → http://169.254.169.254/…`) or
  DNS-rebound to an internal target. New **`McpClient::connect_http_with_client(url,
  client)`** lets the caller pass a hardened `reqwest::Client`
  (`redirect::Policy::none()` + `.resolve(host, vetted_ip)`), closing the
  redirect-bypass and DNS-rebinding holes while keeping the security policy on the
  caller's side. The matching `reqwest` is re-exported as
  `harness_mcp_client::reqwest` so client types unify. `connect_http` now carries
  an explicit SSRF warning in its docs.

## 0.0.10

100% MCP client transport coverage. Pure addition on top of 0.0.9.

### Added

- **`harness-rs-mcp-client` — Streamable HTTP transport.** New
  `McpClient::connect_http(url)` connects to a remote MCP server over Streamable
  HTTP (the standard remote MCP transport; SSE is subsumed by it), in addition to
  the existing `connect_stdio` child-process transport. Behind the `http` feature
  (on by default; `default-features = false` drops the reqwest dependency). The
  tool-proxy layer is transport-agnostic, so remote-tool results flow back through
  the agent loop exactly as with stdio.

## 0.0.9

Thinking-model + local-tool-calling fixes for the OpenAI-compat adapter,
shaken out against Qwen3 on Ollama. Backward-compatible.

### Fixed

- **No-arg tool calls no longer 400 on strict backends.** A tool call with no
  arguments was echoed back with `arguments: ""`, which Ollama rejects
  (`HTTP 400 invalid tool call arguments`). `OpenAiCompat` now normalises any
  non-object arguments to `"{}"` when serializing the assistant turn.
- **Thinking-model replies no longer come back blank.** Models that emit the
  whole answer into the reasoning channel and leave `content` empty (e.g. Qwen3
  via Ollama) now surface that reasoning as the turn's text — both in
  `OpenAiCompat::complete` and in the streaming agent loop when a turn ends with
  no text, no tool calls, and non-empty reasoning.
- **Streamed reasoning is concatenated verbatim** instead of being joined with
  newlines, so fallback replies read as prose rather than one word per line.

### Added

- **`OpenAiCompat` now captures Ollama's `reasoning` field** (in addition to
  DeepSeek's `reasoning_content`) on both the non-streaming and streaming paths.
- **`HARNESS_OPENAI_EXTRA_BODY`** — a JSON object merged into every
  chat-completions request body. Lets callers pass provider-specific knobs the
  typed request doesn't model, e.g. disable Qwen3 thinking on Ollama with
  `{"chat_template_kwargs":{"enable_thinking":false}}`.

## 0.0.8

Local-model ergonomics — an Ollama embeddings adapter and a configurable HTTP
timeout. Pure addition on top of 0.0.7.

### Added — Ollama embeddings

- **`OllamaEmbed`** (`harness-rs-models`) — implements `harness_core::Embedder`
  against a local Ollama server's OpenAI-compatible `/v1/embeddings` endpoint.
  Defaults to Google's `embeddinggemma` (768-dim); `OllamaEmbed::with_model`
  overrides the model/dim. Pairs with `OpenAiCompat::with_key(providers::OLLAMA,
  ..)` for a fully-offline chat + vector-search stack. Opt-in: the chat adapters
  do not reference it.

### Changed — OpenAI-compat timeout

- `OpenAiCompat`'s per-request HTTP timeout (previously a hardcoded 120s) is now
  configurable via `HARNESS_HTTP_TIMEOUT_SECS`, for slow local backends whose
  first-token latency on large models can exceed two minutes. Default unchanged
  at 120s.

## 0.0.7

MCP client — consume external MCP servers from an `AgentLoop`. Pure addition
on top of 0.0.6.

### Added — MCP client

- **New crate `harness-rs-mcp-client`** — a generic MCP (Model Context Protocol)
  client built on the official `rmcp` 1.7 SDK. `McpClient::connect_stdio(program,
  args)` spawns an MCP server as a child process over stdio, lists its tools, and
  exposes each as a harness `Arc<dyn Tool>` (`.tools()` /
  `.tools_with_read_only(names)` / `.tool_names()`). MCP results flow back through
  the standard `AgentLoop` path (`PreToolUse` / `PostToolUse`, session record,
  context) — not a side channel. Complements `harness-rs-mcp` (the server side).
  Verified end-to-end against CortexDB's MCP server (47 RAG/GraphRAG tools;
  `knowledge_save` → `knowledge_search` round-trip).
- Remote tools default to `Destructive` risk; `tools_with_read_only` marks named
  tools `ReadOnly`. Non-object tool args are rejected with `InvalidArgs`; non-text
  content blocks (image/resource/audio) are surfaced via `tracing::warn!` + an
  `omitted_content` key instead of being silently dropped.

### Added — CI

- CI runs the `harness-rs-mcp-client` integration tests and clippy under its
  `test-server` feature (which gates a test-only echo MCP stdio server).

## 0.0.6

FileRecall robustness + release automation. No breaking source changes.

### Fixed

- **`harness-context` FileRecall** — filename sanitization now caps by **bytes**
  (not chars) and hashes over-long names, fixing `ENAMETOOLONG` on Linux for
  long / non-ASCII session keys.

### Added — release

- **Release workflow** — pushing a `v*` tag verifies the tag matches the
  workspace version, runs the test gate, then publishes every `harness-rs-*`
  crate to crates.io in dependency order via `cargo ws publish`.
- README tour sections for recall / learning-loop / scheduler.

## 0.0.5

Three capabilities — cross-session **recall**, a self-evolving **learning loop**,
and in-process **scheduling** — plus new crates. Additive on top of 0.0.4.

### Added — recall (cross-session search)

- **`RecallStore` trait** with two backends: `harness_context::FileRecall` (JSONL)
  and the new optional crate **`harness-rs-recall-sqlite`** (SQLite FTS5 + trigram
  tokenizer for CJK, BM25 ranking). `AgentLoop::with_recall` / `.auto_inject`, an
  owner-scoped `SessionSearchTool` (three query shapes), and an opt-in
  `RecallGuide`. A shared contract test suite covers both backends including
  owner isolation.

### Added — learning loop (self-evolving skills + memory)

- **`AgentLoop::with_learning_loop(LearningConfig)`** — forks a review subagent at
  `SessionEnd` (threshold-gated, best-effort) that patches skills/memory from the
  transcript. New crate **`harness-rs-tools-skills`** with the `skill_manage` tool
  (create/edit/patch/delete `SKILL.md`); `harness-skills` gains `write_skill_md` /
  `delete_skill` (validate-on-write).

### Added — scheduling

- **New crate `harness-rs-scheduler`** — `Job` + `JobStore` / `FileJobStore`, a
  `Scheduler` that ticks and runs a job as a subagent turn, a `Channel` trait with
  `StdoutChannel` + `EmailChannel` (Resend), and a `cronjob` tool for agent
  self-scheduling (schedule-string validated).

### Changed

- **`harness-core`** — `Arc<dyn Model>` is now used via the `DynModel` newtype
  (replacing the blanket `impl Model for Arc<dyn Model>`, which overflowed the
  `Send` auto-trait solver in some async contexts).

## 0.0.4

Observability and open long-term memory. No breaking source changes; pure
additions on top of 0.0.3.

### Added — observability

- **`harness_loop::LiveProgressHook`** — `Hook` that streams every model call,
  tool call, and tool result to stderr in real time. Pair with
  `AgentLoop::with_hook` to watch what the agent is doing instead of staring
  at a silent terminal. Independent of `SessionRecorder`; both can be
  installed together.
- **`harness_loop::format_event_verbose`** — multi-line formatter that surfaces
  model text, reasoning, full tool args, tool result preview, and failure
  reasons (errors / hint / message / error keys). Used by the live hook and
  by `harness trace --verbose`.
- **`harness trace --verbose`** (alias `-v`) — selects the verbose formatter
  when pretty-printing a recorded JSONL session.
- **`Event::BudgetWarning { ratio }`** is now fired (was defined but unused).
  Currently emitted exactly once, with `ratio = 1.0`, immediately before the
  forced final-synthesis pass — so observers can clearly label that boundary.
  `SessionEvent::BudgetWarning` mirrors it for replay.

### Added — loop completeness

- **Forced final-synthesis on budget exhaustion** — when `run_with_max_iters`
  would otherwise return `Outcome::BudgetExhausted { last_text: None, .. }`,
  the loop makes one extra tool-less model call asking for the best-effort
  conclusion. The result lands in `last_text`. Closes the "agent burned all
  iterations on tool calls, returned no answer" failure mode. Regression
  test: `budget_exhausted_forces_final_synthesis_into_last_text`.

### Added — long-term, open memory

The piece Harrison Chase ("your harness, your memory") and Viv Trivedi
("distil traces into higher-level memory primitives") call out as the
moat against provider lock-in. All on the user's disk; nothing on a
third-party server.

- **`harness_core::Memory`** trait + **`MemoryEntry`** + `MemoryError`.
- **`harness_context::FileMemory`** — append-only JSONL backend with
  keyword-overlap recall (ties broken by recency). No embedding deps;
  swap-in a vector backend by implementing the trait.
- **`harness_loop::MemoryGuide`** — Guide::Always; at session start calls
  `recall(task.description, top_k)` and injects the hits into `ctx.guides`
  as a single `Block::Text` so the model sees them in the system prompt.
- **`harness_loop::MemoryWriter`** — Hook that persists the verbatim final
  assistant text on `TaskCompleted` (skips `BudgetExhausted`).
- **`harness_loop::MemorySynthesizer`** — smarter alternative: uses a cheap
  separate "synth model" (e.g. `deepseek-v4-flash`, `gpt-5-nano`) to
  distil each session into 1-3 atomic durable facts tagged for retrieval.
  Markdown fences tolerated; unparseable model output falls back to a
  `"synth-raw"` entry rather than silent drop. `flush_pending()` awaits
  spawned writes so callers can guarantee persistence before `main()`
  returns (otherwise tokio runtime drop cancels in-flight commits).

### Examples

- `--progress` / `HARNESS_PROGRESS=1` installs `LiveProgressHook` on
  `personal-assistant` and `investor-bot`.
- `--record <path>` writes a JSONL session log (parity between both
  examples).
- `--memory <path>` + `--synth-model <id>` (env: `HARNESS_SYNTH_MODEL`)
  installs `MemoryGuide` + `MemorySynthesizer` on both examples. Synth
  model defaults to `deepseek-v4-flash`.
- `HARNESS_BASE_URL` / `HARNESS_MODEL` / `HARNESS_API_KEY` env vars let
  the same binaries drive any OpenAI-compatible endpoint without code
  edits; DeepSeek defaults preserved.
- Both `BudgetExhausted` print sites now surface `last_text` (the
  forced-synthesis answer).
- investor-bot SYSTEM_PROMPT strengthened with explicit budget rules:
  stop retrying after 2 empty searches; abandon URLs returning
  401/403/503; commit to a partial answer marking unverified facts as
  UNKNOWN.

### Fixed

- `harness new` scaffold was pinning `0.0.1` deps (pre-publish; would
  never build) and using the wrong package names in `[patch.crates-io]`
  (`harness = ...` instead of `harness-rs = ...`, so `--local` never
  actually patched). Now pins `0.0.4`, correct published names, and a
  `main.rs` that demonstrates the env-var endpoint config and
  `HARNESS_PROGRESS` opt-in.

### Tests

- 133 passing (was 123). 10 new tests cover live progress, forced
  synthesis, budget-warning emission, file memory round-trips,
  memory-writer persistence, memory-synth JSON parsing + fence stripping
  + raw-fallback, and the cross-session end-to-end recall.

## 0.0.3

Re-publish of the 0.0.2 feature set so that every workspace crate ships
consistent code. 0.0.2 went out in stages, and several crates landed
on crates.io before the `PreAutoFix`/`PostAutoFix` events were added
to `harness-rs-core`. Downstream consumers that bumped a single crate
to 0.0.2 could hit `error[E0599]: no variant named PreAutoFix`. 0.0.3
fixes that by re-publishing every crate at the same source revision.
No new features — depend on 0.0.3 over 0.0.2 if you want any of the
"0.0.2" CHANGELOG entries below to actually be present.

## 0.0.2

The first version anyone outside this checkout should depend on. Adds a
proper user-profile mechanism, daemon scheduler, retry/backoff in the model
adapters, and closes the security holes from the self-audit.

**Known issue:** published in stages; crates published before
`PreAutoFix`/`PostAutoFix` were added to `harness-rs-core` are missing
those events. Use 0.0.3 instead.

### Added

- **`harness-rs-daemon`** — optional standalone scheduler crate. Reads a
  declarative TOML config (`daily HH:MM` / `weekly mon HH:MM` / `every Nm`)
  and spawns each job as a subprocess. Pair with launchd / systemd to run
  forever. Does not depend on any other `harness-rs-*` crate.
- **`UserProfile` + `ProfileGuide`** in core/loop — ambient user context
  (name, timezone, locale, free-form `extra` map) that any tool can read
  from `World.profile`. The framework provides the slot; apps decide where
  the data comes from. Opt-in `ProfileGuide` injects it into the system
  prompt.
- **`AgentLoop::run_with_seed_history`** — lets REPL apps push prior
  conversation into `ctx.history` (where the compactor sees it) instead of
  string-concatenating into `task.description` (where it didn't).
- **Retry/backoff** in `OpenAiCompat::complete` and `AnthropicNative::complete`.
  5xx + 429 + send/body errors retried up to 3× with 1s/2s/4s backoff.
  Other 4xx (auth, bad request) propagate immediately.
- **`Outcome` partial-work surface** — `Outcome::Done` and
  `Outcome::BudgetExhausted` now both carry `tools_called: u32`,
  `usage: Usage`, and (for BudgetExhausted) `last_text: Option<String>`.
  Both variants `#[non_exhaustive]`.
- **MCP server resources + prompts** — `harness-rs-mcp` gains
  `resources/list`, `resources/read`, `prompts/list`. Skills can be mounted
  via `McpServer::with_skill(...)` and become `harness://skill/<name>`
  resources for any MCP client (Claude Code / Cursor / Codex).
- **`harness mcp serve --skills <dir>`** — CLI flag to expose a filesystem
  skills directory as MCP resources without writing code.
- **`Event::PreAutoFix` + `Event::PostAutoFix`** — hooks can intercept
  sensor-emitted `FixPatch` patches per-patch. Default safelist denies
  `RunCommand` outside `cargo fmt|clippy|fix / rustfmt / gofmt / prettier
  / ruff / black`. Hooks may widen with `HookOutcome::Allow`.
- **`is_default_safe_fix(&FixPatch) -> bool`** — public, so apps can run
  the same gate independently.
- **Wider `shell_read` allowlist** — `npm/pnpm/yarn/bun`, `python/pip` family
  (read-only only), `node/deno --version`, `go` (version/env/list/vet/doc/fmt),
  `make --version|--dry-run`, `docker/podman/kubectl` inspection subcommands,
  plus `tree / stat / file / du / df / ps / uname / hostname / date / env /
  which / whereis`. Write subcommands explicitly rejected with a 'use
  shell_exec' message.
- **Multi-engine search** in `examples/investor-bot`: DuckDuckGo → Bing
  fallback chain + one retry per engine. Returns structured "engines_tried"
  + "errors" + "hint" when both empty so the agent can pivot.
- **`harness new --local` / `--workspace`** — auto-wires `[patch.crates-io]`
  to a local checkout so the scaffolded project builds before crates.io is
  populated.
- **`harness skills export` round-trip verified** — emitted SKILL.md
  validates against the agentskills.io spec and re-loads identically,
  including `metadata.harness.*`.
- **examples/personal-assistant** — full scheduling agent: calendar events
  + todo tasks + REPL mode + brief mode.
- **examples/investor-bot** — autonomous research agent over public web
  + SEC filings. Cites sources, refuses to hallucinate, always disclaims.
- **GitHub Actions CI** — `cargo check / test / fmt / clippy -D warnings`
  on ubuntu + macos.
- **123 unit / integration tests** (up from 92).

### Changed

- **`[lib] name = "harness_X"`** override on every library crate so external
  users write `use harness_core::*` instead of the auto-derived
  `harness_rs_core::*`. (`harness-rs` facade exposes itself as `harness`.)
- **19 public enums marked `#[non_exhaustive]`** — `Event`, `HookOutcome`,
  `Block`, `ToolRisk`, `Stage`, `CompactionStage`, `StopReason`, `ModelDelta`,
  `SubagentStatus`, `Severity`, `TurnRole`, `Execution`, `NotificationKind`,
  `SessionSource`, `ResourceKind`, `FixPatch`, `GuideScope`, `HarnessError`,
  `Transition`. Downstream matches need `..` from now on; new variants in
  0.0.x bumps won't break consumers.

### Fixed

- **DeepSeek `reasoning_content` round-trip** — Block::Reasoning preserved
  across turns; both OpenAiCompat and AnthropicNative re-emit on the wire.
  Without this, the second model call in a multi-turn loop returned
  `HTTP 400: reasoning_content in thinking mode must be passed back`.
- **`OpenAiCompat::build_messages`** no longer re-appends task description
  as a final user message after every tool call (was duplicating the task
  on each subsequent model call).
- **`providers::ollama`** uses Ollama's real default port `11434` (was an
  incorrect 43511).
- **`apply_patches` temp-diff filenames** include pid + nanos + atomic
  counter (was ms-only, collided on parallel patch application).
- **`patch -p1` tried first, then `-p0`** (was hardcoded `-p0`, silently
  losing git-style diffs).
- **`Skill` trait no longer marked `async_trait`** — none of its methods
  are async; the attribute was wrong and prevented `#[skill]` macro
  expansion in some edge cases.
- **Symlink escape in `harness-tools-fs`** — `resolve()` canonicalises
  after access; in-workspace symlinks pointing outside the root are now
  rejected.
- **`ReadFile` truncation visibility** — return value includes
  `truncated: bool` so the model knows it didn't see the whole file.

### Security

- **`FixPatch::RunCommand` was a silent arbitrary-code-execution surface**
  for any sensor (first- or third-party) to abuse. Default safelist + new
  `PreAutoFix` hook event close that. See `is_default_safe_fix` above.
- **`shell_read` per-program safe-args predicate** blocks
  `cargo install / publish`, `git config <k> <v>`, `find -exec / -delete /
  -ok / -okdir`, `xargs --exec`, and the language `install / run / exec`
  family on npm/pip/etc. Previously program-only allowlist.
- **`#[non_exhaustive]` everywhere relevant** — adding `SecretLeakingVariant`
  later won't compile against a downstream that does
  `match (..) { Existing => .., AlsoExisting => .. }` and miss the new arm.

## 0.0.1

Initial publish (15 of 18 crates landed at this version before the rename
to the `[lib] name = "harness_X"` scheme; users on 0.0.1 see import names
`harness_rs_core::*` etc.). Functional but does NOT contain the profile
mechanism, daemon, retry, or any of the audit fixes above. Prefer 0.0.2.

- Initial cut of `Model / Tool / Guide / Sensor / Hook / Compactor / Skill`
  traits.
- Five macros: `#[skill]` (agentskills.io-compliant) + `#[tool]` /
  `#[guide]` / `#[sensor]` / `#[hook]`.
- AgentLoop ReAct with auto-fix patch application + sensor feedback.
- DefaultCompactor with 5 stages (BudgetReduce → Snip → Microcompact →
  ContextCollapse → AutoCompact).
- HookBus over 27 lifecycle events (Allow / Deny / Inject / Mutate).
- Blueprint state machine (deterministic + agent hybrid).
- WorktreeSandbox + NullSandbox (Container / VM stubs).
- agentskills.io-spec-compliant `harness-rs-skills` (validate / list / lint
  / export).
- ToolRegistry + AgentLoop builder pattern.
- OpenAiCompat (DeepSeek / Groq / Together / Ollama / any OpenAI-shaped
  endpoint) + AnthropicNative (Messages API with content blocks).
- MockModel for deterministic tests.
- MCP stdio JSON-RPC server (initialize / ping / tools/list / tools/call).
- SessionRecorder + read_session + replay_as_mock — record any run, replay
  it deterministically offline against a fresh AgentLoop.
- harness CLI: `new`, `skills validate/list/lint/export`, `trace`,
  `mcp serve`.
- 92 unit / integration tests passing.

[Unreleased]: https://github.com/liliang-cn/harness-rs/compare/v0.0.4...HEAD
[0.0.4]:      https://github.com/liliang-cn/harness-rs/compare/v0.0.3...v0.0.4
[0.0.3]:      https://github.com/liliang-cn/harness-rs/compare/v0.0.2...v0.0.3
[0.0.2]:      https://github.com/liliang-cn/harness-rs/compare/v0.0.1...v0.0.2
[0.0.1]:      https://github.com/liliang-cn/harness-rs/releases/tag/v0.0.1
