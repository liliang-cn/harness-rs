//! Investor research agent — built on harness-rs.
//!
//! 4 custom `#[tool]`s let an LLM autonomously navigate public financial info:
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
use harness::ToolError;
use harness::prelude::*;
use harness_context::with_profile;
use harness_core::{Model, UserProfile};
use harness_loop::{AgentLoop, Outcome, ProfileGuide};
use harness_models::OpenAiCompat;
// Force-link harness-rs-tools-web so its `#[tool]` registrations land in
// `inventory` and `iter_macro_tools()` picks up `web_search` + `web_fetch`.
use harness_tools_web as _;
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
    id: String,
    topic: String,
    content: String,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    saved_at: DateTime<Utc>,
}

fn store_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".harness-investor")
}

fn notes_path() -> PathBuf {
    store_dir().join("notes.json")
}

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
// Tool 0: current_time — grounds "today / this week / recent" queries
// =================================================================

/// Return the current wall-clock time. Always call this first for any query
/// that mentions "today / yesterday / this week / recent / latest / 最近 / 今天".
/// Uses World.profile.tz when set, falls back to UTC.
#[harness::tool(
    name = "current_time",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
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
    }"#
)]
async fn save_note(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let topic = args
        .get("topic")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "investor".into(),
            reason: "topic required".into(),
        })?
        .to_string();
    let content = args
        .get("content")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "investor".into(),
            reason: "content required".into(),
        })?
        .to_string();
    let source_url = args
        .get("source_url")
        .and_then(|v| v.as_str())
        .map(String::from);
    let tags: Vec<String> = args
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut notes = load_notes();
    let note = Note {
        id: Uuid::new_v4().to_string()[..8].to_string(),
        topic,
        content,
        source_url,
        tags,
        saved_at: Utc::now(),
    };
    notes.push(note.clone());
    save_notes(&notes)?;
    Ok(ToolResult {
        ok: true,
        content: json!({"saved": note}),
        trace: None,
    })
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
    }"#
)]
async fn list_notes(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let tag = args
        .get("tag")
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase());
    let since = args
        .get("since")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;

    let mut hits: Vec<Note> = load_notes()
        .into_iter()
        .filter(|n| {
            tag.as_deref()
                .is_none_or(|t| n.tags.iter().any(|x| x.to_lowercase().contains(t)))
        })
        .filter(|n| since.is_none_or(|s| n.saved_at >= s))
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
    let want = [
        "current_time",
        "web_search",
        "web_fetch",
        "save_note",
        "list_notes",
    ];
    iter_macro_tools()
        .filter(|t| want.contains(&t.name()))
        .collect()
}

// =================================================================
// Prompt
// =================================================================

