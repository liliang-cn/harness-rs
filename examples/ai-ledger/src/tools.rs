use crate::db::{Db, today_year_month};
use crate::model::*;
use chrono::{DateTime, Local, TimeZone, Utc};
use harness::ToolError;
use harness::prelude::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::str::FromStr;
use uuid::Uuid;

pub fn ledger_path() -> PathBuf {
    if let Ok(p) = std::env::var("HARNESS_LEDGER_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".harness-ledger").join("ledger.db")
}

/// Read the authenticated user_id from the agent's `World.profile.extra`.
/// The HTTP layer (`server::chat_*_handler`) plants it on every request.
pub(crate) fn uid_of(w: &World) -> Result<String, ToolError> {
    w.profile
        .extra::<String>("user_id")
        .ok_or_else(|| ToolError::Exec("no user_id on world".into()))
}

pub(crate) fn is_trial(w: &World) -> bool {
    w.profile
        .extra::<String>("tier")
        .map(|t| t == "trial")
        .unwrap_or(true)
}

pub(crate) fn trial_limit_result(kind: &str, used: u32, limit: u32) -> ToolResult {
    ToolResult {
        ok: false,
        content: serde_json::json!({
            "error": "trial_limit",
            "kind": kind,
            "used": used,
            "limit": limit,
            "hint": "trial 额度上限，升级 paid 账户解除（找邀请码注册）",
        }),
        trace: None,
    }
}

pub(crate) fn open_db() -> Result<Db, ToolError> {
    let p = ledger_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::Exec(e.to_string()))?;
    }
    Db::open(&p).map_err(|e| ToolError::Exec(format!("db open: {e}")))
}

fn mk_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .and_then(|n| Local.from_local_datetime(&n).single())
                .map(|d| d.with_timezone(&Utc))
        })
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(12, 0, 0))
                .and_then(|n| Local.from_local_datetime(&n).single())
                .map(|d| d.with_timezone(&Utc))
        })
}

fn parse_decimal(v: &Value, field: &str) -> Result<Decimal, ToolError> {
    if let Some(s) = v.as_str() {
        return Decimal::from_str(s).map_err(|e| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("{field}: {e}"),
        });
    }
    if let Some(f) = v.as_f64() {
        return Decimal::try_from(f).map_err(|e| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("{field}: {e}"),
        });
    }
    if let Some(i) = v.as_i64() {
        return Ok(Decimal::from(i));
    }
    Err(ToolError::InvalidArgs {
        name: "ledger".into(),
        reason: format!("{field}: not a number"),
    })
}

fn need_str<'a>(args: &'a Value, field: &str) -> Result<&'a str, ToolError> {
    args.get(field)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("{field} required"),
        })
}

// ============================================================
// current_time — same idea as personal-assistant
// ============================================================

/// Get the current wall-clock time. Always call this before interpreting relative
/// dates like "今天" / "yesterday" / "上周" / "next Friday".
#[harness::tool(
    name = "current_time",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn current_time(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let now_utc = Utc::now();
    let tz_name = w.profile.tz.clone();
    let (iso_local, weekday, human, tz_source) = match tz_name
        .as_deref()
        .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
    {
        Some(tz) => {
            let local = now_utc.with_timezone(&tz);
            (
                local.to_rfc3339(),
                local.format("%A").to_string(),
                local.format("%Y-%m-%d %H:%M %Z").to_string(),
                format!("profile.tz={}", tz_name.as_deref().unwrap_or("?")),
            )
        }
        None => {
            let local = now_utc.with_timezone(&Local);
            (
                local.to_rfc3339(),
                local.format("%A").to_string(),
                local.format("%Y-%m-%d %H:%M %Z").to_string(),
                "system-clock".into(),
            )
        }
    };
    Ok(ToolResult {
        ok: true,
        content: json!({
            "iso_utc": now_utc.to_rfc3339(),
            "iso_local": iso_local,
            "weekday": weekday,
            "human": human,
            "timezone": tz_source,
        }),
        trace: None,
    })
}

// ============================================================
// accounts
// ============================================================

/// List all accounts (cash / debit / credit / wallet) with their currency and id.
/// Call this whenever you need an `account_id` and the user only said a friendly name.
#[harness::tool(
    name = "list_accounts",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn list_accounts(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let accs = db
        .list_accounts(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"count": accs.len(), "accounts": accs}),
        trace: None,
    })
}

