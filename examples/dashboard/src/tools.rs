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

/// Get the user's current net worth: cash + investments − debt, aggregated
/// to their base_currency. Use this when the user asks "how am I doing?" /
/// "what's my net worth?" / "我现在多少身家" / "this month vs last month".
///
/// Returns the latest snapshot plus a delta vs `compare_days` ago (default
/// 30). If no historical snapshot exists for the comparison date, returns
/// the absolute number without a delta.
#[harness::tool(
    name = "get_net_worth",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "compare_days": {
                "type": "integer",
                "description": "Days ago to compare against for delta (default 30). Use 7 for week-over-week, 30 for monthly, 365 for year-over-year.",
                "default": 30
            }
        }
    }"#
)]
async fn get_net_worth(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let compare_days = args
        .get("compare_days")
        .and_then(|v| v.as_i64())
        .unwrap_or(30)
        .clamp(1, 3650) as i64;
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    // Pull the user to get their base_currency (canonical). Profile.extra
    // doesn't carry it.
    let user = db
        .get_user_by_id(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .ok_or_else(|| ToolError::Exec("user gone".into()))?;
    let base = user.base_currency.clone();

    // Latest snapshot — if missing (brand-new user, cron hasn't run),
    // compute one now so the agent has something to say.
    let snap = match db
        .latest_net_worth_snapshot(&uid)
        .map_err(|e| ToolError::Exec(e.to_string()))?
    {
        Some(s) => s,
        None => crate::net_worth::snapshot_now(&db, &uid, &base)
            .map_err(|e| ToolError::Exec(format!("snapshot: {e}")))?,
    };

    // Past snapshot for delta — pick the first row on or after the target
    // date. If none exists yet (history shorter than compare_days), skip.
    let target = (chrono::Utc::now() - chrono::Duration::days(compare_days))
        .format("%Y-%m-%d")
        .to_string();
    let series = db
        .net_worth_series(&uid, &target, &snap.snapshot_date)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    let past = series
        .iter()
        .find(|s| s.snapshot_date >= target && s.snapshot_date < snap.snapshot_date);

    let (delta_abs, delta_pct, past_date) = match past {
        Some(p) if p.net_amt != 0.0 => {
            let abs = snap.net_amt - p.net_amt;
            let pct = (abs / p.net_amt.abs()) * 100.0;
            (Some(abs), Some(pct), Some(p.snapshot_date.clone()))
        }
        _ => (None, None, None),
    };

    Ok(ToolResult {
        ok: true,
        content: json!({
            "as_of": snap.snapshot_date,
            "base_currency": base,
            "net": snap.net_amt,
            "cash": snap.cash_amt,
            "investments": snap.investments_amt,
            "debt": snap.debt_amt,
            "compare_days": compare_days,
            "compared_to": past_date,
            "delta_abs": delta_abs,
            "delta_pct": delta_pct,
        }),
        trace: None,
    })
}

// ============================================================
// loans / mortgages / receivables
// ============================================================

/// Create a new loan, mortgage, or receivable. Creates the underlying account
/// row AND the loans row in one shot. Sign of `opening_balance` is flipped
/// per kind: Loan/Mortgage → negative (you owe); Receivable → positive
/// (someone owes you).
///
/// Ask the user to confirm principal + APR + start_date before calling.
/// Do NOT invent term_months or monthly_payment — leave them null if the
/// user didn't volunteer them.
#[harness::tool(
    name = "add_loan",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "name":            {"type": "string", "description": "Short label shown in the UI, e.g. \"Home mortgage\", \"Toyota Corolla loan\", \"Lent to Alice\"."},
            "kind":            {"type": "string", "enum": ["loan", "mortgage", "receivable"], "description": "loan = generic borrowing; mortgage = home loan with amortization; receivable = someone owes you."},
            "counterparty":    {"type": "string", "description": "Bank / lender / friend's name."},
            "principal":       {"type": "string", "description": "Original amount, positive decimal as string. For receivables, this is what you lent out."},
            "currency":        {"type": "string", "description": "ISO 4217, e.g. \"USD\", \"CNY\"."},
            "apr":             {"type": "string", "description": "Annual percentage rate as decimal, e.g. \"0.045\" for 4.5%. Use \"0\" for interest-free IOUs."},
            "term_months":     {"type": "integer", "nullable": true, "description": "Loan duration in months. Use null for open-ended IOUs."},
            "monthly_payment": {"type": "string", "nullable": true, "description": "Optional. Recurring payment amount as decimal string."},
            "start_date":      {"type": "string", "description": "YYYY-MM-DD. When the loan was originated."},
            "note":            {"type": "string", "nullable": true}
        },
        "required": ["name", "kind", "counterparty", "principal", "currency", "apr", "start_date"]
    }"#
)]
async fn add_loan(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let name = need_str(&args, "name")?.to_string();
    let kind_s = need_str(&args, "kind")?;
    let kind: AccountKind = match kind_s {
        "loan" | "mortgage" | "receivable" => {
            serde_json::from_str(&format!("\"{kind_s}\"")).map_err(|_| ToolError::InvalidArgs {
                name: "ledger".into(),
                reason: format!("unknown kind `{kind_s}`"),
            })?
        }
        other => {
            return Err(ToolError::InvalidArgs {
                name: "ledger".into(),
                reason: format!(
                    "kind `{other}` not allowed for add_loan — use loan / mortgage / receivable"
                ),
            });
        }
    };
    let counterparty = need_str(&args, "counterparty")?.to_string();
    let principal_val = args
        .get("principal")
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "principal required".into(),
        })?;
    let principal = parse_decimal(principal_val, "principal")?;
    if principal <= Decimal::ZERO {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "principal must be positive".into(),
        });
    }
    let currency = need_str(&args, "currency")?.to_uppercase();
    let apr_s = need_str(&args, "apr")?.to_string();
    let _apr_check: f64 = apr_s.parse().map_err(|_| ToolError::InvalidArgs {
        name: "ledger".into(),
        reason: format!("apr `{apr_s}` is not a decimal"),
    })?;
    let term_months = args.get("term_months").and_then(|v| v.as_i64());
    let monthly_payment = args
        .get("monthly_payment")
        .and_then(|v| v.as_str())
        .map(String::from);
    let start_date = need_str(&args, "start_date")?.to_string();
    // Validate start_date format up-front so we don't half-insert.
    chrono::NaiveDate::parse_from_str(&start_date, "%Y-%m-%d").map_err(|_| {
        ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("start_date `{start_date}` must be YYYY-MM-DD"),
        }
    })?;
    let note = args.get("note").and_then(|v| v.as_str()).map(String::from);

    // Sign convention: Loan/Mortgage → -principal (you owe). Receivable → +principal.
    let opening = match kind {
        AccountKind::Receivable => principal,
        _ => -principal,
    };

    let acct = Account {
        id: mk_id(),
        name: name.clone(),
        kind,
        currency: currency.clone(),
        opening_balance: opening,
        created_at: Utc::now(),
    };

    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    db.insert_account(&uid, &acct)
        .map_err(|e| ToolError::Exec(format!("insert_account: {e}")))?;
    // If the loans-row insert fails the account still exists; report the
    // error so the user can decide whether to retry. record_loan_payment
    // would be inert against an orphaned account anyway.
    db.insert_loan(
        &acct.id,
        &uid,
        &counterparty,
        &principal.to_string(),
        &apr_s,
        term_months,
        monthly_payment.as_deref(),
        &start_date,
        note.as_deref(),
    )
    .map_err(|e| ToolError::Exec(format!("insert_loan: {e}")))?;

    let kind_label = match kind {
        AccountKind::Mortgage => "mortgage",
        AccountKind::Receivable => "receivable",
        _ => "loan",
    };
    let summary = format!(
        "added {kind_label} `{name}` ({counterparty}) — principal {} {currency} @ {} APR",
        principal, apr_s
    );
    Ok(ToolResult {
        ok: true,
        content: json!({
            "account_id":   acct.id,
            "kind":         acct.kind,
            "summary":      summary,
        }),
        trace: None,
    })
}

