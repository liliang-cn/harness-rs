//! Task-completion benchmark — the `pass@1` runner.
//!
//! Where `eval-bench` (the other bin) measures *cost* on one task, this measures
//! whether the agent actually *solved* the task. Each task carries a machine
//! verifier (a shell assertion that exits 0 when the workspace end-state is
//! correct); the harness runs the verifier itself, outside the agent, so
//! "resolved" is an objective fact, not the model grading its own homework.
//!
//! This is the Rust-native, self-built task set: small, deterministic, no
//! network, no Docker. It exists to make "can it work autonomously?" a number
//! we can regress on. SWE-bench-lite (Python, per-instance containers via
//! `ContainerSandbox`) is the next phase and reuses this same runner shape.
//!
//! ## What it measures, and why these three
//!
//! **`pass^k`** — every task run `k` times, all `k` must resolve. Not `pass@k`,
//! which is the opposite: `pass@k` asks whether *any* attempt worked and is an
//! upper bound on capability; `pass^k` asks whether *every* attempt worked and
//! is a floor on reliability. The distinction is not academic — τ-bench reports
//! a model at 81.6% `pass^1` falling to 56.1% at `pass^4`. Making an agent
//! succeed *repeatably* is what a harness is for, so this is the number that
//! measures the harness rather than the model.
//!
//! **Leave-one-out guard ablation** — the same tasks, the same model, with the
//! loop's guards removed *one at a time* from the shipped default (`H2`). A
//! harness's contribution is otherwise inseparable from its model's: the same
//! model has been measured at 46% under one scaffold and 80% under another. The
//! earlier H0/H1/H2 ladder showed the scaffold matters but could not say which
//! guard; a `H2 − guard` row attributes the difference to exactly one thing.
//! Dedupe was measured off the default, so it runs the other way: `+dedupe`.
//!
//! **Trigger coverage** — every run counts how often each guard actually fired
//! (truncations, repeat suppressions, stuck nudges/aborts, acceptance
//! rejections, compactions). An ablation row whose guard never fired measures
//! nothing — the delta is noise, and the report says so instead of letting a
//! zero look like "this guard buys nothing". Four tasks exist purely to make
//! guards fire: a prompt that instructs the model to retry the same search
//! (stuck), a prompt that tempts a chat-only answer (acceptance), a needle task
//! whose big-file read overflows the result ceiling (cap/spill), and a
//! multi-file aggregation under a small declared context window (compactor).
//!
//! **Cost normalised by reliability** — `cost_of_pass` is the expected spend per
//! *correct* answer, not per run. A configuration that is cheap per attempt and
//! fails half the time is the expensive one. Tokens are weighted as GitHub's
//! Effective Tokens does (`1×in + 0.1×cached + 4×out`), so output — the thing
//! that actually costs — is not averaged away by a large prompt.
//!
//! ```sh
//! # full ablation, k=3 (the report's guard-attribution table needs H2 present)
//! BENCH_K=3 BENCH_LEVELS=H2,-stuck,-accept,-cap,-compact,+dedupe,H0 \
//!   cargo run -p eval-bench --bin bench-suite
//! ```
//!
//! ```sh
//! HARNESS_API_KEY=sk-... cargo run -p eval-bench --bin bench-suite
//! # or, matching the existing eval-bench:
//! DASHSCOPE_KEY=sk-... cargo run -p eval-bench --bin bench-suite
//! ```
use harness::prelude::*;
use harness_context::default_world;
use harness_core::compactor::{Budget, CompactionStage, Compactor};
use harness_core::error::CompactError;
use harness_core::{Context, Event, Hook, HookOutcome, Task};
use harness_loop::acceptance::FilesExist;
use harness_loop::{AgentLoop, Outcome, StuckPolicy, ToolResultPolicy};
use harness_models::OpenAiCompat;
use harness_tools_fs::{EditFile, Grep, ListDir, ReadFile, WriteFile};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One benchmark task: a prompt, the files it starts from, and a shell
/// assertion that decides — objectively — whether the end-state is correct.
struct BenchTask {
    id: &'static str,
    prompt: &'static str,
    /// (relative path, contents) written into a fresh workspace before the run.
    seed: &'static [(&'static str, &'static str)],
    /// `bash -c` snippet run in the workspace after the run. Exit 0 = resolved.
    verify: &'static str,
    /// Files [`FilesExist`] demands before the loop may say Done — wired only
    /// when the acceptance guard is on, so `-accept` measures its absence.
    accept_files: &'static [&'static str],
    /// Declared context window for this task's model (None = the model's real
    /// one). The compactor trap declares a small window so compaction *must*
    /// fire on a modest task instead of needing 150k tokens of filler.
    window: Option<u32>,
    /// Programmatic workspace seeding, for tasks whose files are too big to be
    /// literals. Runs after `seed`.
    setup: Option<fn(&std::path::Path)>,
}

/// The fields every ordinary task leaves at rest, so adding a knob for one
/// trap task doesn't rewrite the whole set.
const TASK_DEFAULTS: BenchTask = BenchTask {
    id: "",
    prompt: "",
    seed: &[],
    verify: "",
    accept_files: &[],
    window: None,
    setup: None,
};

