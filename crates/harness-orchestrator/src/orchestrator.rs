//! `Orchestrator` — drives a Run's DAG to completion.
//!
//! One scheduling loop: launch every Job whose dependencies have succeeded
//! (up to a concurrency cap), await completions, then succeed / retry /
//! dead-letter each Job, persist after every transition, optionally replan,
//! and enforce the run-level token budget. Sub-agent futures are not `Send`,
//! so concurrency is cooperative — a `FuturesUnordered` polled on one thread,
//! not `tokio::spawn` across threads. That is plenty for I/O-bound LLM work.

use crate::dag::PlanDelta;
use crate::job::{Job, JobId, JobResult, JobState};
use crate::planner::Planner;
use crate::route::{Next, Router};
use crate::run::{Run, RunReport, RunState};
use crate::runner::JobRunner;
use crate::store::RunStore;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

type JobFut = Pin<Box<dyn Future<Output = (JobId, u32, Result<JobResult, String>)>>>;

/// Schedules and supervises a Run.
pub struct Orchestrator {
    runner: Arc<dyn JobRunner>,
    planner: Option<Arc<dyn Planner>>,
    store: Option<Arc<dyn RunStore>>,
    max_concurrency: usize,
    max_replans: u32,
    /// Routers live here rather than on the `Dag` because the Dag is state and
    /// gets restored from the store, while these are code: a resumed run
    /// rebuilds the Orchestrator and keeps its routes, instead of silently
    /// losing them and running straight past a loop it was in the middle of.
    routes: std::collections::HashMap<JobId, Arc<dyn Router>>,
    max_visits: u32,
}

impl Orchestrator {
    pub fn new(runner: Arc<dyn JobRunner>) -> Self {
        Self {
            runner,
            planner: None,
            store: None,
            max_concurrency: 4,
            max_replans: 8,
            routes: std::collections::HashMap::new(),
            max_visits: 5,
        }
    }

    pub fn with_planner(mut self, p: Arc<dyn Planner>) -> Self {
        self.planner = Some(p);
        self
    }
    pub fn with_store(mut self, s: Arc<dyn RunStore>) -> Self {
        self.store = Some(s);
        self
    }
    pub fn with_max_concurrency(mut self, n: usize) -> Self {
        self.max_concurrency = n.max(1);
        self
    }
    /// Decide what happens after `job` succeeds — see [`Next`].
    ///
    /// ```ignore
    /// .route("review", |r: &JobResult| {
    ///     if r.text.contains("LGTM") { Next::Continue } else { Next::back_to("revise") }
    /// })
    /// ```
    pub fn route<R>(mut self, job: impl Into<JobId>, router: R) -> Self
    where
        R: Router + 'static,
    {
        self.routes.insert(job.into(), Arc::new(router));
        self
    }

    /// How many times one job may be entered before the run gives up on the
    /// loop. Default 5.
    ///
    /// A router that never converges is the failure mode a cycle introduces,
    /// and it does not announce itself — it just keeps spending. On the cap the
    /// job is dead-lettered with the count in its error, which reads as a stuck
    /// loop rather than as a mysterious halt.
    pub fn with_max_visits(mut self, n: u32) -> Self {
        self.max_visits = n.max(1);
        self
    }

    /// Cap on how many times the planner may be (re)invoked. Replanning only
    /// happens when a planner is installed.
    pub fn with_max_replans(mut self, n: u32) -> Self {
        self.max_replans = n;
        self
    }