/// Record a loan / mortgage / receivable payment. Writes ONE Transfer
/// transaction with account_id/counter_account_id flipped depending on
/// direction:
///   * Loan / Mortgage  →  cash → loan       (cash decreases, debt moves toward 0)
///   * Receivable        →  receivable → cash (their debt to you decreases, cash up)
/// Look up the loan first via `loan_summary` if you only have a friendly name.
#[harness::tool(
    name = "record_loan_payment",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "loan_account_id":  {"type": "string", "description": "The id of the Loan/Mortgage/Receivable account."},
            "cash_account_id":  {"type": "string", "description": "The user's cash account the payment came from (Loan/Mortgage case) or went to (Receivable case)."},
            "amount":           {"type": "string", "description": "Payment amount, positive decimal as string."},
            "occurred_at":      {"type": "string", "description": "RFC3339 or YYYY-MM-DD. When the payment happened."},
            "note":             {"type": "string", "nullable": true}
        },
        "required": ["loan_account_id", "cash_account_id", "amount", "occurred_at"]
    }"#
)]
async fn record_loan_payment(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let loan_id = need_str(&args, "loan_account_id")?.to_string();
    let cash_id = need_str(&args, "cash_account_id")?.to_string();
    if loan_id == cash_id {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "loan_account_id and cash_account_id must differ".into(),
        });
    }
    let amount_val = args.get("amount").ok_or_else(|| ToolError::InvalidArgs {
        name: "ledger".into(),
        reason: "amount required".into(),
    })?;
    let amount = parse_decimal(amount_val, "amount")?;
    if amount <= Decimal::ZERO {
        return Err(ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: "amount must be positive".into(),
        });
    }
    let occurred_at_s = need_str(&args, "occurred_at")?;
    let occurred_at = parse_iso(occurred_at_s).ok_or_else(|| ToolError::InvalidArgs {
        name: "ledger".into(),
        reason: format!("could not parse `{occurred_at_s}` — use RFC3339 or YYYY-MM-DD"),
    })?;
    let note = args.get("note").and_then(|v| v.as_str()).map(String::from);

    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;

    let loan_acct = db
        .get_account(&uid, &loan_id)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("loan_account_id `{loan_id}` does not exist"),
        })?;
    let cash_acct = db
        .get_account(&uid, &cash_id)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "ledger".into(),
            reason: format!("cash_account_id `{cash_id}` does not exist"),
        })?;

    // Pick from/to per the direction rule above.
    let (from, to) = match loan_acct.kind {
        AccountKind::Loan | AccountKind::Mortgage => (cash_id.clone(), loan_id.clone()),
        AccountKind::Receivable => (loan_id.clone(), cash_id.clone()),
        other => {
            return Err(ToolError::InvalidArgs {
                name: "ledger".into(),
                reason: format!(
                    "loan_account_id `{loan_id}` is a `{:?}` account, not a Loan/Mortgage/Receivable",
                    other
                ),
            });
        }
    };

    // ONE Transfer row. compute_account_balance / net_worth fold both legs
    // off this single row (own = -amt outgoing, counter = +amt incoming),
    // so we never double-count.
    let txn = Transaction {
        id: mk_id(),
        kind: TxnKind::Transfer,
        amount,
        currency: loan_acct.currency.clone(),
        account_id: from,
        counter_account_id: Some(to),
        category: Some("loan_payment".into()),
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
    db.insert_transaction(&uid, &txn)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    // Note: we deliberately do NOT auto-flip the loan to `paid_off` here —
    // tiny rounding from daily interest accrual would otherwise leave the
    // status flapping. The user can retire it via the REST endpoint.
    let new_balance = db
        .compute_account_balance(&uid, &loan_acct.id)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "logged": txn,
            "loan_balance_after": format!("{:.2}", new_balance),
            "cash_account":       cash_acct.name,
            "loan_account":       loan_acct.name,
        }),
        trace: None,
    })
}

