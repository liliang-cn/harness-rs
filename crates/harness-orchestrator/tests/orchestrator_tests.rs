//! End-to-end orchestrator tests driven by a trivial in-process `JobRunner`
//! (no model needed) so the scheduling, dependency, retry, replan, resume,
//! and budget logic is exercised deterministically.

use async_trait::async_trait;
use harness_orchestrator::{
    Backoff, Dag, InMemoryRunStore, Job, JobError, JobId, JobResult, JobRunner, JobState,
    Orchestrator, PlanDelta, Planner, PlannerError, RetryPolicy, Run, RunBudget, RunState,
    RunStore,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A runner backed by a closure over shared state. Records the order Jobs
/// start in, and can be told to fail a Job's first N attempts.
#[derive(Default)]
struct TestRunner {
    started: Mutex<Vec<String>>,
    /// job_id -> number of attempts that should fail before succeeding
    fail_attempts: Mutex<HashMap<String, u32>>,
    /// tokens reported per Job
    tokens_per_job: u64,
}

impl TestRunner {
    fn new() -> Self {
        Self::default()
    }
    fn with_tokens(mut self, t: u64) -> Self {
        self.tokens_per_job = t;
        self
    }
    fn fail_first(self, job: &str, times: u32) -> Self {
        self.fail_attempts.lock().unwrap().insert(job.into(), times);
        self
    }
    fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }
}

#[async_trait(?Send)]
impl JobRunner for TestRunner {
    async fn run(&self, job: &Job, deps: &[(JobId, JobResult)]) -> Result<JobResult, JobError> {
        self.started.lock().unwrap().push(job.id.clone());
        {
            let mut f = self.fail_attempts.lock().unwrap();
            if let Some(remaining) = f.get_mut(&job.id)
                && *remaining > 0
            {
                *remaining -= 1;
                return Err(JobError::Run(format!("boom {}", job.id)));
            }
        }
        let text = format!("done:{} (deps={})", job.id, deps.len());
        Ok(JobResult::new(text).with_tokens(self.tokens_per_job, 0))
    }
}

fn job(id: &str, deps: &[&str]) -> Job {
    Job::new(id, format!("do {id}")).with_deps(deps.iter().copied())
}

fn order_pos(order: &[String], id: &str) -> usize {
    order.iter().position(|x| x == id).expect("job ran")
}

#[tokio::test]
async fn diamond_dag_respects_dependencies_and_fans_out() {
    // a -> {b, c} -> d
    let dag = Dag::from_jobs([
        job("a", &[]),
        job("b", &["a"]),
        job("c", &["a"]),
        job("d", &["b", "c"]),
    ]);
    let runner = Arc::new(TestRunner::new());
    let orch = Orchestrator::new(runner.clone());
    let report = orch.run(Run::new("r1", "diamond", dag)).await;

    assert_eq!(report.state, RunState::Completed);
    assert_eq!(report.succeeded(), 4);
    let order = runner.started();
    // a before b and c; b and c before d.
    assert!(order_pos(&order, "a") < order_pos(&order, "b"));
    assert!(order_pos(&order, "a") < order_pos(&order, "c"));
    assert!(order_pos(&order, "b") < order_pos(&order, "d"));
    assert!(order_pos(&order, "c") < order_pos(&order, "d"));
    // d saw both upstream results.
    let d_text = report
        .jobs
        .iter()
        .find(|(id, _, _)| id == "d")
        .and_then(|(_, _, t)| t.clone())
        .unwrap();
    assert!(d_text.contains("deps=2"), "d should see 2 deps: {d_text}");
}

#[tokio::test]
async fn retry_then_succeed() {
    let dag = Dag::from_jobs([job("a", &[])]);
    let runner = Arc::new(TestRunner::new().fail_first("a", 1)); // fail once, then succeed
    let orch = Orchestrator::new(runner.clone());
    let mut run = Run::new("r2", "retry", dag);
    run.dag.get_mut("a").unwrap().retry = RetryPolicy::new(3, Backoff::None);

    let report = orch.run(run).await;
    assert_eq!(report.state, RunState::Completed);
    assert_eq!(report.succeeded(), 1);
    // ran twice: 1 failure + 1 success
    assert_eq!(runner.started().iter().filter(|x| *x == "a").count(), 2);
}

