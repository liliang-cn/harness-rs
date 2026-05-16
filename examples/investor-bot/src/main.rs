//! Investor research agent — built on harness-rs.
//!
//! 4 custom #[tool]s let an LLM autonomously navigate public financial info:
//!   • web_search   — DuckDuckGo HTML search (no API key)
//!   • web_fetch    — GET a URL, strip HTML to readable text (capped)
//!   • save_note    — persist a research finding with source URL + tags
//!   • list_notes   — recall saved findings, optionally filtered
//!
//! Storage: ~/.harness-investor/notes.json
//!
//! THIS IS NOT FINANCIAL ADVICE. The agent retrieves and summarises public
//! information. Verify everything before acting on it.
//!
//! ```sh
//! DEEPSEEK_API_KEY=sk-... investor "Buffett 最近季度新增了什么仓位？"
//! DEEPSEEK_API_KEY=sk-... investor "GS 给 NVDA 设的目标价是多少？"
//! DEEPSEEK_API_KEY=sk-... investor --repl
//! DEEPSEEK_API_KEY=sk-... investor list                       # 看历史笔记
//! ```

use chrono::{DateTime, Utc};
use clap::Parser;
use harness::prelude::*;
use harness::ToolError;
use harness_context::with_profile;
use harness_core::{Model, UserProfile};
use harness_loop::{AgentLoop, Outcome, ProfileGuide};
use harness_models::{OpenAiCompat, providers::DEEPSEEK};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

// =================================================================
// Storage
// =================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Note {
    id:         String,
    topic:      String,
    content:    String,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    tags:       Vec<String>,
    saved_at:   DateTime<Utc>,
}

fn store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".harness-investor")
}

fn notes_path() -> PathBuf { store_dir().join("notes.json") }

fn load_notes() -> Vec<Note> {
    std::fs::read_to_string(notes_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_notes(notes: &[Note]) -> Result<(), ToolError> {
    let dir = store_dir();
    std::fs::create_dir_all(&dir).map_err(|e| ToolError::Exec(e.to_string()))?;
    let json = serde_json::to_string_pretty(notes).map_err(|e| ToolError::Exec(e.to_string()))?;
    std::fs::write(notes_path(), json).map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(())
}

// =================================================================
// HTTP — shared client w/ polite User-Agent
// =================================================================

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("investor-bot/0.0.2 (research agent; harness-rs example)")
        .timeout(std::time::Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .expect("reqwest client")
}

// =================================================================
// Tool 0: current_time — grounds "today / this week / recent" queries
// =================================================================

/// Return the current wall-clock time. Always call this first for any query
/// that mentions "today / yesterday / this week / recent / latest / 最近 / 今天".
/// Uses World.profile.tz when set, falls back to UTC.
#[harness::tool(
    name = "current_time",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#,
)]
async fn current_time(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let now_ms = w.clock.now_ms();
    let utc = chrono::DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(|| ToolError::Exec("clock returned invalid timestamp".into()))?;

    let (iso_local, weekday, human, tz_source) = match w
        .profile
        .tz
        .as_deref()
        .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
    {
        Some(tz) => {
            let local = utc.with_timezone(&tz);
            (
                local.to_rfc3339(),
                local.format("%A").to_string(),
                local.format("%Y-%m-%d %H:%M %Z").to_string(),
                format!("profile.tz={}", w.profile.tz.as_deref().unwrap_or("?")),
            )
        }
        None => {
            let local = utc.with_timezone(&chrono::Utc);
            (
                local.to_rfc3339(),
                local.format("%A").to_string(),
                local.format("%Y-%m-%d %H:%M UTC").to_string(),
                "UTC (no profile tz)".to_string(),
            )
        }
    };

    Ok(ToolResult {
        ok: true,
        content: json!({
            "iso_utc":   utc.to_rfc3339(),
            "iso_local": iso_local,
            "weekday":   weekday,
            "human":     human,
            "timezone":  tz_source,
        }),
        trace: None,
    })
}

// =================================================================
// Tool 1: web_search — DuckDuckGo HTML, no API key needed
// =================================================================

#[derive(Debug, Serialize)]
struct SearchHit { rank: u32, title: String, url: String, snippet: String }