// ============================================================
// receipt attachments — Gemini Vision extraction
// ============================================================

/// POST an image to Gemini's `gemini-3.5-flash:generateContent` with an
/// inline_data part + a strict `response_schema`. Returns the parsed
/// structured object (NOT the raw Gemini envelope) — Gemini wraps
/// `response_mime_type=application/json` output as a JSON string inside
/// `candidates[0].content.parts[0].text`, so we parse twice.
async fn gemini_extract_receipt(
    api_key: &str,
    mime_type: &str,
    image_bytes: &[u8],
) -> anyhow::Result<Value> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(image_bytes);
    let body = json!({
        "contents": [{
            "parts": [
                {"text": "Extract the receipt as JSON matching the schema. If a field is unclear, set it to null. Use ISO 4217 currency codes. occurred_at is ISO 8601 (YYYY-MM-DD)."},
                {"inline_data": {"mime_type": mime_type, "data": b64}}
            ]
        }],
        "generationConfig": {
            "response_mime_type": "application/json",
            "response_schema": {
                "type": "object",
                "properties": {
                    "merchant":      {"type": "string", "nullable": true},
                    "amount":        {"type": "string"},
                    "currency":      {"type": "string"},
                    "occurred_at":   {"type": "string"},
                    "category_hint": {"type": "string", "nullable": true},
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name":  {"type": "string"},
                                "price": {"type": "string", "nullable": true}
                            },
                            "required": ["name"]
                        }
                    },
                    "raw_text":   {"type": "string", "nullable": true},
                    "confidence": {"type": "string", "enum": ["high", "medium", "low"]}
                },
                "required": ["amount", "currency", "occurred_at", "confidence"]
            }
        }
    });

    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.5-flash:generateContent?key={api_key}"
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .map_err(|e| anyhow::anyhow!("reqwest build: {e}"))?;
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("gemini request: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("gemini body: {e}"))?;
    if !status.is_success() {
        // Surface the raw response body so the caller sees what Gemini said.
        return Err(anyhow::anyhow!("gemini {}: {}", status, text));
    }
    let envelope: Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("gemini envelope parse: {e}; body={text}"))?;
    let inner_text = envelope
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow::anyhow!("gemini: candidates[0].content.parts[0].text missing; envelope={envelope}"))?;
    let parsed: Value = serde_json::from_str(inner_text)
        .map_err(|e| anyhow::anyhow!("gemini structured-text parse: {e}; text={inner_text}"))?;
    Ok(parsed)
}

