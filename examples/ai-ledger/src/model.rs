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
