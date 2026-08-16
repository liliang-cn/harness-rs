//! Cost of `SkillDistiller::gate` — the post-episode trigger check.
//!
//! Three claims are under test:
//!
//! 1. **Zero model calls on rejection.** The distiller holds an `Arc<dyn Model>`;
//!    this bench hands it a model that increments a counter and returns an error
//!    if it is ever called. The counter is asserted to be 0 at the end.
//! 2. **Zero I/O.** `gate(&self, ep, existing: &[ExistingSkill])` receives the
//!    skills list already in memory, so it has nothing to read. What *is* I/O is
//!    `existing_skills_in(root)`, the disk scan that builds that slice — a
//!    separate call, measured separately here, because a host that rebuilds the
//!    list per episode pays it per episode.
//! 3. **Linear in skill count.** The duplicate check (`nearest`) is a scan:
//!    every existing skill gets a `format!` and a fresh tokenisation of *both*
//!    sides. Measured at 0 / 50 / 500.
//!
//! Allocation counts live in the sibling `distill_gate_allocs` bench, so the
//! counting allocator cannot skew the timings here.

use harness_core::{Context, Model, ModelError, ModelInfo, ModelOutput};
use harness_experience::{
    Episode, ExistingSkill, GateDecision, SkillDistiller, existing_skills_in,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// ───── timing helper ──────────────────────────────────────────────────────

struct Stats {
    n: usize,
    min: Duration,
    median: Duration,
    mean: Duration,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "min {:>10.3?}  median {:>10.3?}  mean {:>10.3?}  (n={})",
            self.min, self.median, self.mean, self.n
        )
    }
}

fn summarise(mut s: Vec<Duration>) -> Stats {
    s.sort();
    let n = s.len();
    let total: Duration = s.iter().sum();
    Stats {
        n,
        min: s[0],
        median: s[n / 2],
        mean: total / n as u32,
    }
}

/// Time `iters` *batches* of `inner` calls and divide, so a sub-microsecond
/// operation isn't measuring `Instant::now` resolution.
fn bench_batched<F: FnMut()>(warmup: usize, iters: usize, inner: usize, mut f: F) -> Stats {
    for _ in 0..warmup * inner {
        f();
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        for _ in 0..inner {
            f();
        }
        samples.push(t.elapsed() / inner as u32);
    }
    summarise(samples)
}

// ───── a model that must never be called ──────────────────────────────────

static MODEL_CALLS: AtomicUsize = AtomicUsize::new(0);

struct TripwireModel;

#[async_trait::async_trait]
impl Model for TripwireModel {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            handle: "tripwire".into(),
            provider: "bench".into(),
            model: "tripwire".into(),
            context_window: 0,
            input_cost_usd_per_million_tokens: None,
            output_cost_usd_per_million_tokens: None,
            supports_tool_use: false,
            supports_streaming: false,
            supports_web_grounding: false,
        }
    }
    async fn complete(&self, _ctx: &Context) -> Result<ModelOutput, ModelError> {
        MODEL_CALLS.fetch_add(1, Ordering::SeqCst);
        Err(ModelError::Invalid("gate must not reach the model".into()))
    }
}

// ───── fixtures ───────────────────────────────────────────────────────────

/// A realistic episode that clears every arithmetic check, so the gate always
/// runs all the way to the duplicate scan — the part that is linear in skills.
fn passing_episode() -> Episode {
    Episode::new(
        "roll out the multi-tenant billing migration to production, verify the invoice totals \
         against last month, and hold traffic at ten percent until the smoke suite is green",
        "migration applied, invoices reconciled, traffic ramped in three steps over forty minutes",
    )
    .with_tools([
        "read_file",
        "shell",
        "write_file",
        "http_get",
        "shell",
        "read_file",
    ])
    .with_success(true)
}

/// Existing skills that are deliberately *not* about billing, so `nearest`
/// never short-circuits and the full list is always scanned. (`nearest` uses
/// `max_by` over a filtered iterator — it has no early exit either way, but
/// keeping them unrelated also keeps the decision `Distill`.)
fn skills(n: usize) -> Vec<ExistingSkill> {
    (0..n)
        .map(|i| {
            ExistingSkill::new(
                format!("unrelated-procedure-{i}"),
                format!(
                    "Use when the operator needs to rotate the {i}th warehouse credential set, \
                     reconcile the shipping manifest, or re-index the catalogue search shards \
                     after a bulk import has landed."
                ),
            )
        })
        .collect()
}