fn uploads_root() -> PathBuf {
    std::env::var("HARNESS_LEDGER_UPLOADS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./uploads"))
}

/// Extract structured receipt data from a previously uploaded attachment.
/// Reads the attachment bytes from disk and POSTs to Gemini Vision with a
/// strict response schema. v1 only handles images — PDF returns an error
/// so the agent can fall back to asking the user to type the entry.
#[harness::tool(
    name = "extract_receipt",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "attachment_id": {"type": "string", "description": "One of the attachment IDs visible on profile.extra.attachment_ids."}
        },
        "required": ["attachment_id"]
    }"#
)]
async fn extract_receipt(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let attachment_id = need_str(&args, "attachment_id")?.to_string();
    let uid = uid_of(w)?;
    let db = open_db()?;
    let rec = db
        .get_attachment(&uid, &attachment_id)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .ok_or_else(|| ToolError::Exec("attachment not found or not yours".into()))?;
    if rec.kind != "image" {
        return Ok(ToolResult {
            ok: false,
            content: json!({
                "error": format!("extract_receipt only handles images in v1 (got kind={})", rec.kind),
                "hint": "ask the user to type the entry manually, or re-upload as an image",
            }),
            trace: None,
        });
    }
    let api_key = std::env::var("GEMINI_API_KEY")
        .map_err(|_| ToolError::Exec("GEMINI_API_KEY not set in env".into()))?;
    let full = uploads_root().join(&rec.path);
    let bytes = std::fs::read(&full)
        .map_err(|e| ToolError::Exec(format!("read attachment {}: {e}", full.display())))?;
    let extracted = gemini_extract_receipt(&api_key, &rec.mime_type, &bytes)
        .await
        .map_err(|e| ToolError::Exec(format!("gemini: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: extracted,
        trace: None,
    })
}

/// List the user's active loans / mortgages / receivables with remaining
/// principal, next due date, and progress %. Same shape as `/api/me/loans`.
/// Use this for overview questions ("我现在有哪些贷款", "how much do I still owe")
/// and to look up an `account_id` before calling `record_loan_payment`.
#[harness::tool(
    name = "loan_summary",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "include_paid_off": {"type": "boolean", "default": false, "description": "If true, also include paid_off / cancelled loans. Defaults to active-only."}
        }
    }"#
)]
async fn loan_summary(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let include_paid_off = args
        .get("include_paid_off")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    let loans = crate::loans::summarise(&db, &uid, include_paid_off)
        .map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "count": loans.len(),
            "include_paid_off": include_paid_off,
            "loans": loans,
        }),
        trace: None,
    })
}

