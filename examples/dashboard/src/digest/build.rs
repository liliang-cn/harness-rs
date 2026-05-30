//! Assemble a `Digest` for one user from existing ledger + net-worth data.
//! All generation is pure-ish (DB reads only); delivery lives in `deliver`.

use crate::db::Db;
use crate::digest::model::{Digest, MarketBrief, SpendingSection, WealthSection};
use crate::model::{Transaction, TxnKind};
use chrono::{DateTime, Duration, TimeZone, Utc};
#[cfg(test)]
use chrono::{Datelike, Timelike};
use chrono_tz::Tz;
use rust_decimal::prelude::ToPrimitive;
use std::collections::HashMap;

/// The user-local [start, end) of "yesterday", expressed as UTC instants.
pub fn yesterday_bounds(now_utc: DateTime<Utc>, tz: Tz) -> (DateTime<Utc>, DateTime<Utc>) {
    let local_today = now_utc.with_timezone(&tz).date_naive();
    let local_yest = local_today - Duration::days(1);
    let start_local = tz
        .from_local_datetime(&local_yest.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap();
    let end_local = tz
        .from_local_datetime(&local_today.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap();
    (
        start_local.with_timezone(&Utc),
        end_local.with_timezone(&Utc),
    )
}

/// Aggregate expense transactions into a SpendingSection. Income/transfer are
/// ignored; categories without a label fold into "未分类". Sorted desc, top 5.
pub fn spending_from_txns(txns: &[Transaction], currency: &str) -> SpendingSection {
    let mut by_cat: HashMap<String, f64> = HashMap::new();
    let mut total = 0.0;
    for t in txns {
        if !matches!(t.kind, TxnKind::Expense) {
            continue;
        }
        let amt = t.amount.to_f64().unwrap_or(0.0);
        total += amt;
        let cat = t.category.clone().unwrap_or_else(|| "未分类".into());
        *by_cat.entry(cat).or_insert(0.0) += amt;
    }
    let mut by_category: Vec<(String, f64)> = by_cat.into_iter().collect();
    by_category.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    by_category.truncate(5);
    SpendingSection {
        total,
        currency: currency.to_string(),
        by_category,
    }
}

/// WealthSection from the two latest net-worth snapshots. With <2 snapshots,
/// deltas are 0. With none, everything is 0 (new user).
pub fn wealth_from_snapshots(db: &Db, user_id: &str, base_currency: &str) -> WealthSection {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let from = (Utc::now() - Duration::days(14))
        .format("%Y-%m-%d")
        .to_string();
    let series = db
        .net_worth_series(user_id, &from, &today)
        .unwrap_or_default();
    let latest = series.last();
    let prev = if series.len() >= 2 {
        series.get(series.len() - 2)
    } else {
        None
    };
    match latest {
        Some(l) => WealthSection {
            net_worth: l.net_amt,
            net_delta: prev.map(|p| l.net_amt - p.net_amt).unwrap_or(0.0),
            cash: l.cash_amt,
            investments: l.investments_amt,
            investments_delta: prev
                .map(|p| l.investments_amt - p.investments_amt)
                .unwrap_or(0.0),
            debt: l.debt_amt,
            currency: l.base_currency.clone(),
        },
        None => WealthSection {
            net_worth: 0.0,
            net_delta: 0.0,
            cash: 0.0,
            investments: 0.0,
            investments_delta: 0.0,
            debt: 0.0,
            currency: base_currency.to_string(),
        },
    }
}

/// Assemble the full digest for one user. `market` is the shared brief (may be
/// None). `now_utc` and `tz` define which local "yesterday" to summarize.
pub fn build_digest(
    db: &Db,
    user_id: &str,
    base_currency: &str,
    now_utc: DateTime<Utc>,
    tz: Tz,
    market: Option<MarketBrief>,
) -> anyhow::Result<Digest> {
    let (start, end) = yesterday_bounds(now_utc, tz);
    let txns = db.list_transactions(user_id, start, end, None, None)?;
    let spending = spending_from_txns(&txns, base_currency);
    let wealth = wealth_from_snapshots(db, user_id, base_currency);
    let date = (now_utc.with_timezone(&tz).date_naive() - Duration::days(1))
        .format("%Y-%m-%d")
        .to_string();
    Ok(Digest {
        date,
        spending,
        wealth,
        market,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, AccountKind};
    use chrono_tz::Asia::Shanghai;
    use rust_decimal::Decimal;

    fn txn(kind: TxnKind, amt: i64, cat: Option<&str>) -> Transaction {
        Transaction {
            id: "t".into(),
            kind,
            amount: Decimal::new(amt, 0),
            currency: "CNY".into(),
            account_id: "a".into(),
            counter_account_id: None,
            category: cat.map(|c| c.to_string()),
            note: None,
            occurred_at: Utc::now(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn aggregates_expenses_only_top5_desc() {
        let txns = vec![
            txn(TxnKind::Expense, 100, Some("餐饮")),
            txn(TxnKind::Expense, 50, Some("餐饮")),
            txn(TxnKind::Expense, 200, Some("购物")),
            txn(TxnKind::Income, 9999, Some("工资")),
            txn(TxnKind::Expense, 30, None),
        ];
        let s = spending_from_txns(&txns, "CNY");
        assert_eq!(s.total, 380.0);
        assert_eq!(s.by_category[0], ("购物".into(), 200.0));
        assert_eq!(s.by_category[1], ("餐饮".into(), 150.0));
        assert!(s.by_category.iter().any(|(c, _)| c == "未分类"));
    }

    #[test]
    fn yesterday_bounds_span_one_local_day() {
        let now = Utc.with_ymd_and_hms(2026, 5, 29, 2, 0, 0).unwrap();
        let (start, end) = yesterday_bounds(now, Shanghai);
        assert_eq!(end - start, Duration::days(1));
        assert_eq!(start.with_timezone(&Shanghai).day(), 28);
        assert_eq!(start.with_timezone(&Shanghai).hour(), 0);
    }

    #[test]
    fn build_digest_end_to_end_from_db() {
        let db = Db::open_in_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 5, 29, 2, 0, 0).unwrap();
        let (start, _end) = yesterday_bounds(now, Shanghai);
        let acct = Account {
            id: "a1".into(),
            name: "微信".into(),
            kind: AccountKind::Wallet,
            currency: "CNY".into(),
            opening_balance: Decimal::ZERO,
            created_at: Utc::now(),
        };
        db.insert_account("u1", &acct).unwrap();
        db.insert_transaction(
            "u1",
            &Transaction {
                id: "tx1".into(),
                kind: TxnKind::Expense,
                amount: Decimal::new(4200, 2),
                currency: "CNY".into(),
                account_id: "a1".into(),
                counter_account_id: None,
                category: Some("餐饮".into()),
                note: None,
                occurred_at: start + Duration::hours(2),
                created_at: Utc::now(),
            },
        )
        .unwrap();

        let d = build_digest(&db, "u1", "CNY", now, Shanghai, None).unwrap();
        assert_eq!(d.date, "2026-05-28");
        assert_eq!(d.spending.total, 42.0);
        assert_eq!(d.spending.by_category[0], ("餐饮".into(), 42.0));
        assert_eq!(d.wealth.net_worth, 0.0);
        assert!(d.market.is_none());
    }
}
