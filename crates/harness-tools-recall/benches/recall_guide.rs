//! Per-turn cost of `RecallGuide`, and the price of its CJK fallback.
//!
//! Plain wall-clock timing (no criterion in this workspace); warmed, then N
//! timed iterations, reported as min / median / mean.
//!
//! The interesting question is not the happy path — it is what a *miss* costs.
//! `search_with_cjk_fallback` issues the verbatim query, and if that misses and
//! the query contains han characters it decomposes the query into
//! non-overlapping chunks of 4, then 3, then 2 characters, probing the store
//! with each, up to `MAX_CJK_PROBES = 12` extra round-trips.
//!
//! Two things make that more than a constant factor:
//!
//! - the backend routes a query with ≥3 han chars to the FTS5 trigram table,
//!   but a **2-char** chunk falls through to `content LIKE '%…%'`, which is a
//!   full table scan;
//! - the number of probes depends on query length, so a long question and a
//!   short one fail at different prices.
//!
//! A `CountingRecall` decorator counts the actual `search` calls, so the probe
//! counts below are observed, not derived from reading the constant.
//!
//! Everything runs against an in-memory SQLite. No network, no model.

use async_trait::async_trait;
use harness_context::default_world;
use harness_core::{
    Block, Context, Guide, RecallError, RecallMessage, RecallStore, SessionHit, SessionMeta, Task,
    World,
};
use harness_recall_sqlite::SqliteRecall;
use harness_tools_recall::{RECALL_OWNER_KEY, RecallGuide};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const OWNER: &str = "alice";

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

// ───── a store that counts its own round-trips ────────────────────────────

struct CountingRecall {
    inner: Arc<dyn RecallStore>,
    searches: AtomicUsize,
}

impl CountingRecall {
    fn new(inner: Arc<dyn RecallStore>) -> Self {
        Self {
            inner,
            searches: AtomicUsize::new(0),
        }
    }
    fn take(&self) -> usize {
        self.searches.swap(0, Ordering::SeqCst)
    }
}

