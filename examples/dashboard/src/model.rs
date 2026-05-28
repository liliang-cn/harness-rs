use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    Cash,
    Debit,
    Credit,
    Wallet,
    Loan,        // you owe — car loan, personal loan
    Mortgage,    // you owe — has amortization
    Receivable,  // someone owes you (lent to friend, pending refund)
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub kind: AccountKind,
    pub currency: String,
    pub opening_balance: Decimal,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TxnKind {
    Expense,
    Income,
    Transfer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    pub id: String,
    pub kind: TxnKind,
    pub amount: Decimal,
    pub currency: String,
    pub account_id: String,
    pub counter_account_id: Option<String>,
    pub category: Option<String>,
    pub note: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub category: String,
    pub currency: String,
    pub monthly_limit: Decimal,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryTotal {
    pub category: String,
    pub currency: String,
    pub total: Decimal,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetStatus {
    pub category: String,
    pub currency: String,
    pub limit: Decimal,
    pub used: Decimal,
    pub remaining: Decimal,
    pub over_budget: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Frequency {
    Weekly,
    Monthly,
    Quarterly,
    Yearly,
}

impl Frequency {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "weekly" | "week" => Some(Self::Weekly),
            "monthly" | "month" => Some(Self::Monthly),
            "quarterly" | "quarter" => Some(Self::Quarterly),
            "yearly" | "annual" | "year" => Some(Self::Yearly),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
            Self::Quarterly => "quarterly",
            Self::Yearly => "yearly",
        }
    }
    /// Advance a YYYY-MM-DD date by one period. Month/quarter/year math
    /// clamps to the last day of the target month so 2026-01-31 → monthly →
    /// 2026-02-28 (not "2026-03-03" or panic).
    pub fn advance(&self, from: chrono::NaiveDate) -> chrono::NaiveDate {
        use chrono::Datelike;
        match self {
            Self::Weekly => from + chrono::Duration::days(7),
            Self::Monthly => add_months(from, 1),
            Self::Quarterly => add_months(from, 3),
            Self::Yearly => {
                let next = from.with_year(from.year() + 1);
                next.unwrap_or_else(|| add_months(from, 12))
            }
        }
    }
}

fn add_months(from: chrono::NaiveDate, months: u32) -> chrono::NaiveDate {
    use chrono::Datelike;
    let mut y = from.year();
    let mut m = from.month() + months;
    while m > 12 {
        y += 1;
        m -= 12;
    }
    // Clamp day to end-of-month if target month is shorter (e.g. Jan 31 → Feb 28).
    let last = last_day_of_month(y, m);
    let d = from.day().min(last);
    chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap_or(from)
}

fn last_day_of_month(year: i32, month: u32) -> u32 {
    let next_first = if month == 12 {
        chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    let last = next_first
        .and_then(|d| d.pred_opt())
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(year, month, 28).unwrap());
    use chrono::Datelike;
    last.day()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSession {
    pub id: String,
    pub title: Option<String>,
    pub model_id: Option<String>,
    pub message_count: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: String,
    pub session_id: String,
    /// "user" | "asst"
    pub role: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iters: Option<u32>,
    pub created_at: DateTime<Utc>,
    /// Attachment ids the user uploaded when sending this message. Empty
    /// for assistant turns and for messages predating the feature.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<String>,
    /// Artifacts the assistant emitted in this turn (render_artifact tool
    /// args). Empty for turns without artifacts. Stored so the chat
    /// re-hydrates artifact cards on reload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub name: String,
    pub amount: Decimal,
    pub currency: String,
    pub frequency: Frequency,
    /// YYYY-MM-DD of the next expected charge.
    pub next_charge_date: chrono::NaiveDate,
    pub account_id: String,
    pub category: Option<String>,
    /// Free-form, e.g. "Android/Google Play", "AmEx ****1234".
    pub pay_channel: Option<String>,
    pub note: Option<String>,
    /// "active" | "cancelled".
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub cancelled_at: Option<DateTime<Utc>>,
}
