use crate::db::Db;
use crate::portfolio::model::{Asset, AssetClass, PriceQuote};
use chrono::Utc;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

/// Cache key for the global CNY-denominated physical gold price
/// (上海黄金交易所 Au9999, ¥/克). One row per process, refreshed via Gemini
/// grounding. See `fetch_cny_gold_cached`.
pub const CNY_GOLD_CACHE_KEY: &str = "cny_gold_au9999";

/// Default TTL for the CNY gold cache: 15 minutes. Override via
/// `HARNESS_GOLD_CACHE_TTL_SEC`. Physical gold moves slowly enough that a
/// 15-minute stale read is fine for a personal portfolio dashboard.
pub fn cny_gold_cache_ttl() -> Duration {
    std::env::var("HARNESS_GOLD_CACHE_TTL_SEC")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(900))
}

#[derive(Debug, thiserror::Error)]
pub enum QuoteError {
    #[error("network: {0}")]
    Network(String),
    #[error("parse: {0}")]
    Parse(String),
    #[error("no data for {0}")]
    NoData(String),
    #[error("unsupported asset class: {0}")]
    Unsupported(String),
}

pub fn make_client() -> reqwest::Client {
    // Yahoo Finance's WAF blocks unknown user-agents with 403. Use a recent
    // desktop Chrome UA — keeps both Yahoo and CoinGecko happy.
    reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .timeout(Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

pub async fn fetch_price(
    client: &reqwest::Client,
    asset: &Asset,
) -> Result<PriceQuote, QuoteError> {
    match asset.asset_class {
        AssetClass::Stock | AssetClass::Etf => {
            // Yahoo first — fast where reachable. Falls back to Tencent
            // (qt.gtimg.cn) for IPs Yahoo blocks (China + various JP/HK clouds).
            // Last-resort fallback is Gemini Search Grounding (cached) when
            // both structured sources fail — slower + costs a Google API call,
            // but unblocks IPs that get firewalled out of both Yahoo and
            // Tencent.
            match fetch_yahoo(client, asset).await {
                Ok(q) => Ok(q),
                Err(e_y) => match fetch_tencent_us(client, asset).await {
                    Ok(q) => Ok(q),
                    Err(e_t) => {
                        let path = crate::tools::ledger_path();
                        match fetch_via_gemini_cached(client, asset, &path, gemini_quote_ttl())
                            .await
                        {
                            Ok(q) => Ok(q),
                            Err(e_g) => Err(QuoteError::Network(format!(
                                "yahoo: {e_y}; tencent: {e_t}; gemini: {e_g}"
                            ))),
                        }
                    }
                },
            }
        }
        AssetClass::Commodity => {
            // CNY-denominated physical metal → Gemini Search Grounding, cached
            // globally in `quote_cache`. Eastmoney's push2 cluster blocks many
            // cloud IPs and intermittently 502s even from healthy ones, so we
            // route through Gemini for a reliable real-world quote.
            if asset.currency.eq_ignore_ascii_case("CNY") {
                let path = crate::tools::ledger_path();
                return fetch_cny_gold_cached(client, asset, &path, cny_gold_cache_ttl()).await;
            }
            // USD commodity (futures): Yahoo → Tencent hf_* → Gemini cached.
            match fetch_yahoo(client, asset).await {
                Ok(q) => Ok(q),
                Err(e_y) => match fetch_tencent_commodity(client, asset).await {
                    Ok(q) => Ok(q),
                    Err(e_t) => {
                        let path = crate::tools::ledger_path();
                        match fetch_via_gemini_cached(client, asset, &path, gemini_quote_ttl())
                            .await
                        {
                            Ok(q) => Ok(q),
                            Err(e_g) => Err(QuoteError::Network(format!(
                                "yahoo: {e_y}; tencent: {e_t}; gemini: {e_g}"
                            ))),
                        }
                    }
                },
            }
        }
        AssetClass::Crypto => fetch_coingecko(client, asset).await,
        AssetClass::Other => Err(QuoteError::Unsupported(asset.symbol.clone())),
    }
}

// ── Yahoo Finance (chart endpoint, no auth required) ──

#[derive(Deserialize)]
struct YahooChart {
    chart: YahooChartInner,
}
#[derive(Deserialize)]
struct YahooChartInner {
    result: Option<Vec<YahooResult>>,
    error: Option<YahooError>,
}
#[derive(Deserialize)]
struct YahooResult {
    meta: YahooMeta,
}
#[derive(Deserialize)]
struct YahooMeta {
    #[serde(rename = "regularMarketPrice")]
    regular_market_price: Option<f64>,
    currency: Option<String>,
}
#[derive(Deserialize)]
struct YahooError {
    description: String,
}

async fn fetch_yahoo(client: &reqwest::Client, asset: &Asset) -> Result<PriceQuote, QuoteError> {
    let symbol = &asset.symbol;
    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{}?interval=1d&range=1d",
        urlencode(symbol)
    );
    let body = client
        .get(&url)
        .send()
        .await
        .map_err(|e| QuoteError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| QuoteError::Network(e.to_string()))?
        .text()
        .await
        .map_err(|e| QuoteError::Network(e.to_string()))?;
    let parsed: YahooChart =
        serde_json::from_str(&body).map_err(|e| QuoteError::Parse(e.to_string()))?;
    if let Some(err) = parsed.chart.error {
        return Err(QuoteError::NoData(err.description));
    }
    let result = parsed
        .chart
        .result
        .as_ref()
        .and_then(|r| r.first())
        .ok_or_else(|| QuoteError::NoData(symbol.clone()))?;
    let price = result
        .meta
        .regular_market_price
        .ok_or_else(|| QuoteError::NoData(symbol.clone()))?;
    let currency = result
        .meta
        .currency
        .clone()
        .unwrap_or_else(|| asset.currency.clone());
    let price_dec = Decimal::from_str(&format!("{:.6}", price))
        .map_err(|e| QuoteError::Parse(e.to_string()))?;
    Ok(PriceQuote {
        asset_id: asset.id.clone(),
        price: price_dec,
        currency,
        fetched_at: Utc::now(),
        source: "yahoo".into(),
    })
}

// ── Tencent Finance (qt.gtimg.cn, China-friendly fallback for US stocks) ──
// Response format (GBK-encoded, but our needed fields are ASCII):
//   v_usAAPL="200~Apple~AAPL.OQ~298.97~297.84~296.97~42243561~..."
//                                  ^ index 3 = current price (USD)

async fn fetch_tencent_us(
    client: &reqwest::Client,
    asset: &Asset,
) -> Result<PriceQuote, QuoteError> {
    let url = format!("http://qt.gtimg.cn/q=us{}", urlencode(&asset.symbol));
    let bytes = client
        .get(&url)
        .send()
        .await
        .map_err(|e| QuoteError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| QuoteError::Network(e.to_string()))?
        .bytes()
        .await
        .map_err(|e| QuoteError::Network(e.to_string()))?;
    // Body is GBK; replace garbled bytes — we only need the ASCII price field.
    let body = String::from_utf8_lossy(&bytes);
    let eq = body
        .find('=')
        .ok_or_else(|| QuoteError::Parse("no `=` in tencent response".into()))?;
    let payload = body[eq + 1..]
        .trim()
        .trim_start_matches('"')
        .trim_end_matches(';')
        .trim_end_matches('"');
    if payload.is_empty() || payload == "1" {
        return Err(QuoteError::NoData(format!(
            "tencent has no data for us{}",
            asset.symbol
        )));
    }
    let parts: Vec<&str> = payload.split('~').collect();
    if parts.len() < 4 {
        return Err(QuoteError::Parse(format!(
            "tencent response has {} fields, need ≥4",
            parts.len()
        )));
    }
    let price_str = parts[3];
    let price = Decimal::from_str(price_str)
        .map_err(|e| QuoteError::Parse(format!("price `{price_str}`: {e}")))?;
    if price <= Decimal::ZERO {
        return Err(QuoteError::NoData(format!(
            "tencent returned zero/negative price for us{}",
            asset.symbol
        )));
    }
    Ok(PriceQuote {
        asset_id: asset.id.clone(),
        price,
        currency: asset.currency.clone(),
        fetched_at: Utc::now(),
        source: "tencent".into(),
    })
}

// ── Tencent commodity futures (qt.gtimg.cn hf_* endpoint) ──
// Format is comma-separated (not tilde):
//   v_hf_GC="4500.72,-0.23,4499.40,4499.60,4512.00,4455.00,20:27:06,..."
//            ^ index 0 = current price (USD/oz for COMEX futures)
// Symbol mapping: "GC=F" → "hf_GC", "SI=F" → "hf_SI" (strip =F suffix).
// If symbol doesn't have =F, use as-is and let it 404 if invalid.

async fn fetch_tencent_commodity(
    client: &reqwest::Client,
    asset: &Asset,
) -> Result<PriceQuote, QuoteError> {
    let base = asset.symbol.strip_suffix("=F").unwrap_or(&asset.symbol);
    let url = format!("http://qt.gtimg.cn/q=hf_{}", urlencode(base));
    let bytes = client
        .get(&url)
        .send()
        .await
        .map_err(|e| QuoteError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| QuoteError::Network(e.to_string()))?
        .bytes()
        .await
        .map_err(|e| QuoteError::Network(e.to_string()))?;
    let body = String::from_utf8_lossy(&bytes);
    let eq = body
        .find('=')
        .ok_or_else(|| QuoteError::Parse("no `=` in tencent commodity response".into()))?;
    let payload = body[eq + 1..]
        .trim()
        .trim_start_matches('"')
        .trim_end_matches(';')
        .trim_end_matches('"');
    if payload.is_empty() || payload == "1" {
        return Err(QuoteError::NoData(format!(
            "tencent has no commodity data for hf_{base}"
        )));
    }
    let parts: Vec<&str> = payload.split(',').collect();
    if parts.is_empty() {
        return Err(QuoteError::Parse("empty tencent commodity payload".into()));
    }
    let price_str = parts[0];
    let price = Decimal::from_str(price_str)
        .map_err(|e| QuoteError::Parse(format!("price `{price_str}`: {e}")))?;
    if price <= Decimal::ZERO {
        return Err(QuoteError::NoData(format!(
            "tencent zero price for hf_{base}"
        )));
    }
    Ok(PriceQuote {
        asset_id: asset.id.clone(),
        price,
        currency: asset.currency.clone(),
        fetched_at: Utc::now(),
        source: "tencent".into(),
    })
}

// ── CoinGecko (simple price endpoint, no auth) ──

async fn fetch_coingecko(
    client: &reqwest::Client,
    asset: &Asset,
) -> Result<PriceQuote, QuoteError> {
    let coin_id = asset.provider_id.as_deref().ok_or_else(|| {
        QuoteError::NoData(format!(
            "crypto {} missing provider_id (CoinGecko coin id)",
            asset.symbol
        ))
    })?;
    let vs = asset.currency.to_lowercase();
    let url = format!(
        "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies={}",
        urlencode(coin_id),
        urlencode(&vs)
    );
    let body = client
        .get(&url)
        .send()
        .await
        .map_err(|e| QuoteError::Network(e.to_string()))?
        .error_for_status()
        .map_err(|e| QuoteError::Network(e.to_string()))?
        .text()
        .await
        .map_err(|e| QuoteError::Network(e.to_string()))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| QuoteError::Parse(e.to_string()))?;
    let price = parsed
        .get(coin_id)
        .and_then(|o| o.get(&vs))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| QuoteError::NoData(format!("CoinGecko {coin_id}/{vs}")))?;
    let price_dec = Decimal::from_str(&format!("{:.10}", price))
        .map_err(|e| QuoteError::Parse(e.to_string()))?;
    Ok(PriceQuote {
        asset_id: asset.id.clone(),
        price: price_dec,
        currency: asset.currency.clone(),
        fetched_at: Utc::now(),
        source: "coingecko".into(),
    })
}

