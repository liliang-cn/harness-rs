//! Subscription / recurring-expense tools.
//!
//! Models the "I pay $250/month for Claude Code, $11/month for Netflix,
//! ¥199/year for a domain, …" reality. A subscription is a *template* —
//! amount + frequency + next-charge date. Each actual charge becomes a
//! regular `transactions` row via `record_subscription_charge`, which
//! also advances the template's `next_charge_date`.
//!
//! A separate CLI mode (`ledger --auto-charge-subs`) scans for due
//! subscriptions across all users and records them. Run it daily via
//! cron / `harness-rs-daemon`.

use crate::model::{Frequency, Subscription, Transaction, TxnKind};
use crate::tools::{is_trial, open_db, trial_limit_result, uid_of};
use chrono::{Local, NaiveDate, TimeZone, Utc};
use harness::ToolError;
use harness::prelude::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use uuid::Uuid;

pub const TRIAL_MAX_SUBSCRIPTIONS: u32 = 5;

fn mk_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

fn need_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "subscription".into(),
            reason: format!("{field} required"),
        })
}

fn parse_decimal(v: &Value, field: &str) -> Result<Decimal, ToolError> {
    if let Some(s) = v.as_str() {
        return Decimal::from_str(s).map_err(|e| ToolError::InvalidArgs {
            name: "subscription".into(),
            reason: format!("{field}: {e}"),
        });
    }
    if let Some(f) = v.as_f64() {
        return Decimal::try_from(f).map_err(|e| ToolError::InvalidArgs {
            name: "subscription".into(),
            reason: format!("{field}: {e}"),
        });
    }
    if let Some(i) = v.as_i64() {
        return Ok(Decimal::from(i));
    }
    Err(ToolError::InvalidArgs {
        name: "subscription".into(),
        reason: format!("{field}: not a number"),
    })
}

fn parse_date(s: &str) -> Result<NaiveDate, ToolError> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| ToolError::InvalidArgs {
        name: "subscription".into(),
        reason: format!("date `{s}` not YYYY-MM-DD"),
    })
}

/// Register a recurring expense (subscription, rent, gym, …). Once
/// registered, you can either record each actual charge manually via
/// `record_subscription_charge`, or let the daily `--auto-charge-subs`
/// runner do it. The subscription itself is NOT a transaction — it's a
/// schedule that GENERATES transactions on each charge.
#[harness::tool(
    name = "add_subscription",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "name":             {"type": "string", "description": "Friendly name, e.g. \"Claude Code Max\", \"Netflix\", \"房租\"."},
            "amount":           {"description": "Per-period amount as a positive number or decimal string."},
            "currency":         {"type": "string", "description": "ISO code, e.g. USD / CNY / EUR. Required — DO NOT guess."},
            "frequency":        {"type": "string", "enum": ["weekly", "monthly", "quarterly", "yearly"]},
            "next_charge_date": {"type": "string", "description": "YYYY-MM-DD of next expected charge. Required."},
            "account_id":       {"type": "string", "description": "Which account gets charged. If omitted and the user only has one account, that one is used."},
            "category":         {"type": "string", "description": "Defaults to \"订阅\"."},
            "pay_channel":      {"type": "string", "description": "Optional free-form, e.g. \"Android/Google Play\", \"AmEx ****1234\"."},
            "note":             {"type": "string"}
        },
        "required": ["name", "amount", "currency", "frequency", "next_charge_date"]
    }"#
)]
async fn add_subscription(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let name = need_str(&args, "name")?.to_string();
    let amount = parse_decimal(args.get("amount").unwrap(), "amount")?;
    if amount <= Decimal::ZERO {
        return Err(ToolError::InvalidArgs {
            name: "subscription".into(),
            reason: "amount must be positive".into(),
        });
    }
    let currency = need_str(&args, "currency")?.to_uppercase();
    let freq_s = need_str(&args, "frequency")?;
    let frequency = Frequency::parse(freq_s).ok_or_else(|| ToolError::InvalidArgs {
        name: "subscription".into(),
        reason: format!("unknown frequency `{freq_s}` (use weekly|monthly|quarterly|yearly)"),
    })?;
    let next_charge_date = parse_date(need_str(&args, "next_charge_date")?)?;
    let category = args
        .get("category")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| Some("订阅".into()));
    let pay_channel = args
        .get("pay_channel")
        .and_then(|v| v.as_str())
        .map(String::from);
    let note = args.get("note").and_then(|v| v.as_str()).map(String::from);

    let db = open_db()?;
    let uid = uid_of(w)?;
    let account_id = match args.get("account_id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            let accs = db
                .list_accounts(&uid)
                .map_err(|e| ToolError::Exec(e.to_string()))?;
            if accs.len() != 1 {
                return Ok(ToolResult {
                    ok: false,
                    content: json!({
                        "error": "account_id required",
                        "hint": "user has 0 or 2+ accounts — pick one explicitly via list_accounts",
                        "account_count": accs.len(),
                    }),
                    trace: None,
                });
            }
            accs[0].id.clone()
        }
    };

    if is_trial(w) {
        let n = db
            .count_user_subscriptions(&uid)
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        if n >= TRIAL_MAX_SUBSCRIPTIONS {
            return Ok(trial_limit_result("subscriptions", n, TRIAL_MAX_SUBSCRIPTIONS));
        }
    }

    let sub = Subscription {
        id: mk_id(),
        name,
        amount,
        currency,
        frequency,
        next_charge_date,
        account_id,
        category,
        pay_channel,
        note,
        status: "active".into(),
        created_at: Utc::now(),
        cancelled_at: None,
    };
    db.insert_subscription(&uid, &sub)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"added": sub}),
        trace: None,
    })
}