// ============================================================
// embedder helper
// ============================================================

fn embedder() -> Result<std::sync::Arc<dyn harness_core::Embedder>, ToolError> {
    crate::embed_slot::get().ok_or_else(|| ToolError::Exec("embedder not configured".into()))
}

// ============================================================
// project tools
// ============================================================

/// Create a project (top-level) or a milestone (pass parent_id). Use kind="project"
/// for any aspiration with an optional target_date + review cadence. Call
/// `current_time` first to resolve relative dates like "今年9月". Pass parent_id
/// to make it a milestone under an existing project.
#[harness::tool(
    name = "create_project",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "name":                 {"type": "string", "description": "Short headline for the project or milestone."},
            "detail":               {"type": "string", "description": "Optional longer description / markdown."},
            "target_date":          {"type": "string", "description": "Target completion date, YYYY-MM-DD, e.g. 2026-09-30. Omit if unknown."},
            "review_interval_days": {"type": "integer", "description": "Review cadence in days (e.g. 7, 30). Omit for milestones.", "minimum": 1},
            "parent_id":            {"type": "string", "description": "If this is a milestone, the parent project id."}
        },
        "required": ["name"]
    }"#
)]
async fn create_project(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let name = args
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "create_project".into(),
            reason: "name required".into(),
        })?;
    let detail = args.get("detail").and_then(|v| v.as_str()).unwrap_or("");
    let target_date = args.get("target_date").and_then(|v| v.as_str());
    let interval = args.get("review_interval_days").and_then(|v| v.as_i64());
    let parent_id = args.get("parent_id").and_then(|v| v.as_str());
    let db = open_db()?;
    let project = db
        .create_project(&uid, name, detail, parent_id, target_date, interval)
        .map_err(|e| ToolError::Exec(format!("insert project: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "id": project.id,
            "name": project.name,
            "parent_id": project.parent_id,
            "target_date": project.target_date,
            "next_review_at": project.next_review_at
        }),
        trace: Some(format!("created project {}", project.id)),
    })
}

/// Break a project into milestones. Pass the parent project id and a list of
/// milestone names/details. Each milestone is created as a child project.
#[harness::tool(
    name = "add_milestones",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "parent_id": {"type": "string", "description": "The parent project id."},
            "milestones": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name":   {"type": "string"},
                        "detail": {"type": "string"}
                    },
                    "required": ["name"]
                }
            }
        },
        "required": ["parent_id", "milestones"]
    }"#
)]
async fn add_milestones(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let parent_id = args
        .get("parent_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "add_milestones".into(),
            reason: "parent_id required".into(),
        })?;
    let milestones = args
        .get("milestones")
        .and_then(|v| v.as_array())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "add_milestones".into(),
            reason: "milestones required".into(),
        })?;
    let db = open_db()?;
    // Validate parent exists + belongs to user.
    if db
        .get_project(&uid, parent_id)
        .map_err(|e| ToolError::Exec(format!("{e}")))?
        .is_none()
    {
        return Err(ToolError::Exec(format!(
            "parent project `{parent_id}` not found"
        )));
    }
    let mut ids = Vec::new();
    for ms in milestones {
        let name = ms.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        if name.is_empty() {
            continue;
        }
        let detail = ms.get("detail").and_then(|v| v.as_str()).unwrap_or("");
        let m = db
            .create_project(&uid, name, detail, Some(parent_id), None, None)
            .map_err(|e| ToolError::Exec(format!("insert milestone: {e}")))?;
        ids.push(m.id);
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "parent_id": parent_id, "created": ids.len(), "ids": ids }),
        trace: Some(format!(
            "added {} milestones to {parent_id}",
            ids.len()
        )),
    })
}