// ── CNY gold via Gemini Search Grounding (cached) ──

/// Cached read-through for ¥/克 of physical Au9999.
///
/// 1. Check `quote_cache[CNY_GOLD_CACHE_KEY]`. If present and younger than
///    `ttl`, return as a `PriceQuote` whose `source` is tagged
///    `gemini-cache:<age_sec>s`.
/// 2. Otherwise call `fetch_cny_gold_via_gemini`, write the result into
///    `quote_cache`, return it.
///
/// On cache-write failure we still return the fresh quote — the cache is an
/// optimisation, not a correctness requirement.
pub async fn fetch_cny_gold_cached(
    client: &reqwest::Client,
    asset: &Asset,
    db_path: &Path,
    ttl: Duration,
) -> Result<PriceQuote, QuoteError> {
    // Scope the SQLite connection: rusqlite's `Connection` is `!Send` (it
    // holds a `RefCell` statement cache), so it must be dropped BEFORE any
    // .await or the tokio multi-thread runtime refuses to schedule us.
    {
        let db = Db::open(db_path).map_err(|e| QuoteError::Network(format!("db open: {e}")))?;
        if let Ok(Some(c)) = db.get_cached_quote(CNY_GOLD_CACHE_KEY) {
            let age_sec = Utc::now().signed_duration_since(c.fetched_at).num_seconds();
            if age_sec >= 0 && (age_sec as u64) < ttl.as_secs() {
                return Ok(PriceQuote {
                    asset_id: asset.id.clone(),
                    price: c.price,
                    currency: c.currency,
                    fetched_at: c.fetched_at,
                    source: format!("{} (cache {}s)", c.source, age_sec),
                });
            }
        }
    }
    let fresh = fetch_cny_gold_via_gemini(client, asset).await?;
    {
        if let Ok(db) = Db::open(db_path) {
            let _ = db.put_cached_quote(
                CNY_GOLD_CACHE_KEY,
                fresh.price,
                &fresh.currency,
                &fresh.source,
                fresh.fetched_at,
            );
        }
    }
    Ok(fresh)
}