/// Search the public web via DuckDuckGo HTML. Returns ranked title + URL + snippet for top N hits. Use first to find candidate sources.
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
    }"#,
)]
async fn web_search(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let query = args.get("query").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "investor".into(), reason: "query required".into() })?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(8).min(20) as usize;

    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencoding::encode(query));
    let body = http_client()
        .get(&url)
        .send().await.map_err(|e| ToolError::Exec(format!("search: {e}")))?
        .text().await.map_err(|e| ToolError::Exec(format!("search body: {e}")))?;

    let doc = Html::parse_document(&body);
    let result_sel = Selector::parse("div.result, div.web-result").unwrap();
    let title_sel  = Selector::parse("a.result__a, a.result-link").unwrap();
    let snip_sel   = Selector::parse(".result__snippet, .result-snippet").unwrap();

    let mut hits = Vec::with_capacity(limit);
    for (i, node) in doc.select(&result_sel).take(limit).enumerate() {
        let (title, url) = node.select(&title_sel).next()
            .map(|a| {
                let t = a.text().collect::<String>().trim().to_string();
                let raw = a.value().attr("href").unwrap_or("").to_string();
                (t, unwrap_duckduckgo_redirect(&raw))
            })
            .unwrap_or_default();
        let snippet = node.select(&snip_sel).next()
            .map(|s| s.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default();
        if !title.is_empty() && !url.is_empty() {
            hits.push(SearchHit { rank: i as u32 + 1, title, url, snippet });
        }
    }

    Ok(ToolResult {
        ok: true,
        content: json!({"query": query, "count": hits.len(), "results": hits}),
        trace: None,
    })
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

// =================================================================
// Tool 2: web_fetch — GET URL, strip to readable text
// =================================================================

/// Fetch a URL and return the readable text content (HTML stripped). Truncates at max_chars. Use after web_search to read promising pages.
#[harness::tool(
    name = "web_fetch",
    risk = "network",
    schema = r#"{
        "type": "object",
        "properties": {
            "url":   {"type": "string"},
            "max_chars": {"type": "integer", "minimum": 200, "maximum": 20000, "default": 6000}
        },
        "required": ["url"]
    }"#,
)]
async fn web_fetch(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let url = args.get("url").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "investor".into(), reason: "url required".into() })?;
    let max_chars = args.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(6000) as usize;

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(ToolError::InvalidArgs { name: "investor".into(), reason: format!("not http(s): {url}") });
    }

    let resp = http_client()
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.5")
        .send().await.map_err(|e| ToolError::Exec(format!("fetch: {e}")))?;
    let status = resp.status();
    let ct = resp.headers().get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.text().await.map_err(|e| ToolError::Exec(format!("body: {e}")))?;

    let cleaned = if ct.contains("application/json") || ct.contains("text/plain") {
        body
    } else {
        html_to_text(&body)
    };

    let (text, truncated) = clip_text(&cleaned, max_chars);

    Ok(ToolResult {
        ok: status.is_success(),
        content: json!({
            "url":          url,
            "status":       status.as_u16(),
            "content_type": ct,
            "text":         text,
            "truncated":    truncated,
            "original_chars": cleaned.chars().count(),
        }),
        trace: None,
    })
}

fn html_to_text(html: &str) -> String {
    let doc = Html::parse_document(html);
    let body_sel = Selector::parse("body").unwrap();
    let target = doc.select(&body_sel).next().unwrap_or_else(|| doc.root_element());
    let mut buf = String::new();
    walk_text(target, &mut buf);
    buf.split_whitespace().collect::<Vec<_>>().join(" ")
}

const SKIP_TAGS: &[&str] = &["script", "style", "nav", "footer", "header", "noscript", "iframe", "svg"];

fn walk_text(node: scraper::ElementRef<'_>, out: &mut String) {
    if SKIP_TAGS.contains(&node.value().name()) { return; }
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
    if s.chars().count() <= max_chars { (s.to_string(), false) }
    else {
        let head: String = s.chars().take(max_chars * 8 / 10).collect();
        let tail: String = s.chars().rev().take(max_chars * 2 / 10).collect::<String>().chars().rev().collect();
        (format!("{head}\n\n[…truncated…]\n\n{tail}"), true)
    }
}

// =================================================================
// Tool 3: save_note — persist a research finding
// =================================================================