/// Update a project: change status (active|paused|done|dropped), name, detail,
/// target_date (YYYY-MM-DD), or review_interval_days. Get the id first via list_projects.
#[harness::tool(
    name = "update_project",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "id":                   {"type": "string"},
            "status":               {"type": "string", "enum": ["active", "paused", "done", "dropped"]},
            "name":                 {"type": "string"},
            "detail":               {"type": "string"},
            "target_date":          {"type": "string"},
            "review_interval_days": {"type": "integer", "minimum": 1}
        },
        "required": ["id"]
    }"#
)]
async fn update_project(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "update_project".into(),
            reason: "id required".into(),
        })?;
    let db = open_db()?;
    let n = db
        .update_project(
            &uid,
            id,
            args.get("status").and_then(|v| v.as_str()),
            args.get("name").and_then(|v| v.as_str()),
            args.get("detail").and_then(|v| v.as_str()),
            args.get("target_date").and_then(|v| v.as_str()),
            args.get("review_interval_days").and_then(|v| v.as_i64()),
        )
        .map_err(|e| ToolError::Exec(format!("update project: {e}")))?;
    if n == 0 {
        return Err(ToolError::Exec(format!("project `{id}` not found")));
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "id": id, "updated": n }),
        trace: None,
    })
}

/// List the user's projects. Use due_for_review=true to get only projects whose
/// review is due (for 复盘). Pass parent_id to list milestones under a project.
#[harness::tool(
    name = "list_projects",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "status":         {"type": "string", "enum": ["active", "paused", "done", "dropped"]},
            "parent_id":      {"type": "string", "description": "If set, list milestones of this project."},
            "due_for_review": {"type": "boolean", "description": "If true, return only projects whose review is due."}
        }
    }"#
)]
async fn list_projects(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let db = open_db()?;
    let projects = if let Some(pid) = args.get("parent_id").and_then(|v| v.as_str()) {
        db.list_milestones(&uid, pid)
            .map_err(|e| ToolError::Exec(format!("{e}")))?
    } else {
        let due = args
            .get("due_for_review")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let status = args.get("status").and_then(|v| v.as_str()).or(Some("active"));
        db.list_projects(&uid, status, due)
            .map_err(|e| ToolError::Exec(format!("{e}")))?
    };
    Ok(ToolResult {
        ok: true,
        content: json!({ "count": projects.len(), "projects": projects }),
        trace: None,
    })
}

/// Log a review (复盘) for a project: progress + optional next steps. Advances the
/// project's next review by its cadence (or next_review_in_days if provided).
#[harness::tool(
    name = "log_project_review",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "project_id":         {"type": "string"},
            "progress":           {"type": "string", "description": "What happened / self-assessment."},
            "next_steps":         {"type": "string"},
            "next_review_in_days":{"type": "integer", "minimum": 1}
        },
        "required": ["project_id", "progress"]
    }"#
)]
async fn log_project_review(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let project_id = args
        .get("project_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "log_project_review".into(),
            reason: "project_id required".into(),
        })?;
    let progress = args
        .get("progress")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "log_project_review".into(),
            reason: "progress required".into(),
        })?;
    let next_steps = args
        .get("next_steps")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let override_days = args
        .get("next_review_in_days")
        .and_then(|v| v.as_i64());
    let db = open_db()?;
    if db
        .get_project(&uid, project_id)
        .map_err(|e| ToolError::Exec(format!("{e}")))?
        .is_none()
    {
        return Err(ToolError::Exec(format!(
            "project `{project_id}` not found"
        )));
    }
    let review = db
        .add_project_review(&uid, project_id, progress, next_steps, override_days)
        .map_err(|e| ToolError::Exec(format!("add project review: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({ "review_id": review.id, "project_id": project_id }),
        trace: Some(format!("logged review for {project_id}")),
    })
}

