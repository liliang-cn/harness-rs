//! Daily interest accrual for active loans + mortgages.
//!
//! For each active loan, advance `last_accrued_date` to today by booking
//! a daily-compounded interest expense transaction on the loan account.
//! Receivable accounts (where someone owes the user) are interest-free
//! in v1 — friends and refunds don't charge interest. The cron is
//! idempotent: if it already ran today there's nothing to accrue.

use crate::db::Db;
use chrono::{Duration, NaiveDate, Utc};
use rusqlite::Result as SqlResult;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Spawn the background loop. Runs once at startup (so a service restart
/// catches up any days missed while the process was down) and then once a
/// day at ~00:10 UTC, intentionally a few minutes after the net-worth
/// snapshot cron so that today's interest is already booked before the
/// next day's snapshot is taken.
pub fn spawn_accrual_cron(db_path: PathBuf) {
    tokio::spawn(async move {
        if let Err(e) = accrue_all(&db_path) {
            tracing::warn!(err = %e, "initial loan accrual failed");
        }
        loop {
            let now = Utc::now();
            let tomorrow = (now.date_naive() + Duration::days(1))
                .and_hms_opt(0, 10, 0)
                .unwrap()
                .and_utc();
            let wait = (tomorrow - now)
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(3600));
            tokio::time::sleep(wait).await;
            if let Err(e) = accrue_all(&db_path) {
                tracing::warn!(err = %e, "daily loan accrual failed");
            }
        }
    });
}

/// One pass over every user's active Loan/Mortgage accounts. Skips
/// Receivables (interest-free in v1) and 0% APR loans (just advance the
/// cursor so we don't loop). Idempotent — same-day reruns are no-ops
/// because `days = today - last_accrued_date` is 0.
fn accrue_all(db_path: &Path) -> anyhow::Result<()> {
    let db = Db::open(db_path)?;
    let user_ids = db.list_all_user_ids()?;
    let today = Utc::now().date_naive();
    let today_iso = today.format("%Y-%m-%d").to_string();
    for uid in &user_ids {
        let loans = db.list_loans(uid)?;
        for loan in loans.iter().filter(|l| l.status == "active") {
            let Some(acct) = db.get_account(uid, &loan.account_id)? else {
                continue;
            };
            // Receivables (friends owing you, pending refunds) don't charge
            // interest in v1. Bail before we book anything.
            if matches!(acct.kind, crate::model::AccountKind::Receivable) {
                continue;
            }

            let apr: f64 = loan.apr.parse().unwrap_or(0.0);
            if apr <= 0.0 {
                // 0% loan (interest-free family loan, BNPL): no booking,
                // but still advance the cursor so we don't keep
                // re-evaluating it tomorrow.
                db.set_loan_last_accrued(&loan.account_id, &today_iso)?;
                continue;
            }

            let last =
                NaiveDate::parse_from_str(&loan.last_accrued_date, "%Y-%m-%d").unwrap_or(today);
            let days = (today - last).num_days();
            if days <= 0 {
                continue;
            }

            let current_balance = db.compute_account_balance(uid, &loan.account_id)?;
            // Compound daily: factor = (1 + apr/365)^days. Apply to |balance|
            // then re-sign so a -1000 debt at 5% APR for 30 days becomes
            // ~-1004.11, i.e. more negative.
            let factor = (1.0 + apr / 365.0).powi(days as i32);
            let new_abs = current_balance.abs() * factor;
            let new_balance = if current_balance < 0.0 {
                -new_abs
            } else {
                new_abs
            };
            let delta = new_balance - current_balance;

            // Book the |delta| as an Expense on the loan account. For a debt
            // (negative balance, negative delta) this further decreases the
            // balance through the existing fold-transactions math in
            // net_worth. Threshold below 0.0001 to skip noise from a freshly
            // booked loan with no balance yet.
            if delta.abs() > 0.0001 {
                let interest_amount =
                    Decimal::from_str(&format!("{:.4}", delta.abs())).unwrap_or(Decimal::ZERO);
                db.insert_system_interest_transaction(
                    uid,
                    &loan.account_id,
                    &acct.currency,
                    interest_amount,
                    apr,
                    days,
                    &today_iso,
                )?;
            }
            db.set_loan_last_accrued(&loan.account_id, &today_iso)?;
        }
    }
    Ok(())
}

/// Compose the per-loan JSON view used by both `/api/me/loans` and the
/// `loan_summary` agent tool — joining each `loans` row with its
/// `accounts` row + derived `remaining` / `progress_pct` / `next_due_date`.
/// Keeping a single code path here means the agent and the REST API can
/// never disagree about what counts as "outstanding".
pub fn summarise(db: &Db, user_id: &str, include_paid_off: bool) -> SqlResult<Vec<Value>> {
    use chrono::{Datelike, Months};
    let loans = db.list_loans(user_id)?;
    let today = Utc::now().date_naive();
    let mut out: Vec<Value> = Vec::with_capacity(loans.len());
    for l in &loans {
        if !include_paid_off && l.status != "active" {
            continue;
        }
        // Skip orphaned loan rows whose account was deleted.
        let Some(acct) = db.get_account(user_id, &l.account_id)? else {
            continue;
        };
        let balance = db.compute_account_balance(user_id, &l.account_id)?;

        // Sign convention: debt accounts (Loan/Mortgage) carry a negative
        // balance, Receivables carry a positive one. `remaining` is the
        // unsigned "how much is still outstanding".
        let remaining_f = match acct.kind {
            crate::model::AccountKind::Receivable => balance.max(0.0),
            _ => balance.abs(),
        };

        let principal_f: f64 = l.principal.parse().unwrap_or(0.0);
        let progress_pct = if principal_f > 0.0 {
            ((principal_f - remaining_f) / principal_f * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        };

        let next_due_date: Option<String> = match (l.monthly_payment.as_ref(), l.term_months) {
            (Some(_), Some(term)) if term > 0 => {
                NaiveDate::parse_from_str(&l.start_date, "%Y-%m-%d")
                    .ok()
                    .and_then(|start| {
                        let elapsed = (today.year() as i64 * 12 + today.month() as i64)
                            - (start.year() as i64 * 12 + start.month() as i64);
                        let next_n = (elapsed.max(0) + 1).min(term).max(0) as u32;
                        start.checked_add_months(Months::new(next_n))
                    })
                    .map(|d| d.format("%Y-%m-%d").to_string())
            }
            _ => None,
        };

        out.push(json!({
            "account_id":      l.account_id,
            "name":             acct.name,
            "kind":             acct.kind,
            "counterparty":     l.counterparty,
            "principal":        l.principal,
            "remaining":        format!("{:.2}", remaining_f),
            "currency":         acct.currency,
            "apr":              l.apr,
            "term_months":      l.term_months,
            "monthly_payment":  l.monthly_payment,
            "start_date":       l.start_date,
            "next_due_date":    next_due_date,
            "progress_pct":     (progress_pct * 100.0).round() / 100.0,
            "status":           l.status,
            "note":             l.note,
        }));
    }
    Ok(out)
}
