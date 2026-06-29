//! `Dag` — the set of Jobs and their dependency edges, plus the planner's
//! delta type for dynamic replanning.

use crate::job::{Job, JobId, JobState};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A directed acyclic graph of Jobs. Edges are implicit in each Job's
/// `deps`. The orchestrator schedules a Job once all of its deps have
/// `Succeeded`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dag {
    jobs: HashMap<JobId, Job>,
}

impl Dag {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
        }
    }

    /// Build from a list of Jobs.
    pub fn from_jobs(jobs: impl IntoIterator<Item = Job>) -> Self {
        let mut d = Dag::new();
        for j in jobs {
            d.jobs.insert(j.id.clone(), j);
        }
        d
    }

    pub fn add(&mut self, job: Job) {
        self.jobs.insert(job.id.clone(), job);
    }

    pub fn get(&self, id: &str) -> Option<&Job> {
        self.jobs.get(id)
    }
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Job> {
        self.jobs.get_mut(id)
    }
    pub fn len(&self) -> usize {
        self.jobs.len()
    }
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }
    pub fn ids(&self) -> Vec<JobId> {
        self.jobs.keys().cloned().collect()
    }
    pub fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    /// Are all of `job`'s dependencies in a `Succeeded` state?
    pub fn deps_satisfied(&self, job: &Job) -> bool {
        job.deps.iter().all(|d| {
            self.jobs
                .get(d)
                .map(|j| j.state == JobState::Succeeded)
                .unwrap_or(false)
        })
    }

    /// Is any dependency of `job` blocked (dead-lettered / cancelled), so
    /// `job` can never run?
    pub fn deps_blocked(&self, job: &Job) -> bool {
        job.deps.iter().any(|d| {
            self.jobs
                .get(d)
                .map(|j| matches!(j.state, JobState::DeadLettered | JobState::Cancelled))
                .unwrap_or(true) // missing dep ⇒ unsatisfiable
        })
    }

    /// Succeeded `(id, result)` pairs — used as context for downstream Jobs
    /// and for the planner's replan input.
    pub fn succeeded_results(&self) -> Vec<(JobId, crate::job::JobResult)> {
        self.jobs
            .values()
            .filter(|j| j.state == JobState::Succeeded)
            .filter_map(|j| j.result.clone().map(|r| (j.id.clone(), r)))
            .collect()
    }

    /// Detect a dependency cycle (returns the first cycle's member ids).
    /// The orchestrator rejects a cyclic DAG up front rather than deadlock.
    pub fn find_cycle(&self) -> Option<Vec<JobId>> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            White,
            Gray,
            Black,
        }
        let mut mark: HashMap<&str, Mark> = self
            .jobs
            .keys()
            .map(|k| (k.as_str(), Mark::White))
            .collect();
        let mut stack: Vec<&str> = Vec::new();

        fn dfs<'a>(
            node: &'a str,
            jobs: &'a HashMap<JobId, Job>,
            mark: &mut HashMap<&'a str, Mark>,
            stack: &mut Vec<&'a str>,
        ) -> Option<Vec<JobId>> {
            mark.insert(node, Mark::Gray);
            stack.push(node);
            if let Some(j) = jobs.get(node) {
                for dep in &j.deps {
                    match mark.get(dep.as_str()).copied() {
                        Some(Mark::Gray) => {
                            // back edge ⇒ cycle
                            let from = stack.iter().position(|n| *n == dep.as_str()).unwrap_or(0);
                            return Some(stack[from..].iter().map(|s| s.to_string()).collect());
                        }
                        Some(Mark::White) => {
                            if let Some(c) = dfs(dep.as_str(), jobs, mark, stack) {
                                return Some(c);
                            }
                        }
                        _ => {}
                    }
                }
            }
            stack.pop();
            mark.insert(node, Mark::Black);
            None
        }

        let ids: Vec<&str> = self.jobs.keys().map(|s| s.as_str()).collect();
        for id in ids {
            if mark.get(id).copied() == Some(Mark::White)
                && let Some(c) = dfs(id, &self.jobs, &mut mark, &mut stack)
            {
                return Some(c);
            }
        }
        None
    }
}

/// What a [`crate::Planner`] returns when asked to (re)plan: either more
/// Jobs to merge into the DAG, or "the goal is done".
#[derive(Debug, Clone)]
pub enum PlanDelta {
    /// Merge these Jobs into the running DAG (the dynamic-replanning edge).
    Add(Vec<Job>),
    /// No more work — the orchestrator can finish once in-flight Jobs drain.
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(id: &str, deps: &[&str]) -> Job {
        Job::new(id, format!("do {id}")).with_deps(deps.iter().copied())
    }

    #[test]
    fn deps_satisfied_only_when_all_succeeded() {
        let mut d = Dag::from_jobs([j("a", &[]), j("b", &[]), j("c", &["a", "b"])]);
        let c = d.get("c").unwrap().clone();
        assert!(!d.deps_satisfied(&c));
        d.get_mut("a").unwrap().state = JobState::Succeeded;
        assert!(!d.deps_satisfied(&c));
        d.get_mut("b").unwrap().state = JobState::Succeeded;
        assert!(d.deps_satisfied(&c));
    }

    #[test]
    fn dead_dep_blocks_downstream() {
        let mut d = Dag::from_jobs([j("a", &[]), j("c", &["a"])]);
        d.get_mut("a").unwrap().state = JobState::DeadLettered;
        let c = d.get("c").unwrap().clone();
        assert!(d.deps_blocked(&c));
    }

    #[test]
    fn detects_cycle() {
        let d = Dag::from_jobs([j("a", &["c"]), j("b", &["a"]), j("c", &["b"])]);
        assert!(d.find_cycle().is_some());
        let ok = Dag::from_jobs([j("a", &[]), j("b", &["a"]), j("c", &["b"])]);
        assert!(ok.find_cycle().is_none());
    }
}