/// Create a new account (e.g. 微信钱包 / 招商银行储蓄卡 / Wise USD).
/// Ask the user before silently inventing accounts.
#[harness::tool(
    name = "add_account",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "name":            {"type": "string", "description": "Human label, e.g. 微信 / 招行信用卡."},
            "kind":            {"type": "string", "enum": ["cash", "debit", "credit", "wallet", "other"], "default": "other"},
            "currency":        {"type": "string", "description": "ISO 4217, e.g. CNY / USD / JPY.", "default": "CNY"},
            "opening_balance": {"type": "string", "description": "Decimal string, e.g. \"0\" or \"-1234.56\".", "default": "0"}
        },
        "required": ["name"]
    }"#
)]
async fn add_account(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let name = need_str(&args, "name")?.to_string();
    let kind_s = args.get("kind").and_then(|v| v.as_str()).unwrap_or("other");
    let kind: AccountKind = serde_json::from_str(&format!("\"{kind_s}\""))
        .map_err(|_| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("unknown kind `{kind_s}`"),
        })?;
    let currency = args
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("CNY")
        .to_uppercase();
    let opening = match args.get("opening_balance") {
        Some(v) => parse_decimal(v, "opening_balance")?,
        None => Decimal::ZERO,
    };
    let a = Account {
        id: mk_id(),
        name,
        kind,
        currency,
        opening_balance: opening,
        created_at: Utc::now(),
    };
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    db.insert_account(&uid, &a)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"added": a}),
        trace: None,
    })
}

// ============================================================
// transactions
// ============================================================

/// Record a single expense or income. Use record_transfer for movements between
/// the user's own accounts. `amount` is always positive — `kind` carries the sign.
#[harness::tool(
    name = "log_transaction",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "kind":        {"type": "string", "enum": ["expense", "income"], "default": "expense"},
            "amount":      {"description": "Positive amount as decimal string preferred (\"12.50\"); ints also accepted."},
            "currency":    {"type": "string", "default": "CNY"},
            "account_id":  {"type": "string", "description": "From list_accounts. Omit only if exactly one account exists."},
            "category":    {"type": "string", "description": "Free-form category name. Reuse existing where possible (see list_categories)."},
            "note":        {"type": "string"},
            "occurred_at": {"type": "string", "description": "ISO 8601 / RFC3339 or YYYY-MM-DD. Defaults to now."}
        },
        "required": ["amount"]
    }"#
)]
async fn log_transaction(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let kind_s = args
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("expense");
    let kind: TxnKind =
        serde_json::from_str(&format!("\"{kind_s}\"")).map_err(|_| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("unknown kind `{kind_s}`"),
        })?;
    if matches!(kind, TxnKind::Transfer) {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "use record_transfer for transfers".into(),
        });
    }
    let amount_val = args.get("amount").ok_or_else(|| ToolError::InvalidArgs {
        name: "ledger".into(),
        reason: "amount required".into(),
    })?;
    let amount = parse_decimal(amount_val, "amount")?;
    let currency = args
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("CNY")
        .to_uppercase();

    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
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
                        "hint": "call list_accounts and pick one, or add_account first",
                        "account_count": accs.len(),
                    }),
                    trace: None,
                });
            }
            accs[0].id.clone()
        }
    };
    let category = args
        .get("category")
        .and_then(|v| v.as_str())
        .map(String::from);
    let note = args.get("note").and_then(|v| v.as_str()).map(String::from);
    let occurred_at = match args.get("occurred_at").and_then(|v| v.as_str()) {
        Some(s) => parse_iso(s).ok_or_else(|| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("could not parse `{s}` — use RFC3339 or YYYY-MM-DD"),
        })?,
        None => Utc::now(),
    };

    let t = Transaction {
        id: mk_id(),
        kind,
        amount,
        currency,
        account_id,
        counter_account_id: None,
        category,
        note,
        occurred_at,
        created_at: Utc::now(),
    };
    if is_trial(w) {
        let n = db
            .count_user_transactions(&uid)
            .map_err(|e| ToolError::Exec(e.to_string()))?;
        if n >= 50 {
            return Ok(trial_limit_result("transactions", n, 50));
        }
    }
    db.insert_transaction(&uid, &t)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"logged": t}),
        trace: None,
    })
}