/// Returns the Gemini API key from env, preferring the dedicated
/// `GEMINI_API_KEY`. If unset, falls back to `HARNESS_API_KEY` *only when*
/// `HARNESS_MODEL_PROVIDER=gemini` — otherwise the harness key may be a
/// DeepSeek/OpenAI key and would 401 against Google.
fn gemini_api_key() -> Result<String, QuoteError> {
    if let Ok(k) = std::env::var("GEMINI_API_KEY")
        && !k.is_empty()
    {
        return Ok(k);
    }
    let provider = std::env::var("HARNESS_MODEL_PROVIDER")
        .unwrap_or_default()
        .to_lowercase();
    if provider == "gemini"
        && let Ok(k) = std::env::var("HARNESS_API_KEY")
        && !k.is_empty()
    {
        return Ok(k);
    }
    Err(QuoteError::Network(
        "set GEMINI_API_KEY (or HARNESS_API_KEY when HARNESS_MODEL_PROVIDER=gemini)".into(),
    ))
}

/// One-shot Google Gemini call with Search Grounding enabled. Returns the
/// parsed price plus the model id (so the caller can tag the source string).
///
/// The prompt MUST be tight: "answer with just a number, e.g. 998.20". Gemini
/// 3.x thinking is disabled (`thinkingBudget=0`) to keep latency ~1-2s and
/// cost ~6 output tokens per call — for "what's the spot price right now" we
/// don't need internal reasoning, just a fresh grounded number.
///
/// `sanity_min` / `sanity_max` reject obviously-wrong figures (e.g. model
/// returns USD/oz when asked CNY/g, or basis points instead of a price).
async fn gemini_grounded_price(
    client: &reqwest::Client,
    prompt: &str,
    sanity_min: Decimal,
    sanity_max: Decimal,
) -> Result<(Decimal, String), QuoteError> {
    let api_key = gemini_api_key()?;
    let model = std::env::var("HARNESS_QUOTE_MODEL")
        .or_else(|_| std::env::var("HARNESS_GOLD_MODEL"))
        .unwrap_or_else(|_| "gemini-3.5-flash".to_string());
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        urlencode(&model),
        urlencode(&api_key),
    );
    let body = serde_json::json!({
        "contents": [{ "parts": [{ "text": prompt }] }],
        "tools": [{"google_search": {}}],
        "generationConfig": {
            "temperature": 0.0,
            "thinkingConfig": {"thinkingBudget": 0},
            "maxOutputTokens": 512
        }
    });
    let resp_text = client
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| QuoteError::Network(format!("gemini POST: {e}")))?
        .error_for_status()
        .map_err(|e| QuoteError::Network(format!("gemini status: {e}")))?
        .text()
        .await
        .map_err(|e| QuoteError::Network(format!("gemini body: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| QuoteError::Parse(format!("gemini json: {e}")))?;
    let parts = v
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
        .ok_or_else(|| QuoteError::NoData("gemini: no candidates[0].content.parts".into()))?;
    let mut combined = String::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
            combined.push_str(t);
            combined.push(' ');
        }
    }
    let price = extract_first_decimal(&combined).ok_or_else(|| {
        QuoteError::Parse(format!("gemini returned no parseable number: {combined:?}"))
    })?;
    if price < sanity_min || price > sanity_max {
        return Err(QuoteError::Parse(format!(
            "gemini price {price} outside sanity range [{sanity_min}, {sanity_max}] — refusing"
        )));
    }
    Ok((price, model))
}