// ============================================================
// note tools
// ============================================================

/// Create a new note. Always extract the user's full intent into `body` —
/// don't summarise. `title` should be 4-15 chars capturing the gist; leave
/// empty if unsure. `tags` is comma-separated keywords. When the user is
/// clearly working within a specific project, pass its id as `project_id`.
#[harness::tool(
    name = "create_note",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "title":      {"type": "string", "description": "Short headline, ≤ 15 chars. Empty if unsure."},
            "body":       {"type": "string", "description": "The full note text from the user."},
            "tags":       {"type": "string", "description": "Comma-separated tags, optional."},
            "project_id": {"type": "string", "description": "Optional project id to attach this note to."}
        },
        "required": ["body"]
    }"#
)]
async fn create_note(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let body = args
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "create_note".into(),
            reason: "body required".into(),
        })?;
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let project_id = args.get("project_id").and_then(|v| v.as_str());
    let db = open_db()?;
    let note = db
        .create_note(&uid, project_id, title, body, &tags)
        .map_err(|e| ToolError::Exec(format!("insert note: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "id": note.id,
            "title": note.title,
            "tags": note.tags,
            "project_id": note.project_id,
            "embedding_status": "pending — search will use grep fallback until the worker fills it (~5s)"
        }),
        trace: Some(format!("created note {} ({} chars)", note.id, note.body.len())),
    })
}

/// Semantic search across the user's notes. Pass a natural-language query
/// (English or Chinese). Returns top_k notes ranked by cosine similarity, or
/// substring matches if embeddings aren't ready yet. Use this whenever the
/// user asks "did I write about X" / "关于 X 的笔记" / "find my note on Y".
#[harness::tool(
    name = "search_notes",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "The user's question/topic verbatim."},
            "top_k": {"type": "integer", "description": "Max results, default 8.", "minimum": 1, "maximum": 50}
        },
        "required": ["query"]
    }"#
)]
async fn search_notes(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let q = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "search_notes".into(),
            reason: "query required".into(),
        })?;
    let top_k = args
        .get("top_k")
        .and_then(|v| v.as_u64())
        .unwrap_or(8) as usize;
    let emb = embedder()?;
    let path = ledger_path();
    let hits = crate::search::semantic_search(&path, &uid, &emb, q, top_k, None)
        .await
        .map_err(|e| ToolError::Exec(format!("search: {e}")))?;
    Ok(ToolResult {
        ok: true,
        content: json!({
            "count": hits.len(),
            "hits": hits,
            "mode": if hits.iter().any(|h| h.via_grep) { "grep" } else { "semantic" }
        }),
        trace: Some(format!("search '{q}' → {} hits", hits.len())),
    })
}

/// List the user's notes by updated_at, optionally filtered by date range.
/// Use for time-scoped queries ("今天写了什么" / "notes from last week").
/// `since` and `until` are RFC3339 UTC timestamps; resolve relative dates by
/// calling `current_time` first.
#[harness::tool(
    name = "list_recent_notes",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "limit": {"type": "integer", "description": "Default 10, max 200.", "minimum": 1, "maximum": 200},
            "since": {"type": "string", "description": "RFC3339 UTC, inclusive lower bound on updated_at."},
            "until": {"type": "string", "description": "RFC3339 UTC, inclusive upper bound on updated_at."}
        }
    }"#
)]
async fn list_recent_notes(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .min(200) as u32;
    let since = args.get("since").and_then(|v| v.as_str());
    let until = args.get("until").and_then(|v| v.as_str());
    let db = open_db()?;
    let notes = if since.is_some() || until.is_some() {
        db.list_notes_in_range(&uid, None, since, until, limit)
            .map_err(|e| ToolError::Exec(format!("list: {e}")))?
    } else {
        db.list_recent_notes(&uid, None, limit)
            .map_err(|e| ToolError::Exec(format!("list: {e}")))?
    };
    Ok(ToolResult {
        ok: true,
        content: json!({
            "count": notes.len(),
            "notes": notes,
            "filter": { "since": since, "until": until }
        }),
        trace: None,
    })
}

