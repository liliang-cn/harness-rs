//! Net-worth aggregation + daily snapshot cron.
//!
//! Composition is bucketed by account kind:
//!   cash + debit + wallet      → "cash" (asset)
//!   credit                     → "debt" (liability)
//!   other                      → skipped (we don't know the sign)
//!
//! Each account's current balance = `opening_balance` + net of its
//! transactions (income/expense/transfer in/out). Amounts are FX-converted
//! to the user's `base_currency` at write time using `fx::convert`, which
//! reads from the cached `fx_rates` table.
//!
//! Investments come from `(qty × latest_price)` per held asset; the price
//! is whatever sits in the `prices` table (refreshed elsewhere).
//!
//! The cron computes a full snapshot per user once a day at ~00:05 UTC and
//! UPSERTs into `net_worth_snapshots`. Re-runs are safe; the (user_id,
//! date) composite key overwrites today's row.

use crate::db::{Db, NetWorthSnapshot};
use crate::fx;
use crate::model::{AccountKind, TxnKind};
use crate::portfolio::model::aggregate_trades;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use std::path::{Path, PathBuf};

/// Run-once: compute and persist today's snapshot for one user. Used by
/// both the cron and the on-demand "refresh" admin endpoint.
pub fn snapshot_now(
    db: &Db,
    user_id: &str,
    base_currency: &str,
) -> anyhow::Result<NetWorthSnapshot> {
    let today = Utc::now().format("%Y-%m-%d").to_string();

    // ── cash + debt: iterate accounts, fold transactions ──
    let accounts = db.list_accounts(user_id)?;
    let txns = db.list_transactions(
        user_id,
        Utc.timestamp_opt(0, 0).unwrap(),
        Utc::now() + Duration::days(1),
        None,
        None,
    )?;

    let mut cash_native: std::collections::HashMap<String, f64> = Default::default();
    let mut debt_native: std::collections::HashMap<String, f64> = Default::default();

    for a in &accounts {
        let mut bal = a.opening_balance.to_f64().unwrap_or(0.0);
        for t in &txns {
            if t.account_id == a.id {
                let amt = t.amount.to_f64().unwrap_or(0.0);
                match t.kind {
                    TxnKind::Income => bal += amt,
                    TxnKind::Expense => bal -= amt,
                    TxnKind::Transfer => bal -= amt, // outgoing leg
                }
            } else if t.counter_account_id.as_deref() == Some(a.id.as_str())
                && matches!(t.kind, TxnKind::Transfer)
            {
                bal += t.amount.to_f64().unwrap_or(0.0); // incoming leg
            }
        }
        let bucket = match a.kind {
            AccountKind::Cash
            | AccountKind::Debit
            | AccountKind::Wallet
            | AccountKind::Receivable => &mut cash_native,
            AccountKind::Credit | AccountKind::Loan | AccountKind::Mortgage => &mut debt_native,
            AccountKind::Other => continue,
        };
        *bucket.entry(a.currency.clone()).or_insert(0.0) += bal;
    }

    // ── investments: aggregate trades × latest price ──
    let assets = db.list_assets(user_id)?;
    let mut invest_native: std::collections::HashMap<String, f64> = Default::default();
    for asset in &assets {
        let trades = db.list_trades(user_id, Some(&asset.id), 10_000)?;
        let (qty, _cost, _realized) = aggregate_trades(&asset.id, &trades);
        if qty <= Decimal::ZERO {
            continue;
        }
        let Some(latest) = db.latest_price(user_id, &asset.id)? else {
            // No price → treat as zero. We could fall back to last trade
            // price, but that hides "stale price" from the user.
            continue;
        };
        let value = (qty * latest.price).to_f64().unwrap_or(0.0);
        *invest_native.entry(latest.currency.clone()).or_insert(0.0) += value;
    }

    // ── FX-convert everything to base_currency ──
    let convert_bucket = |bucket: &std::collections::HashMap<String, f64>| -> f64 {
        bucket
            .iter()
            .map(|(ccy, amt)| {
                fx::convert(db, *amt, ccy, base_currency)
                    .ok()
                    .flatten()
                    .unwrap_or(*amt) // same-currency or missing rate: pass through
            })
            .sum()
    };

    let cash = convert_bucket(&cash_native);
    let investments = convert_bucket(&invest_native);
    let debt = convert_bucket(&debt_native).abs(); // store as positive

    db.upsert_net_worth_snapshot(user_id, &today, base_currency, cash, investments, debt)?;
    Ok(NetWorthSnapshot {
        snapshot_date: today,
        base_currency: base_currency.into(),
        cash_amt: cash,
        investments_amt: investments,
        debt_amt: debt,
        net_amt: cash + investments - debt,
    })
}

/// Background loop. Runs the snapshot for every user once at startup (to
/// warm the table for users who registered before this feature shipped)
/// and once every 24h afterwards, anchored at ~00:05 UTC.
pub fn spawn_snapshot_cron(db_path: PathBuf) {
    tokio::spawn(async move {
        // Initial snapshot for everyone so the dashboard isn't blank.
        if let Err(e) = run_for_all(&db_path).await {
            tracing::warn!(err = %e, "initial net-worth snapshot run failed");
        }
        // Then tick once a day at ~00:05 UTC. We don't need second-level
        // precision; this just keeps the row fresh.
        loop {
            let now = Utc::now();
            let tomorrow_005 = (now.date_naive() + chrono::Duration::days(1))
                .and_hms_opt(0, 5, 0)
                .unwrap()
                .and_utc();
            let wait = (tomorrow_005 - now)
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(3600));
            tokio::time::sleep(wait).await;
            if let Err(e) = run_for_all(&db_path).await {
                tracing::warn!(err = %e, "daily net-worth snapshot run failed");
            }
        }
    });
}

async fn run_for_all(db_path: &Path) -> anyhow::Result<()> {
    // Open per-tick — Connection is !Send across awaits.
    let db = Db::open(db_path)?;
    let user_ids = db.list_all_user_ids()?;
    for uid in &user_ids {
        let Some(user) = db.get_user_by_id(uid)? else {
            continue;
        };
        match snapshot_now(&db, &user.id, &user.base_currency) {
            Ok(snap) => tracing::debug!(
                user = %uid,
                date = %snap.snapshot_date,
                net = snap.net_amt,
                "snapshot ok"
            ),
            Err(e) => tracing::warn!(user = %uid, err = %e, "snapshot failed for user"),
        }
    }
    Ok(())
}

// Re-export the chrono TimeZone trait so `Utc.timestamp_opt(...)` compiles
// without each caller pulling it in.
use chrono::TimeZone;