/// Record a transfer between two of the user's accounts. Writes two legs so
/// monthly_report does not double-count this as an expense.
#[harness::tool(
    name = "record_transfer",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "from_account_id": {"type": "string"},
            "to_account_id":   {"type": "string"},
            "amount":          {"description": "Positive decimal string preferred."},
            "currency":        {"type": "string", "default": "CNY"},
            "note":            {"type": "string"},
            "occurred_at":     {"type": "string"}
        },
        "required": ["from_account_id", "to_account_id", "amount"]
    }"#
)]
async fn record_transfer(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let from = need_str(&args, "from_account_id")?.to_string();
    let to = need_str(&args, "to_account_id")?.to_string();
    if from == to {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "from and to accounts must differ".into(),
        });
    }
    let amount = parse_decimal(
        args.get("amount").ok_or_else(|| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "amount required".into(),
        })?,
        "amount",
    )?;
    let currency = args
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("CNY")
        .to_uppercase();
    let note = args.get("note").and_then(|v| v.as_str()).map(String::from);
    let occurred_at = match args.get("occurred_at").and_then(|v| v.as_str()) {
        Some(s) => parse_iso(s).ok_or_else(|| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("could not parse `{s}`"),
        })?,
        None => Utc::now(),
    };

    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    if db
        .get_account(&uid, &from)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .is_none()
    {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("from_account_id `{from}` does not exist"),
        });
    }
    if db
        .get_account(&uid, &to)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .is_none()
    {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("to_account_id `{to}` does not exist"),
        });
    }

    let now = Utc::now();
    let out = Transaction {
        id: mk_id(),
        kind: TxnKind::Transfer,
        amount,
        currency: currency.clone(),
        account_id: from.clone(),
        counter_account_id: Some(to.clone()),
        category: None,
        note: note.clone(),
        occurred_at,
        created_at: now,
    };
    let inb = Transaction {
        id: mk_id(),
        kind: TxnKind::Transfer,
        amount,
        currency,
        account_id: to,
        counter_account_id: Some(from),
        category: None,
        note,
        occurred_at,
        created_at: now,
    };
    db.insert_transaction(&uid, &out)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    db.insert_transaction(&uid, &inb)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"out": out, "in": inb}),
        trace: None,
    })
}

/// Query transactions within a date range, optionally filtered by category or account.
#[harness::tool(
    name = "list_transactions",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "from":       {"type": "string", "description": "RFC3339 / YYYY-MM-DD start, default 30 days ago."},
            "to":         {"type": "string", "description": "RFC3339 / YYYY-MM-DD end, default now."},
            "category":   {"type": "string"},
            "account_id": {"type": "string"},
            "limit":      {"type": "integer", "default": 50, "minimum": 1, "maximum": 500}
        }
    }"#
)]
async fn list_transactions(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let now = Utc::now();
    let from = args
        .get("from")
        .and_then(|v| v.as_str())
        .and_then(parse_iso)
        .unwrap_or(now - chrono::Duration::days(30));
    let to = args
        .get("to")
        .and_then(|v| v.as_str())
        .and_then(parse_iso)
        .unwrap_or(now);
    let category = args
        .get("category")
        .and_then(|v| v.as_str())
        .map(String::from);
    let account_id = args
        .get("account_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .min(500) as usize;

    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let mut all = db
        .list_transactions(&uid, from, to, category.as_deref(), account_id.as_deref())
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let total = all.len();
    all.truncate(limit);
    Ok(ToolResult {
        ok: true,
        content: json!({
            "range_from": from.to_rfc3339(),
            "range_to":   to.to_rfc3339(),
            "total_matched": total,
            "returned": all.len(),
            "transactions": all,
        }),
        trace: None,
    })
}

// ============================================================
// reports & budgets
// ============================================================

/// Aggregate this month's expenses by category and currency. Defaults to the
/// current month if year/month are omitted.
#[harness::tool(
    name = "monthly_report",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "year":  {"type": "integer", "description": "4-digit year. Defaults to current."},
            "month": {"type": "integer", "minimum": 1, "maximum": 12, "description": "Defaults to current."}
        }
    }"#
)]
async fn monthly_report(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let (cy, cm) = today_year_month();
    let year = args
        .get("year")
        .and_then(|v| v.as_i64())
        .unwrap_or(cy as i64) as i32;
    let month = args
        .get("month")
        .and_then(|v| v.as_u64())
        .unwrap_or(cm as u64) as u32;
    if !(1..=12).contains(&month) {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("month must be 1..12, got {month}"),
        });
    }
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let totals = db
        .monthly_totals(&uid, year, month)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let grand: std::collections::HashMap<String, Decimal> = totals.iter().fold(
        std::collections::HashMap::new(),
        |mut acc, t| {
            *acc.entry(t.currency.clone()).or_insert(Decimal::ZERO) += t.total;
            acc
        },
    );
    Ok(ToolResult {
        ok: true,
        content: json!({
            "year": year,
            "month": month,
            "by_category": totals,
            "grand_total_by_currency": grand,
        }),
        trace: None,
    })
}

