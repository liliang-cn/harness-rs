//! Per-turn cost of `ExperienceGuide` — the similar-past-episodes injection
//! that runs on every model turn.
//!
//! Plain wall-clock timing (no criterion in this workspace); warmed, then N
//! timed iterations, reported as min / median / mean.
//!
//! Measured over two backends:
//!
//! - `FileMemory` — the shipped JSONL store. `ExperienceStore::recall`
//!   over-fetches `2*k`, then filters on the `experience` tag and re-parses
//!   each hit with `Episode::parse`, so there is per-hit work on top of the
//!   backend scan.
//! - `NullMemory` — an O(1) stub, to isolate the guide's own cost.
//!
//! No network, no model.

use harness_context::FileMemory;
use harness_core::{Block, Context, Guide, Memory, MemoryEntry, MemoryError, Task, Turn, TurnRole};
use harness_experience::{EXPERIENCE_TAG, Episode, ExperienceGuide, ExperienceStore};
use std::sync::Arc;
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

// ───── backends / corpora ─────────────────────────────────────────────────

struct NullMemory {
    canned: Vec<MemoryEntry>,
}

#[async_trait::async_trait]
impl Memory for NullMemory {
    async fn recall(&self, _q: &str, k: usize) -> Result<Vec<MemoryEntry>, MemoryError> {
        Ok(self.canned.iter().take(k).cloned().collect())
    }
    async fn write(&self, _e: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
}

const QUERY: &str = "deploy the billing service to production and verify the rollout";

fn episode(i: usize) -> Episode {
    Episode::new(
        format!(
            "deploy the billing service release {i} to production after the migration lands and \
             verify the rollout against the smoke suite"
        ),
        format!(
            "ran the migration, deployed release {i}, smoke suite green, traffic shifted in two \
             steps with a five minute soak between them"
        ),
    )
    .with_tools(["read_file", "shell", "write_file", "http_get"])
    .with_skills(["deploy-runbook"])
    .with_success(true)
}

/// The exact `MemoryEntry` shape `ExperienceStore::record` writes, produced
/// directly so setup isn't O(n) file opens.
fn entry_for(ep: &Episode) -> MemoryEntry {
    let mut tags = vec![EXPERIENCE_TAG.to_string()];
    tags.extend(ep.tools.iter().map(|t| format!("tool:{t}")));
    tags.extend(ep.skills.iter().map(|s| format!("skill:{s}")));
    MemoryEntry::new(ep.render())
        .with_source("experience")
        .with_tags(tags)
}

fn tmpdir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("harness-bench-exp-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn file_memory_with(dir: &std::path::Path, name: &str, n: usize) -> Arc<dyn Memory> {
    use std::io::Write;
    let path = dir.join(format!("{name}.jsonl"));
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for i in 0..n {
        let mut e = entry_for(&episode(i));
        e.id = format!("e{i:08}");
        e.created_ms = 1_700_000_000_000 + i as i64;
        writeln!(f, "{}", serde_json::to_string(&e).unwrap()).unwrap();
    }
    f.flush().unwrap();
    drop(f);
    Arc::new(FileMemory::open(&path).unwrap())
}

fn ctx_with_user_turn(q: &str) -> Context {
    let mut c = Context::new(Task {
        description: q.to_string(),
        source: None,
        deadline: None,
    });
    c.history.push(Turn {
        role: TurnRole::User,
        blocks: vec![Block::Text(q.to_string())],
    });
    c
}

fn injected_chars(ctx: &Context) -> usize {
    ctx.guides
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

    println!("\n=== ExperienceGuide (harness-experience) — per-turn injection cost ===");
    println!("path measured: Guide::apply_before_iter (the every-turn one), top_k=3\n");

    // guide overhead alone
    {
        let canned: Vec<MemoryEntry> = (0..6).map(|i| entry_for(&episode(i))).collect();
        let store = Arc::new(ExperienceStore::new(Arc::new(NullMemory { canned })));
        let guide = ExperienceGuide::new(store);
        let w = harness_context::default_world(".");
        let mut probe = ctx_with_user_turn(QUERY);
        rt.block_on(guide.apply_before_iter(&mut probe, &w))
            .unwrap();
        let chars = injected_chars(&probe);
        let s = bench_async(&rt, 200, 2_000, || {
            let (guide, w) = (&guide, &w);
            async move {
                let mut c = ctx_with_user_turn(QUERY);
                guide.apply_before_iter(&mut c, w).await.unwrap();
                std::hint::black_box(&c);
            }
        });
        println!("guide-only (O(1) stub backend)      {s}   rendered {chars} chars");
    }

    for &n in &[10usize, 1_000, 50_000] {
        let mem = file_memory_with(&dir, &format!("exp-{n}"), n);
        let store = Arc::new(ExperienceStore::new(mem));
        let guide = ExperienceGuide::new(store);
        let w = harness_context::default_world(".");
        let mut probe = ctx_with_user_turn(QUERY);
        rt.block_on(guide.apply_before_iter(&mut probe, &w))
            .unwrap();
        let chars = injected_chars(&probe);
        let iters = if n >= 50_000 { 20 } else { 200 };
        let s = bench_async(&rt, 3, iters, || {
            let (guide, w) = (&guide, &w);
            async move {
                let mut c = ctx_with_user_turn(QUERY);
                guide.apply_before_iter(&mut c, w).await.unwrap();
                std::hint::black_box(&c);
            }
        });
        println!("FileMemory  n={n:<6}                 {s}   rendered {chars} chars");
    }

    let _ = std::fs::remove_dir_all(&dir);
    println!();
}
