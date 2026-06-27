//! `LoopScheduler` — runs registered loops on their declared cadence.
//!
//! This is the "automations / scheduling" building block: it ticks on an
//! interval and, for every loop whose cadence is due, runs one round and
//! delivers the report through a [`LoopSink`]. It mirrors
//! `harness-scheduler`'s execution model (a dedicated thread with its own
//! current-thread Tokio runtime, because a sub-agent future is not `Send`)
//! but understands whole loop *rounds* — maker, checker, budget, gate —
//! rather than single agent turns.
//!
//! Registering a production pattern is one line:
//!
//! ```ignore
//! let engine = LoopEngine::new(patterns::daily_triage(), model)
//!     .with_maker_tool(read_only_tool);
//! LoopScheduler::new().register(engine).spawn(); // ticks forever
//! ```

use crate::engine::{LoopEngine, RoundReport};
use harness_daemon::Schedule;
use std::sync::Arc;
use std::time::Duration;

/// Where a finished round's report goes. Implement this to route reports to
/// Slack, email, a file, a tracker — anywhere. The default is stdout.
pub trait LoopSink: Send + Sync {
    fn deliver(&self, report: &RoundReport);
}

/// Prints deliverable reports to stdout.
pub struct StdoutSink;

impl LoopSink for StdoutSink {
    fn deliver(&self, report: &RoundReport) {
        println!("{}", report.render());
    }
}

struct ScheduledLoop {
    engine: LoopEngine,
    schedule: Schedule,
    /// Next fire time in epoch-ms; `None` means "due now".
    next_run_ms: Option<i64>,
}

/// Ticks registered loops on their cadence.
pub struct LoopScheduler {
    entries: Vec<ScheduledLoop>,
    tick: Duration,
    sink: Arc<dyn LoopSink>,
}

impl Default for LoopScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopScheduler {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            tick: Duration::from_secs(60),
            sink: Arc::new(StdoutSink),
        }
    }

    /// How often the scheduler wakes to check for due loops. A loop never
    /// fires more often than this, regardless of its cadence.
    pub fn with_tick(mut self, d: Duration) -> Self {
        self.tick = d;
        self
    }

    pub fn with_sink(mut self, s: Arc<dyn LoopSink>) -> Self {
        self.sink = s;
        self
    }

    /// Register a loop. Its `spec().cadence` is parsed into a schedule; an
    /// unparseable cadence is rejected so the problem surfaces at wiring
    /// time rather than silently never firing.
    pub fn register(mut self, engine: LoopEngine) -> Self {
        match Schedule::parse(&engine.spec().cadence) {
            Ok(schedule) => self.entries.push(ScheduledLoop {
                engine,
                schedule,
                next_run_ms: None,
            }),
            Err(e) => {
                tracing::warn!(
                    loop = %engine.spec().name,
                    cadence = %engine.spec().cadence,
                    error = %e,
                    "loop-scheduler: bad cadence, loop not registered"
                );
            }
        }
        self
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Run every currently-due loop once. Returns how many fired. Best
    /// effort: one loop's failure never stops the others (the engine itself
    /// never returns `Err`).
    pub async fn tick_once(&mut self) -> usize {
        let now = chrono::Local::now();
        let now_ms = now.timestamp_millis();
        let mut fired = 0;
        for entry in &mut self.entries {
            let due = entry.next_run_ms.map(|t| t <= now_ms).unwrap_or(true);
            if !due {
                continue;
            }
            fired += 1;
            let report = entry.engine.run_once().await;
            if report.should_deliver() {
                self.sink.deliver(&report);
            }
            entry.next_run_ms = Some(entry.schedule.next_after(now).timestamp_millis());
        }
        fired
    }

    /// Spawn the tick loop on a dedicated thread with its own single-threaded
    /// Tokio runtime. Runs forever.
    pub fn spawn(mut self) {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("loop-scheduler: build tokio rt");
            rt.block_on(async move {
                loop {
                    let _ = self.tick_once().await;
                    tokio::time::sleep(self.tick).await;
                }
            });
        });
    }
}
