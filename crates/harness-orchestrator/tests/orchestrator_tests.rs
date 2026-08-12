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

// ── conditional edges + bounded cycles ──────────────────────────────────────

use harness_orchestrator::Next;
use std::sync::atomic::{AtomicU32, Ordering};

/// A runner whose `review` job rejects the first `reject_times` drafts, then
/// approves — the shape of every iterative-refinement loop.
struct ReviewRunner {
    started: Mutex<Vec<String>>,
    reviews: AtomicU32,
    reject_times: u32,
}

impl ReviewRunner {
    fn new(reject_times: u32) -> Self {
        Self {
            started: Mutex::new(Vec::new()),
            reviews: AtomicU32::new(0),
            reject_times,
        }
    }
    fn started(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }
}

#[async_trait(?Send)]
impl JobRunner for ReviewRunner {
    async fn run(&self, job: &Job, _deps: &[(JobId, JobResult)]) -> Result<JobResult, JobError> {
        self.started.lock().unwrap().push(job.id.clone());
        if job.id == "review" {
            let n = self.reviews.fetch_add(1, Ordering::SeqCst);
            let verdict = if n < self.reject_times {
                "needs work"
            } else {
                "LGTM"
            };
            return Ok(JobResult::new(verdict));
        }
        Ok(JobResult::new(format!("done:{}", job.id)))
    }
}

fn review_dag() -> Dag {
    Dag::from_jobs([
        job("draft", &[]),
        job("revise", &["draft"]),
        job("review", &["revise"]),
        job("publish", &["review"]),
    ])
}

/// The shape a DAG cannot express: reject → revise → review again. Without a
/// router the caller unrolls the loop by hand and still guesses the count.
#[tokio::test]
async fn a_rejected_review_sends_the_graph_back_and_converges() {
    let runner = Arc::new(ReviewRunner::new(2)); // reject twice, then approve
    let orch = Orchestrator::new(runner.clone()).route("review", |r: &JobResult| {
        if r.text.contains("LGTM") {
            Next::Continue
        } else {
            Next::back_to("revise")
        }
    });

    let report = orch
        .run(Run::new("r-loop", "review loop", review_dag()))
        .await;
    assert_eq!(report.state, RunState::Completed, "{report:?}");

    // revise/review ran three times each: the loop actually looped.
    let started = runner.started();
    let revises = started.iter().filter(|s| *s == "revise").count();
    let reviews = started.iter().filter(|s| *s == "review").count();
    assert_eq!((revises, reviews), (3, 3), "order: {started:?}");
    // …and `publish` ran once, only after the approval.
    assert_eq!(started.iter().filter(|s| *s == "publish").count(), 1);
    assert_eq!(started.last().map(String::as_str), Some("publish"));
}

/// A router that never approves is the failure mode a cycle introduces, and it
/// does not announce itself — it just keeps spending. The cap ends it, and says
/// why.
#[tokio::test]
async fn a_loop_that_never_converges_is_dead_lettered_with_the_count() {
    let runner = Arc::new(ReviewRunner::new(u32::MAX)); // never approves
    let orch = Orchestrator::new(runner.clone())
        .route("review", |_: &JobResult| Next::back_to("revise"))
        .with_max_visits(3);

    let report = orch
        .run(Run::new("r-spin", "stuck loop", review_dag()))
        .await;

    let started = runner.started();
    let revises = started.iter().filter(|s| *s == "revise").count();
    assert!(
        revises <= 4,
        "the cap must stop the loop, ran {revises} times: {started:?}"
    );
    assert_ne!(
        report.state,
        RunState::Completed,
        "a stuck loop is not a success"
    );
}

/// Converging early should stop the run, not carry on paying for whatever else
/// the graph still lists — the escalate case in an iterative loop.
#[tokio::test]
async fn a_router_can_stop_the_run_early() {
    let runner = Arc::new(ReviewRunner::new(0)); // approves immediately
    let orch = Orchestrator::new(runner.clone()).route("review", |_: &JobResult| Next::Stop);

    let report = orch
        .run(Run::new("r-stop", "early stop", review_dag()))
        .await;

    assert_eq!(report.state, RunState::Completed, "{report:?}");
    assert!(
        !runner.started().contains(&"publish".to_string()),
        "Stop must leave the rest unrun: {:?}",
        runner.started()
    );
}

/// Without a router nothing changes — the DAG behaves exactly as before.
#[tokio::test]
async fn a_graph_without_routers_is_the_dag_it_always_was() {
    let runner = Arc::new(ReviewRunner::new(0));
    let report = Orchestrator::new(runner.clone())
        .run(Run::new("r-plain", "plain dag", review_dag()))
        .await;
    assert_eq!(report.state, RunState::Completed);
    assert_eq!(runner.started().len(), 4, "{:?}", runner.started());
}

