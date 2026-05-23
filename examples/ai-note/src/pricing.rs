//! Per-model token pricing in USD per million tokens. Used by
//! /api/admin/users to surface an estimated `cost_usd` per user.
//!
//! The rate card lives in `provider_config` (key `pricing_rate_card`,
//! JSON value) so the operator can tweak rates from the admin UI
//! without a redeploy. `default_rate_card()` seeds reasonable mid-2025
//! list prices on first launch; for unknown models we fall back to a
//! sensible default so the number is never wildly off.
//!
//! Embedding tokens are NOT included — `gemini-embedding-001` is free
//! under 1500 RPM, which our embed worker stays well below.

use serde::{Deserialize, Serialize};

/// USD per million tokens for one model.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelRate {
    pub input: f64,
    pub output: f64,
}

/// Mapping model_id → rate. JSON shape: `{"deepseek-v4-flash":{"input":0.10,"output":0.60},...}`.
pub type RateCard = std::collections::HashMap<String, ModelRate>;

/// Fallback when the rate card lacks the requested model — roughly
/// equivalent to deepseek-v4-flash so the number is never wildly off.
pub const FALLBACK_RATE: ModelRate = ModelRate { input: 0.10, output: 0.60 };

/// Default rate card seeded on first launch. Admin can edit afterwards
/// via PATCH /api/admin/config.
pub fn default_rate_card() -> RateCard {
    let mut m = RateCard::new();
    m.insert("deepseek-v4-flash".into(), ModelRate { input: 0.10,  output: 0.60 });
    m.insert("deepseek-v4-pro".into(),   ModelRate { input: 0.55,  output: 2.20 });
    m.insert("gemini-3.5-flash".into(),  ModelRate { input: 0.075, output: 0.30 });
    m.insert("gemini-3.5-pro".into(),    ModelRate { input: 1.25,  output: 5.00 });
    m
}

/// Total USD cost given input + output token counts under `model_id`'s
/// rate card. Returns 0 cleanly if both counts are zero. Unknown models
/// fall back to `FALLBACK_RATE`.
pub fn cost_usd(rates: &RateCard, model_id: &str, tokens_in: i64, tokens_out: i64) -> f64 {
    if tokens_in <= 0 && tokens_out <= 0 {
        return 0.0;
    }
    let rate = rates.get(model_id).copied().unwrap_or(FALLBACK_RATE);
    let i = tokens_in.max(0) as f64;
    let o = tokens_out.max(0) as f64;
    (i * rate.input + o * rate.output) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_in_zero_out() {
        assert_eq!(cost_usd(&default_rate_card(), "deepseek-v4-flash", 0, 0), 0.0);
    }

    #[test]
    fn deepseek_flash_6k_in_500_out() {
        // (6000 * 0.10 + 500 * 0.60) / 1_000_000 = 900 / 1_000_000 = 0.0009
        let c = cost_usd(&default_rate_card(), "deepseek-v4-flash", 6000, 500);
        assert!((c - 0.0009).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_falls_back() {
        // 1M in × 0.10 + 1M out × 0.60 = 0.70
        let c = cost_usd(&default_rate_card(), "totally-fake-model", 1_000_000, 1_000_000);
        assert!((c - 0.70).abs() < 1e-6);
    }

    #[test]
    fn empty_card_still_falls_back() {
        let empty = RateCard::new();
        let c = cost_usd(&empty, "any-model", 1_000_000, 0);
        assert!((c - 0.10).abs() < 1e-9);
    }

    #[test]
    fn round_trip_json() {
        let card = default_rate_card();
        let json = serde_json::to_string(&card).unwrap();
        let back: RateCard = serde_json::from_str(&json).unwrap();
        assert_eq!(card, back);
    }
}
