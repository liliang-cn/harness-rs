//! `Job` — one node in a Run's DAG, and its state machine.
//!
//! A Job is a single unit of work (typically one sub-agent invocation) with
//! dependencies on other Jobs, a retry policy, and a place to hold its
//! result. The two-level state model (Run + Job) is what lets an
//! orchestrator recover from a crash: a Job stuck in `Running` when the
//! process died is reset to `Pending` on resume.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub type JobId = String;

/// Lifecycle of a single Job. Mirrors the states a durable task system
/// needs to recover and retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Created; waiting for its dependencies to succeed.
    Pending,
    /// Dependencies satisfied; eligible to run, awaiting a concurrency slot.
    Queued,
    /// Currently executing.
    Running,
    /// Finished successfully; `result` is populated.
    Succeeded,
    /// This attempt failed; may transition to `Retrying` or `DeadLettered`.
    Failed,
    /// Waiting out a backoff delay before the next attempt.
    Retrying,
    /// Exhausted its retries; needs human attention. Blocks dependents.
    DeadLettered,
    /// Cancelled (e.g. the Run was cancelled or hit its budget).
    Cancelled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::DeadLettered | JobState::Cancelled
        )
    }
    pub fn label(self) -> &'static str {
        match self {
            JobState::Pending => "pending",
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Retrying => "retrying",
            JobState::DeadLettered => "dead-lettered",
            JobState::Cancelled => "cancelled",
        }
    }
}

/// What a Job produced. Kept in memory — this is a single-machine
/// orchestrator, so there's no Claim-Check indirection (that's a
/// distributed concern, explicitly out of scope).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobResult {
    pub text: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl JobResult {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }
    pub fn with_tokens(mut self, input: u64, output: u64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Backoff strategy between retry attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backoff {
    /// Retry immediately.
    None,
    /// Wait a fixed duration before each retry.
    Fixed(Duration),
    /// `base * factor^(attempt-1)`, capped at `max`.
    Exponential {
        base: Duration,
        factor: u32,
        max: Duration,
    },
}

impl Backoff {
    /// Delay before the given attempt number (1-based: attempt 1 is the
    /// first retry, i.e. the 2nd run overall).
    pub fn delay(self, attempt: u32) -> Duration {
        match self {
            Backoff::None => Duration::ZERO,
            Backoff::Fixed(d) => d,
            Backoff::Exponential { base, factor, max } => {
                let mult = factor.saturating_pow(attempt.saturating_sub(1));
                base.saturating_mul(mult).min(max)
            }
        }
    }
}

/// How many times a Job may run, and how long to wait between attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Total attempts allowed, including the first. `1` means no retry.
    pub max_attempts: u32,
    pub backoff: Backoff,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            backoff: Backoff::None,
        }
    }
}

impl RetryPolicy {
    /// No retries.
    pub fn once() -> Self {
        Self::default()
    }
    /// `attempts` total tries with the given backoff.
    pub fn new(max_attempts: u32, backoff: Backoff) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            backoff,
        }
    }
}

/// One node in the Run DAG.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    /// Ids of Jobs that must `Succeed` before this one can run.
    pub deps: Vec<JobId>,
    /// The task description handed to the runner (e.g. a sub-agent prompt).
    pub prompt: String,
    pub retry: RetryPolicy,
    pub state: JobState,
    pub result: Option<JobResult>,
    /// How many times this Job has been run so far.
    pub attempts: u32,
    /// Error from the most recent failed attempt, if any.
    pub last_error: Option<String>,
}

impl Job {
    pub fn new(id: impl Into<JobId>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            deps: Vec::new(),
            prompt: prompt.into(),
            retry: RetryPolicy::default(),
            state: JobState::Pending,
            result: None,
            attempts: 0,
            last_error: None,
        }
    }

    pub fn with_deps<I, S>(mut self, deps: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<JobId>,
    {
        self.deps = deps.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_backoff_grows_and_caps() {
        let b = Backoff::Exponential {
            base: Duration::from_secs(1),
            factor: 2,
            max: Duration::from_secs(10),
        };
        assert_eq!(b.delay(1), Duration::from_secs(1)); // base * 2^0
        assert_eq!(b.delay(2), Duration::from_secs(2)); // base * 2^1
        assert_eq!(b.delay(3), Duration::from_secs(4)); // base * 2^2
        assert_eq!(b.delay(5), Duration::from_secs(10)); // 16 capped to 10
    }

    #[test]
    fn retry_policy_min_one_attempt() {
        assert_eq!(RetryPolicy::new(0, Backoff::None).max_attempts, 1);
    }

    #[test]
    fn terminal_states() {
        assert!(JobState::Succeeded.is_terminal());
        assert!(JobState::DeadLettered.is_terminal());
        assert!(!JobState::Pending.is_terminal());
        assert!(!JobState::Running.is_terminal());
    }
}
