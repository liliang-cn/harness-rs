//! Conditional edges and bounded cycles — what turns the DAG into a graph.
//!
//! A DAG says what may run in parallel. It cannot say *"if the review fails, go
//! back and revise"*, and that shape is most of agent work: draft → review →
//! revise → review again. Without it a caller either unrolls the loop by hand
//! (three copies of the same job, and still a guess at how many) or drops out of
//! the orchestrator entirely.
//!
//! One concept covers both gaps. A [`Router`] runs after a job succeeds and says
//! what happens next: carry on, jump back to an earlier job, or stop the run.
//! The `deps` graph stays acyclic — cycle detection at load time keeps working —
//! and re-entry is a scheduling decision, made once per completion, with a hard
//! visit cap so a loop that never converges ends as a dead letter rather than a
//! bill.
//!
//! Put the criterion in the router, not in the prompt. A model asked to judge
//! its own output against a rule tends to agree with itself — measured on a real
//! run, one replied "LGTM" for a 30-character answer against a limit of 15, and
//! the loop never ran. The same check as code is exact, and it costs no tokens.
//!
//! ```ignore
//! let orch = Orchestrator::new(runner)
//!     .route("review", |r: &JobResult| {
//!         let n = r.text.chars().count();
//!         if n <= LIMIT {
//!             Next::Continue
//!         } else {
//!             Next::back_to_with("revise", format!("{n} characters, limit is {LIMIT}"))
//!         }
//!     })
//!     .with_max_visits(4);
//! ```

use crate::job::{JobId, JobResult};

/// What happens after a job succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Next {
    /// Ordinary progress: dependents run once their deps are satisfied. This is
    /// what every job does without a router.
    Continue,
    /// Re-enter the graph at `job`, which runs again — along with everything
    /// downstream of it that had already finished. The cycle.
    ///
    /// `feedback` is what the job is told on the way back in. Measured on a real
    /// run without it: a `revise` job re-entered five times produced 33, 33, 33,
    /// 31, 33 characters against a limit of 26 — it saw the same draft each lap
    /// and no word of why it had been rejected, so it wrote the same thing. A
    /// loop that cannot see its own last attempt repeats; it does not refine.
    Goto {
        job: JobId,
        feedback: Option<String>,
    },
    /// End the run here and report success, leaving the rest of the graph
    /// unrun. The early exit an iterative loop needs when it converges before
    /// its budget: the answer is good enough, so stop paying for more.
    Stop,
}

impl Next {
    /// `Next::Goto`, named for how it reads at a call site: `back_to("revise")`.
    ///
    /// Prefer [`back_to_with`](Self::back_to_with) — a job told only "again"
    /// tends to answer the same way.
    pub fn back_to(job: impl Into<JobId>) -> Self {
        Next::Goto {
            job: job.into(),
            feedback: None,
        }
    }

    /// Go back, and say why. The `feedback` reaches the job as the first thing
    /// it reads, above its own previous attempt.
    pub fn back_to_with(job: impl Into<JobId>, feedback: impl Into<String>) -> Self {
        Next::Goto {
            job: job.into(),
            feedback: Some(feedback.into()),
        }
    }
}

/// Decides [`Next`] from a finished job's result.
///
/// Implemented for any `Fn(&JobResult) -> Next`, so a closure is a router and
/// nothing else has to be written for the common case.
pub trait Router: Send + Sync {
    fn route(&self, result: &JobResult) -> Next;
}

impl<F> Router for F
where
    F: Fn(&JobResult) -> Next + Send + Sync,
{
    fn route(&self, result: &JobResult) -> Next {
        self(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closure_is_a_router() {
        let r = |res: &JobResult| {
            if res.text.contains("LGTM") {
                Next::Continue
            } else {
                Next::back_to("revise")
            }
        };
        assert_eq!(r.route(&JobResult::new("LGTM, ship it")), Next::Continue);
        assert_eq!(
            r.route(&JobResult::new("needs work")),
            Next::back_to("revise")
        );
    }
}