/// Persist a research finding with optional source URL + tags. Use for any material fact you'll want to recall later.
#[harness::tool(
    name = "save_note",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "topic":      {"type": "string", "description": "Short heading, e.g. 'Buffett Q1 2026 13F: top adds'."},
            "content":    {"type": "string", "description": "The finding itself in prose. Include numbers, dates, the source's claim verbatim."},
            "source_url": {"type": "string", "description": "URL of the page the fact came from. ALWAYS include when available."},
            "tags":       {"type": "array",  "items": {"type": "string"}, "description": "e.g. ['buffett', '13F', 'AAPL']"}
        },
        "required": ["topic", "content"]
    }"#,
)]
async fn save_note(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let topic = args.get("topic").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "investor".into(), reason: "topic required".into() })?
        .to_string();
    let content = args.get("content").and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs { name: "investor".into(), reason: "content required".into() })?
        .to_string();
    let source_url = args.get("source_url").and_then(|v| v.as_str()).map(String::from);
    let tags: Vec<String> = args.get("tags")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();

    let mut notes = load_notes();
    let note = Note {
        id:         Uuid::new_v4().to_string()[..8].to_string(),
        topic, content, source_url, tags,
        saved_at:   Utc::now(),
    };
    notes.push(note.clone());
    save_notes(&notes)?;
    Ok(ToolResult { ok: true, content: json!({"saved": note}), trace: None })
}

// =================================================================
// Tool 4: list_notes — recall research
// =================================================================

/// Recall previously-saved research notes, optionally filtered by tag or saved-after timestamp.
#[harness::tool(
    name = "list_notes",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "tag":   {"type": "string", "description": "Filter by tag (case-insensitive substring match)."},
            "since": {"type": "string", "description": "ISO 8601 — only return notes saved at or after this time."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20}
        }
    }"#,
)]
async fn list_notes(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let tag = args.get("tag").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
    let since = args.get("since").and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let mut hits: Vec<Note> = load_notes().into_iter()
        .filter(|n| tag.as_deref().map_or(true,
            |t| n.tags.iter().any(|x| x.to_lowercase().contains(t))))
        .filter(|n| since.map_or(true, |s| n.saved_at >= s))
        .collect();
    hits.sort_by(|a, b| b.saved_at.cmp(&a.saved_at));
    hits.truncate(limit);

    Ok(ToolResult {
        ok: true,
        content: json!({"count": hits.len(), "notes": hits}),
        trace: None,
    })
}

// =================================================================
// Tool registration
// =================================================================

fn collect_tools() -> Vec<Arc<dyn Tool>> {
    use harness_core::iter_macro_tools;
    let want = ["current_time", "web_search", "web_fetch", "save_note", "list_notes"];
    iter_macro_tools().filter(|t| want.contains(&t.name())).collect()
}

// =================================================================
// Prompt
// =================================================================

const SYSTEM_PROMPT: &str = "\
You are an investment-research assistant. You retrieve PUBLIC information from \
the open web and SEC filings; you DO NOT give financial advice. Every factual \
claim in your reply must come from a tool result you can cite — if you don't \
have a source, say 'I don't know' instead of guessing.\n\
\n\
Workflow:\n\
0. If the question mentions any relative time (today, this week, recent, latest, 最新, 最近, 今天, 上周, 本季度, etc.) — call `current_time` FIRST to ground yourself. Otherwise skip step 0.\n\
1. Call `web_search` with a precise query. Read the top hits' titles + snippets.\n\
2. Call `web_fetch` on the 1-3 most relevant URLs to get the actual content.\n\
3. Cross-check: if numbers vary across sources, list both with attribution.\n\
4. Call `save_note` for material findings, ALWAYS with `source_url` and tags.\n\
5. Reply concisely with the answer + a bullet list of source URLs.\n\
\n\
Useful sources:\n\
- SEC EDGAR (https://www.sec.gov/cgi-bin/browse-edgar) for 13F, Form 4, 10-K, 8-K filings\n\
- Berkshire Hathaway CIK: 0001067983 for Buffett\n\
- dataroma.com for superinvestor portfolios (Buffett, Druckenmiller, Burry, etc.)\n\
- finviz.com / yahoo finance / marketwatch for analyst targets + quotes\n\
- capitoltrades.com / quiverquant.com for US Congress trades\n\
\n\
Time-sensitive caveat: institutional filings (13F) lag by up to 45 days from \
quarter-end. Always state the AS-OF date when reporting positions.\n\
\n\
You are NOT a financial advisor. End every numerical / position-related reply \
with 'Not investment advice; verify independently.' (or its zh-CN equivalent \
if locale is set).";

