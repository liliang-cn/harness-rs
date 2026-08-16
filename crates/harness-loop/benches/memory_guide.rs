//! Per-turn cost of `MemoryGuide` — the semantic-recall injection that runs on
//! every model turn.
//!
//! Plain wall-clock timing (no criterion in this workspace). Each case is
//! warmed, then run N times; we report min / median / mean so a single OS
//! hiccup can't masquerade as a trend.
//!
//! Two backends are measured on purpose:
//!
//! - `FileMemory` — the JSONL store that actually ships. Its `recall` reads and
//!   parses the *entire* file per call, so this is where store size shows up.
//! - `NullMemory` — an O(1) stub that hands back a fixed slice. Subtracting it
//!   from the FileMemory number isolates the guide's own work (filter, score,
//!   format, strip-and-push) from the backend's.
//!
//! Nothing here touches the network or a model.

use harness_core::{Block, Context, Guide, Memory, MemoryEntry, MemoryError, Task, Turn, TurnRole};
use harness_loop::MemoryGuide;
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

fn summarise(mut samples: Vec<Duration>) -> Stats {
    samples.sort();
    let n = samples.len();
    let total: Duration = samples.iter().sum();
    Stats {
        n,
        min: samples[0],
        median: samples[n / 2],
        mean: total / n as u32,
    }
}

/// Run `f` `warmup + iters` times, discard the warmup, summarise the rest.
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

// ───── backends ───────────────────────────────────────────────────────────

/// O(1) stub: ignores the query, returns the first `k` of a fixed list. Exists
/// so the guide's own cost can be separated from the backend's.
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

// ───── corpora ────────────────────────────────────────────────────────────

const ASCII_QUERY: &str = "how should the billing service handle retries on payment webhooks";
const CJK_QUERY: &str = "结算服务在支付回调重试的时候应该怎么处理幂等问题";
/// A contiguous term that genuinely appears in `cjk_entry`.
const CJK_SUBSTRING: &str = "支付回调重试";

fn ascii_entry(i: usize) -> MemoryEntry {
    MemoryEntry::new(format!(
        "note {i}: the billing service retries payment webhooks with an idempotency key derived \
         from the provider event id; the deploy runbook for release {i} covers the rollback path"
    ))
    .with_source("bench")
    .with_tags(["bench", if i.is_multiple_of(3) { "billing" } else { "misc" }])
}

fn cjk_entry(i: usize) -> MemoryEntry {
    MemoryEntry::new(format!(
        "笔记 {i}：结算服务在处理支付回调重试时使用幂等键，键由渠道事件号推导；\
         第 {i} 次发布的运维手册里写了回滚步骤和值班联系人"
    ))
    .with_source("bench")
    .with_tags(["bench", "结算"])
}

fn tmpdir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("harness-bench-mem-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Build a JSONL file with `n` entries by writing the lines directly — going
/// through `Memory::write` would be O(n) file opens and dominate setup.
fn file_memory_with(
    dir: &std::path::Path,
    name: &str,
    entries: Vec<MemoryEntry>,
) -> Arc<dyn Memory> {
    use std::io::Write;
    let path = dir.join(format!("{name}.jsonl"));
    let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    for mut e in entries {
        if e.id.is_empty() {
            e.id = format!("{:016x}", fastrand_ish());
        }
        if e.created_ms == 0 {
            e.created_ms = 1_700_000_000_000;
        }
        writeln!(f, "{}", serde_json::to_string(&e).unwrap()).unwrap();
    }
    f.flush().unwrap();
    drop(f);
    Arc::new(harness_context::FileMemory::open(&path).unwrap())
}

/// Tiny xorshift so entry ids are distinct without pulling in a rand crate.
fn fastrand_ish() -> u64 {
    use std::cell::Cell;
    thread_local!(static S: Cell<u64> = const { Cell::new(0x2545F4914F6CDD1D) });
    S.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x
    })
}

