//! `harness-rs-tools-web` — two `#[tool]`-registered web access primitives that
//! any harness-rs agent can pick up via the inventory registry.
//!
//! - [`web_search`] — DuckDuckGo HTML → Bing fallback, returns ranked
//!   `title / url / snippet` JSON. No API key required.
//! - [`web_fetch`] — GET URL, strip HTML → readable text, with truncation.
//! - [`GroundedWebSearch`] — same tool name, but asks the *model's own*
//!   built-in search first (Gemini grounding via [`Model::search_web`]) and
//!   only falls back to the scraper when the model has none. Register it
//!   after the macro tools and it replaces the scraper by name:
//!   ```ignore
//!   let model: Arc<dyn Model> = Arc::new(OpenAiCompat::with_key(base, "gemini-3.6-flash", key));
//!   AgentLoop::boxed(model.clone())
//!       .with_tool(Arc::new(GroundedWebSearch::new(model)))
//!   ```
//!
//! Both retry once on transient network failure and report engine + status
//! honestly when they come up empty. Patterned after the original two-tool
//! shape in `examples/investor-bot` (audit #12).

use harness::ToolError;
use harness::prelude::*;
use scraper::{Html, Selector};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!(
            "harness-rs-tools-web/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/liliang-cn/harness-rs)"
        ))
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .expect("reqwest client")
}

// ─────────────────────────────────────────────────────────────────────
// web_search
// ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct SearchHit {
    rank: u32,
    title: String,
    url: String,
    snippet: String,
}

/// Search the public web. Tries DuckDuckGo HTML first, falls back to Bing if
/// DDG returns nothing or errors. Each engine gets one retry on transient
/// failure. Returns ranked `title / url / snippet` for the top-N hits. Use
/// this before `web_fetch` to find candidate sources.
#[harness::tool(
    name = "web_search",
    risk = "network",
    schema = r#"{
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 20, "default": 8}
        },
        "required": ["query"]
    }"#
)]
async fn web_search(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let query =
        args.get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "web_search".into(),
                reason: "query required".into(),
            })?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(8)
        .min(20) as usize;

    scraper_search(query, limit).await
}

/// The scraper path: DuckDuckGo → Bing, one retry each. Shared by the plain
/// [`web_search`] tool and [`GroundedWebSearch`]'s fallback.
async fn scraper_search(query: &str, limit: usize) -> Result<ToolResult, ToolError> {
    let mut tried: Vec<String> = Vec::new();
    let mut errs: Vec<String> = Vec::new();

    for engine in [SearchEngine::DuckDuckGo, SearchEngine::Bing] {
        tried.push(engine.name().into());
        match search_with_retry(engine, query, limit).await {
            Ok(hits) if !hits.is_empty() => {
                return Ok(ToolResult {
                    ok: true,
                    content: json!({
                        "query":   query,
                        "count":   hits.len(),
                        "engine":  engine.name(),
                        "results": hits,
                    }),
                    trace: None,
                });
            }
            Ok(_) => errs.push(format!("{}: 0 results", engine.name())),
            Err(e) => errs.push(format!("{}: {e}", engine.name())),
        }
    }

    Ok(ToolResult {
        ok: false,
        content: json!({
            "query":         query,
            "count":         0,
            "engines_tried": tried,
            "errors":        errs,
            "results":       serde_json::Value::Array(vec![]),
            "hint":          "All engines empty or errored — try a more specific query, or web_fetch a known URL.",
        }),
        trace: None,
    })
}

// ─────────────────────────────────────────────────────────────────────
// GroundedWebSearch — the model's own search first, scraper as fallback
// ─────────────────────────────────────────────────────────────────────

/// `web_search`, but the model's built-in search engine answers first.
///
/// Providers with server-side grounding (Gemini's googleSearch) search better
/// than an HTML scraper: fresher index, no bot-walls, and the provider
/// synthesizes an answer with sources instead of returning links to fetch.
/// When the session's model has that capability ([`Model::search_web`] returns
/// `Some`), this tool uses it; when it doesn't — or the grounded call fails —
/// it falls back to the DuckDuckGo/Bing scraper, so registering this tool is
/// never a downgrade.
///
/// It deliberately shares the name `web_search`: registered via
/// `AgentLoop::with_tool` *after* the inventory tools, it replaces the scraper
/// by name and nothing model-facing changes. Wiring:
///
/// ```ignore
/// let model: Arc<dyn Model> = Arc::new(OpenAiCompat::with_key(base, model_id, key));
/// AgentLoop::boxed(model.clone())
///     .with_tool(Arc::new(GroundedWebSearch::new(model)))
/// ```
pub struct GroundedWebSearch {
    model: std::sync::Arc<dyn harness_core::Model>,
}