// =================================================================
// CLI
// =================================================================

#[derive(Parser, Debug)]
#[command(name = "investor", about = "Autonomous investment-research agent (harness-rs).")]
struct Cli {
    #[arg(default_values_t = vec!["What's Buffett's largest position right now?".to_string()])]
    task: Vec<String>,

    #[arg(long, default_value = "pro")]
    tier: String,

    #[arg(long, default_value_t = 12)]
    max_iters: u32,

    /// REPL mode for multi-turn investigation.
    #[arg(long)]
    repl: bool,

    /// Just print all saved notes and exit.
    #[arg(long)]
    list: bool,

    /// Clear all saved notes.
    #[arg(long)]
    clear: bool,

    #[arg(long)] name:   Option<String>,
    #[arg(long)] tz:     Option<String>,
    #[arg(long)] locale: Option<String>,

    /// Record a session log for replay/debugging.
    #[arg(long)]
    record: Option<PathBuf>,
}

fn build_profile(cli: &Cli) -> UserProfile {
    UserProfile {
        name:   cli.name.clone().or_else(|| std::env::var("HARNESS_USER_NAME").ok()),
        tz:     cli.tz.clone().or_else(|| std::env::var("HARNESS_USER_TZ").ok()),
        locale: cli.locale.clone().or_else(|| std::env::var("HARNESS_USER_LOCALE").ok()),
        ..Default::default()
    }
}

fn build_task_description(user_request: &str, history: &[(String, String)]) -> String {
    let mut s = SYSTEM_PROMPT.to_string();
    if !history.is_empty() {
        s.push_str("\n\nPrior conversation:\n");
        for (role, text) in history {
            let clipped: String = text.chars().take(400).collect();
            s.push_str(&format!("[{role}] {clipped}\n"));
        }
    }
    s.push_str(&format!("\n[user] {user_request}"));
    s
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.list {
        let notes = load_notes();
        if notes.is_empty() {
            println!("(no saved notes)");
            return Ok(());
        }
        for n in &notes {
            println!("[{}] {} ({})", n.id, n.topic, n.saved_at.format("%Y-%m-%d %H:%M UTC"));
            println!("       tags: {}", n.tags.join(", "));
            if let Some(url) = &n.source_url { println!("       src:  {url}"); }
            println!("       {}\n", n.content);
        }
        println!("{} note(s)", notes.len());
        return Ok(());
    }

    if cli.clear {
        if notes_path().exists() {
            std::fs::remove_file(notes_path())?;
            println!("✓ cleared {}", notes_path().display());
        }
        return Ok(());
    }

    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| anyhow::anyhow!("set DEEPSEEK_API_KEY"))?;

    let model_id = match cli.tier.as_str() {
        "flash" => "deepseek-v4-flash",
        _       => "deepseek-v4-pro",
    };
    let info_model = OpenAiCompat::with_key(DEEPSEEK, model_id, api_key.clone());
    let info = info_model.info();
    drop(info_model);

    let tools = collect_tools();
    let profile = build_profile(&cli);

    println!(
        "→ investor-bot\n  model:     {} ({}/{})\n  tools:     {} registered\n  store:     {}",
        info.handle, info.provider, info.model, tools.len(), notes_path().display(),
    );
    if profile.name.is_some() || profile.tz.is_some() {
        println!("  profile:   {}", profile.summary_line());
    }
    if let Some(p) = &cli.record { println!("  recording: {}", p.display()); }
    println!();

    if cli.repl {
        run_repl(model_id, api_key, tools, profile, cli.max_iters, cli.record).await
    } else {
        run_once(model_id, api_key, tools, profile, cli.max_iters, cli.task.join(" "), cli.record).await
    }
}