/// List subscriptions for the current user. Active only by default; pass
/// `include_cancelled: true` to see the full history.
#[harness::tool(
    name = "list_subscriptions",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "include_cancelled": {"type": "boolean", "default": false}
        }
    }"#
)]
async fn list_subscriptions(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let include_cancelled = args
        .get("include_cancelled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let db = open_db()?;
    let uid = uid_of(w)?;
    let subs = db
        .list_subscriptions(&uid, !include_cancelled)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"count": subs.len(), "subscriptions": subs}),
        trace: None,
    })
}

/// Cancel a subscription — keeps the row + past charge history, just stops
/// future auto-charges. Use the id from `list_subscriptions`.
#[harness::tool(
    name = "cancel_subscription",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "subscription_id": {"type": "string"}
        },
        "required": ["subscription_id"]
    }"#
)]
async fn cancel_subscription(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let id = need_str(&args, "subscription_id")?.to_string();
    let db = open_db()?;
    let uid = uid_of(w)?;
    let n = db
        .cancel_subscription(&uid, &id)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: n > 0,
        content: if n > 0 {
            json!({"cancelled_subscription_id": id})
        } else {
            json!({"error": format!("no active subscription with id `{id}`")})
        },
        trace: None,
    })
}

/// Record a real-world charge against a subscription: creates a
/// `transactions` row (currency = subscription currency, category =
/// subscription category, note prefixed with the subscription name) AND
/// advances the subscription's `next_charge_date` by one period.
///
/// Use this when the user says "扣款了" / "Netflix 这个月扣了" / "yearly
/// renewal just hit"; the daily `--auto-charge-subs` runner calls the
/// same flow under the hood.
#[harness::tool(
    name = "record_subscription_charge",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "subscription_id": {"type": "string"},
            "occurred_at":     {"type": "string", "description": "YYYY-MM-DD when the charge actually hit. Defaults to today."}
        },
        "required": ["subscription_id"]
    }"#
)]
async fn record_subscription_charge(
    args: Value,
    w: &mut World,
) -> Result<ToolResult, ToolError> {
    let sub_id = need_str(&args, "subscription_id")?.to_string();
    let db = open_db()?;
    let uid = uid_of(w)?;
    let sub = db
        .get_subscription(&uid, &sub_id)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "subscription".into(),
            reason: format!("no subscription with id `{sub_id}`"),
        })?;
    if sub.status != "active" {
        return Ok(ToolResult {
            ok: false,
            content: json!({"error": format!("subscription `{}` is {}", sub.name, sub.status)}),
            trace: None,
        });
    }
    let occurred_naive = match args.get("occurred_at").and_then(|v| v.as_str()) {
        Some(s) => parse_date(s)?,
        None => Utc::now().date_naive(),
    };
    // Anchor the transaction to local 15:00 (matches portfolio trade default).
    let occurred_at = Local
        .from_local_datetime(&occurred_naive.and_hms_opt(15, 0, 0).unwrap())
        .single()
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);

    if is_trial(w) {
        let n = db
            .count_user_transactions(&uid)
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        if n >= crate::auth::TRIAL_MAX_TRANSACTIONS {
            return Ok(trial_limit_result(
                "transactions",
                n,
                crate::auth::TRIAL_MAX_TRANSACTIONS,
            ));
        }
    }

    let note_prefix = format!("[订阅] {}", sub.name);
    let combined_note = match sub.note.as_deref() {
        Some(n) if !n.is_empty() => format!("{note_prefix} · {n}"),
        _ => note_prefix.clone(),
    };
    let txn = Transaction {
        id: mk_id(),
        kind: TxnKind::Expense,
        amount: sub.amount,
        currency: sub.currency.clone(),
        account_id: sub.account_id.clone(),
        counter_account_id: None,
        category: sub.category.clone(),
        note: Some(combined_note),
        occurred_at,
        created_at: Utc::now(),
    };
    db.insert_transaction(&uid, &txn)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    db.advance_subscription(&uid, &sub_id)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    // Re-fetch so the response carries the updated next_charge_date.
    let updated = db
        .get_subscription(&uid, &sub_id)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "charged": txn,
            "subscription": updated,
        }),
        trace: None,
    })
}