async fn fetch_cny_gold_via_gemini(
    client: &reqwest::Client,
    asset: &Asset,
) -> Result<PriceQuote, QuoteError> {
    let prompt = "今天上海黄金交易所 Au9999 现货黄金的最新人民币价格（元/克）是多少？\
                  只输出数字，比如 998.20。不要任何符号、单位、说明或解释。";
    // SGE Au9999 historically 200–2000 ¥/g; allow 200–5000 for headroom.
    let (price, model) =
        gemini_grounded_price(client, prompt, Decimal::new(200, 0), Decimal::new(5000, 0)).await?;
    Ok(PriceQuote {
        asset_id: asset.id.clone(),
        price,
        currency: asset.currency.clone(),
        fetched_at: Utc::now(),
        source: format!("gemini:{model}"),
    })
}

// ── Generic Gemini fallback (any asset) ──

fn gemini_asset_prompt(asset: &Asset) -> String {
    let cur = asset.currency.to_uppercase();
    match asset.asset_class {
        AssetClass::Stock | AssetClass::Etf => format!(
            "What is the current/last regular-market trade price of {} (ticker {}) \
             in {}? If markets are closed, give the most recent close. \
             Reply with ONLY a number, like 198.50. No currency symbol, no units, no explanation.",
            asset.name, asset.symbol, cur,
        ),
        AssetClass::Commodity => format!(
            "What is the current price of commodity {} ({}) in {} per standard contract unit \
             (USD/oz for COMEX metals like GC=F SI=F, USD/bbl for CL=F)? \
             Reply with ONLY a number like 4500.50. No currency symbol, no units, no explanation.",
            asset.name, asset.symbol, cur,
        ),
        AssetClass::Crypto => format!(
            "What is the current spot price of {} ({}) cryptocurrency in {}? \
             Reply with ONLY a number like 95000.50. No currency symbol, no units, no explanation.",
            asset.name, asset.symbol, cur,
        ),
        AssetClass::Other => format!(
            "What is the current market price of {} ({}) in {}? \
             Reply with ONLY a number. No currency symbol or explanation.",
            asset.name, asset.symbol, cur,
        ),
    }
}

