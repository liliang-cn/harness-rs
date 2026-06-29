//! Governance: each proposed action is classified to a maturity level and run
//! through a [`HumanGate`]. Low-blast-radius actions (reorder, small markdown)
//! auto-apply at L3 via an [`ActionExecutor`] that performs a *real* database
//! write; bigger actions (large markdown, pausing a product) escalate to a
//! human and are recorded for review. This is loop-engineering's
//! propose → gate → act discipline applied to live operations.

use crate::action::OpsAction;
use async_trait::async_trait;
use harness_core::World;
use harness_loop_engine::{
    ActionError, ActionExecutor, ActionReceipt, AllowlistGate, GateDecision, HumanGate, LoopSpec,
    ProposedAction,
};
use sqlx::PgPool;

#[derive(Debug, Default)]
pub struct GovernSummary {
    pub applied: Vec<String>,
    pub escalated: Vec<String>,
}

/// Executes one approved action as a real DB write. Holds the concrete
/// `OpsAction`; the loop-engine trait params drive the receipt.
struct DbActionExecutor {
    pool: PgPool,
    action: OpsAction,
}

#[async_trait]
impl ActionExecutor for DbActionExecutor {
    async fn execute(
        &self,
        _spec: &LoopSpec,
        action: &ProposedAction,
        _world: &mut World,
    ) -> Result<ActionReceipt, ActionError> {
        let a = &self.action;
        let res = match a.kind.as_str() {
            "reorder" => {
                sqlx::query("INSERT INTO purchase_orders (sku, qty, reason) VALUES ($1,$2,$3)")
                    .bind(&a.sku)
                    .bind(a.qty.unwrap_or(0))
                    .bind(&a.reason)
                    .execute(&self.pool)
                    .await
            }
            "markdown" => {
                sqlx::query("INSERT INTO price_changes (sku, pct, reason) VALUES ($1,$2,$3)")
                    .bind(&a.sku)
                    .bind(a.pct.unwrap_or(0))
                    .bind(&a.reason)
                    .execute(&self.pool)
                    .await
            }
            other => {
                return Err(ActionError::Exec(format!(
                    "non-auto-appliable kind `{other}`"
                )));
            }
        };
        res.map_err(|e| ActionError::Exec(e.to_string()))?;

        // Audit trail.
        let _ = sqlx::query(
            "INSERT INTO action_log (kind, sku, decision, detail) VALUES ($1,$2,'auto-applied',$3)",
        )
        .bind(&a.kind)
        .bind(&a.sku)
        .bind(a.human())
        .execute(&self.pool)
        .await;

        Ok(ActionReceipt::new(action.kind.clone(), a.human()))
    }
}

/// Run every proposed action through the gate, applying or escalating each.
pub async fn govern_and_act(pool: &PgPool, actions: &[OpsAction]) -> anyhow::Result<GovernSummary> {
    // Auto-apply only reorder and small markdowns (and only at L3).
    let gate = AllowlistGate::new(["reorder", "markdown-small"]);
    let mut world = harness_context::default_world(".");
    let mut summary = GovernSummary::default();

    for a in actions {
        let (kind_key, level) = a.gate_kind_and_level();
        let proposed = ProposedAction::new(kind_key, a.human(), true);
        match gate.decide(level, &proposed) {
            GateDecision::AutoProceed => {
                let exec = DbActionExecutor {
                    pool: pool.clone(),
                    action: a.clone(),
                };
                let spec = LoopSpec::new("ops", "apply approved operational actions", level);
                match exec.execute(&spec, &proposed, &mut world).await {
                    Ok(r) => summary
                        .applied
                        .push(format!("[{}] {}", level.label(), r.summary)),
                    Err(e) => summary
                        .escalated
                        .push(format!("apply failed for {}: {e}", a.sku)),
                }
            }
            GateDecision::Escalate { reason } => {
                let _ = sqlx::query(
                    "INSERT INTO escalations (kind, sku, detail, reason) VALUES ($1,$2,$3,$4)",
                )
                .bind(&a.kind)
                .bind(&a.sku)
                .bind(a.human())
                .bind(&reason)
                .execute(pool)
                .await;
                let _ = sqlx::query(
                    "INSERT INTO action_log (kind, sku, decision, detail) VALUES ($1,$2,'escalated',$3)",
                )
                .bind(&a.kind)
                .bind(&a.sku)
                .bind(a.human())
                .execute(pool)
                .await;
                summary
                    .escalated
                    .push(format!("[{}] {} — {reason}", level.label(), a.human()));
            }
        }
    }
    Ok(summary)
}