/// Set or update the monthly budget for a category. Upserts on (category, currency).
#[harness::tool(
    name = "set_budget",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "category":      {"type": "string"},
            "currency":      {"type": "string", "default": "CNY"},
            "monthly_limit": {"description": "Positive decimal string."}
        },
        "required": ["category", "monthly_limit"]
    }"#
)]
async fn set_budget(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let category = need_str(&args, "category")?.to_string();
    let currency = args
        .get("currency")
        .and_then(|v| v.as_str())
        .unwrap_or("CNY")
        .to_uppercase();
    let limit = parse_decimal(
        args.get("monthly_limit")
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "ledger".into(),
                reason: "monthly_limit required".into(),
            })?,
        "monthly_limit",
    )?;
    if limit <= Decimal::ZERO {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "monthly_limit must be positive".into(),
        });
    }
    let b = Budget {
        category,
        currency,
        monthly_limit: limit,
        created_at: Utc::now(),
    };
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    db.set_budget(&uid, &b)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({"set": b}),
        trace: None,
    })
}

/// Report this month's budget status — how much used, how much remaining, and
/// which categories are over-budget.
#[harness::tool(
    name = "check_budgets",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "year":  {"type": "integer"},
            "month": {"type": "integer", "minimum": 1, "maximum": 12}
        }
    }"#
)]
async fn check_budgets(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let (cy, cm) = today_year_month();
    let year = args
        .get("year")
        .and_then(|v| v.as_i64())
        .unwrap_or(cy as i64) as i32;
    let month = args
        .get("month")
        .and_then(|v| v.as_u64())
        .unwrap_or(cm as u64) as u32;
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let statuses = db
        .budget_status(&uid, year, month)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let over: Vec<&BudgetStatus> = statuses.iter().filter(|s| s.over_budget).collect();
    Ok(ToolResult {
        ok: true,
        content: json!({
            "year": year,
            "month": month,
            "budgets": statuses,
            "over_count": over.len(),
        }),
        trace: None,
    })
}

fn jaccard_chars(a: &str, b: &str) -> f64 {
    use std::collections::HashSet;
    let sa: HashSet<char> = a.chars().collect();
    let sb: HashSet<char> = b.chars().collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let shared = sa.intersection(&sb).count() as f64;
    let total = sa.union(&sb).count() as f64;
    shared / total
}

/// Delete a single ledger transaction by id. Use when the user wants to undo a
/// wrong entry. Get the id from `list_transactions`. To remove multiple,
/// call this once per id.
#[harness::tool(
    name = "delete_transaction",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "transaction_id": {"type": "string", "description": "Txn id from list_transactions / log_transaction response."}
        },
        "required": ["transaction_id"]
    }"#
)]
async fn delete_transaction(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let id = need_str(&args, "transaction_id")?.to_string();
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let n = db
        .delete_transaction(&uid, &id)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: n > 0,
        content: if n > 0 {
            json!({"deleted_transaction_id": id})
        } else {
            json!({"error": format!("no transaction with id `{id}`")})
        },
        trace: None,
    })
}

