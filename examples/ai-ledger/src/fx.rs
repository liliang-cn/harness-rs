//! FX rate fetcher backing the net-worth dashboard.
//!
//! We use Frankfurter's `/latest` endpoint — ECB official mid prices,
//! no API key, free. (exchangerate.host moved to key-gated tiers in
//! 2024.) Results land in the `fx_rates` SQLite table,
//! keyed by (base, quote, date). The snapshot cron and on-demand
//! conversions both read from that cache so we never block on the network
//! during a request.
//!
//! Convention: rates are stored as f64 — net-worth display is rounded
//! to two decimals anyway, and `f64` round-trips fine through SQLite TEXT
//! columns via `to_string` / `parse`.

use crate::db::Db;
use chrono::Utc;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Currencies we proactively cache every day. Anything outside this set
/// gets fetched on demand the first time someone has an account in it.
/// Order is fine to grow later — exchangerate.host charges nothing per
/// pair.
pub const TRACKED_CURRENCIES: &[&str] = &[
    "USD", "EUR", "GBP", "JPY", "CNY", "HKD", "SGD", "AUD", "CAD", "CHF", "KRW",
];

const SOURCE: &str = "frankfurter";
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Deserialize)]
struct Latest {
    rates: HashMap<String, f64>,
    #[serde(default)]
    date: Option<String>,
}

/// Convert `amount` from `from` to `to`, using today's cached rate. Falls
/// back to the most recent cached rate on or before today if today's
/// fetch hasn't happened yet. Same-currency conversion returns the input
/// unchanged. Returns None if no rate can be found.
pub fn convert(db: &Db, amount: f64, from: &str, to: &str) -> rusqlite::Result<Option<f64>> {
    if from.eq_ignore_ascii_case(to) {
        return Ok(Some(amount));
    }
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let rate = db.latest_fx_rate(from, to, &today)?;
    Ok(rate.map(|r| amount * r))
}

/// Pull today's rates for one base currency against every tracked quote
/// currency and persist them. Idempotent — safe to call mid-day or
/// multiple times. Used both at startup (warm cache) and by the daily
/// cron.
pub async fn refresh_for_base(db_path: &PathBuf, base: &str) -> anyhow::Result<usize> {
    // Frankfurter (ECB rates) — drops only the requested base from the
    // symbol list so we don't get back a redundant 1.0.
    let symbols: Vec<&&str> = TRACKED_CURRENCIES
        .iter()
        .filter(|c| !c.eq_ignore_ascii_case(base))
        .collect();
    let url = format!(
        "https://api.frankfurter.dev/v1/latest?base={}&symbols={}",
        base,
        symbols.iter().map(|s| **s).collect::<Vec<_>>().join(",")
    );
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()?;
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("exchangerate.host returned {status}");
    }
    let body: Latest = resp.json().await?;
    let date = body
        .date
        .unwrap_or_else(|| Utc::now().format("%Y-%m-%d").to_string());

    // Open a fresh DB connection — rusqlite Connection is not Send so we
    // can't share one across the tokio task boundary.
    let db = Db::open(db_path)?;
    let mut n = 0usize;
    for (quote, rate) in &body.rates {
        if quote.eq_ignore_ascii_case(base) {
            continue;
        }
        db.insert_fx_rate(base, quote, *rate, &date, SOURCE)?;
        n += 1;
    }
    Ok(n)
}

/// Background loop. Refreshes every tracked base currency on startup, then
/// once per ~6h afterwards (exchangerate.host updates daily; 6h gives 3-4
/// chances to catch the new day's rates after ECB publishes them).
pub fn spawn_refresher(db_path: PathBuf) {
    tokio::spawn(async move {
        // Initial warm-up. Don't fail the server if the network is down at
        // boot — we'll retry on the regular interval.
        for base in TRACKED_CURRENCIES {
            match refresh_for_base(&db_path, base).await {
                Ok(n) => tracing::info!(base = %base, rows = n, "fx warmed"),
                Err(e) => tracing::warn!(base = %base, err = %e, "fx warm-up failed"),
            }
        }
        let mut tick = tokio::time::interval(Duration::from_secs(6 * 3600));
        tick.tick().await; // skip the immediate first tick
        loop {
            tick.tick().await;
            for base in TRACKED_CURRENCIES {
                match refresh_for_base(&db_path, base).await {
                    Ok(n) => tracing::debug!(base = %base, rows = n, "fx refreshed"),
                    Err(e) => tracing::warn!(base = %base, err = %e, "fx refresh failed"),
                }
            }
        }
    });
}