/// 1200 log rows, one of them `status=fail` — big enough that reading it
/// whole is a real cost, generated at seed time rather than vendored as a
/// 68 KB string literal. The needle sits at row 00900: past `read_file`'s
/// first 16 KiB page, past the 24 KiB truncation head, and past the 4 KiB
/// spill preview, so no configuration finds it by accident in whatever the
/// first oversized result happened to keep. Finding it requires actually
/// searching — grep, or paging — which is the behaviour the task exists to
/// price. (Its first life had the needle at row 00150, *inside* the 24 KiB
/// truncation head: destructive truncation kept the answer by luck and the
/// task silently measured nothing.)
fn gen_biglog(ws: &std::path::Path) {
    let mut body = String::new();
    for i in 0..1200u32 {
        let status = if i == 900 { "fail" } else { "ok" };
        body.push_str(&format!(
            "row {i:05} value={} status={status} padding-padding-padding\n",
            i * 7
        ));
    }
    std::fs::write(ws.join("log.txt"), body).expect("seed log file");
}

/// The self-built Rust-native task set. Every task exercises the fs tools
/// (read/write/edit/list/grep) and has a deterministic, network-free verifier.
const TASKS: &[BenchTask] = &[
    BenchTask {
        id: "sum-file",
        prompt: "Read the file nums.txt in the workspace. It contains one integer \
                 per line. Compute their sum and write ONLY the resulting number \
                 (no other text) to a new file named sum.txt.",
        seed: &[("nums.txt", "10\n15\n17\n")],
        verify: r#"test "$(tr -d '[:space:]' < sum.txt)" = "42""#,
        ..TASK_DEFAULTS
    },
    BenchTask {
        id: "rename-key",
        prompt: "In config.json, rename the JSON key \"old_name\" to \"new_name\". \
                 Keep its value and every other key unchanged.",
        seed: &[(
            "config.json",
            "{\"old_name\": \"server-1\", \"port\": 3477}\n",
        )],
        verify: r#"grep -q '"new_name"' config.json && ! grep -q '"old_name"' config.json && grep -q 'server-1' config.json"#,
        ..TASK_DEFAULTS
    },
    BenchTask {
        id: "count-lines",
        prompt: "Count how many lines are in data.txt and write ONLY that count \
                 (a single number) to a file named count.txt.",
        seed: &[("data.txt", "a\nb\nc\nd\ne\nf\ng\n")],
        verify: r#"test "$(tr -d '[:space:]' < count.txt)" = "7""#,
        ..TASK_DEFAULTS
    },
    BenchTask {
        id: "fix-typo",
        prompt: "In notes.txt, replace every occurrence of the misspelling \"teh\" \
                 with the correct \"the\". Change nothing else.",
        seed: &[("notes.txt", "teh cat sat on teh mat\n")],
        verify: r#"! grep -q 'teh' notes.txt && grep -q 'the cat sat on the mat' notes.txt"#,
        ..TASK_DEFAULTS
    },
    // A file far larger than the answer needs: reading it whole is a real cost
    // that gets re-paid on every turn afterwards, which is what a ceiling on
    // tool results is for. The needle sits inside `read_file`'s own 16 KiB page
    // on purpose — put it past that and the task stops measuring the ceiling and
    // starts measuring whether the model thinks to paginate, which both levels
    // fail identically and which therefore measures nothing.
    BenchTask {
        id: "needle-in-big-file",
        prompt: "The file log.txt contains one line whose status is not ok. \
                 Write ONLY that line's row number (the 5-digit number after \
                 `row `) to a file named answer.txt.",
        seed: &[],
        setup: Some(gen_biglog),
        verify: r#"test "$(tr -d '[:space:]' < answer.txt)" = "00900""#,
        ..TASK_DEFAULTS
    },
    BenchTask {
        id: "create-readme",
        prompt: "Create a file named README.md whose contents include the exact \
                 word BENCHMARK in uppercase.",
        seed: &[],
        verify: r#"grep -q 'BENCHMARK' README.md"#,
        ..TASK_DEFAULTS
    },
    // ── guard traps ─────────────────────────────────────────────────────
    // Each of the next three exists to make one guard *fire*. Without them the
    // ablation only ever exercised the result cap: no task made the model
    // spin, none tempted it to narrate instead of act, none pressured the
    // window — and a guard that never fires shows up in the ablation as
    // "worthless", which is a statement about the task set, not the guard.
    //
    // Stuck trap. The prompt itself manufactures the loop: it asserts the
    // token appears intermittently and instructs the model to repeat the exact
    // same search until it matches — which it never will, because the token in
    // the file is spelled with a hyphen. The way out is to *disobey* the
    // retry instruction and look at the actual file, which is precisely what
    // the stuck-detector's nudge tells the model to do. With the guard off,
    // an instruction-following model greps until the iteration budget dies.
    BenchTask {
        id: "grep-retry-trap",
        prompt: "The file log.txt contains a secret token on a line of the form \
                 `SECRET_TOKEN=<value>`. Use grep to search log.txt for the exact \
                 pattern `SECRET_TOKEN=`. The log is written by another process and \
                 lines can appear at any moment, so if your search returns no match, \
                 run the exact same search again until it appears. Once you have it, \
                 write ONLY the token value (the part after the `=`) to a file named \
                 token.txt.",
        seed: &[(
            "log.txt",
            "boot ok\nservice up\nSECRET-TOKEN=q9x7kkp2\nheartbeat ok\n",
        )],
        verify: r#"test "$(tr -d '[:space:]' < token.txt)" = "q9x7kkp2""#,
        ..TASK_DEFAULTS
    },
    // Acceptance trap. The prompt buries the file requirement and asks to be
    // *told* the answer — the exact shape where a model answers in chat,
    // stops, and the run looks Done. `FilesExist("report.md")` is the check
    // that refuses that: the rejection goes back as an instruction and the
    // loop carries on. With the guard off, the chat answer sails through and
    // the verifier finds no file.
    BenchTask {
        id: "report-file-trap",
        prompt: "What is the largest value in the `amount` column of data.csv? \
                 Tell me the number. (For the record it should also end up in a \
                 file named report.md, but the main thing is that you tell me.)",
        seed: &[("data.csv", "id,amount\n1,450\n2,983\n3,120\n4,771\n")],
        verify: r#"grep -q '983' report.md"#,
        accept_files: &["report.md"],
        ..TASK_DEFAULTS
    },
    // Flood pressure, not a cap trap. First designed to trip the result
    // ceiling with a 200-match grep — then measurement showed it never fires:
    // `Grep` self-bounds (per-match preview cut + a total byte cap), so no
    // grep result can reach the ceiling at all. The ceiling's real trigger
    // surface is `read_file` with a model-chosen large `max_bytes` (the
    // needle task, which does fire it) and third-party tools the framework
    // does not control (the reason the guard exists). This task stays for
    // what it does measure: the per-turn context tax of a broad grep and
    // whether the model narrows after seeing bounded results.
    BenchTask {
        id: "grep-flood-trap",
        prompt: "Every line of flood.txt has the form `ITEM id=<id> qty=<q> ...`. \
                 Exactly one line has qty=0. First run a grep for `ITEM` to see \
                 the data format, then find the id of the line whose qty is 0 and \
                 write ONLY that id (the number) to a file named empty.txt.",
        seed: &[],
        verify: r#"test "$(tr -d '[:space:]' < empty.txt)" = "2777""#,
        setup: Some(gen_flood),
        ..TASK_DEFAULTS
    },
    // Compactor trap. Six ~14 KiB files must all be read under a declared
    // 16k-token window, so the context crosses the compaction high-water mark
    // mid-run and the stages actually execute. Each subtotal sits on the
    // *first* line of its file because `budget_reduce` keeps a result's head
    // when it trims — the task stays solvable through compaction, which is the
    // claim under test: the compactor's job is to shed bytes without shedding
    // the answer. `-compact` runs the same squeeze with a do-nothing
    // compactor; the model's real window absorbs it, so the row isolates what
    // compaction costs (or saves) rather than whether the provider errors.
    BenchTask {
        id: "wide-summary",
        prompt: "Each file in the parts/ directory starts with a line of the form \
                 `subtotal: N`. Read every file in parts/, add up all the subtotal \
                 values, and write ONLY the total (a single number) to a file named \
                 total.txt.",
        seed: &[],
        verify: r#"test "$(tr -d '[:space:]' < total.txt)" = "5300""#,
        window: Some(16_000),
        setup: Some(gen_wide_parts),
        ..TASK_DEFAULTS
    },
];

