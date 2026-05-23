//! Per-model token pricing in USD per million tokens. Used by
//! /api/admin/users to surface an estimated `cost_usd` per user.
//!
//! Numbers are public list prices (rough mid-2025) for the chat models
//! the v1 deployment supports. Update when DeepSeek / Gemini revise their
//! pricing; for unknown models we fall back to the v4-flash rate so the
//! number is never wildly off.
//!
//! Embedding tokens are NOT included — `gemini-embedding-001` is free
//! under 1500 RPM, which our embed worker stays well below.

/// (input_per_million_usd, output_per_million_usd)
type Rate = (f64, f64);

const DEFAULT_RATE: Rate = (0.10, 0.60); // fallback ≈ deepseek-v4-flash

fn rate_for(model_id: &str) -> Rate {
    match model_id {
        // DeepSeek
        "deepseek-v4-flash" => (0.10, 0.60),
        "deepseek-v4-pro"   => (0.55, 2.20),
        // Gemini chat (in case we wire it as the chat model later)
        "gemini-3.5-flash"  => (0.075, 0.30),
        "gemini-3.5-pro"    => (1.25, 5.00),
        _ => DEFAULT_RATE,
    }
}

/// Total USD cost given input + output token counts under `model_id`'s
/// price card. Returns 0 cleanly if both counts are zero.
pub fn cost_usd(model_id: &str, tokens_in: i64, tokens_out: i64) -> f64 {
    if tokens_in <= 0 && tokens_out <= 0 {
        return 0.0;
    }
    let (in_rate, out_rate) = rate_for(model_id);
    let i = tokens_in.max(0) as f64;
    let o = tokens_out.max(0) as f64;
    (i * in_rate + o * out_rate) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_in_zero_out() {
        assert_eq!(cost_usd("deepseek-v4-flash", 0, 0), 0.0);
    }

    #[test]
    fn deepseek_flash_6k_in_500_out() {
        // (6000 * 0.10 + 500 * 0.60) / 1_000_000 = 900 / 1_000_000 = 0.0009
        let c = cost_usd("deepseek-v4-flash", 6000, 500);
        assert!((c - 0.0009).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_falls_back_to_default() {
        // 1M in × 0.10 + 1M out × 0.60 = 0.70
        let c = cost_usd("totally-fake-model", 1_000_000, 1_000_000);
        assert!((c - 0.70).abs() < 1e-6);
    }
}