/// A re-entered job must see its own last answer and why it came back.
///
/// Measured against a real model before this existed: a `revise` job re-entered
/// five times produced 33, 33, 33, 31, 33 characters against a limit of 26 — it
/// received the same draft every lap and no word of the rejection, so it wrote
/// the same thing. A loop that cannot see its own last attempt repeats; it does
/// not refine.
struct EchoPromptRunner {
    seen: Mutex<Vec<String>>,
    laps: AtomicU32,
}

#[async_trait(?Send)]
impl JobRunner for EchoPromptRunner {
    async fn run(&self, job: &Job, deps: &[(JobId, JobResult)]) -> Result<JobResult, JobError> {
        if job.id == "work" {
            // Record the whole prompt the job was handed, deps and all.
            // The real assembly, not a copy of it — a copy drifts silently.
            self.seen
                .lock()
                .unwrap()
                .push(harness_orchestrator::job_prompt(job, deps));
            let n = self.laps.fetch_add(1, Ordering::SeqCst);
            return Ok(JobResult::new(format!("attempt-{n}")));
        }
        Ok(JobResult::new(format!("done:{}", job.id)))
    }
}

#[tokio::test]
async fn a_re_entered_job_is_told_its_last_answer_and_why_it_came_back() {
    let runner = Arc::new(EchoPromptRunner {
        seen: Mutex::new(Vec::new()),
        laps: AtomicU32::new(0),
    });
    let dag = Dag::from_jobs([job("work", &[]), job("check", &["work"])]);

    // Reject the first two laps with a specific, actionable reason.
    let rejections = Arc::new(AtomicU32::new(0));
    let r = rejections.clone();
    let orch = Orchestrator::new(runner.clone())
        .route("check", move |_: &JobResult| {
            if r.fetch_add(1, Ordering::SeqCst) < 2 {
                Next::back_to_with("work", "too long by 7 characters")
            } else {
                Next::Continue
            }
        })
        .with_max_visits(5);

    let report = orch.run(Run::new("r-fb", "refine", dag)).await;
    assert_eq!(report.state, RunState::Completed, "{report:?}");

    let seen = runner.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "the job should have run three times");

    // Lap 1 is a clean start: nothing to improve on yet.
    assert!(!seen[0].contains("rejected"), "lap 1: {}", seen[0]);

    // Laps 2 and 3 carry the reason and the previous answer.
    for (i, prompt) in seen.iter().enumerate().skip(1) {
        assert!(
            prompt.contains("too long by 7 characters"),
            "lap {} lost the feedback:\n{prompt}",
            i + 1
        );
        assert!(
            prompt.contains(&format!("attempt-{}", i - 1)),
            "lap {} cannot see what it answered last time:\n{prompt}",
            i + 1
        );
    }
}

/// A name that matches nothing is the quietest way a graph goes wrong, and the
/// two cases fail differently badly: a mistyped dep leaves its job unreachable
/// (a scheduling mystery), while a mistyped route target never fires at all —
/// the loop simply does not happen and the run reports success.
#[tokio::test]
async fn a_dep_naming_a_job_that_does_not_exist_is_refused_up_front() {
    let runner = Arc::new(TestRunner::new());
    let dag = Dag::from_jobs([job("a", &[]), job("b", &["typo"])]);

    let report = Orchestrator::new(runner.clone())
        .run(Run::new("r-dep", "bad dep", dag))
        .await;

    assert_eq!(report.state, RunState::Failed);
    assert!(
        report.jobs.iter().any(|(_, _, _)| true) && report.jobs.iter().any(|(id, _, _)| id == "b"),
        "{report:?}"
    );
    // Nothing ran: the graph was rejected before spending anything.
    assert!(runner.started().is_empty(), "{:?}", runner.started());
}

#[tokio::test]
async fn a_route_naming_a_job_that_does_not_exist_is_refused_up_front() {
    let runner = Arc::new(TestRunner::new());
    let dag = Dag::from_jobs([job("draft", &[]), job("review", &["draft"])]);

    let report = Orchestrator::new(runner.clone())
        // `reveiw` — the typo that would otherwise mean no loop, silently.
        .route("reveiw", |_: &JobResult| Next::back_to("draft"))
        .run(Run::new("r-route", "bad route", dag))
        .await;

    assert_eq!(
        report.state,
        RunState::Failed,
        "a route pointing at nothing must not pass as a successful run: {report:?}"
    );
    assert!(runner.started().is_empty(), "{:?}", runner.started());
}
