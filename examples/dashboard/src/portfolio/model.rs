use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AssetClass {
    Stock,
    Etf,
    Commodity,
    Crypto,
    Other,
}

impl AssetClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            AssetClass::Stock => "stock",
            AssetClass::Etf => "etf",
            AssetClass::Commodity => "commodity",
            AssetClass::Crypto => "crypto",
            AssetClass::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_lowercase().as_str() {
            "stock" => Self::Stock,
            "etf" => Self::Etf,
            "commodity" => Self::Commodity,
            "crypto" => Self::Crypto,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asset {
    pub id: String,
    /// User-facing ticker, e.g. AAPL, BTC, GC=F (gold futures).
    pub symbol: String,
    pub name: String,
    pub asset_class: AssetClass,
    /// Optional provider-specific id (CoinGecko coin id like "bitcoin").
    /// For Yahoo Finance the `symbol` itself is already the provider id.
    pub provider_id: Option<String>,
    pub currency: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TradeKind {
    Buy,
    Sell,
    /// Pre-existing holding entered as a baseline — "I already owned 100 AAPL
    /// before I started using this app." Counts toward qty and cost basis like
    /// a buy, but does NOT count as a recent trade and never contributes to
    /// realized P/L on its own.
    Opening,
}

impl TradeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TradeKind::Buy => "buy",
            TradeKind::Sell => "sell",
            TradeKind::Opening => "opening",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_lowercase().as_str() {
            "buy" => Self::Buy,
            "sell" => Self::Sell,
            "opening" => Self::Opening,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub id: String,
    pub asset_id: String,
    pub kind: TradeKind,
    pub qty: Decimal,
    pub price_per_unit: Decimal,
    pub currency: String,
    pub fees: Decimal,
    pub occurred_at: DateTime<Utc>,
    pub note: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceQuote {
    pub asset_id: String,
    pub price: Decimal,
    pub currency: String,
    pub fetched_at: DateTime<Utc>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub asset_id: String,
    pub symbol: String,
    pub name: String,
    pub asset_class: AssetClass,
    pub currency: String,
    /// Net qty held (buys − sells).
    pub qty: Decimal,
    /// Average buy cost per unit including pro-rata fees on buys.
    pub avg_cost: Decimal,
    /// Total realized P/L on closed (sold) portion.
    pub realized_pl: Decimal,
    /// Latest known unit price (or None if none recorded).
    pub last_price: Option<Decimal>,
    pub last_price_at: Option<DateTime<Utc>>,
    pub last_price_source: Option<String>,
    /// qty × last_price (None if last_price unknown).
    pub market_value: Option<Decimal>,
    /// (last_price − avg_cost) × qty (None if last_price unknown).
    pub unrealized_pl: Option<Decimal>,
}

/// Build a list of Positions from raw assets + trades + a price lookup callback.
/// The callback returns the latest PriceQuote for an asset_id, or None if none
/// is cached. Pure function: callers wire in either DB-backed or live lookups.
pub fn build_positions(
    assets: &[Asset],
    trades: &[Trade],
    latest_for: impl Fn(&str) -> Option<PriceQuote>,
) -> Vec<Position> {
    assets
        .iter()
        .map(|a| {
            let (qty, avg_cost, realized) = aggregate_trades(&a.id, trades);
            let price = latest_for(&a.id);
            let last_price = price.as_ref().map(|p| p.price);
            let last_price_at = price.as_ref().map(|p| p.fetched_at);
            let last_price_source = price.as_ref().map(|p| p.source.clone());
            let market_value = last_price.map(|p| p * qty);
            let unrealized_pl = last_price.map(|p| (p - avg_cost) * qty);
            Position {
                asset_id: a.id.clone(),
                symbol: a.symbol.clone(),
                name: a.name.clone(),
                asset_class: a.asset_class,
                currency: a.currency.clone(),
                qty,
                avg_cost,
                realized_pl: realized,
                last_price,
                last_price_at,
                last_price_source,
                market_value,
                unrealized_pl,
            }
        })
        .collect()
}

/// Reduce a list of trades for a single asset into a Position-style summary.
/// Uses average-cost basis on buys for `avg_cost`; realized P/L uses (sell_price
/// − running_avg_cost) × sell_qty − sell_fees and clamps avg_cost when fully
/// sold then re-bought.
pub fn aggregate_trades(asset_id: &str, trades: &[Trade]) -> (Decimal, Decimal, Decimal) {
    let mut net_qty = Decimal::ZERO;
    let mut total_cost = Decimal::ZERO;
    let mut realized = Decimal::ZERO;
    let mut sorted: Vec<&Trade> = trades.iter().filter(|t| t.asset_id == asset_id).collect();
    sorted.sort_by_key(|t| t.occurred_at);
    for t in sorted {
        match t.kind {
            TradeKind::Buy | TradeKind::Opening => {
                let added_cost = t.qty * t.price_per_unit + t.fees;
                total_cost += added_cost;
                net_qty += t.qty;
            }
            TradeKind::Sell => {
                let sell_qty = t.qty.min(net_qty);
                if net_qty > Decimal::ZERO && sell_qty > Decimal::ZERO {
                    let avg = total_cost / net_qty;
                    let cost_out = avg * sell_qty;
                    realized += sell_qty * t.price_per_unit - cost_out - t.fees;
                    total_cost -= cost_out;
                    net_qty -= sell_qty;
                    if net_qty <= Decimal::ZERO {
                        net_qty = Decimal::ZERO;
                        total_cost = Decimal::ZERO;
                    }
                } else {
                    // sell-without-position (short) — treat as realized at -fees,
                    // do not let qty go negative for spine simplicity
                    realized -= t.fees;
                }
            }
        }
    }
    let avg_cost = if net_qty > Decimal::ZERO {
        total_cost / net_qty
    } else {
        Decimal::ZERO
    };
    (net_qty, avg_cost, realized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::str::FromStr;

    fn t(asset: &str, kind: TradeKind, qty: &str, price: &str, fees: &str, day: u32) -> Trade {
        Trade {
            id: format!("{}-{}-{}", asset, kind.as_str(), day),
            asset_id: asset.into(),
            kind,
            qty: Decimal::from_str(qty).unwrap(),
            price_per_unit: Decimal::from_str(price).unwrap(),
            currency: "USD".into(),
            fees: Decimal::from_str(fees).unwrap(),
            occurred_at: Utc.with_ymd_and_hms(2026, 5, day, 12, 0, 0).unwrap(),
            note: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn opening_baseline_counts_into_avg_cost_not_realized() {
        let trades = vec![
            // Pre-existing holding entered as baseline.
            t("AAPL", TradeKind::Opening, "100", "150", "0", 1),
            // Real subsequent trade.
            t("AAPL", TradeKind::Sell, "30", "200", "0", 5),
        ];
        let (qty, avg, realized) = aggregate_trades("AAPL", &trades);
        assert_eq!(qty, Decimal::from(70));
        // avg_cost from the opening baseline @ 150
        assert_eq!(avg, Decimal::from(150));
        // realized = (200 - 150) * 30 = 1500
        assert_eq!(realized, Decimal::from(1500));
    }

    #[test]
    fn fifo_avg_cost_buy_only() {
        let trades = vec![
            t("AAPL", TradeKind::Buy, "100", "300", "1", 1),
            t("AAPL", TradeKind::Buy, "100", "400", "1", 2),
        ];
        let (qty, avg, realized) = aggregate_trades("AAPL", &trades);
        assert_eq!(qty, Decimal::from(200));
        // (100*300+1 + 100*400+1) / 200 = (30001+40001)/200 = 70002/200 = 350.01
        assert_eq!(avg, Decimal::from_str("350.01").unwrap());
        assert_eq!(realized, Decimal::ZERO);
    }

    #[test]
    fn realized_pl_on_partial_sell() {
        let trades = vec![
            t("AAPL", TradeKind::Buy, "100", "300", "0", 1),
            t("AAPL", TradeKind::Sell, "40", "400", "0", 5),
        ];
        let (qty, avg, realized) = aggregate_trades("AAPL", &trades);
        assert_eq!(qty, Decimal::from(60));
        // After selling 40 at avg 300, avg cost stays 300 on remaining 60
        assert_eq!(avg, Decimal::from_str("300").unwrap());
        // realized = (400-300)*40 = 4000
        assert_eq!(realized, Decimal::from(4000));
    }

    #[test]
    fn full_close_then_buy_resets_avg_cost() {
        let trades = vec![
            t("AAPL", TradeKind::Buy, "100", "300", "0", 1),
            t("AAPL", TradeKind::Sell, "100", "400", "0", 5),
            t("AAPL", TradeKind::Buy, "50", "350", "0", 10),
        ];
        let (qty, avg, realized) = aggregate_trades("AAPL", &trades);
        assert_eq!(qty, Decimal::from(50));
        assert_eq!(avg, Decimal::from(350));
        // 100 shares bought at 300, sold at 400 = realized 10000
        assert_eq!(realized, Decimal::from(10000));
    }
}