const SYSTEM_PROMPT: &str = "\
You are an investment-research assistant. You retrieve PUBLIC information from \
the open web and SEC filings; you DO NOT give financial advice. Every factual \
claim in your reply must come from a tool result you can cite — if you don't \
have a source for a specific claim, mark that claim as UNKNOWN (do not omit it, \
do not guess).\n\
\n\
Workflow:\n\
0. If the question mentions any relative time (today, this week, recent, latest, 最新, 最近, 今天, 上周, 本季度, etc.) — call `current_time` FIRST to ground yourself. Otherwise skip step 0.\n\
1. Call `web_search` with a precise query. Read the top hits' titles + snippets.\n\
2. Call `web_fetch` on the 1-3 most relevant URLs to get the actual content.\n\
3. Cross-check: if numbers vary across sources, list both with attribution.\n\
4. Call `save_note` for material findings, ALWAYS with `source_url` and tags.\n\
5. Reply concisely with the answer + a bullet list of source URLs.\n\
\n\
CONCLUSION RULES — non-negotiable:\n\
- You MUST emit a final answer message (text, no tool calls) by your second-to-last iteration. \
  Never let the loop hit the iteration budget without a written conclusion.\n\
- If a `web_search` returns 0 results twice in a row for the same intent, STOP retrying and broaden the query, switch sources, or commit to a partial answer marking the missing piece as UNKNOWN.\n\
- If a `web_fetch` returns HTTP 401 / 403 / 503 (blocked / rate-limited / down), DO NOT retry that URL. Move to a different source.\n\
- After ≤3 failed fetch attempts, do not keep searching — synthesise what you have. A partial answer with explicit UNKNOWN fields ranks higher than budget exhaustion.\n\
- The success criterion is: a final text reply containing (a) the asked-for answer or an explicit UNKNOWN marker per missing fact, and (b) source URLs for every claim that is not UNKNOWN.\n\
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
#[command(
    name = "investor",
    about = "Autonomous investment-research agent (harness-rs)."
)]
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

    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    tz: Option<String>,
    #[arg(long)]
    locale: Option<String>,

    /// Record a session log for replay/debugging.
    #[arg(long)]
    record: Option<PathBuf>,

    /// Stream live progress (model calls, tool calls, tool results) to stderr
    /// while the agent runs. Also enabled via `HARNESS_PROGRESS=1`.
    #[arg(long)]
    progress: bool,

    /// Path to a JSONL long-term memory file. When set, the agent recalls
    /// matching prior facts at session start (`MemoryGuide`) and a
    /// synthesizer model distils each session into 1-3 durable facts at
    /// the end (`MemorySynthesizer`).
    #[arg(long)]
    memory: Option<PathBuf>,

    /// Cheap model id used by `MemorySynthesizer` to distil sessions.
    /// Defaults to `deepseek-v4-flash` (or `HARNESS_SYNTH_MODEL` env var
    /// when set). Routed against the same `HARNESS_BASE_URL` /
    /// `HARNESS_API_KEY` as the main model.
    #[arg(long)]
    synth_model: Option<String>,
}