impl GroundedWebSearch {
    pub fn new(model: std::sync::Arc<dyn harness_core::Model>) -> Self {
        Self { model }
    }
}

#[async_trait::async_trait]
impl harness_core::Tool for GroundedWebSearch {
    fn name(&self) -> &str {
        "web_search"
    }

    fn schema(&self) -> &harness_core::ToolSchema {
        static S: std::sync::OnceLock<harness_core::ToolSchema> = std::sync::OnceLock::new();
        S.get_or_init(|| harness_core::ToolSchema {
            name: "web_search".into(),
            description: "Search the web. Uses the model provider's built-in search \
                          (grounded, with sources) when available, otherwise ranked \
                          title/url/snippet results from public engines. Use before \
                          web_fetch to find candidate sources."
                .into(),
            input: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20, "default": 8}
                },
                "required": ["query"]
            }),
        })
    }

    fn risk(&self) -> harness_core::ToolRisk {
        harness_core::ToolRisk::Network
    }

    async fn invoke(&self, args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
        let query =
            args.get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidArgs {
                    name: "web_search".into(),
                    reason: "query required".into(),
                })?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(8)
            .min(20) as usize;

        match self.model.search_web(query).await {
            Some(Ok(answer)) if !answer.trim().is_empty() => Ok(ToolResult {
                ok: true,
                content: json!({
                    "query":  query,
                    "engine": "model-grounding",
                    "model":  self.model.info().model,
                    "answer": answer,
                    "note":   "Answered by the model provider's built-in web search \
                               (grounded). This is a synthesized answer, not a link list.",
                }),
                trace: None,
            }),
            // Grounding exists but failed or came back empty: the scraper is
            // a worse engine than a grounded one, but a better one than none.
            Some(Err(e)) => {
                tracing::warn!(error = %e, "grounded search failed; falling back to scraper");
                scraper_search(query, limit).await
            }
            Some(Ok(_)) | None => scraper_search(query, limit).await,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SearchEngine {
    DuckDuckGo,
    Bing,
}
impl SearchEngine {
    fn name(&self) -> &'static str {
        match self {
            Self::DuckDuckGo => "duckduckgo",
            Self::Bing => "bing",
        }
    }
}

async fn search_with_retry(
    engine: SearchEngine,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, String> {
    let mut last_err = String::new();
    for attempt in 1..=2 {
        let result = match engine {
            SearchEngine::DuckDuckGo => search_duckduckgo(query, limit).await,
            SearchEngine::Bing => search_bing(query, limit).await,
        };
        match result {
            Ok(hits) => return Ok(hits),
            Err(e) => {
                last_err = e;
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                }
            }
        }
    }
    Err(last_err)
}

