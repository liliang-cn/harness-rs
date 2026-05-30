//! The in-process scheduler. Ticks on an interval; for each due job, runs the
//! job's prompt as a Subagent turn, then delivers the output via the job's
//! channel. Best-effort per job — one failure never stops the scheduler.

use crate::channel::{Channel, ChannelRegistry};
use crate::store::JobStore;
use harness_core::{Model, Task, Tool};
use harness_daemon::Schedule;
use harness_loop::{Subagent, SubagentSpec};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct Scheduler {
    store: Arc<dyn JobStore>,
    model: Arc<dyn Model>,
    tools: Vec<Arc<dyn Tool>>,
    channels: ChannelRegistry,
    repo_root: PathBuf,
    tick: Duration,
    max_iters: u32,
}

impl Scheduler {
    pub fn new(store: Arc<dyn JobStore>, model: Arc<dyn Model>) -> Self {
        Self {
            store,
            model,
            tools: Vec::new(),
            channels: ChannelRegistry::new(),
            repo_root: PathBuf::from("."),
            tick: Duration::from_secs(60),
            max_iters: 20,
        }
    }
    pub fn with_tool(mut self, t: Arc<dyn Tool>) -> Self {
        self.tools.push(t);
        self
    }
    pub fn with_channel(mut self, c: Arc<dyn Channel>) -> Self {
        self.channels.register(c);
        self
    }
    pub fn with_tick(mut self, d: Duration) -> Self {
        self.tick = d;
        self
    }
    pub fn with_max_iters(mut self, n: u32) -> Self {
        self.max_iters = n;
        self
    }
    pub fn with_repo_root(mut self, p: impl Into<PathBuf>) -> Self {
        self.repo_root = p.into();
        self
    }