    /// Run `run` to a terminal state and return its report. Never returns an
    /// `Err`: failures land in Job/Run state, not the call site.
    pub async fn run(&self, mut run: Run) -> RunReport {
        // Names that match nothing are the quietest way a graph goes wrong: a
        // mistyped dep leaves its job unreachable and reads as a scheduling
        // mystery, and a mistyped route target never fires at all — the loop
        // simply does not happen and the run reports success. Both are cheap to
        // catch before anything spends a token.
        let unknown_deps = run.dag.unknown_deps();
        let unknown_routes: Vec<&JobId> = self
            .routes
            .keys()
            .filter(|id| run.dag.get(id.as_str()).is_none())
            .collect();
        if !unknown_deps.is_empty() || !unknown_routes.is_empty() {
            let mut why = String::new();
            for (job, dep) in &unknown_deps {
                why.push_str(&format!(
                    "job `{job}` depends on `{dep}`, which is not in the graph; "
                ));
            }
            for id in &unknown_routes {
                why.push_str(&format!(
                    "a route is registered for `{id}`, which is not in the graph; "
                ));
            }
            tracing::warn!(run = %run.id, %why, "orchestrator: graph names something that does not exist");
            for j in run.dag.jobs_mut() {
                if j.last_error.is_none() {
                    j.last_error = Some(why.clone());
                }
            }
            run.state = RunState::Failed;
            self.save(&run).await;
            return self.report(&run);
        }

        if let Some(cycle) = run.dag.find_cycle() {
            tracing::warn!(run = %run.id, ?cycle, "orchestrator: cyclic DAG — aborting");
            run.state = RunState::Failed;
            self.save(&run).await;
            return self.report(&run);
        }

        run.state = RunState::Executing;
        self.save(&run).await;

        let mut inflight: FuturesUnordered<JobFut> = FuturesUnordered::new();
        let mut replans_left = self.max_replans;

        loop {
            self.launch_ready(&mut run, &mut inflight).await;

            if inflight.is_empty() {
                self.cancel_unreachable(&mut run).await;
                self.launch_ready(&mut run, &mut inflight).await;
            }

            if inflight.is_empty() {
                // Nothing running and nothing runnable: replan or finish.
                if self.planner.is_some() && replans_left > 0 {
                    replans_left -= 1;
                    run.state = RunState::Planning;
                    self.save(&run).await;
                    let succeeded = run.dag.succeeded_results();
                    let delta = self
                        .planner
                        .as_ref()
                        .unwrap()
                        .plan(&run.goal, &succeeded)
                        .await;
                    match delta {
                        Ok(PlanDelta::Add(jobs)) if !jobs.is_empty() => {
                            for j in jobs {
                                run.dag.add(j);
                            }
                            if run.dag.find_cycle().is_some() {
                                tracing::warn!(run = %run.id, "orchestrator: replan introduced a cycle — aborting");
                                run.state = RunState::Failed;
                                self.save(&run).await;
                                break;
                            }
                            run.state = RunState::Executing;
                            self.save(&run).await;
                            continue;
                        }
                        Ok(_) => break, // Done or empty Add
                        Err(e) => {
                            tracing::warn!(run = %run.id, error = %e, "orchestrator: replan failed");
                            break;
                        }
                    }
                } else {
                    break;
                }
            }

            // Await the next completion.
            let Some((id, attempt, result)) = inflight.next().await else {
                continue;
            };
            match result {
                Ok(jr) => {
                    run.spent_tokens += jr.total_tokens();
                    let routed = self.routes.get(&id).map(|r| r.route(&jr));
                    if let Some(j) = run.dag.get_mut(&id) {
                        j.state = JobState::Succeeded;
                        j.attempts = attempt;
                        j.result = Some(jr);
                    }

                    match routed {
                        None | Some(Next::Continue) => {}
                        Some(Next::Stop) => {
                            // The loop converged. Everything still pending was
                            // only worth running if it had not.
                            self.cancel_all_nonterminal(&mut run, "run stopped by router");
                            run.state = RunState::Completed;
                            self.save(&run).await;
                            return self.report(&run);
                        }
                        Some(Next::Goto {
                            job: target,
                            feedback,
                        }) => {
                            if let Err(reason) = self.reenter(&mut run, &target, feedback) {
                                tracing::warn!(run = %run.id, %target, %reason, "orchestrator: loop bound reached");
                                if let Some(j) = run.dag.get_mut(&target) {
                                    j.state = JobState::DeadLettered;
                                    j.last_error = Some(reason);
                                }
                            }
                        }
                    }
                    self.save(&run).await;
                    if run.budget.exceeded(run.spent_tokens) {
                        self.cancel_all_nonterminal(&mut run, "run token budget exceeded");
                        run.state = RunState::Failed;
                        self.save(&run).await;
                        return self.report(&run);
                    }
                }
                Err(err) => {
                    let mut retry = None;
                    if let Some(j) = run.dag.get_mut(&id) {
                        j.last_error = Some(err);
                        j.attempts = attempt;
                        if attempt < j.retry.max_attempts {
                            j.state = JobState::Retrying;
                            retry = Some((j.retry.backoff.delay(attempt), attempt + 1, j.clone()));
                        } else {
                            j.state = JobState::DeadLettered;
                        }
                    }
                    self.save(&run).await;
                    if let Some((delay, next_attempt, job)) = retry {
                        let deps = collect_deps(&run, &job);
                        let runner = self.runner.clone();
                        inflight.push(Box::pin(async move {
                            if !delay.is_zero() {
                                tokio::time::sleep(delay).await;
                            }
                            let r = runner.run(&job, &deps).await.map_err(|e| e.to_string());
                            (job.id.clone(), next_attempt, r)
                        }));
                    }
                }
            }
        }

        run.state = RunState::Aggregating;
        self.save(&run).await;
        let failed = run
            .dag
            .jobs()
            .filter(|j| matches!(j.state, JobState::DeadLettered | JobState::Cancelled))
            .count();
        run.state = if failed == 0 {
            RunState::Completed
        } else {
            RunState::Failed
        };
        self.save(&run).await;
        self.report(&run)
    }