/// List all distinct categories ever used, plus this month's usage per category.
/// Useful for spotting near-duplicates ("吃饭" vs "餐饮") to propose merges.
#[harness::tool(
    name = "list_categories",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "year":  {"type": "integer"},
            "month": {"type": "integer", "minimum": 1, "maximum": 12}
        },
        "description": "List distinct categories with this month's usage. Use it to propose merges of near-duplicate categories (e.g. \"吃饭\" vs \"餐饮\") back to the user."
    }"#
)]
async fn list_categories(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let (cy, cm) = today_year_month();
    let year = args
        .get("year")
        .and_then(|v| v.as_i64())
        .unwrap_or(cy as i64) as i32;
    let month = args
        .get("month")
        .and_then(|v| v.as_u64())
        .unwrap_or(cm as u64) as u32;
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let totals = db
        .monthly_totals(&uid, year, month)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let distinct = db
        .distinct_categories(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "all_known_categories": distinct,
            "this_month_usage": totals,
        }),
        trace: None,
    })
}

/// Surface candidate merges: pairs of categories that share characters (typo /
/// variant detection) and low-usage outliers that may be one-off mis-tags.
/// Propose specific merges to the user; only call `apply_category_merge` after
/// they confirm.
#[harness::tool(
    name = "suggest_category_merges",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn suggest_category_merges(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let cats = db
        .distinct_categories(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let (cy, cm) = today_year_month();
    let totals = db
        .monthly_totals(&uid, cy, cm)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    use std::collections::HashMap;
    let by_cat: HashMap<&str, &CategoryTotal> =
        totals.iter().map(|t| (t.category.as_str(), t)).collect();

    let mut pairs = Vec::new();
    for i in 0..cats.len() {
        for j in (i + 1)..cats.len() {
            let a = &cats[i];
            let b = &cats[j];
            let substr = a.contains(b.as_str()) || b.contains(a.as_str());
            let jac = jaccard_chars(a, b);
            if substr || jac >= 0.5 {
                pairs.push(json!({
                    "a": a, "b": b,
                    "substring": substr,
                    "char_jaccard": (jac * 100.0).round() / 100.0,
                    "reason": if substr { "one is a substring of the other" } else { "high character overlap (likely typo / variant)" },
                }));
            }
        }
    }

    let low_usage: Vec<_> = totals
        .iter()
        .filter(|t| t.count == 1)
        .map(|t| {
            json!({
                "category": t.category,
                "count": t.count,
                "total": t.total.to_string(),
                "currency": t.currency,
            })
        })
        .collect();

    // For each category give a quick usage line so the LLM can recommend
    // direction (always keep the higher-usage one as the canonical name).
    let usage_summary: Vec<_> = cats
        .iter()
        .map(|c| {
            let (cnt, tot) = by_cat
                .get(c.as_str())
                .map(|t| (t.count, t.total.to_string()))
                .unwrap_or((0, "0".into()));
            json!({"category": c, "month_count": cnt, "month_total": tot})
        })
        .collect();

    Ok(ToolResult {
        ok: true,
        content: json!({
            "categories": usage_summary,
            "near_duplicate_pairs": pairs,
            "low_usage_this_month": low_usage,
            "hint": "Always merge the lower-usage name INTO the higher-usage canonical name. Ask the user to confirm before calling apply_category_merge.",
        }),
        trace: None,
    })
}

/// Rewrite category name across all transactions and budgets. Destructive,
/// non-reversible without a backup. ALWAYS ask the user to confirm the
/// (from → to) direction before calling — never auto-merge.
#[harness::tool(
    name = "apply_category_merge",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "from": {"type": "string", "description": "Category name to rename away (typically lower-usage)."},
            "to":   {"type": "string", "description": "Canonical category name to keep."}
        },
        "required": ["from", "to"]
    }"#
)]
async fn apply_category_merge(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let from = need_str(&args, "from")?.to_string();
    let to = need_str(&args, "to")?.to_string();
    if from == to {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "from and to must differ".into(),
        });
    }
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let (txn_n, bud_n) = db
        .rename_category(&uid, &from, &to)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    if txn_n == 0 && bud_n == 0 {
        return Ok(ToolResult {
            ok: false,
            content: json!({
                "error": format!("no transactions or budgets had category `{from}`"),
            }),
            trace: None,
        });
    }
    Ok(ToolResult {
        ok: true,
        content: json!({
            "from": from,
            "to": to,
            "transactions_updated": txn_n,
            "budgets_removed_on_collision": bud_n,
        }),
        trace: None,
    })
}