/// Sanity range per asset class — same prompt could yield wildly different
/// magnitudes (BTC ~$100k vs penny stock ~$1), so each class gets its own
/// envelope. These are permissive — only catching obviously-broken values.
fn gemini_sanity_range(asset: &Asset) -> (Decimal, Decimal) {
    match asset.asset_class {
        AssetClass::Stock | AssetClass::Etf => (Decimal::new(1, 2), Decimal::new(1_000_000, 0)), // $0.01 – $1M
        AssetClass::Commodity => (Decimal::new(1, 0), Decimal::new(100_000, 0)), // 1 – 100k
        AssetClass::Crypto => (Decimal::new(1, 8), Decimal::new(10_000_000, 0)), // 0.00000001 – 10M
        AssetClass::Other => (Decimal::new(1, 4), Decimal::new(10_000_000, 0)),
    }
}

fn gemini_cache_key(asset: &Asset) -> String {
    format!(
        "gemini:{}:{}",
        asset.symbol.to_uppercase(),
        asset.currency.to_uppercase()
    )
}

/// Default TTL for Gemini-fallback stock/commodity quotes: 5 minutes. Override
/// via `HARNESS_GEMINI_QUOTE_TTL_SEC`. Shorter than gold (15 min) because
/// equities move more, but long enough to amortise repeat refreshes.
pub fn gemini_quote_ttl() -> Duration {
    std::env::var("HARNESS_GEMINI_QUOTE_TTL_SEC")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(300))
}

