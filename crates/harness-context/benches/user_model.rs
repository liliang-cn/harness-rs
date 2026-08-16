//! Per-turn cost of `UserModelGuide` — the user-portrait injection.
//!
//! Plain wall-clock timing (no criterion in this workspace); warmed, then N
//! timed iterations, reported as min / median / mean.
//!
//! Three separate things get measured, because they have wildly different
//! shapes and lumping them together would hide the answer:
//!
//! - **`UserModel::render_within`** — the pure formatting + eviction sort, at
//!   portrait sizes from empty to far past the 1200-char budget. This is the
//!   only part that runs when the guide is warm.
//! - **`UserModelGuide::apply` cold** — first injection of a session: one
//!   `UserModelStore::load`, i.e. one `Memory::recall(fetch_k=64)` plus a JSON
//!   deserialise per candidate. Scales with the *memory store*, not the
//!   portrait.
//! - **`UserModelGuide::apply_before_iter` warm** — every subsequent turn. The
//!   guide caches the rendered string, so this should be a strip + a clone.
//!
//! No network, no model: the LLM-backed `UserModelUpdater` is out of scope.

use harness_context::user_model::{
    CommField, ConstraintKind, GoalStatus, IdentityField, Observation, PortraitPolicy, UserId,
    UserModel, UserModelDelta, UserModelGuide, UserModelStore,
};
use harness_context::{FileMemory, default_world};
use harness_core::{Block, Context, Guide, Memory, MemoryEntry, Task};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DAY: i64 = 86_400_000;
const NOW: i64 = DAY * 30;

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

fn bench_async<F, Fut>(rt: &tokio::runtime::Runtime, warmup: usize, iters: usize, mut f: F) -> Stats
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for _ in 0..warmup {
        rt.block_on(f());
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        rt.block_on(f());
        samples.push(t.elapsed());
    }
    summarise(samples)
}

// ───── portraits ──────────────────────────────────────────────────────────

/// A portrait with `people` relationships and `n_q` open questions on top of a
/// realistic core. `merge` prunes each collection to `max_items_stored`, so to
/// build a genuinely oversized portrait we bypass the policy cap by raising it.
fn portrait(user: &str, people: usize, n_q: usize, cap: usize) -> UserModel {
    let policy = PortraitPolicy {
        max_items_stored: cap.max(24),
        ..PortraitPolicy::default()
    };
    let mut obs = vec![
        Observation::Constraint {
            mode: ConstraintKind::Never,
            rule: "suggest switching to Kubernetes; the cluster decision is settled".into(),
            scope: None,
            confidence: 0.92,
        },
        Observation::Constraint {
            mode: ConstraintKind::Always,
            rule: "show the failing test output before proposing a fix".into(),
            scope: Some("code review".into()),
            confidence: 0.88,
        },
        Observation::Communication {
            field: CommField::Language,
            value: "zh-CN".into(),
            confidence: 0.95,
        },
        Observation::Identity {
            field: IdentityField::Role,
            value: "solo founder, ships the backend and the iOS client herself".into(),
            confidence: 0.85,
        },
        Observation::Identity {
            field: IdentityField::Timezone,
            value: "Asia/Shanghai".into(),
            confidence: 0.9,
        },
        Observation::Goal {
            title: "ship the multi-tenant server before the pilot customer onboards".into(),
            status: GoalStatus::Active,
            detail: None,
            confidence: 0.8,
        },
    ];
    for i in 0..people {
        obs.push(Observation::Relationship {
            name: format!("colleague number {i}"),
            relation: format!("collaborator on the {i}th side project, reviews the infra changes"),
            note: None,
            confidence: 0.9 - (i as f32 * 0.0005).min(0.7),
        });
    }
    for i in 0..n_q {
        obs.push(Observation::OpenQuestion {
            question: format!(
                "does she want deploys gated on the {i}th integration suite, or only on smoke?"
            ),
            why: None,
            confidence: 0.7,
        });
    }
    let mut m = UserModel::new(UserId::new(user));
    m.merge(
        &UserModelDelta {
            observations: obs,
            ..Default::default()
        },
        DAY,
        &policy,
    );
    m
}

fn tmpdir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("harness-bench-um-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A JSONL memory holding `noise` unrelated entries plus one saved portrait.
fn store_with_noise(
    dir: &std::path::Path,
    name: &str,
    noise: usize,
    model: &UserModel,
) -> Arc<UserModelStore> {
    use std::io::Write;
    let path = dir.join(format!("{name}.jsonl"));
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        for i in 0..noise {
            let mut e = MemoryEntry::new(format!(
                "note {i}: unrelated durable fact about the billing service and its deploy runbook"
            ))
            .with_source("bench")
            .with_tags(["bench", "misc"]);
            e.id = format!("n{i:08}");
            e.created_ms = 1_700_000_000_000 + i as i64;
            writeln!(f, "{}", serde_json::to_string(&e).unwrap()).unwrap();
        }
        f.flush().unwrap();
    }
    let mem: Arc<dyn Memory> = Arc::new(FileMemory::open(&path).unwrap());
    let store = Arc::new(UserModelStore::new(mem));
    // Save through the real path so the tags/《content》 shape is authentic.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(store.save(model)).unwrap();
    store
}