#[tokio::test]
async fn dead_letter_cancels_dependents() {
    // a always fails (max_attempts=1); b depends on a.
    let dag = Dag::from_jobs([job("a", &[]), job("b", &["a"])]);
    let runner = Arc::new(TestRunner::new().fail_first("a", 99)); // always fails
    let orch = Orchestrator::new(runner.clone());
    let report = orch.run(Run::new("r3", "deadletter", dag)).await;

    assert_eq!(report.state, RunState::Failed);
    let state_of = |id: &str| report.jobs.iter().find(|(j, _, _)| j == id).unwrap().1;
    assert_eq!(state_of("a"), JobState::DeadLettered);
    assert_eq!(state_of("b"), JobState::Cancelled);
    // b never started.
    assert!(!runner.started().contains(&"b".to_string()));
}

/// Planner that, once it sees `a` succeeded, adds `b` (depends on a) exactly
/// once, then reports Done.
struct AddBAfterA {
    added: Mutex<bool>,
}
#[async_trait(?Send)]
impl Planner for AddBAfterA {
    async fn plan(
        &self,
        _goal: &str,
        succeeded: &[(JobId, JobResult)],
    ) -> Result<PlanDelta, PlannerError> {
        let mut added = self.added.lock().unwrap();
        if !*added && succeeded.iter().any(|(id, _)| id == "a") {
            *added = true;
            Ok(PlanDelta::Add(vec![job("b", &["a"])]))
        } else {
            Ok(PlanDelta::Done)
        }
    }
}

#[tokio::test]
async fn dynamic_replan_adds_jobs_mid_run() {
    let dag = Dag::from_jobs([job("a", &[])]);
    let runner = Arc::new(TestRunner::new());
    let orch = Orchestrator::new(runner.clone()).with_planner(Arc::new(AddBAfterA {
        added: Mutex::new(false),
    }));
    let report = orch.run(Run::new("r4", "replan", dag)).await;

    assert_eq!(report.state, RunState::Completed);
    assert_eq!(report.succeeded(), 2, "replan should have added b");
    let order = runner.started();
    assert!(order_pos(&order, "a") < order_pos(&order, "b"));
}

#[tokio::test]
async fn resume_restarts_inflight_jobs() {
    let store = Arc::new(InMemoryRunStore::new());
    // Simulate a crash: a succeeded, b was Running when the process died.
    let mut run = Run::new(
        "r5",
        "resume",
        Dag::from_jobs([job("a", &[]), job("b", &["a"])]),
    );
    {
        let a = run.dag.get_mut("a").unwrap();
        a.state = JobState::Succeeded;
        a.result = Some(JobResult::new("done:a (deps=0)"));
        let b = run.dag.get_mut("b").unwrap();
        b.state = JobState::Running; // mid-flight at crash
    }
    store.save(&run).await.unwrap();

    let runner = Arc::new(TestRunner::new());
    let orch = Orchestrator::new(runner.clone()).with_store(store.clone());
    let report = orch.resume("r5").await.expect("run found");

    assert_eq!(report.state, RunState::Completed);
    // Only b should have actually run on resume (a was already succeeded).
    assert_eq!(runner.started(), vec!["b".to_string()]);
}

#[tokio::test]
async fn run_budget_stops_and_cancels_pending() {
    // 3 sequential-ish jobs, 100 tokens each, budget 150 → 2nd trips it.
    let dag = Dag::from_jobs([job("a", &[]), job("b", &["a"]), job("c", &["b"])]);
    let runner = Arc::new(TestRunner::new().with_tokens(100));
    let orch = Orchestrator::new(runner.clone()).with_max_concurrency(1);
    let run = Run::new("r6", "budget", dag).with_budget(RunBudget::max_total_tokens(150));
    let report = orch.run(run).await;

    assert_eq!(report.state, RunState::Failed);
    let state_of = |id: &str| report.jobs.iter().find(|(j, _, _)| j == id).unwrap().1;
    assert_eq!(state_of("a"), JobState::Succeeded);
    assert_eq!(state_of("b"), JobState::Succeeded); // pushed spent to 200
    assert_eq!(state_of("c"), JobState::Cancelled); // never ran — budget gone
    assert!(report.spent_tokens >= 200);
}

#[tokio::test]
async fn cyclic_dag_is_rejected() {
    let dag = Dag::from_jobs([job("a", &["c"]), job("b", &["a"]), job("c", &["b"])]);
    let runner = Arc::new(TestRunner::new());
    let report = Orchestrator::new(runner)
        .run(Run::new("r7", "cycle", dag))
        .await;
    assert_eq!(report.state, RunState::Failed);
}
