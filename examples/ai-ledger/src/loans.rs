//! Daily interest accrual for active loans + mortgages.
//!
//! For each active loan, advance `last_accrued_date` to today by booking
//! a daily-compounded interest expense transaction on the loan account.
//! Receivable accounts (where someone owes the user) are interest-free
//! in v1 — friends and refunds don't charge interest. The cron is
//! idempotent: if it already ran today there's nothing to accrue.

use crate::db::Db;
use chrono::{Duration, NaiveDate, Utc};
use rust_decimal::Decimal;
use std::path::PathBuf;
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
fn accrue_all(db_path: &PathBuf) -> anyhow::Result<()> {
    let db = Db::open(db_path)?;
    let user_ids = db.list_all_user_ids()?;
    let today = Utc::now().date_naive();
    let today_iso = today.format("%Y-%m-%d").to_string();
    for uid in &user_ids {
        let loans = db.list_loans(uid)?;
        for loan in loans.iter().filter(|l| l.status == "active") {
            let Some(acct) = db.get_account(uid, &loan.account_id)? else { continue };
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

            let last = NaiveDate::parse_from_str(&loan.last_accrued_date, "%Y-%m-%d")
                .unwrap_or(today);
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
            let new_balance = if current_balance < 0.0 { -new_abs } else { new_abs };
            let delta = new_balance - current_balance;

            // Book the |delta| as an Expense on the loan account. For a debt
            // (negative balance, negative delta) this further decreases the
            // balance through the existing fold-transactions math in
            // net_worth. Threshold below 0.0001 to skip noise from a freshly
            // booked loan with no balance yet.
            if delta.abs() > 0.0001 {
                let interest_amount = Decimal::from_str(&format!("{:.4}", delta.abs()))
                    .unwrap_or(Decimal::ZERO);
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