fn build_profile(cli: &Cli) -> UserProfile {
    UserProfile {
        name: cli
            .name
            .clone()
            .or_else(|| std::env::var("HARNESS_USER_NAME").ok()),
        tz: cli
            .tz
            .clone()
            .or_else(|| std::env::var("HARNESS_USER_TZ").ok()),
        locale: cli
            .locale
            .clone()
            .or_else(|| std::env::var("HARNESS_USER_LOCALE").ok()),
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
            println!(
                "[{}] {} ({})",
                n.id,
                n.topic,
                n.saved_at.format("%Y-%m-%d %H:%M UTC")
            );
            println!("       tags: {}", n.tags.join(", "));
            if let Some(url) = &n.source_url {
                println!("       src:  {url}");
            }
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

    // Env-var driven overrides — fall back to DeepSeek defaults.
    let api_key = std::env::var("HARNESS_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .map_err(|_| anyhow::anyhow!("set HARNESS_API_KEY or DEEPSEEK_API_KEY"))?;
    let base_url = std::env::var("HARNESS_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
    let default_model_id = match cli.tier.as_str() {
        "flash" => "deepseek-v4-flash",
        _ => "deepseek-v4-pro",
    };
    let model_id_owned =
        std::env::var("HARNESS_MODEL").unwrap_or_else(|_| default_model_id.to_string());
    let model_id: &str = &model_id_owned;
    let info_model = OpenAiCompat::with_key(base_url.clone(), model_id, api_key.clone());
    let info = info_model.info();
    drop(info_model);

    let tools = collect_tools();
    let profile = build_profile(&cli);

    println!(
        "→ investor-bot\n  model:     {} ({}/{})\n  tools:     {} registered\n  store:     {}",
        info.handle,
        info.provider,
        info.model,
        tools.len(),
        notes_path().display(),
    );
    if profile.name.is_some() || profile.tz.is_some() {
        println!("  profile:   {}", profile.summary_line());
    }
    if let Some(p) = &cli.record {
        println!("  recording: {}", p.display());
    }
    let progress = cli.progress
        || std::env::var("HARNESS_PROGRESS")
            .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
            .unwrap_or(false);
    if progress {
        println!("  progress:  live (stderr)");
    }

    // Memory layer (opt-in). When --memory is set, install MemoryGuide (recall
    // at session start) + MemorySynthesizer (distil session into ≤3 atomic
    // facts at end). Synth model is intentionally a separate, cheaper one.
    let memory: Option<Arc<dyn harness_core::Memory>> = match &cli.memory {
        Some(p) => Some(Arc::new(
            harness_context::FileMemory::open(p).map_err(|e| anyhow::anyhow!("memory: {e}"))?,
        )),
        None => None,
    };
    let synth_model_id = cli
        .synth_model
        .clone()
        .or_else(|| std::env::var("HARNESS_SYNTH_MODEL").ok())
        .unwrap_or_else(|| "deepseek-v4-flash".into());
    if let Some(p) = &cli.memory {
        println!("  memory:    {} (synth: {})", p.display(), synth_model_id);
    }
    println!();

    if cli.repl {
        run_repl(
            &base_url,
            model_id,
            api_key,
            tools,
            profile,
            cli.max_iters,
            cli.record,
            progress,
            memory,
            synth_model_id,
        )
        .await
    } else {
        run_once(
            &base_url,
            model_id,
            api_key,
            tools,
            profile,
            cli.max_iters,
            cli.task.join(" "),
            cli.record,
            progress,
            memory,
            synth_model_id,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
    base_url: &str,
    model_id: &str,
    api_key: String,
    tools: Vec<Arc<dyn Tool>>,
    profile: UserProfile,
    max_iters: u32,
    user_request: String,
    record: Option<PathBuf>,
    progress: bool,
    memory: Option<Arc<dyn harness_core::Memory>>,
    synth_model_id: String,
) -> anyhow::Result<()> {
    let model = OpenAiCompat::with_key(base_url.to_string(), model_id, api_key.clone());
    let mut loop_ = AgentLoop::new(model).with_guide(Arc::new(ProfileGuide));
    let mut synth_handle: Option<Arc<harness_loop::MemorySynthesizer>> = None;
    if let Some(mem) = &memory {
        loop_ = loop_.with_guide(Arc::new(
            harness_loop::MemoryGuide::new(mem.clone()).with_top_k(5),
        ));
        let synth_model: Arc<dyn harness_core::Model> = Arc::new(OpenAiCompat::with_key(
            base_url.to_string(),
            synth_model_id.clone(),
            api_key.clone(),
        ));
        let synth = Arc::new(
            harness_loop::MemorySynthesizer::new(mem.clone(), synth_model)
                .with_source("investor-bot")
                .with_max_facts(3),
        );
        loop_ = loop_.with_hook(synth.clone() as Arc<dyn harness_core::Hook>);
        synth_handle = Some(synth);
    }
    if progress {
        loop_ = loop_.with_hook(Arc::new(harness_loop::LiveProgressHook::new()));
    }
    for t in tools {
        loop_ = loop_.with_tool(t);
    }
    if let Some(p) = record {
        let rec =
            harness_loop::SessionRecorder::new(&p).map_err(|e| anyhow::anyhow!("recorder: {e}"))?;
        loop_ = loop_.with_hook(Arc::new(rec));
    }
    let mut world = with_profile(".", profile);
    let task = Task {
        description: build_task_description(&user_request, &[]),
        source: None,
        deadline: None,
    };
    match loop_
        .run_with_max_iters(task, &mut world, max_iters)
        .await?
    {
        Outcome::Done { text, iters, .. } => {
            println!("✓ done after {iters} iteration(s)\n");
            if let Some(t) = text {
                println!("{t}");
            }
        }
        Outcome::BudgetExhausted {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        }
        | Outcome::Stuck {
            iters,
            last_text,
            tools_called,
            usage,
            ..
        } => {
            eprintln!(
                "✗ stopped after {iters} iter(s), {tools_called} tool call(s), \
                       {} in / {} out tokens",
                usage.input_tokens, usage.output_tokens
            );
            if let Some(t) = last_text {
                eprintln!("\n— last assistant message before stopping —\n{t}");
            }
            eprintln!(
                "\n→ partial findings preserved in {}. `investor --list` to recall.",
                notes_path().display()
            );
            if let Some(s) = &synth_handle {
                s.flush_pending().await;
            }
            std::process::exit(2);
        }
    }
    if let Some(s) = synth_handle {
        s.flush_pending().await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_repl(
    base_url: &str,
    model_id: &str,
    api_key: String,
    tools: Vec<Arc<dyn Tool>>,
    profile: UserProfile,
    max_iters: u32,
    record: Option<PathBuf>,
    progress: bool,
    memory: Option<Arc<dyn harness_core::Memory>>,
    synth_model_id: String,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    let mut history: Vec<(String, String)> = Vec::new();
    const EXIT: &[&str] = &["bye", "quit", "exit", ":q"];

    loop {
        stdout.write_all(b"\nyou> ").await?;
        stdout.flush().await?;
        let Some(line) = stdin.next_line().await? else {
            println!("\nbye.");
            break;
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if EXIT.contains(&input.to_lowercase().as_str()) {
            println!("bye.");
            break;
        }

        // Build a real Turn-shaped seed so the Compactor can see + shrink history.
        // (Previously: history was stringified into task.description and bypassed
        // the compactor entirely — see audit issue #2.)
        let seed: Vec<harness_core::Turn> = history
            .iter()
            .take_while(|_| true)
            .map(|(role, text)| {
                let role = match role.as_str() {
                    "user" => harness_core::TurnRole::User,
                    _ => harness_core::TurnRole::Assistant,
                };
                harness_core::Turn {
                    role,
                    blocks: vec![harness_core::Block::Text(text.clone())],
                }
            })
            .collect();

        let model = OpenAiCompat::with_key(base_url.to_string(), model_id, api_key.clone());
        let mut loop_ = AgentLoop::new(model).with_guide(Arc::new(ProfileGuide));
        for t in tools.iter().cloned() {
            loop_ = loop_.with_tool(t);
        }
        let mut synth_handle: Option<Arc<harness_loop::MemorySynthesizer>> = None;
        if let Some(mem) = &memory {
            loop_ = loop_.with_guide(Arc::new(
                harness_loop::MemoryGuide::new(mem.clone()).with_top_k(5),
            ));
            let synth_model: Arc<dyn harness_core::Model> = Arc::new(OpenAiCompat::with_key(
                base_url.to_string(),
                synth_model_id.clone(),
                api_key.clone(),
            ));
            let synth = Arc::new(
                harness_loop::MemorySynthesizer::new(mem.clone(), synth_model)
                    .with_source("investor-bot")
                    .with_max_facts(3),
            );
            loop_ = loop_.with_hook(synth.clone() as Arc<dyn harness_core::Hook>);
            synth_handle = Some(synth);
        }
        if progress {
            loop_ = loop_.with_hook(Arc::new(harness_loop::LiveProgressHook::new()));
        }
        if let Some(p) = &record {
            let rec = harness_loop::SessionRecorder::new(p)
                .map_err(|e| anyhow::anyhow!("recorder: {e}"))?;
            loop_ = loop_.with_hook(Arc::new(rec));
        }
        let mut world = with_profile(".", profile.clone());
        let task = Task {
            // Single-turn description only — no history stuffing.
            description: format!("{SYSTEM_PROMPT}\n\n[user] {input}"),
            source: None,
            deadline: None,
        };
        match loop_
            .run_with_seed_history(task, seed, &mut world, max_iters)
            .await
        {
            Ok(Outcome::Done { text, iters, .. }) => {
                let response = text.unwrap_or_else(|| "(no response)".into());
                println!("\nasst ({iters} iter)> {response}");
                history.push(("user".into(), input.to_string()));
                history.push(("asst".into(), response));
            }
            Ok(Outcome::BudgetExhausted {
                iters,
                last_text,
                tools_called,
                usage,
                ..
            })
            | Ok(Outcome::Stuck {
                iters,
                last_text,
                tools_called,
                usage,
                ..
            }) => {
                eprintln!(
                    "\nasst> ✗ stopped after {iters} iter, {tools_called} tools, \
                           {}/{} tok",
                    usage.input_tokens, usage.output_tokens
                );
                if let Some(t) = last_text {
                    println!("\nasst (partial)> {t}");
                    history.push(("user".into(), input.to_string()));
                    history.push(("asst".into(), t));
                }
            }
            Err(e) => eprintln!("\nasst> ✗ error: {e:#}"),
        }
        // After every REPL turn, await any spawned synth tasks so memory
        // is on disk before the next turn could try to recall.
        if let Some(s) = &synth_handle {
            s.flush_pending().await;
        }
    }
    Ok(())
}