fn ctx_with_user_turn(query: &str) -> Context {
    let mut c = Context::new(Task {
        description: query.to_string(),
        source: None,
        deadline: None,
    });
    c.history.push(Turn {
        role: TurnRole::User,
        blocks: vec![Block::Text(query.to_string())],
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

// ───── main ───────────────────────────────────────────────────────────────

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tmpdir();

    println!("\n=== MemoryGuide (harness-loop) — per-turn injection cost ===");
    println!("machine: {}", machine());
    println!("path measured: Guide::apply_before_iter (the every-turn one)\n");

    // ---- A. guide overhead alone, over an O(1) backend --------------------
    {
        let canned: Vec<MemoryEntry> = (0..5).map(ascii_entry).collect();
        let mem: Arc<dyn Memory> = Arc::new(NullMemory { canned });
        let guide = MemoryGuide::new(mem).with_top_k(5);
        let w = harness_context::default_world(".");
        let mut probe = ctx_with_user_turn(ASCII_QUERY);
        rt.block_on(guide.apply_before_iter(&mut probe, &w))
            .unwrap();
        let chars = injected_chars(&probe);

        let s = bench_async(&rt, 200, 2_000, || {
            let guide = &guide;
            let w = &w;
            async move {
                let mut c = ctx_with_user_turn(ASCII_QUERY);
                guide.apply_before_iter(&mut c, w).await.unwrap();
                std::hint::black_box(&c);
            }
        });
        println!("guide-only (O(1) stub backend, top_k=5)   {s}   rendered {chars} chars");
    }

    // ---- B. over the real FileMemory, by store size -----------------------
    for &n in &[10usize, 1_000, 50_000] {
        let mem = file_memory_with(
            &dir,
            &format!("ascii-{n}"),
            (0..n).map(ascii_entry).collect(),
        );
        let guide = MemoryGuide::new(mem).with_top_k(5);
        let w = harness_context::default_world(".");

        let mut probe = ctx_with_user_turn(ASCII_QUERY);
        rt.block_on(guide.apply_before_iter(&mut probe, &w))
            .unwrap();
        let chars = injected_chars(&probe);

        let iters = if n >= 50_000 { 20 } else { 200 };
        let s = bench_async(&rt, 3, iters, || {
            let guide = &guide;
            let w = &w;
            async move {
                let mut c = ctx_with_user_turn(ASCII_QUERY);
                guide.apply_before_iter(&mut c, w).await.unwrap();
                std::hint::black_box(&c);
            }
        });
        println!("FileMemory ascii  n={n:<6}                {s}   rendered {chars} chars");
    }

    // ---- C. with filters on (over-fetch 3x + rescoring) -------------------
    for &n in &[1_000usize, 50_000] {
        let mem = file_memory_with(
            &dir,
            &format!("filt-{n}"),
            (0..n).map(ascii_entry).collect(),
        );
        let guide = MemoryGuide::new(mem)
            .with_top_k(5)
            .with_min_score(0.2)
            .with_excluded_tags(["secret"]);
        let w = harness_context::default_world(".");
        let mut probe = ctx_with_user_turn(ASCII_QUERY);
        rt.block_on(guide.apply_before_iter(&mut probe, &w))
            .unwrap();
        let chars = injected_chars(&probe);
        let iters = if n >= 50_000 { 20 } else { 200 };
        let s = bench_async(&rt, 3, iters, || {
            let guide = &guide;
            let w = &w;
            async move {
                let mut c = ctx_with_user_turn(ASCII_QUERY);
                guide.apply_before_iter(&mut c, w).await.unwrap();
                std::hint::black_box(&c);
            }
        });
        println!("FileMemory + filters n={n:<6}             {s}   rendered {chars} chars");
    }

    // ---- D. CJK corpus: same store size, different char/token profile -----
    //
    // Two queries against the *same* Chinese corpus:
    //
    //   CJK_QUERY      — a natural sentence. `FileMemory::tokenise` splits on
    //                    non-alphanumerics, and every han char is alphanumeric,
    //                    so a space-free sentence collapses to ONE token that
    //                    is a substring of nothing. Expect 0 chars rendered.
    //   CJK_SUBSTRING  — a contiguous 4-char term that really is a substring.
    //
    // The gap between them is the point: the cost is paid either way.
    for &n in &[1_000usize, 50_000] {
        let mem = file_memory_with(&dir, &format!("cjk-{n}"), (0..n).map(cjk_entry).collect());
        let guide = MemoryGuide::new(mem).with_top_k(5);
        let w = harness_context::default_world(".");
        for (label, q) in [("sentence", CJK_QUERY), ("substring", CJK_SUBSTRING)] {
            let mut probe = ctx_with_user_turn(q);
            rt.block_on(guide.apply_before_iter(&mut probe, &w))
                .unwrap();
            let chars = injected_chars(&probe);
            let iters = if n >= 50_000 { 20 } else { 200 };
            let s = bench_async(&rt, 3, iters, || {
                let guide = &guide;
                let w = &w;
                async move {
                    let mut c = ctx_with_user_turn(q);
                    guide.apply_before_iter(&mut c, w).await.unwrap();
                    std::hint::black_box(&c);
                }
            });
            println!("FileMemory CJK {label:<9} n={n:<6}      {s}   rendered {chars} chars");
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    println!();
}

fn machine() -> String {
    std::process::Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