    /// Resume a Run saved by a [`RunStore`]: reset Jobs caught mid-flight to
    /// `Pending` and continue. Returns `None` if no store is configured or
    /// the Run isn't found.
    pub async fn resume(&self, run_id: &str) -> Option<RunReport> {
        let store = self.store.as_ref()?;
        let mut run = match store.load(run_id).await {
            Ok(Some(r)) => r,
            _ => return None,
        };
        for id in run.dag.ids() {
            if let Some(j) = run.dag.get_mut(&id)
                && matches!(
                    j.state,
                    JobState::Running | JobState::Queued | JobState::Retrying
                )
            {
                j.state = JobState::Pending;
            }
        }
        Some(self.run(run).await)
    }

    // ── internals ──────────────────────────────────────────────────────

    /// Send the graph back through `target`: it and everything downstream of it
    /// return to `Pending` so the loop actually loops.
    ///
    /// Fails — leaving the run to dead-letter the job — once any of them has
    /// been entered `max_visits` times. The cap is checked before the reset, so
    /// a run that would exceed it stops instead of paying for one more lap.
    fn reenter(&self, run: &mut Run, target: &str, feedback: Option<String>) -> Result<(), String> {
        let affected = run.dag.downstream_of(target);
        for id in &affected {
            if let Some(j) = run.dag.get(id.as_str())
                && j.visits >= self.max_visits
            {
                return Err(format!(
                    "`{id}` has been entered {} times (max_visits {}); the loop is not converging",
                    j.visits, self.max_visits
                ));
            }
        }
        for id in &affected {
            if let Some(j) = run.dag.get_mut(id.as_str())
                && j.state.is_terminal()
            {
                // Carry the last answer forward before clearing it: a job that
                // cannot see what it already said will say it again.
                j.prior_attempt = j.result.take().map(|r| r.text);
                if id.as_str() == target {
                    j.feedback = feedback.clone();
                }
                j.state = JobState::Pending;
                j.visits += 1;
                j.attempts = 0;
                j.last_error = None;
            }
        }
        Ok(())
    }

    async fn launch_ready(&self, run: &mut Run, inflight: &mut FuturesUnordered<JobFut>) {
        loop {
            if inflight.len() >= self.max_concurrency {
                break;
            }
            let pick = run
                .dag
                .jobs()
                .find(|j| j.state == JobState::Pending && run.dag.deps_satisfied(j))
                .map(|j| j.id.clone());
            let Some(id) = pick else {
                break;
            };
            let job = {
                let j = run.dag.get_mut(&id).unwrap();
                j.state = JobState::Running;
                j.attempts = 1;
                j.clone()
            };
            let deps = collect_deps(run, &job);
            self.save(run).await;
            let runner = self.runner.clone();
            inflight.push(Box::pin(async move {
                let r = runner.run(&job, &deps).await.map_err(|e| e.to_string());
                (job.id.clone(), 1u32, r)
            }));
        }
    }

    async fn cancel_unreachable(&self, run: &mut Run) {
        let dead: Vec<JobId> = run
            .dag
            .jobs()
            .filter(|j| j.state == JobState::Pending && run.dag.deps_blocked(j))
            .map(|j| j.id.clone())
            .collect();
        if dead.is_empty() {
            return;
        }
        for id in dead {
            if let Some(j) = run.dag.get_mut(&id) {
                j.state = JobState::Cancelled;
                j.last_error = Some("dependency dead-lettered or missing".into());
            }
        }
        self.save(run).await;
    }

    fn cancel_all_nonterminal(&self, run: &mut Run, reason: &str) {
        let ids: Vec<JobId> = run
            .dag
            .jobs()
            .filter(|j| !j.state.is_terminal())
            .map(|j| j.id.clone())
            .collect();
        for id in ids {
            if let Some(j) = run.dag.get_mut(&id) {
                j.state = JobState::Cancelled;
                j.last_error = Some(reason.into());
            }
        }
    }

    async fn save(&self, run: &Run) {
        if let Some(s) = &self.store
            && let Err(e) = s.save(run).await
        {
            tracing::warn!(run = %run.id, error = %e, "orchestrator: persist failed");
        }
    }

    fn report(&self, run: &Run) -> RunReport {
        RunReport {
            run_id: run.id.clone(),
            goal: run.goal.clone(),
            state: run.state,
            spent_tokens: run.spent_tokens,
            jobs: run
                .dag
                .jobs()
                .map(|j| crate::run::JobReport {
                    id: j.id.clone(),
                    state: j.state,
                    result: j.result.as_ref().map(|r| r.text.clone()),
                    error: j.last_error.clone(),
                    attempts: j.attempts,
                    visits: j.visits,
                })
                .collect(),
        }
    }
}

fn collect_deps(run: &Run, job: &Job) -> Vec<(JobId, JobResult)> {
    job.deps
        .iter()
        .filter_map(|d| {
            run.dag
                .get(d)
                .and_then(|dj| dj.result.clone().map(|r| (d.clone(), r)))
        })
        .collect()
}