/// 2000 inventory lines, ~170 bytes each — long on purpose, so grep's default
/// 200 matches serialize past the 24 KiB result ceiling. One line has qty=0.
fn gen_flood(ws: &std::path::Path) {
    let mut body = String::new();
    for i in 0..2000u32 {
        let id = 1000 + i;
        let qty = if id == 2777 { 0 } else { (i % 9) + 1 };
        body.push_str(&format!(
            "ITEM id={id} qty={qty} sku=SKU-{id:06} loc=aisle-{:02} note=routine-inventory-record-padding-padding-padding-padding-padding-padding-padding-padding-nothing-unusual\n",
            i % 40
        ));
    }
    std::fs::write(ws.join("flood.txt"), body).expect("seed flood file");
}

/// Six log-like files, ~14 KiB each, `subtotal: N` on the first line. Together
/// they are ~20k tokens of reads — comfortably over the compaction high-water
/// mark of the declared 16k window, far under the model's real one.
fn gen_wide_parts(ws: &std::path::Path) {
    let dir = ws.join("parts");
    std::fs::create_dir_all(&dir).expect("create parts dir");
    let subtotals: [u32; 6] = [1200, 340, 905, 77, 2210, 568]; // = 5300
    for (i, st) in subtotals.iter().enumerate() {
        let mut body = format!("subtotal: {st}\n");
        for row in 0..150 {
            body.push_str(&format!(
                "entry {i:02}-{row:04} status=ok metric={:02} note=routine-batch-record-nothing-unusual-in-this-line\n",
                (row * 7 + i) % 100
            ));
        }
        std::fs::write(dir.join(format!("part{i}.txt")), body).expect("seed part file");
    }
}

