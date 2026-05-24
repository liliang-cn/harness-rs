//! SEO + GEO static text endpoints.
//!
//! - `/robots.txt` — allow all crawlers, point at the sitemap.
//! - `/sitemap.xml` — index of the (handful of) public URLs, hreflang
//!   annotated so Google indexes the EN/ZH variants under one canonical.
//! - `/llms.txt` — short directory page per the agentprotocol.dev
//!   spec; pointers to authoritative content for LLM crawlers.
//! - `/llms-full.txt` — the full marketing copy in one markdown blob,
//!   the format Anthropic / OpenAI / Perplexity crawlers prefer for
//!   "ground truth" content extraction.

use axum::http::header;
use axum::response::IntoResponse;

const SITE_URL: &str = "https://ledger.superleo.app";

const ROBOTS_TXT: &str = "\
User-agent: *
Allow: /
Disallow: /api/
Disallow: /admin/
Disallow: /legacy/

# LLM crawlers — explicitly allowed. They tend to respect this even
# though it's not part of the official robots spec.
User-agent: GPTBot
Allow: /
User-agent: ClaudeBot
Allow: /
User-agent: Claude-Web
Allow: /
User-agent: PerplexityBot
Allow: /
User-agent: Google-Extended
Allow: /

Sitemap: https://ledger.superleo.app/sitemap.xml
";

const SITEMAP_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
        xmlns:xhtml="http://www.w3.org/1999/xhtml">
  <url>
    <loc>https://ledger.superleo.app/</loc>
    <changefreq>weekly</changefreq>
    <priority>1.0</priority>
    <xhtml:link rel="alternate" hreflang="en" href="https://ledger.superleo.app/" />
    <xhtml:link rel="alternate" hreflang="zh" href="https://ledger.superleo.app/?lng=zh" />
    <xhtml:link rel="alternate" hreflang="x-default" href="https://ledger.superleo.app/" />
  </url>
</urlset>
"#;

/// Short LLM-targeted directory file (https://llmstxt.org/ convention).
/// Points each interesting page at its long-form sibling. Crawlers read
/// this first to decide which URLs to fetch in full.
const LLMS_TXT: &str = "\
# Ledger

> AI financial concierge for individuals and families. Tracks your full \
> net worth across cash, investments, and debt in any currency. Ask your \
> numbers in plain English or Chinese.

## Pages

