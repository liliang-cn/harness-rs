//! Dynamic replanning: after the base analyses finish, this planner queries
//! the live database for anomalies and adds *only the deep-dive Jobs the data
//! warrants*, plus a synthesis Job that emits structured actions. This is the
//! feedback edge — the plan adapts to what the data actually shows.

use crate::actionspec;
use async_trait::async_trait;
use harness_orchestrator::{
    Backoff, Job, JobId, JobResult, PlanDelta, Planner, PlannerError, RetryPolicy,
};
use sqlx::{PgPool, Row};
use std::sync::Mutex;
use std::time::Duration;

/// The Job id whose runner simulates one transient failure (to exercise
/// retry + backoff). Public so the runner can recognize it.
pub const FLAKY_JOB: &str = "deepdive-reorder";

pub struct AnomalyPlanner {
    pool: PgPool,
    prior: String,
    expanded: Mutex<bool>,
}

impl AnomalyPlanner {
    pub fn new(pool: PgPool, prior: String) -> Self {
        Self {
            pool,
            prior,
            expanded: Mutex::new(false),
        }
    }

    async fn count(&self, sql: &str) -> i64 {
        match sqlx::query(sql).fetch_one(&self.pool).await {
            Ok(row) => row.try_get::<i64, _>("n").unwrap_or(0),
            Err(e) => {
                tracing::warn!(error = %e, "anomaly count query failed");
                0
            }
        }
    }
}

#[async_trait(?Send)]
impl Planner for AnomalyPlanner {
    async fn plan(
        &self,
        _goal: &str,
        succeeded: &[(JobId, JobResult)],
    ) -> Result<PlanDelta, PlannerError> {
        {
            let mut e = self.expanded.lock().unwrap();
            if *e {
                return Ok(PlanDelta::Done);
            }
            // Only expand once the base analyses are in.
            let base_done = ["sales", "inventory", "reviews"]
                .iter()
                .all(|b| succeeded.iter().any(|(id, _)| id == b));
            if !base_done {
                return Ok(PlanDelta::Done);
            }
            *e = true;
        }

        let low_stock = self
            .count("SELECT COUNT(*) AS n FROM products WHERE stock_qty <= reorder_level")
            .await;
        let dead_stock = self
            .count(
                "SELECT COUNT(*) AS n FROM products p WHERE p.stock_qty > 200 AND \
                 (SELECT COALESCE(SUM(qty),0) FROM order_items oi JOIN orders o ON o.id=oi.order_id \
                  WHERE oi.product_id=p.id) < 80",
            )
            .await;
        let rep_risk = self
            .count(
                "SELECT COUNT(*) AS n FROM (SELECT product_id FROM reviews GROUP BY product_id \
                 HAVING COUNT(*)>=3 AND AVG(rating) < 2.5) q",
            )
            .await;

        tracing::info!(low_stock, dead_stock, rep_risk, "anomaly scan");

        let mut jobs: Vec<Job> = Vec::new();
        let mut deepdive_ids: Vec<String> = Vec::new();

        if low_stock > 0 {
            deepdive_ids.push(FLAKY_JOB.into());
            jobs.push(
                Job::new(
                    FLAKY_JOB,
                    "You are a replenishment planner. Using sql_query, list products at or below \
                     reorder_level with their recent sales velocity; for each, call market_signal \
                     to check external demand, then recommend a reorder quantity. Be specific: \
                     SKU, current stock, suggested qty, and the demand signal.",
                )
                .with_deps(["inventory"])
                // exercise retry + exponential backoff (runner fails attempt 1)
                .with_retry(RetryPolicy::new(
                    3,
                    Backoff::Exponential {
                        base: Duration::from_millis(200),
                        factor: 2,
                        max: Duration::from_secs(2),
                    },
                )),
            );
        }
        if dead_stock > 0 {
            deepdive_ids.push("deepdive-liquidation".into());
            jobs.push(
                Job::new(
                    "deepdive-liquidation",
                    "You are a liquidation analyst. Using sql_query, identify dead stock (high \
                     stock_qty, very low total units sold) and the capital tied up in it. \
                     Recommend a markdown percentage per SKU to clear it.",
                )
                .with_deps(["sales", "inventory"]),
            );
        }
        if rep_risk > 0 {
            deepdive_ids.push("deepdive-quality".into());
            jobs.push(
                Job::new(
                    "deepdive-quality",
                    "You are a quality analyst. Using sql_query, find products with avg rating < 2.5 \
                     and >=3 reviews; pull a few of their worst review titles/bodies to infer the \
                     root cause. Recommend whether each should be paused.",
                )
                .with_deps(["reviews"]),
            );
        }

        // Synthesis job → structured actions. Depends on every deep-dive.
        let mut synth = Job::new("synthesize", actionspec::synthesis_prompt(&self.prior));
        synth = synth.with_deps(deepdive_ids.iter().cloned());
        jobs.push(synth);

        Ok(PlanDelta::Add(jobs))
    }
}
