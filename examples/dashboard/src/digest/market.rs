//! Shared per-UTC-day market brief: gold / bitcoin / a stock index, each with
//! a current level + one-line Chinese trend conclusion, plus a short overall
//! summary. Generated once per UTC day via Gemini grounding and cached in the
//! `daily_market_brief` table so every user's digest that day reuses it.

use crate::db::Db;
use crate::digest::model::{MarketBrief, Quote};
use chrono::Utc;

/// Per-tick holder so we generate the shared brief at most once per cron tick.
pub struct MarketBriefCacheState {
    pub brief: Option<MarketBrief>,
}

/// The Chinese grounding prompt. We ask for a strict pipe-delimited format so
/// the response is trivially parseable (Gemini can't combine grounding with
/// JSON-schema output in one call).
pub const MARKET_PROMPT: &str = "\
用 Google 搜索查到当前最新行情，然后严格按下面 4 行格式输出，不要任何多余文字、不要 Markdown：\n\
黄金|<伦敦金现货美元/盎司价格数字>|<一句话中文结论>\n\
比特币|<美元价格数字>|<一句话中文结论>\n\
纳斯达克|<纳斯达克综合指数点位数字>|<一句话中文结论>\n\
总结|<1到2句中文综合点评>\n\
示例：黄金|2360.5|金价小幅走高，避险情绪升温。";

/// Parse the 4-line pipe format into a `MarketBrief`. Returns `None` if any of
/// the four expected lines is missing.
pub fn parse_market_response(text: &str) -> Option<MarketBrief> {
    let mut gold = None;
    let mut btc = None;
    let mut index = None;
    let mut summary = None;
    for line in text.lines() {
        let line = line.trim();
        let mut cols = line.splitn(3, '|');
        let tag = cols.next().unwrap_or("").trim();
        match tag {
            "黄金" => {
                let price = cols.next().unwrap_or("").trim().to_string();
                let concl = cols.next().unwrap_or("").trim().to_string();
                if !price.is_empty() {
                    gold = Some(Quote {
                        name: "黄金".into(),
                        price,
                        conclusion: concl,
                    });
                }
            }
            "比特币" => {
                let price = cols.next().unwrap_or("").trim().to_string();
                let concl = cols.next().unwrap_or("").trim().to_string();
                if !price.is_empty() {
                    btc = Some(Quote {
                        name: "比特币".into(),
                        price,
                        conclusion: concl,
                    });
                }
            }
            "纳斯达克" => {
                let price = cols.next().unwrap_or("").trim().to_string();
                let concl = cols.next().unwrap_or("").trim().to_string();
                if !price.is_empty() {
                    index = Some(Quote {
                        name: "纳斯达克".into(),
                        price,
                        conclusion: concl,
                    });
                }
            }
            "总结" => {
                let s = cols.next().unwrap_or("").trim().to_string();
                if !s.is_empty() {
                    summary = Some(s);
                }
            }
            _ => {}
        }
    }
    Some(MarketBrief {
        gold: gold?,
        btc: btc?,
        index: index?,
        summary: summary?,
    })
}

/// Return today's (UTC) market brief, generating + caching it on first call.
/// On generation/parse failure, logs a WARN and returns `None` (the digest
/// still sends without the market section).
pub async fn ensure_market_brief(db: &Db, client: &reqwest::Client) -> Option<MarketBrief> {
    let day = Utc::now().format("%Y-%m-%d").to_string();
    if let Ok(Some(v)) = db.get_market_brief(&day)
        && let Ok(b) = serde_json::from_value::<MarketBrief>(v)
    {
        return Some(b);
    }
    match generate_market_brief(client).await {
        Some(brief) => {
            if let Ok(v) = serde_json::to_value(&brief) {
                let _ = db.put_market_brief(&day, &v);
            }
            Some(brief)
        }
        None => {
            tracing::warn!(day = %day, "market brief generation failed; digest will omit market section");
            None
        }
    }
}

/// One grounded Gemini call → parsed `MarketBrief`. Mirrors the request shape
/// in `portfolio/quotes.rs::gemini_grounded_price` (google_search tool,
/// thinkingBudget 0). Returns `None` on any network/parse error.
async fn generate_market_brief(client: &reqwest::Client) -> Option<MarketBrief> {
    let api_key = std::env::var("GEMINI_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())?;
    let model =
        std::env::var("HARNESS_QUOTE_MODEL").unwrap_or_else(|_| "gemini-3.5-flash".to_string());
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        urlencoding(&model),
        urlencoding(&api_key),
    );
    let body = serde_json::json!({
        "contents": [{ "parts": [{ "text": MARKET_PROMPT }] }],
        "tools": [{"google_search": {}}],
        "generationConfig": {
            "temperature": 0.0,
            "thinkingConfig": {"thinkingBudget": 0},
            "maxOutputTokens": 1024
        }
    });
    let resp = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(40))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&resp).ok()?;
    let parts = v.pointer("/candidates/0/content/parts")?.as_array()?;
    let mut combined = String::new();
    for p in parts {
        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
            combined.push_str(t);
            combined.push('\n');
        }
    }
    parse_market_response(&combined)
}

/// Minimal percent-encoder for URL path/query segments (avoids pulling a new
/// dep; the model id + key are already URL-safe in practice but we guard `/`).
fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_response() {
        let text = "黄金|2360.5|金价小幅走高，避险升温。\n比特币|67000|震荡回落。\n纳斯达克|17500|科技股领涨。\n总结|风险偏好整体回暖。";
        let b = parse_market_response(text).unwrap();
        assert_eq!(b.gold.price, "2360.5");
        assert_eq!(b.gold.conclusion, "金价小幅走高，避险升温。");
        assert_eq!(b.btc.price, "67000");
        assert_eq!(b.index.name, "纳斯达克");
        assert_eq!(b.summary, "风险偏好整体回暖。");
    }

    #[test]
    fn missing_line_returns_none() {
        let text = "黄金|2360.5|金价走高\n比特币|67000|回落";
        assert!(parse_market_response(text).is_none());
    }

    #[test]
    fn tolerates_blank_and_extra_lines() {
        let text =
            "\n这是模型的废话\n黄金|2360|稳\n比特币|67000|稳\n纳斯达克|17500|稳\n总结|稳。\n再见";
        assert!(parse_market_response(text).is_some());
    }
}