fn write_skills_root(n: usize) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "harness-bench-skills-{}-{nanos}",
        std::process::id()
    ));
    for s in skills(n) {
        let dir = root.join(&s.name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: {}\ndescription: {}\n---\n\n# {}\n\nSteps go here.\n",
                s.name, s.description, s.name
            ),
        )
        .unwrap();
    }
    root
}

fn main() {
    let distiller = SkillDistiller::new(Arc::new(TripwireModel));

    println!("\n=== SkillDistiller::gate (harness-experience) — post-episode trigger ===");
    println!("model: a tripwire that counts calls; asserted 0 at the end\n");

    // ---- 1. accept path, by existing-skill count --------------------------
    let ep = passing_episode();
    for &n in &[0usize, 50, 500] {
        let existing = skills(n);
        // Sanity: this really is the full-scan path.
        assert_eq!(distiller.gate(&ep, &existing), GateDecision::Distill);
        let s = bench_batched(50, 200, 20, || {
            std::hint::black_box(distiller.gate(std::hint::black_box(&ep), &existing));
        });
        println!("gate → Distill      existing skills={n:<4}  {s}");
    }

    // ---- 2. rejection paths (never reach the duplicate scan) --------------
    let rejects: [(&str, Episode); 4] = [
        (
            "NotSuccessful",
            Episode::new(passing_episode().situation, "it failed").with_tools([
                "read_file",
                "shell",
                "write_file",
                "http_get",
            ]),
        ),
        (
            "TooFewToolCalls",
            Episode::new(passing_episode().situation, "done")
                .with_tools(["shell"])
                .with_success(true),
        ),
        (
            "TooFewDistinctTools",
            Episode::new(passing_episode().situation, "done")
                .with_tools(["shell", "shell", "shell", "shell", "shell"])
                .with_success(true),
        ),
        (
            "SituationTooShort",
            Episode::new("tiny", "done")
                .with_tools(["read_file", "shell", "write_file", "http_get"])
                .with_success(true),
        ),
    ];
    let big = skills(500);
    for (label, ep) in &rejects {
        assert!(
            matches!(distiller.gate(ep, &big), GateDecision::Skip(_)),
            "{label} should be a Skip"
        );
        let s = bench_batched(50, 200, 20, || {
            std::hint::black_box(distiller.gate(std::hint::black_box(ep), &big));
        });
        println!("gate → Skip({label:<20})  existing=500  {s}");
    }

    // ---- 3. duplicate path: the scan finds a match ------------------------
    {
        let mut existing = skills(500);
        existing.push(ExistingSkill::new(
            "billing-migration-rollout",
            "roll out the multi tenant billing migration to production verify the invoice totals \
             against last month and hold traffic at ten percent until the smoke suite is green",
        ));
        assert!(matches!(
            distiller.gate(&ep, &existing),
            GateDecision::Skip(_)
        ));
        let s = bench_batched(50, 200, 20, || {
            std::hint::black_box(distiller.gate(std::hint::black_box(&ep), &existing));
        });
        println!("gate → Skip(DuplicateSkill)      existing=501  {s}");
    }

    // ---- 4. the I/O that is NOT in gate: building the skills list ---------
    println!();
    for &n in &[0usize, 50, 500] {
        let root = write_skills_root(n);
        let found = existing_skills_in(&root);
        assert_eq!(found.len(), n, "scan should find every skill written");
        let iters = if n >= 500 { 20 } else { 50 };
        let s = bench_batched(2, iters, 1, || {
            std::hint::black_box(existing_skills_in(&root));
        });
        println!("existing_skills_in(root)  skills={n:<4}  {s}   <-- disk scan, NOT part of gate");
        let _ = std::fs::remove_dir_all(&root);
    }

    let calls = MODEL_CALLS.load(Ordering::SeqCst);
    println!("\nmodel calls made by every gate() above: {calls}");
    assert_eq!(calls, 0, "gate must never reach the model");
    println!();
}
