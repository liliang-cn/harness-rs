//! Allocation profile of `SkillDistiller::gate`.
//!
//! Separate binary from `distill_gate` on purpose: the counting global
//! allocator below adds a couple of atomics to every malloc/free, which would
//! quietly inflate the timings if the two shared a process.
//!
//! What this answers: is `gate` "allocation-light"? The rejection paths should
//! allocate nothing at all (they compare integers and build a `SkipReason` on
//! the stack); the accept path allocates inside `nearest`, once per existing
//! skill, and this quantifies it.

use harness_core::{Context, Model, ModelError, ModelInfo, ModelOutput};
use harness_experience::{Episode, ExistingSkill, GateDecision, SkillDistiller};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ───── counting allocator ─────────────────────────────────────────────────

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(l.size(), Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new.saturating_sub(l.size()), Ordering::Relaxed);
        unsafe { System.realloc(p, l, new) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn measure<F: FnMut()>(reps: usize, mut f: F) -> (f64, f64) {
    let a0 = ALLOCS.load(Ordering::Relaxed);
    let b0 = BYTES.load(Ordering::Relaxed);
    for _ in 0..reps {
        f();
    }
    let a = ALLOCS.load(Ordering::Relaxed) - a0;
    let b = BYTES.load(Ordering::Relaxed) - b0;
    (a as f64 / reps as f64, b as f64 / reps as f64)
}

// ───── fixtures (same as distill_gate) ────────────────────────────────────

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

fn main() {
    let distiller = SkillDistiller::new(Arc::new(TripwireModel));
    let ep = passing_episode();

    println!("\n=== SkillDistiller::gate — allocations per call ===");
    println!("(counting global allocator; alloc + realloc events, per gate() call)\n");

    for &n in &[0usize, 50, 500] {
        let existing = skills(n);
        assert_eq!(distiller.gate(&ep, &existing), GateDecision::Distill);
        let (a, b) = measure(200, || {
            std::hint::black_box(distiller.gate(std::hint::black_box(&ep), &existing));
        });
        println!("gate → Distill   existing={n:<4}   {a:>9.1} allocs   {b:>11.0} bytes");
    }

    let big = skills(500);
    let reject = Episode::new(passing_episode().situation, "done")
        .with_tools(["shell"])
        .with_success(true);
    assert!(matches!(
        distiller.gate(&reject, &big),
        GateDecision::Skip(_)
    ));
    let (a, b) = measure(2_000, || {
        std::hint::black_box(distiller.gate(std::hint::black_box(&reject), &big));
    });
    println!("gate → Skip(early)  existing=500   {a:>9.1} allocs   {b:>11.0} bytes");

    let short = Episode::new("tiny", "done")
        .with_tools(["read_file", "shell", "write_file", "http_get"])
        .with_success(true);
    let (a, b) = measure(2_000, || {
        std::hint::black_box(distiller.gate(std::hint::black_box(&short), &big));
    });
    println!("gate → Skip(SituationTooShort)     {a:>9.1} allocs   {b:>11.0} bytes");

    let calls = MODEL_CALLS.load(Ordering::SeqCst);
    println!("\nmodel calls: {calls}");
    assert_eq!(calls, 0, "gate must never reach the model");
    println!();
}