async fn search_duckduckgo(query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    // kl=us-en pins region/language to US English so server-IP geolocation
    // doesn't poison results (e.g. JP-localised DDG for a JP IP).
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}&kl=us-en",
        urlencoding::encode(query)
    );
    let body = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .text()
        .await
        .map_err(|e| format!("body: {e}"))?;

    if is_ddg_anomaly(&body) {
        return Err("DDG anti-bot anomaly modal (IP rate-limited)".into());
    }

    let doc = Html::parse_document(&body);
    let result_sel = Selector::parse("div.result, div.web-result").unwrap();
    let title_sel = Selector::parse("a.result__a, a.result-link").unwrap();
    let snip_sel = Selector::parse(".result__snippet, .result-snippet").unwrap();

    let mut hits = Vec::with_capacity(limit);
    for (i, node) in doc.select(&result_sel).take(limit).enumerate() {
        let (title, url) = node
            .select(&title_sel)
            .next()
            .map(|a| {
                let t = a.text().collect::<String>().trim().to_string();
                let raw = a.value().attr("href").unwrap_or("").to_string();
                (t, unwrap_duckduckgo_redirect(&raw))
            })
            .unwrap_or_default();
        let snippet = node
            .select(&snip_sel)
            .next()
            .map(|s| {
                s.text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if !title.is_empty() && !url.is_empty() {
            hits.push(SearchHit {
                rank: i as u32 + 1,
                title,
                url,
                snippet,
            });
        }
    }
    Ok(hits)
}

async fn search_bing(query: &str, limit: usize) -> Result<Vec<SearchHit>, String> {
    // mkt=en-US forces US English market; otherwise Bing serves Japanese /
    // Chinese localised results for IPs in those regions and the SERP layout
    // changes (snippets lose technical/financial sources).
    let url = format!(
        "https://www.bing.com/search?q={}&mkt=en-US&setlang=en",
        urlencoding::encode(query)
    );
    let body = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?
        .text()
        .await
        .map_err(|e| format!("body: {e}"))?;

    if is_bing_captcha(&body) {
        return Err("Bing captcha challenge (IP flagged)".into());
    }

    let doc = Html::parse_document(&body);
    let result_sel = Selector::parse("li.b_algo").unwrap();
    let title_sel = Selector::parse("h2 a").unwrap();
    let snip_sel =
        Selector::parse(".b_caption p, .b_lineclamp2, .b_lineclamp3, .b_lineclamp4").unwrap();

    let mut hits = Vec::with_capacity(limit);
    for (i, node) in doc.select(&result_sel).take(limit).enumerate() {
        let (title, url) = node
            .select(&title_sel)
            .next()
            .map(|a| {
                (
                    a.text().collect::<String>().trim().to_string(),
                    a.value().attr("href").unwrap_or("").to_string(),
                )
            })
            .unwrap_or_default();
        let snippet = node
            .select(&snip_sel)
            .next()
            .map(|s| {
                s.text()
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        if !title.is_empty() && url.starts_with("http") {
            hits.push(SearchHit {
                rank: i as u32 + 1,
                title,
                url,
                snippet,
            });
        }
    }
    Ok(hits)
}

fn is_ddg_anomaly(body: &str) -> bool {
    body.contains("anomaly-modal") || body.contains("Anomaly detected")
}

fn is_bing_captcha(body: &str) -> bool {
    body.contains("captcha_text") || body.contains("class=\"captcha\"")
}

/// DuckDuckGo's `/l/?uddg=ENCODED&kh=...` redirect → unwrap to the target URL.
fn unwrap_duckduckgo_redirect(href: &str) -> String {
    if let Some(idx) = href.find("uddg=") {
        let rest = &href[idx + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        if let Ok(decoded) = urlencoding::decode(&rest[..end]) {
            return decoded.into_owned();
        }
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

// ─────────────────────────────────────────────────────────────────────
// web_fetch
// ─────────────────────────────────────────────────────────────────────

/// Fetch a URL and return readable text content (HTML stripped to plain
/// paragraphs, JSON/text passed through). Truncates at `max_chars`. Use
/// after `web_search` to read promising pages.
#[harness::tool(
    name = "web_fetch",
    risk = "network",
    schema = r#"{
        "type": "object",
        "properties": {
            "url":       {"type": "string"},
            "max_chars": {"type": "integer", "minimum": 200, "maximum": 20000, "default": 6000}
        },
        "required": ["url"]
    }"#
)]
async fn web_fetch(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "web_fetch".into(),
            reason: "url required".into(),
        })?;
    let max_chars = args
        .get("max_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(6000) as usize;

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ToolError::InvalidArgs {
            name: "web_fetch".into(),
            reason: format!("not http(s): {url}"),
        });
    }

    let (status, ct, body) = {
        let mut last_err = String::new();
        let mut got: Option<(reqwest::StatusCode, String, String)> = None;
        for attempt in 1..=2 {
            match http_client()
                .get(url)
                .header(
                    "Accept",
                    "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.5",
                )
                .send()
                .await
            {
                Ok(resp) => {
                    let s = resp.status();
                    let c = resp
                        .headers()
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    match resp.text().await {
                        Ok(b) => {
                            got = Some((s, c, b));
                            break;
                        }
                        Err(e) => last_err = format!("body: {e}"),
                    }
                }
                Err(e) => last_err = format!("send: {e}"),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(600)).await;
            }
        }
        got.ok_or_else(|| ToolError::Exec(format!("fetch: {last_err}")))?
    };

    let cleaned = if ct.contains("application/json") || ct.contains("text/plain") {
        body
    } else {
        html_to_text(&body)
    };

    let (text, truncated) = clip_text(&cleaned, max_chars);

    Ok(ToolResult {
        ok: status.is_success(),
        content: json!({
            "url":            url,
            "status":         status.as_u16(),
            "content_type":   ct,
            "text":           text,
            "truncated":      truncated,
            "original_chars": cleaned.chars().count(),
        }),
        trace: None,
    })
}

