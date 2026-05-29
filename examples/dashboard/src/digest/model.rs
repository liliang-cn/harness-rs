//! The serializable digest payload. Stored as `notifications.body` JSON and
//! rendered to email HTML.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Digest {
    pub date: String, // user-local date the digest covers (the "yesterday")
    pub spending: SpendingSection,
    pub wealth: WealthSection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market: Option<MarketBrief>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendingSection {
    pub total: f64,
    pub currency: String,
    /// (category, amount), highest first, capped to a handful.
    pub by_category: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WealthSection {
    pub net_worth: f64,
    pub net_delta: f64,
    pub cash: f64,
    pub investments: f64,
    pub investments_delta: f64,
    pub debt: f64,
    pub currency: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketBrief {
    pub gold: Quote,
    pub btc: Quote,
    pub index: Quote,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Quote {
    pub name: String,
    pub price: String,
    pub conclusion: String,
}
