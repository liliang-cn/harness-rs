//! Two delivery adapters for a `Digest`:
//!   - `deliver_in_app` inserts a `notifications` row.
//!   - `deliver_email` renders HTML and POSTs to Resend.
//! Channel selection ("in_app" | "email" | "both") is the caller's job.

use crate::db::Db;
use crate::digest::model::Digest;

/// Localized (Chinese) email subject for a digest covering `date`.
pub fn email_subject(d: &Digest) -> String {
    format!("今日简报 · {}", d.date)
}

/// Render the digest to a simple inline-styled HTML email body.
pub fn render_email_html(d: &Digest) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "<div style=\"font-family:-apple-system,Segoe UI,Roboto,sans-serif;max-width:560px;margin:0 auto;color:#1a1a1a\">\
         <h2 style=\"margin:0 0 4px\">今日简报</h2>\
         <div style=\"color:#888;font-size:13px;margin-bottom:16px\">{}</div>",
        d.date
    ));
    s.push_str(&format!(
        "<h3 style=\"margin:16px 0 6px\">昨日支出</h3>\
         <div style=\"font-size:20px;font-weight:600\">{:.2} {}</div>",
        d.spending.total, d.spending.currency
    ));
    if !d.spending.by_category.is_empty() {
        s.push_str("<ul style=\"margin:6px 0;padding-left:18px;color:#444\">");
        for (cat, amt) in &d.spending.by_category {
            s.push_str(&format!("<li>{cat}: {amt:.2}</li>"));
        }
        s.push_str("</ul>");
    }
    let w = &d.wealth;
    s.push_str(&format!(
        "<h3 style=\"margin:16px 0 6px\">资产</h3>\
         <div>净值 {:.2} {} （较前一日 {:+.2}）</div>\
         <div style=\"color:#444;font-size:14px\">现金 {:.2} · 投资 {:.2}（{:+.2}）· 负债 {:.2}</div>",
        w.net_worth, w.currency, w.net_delta, w.cash, w.investments, w.investments_delta, w.debt
    ));
    if let Some(m) = &d.market {
        s.push_str("<h3 style=\"margin:16px 0 6px\">市场</h3><ul style=\"margin:6px 0;padding-left:18px;color:#444\">");
        for q in [&m.gold, &m.btc, &m.index] {
            s.push_str(&format!("<li><b>{}</b> {} — {}</li>", q.name, q.price, q.conclusion));
        }
        s.push_str("</ul>");
        s.push_str(&format!("<p style=\"color:#444\">{}</p>", m.summary));
    }
    s.push_str("<hr style=\"border:none;border-top:1px solid #eee;margin:20px 0\">\
                <div style=\"color:#aaa;font-size:12px\">来自 Dashboard · 可在「我的 → 每日简报」中关闭</div></div>");
    s
}

/// Build the JSON body for Resend's POST /emails. Pure — no network.
pub fn resend_body(from: &str, to: &str, subject: &str, html: &str) -> serde_json::Value {
    serde_json::json!({
        "from": from,
        "to": [to],
        "subject": subject,
        "html": html,
    })
}

/// Insert the digest as an in-app notification row.
pub fn deliver_in_app(db: &Db, user_id: &str, digest: &Digest) -> anyhow::Result<()> {
    let body = serde_json::to_value(digest)?;
    db.insert_notification(user_id, "digest", "今日简报", &body)?;
    Ok(())
}

/// Send the digest by email via Resend. Reads `RESEND_API_KEY` + `DIGEST_FROM`
/// from env. If the key is unset, logs a WARN and returns Ok (email skipped —
/// in-app unaffected). A non-2xx Resend response is logged WARN but not fatal.
pub async fn deliver_email(
    client: &reqwest::Client,
    to_email: &str,
    digest: &Digest,
) -> anyhow::Result<()> {
    let api_key = match std::env::var("RESEND_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            tracing::warn!("RESEND_API_KEY unset; skipping digest email");
            return Ok(());
        }
    };
    let from = std::env::var("DIGEST_FROM")
        .unwrap_or_else(|_| "Dashboard <onboarding@resend.dev>".to_string());
    let subject = email_subject(digest);
    let html = render_email_html(digest);
    let body = resend_body(&from, to_email, &subject, &html);
    let resp = client
        .post("https://api.resend.com/emails")
        .bearer_auth(&api_key)
        .json(&body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await;
    match resp {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => {
            let code = r.status();
            let txt = r.text().await.unwrap_or_default();
            tracing::warn!(status = %code, body = %txt, "Resend digest email non-2xx");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(err = %e, "Resend digest email failed");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::model::*;

    fn sample() -> Digest {
        Digest {
            date: "2026-05-28".into(),
            spending: SpendingSection { total: 42.0, currency: "CNY".into(), by_category: vec![("餐饮".into(), 42.0)] },
            wealth: WealthSection { net_worth: 1000.0, net_delta: 10.0, cash: 500.0, investments: 600.0, investments_delta: 5.0, debt: 100.0, currency: "CNY".into() },
            market: Some(MarketBrief {
                gold: Quote { name: "黄金".into(), price: "2360".into(), conclusion: "走高".into() },
                btc: Quote { name: "比特币".into(), price: "67000".into(), conclusion: "回落".into() },
                index: Quote { name: "纳斯达克".into(), price: "17500".into(), conclusion: "领涨".into() },
                summary: "整体回暖".into(),
            }),
        }
    }

    #[test]
    fn subject_includes_date() {
        assert_eq!(email_subject(&sample()), "今日简报 · 2026-05-28");
    }

    #[test]
    fn html_contains_all_sections() {
        let h = render_email_html(&sample());
        assert!(h.contains("昨日支出"));
        assert!(h.contains("42.00 CNY"));
        assert!(h.contains("餐饮"));
        assert!(h.contains("净值 1000.00 CNY"));
        assert!(h.contains("黄金"));
        assert!(h.contains("整体回暖"));
    }

    #[test]
    fn html_omits_market_when_absent() {
        let mut d = sample();
        d.market = None;
        let h = render_email_html(&d);
        assert!(!h.contains("市场"));
    }

    #[test]
    fn resend_body_shape() {
        let b = resend_body("Dashboard <d@x.com>", "u@y.com", "subj", "<p>hi</p>");
        assert_eq!(b["from"], "Dashboard <d@x.com>");
        assert_eq!(b["to"][0], "u@y.com");
        assert_eq!(b["subject"], "subj");
        assert_eq!(b["html"], "<p>hi</p>");
    }
}