/// Generic cached read-through: check `quote_cache[gemini:<sym>:<cur>]`, fall
/// through to Gemini grounding on miss, write back on success. Same scoping
/// rules as `fetch_cny_gold_cached` — db connections never live across await.
pub async fn fetch_via_gemini_cached(
    client: &reqwest::Client,
    asset: &Asset,
    db_path: &Path,
    ttl: Duration,
) -> Result<PriceQuote, QuoteError> {
    let key = gemini_cache_key(asset);
    {
        let db = Db::open(db_path).map_err(|e| QuoteError::Network(format!("db open: {e}")))?;
        if let Ok(Some(c)) = db.get_cached_quote(&key) {
            let age_sec = Utc::now().signed_duration_since(c.fetched_at).num_seconds();
            if age_sec >= 0 && (age_sec as u64) < ttl.as_secs() {
                return Ok(PriceQuote {
                    asset_id: asset.id.clone(),
                    price: c.price,
                    currency: c.currency,
                    fetched_at: c.fetched_at,
                    source: format!("{} (cache {}s)", c.source, age_sec),
                });
            }
        }
    }
    let prompt = gemini_asset_prompt(asset);
    let (min, max) = gemini_sanity_range(asset);
    let (price, model) = gemini_grounded_price(client, &prompt, min, max).await?;
    let fresh = PriceQuote {
        asset_id: asset.id.clone(),
        price,
        currency: asset.currency.clone(),
        fetched_at: Utc::now(),
        source: format!("gemini:{model}"),
    };
    {
        if let Ok(db) = Db::open(db_path) {
            let _ = db.put_cached_quote(
                &key,
                fresh.price,
                &fresh.currency,
                &fresh.source,
                fresh.fetched_at,
            );
        }
    }
    Ok(fresh)
}

/// Find the first decimal token (digits with an optional single `.`) in `s`.
/// Tolerates leading currency symbols, Chinese characters, whitespace.
fn extract_first_decimal(s: &str) -> Option<Decimal> {
    let mut start: Option<usize> = None;
    let mut end: usize = 0;
    let mut saw_dot = false;
    for (i, ch) in s.char_indices() {
        let is_digit = ch.is_ascii_digit();
        let is_dot = ch == '.';
        if is_digit || (is_dot && start.is_some() && !saw_dot) {
            if start.is_none() {
                start = Some(i);
            }
            if is_dot {
                saw_dot = true;
            }
            end = i + ch.len_utf8();
        } else if start.is_some() {
            break;
        }
    }
    let s_ = start?;
    let raw = s[s_..end].trim_end_matches('.');
    Decimal::from_str(raw).ok().filter(|d| *d > Decimal::ZERO)
}

// Minimal URL-component encoder (path segment / query value).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