    /// Spawn the tick loop on a dedicated thread with its own single-threaded
    /// Tokio runtime. Runs forever. The Subagent future is not `Send`, so we
    /// cannot use `tokio::spawn`; a dedicated thread sidesteps the requirement.
    pub fn spawn(self) {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scheduler: build tokio rt");
            rt.block_on(async move {
                loop {
                    let _ = self.tick_once().await;
                    tokio::time::sleep(self.tick).await;
                }
            });
        });
    }

    /// Run every currently-due job once. Returns how many fired.
    pub async fn tick_once(&self) -> usize {
        let now = chrono::Local::now();
        let now_ms = now.timestamp_millis();
        let jobs = match self.store.list().await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "scheduler: list jobs failed");
                return 0;
            }
        };
        let mut fired = 0;
        for job in jobs {
            if !job.enabled {
                continue;
            }
            let due = job.next_run_ms.map(|t| t <= now_ms).unwrap_or(true);
            if !due {
                continue;
            }
            fired += 1;

            // Run the job prompt as a Subagent turn (best-effort).
            let mut world = harness_context::default_world(self.repo_root.clone());
            let mut spec = SubagentSpec::new(
                job.name.clone(),
                Task {
                    description: job.prompt.clone(),
                    source: None,
                    deadline: None,
                },
            )
            .with_max_iters(self.max_iters);
            for t in &self.tools {
                spec = spec.with_tool(t.clone());
            }
            let sub = Subagent::new(harness_core::DynModel(self.model.clone()), spec);
            let output = match sub.run(&mut world).await {
                Ok(report) => report.text.unwrap_or_default(),
                Err(e) => {
                    tracing::warn!(job = %job.name, error = %e, "scheduler: job run failed");
                    String::new()
                }
            };

            // Deliver unless [SILENT] / empty.
            let trimmed = output.trim();
            if !trimmed.is_empty() && trimmed != "[SILENT]" {
                match self.channels.get(&job.channel) {
                    Some(ch) => {
                        if let Err(e) = ch.send(&output, &job).await {
                            tracing::warn!(job = %job.name, error = %e, "scheduler: delivery failed");
                        }
                    }
                    None => {
                        tracing::warn!(job = %job.name, channel = %job.channel, "scheduler: unknown channel")
                    }
                }
            }

            // Advance next_run.
            let next = Schedule::parse(&job.schedule)
                .ok()
                .map(|s| s.next_after(now).timestamp_millis());
            if let Err(e) = self.store.record_run(&job.id, now_ms, next).await {
                tracing::warn!(job = %job.name, error = %e, "scheduler: record_run failed");
            }
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{Channel, ChannelError};
    use crate::store::{FileJobStore, Job, JobStore};
    use async_trait::async_trait;
    use harness_core::{Context, ModelError, ModelInfo, ModelOutput, StopReason, Usage};
    use std::sync::Mutex as StdMutex;

    fn mi() -> ModelInfo {
        ModelInfo {
            handle: "mock".into(),
            provider: "mock".into(),
            model: "mock".into(),
            context_window: 8192,
            input_cost_usd_per_million_tokens: None,
            output_cost_usd_per_million_tokens: None,
            supports_tool_use: false,
            supports_streaming: false,
        }
    }

    struct SayModel {
        text: String,
    }
    #[async_trait]
    impl Model for SayModel {
        async fn complete(&self, _c: &Context) -> Result<ModelOutput, ModelError> {
            Ok(ModelOutput {
                text: Some(self.text.clone()),
                tool_calls: vec![],
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
                reasoning: None,
            })
        }
        fn info(&self) -> ModelInfo {
            mi()
        }
    }

    #[derive(Default)]
    struct CapturingChannel {
        sent: Arc<StdMutex<Vec<String>>>,
    }
    #[async_trait]
    impl Channel for CapturingChannel {
        fn key(&self) -> &str {
            "cap"
        }
        async fn send(&self, output: &str, _job: &Job) -> Result<(), ChannelError> {
            self.sent.lock().unwrap().push(output.to_string());
            Ok(())
        }
    }

    fn tmp() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("harness-sched-{}-{n}.json", std::process::id()))
    }

    #[tokio::test]
    async fn due_job_runs_and_delivers_and_advances() {
        let p = tmp();
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(&p).unwrap());
        let job = Job::new("daily-brief", "daily 08:00", "write the brief", "cap", 1)
            .with_next_run(Some(0));
        store.add(&job).await.unwrap();

        let captured = Arc::new(StdMutex::new(Vec::new()));
        let cap = CapturingChannel {
            sent: captured.clone(),
        };
        let model: Arc<dyn Model> = Arc::new(SayModel {
            text: "the brief".into(),
        });
        let sched = Scheduler::new(store.clone(), model)
            .with_channel(Arc::new(cap))
            .with_repo_root(".");

        let fired = sched.tick_once().await;
        assert_eq!(fired, 1);
        assert_eq!(
            captured.lock().unwrap().as_slice(),
            &["the brief".to_string()]
        );
        let j = store.get(&job.id).await.unwrap().unwrap();
        assert!(j.next_run_ms.unwrap() > chrono::Local::now().timestamp_millis() - 1000);
        assert!(j.last_run_ms.is_some());
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn silent_output_is_not_delivered() {
        let p = tmp();
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(&p).unwrap());
        store
            .add(&Job::new("j", "daily 08:00", "p", "cap", 1).with_next_run(Some(0)))
            .await
            .unwrap();
        let captured = Arc::new(StdMutex::new(Vec::new()));
        let sched = Scheduler::new(
            store,
            Arc::new(SayModel {
                text: "[SILENT]".into(),
            }),
        )
        .with_channel(Arc::new(CapturingChannel {
            sent: captured.clone(),
        }));
        sched.tick_once().await;
        assert!(
            captured.lock().unwrap().is_empty(),
            "[SILENT] suppresses delivery"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[tokio::test]
    async fn future_job_does_not_fire() {
        let p = tmp();
        let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(&p).unwrap());
        let future = chrono::Local::now().timestamp_millis() + 10_000_000;
        store
            .add(&Job::new("j", "daily 08:00", "p", "cap", 1).with_next_run(Some(future)))
            .await
            .unwrap();
        let sched = Scheduler::new(store, Arc::new(SayModel { text: "x".into() }))
            .with_channel(Arc::new(CapturingChannel::default()));
        assert_eq!(sched.tick_once().await, 0);
        let _ = std::fs::remove_file(&p);
    }
}
