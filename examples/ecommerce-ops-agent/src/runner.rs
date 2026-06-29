//! A `JobRunner` wrapper that delegates to a real `SubagentJobRunner` but
//! makes one designated Job fail transiently on its first attempt. This
//! simulates a flaky data dependency so the orchestrator's retry + backoff
//! path is genuinely exercised end-to-end (the Job then succeeds on retry).

use crate::planner::FLAKY_JOB;
use async_trait::async_trait;
use harness_orchestrator::{Job, JobError, JobId, JobResult, JobRunner, SubagentJobRunner};
use std::collections::HashMap;
use std::sync::Mutex;

pub struct OpsJobRunner {
    inner: SubagentJobRunner,
    attempts: Mutex<HashMap<String, u32>>,
}

impl OpsJobRunner {
    pub fn new(inner: SubagentJobRunner) -> Self {
        Self {
            inner,
            attempts: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait(?Send)]
impl JobRunner for OpsJobRunner {
    async fn run(&self, job: &Job, deps: &[(JobId, JobResult)]) -> Result<JobResult, JobError> {
        if job.id == FLAKY_JOB {
            let first = {
                let mut a = self.attempts.lock().unwrap();
                let n = a.entry(job.id.clone()).or_insert(0);
                *n += 1;
                *n == 1
            };
            if first {
                return Err(JobError::Run(
                    "transient: data-warehouse connection reset — will retry".into(),
                ));
            }
        }
        self.inner.run(job, deps).await
    }
}