fn html_to_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    let body_sel = Selector::parse("body").unwrap();
    let target = doc
        .select(&body_sel)
        .next()
        .unwrap_or_else(|| doc.root_element());
    let mut buf = String::new();
    walk_text(target, &mut buf);
    buf.split_whitespace().collect::<Vec<_>>().join(" ")
}

const SKIP_TAGS: &[&str] = &[
    "script", "style", "nav", "footer", "header", "noscript", "iframe", "svg",
];

fn walk_text(node: scraper::ElementRef<'_>, out: &mut String) {
    if SKIP_TAGS.contains(&node.value().name()) {
        return;
    }
    for child in node.children() {
        if let Some(el) = scraper::ElementRef::wrap(child) {
            walk_text(el, out);
        } else if let Some(text) = child.value().as_text() {
            out.push_str(text);
            out.push(' ');
        }
    }
}

fn clip_text(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        (s.to_string(), false)
    } else {
        let head: String = s.chars().take(max_chars * 8 / 10).collect();
        let tail: String = s
            .chars()
            .rev()
            .take(max_chars * 2 / 10)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        (format!("{head}\n\n[…truncated…]\n\n{tail}"), true)
    }
}

#[cfg(test)]
mod tests {
    use harness_core::iter_macro_tools;

    /// A model whose only talent is grounded search.
    struct GroundedStub;
    #[async_trait::async_trait]
    impl harness_core::Model for GroundedStub {
        async fn complete(
            &self,
            _ctx: &harness_core::Context,
        ) -> Result<harness_core::ModelOutput, harness_core::ModelError> {
            unreachable!("not used")
        }
        fn info(&self) -> harness_core::ModelInfo {
            harness_core::ModelInfo {
                handle: "stub".into(),
                provider: "test".into(),
                model: "stub-grounded".into(),
                context_window: 8192,
                input_cost_usd_per_million_tokens: None,
                output_cost_usd_per_million_tokens: None,
                supports_tool_use: false,
                supports_streaming: false,
                supports_web_grounding: true,
            }
        }
        async fn search_web(
            &self,
            query: &str,
        ) -> Option<Result<String, harness_core::ModelError>> {
            Some(Ok(format!("grounded answer for {query}")))
        }
    }

    #[tokio::test]
    async fn grounded_search_prefers_the_models_own_engine() {
        use harness_core::Tool;
        let ws = std::env::temp_dir().join(format!("gws-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let mut world = harness_context::default_world(&ws);
        let tool = super::GroundedWebSearch::new(std::sync::Arc::new(GroundedStub));
        // Same registry name as the scraper — registering it replaces web_search.
        assert_eq!(tool.name(), "web_search");
        let r = tool
            .invoke(serde_json::json!({"query": "rust harness"}), &mut world)
            .await
            .unwrap();
        assert!(r.ok);
        assert_eq!(r.content["engine"], "model-grounding");
        assert!(
            r.content["answer"]
                .as_str()
                .unwrap()
                .contains("grounded answer for rust harness")
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn tools_register_via_inventory() {
        let names: Vec<String> = iter_macro_tools().map(|t| t.name().to_string()).collect();
        assert!(
            names.iter().any(|n| n == "web_search"),
            "web_search not registered: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "web_fetch"),
            "web_fetch not registered: {names:?}"
        );
    }

    #[test]
    fn ddg_redirect_unwrap() {
        let raw = "/l/?kh=-1&uddg=https%3A%2F%2Fexample.com%2Fa%2Fb%3Fq%3D1&extra=2";
        let got = super::unwrap_duckduckgo_redirect(raw);
        assert_eq!(got, "https://example.com/a/b?q=1");
    }

    #[test]
    fn html_to_text_strips_script_and_collapses_whitespace() {
        let html = r#"<html><body>
            <script>var x = 1;</script>
            <p>Hello   world</p>
            <p>Second   line</p>
            <style>.x{}</style>
        </body></html>"#;
        let text = super::html_to_text(html);
        assert!(text.contains("Hello world"), "got: {text}");
        assert!(text.contains("Second line"), "got: {text}");
        assert!(!text.contains("var x"), "script leaked: {text}");
    }

    #[test]
    fn clip_text_under_limit_no_truncate() {
        let (t, tr) = super::clip_text("short", 100);
        assert!(!tr);
        assert_eq!(t, "short");
    }

    #[test]
    fn clip_text_over_limit_marks_truncated() {
        let long: String = "abcdefghij".repeat(100); // 1000 chars
        let (_, tr) = super::clip_text(&long, 200);
        assert!(tr);
    }
}