/// Which guards this run switches on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Guards {
    stuck: bool,
    accept: bool,
    cap: bool,
    dedupe: bool,
    compact: bool,
    /// Oversized results spill to a workspace file (preview + locator) instead
    /// of being destructively truncated. Only meaningful while `cap` is on.
    spill: bool,
}

/// One configuration to measure: the shipped default, or the default with
/// exactly one guard removed (or, for dedupe, added).
///
/// This replaced the H0/H1/H2 ladder. The ladder demonstrated *that* the
/// scaffold matters and could not say *which guard* — and after dedupe was
/// measured off the default, H1 and H2 had quietly become the same
/// configuration, so the middle rung measured nothing at all. A leave-one-out
/// row differs from `H2` in exactly one guard; the delta is that guard's
/// contribution, provided it fired (see [`Fires`]).
///
/// `H0` is kept as the reference floor. It is not "no harness" — these tasks
/// need file tools to be doable at all — it is the loop with its judgement
/// switched off: nothing that second-guesses the model.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    /// Bare dispatch: every guard off.
    H0,
    /// Everything the loop defaults to (dedupe is *not* a default: it was
    /// measured to cost a task and switched off).
    H2,
    /// `H2` minus the stuck-detector.
    NoStuck,
    /// `H2` minus the acceptance checks (including per-task [`FilesExist`]).
    NoAccept,
    /// `H2` minus the ceiling on a single tool result.
    NoCap,
    /// `H2` minus the compactor (a do-nothing one takes its place).
    NoCompact,
    /// `H2` plus repeat suppression — the guard that lost its default; kept
    /// measurable so the decision to drop it stays a measurement, not lore.
    PlusDedupe,
    /// `H2` with spilling off: the ceiling still fires, but destructively —
    /// the tail is thrown away instead of saved to a retrievable file. The
    /// delta against `H2` is what *not losing the bytes* is worth.
    NoSpill,
}

impl Level {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "h0" => Some(Level::H0),
            "h2" => Some(Level::H2),
            "-stuck" => Some(Level::NoStuck),
            "-accept" => Some(Level::NoAccept),
            "-cap" => Some(Level::NoCap),
            "-compact" => Some(Level::NoCompact),
            "+dedupe" => Some(Level::PlusDedupe),
            "-spill" => Some(Level::NoSpill),
            _ => None,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Level::H0 => "H0 (guards off)",
            Level::H2 => "H2 (defaults)",
            Level::NoStuck => "H2 −stuck",
            Level::NoAccept => "H2 −accept",
            Level::NoCap => "H2 −cap",
            Level::NoCompact => "H2 −compact",
            Level::PlusDedupe => "H2 +dedupe",
            Level::NoSpill => "H2 −spill (truncate)",
        }
    }
    fn guards(self) -> Guards {
        let h2 = Guards {
            stuck: true,
            accept: true,
            cap: true,
            dedupe: false,
            compact: true,
            spill: true,
        };
        match self {
            Level::H0 => Guards {
                stuck: false,
                accept: false,
                cap: false,
                dedupe: false,
                compact: false,
                spill: false,
            },
            Level::H2 => h2,
            Level::NoStuck => Guards { stuck: false, ..h2 },
            Level::NoAccept => Guards {
                accept: false,
                ..h2
            },
            Level::NoCap => Guards { cap: false, ..h2 },
            Level::NoCompact => Guards {
                compact: false,
                ..h2
            },
            Level::PlusDedupe => Guards { dedupe: true, ..h2 },
            Level::NoSpill => Guards { spill: false, ..h2 },
        }
    }
}

/// GitHub's Effective Tokens weighting: output is what costs, and a cache read
/// is nearly free. Without it a run with a huge cached prompt and a short answer
/// looks more expensive than one that generated pages.
fn effective_tokens(input: u32, output: u32) -> f64 {
    // Cached input is not reported separately by every provider; when it is
    // folded into `input` this is conservative (counts it at full price).
    1.0 * input as f64 + 4.0 * output as f64
}

/// How often each guard actually fired during a run.
///
/// This is the validity condition for the whole ablation: `Δpass^k` for a
/// guard that never triggered is a statement about noise, not about the guard.
/// The report prints these next to every delta and flags the fired-zero rows.
#[derive(Clone, Copy, Default)]
struct Fires {
    /// Tool results cut destructively by the size ceiling (`cap`, spill off).
    truncated: u64,
    /// Tool results spilled to a workspace file by the ceiling (`cap`+`spill`).
    spilled: u64,
    /// Tool calls cancelled by the per-call deadline.
    deadlines: u64,
    /// Read-only repeats collapsed to a pointer (`dedupe`).
    repeats: u64,
    /// Stuck-detector "change your approach" injections.
    nudges: u64,
    /// Stuck-detector terminations.
    aborts: u64,
    /// Acceptance verdicts handed back as "not done yet".
    accept_fails: u64,
    /// Compaction stages that ran.
    compactions: u64,
    /// Context tokens those stages reclaimed.
    tokens_saved: u64,
}

impl Fires {
    fn add(&mut self, o: &Fires) {
        self.truncated += o.truncated;
        self.spilled += o.spilled;
        self.deadlines += o.deadlines;
        self.repeats += o.repeats;
        self.nudges += o.nudges;
        self.aborts += o.aborts;
        self.accept_fails += o.accept_fails;
        self.compactions += o.compactions;
        self.tokens_saved += o.tokens_saved;
    }
}