/// Update an existing note's title / body / tags by id. Each field is optional;
/// only provided ones are changed. Embedding clears + re-pending on any touch.
/// Get the id first via search_notes / list_recent_notes.
#[harness::tool(
    name = "update_note",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "id":    {"type": "string"},
            "title": {"type": "string"},
            "body":  {"type": "string"},
            "tags":  {"type": "string", "description": "Comma-separated, optional."}
        },
        "required": ["id"]
    }"#
)]
async fn update_note(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "update_note".into(),
            reason: "id required".into(),
        })?;
    let title = args.get("title").and_then(|v| v.as_str());
    let body = args.get("body").and_then(|v| v.as_str());
    let tags: Option<Vec<String>> = args.get("tags").and_then(|v| v.as_str()).map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    });
    let db = open_db()?;
    let n = db
        .update_note(&uid, id, title, body, tags.as_deref())
        .map_err(|e| ToolError::Exec(format!("update: {e}")))?;
    if n == 0 {
        return Err(ToolError::Exec(format!("note `{id}` not found")));
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "id": id, "updated": n, "embedding_status": "re-pending" }),
        trace: None,
    })
}

/// Delete a note by id. Confirm with the user before calling — no soft-delete.
/// Get the id first via search_notes / list_recent_notes.
#[harness::tool(
    name = "delete_note",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "id": {"type": "string"}
        },
        "required": ["id"]
    }"#
)]
async fn delete_note(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let uid = uid_of(w)?;
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "delete_note".into(),
            reason: "id required".into(),
        })?;
    let db = open_db()?;
    let n = db
        .delete_note(&uid, id)
        .map_err(|e| ToolError::Exec(format!("delete: {e}")))?;
    if n == 0 {
        return Err(ToolError::Exec(format!("note `{id}` not found")));
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "deleted": id }),
        trace: None,
    })
}

/// Render a data-bound React page to the user. Does no server-side rendering —
/// it validates the request and acks; the client fetches the declared data and
/// renders `code` in a sandboxed iframe. The ChannelHook turns this call into
/// an `artifact` SSE event (see server.rs).
#[harness::tool(
    name = "render_artifact",
    risk = "read-only",
    schema = r#"{
      "type": "object",
      "properties": {
        "title": { "type": "string", "description": "Short title shown on the artifact card" },
        "data": {
          "type": "object",
          "properties": {
            "source": { "type": "string", "enum": ["project"], "description": "Data source; only 'project' is supported" },
            "id": { "type": "string", "description": "The project id to bind" }
          },
          "required": ["source", "id"]
        },
        "code": { "type": "string", "description": "ONE self-contained React component named App that reads window.DATA. No React import needed (automatic JSX runtime). You may import from 'recharts' and 'react'." }
      },
      "required": ["title", "data", "code"]
    }"#
)]
async fn render_artifact(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let code = args.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let data = args.get("data").cloned().unwrap_or(Value::Null);
    let source = data.get("source").and_then(|v| v.as_str()).unwrap_or("");
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");

    if source != "project" {
        return Err(ToolError::InvalidArgs {
            name: "render_artifact".into(),
            reason: format!("unsupported data source `{source}` (only `project` in Phase 1)"),
        });
    }
    if title.is_empty() || code.is_empty() || id.is_empty() {
        return Err(ToolError::InvalidArgs {
            name: "render_artifact".into(),
            reason: "title, code, and data.id are required".into(),
        });
    }
    let db = open_db()?;
    let uid = uid_of(w)?;
    if db
        .get_project(&uid, id)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .is_none()
    {
        return Err(ToolError::InvalidArgs {
            name: "render_artifact".into(),
            reason: format!("project `{id}` not found"),
        });
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "ok": true, "note": "artifact shown to the user" }),
        trace: None,
    })
}