async fn run_once(
    model_id: &str,
    api_key:  String,
    tools:    Vec<Arc<dyn Tool>>,
    profile:  UserProfile,
    max_iters: u32,
    user_request: String,
    record:   Option<PathBuf>,
) -> anyhow::Result<()> {
    let model = OpenAiCompat::with_key(DEEPSEEK, model_id, api_key);
    let mut loop_ = AgentLoop::new(model).with_guide(Arc::new(ProfileGuide));
    for t in tools { loop_ = loop_.with_tool(t); }
    if let Some(p) = record {
        let rec = harness_loop::SessionRecorder::new(&p)
            .map_err(|e| anyhow::anyhow!("recorder: {e}"))?;
        loop_ = loop_.with_hook(Arc::new(rec));
    }
    let mut world = with_profile(".", profile);
    let task = Task {
        description: build_task_description(&user_request, &[]),
        source: None, deadline: None,
    };
    match loop_.run_with_max_iters(task, &mut world, max_iters).await? {
        Outcome::Done { text, iters, .. } => {
            println!("✓ done after {iters} iteration(s)\n");
            if let Some(t) = text { println!("{t}"); }
        }
        Outcome::BudgetExhausted { iters, last_text, tools_called, usage, .. } => {
            eprintln!("✗ budget exhausted after {iters} iter(s), {tools_called} tool call(s), \
                       {} in / {} out tokens", usage.input_tokens, usage.output_tokens);
            if let Some(t) = last_text {
                eprintln!("\n— last assistant message before budget ran out —\n{t}");
            }
            eprintln!("\n→ partial findings preserved in {}. `investor --list` to recall.",
                notes_path().display());
            std::process::exit(2);
        }
    }
    Ok(())
}

async fn run_repl(
    model_id: &str,
    api_key:  String,
    tools:    Vec<Arc<dyn Tool>>,
    profile:  UserProfile,
    max_iters: u32,
    record:   Option<PathBuf>,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stdin  = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut history: Vec<(String, String)> = Vec::new();
    const EXIT: &[&str] = &["bye", "quit", "exit", ":q"];

    loop {
        stdout.write_all(b"\nyou> ").await?;
        stdout.flush().await?;
        let Some(line) = stdin.next_line().await? else { println!("\nbye."); break; };
        let input = line.trim();
        if input.is_empty() { continue; }
        if EXIT.contains(&input.to_lowercase().as_str()) { println!("bye."); break; }

        // Build a real Turn-shaped seed so the Compactor can see + shrink history.
        // (Previously: history was stringified into task.description and bypassed
        // the compactor entirely — see audit issue #2.)
        let seed: Vec<harness_core::Turn> = history
            .iter()
            .take_while(|_| true)
            .map(|(role, text)| {
                let role = match role.as_str() {
                    "user" => harness_core::TurnRole::User,
                    _      => harness_core::TurnRole::Assistant,
                };
                harness_core::Turn { role, blocks: vec![harness_core::Block::Text(text.clone())] }
            })
            .collect();

        let model = OpenAiCompat::with_key(DEEPSEEK, model_id, api_key.clone());
        let mut loop_ = AgentLoop::new(model).with_guide(Arc::new(ProfileGuide));
        for t in tools.iter().cloned() { loop_ = loop_.with_tool(t); }
        if let Some(p) = &record {
            let rec = harness_loop::SessionRecorder::new(p)
                .map_err(|e| anyhow::anyhow!("recorder: {e}"))?;
            loop_ = loop_.with_hook(Arc::new(rec));
        }
        let mut world = with_profile(".", profile.clone());
        let task = Task {
            // Single-turn description only — no history stuffing.
            description: format!("{SYSTEM_PROMPT}\n\n[user] {input}"),
            source: None, deadline: None,
        };
        match loop_.run_with_seed_history(task, seed, &mut world, max_iters).await {
            Ok(Outcome::Done { text, iters, .. }) => {
                let response = text.unwrap_or_else(|| "(no response)".into());
                println!("\nasst ({iters} iter)> {response}");
                history.push(("user".into(), input.to_string()));
                history.push(("asst".into(), response));
            }
            Ok(Outcome::BudgetExhausted { iters, last_text, tools_called, usage, .. }) => {
                eprintln!("\nasst> ✗ budget out after {iters} iter, {tools_called} tools, \
                           {}/{} tok", usage.input_tokens, usage.output_tokens);
                if let Some(t) = last_text {
                    println!("\nasst (partial)> {t}");
                    history.push(("user".into(), input.to_string()));
                    history.push(("asst".into(), t));
                }
            }
            Err(e) => eprintln!("\nasst> ✗ error: {e:#}"),
        }
    }
    Ok(())
}