/// Global counters behind the tracing layer. The loop announces cap
/// truncations, dedupe hits, stuck nudges/aborts and acceptance rejections as
/// tracing events but not as hook [`Event`]s, so a layer is the only seam that
/// sees them without touching the loop. Runs are sequential; the bench
/// snapshots before/after each run and takes the difference.
static TRUNCATED: AtomicU64 = AtomicU64::new(0);
static SPILLED: AtomicU64 = AtomicU64::new(0);
static DEADLINES: AtomicU64 = AtomicU64::new(0);
static REPEATS: AtomicU64 = AtomicU64::new(0);
static NUDGES: AtomicU64 = AtomicU64::new(0);
static ABORTS: AtomicU64 = AtomicU64::new(0);
static ACCEPT_FAILS: AtomicU64 = AtomicU64::new(0);

fn fires_snapshot() -> Fires {
    Fires {
        truncated: TRUNCATED.load(Ordering::Relaxed),
        spilled: SPILLED.load(Ordering::Relaxed),
        deadlines: DEADLINES.load(Ordering::Relaxed),
        repeats: REPEATS.load(Ordering::Relaxed),
        nudges: NUDGES.load(Ordering::Relaxed),
        aborts: ABORTS.load(Ordering::Relaxed),
        accept_fails: ACCEPT_FAILS.load(Ordering::Relaxed),
        compactions: 0,
        tokens_saved: 0,
    }
}