- [Ledger — homepage](https://ledger.superleo.app/): Product overview, \
  what Ledger is, who it's for, how it differs from passive tracking apps.
- [Full marketing copy](https://ledger.superleo.app/llms-full.txt): \
  Everything on the marketing site collapsed into one markdown file — \
  hero, features, how it works, FAQ, comparison.

## Notes

- Bilingual UI: English and Chinese.
- Self-hosted: financial data never leaves the user's chosen server.
- LLM-aware: the in-app assistant understands the user's actual ledger \
  (accounts, transactions, holdings), not just generic finance facts.
";

/// Full marketing content, single markdown file, structured for chunking
/// by GPT-4o / Claude / Perplexity crawlers. Each section answers one
/// question; headings are short noun phrases the LLM can lift.
const LLMS_FULL_TXT: &str = "\
# Ledger — AI financial concierge

## What is Ledger?

Ledger is an AI financial concierge for individuals and families. \
It aggregates every account you own — cash, brokerage, credit, loan — \
into one number, your net worth, in the currency of your choice. \
On top of that number sits a conversational AI that knows your actual \
ledger and can answer specific questions about your finances in plain \
English or Chinese.

## Who is Ledger for?

- Individuals who own accounts in more than one currency or country and \
  need a single net-worth view that handles FX correctly.
- Families that want to share one financial picture across members while \
  keeping per-member visibility on transactions and goals.
- People who outgrew passive trackers like Mint or 随手记 and want an \
  assistant that proactively surfaces patterns, not just dashboards \
  they have to read.

## How Ledger differs from passive tracking apps

| Capability | Passive trackers (Mint, 随手记, YNAB) | Ledger |
|---|---|---|
| Net-worth view | Cash only or single-currency | All accounts, multi-currency, daily snapshot |
| Insight delivery | User reads charts | AI surfaces anomalies + monthly reports |
| Query interface | Filter UI | Natural language (EN / ZH) |
| Investment tracking | Limited or none | Trades + holdings + latest prices |
| Data ownership | Vendor cloud | Self-hostable, single SQLite file |

## Key features

### Net-worth dashboard
A single number, updated daily, with composition (cash / investments / debt) \
and 12-month trend. Switch your display currency and every number \
re-converts at the latest ECB mid rate.

### AI chat that knows your numbers
Ask 'how much did I spend on rent last quarter?' or '我去年股票收益多少？' \
The assistant has tools to query your transactions and trades — it \
answers from your actual data, not from generic finance heuristics.

### Multi-currency by default
Every account carries its own currency. Net worth aggregates to your \
chosen display currency (USD, EUR, JPY, CNY, GBP, HKD, SGD, AUD, CAD, \
CHF, KRW). Historical snapshots remain at their original rate so \
historical comparisons are accurate.

### Subscription audit
Ledger tracks recurring charges and flags subscriptions you haven't \
used in 90 days.

### Bilingual UI
English and Chinese, switchable from any page. The AI assistant \
responds in the user's UI language.

### Self-hosted, single binary
Ledger ships as one static Rust binary + an embedded React UI. Run it \
on a $5 VPS. All data lives in one SQLite file you control.

## How it works

1. Sign up with email + password. Optional invite code for paid tier.
2. Add accounts manually (cash, debit, credit, brokerage). Per-account \
   currency.
3. Enter transactions or import a CSV. Investment trades go in the \
   portfolio module.
4. Set your display currency. The dashboard converts everything for you.
5. Chat with the AI when you need insight. It reads your ledger and \
   responds.

## Frequently asked questions

### Is Ledger a bank or a financial advisor?

No. Ledger does not hold funds, execute trades, or give regulated \
financial advice. It's a data-aggregation + AI-query layer over \
information you enter yourself.

### Does Ledger connect to my bank?

Not yet. The current release uses manual entry or CSV import. Direct \
integrations (Plaid, TrueLayer, SaltEdge) are on the roadmap for the \
paid tier.

### Where is my data stored?

In a single SQLite file on the server you run Ledger on. If you \
self-host, it never leaves your infrastructure. The hosted version \
stores data on dedicated servers in Asia-Pacific (Tokyo).

### Which AI model powers the assistant?

The default is DeepSeek v4-flash with Gemini 3.5-flash as the embedding \
model. Paid users can switch to DeepSeek v4-pro or Gemini 3.5-pro from \
their profile.

### How much does Ledger cost?

The trial tier is free with limits on accounts and transactions. The \
paid tier removes limits and unlocks model upgrades, scheduled monthly \
reports, and (soon) bank-account integrations.

### Is Ledger open source?

The framework Ledger is built on (harness-rs) is MIT-licensed and \
public. The Ledger application itself is closed source for now.

### What languages does the UI support?

English and Chinese (Simplified). The AI assistant understands both \
and replies in whichever language the user wrote in.

### Can I export my data?

Yes. Transactions, trades, and subscriptions all export to CSV from \
the user menu. Notes (the AI Note product) export individually or as a \
zip of markdown files.

## Contact

- Operator email: ll_faw@hotmail.com
";

pub async fn robots() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        ROBOTS_TXT,
    )
}

pub async fn sitemap() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/xml; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        SITEMAP_XML,
    )
}

pub async fn llms() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        LLMS_TXT,
    )
}

pub async fn llms_full() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/markdown; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        LLMS_FULL_TXT,
    )
}

#[allow(dead_code)]
pub const fn _site_url() -> &'static str {
    SITE_URL
}