#[async_trait]
impl RecallStore for CountingRecall {
    async fn ensure_session(
        &self,
        owner: &str,
        session_id: &str,
        meta: &SessionMeta,
    ) -> Result<(), RecallError> {
        self.inner.ensure_session(owner, session_id, meta).await
    }
    async fn append(
        &self,
        owner: &str,
        session_id: &str,
        msg: &RecallMessage,
    ) -> Result<i64, RecallError> {
        self.inner.append(owner, session_id, msg).await
    }
    async fn search(
        &self,
        owner: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionHit>, RecallError> {
        self.searches.fetch_add(1, Ordering::SeqCst);
        self.inner.search(owner, query, limit).await
    }
    async fn scroll(
        &self,
        owner: &str,
        session_id: &str,
        around: i64,
        window: usize,
    ) -> Result<Vec<RecallMessage>, RecallError> {
        self.inner.scroll(owner, session_id, around, window).await
    }
    async fn recent(&self, owner: &str, limit: usize) -> Result<Vec<SessionMeta>, RecallError> {
        self.inner.recent(owner, limit).await
    }
}

// ───── corpus ─────────────────────────────────────────────────────────────

/// Half English, half Chinese, spread over sessions of 100 messages, so both
/// the FTS path and the trigram path have something real to chew on.
async fn corpus(n: usize) -> Arc<CountingRecall> {
    let inner: Arc<dyn RecallStore> = Arc::new(SqliteRecall::open_in_memory().unwrap());
    let mut ts = 1_700_000_000_000i64;
    let per_session = 100;
    for i in 0..n {
        let sid = format!("s{}", i / per_session);
        if i % per_session == 0 {
            inner
                .ensure_session(OWNER, &sid, &SessionMeta::new(&sid, ts))
                .await
                .unwrap();
        }
        let content = if i % 2 == 0 {
            format!(
                "message {i}: we agreed the billing service would retry payment webhooks with an \
                 idempotency key, and the deploy runbook covers the rollback"
            )
        } else {
            format!(
                "第 {i} 条：我们上次说的那家咖啡馆在南京西路，明天的复盘会先讲支付服务的重试策略"
            )
        };
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        inner
            .append(OWNER, &sid, &RecallMessage::new(role, content, ts))
            .await
            .unwrap();
        ts += 1_000;
    }
    Arc::new(CountingRecall::new(inner))
}

fn world() -> World {
    let mut w = default_world(".");
    w.profile
        .extra
        .insert(RECALL_OWNER_KEY.into(), json!(OWNER));
    w
}

fn ctx(q: &str) -> Context {
    Context::new(Task {
        description: q.to_string(),
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

// ───── queries ────────────────────────────────────────────────────────────

struct Case {
    label: &'static str,
    query: &'static str,
}

const CASES: &[Case] = &[
    // Verbatim FTS hit, one store call.
    Case {
        label: "ascii hit (1 probe)",
        query: "billing webhooks idempotency",
    },
    // Contiguous han substring: trigram matches verbatim, one store call.
    Case {
        label: "cjk verbatim hit",
        query: "支付服务的重试策略",
    },
    // The shape the fallback exists for: no transcript contains this phrase
    // verbatim, but chunking it lands the hit.
    Case {
        label: "cjk fallback hit",
        query: "上次说的咖啡",
    },
    // Nothing matches, and the query is short, so the chunk ladder reaches
    // 2-char chunks — which the backend answers with a LIKE full table scan.
    Case {
        label: "cjk miss, short query",
        query: "紫色犀牛",
    },
    // Nothing matches and the query is long: the 12-probe ceiling is what stops
    // it, not running out of chunks. This is the worst case.
    Case {
        label: "cjk miss, long query",
        query: "紫色犀牛骑着独轮车穿过撒哈拉沙漠去参加钢琴比赛结果迟到了三个小时非常抱歉",
    },
    // The actual worst case, and it is not the one with the most probes. A
    // 2-char chunk has `count_cjk < 3`, so the backend does NOT use the trigram
    // index — it falls through to `content LIKE '%..%'`, a full table scan. A
    // 12-han-char query spends 3 probes on 4-char chunks and 4 on 3-char
    // chunks, leaving 5 of the 12-probe budget for 2-char chunks: five full
    // table scans in one turn.
    Case {
        label: "cjk miss, 5 LIKE scans",
        query: "紫色犀牛独轮车撒哈拉沙漠",
    },
    // Control: an ASCII miss takes the fallback's early exit (no han chars) —
    // but the backend still does one LIKE scan after FTS misses.
    Case {
        label: "ascii miss (1 probe)",
        query: "purple rhinoceros unicycle sahara",
    },
];

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    println!("\n=== RecallGuide (harness-tools-recall) — per-turn injection + CJK fallback ===");
    println!("backend: SqliteRecall (in-memory), top_k=2, path = Guide::apply\n");

    for &n in &[10usize, 1_000, 50_000] {
        let t = Instant::now();
        let store = rt.block_on(corpus(n));
        let build = t.elapsed();
        println!("-- corpus n={n} messages (built in {build:.2?})");

        let store_dyn: Arc<dyn RecallStore> = store.clone();
        let guide = RecallGuide::new(store_dyn).with_owner(OWNER);
        let w = world();

        for case in CASES {
            // One instrumented run for the probe count and the rendered size.
            store.take();
            let mut probe = ctx(case.query);
            rt.block_on(guide.apply(&mut probe, &w)).unwrap();
            let probes = store.take();
            let chars = injected_chars(&probe);

            let iters = match (n, probes) {
                (50_000, p) if p > 1 => 10,
                (50_000, _) => 50,
                (1_000, _) => 200,
                _ => 500,
            };
            let s = bench_async(&rt, 2, iters, || {
                let (guide, w) = (&guide, &w);
                async move {
                    let mut c = ctx(case.query);
                    guide.apply(&mut c, w).await.unwrap();
                    std::hint::black_box(&c);
                }
            });
            println!(
                "   {:<24} store calls={probes:<3} {s}   rendered {chars} chars",
                case.label
            );
        }
        println!();
    }
}