fn ctx() -> Context {
    Context::new(Task {
        description: "do a thing".into(),
        source: None,
        deadline: None,
    })
}

fn injected_chars(c: &Context) -> usize {
    c.guides
        .iter()
        .map(|b| match b {
            Block::Text(t) => t.chars().count(),
            _ => 0,
        })
        .sum()
}

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tmpdir();
    let policy = PortraitPolicy::default();

    println!("\n=== UserModelGuide (harness-context) — per-turn portrait injection ===");
    println!("render budget: {} chars\n", policy.render_budget_chars);

    // ---- 1. render_within, by portrait size -------------------------------
    println!("-- UserModel::render_within (pure CPU: candidate build + tier sort + greedy fill)");
    for &(label, people, qs, cap) in &[
        ("empty", 0usize, 0usize, 24usize),
        ("small (6 items)", 0, 0, 24),
        ("default cap (24/collection)", 24, 24, 24),
        ("oversized (200)", 200, 200, 1_000),
        ("absurd (1000)", 1_000, 1_000, 5_000),
    ] {
        let m = if label == "empty" {
            UserModel::new(UserId::new("alice"))
        } else {
            portrait("alice", people, qs, cap)
        };
        let stored: usize = m.constraints.len()
            + m.relationships.len()
            + m.open_questions.len()
            + m.goals.len()
            + m.expertise.len()
            + m.communication.notes.len();
        let out = m.render_within(NOW, &policy, policy.render_budget_chars);
        let chars = out.as_ref().map(|s| s.chars().count()).unwrap_or(0);
        let s = bench_batched(20, 200, 50, || {
            std::hint::black_box(m.render_within(NOW, &policy, policy.render_budget_chars));
        });
        println!("   {label:<28} stored={stored:<5}  {s}   rendered {chars} chars");
    }

    // ---- 2. guide, cold (session start) vs warm (every later turn) --------
    println!("\n-- UserModelGuide over FileMemory: cold load vs warm cached re-injection");
    let m = portrait("alice", 24, 24, 24);
    for &noise in &[10usize, 1_000, 50_000] {
        let store = store_with_noise(&dir, &format!("um-{noise}"), noise, &m);

        // cold: a fresh guide per iteration, so the cache is always empty.
        let iters = if noise >= 50_000 { 20 } else { 100 };
        let cold = bench_async(&rt, 3, iters, || {
            let store = store.clone();
            async move {
                let g = UserModelGuide::new(store, UserId::new("alice")).with_now_ms(NOW);
                let mut c = ctx();
                g.apply(&mut c, &default_world(".")).await.unwrap();
                std::hint::black_box(&c);
            }
        });

        // warm: one guide, re-injecting as the loop would every iteration.
        let g = Arc::new(UserModelGuide::new(store, UserId::new("alice")).with_now_ms(NOW));
        let mut probe = ctx();
        rt.block_on(g.apply(&mut probe, &default_world(".")))
            .unwrap();
        let chars = injected_chars(&probe);
        let w = default_world(".");
        let warm = bench_async(&rt, 100, 2_000, || {
            let (g, w) = (g.clone(), &w);
            async move {
                let mut c = ctx();
                g.apply_before_iter(&mut c, w).await.unwrap();
                std::hint::black_box(&c);
            }
        });

        println!("   cold  store={noise:<6}  {cold}   rendered {chars} chars");
        println!("   warm  store={noise:<6}  {warm}   rendered {chars} chars");
    }

    // ---- 3. what an unknown user costs ------------------------------------
    println!("\n-- an unknown user (nothing stored): still pays the store read");
    for &noise in &[1_000usize, 50_000] {
        let store = store_with_noise(&dir, &format!("um-miss-{noise}"), noise, &m);
        let iters = if noise >= 50_000 { 20 } else { 100 };
        let s = bench_async(&rt, 3, iters, || {
            let store = store.clone();
            async move {
                let g = UserModelGuide::new(store, UserId::new("nobody")).with_now_ms(NOW);
                let mut c = ctx();
                g.apply(&mut c, &default_world(".")).await.unwrap();
                assert!(c.guides.is_empty());
                std::hint::black_box(&c);
            }
        });
        println!("   cold miss  store={noise:<6}  {s}   rendered 0 chars");
    }

    let _ = std::fs::remove_dir_all(&dir);
    println!();
}