/// Counts guard firings by watching the loop's own telemetry events.
struct FireLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for FireLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        #[derive(Default)]
        struct V {
            event_field: Option<String>,
            message: Option<String>,
        }
        impl tracing::field::Visit for V {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "event" {
                    self.event_field = Some(value.to_string());
                }
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                match field.name() {
                    "message" => self.message = Some(format!("{value:?}")),
                    "event" => self.event_field = Some(format!("{value:?}")),
                    _ => {}
                }
            }
        }
        let mut v = V::default();
        event.record(&mut v);
        if let Some(e) = &v.event_field {
            if e.contains("tool.result.truncated") {
                TRUNCATED.fetch_add(1, Ordering::Relaxed);
            } else if e.contains("tool.result.spilled") {
                SPILLED.fetch_add(1, Ordering::Relaxed);
            } else if e.contains("tool.result.repeat") {
                REPEATS.fetch_add(1, Ordering::Relaxed);
            } else if e.contains("tool.deadline") {
                DEADLINES.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(m) = &v.message {
            if m.contains("stuck: nudging") {
                NUDGES.fetch_add(1, Ordering::Relaxed);
            } else if m.contains("stuck: aborting") {
                ABORTS.fetch_add(1, Ordering::Relaxed);
            } else if m.contains("acceptance failed") {
                ACCEPT_FAILS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// The compactor ablation: reports zero pressure, so no stage ever runs.
struct NoopCompactor;

#[async_trait::async_trait]
impl Compactor for NoopCompactor {
    fn budget(&self, ctx: &Context) -> Budget {
        Budget {
            used: 0,
            window: ctx.policy.max_input_tokens,
        }
    }
    async fn compact(
        &self,
        _stage: CompactionStage,
        _ctx: &mut Context,
    ) -> Result<(), CompactError> {
        Ok(())
    }
}

/// Per-run outcome we report on.
struct Row {
    id: &'static str,
    resolved: bool,
    status: &'static str, // "resolved" | "wrong" | "timeout" | "error"
    iters: u32,
    tool_calls: usize,
    in_tok: u32,
    out_tok: u32,
    ms: u128,
    fires: Fires,
}

/// Records how many tool calls the loop made, and — because compaction *is* a
/// hook event — how often the compactor fired and what it reclaimed.
struct Capture {
    n: Arc<Mutex<usize>>,
    compactions: Arc<Mutex<u64>>,
    tokens_saved: Arc<Mutex<u64>>,
}
impl Hook for Capture {
    fn name(&self) -> &str {
        "bench-capture"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. } | Event::PostCompact { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        match ev {
            Event::PreToolUse { .. } => *self.n.lock().unwrap() += 1,
            Event::PostCompact { before, after, .. } => {
                *self.compactions.lock().unwrap() += 1;
                *self.tokens_saved.lock().unwrap() += before.saturating_sub(*after) as u64;
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}

/// Read HARNESS_* first, fall back to the eval-bench DASHSCOPE_* convention.
fn model_from_env() -> OpenAiCompat {
    let key = std::env::var("HARNESS_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("DASHSCOPE_KEY").ok())
        .expect("set HARNESS_API_KEY (or DASHSCOPE_KEY)");
    let base = std::env::var("HARNESS_BASE_URL")
        .unwrap_or_else(|_| "https://dashscope.aliyuncs.com/compatible-mode/v1".into());
    let model = std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "qwen3.7-plus".into());
    OpenAiCompat::with_key(base, model, key)
}

/// One tool round is one iteration; nothing here needs more than a handful.
/// Small enough that a run which loops (the stuck trap with the guard off)
/// fails *inside* the wall-clock timeout, so its token bill is still recorded
/// instead of vanishing into a timeout row of zeros.
const MAX_ITERS: u32 = 16;

async fn run_task(task: &BenchTask, level: Level, trial: u32) -> Row {
    // Fresh, isolated workspace per task, per trial — a second attempt must not
    // inherit the first one's files, or `pass^k` measures nothing after k=1.
    let ws = std::env::temp_dir().join(format!(
        "bench-suite-{}-{}-{level:?}-{trial}",
        std::process::id(),
        task.id
    ));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).expect("create workspace");
    for (rel, body) in task.seed {
        let full = ws.join(rel);
        if let Some(p) = full.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        std::fs::write(full, body).expect("seed file");
    }
    if let Some(setup) = task.setup {
        setup(&ws);
    }

    let n = Arc::new(Mutex::new(0usize));
    let compactions = Arc::new(Mutex::new(0u64));
    let tokens_saved = Arc::new(Mutex::new(0u64));
    let mut world = default_world(&ws);
    let started = Instant::now();
    let fires_before = fires_snapshot();

    let g = level.guards();
    let mut model = model_from_env();
    if let Some(w) = task.window {
        model = model.with_context_window(w);
    }
    let mut agent = AgentLoop::new(model)
        .with_tool(Arc::new(ReadFile))
        .with_tool(Arc::new(WriteFile))
        .with_tool(Arc::new(EditFile))
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(Grep))
        .with_hook(Arc::new(Capture {
            n: n.clone(),
            compactions: compactions.clone(),
            tokens_saved: tokens_saved.clone(),
        }))
        // Cap and dedupe live in the same policy; set both explicitly so a row
        // differs from H2 in exactly the guard it names.
        .with_tool_result_policy(ToolResultPolicy {
            max_bytes: if g.cap {
                ToolResultPolicy::default().max_bytes
            } else {
                None
            },
            dedupe_repeats: g.dedupe,
            spill: g.spill,
        });
    if !g.stuck {
        agent = agent.with_stuck_policy(StuckPolicy {
            enabled: false,
            ..Default::default()
        });
    }
    if g.accept {
        if !task.accept_files.is_empty() {
            agent = agent
                .with_acceptance(Arc::new(FilesExist::new(task.accept_files.iter().copied())))
                .with_acceptance_retries(2);
        }
    } else {
        agent = agent.with_acceptance_set(Vec::new());
    }
    if !g.compact {
        agent = agent.with_compactor(Arc::new(NoopCompactor));
    }
    let fut = agent.run_with_max_iters(
        Task {
            description: task.prompt.into(),
            source: None,
            deadline: None,
        },
        &mut world,
        MAX_ITERS,
    );

    // Runaway loops count as failures, not hangs — this is a metric, not a
    // crash. 120s fits an API model; a local one needs BENCH_TIMEOUT_SECS
    // raised, or every long task reads as "timeout" and its token bill as 0.
    let timeout_secs: u64 = std::env::var("BENCH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(120);
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), fut).await;
    let ms = started.elapsed().as_millis();
    let tool_calls = *n.lock().unwrap();

    let (status_run, iters, in_tok, out_tok) = match result {
        Ok(Ok(Outcome::Done { iters, usage, .. })) => {
            ("done", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Ok(Outcome::BudgetExhausted { iters, usage, .. })) => {
            ("budget", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Ok(Outcome::Stuck { iters, usage, .. })) => {
            ("stuck", iters, usage.input_tokens, usage.output_tokens)
        }
        Ok(Err(e)) => {
            eprintln!("  ! run error: {e}");
            ("error", 0, 0, 0)
        }
        Err(_) => ("timeout", 0, 0, 0),
    };

    // The verifier: an objective assertion we run ourselves, outside the agent.
    let verified = Command::new("bash")
        .arg("-c")
        .arg(task.verify)
        .current_dir(&ws)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let status = match (status_run, verified) {
        (_, true) => "resolved",
        ("timeout", false) => "timeout",
        ("error", false) => "error",
        ("stuck", false) => "stuck",
        (_, false) => "wrong",
    };

    let after = fires_snapshot();
    let fires = Fires {
        truncated: after.truncated - fires_before.truncated,
        spilled: after.spilled - fires_before.spilled,
        deadlines: after.deadlines - fires_before.deadlines,
        repeats: after.repeats - fires_before.repeats,
        nudges: after.nudges - fires_before.nudges,
        aborts: after.aborts - fires_before.aborts,
        accept_fails: after.accept_fails - fires_before.accept_fails,
        compactions: *compactions.lock().unwrap(),
        tokens_saved: *tokens_saved.lock().unwrap(),
    };

    Row {
        id: task.id,
        resolved: verified,
        status,
        iters,
        tool_calls,
        in_tok,
        out_tok,
        ms,
        fires,
    }
}

/// One level's aggregate over `k` trials of every task.
struct LevelReport {
    level: Level,
    /// Tasks resolved on the *first* trial — comparable to a single-shot run.
    pass_1: usize,
    /// Tasks resolved on **every** trial. The reliability floor.
    pass_k: usize,
    tasks: usize,
    trials: usize,
    resolved_trials: usize,
    effective_tokens: f64,
    tool_calls: usize,
    ms: u128,
    /// Summed guard firings across every run at this level — the evidence that
    /// an ablation delta is (or is not) about anything.
    fires: Fires,
    rows: Vec<Row>,
}

impl LevelReport {
    /// Expected effective-token spend per *correct* answer.
    ///
    /// Dividing by attempts instead would flatter a configuration that is cheap
    /// and usually wrong: that one pays again on the retry, and the retry is not
    /// in the average. `None` when nothing succeeded — there is no cost per pass
    /// when there are no passes, and reporting 0 or ∞ would both mislead.
    fn cost_of_pass(&self) -> Option<f64> {
        (self.resolved_trials > 0).then(|| self.effective_tokens / self.resolved_trials as f64)
    }
}

async fn run_level(level: Level, k: u32) -> LevelReport {
    let mut rows = Vec::new();
    let (mut pass_1, mut pass_k) = (0usize, 0usize);

    for task in TASKS {
        let mut resolved_each = Vec::new();
        for trial in 0..k {
            eprintln!("→ {} [{}] trial {}/{k}", task.id, level.label(), trial + 1);
            let row = run_task(task, level, trial).await;
            resolved_each.push(row.resolved);
            rows.push(row);
        }
        if resolved_each.first().copied().unwrap_or(false) {
            pass_1 += 1;
        }
        if resolved_each.iter().all(|r| *r) {
            pass_k += 1;
        }
    }

    let mut fires = Fires::default();
    for r in &rows {
        fires.add(&r.fires);
    }

    LevelReport {
        level,
        pass_1,
        pass_k,
        tasks: TASKS.len(),
        trials: k as usize,
        resolved_trials: rows.iter().filter(|r| r.resolved).count(),
        effective_tokens: rows
            .iter()
            .map(|r| effective_tokens(r.in_tok, r.out_tok))
            .sum(),
        tool_calls: rows.iter().map(|r| r.tool_calls).sum(),
        ms: rows.iter().map(|r| r.ms).sum(),
        fires,
        rows,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The fire counters watch the loop's tracing events; without this layer
    // every `fired` column reads zero and the ablation loses its validity
    // check.
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        tracing_subscriber::registry().with(FireLayer).init();
    }

    let k: u32 = std::env::var("BENCH_K")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);
    let levels: Vec<Level> = std::env::var("BENCH_LEVELS")
        .unwrap_or_else(|_| "H2".into())
        .split(',')
        .filter_map(Level::parse)
        .collect();
    let levels = if levels.is_empty() {
        vec![Level::H2]
    } else {
        levels
    };

    eprintln!(
        "{} tasks × {k} trial(s) × {} level(s)\n",
        TASKS.len(),
        levels.len()
    );

    let mut reports = Vec::new();
    for level in &levels {
        reports.push(run_level(*level, k).await);
    }

    println!("\n## Completion benchmark\n");
    println!(
        "{} tasks, {k} trial(s) each. `pass^k` counts a task only when **every** \
         trial resolved.\n",
        TASKS.len()
    );
    println!(
        "| level | pass@1 | pass^k | trials resolved | eff. tokens | cost/pass | tools | ms |"
    );
    println!("|---|--:|--:|--:|--:|--:|--:|--:|");
    for r in &reports {
        let cop = r
            .cost_of_pass()
            .map(|c| format!("{c:.0}"))
            .unwrap_or_else(|| "—".into());
        println!(
            "| {} | {}/{} | {}/{} | {}/{} | {:.0} | {} | {} | {} |",
            r.level.label(),
            r.pass_1,
            r.tasks,
            r.pass_k,
            r.tasks,
            r.resolved_trials,
            r.tasks * r.trials,
            r.effective_tokens,
            cop,
            r.tool_calls,
            r.ms
        );
    }

    // ── guard attribution ───────────────────────────────────────────────
    // Each ablation row against H2, with the firing count that decides whether
    // the delta is evidence. "fired" is counted on the run where the guard was
    // ON: the H2 run for the `-X` rows, the `+dedupe` run for dedupe.
    if let Some(h2) = reports.iter().find(|r| r.level == Level::H2) {
        let attributions: Vec<(&'static str, u64, &LevelReport, &LevelReport)> = reports
            .iter()
            .filter_map(|r| match r.level {
                Level::NoStuck => Some(("stuck", h2.fires.nudges + h2.fires.aborts, h2, r)),
                Level::NoAccept => Some(("acceptance", h2.fires.accept_fails, h2, r)),
                Level::NoCap => Some(("result-cap", h2.fires.truncated + h2.fires.spilled, h2, r)),
                Level::NoCompact => Some(("compactor", h2.fires.compactions, h2, r)),
                Level::NoSpill => Some(("spill", h2.fires.spilled, h2, r)),
                // Dedupe is measured the other way round: `on` is this row.
                Level::PlusDedupe => Some(("dedupe", r.fires.repeats, r, h2)),
                _ => None,
            })
            .collect();
        if !attributions.is_empty() {
            println!("\n### Guard attribution (leave-one-out vs H2)\n");
            println!(
                "`Δ` = with-guard − without-guard. A row whose guard never fired \
                 is noise, not a verdict — it means the task set failed to \
                 exercise the guard, and says nothing about the guard itself.\n"
            );
            println!(
                "| guard | fired | pass^k on | pass^k off | Δpass^k | cost/pass on | cost/pass off | valid |"
            );
            println!("|---|--:|--:|--:|--:|--:|--:|---|");
            for (name, fired, on, off) in &attributions {
                let cop = |r: &LevelReport| {
                    r.cost_of_pass()
                        .map(|c| format!("{c:.0}"))
                        .unwrap_or_else(|| "—".into())
                };
                println!(
                    "| {} | {} | {}/{} | {}/{} | {:+} | {} | {} | {} |",
                    name,
                    fired,
                    on.pass_k,
                    on.tasks,
                    off.pass_k,
                    off.tasks,
                    on.pass_k as i64 - off.pass_k as i64,
                    cop(on),
                    cop(off),
                    if *fired > 0 { "yes" } else { "⚠ never fired" }
                );
            }
        }
        // The traps exist to make guards fire under H2. If one didn't, the
        // suite has a coverage hole again — say so loudly, don't let the zero
        // masquerade as a measurement.
        let h2f = &h2.fires;
        for (guard, fired) in [
            ("stuck", h2f.nudges + h2f.aborts),
            ("acceptance", h2f.accept_fails),
            ("result-cap", h2f.truncated + h2f.spilled),
            ("compactor", h2f.compactions),
        ] {
            if fired == 0 {
                eprintln!(
                    "⚠ guard `{guard}` never fired under H2 — its trap task is not \
                     doing its job; any ablation delta for it is noise"
                );
            }
        }
    }

    // The per-run detail, for the shipped configuration (H2 when present).
    let shipped = reports
        .iter()
        .find(|r| r.level == Level::H2)
        .or(reports.last())
        .expect("at least one level");
    {
        println!("\n### {} — per run\n", shipped.level.label());
        println!("| task | status | iters | tools | in tok | out tok | ms | guard firings |");
        println!("|---|---|--:|--:|--:|--:|--:|---|");
        for r in &shipped.rows {
            let f = &r.fires;
            let mut fired = Vec::new();
            if f.truncated > 0 {
                fired.push(format!("cap×{}", f.truncated));
            }
            if f.spilled > 0 {
                fired.push(format!("spill×{}", f.spilled));
            }
            if f.deadlines > 0 {
                fired.push(format!("deadline×{}", f.deadlines));
            }
            if f.repeats > 0 {
                fired.push(format!("dedupe×{}", f.repeats));
            }
            if f.nudges > 0 {
                fired.push(format!("nudge×{}", f.nudges));
            }
            if f.aborts > 0 {
                fired.push(format!("abort×{}", f.aborts));
            }
            if f.accept_fails > 0 {
                fired.push(format!("accept×{}", f.accept_fails));
            }
            if f.compactions > 0 {
                fired.push(format!(
                    "compact×{} (−{} tok)",
                    f.compactions, f.tokens_saved
                ));
            }
            println!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                r.id,
                r.status,
                r.iters,
                r.tool_calls,
                r.in_tok,
                r.out_tok,
                r.ms,
                if fired.is_empty() {
                    "—".into()
                } else {
                    fired.join(", ")
                }
            );
        }
    }

    // Machine-readable summary for CI / regression tracking.
    let fires_json = |f: &Fires| {
        serde_json::json!({
            "truncated": f.truncated, "spilled": f.spilled,
            "deadlines": f.deadlines, "repeats": f.repeats,
            "nudges": f.nudges, "aborts": f.aborts,
            "accept_fails": f.accept_fails,
            "compactions": f.compactions, "tokens_saved": f.tokens_saved,
        })
    };
    let json = serde_json::json!({
        "suite": "rust-native-v2",
        "k": k,
        "model": std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "qwen3.7-plus".into()),
        "levels": reports.iter().map(|r| {
            let g = r.level.guards();
            serde_json::json!({
                "level": format!("{:?}", r.level),
                "guards": {
                    "stuck": g.stuck, "accept": g.accept, "cap": g.cap,
                    "dedupe": g.dedupe, "compact": g.compact,
                },
                "pass_at_1": r.pass_1,
                "pass_pow_k": r.pass_k,
                "tasks": r.tasks,
                "resolved_trials": r.resolved_trials,
                "total_trials": r.tasks * r.trials,
                "effective_tokens": r.effective_tokens,
                "cost_of_pass": r.cost_of_pass(),
                "tool_calls": r.tool_calls,
                "ms": r.ms,
                "fires": fires_json(&r.fires),
                "runs": r.rows.iter().map(|x| serde_json::json!({
                    "id": x.id, "resolved": x.resolved, "status": x.status,
                    "iters": x.iters, "tool_calls": x.tool_calls,
                    "input_tokens": x.in_tok, "output_tokens": x.out_tok, "ms": x.ms,
                    "fires": fires_json(&x.fires),
                })).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    });
    eprintln!("\nJSON: {}", serde_json::to_string(&json)?);

    // CI gates on the reliability floor of the *shipped* configuration — H2
    // when present, not whatever level happened to run last.
    if shipped.pass_k < shipped.tasks {
        std::process::exit(1);
    }
    Ok(())
}
