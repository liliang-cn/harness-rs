//! Personal assistant agent — calendar/scheduling demo for the harness-rs framework.
//!
//! Five custom `#[tool]`s manipulate a JSON event store at `~/.harness-assistant/events.json`.
//! The agent loop wires them up and lets DeepSeek handle natural language scheduling.
//!
//! ```sh
//! DEEPSEEK_API_KEY=sk-... assistant "what's on my calendar this week?"
//! DEEPSEEK_API_KEY=sk-... assistant "schedule lunch with Sarah next Friday at 12:30"
//! DEEPSEEK_API_KEY=sk-... assistant "cancel the gym session this evening"
//! ```

use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use clap::Parser;
use harness::ToolError;
use harness::prelude::*;
use harness_context::with_profile;
use harness_core::Model;
use harness_core::UserProfile;
use harness_loop::ProfileGuide;
use harness_loop::{AgentLoop, Outcome};
use harness_models::OpenAiCompat;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

// =================================================================
// Storage — a flat JSON file under ~/.harness-assistant/
// =================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Event {
    id: String,
    title: String,
    start: DateTime<Utc>,
    duration_minutes: u32,
    #[serde(default)]
    notes: Option<String>,
}

fn store_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".harness-assistant")
        .join("events.json")
}

fn load_events() -> Vec<Event> {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_events(events: &[Event]) -> Result<(), ToolError> {
    let p = store_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::Exec(e.to_string()))?;
    }
    let json = serde_json::to_string_pretty(events).map_err(|e| ToolError::Exec(e.to_string()))?;
    std::fs::write(&p, json).map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(())
}

// =================================================================
// Todos — separate file, simpler shape than Events
// =================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Todo {
    id: String,
    title: String,
    #[serde(default)]
    due: Option<DateTime<Utc>>,
    #[serde(default = "default_priority")]
    priority: String, // "low" | "med" | "high"
    #[serde(default)]
    done: bool,
    #[serde(default)]
    notes: Option<String>,
}
fn default_priority() -> String {
    "med".into()
}

fn tasks_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".harness-assistant")
        .join("tasks.json")
}

fn load_tasks() -> Vec<Todo> {
    std::fs::read_to_string(tasks_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_tasks(tasks: &[Todo]) -> Result<(), ToolError> {
    let p = tasks_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::Exec(e.to_string()))?;
    }
    let json = serde_json::to_string_pretty(tasks).map_err(|e| ToolError::Exec(e.to_string()))?;
    std::fs::write(&p, json).map_err(|e| ToolError::Exec(e.to_string()))?;
    Ok(())
}

fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    // Accept either RFC3339 with timezone, or local-naive (assume Local).
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .and_then(|n| Local.from_local_datetime(&n).single())
                .map(|d| d.with_timezone(&Utc))
        })
}

// =================================================================
// Tools — 5 #[tool]s the agent can call
// =================================================================

/// Get the current wall-clock time. Always call this before interpreting relative
/// dates like "tomorrow", "next Friday", "this evening".
#[harness::tool(
    name = "current_time",
    risk = "read-only",
    schema = r#"{"type": "object", "properties": {}}"#
)]
async fn current_time(_args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let now_utc = Utc::now();

    // App-side policy: prefer the user's profile timezone over the system clock.
    // This is what makes the framework's `World.profile` actually do something.
    let tz_name = w.profile.tz.clone();
    let (iso_local, weekday, human, tz_source) = match tz_name
        .as_deref()
        .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
    {
        Some(tz) => {
            let local = now_utc.with_timezone(&tz);
            (
                local.to_rfc3339(),
                local.format("%A").to_string(),
                local.format("%Y-%m-%d %H:%M %Z").to_string(),
                format!("profile.tz={}", tz_name.as_deref().unwrap_or("?")),
            )
        }
        None => {
            let local = now_utc.with_timezone(&Local);
            (
                local.to_rfc3339(),
                local.format("%A").to_string(),
                local.format("%Y-%m-%d %H:%M %Z").to_string(),
                "system-clock".to_string(),
            )
        }
    };

    Ok(ToolResult {
        ok: true,
        content: json!({
            "iso_utc":     now_utc.to_rfc3339(),
            "iso_local":   iso_local,
            "weekday":     weekday,
            "human":       human,
            "timezone":    tz_source,
        }),
        trace: None,
    })
}

/// List events in a time range. Default range: now → +7 days.
#[harness::tool(
    name = "list_events",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "from": {"type": "string", "description": "ISO 8601 / RFC3339 start. Default: now."},
            "to":   {"type": "string", "description": "ISO 8601 / RFC3339 end. Default: now + 7 days."}
        }
    }"#
)]
async fn list_events(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let now = Utc::now();
    let from = args
        .get("from")
        .and_then(|v| v.as_str())
        .and_then(parse_iso)
        .unwrap_or(now);
    let to = args
        .get("to")
        .and_then(|v| v.as_str())
        .and_then(parse_iso)
        .unwrap_or_else(|| now + Duration::days(7));

    let mut hits: Vec<Event> = load_events()
        .into_iter()
        .filter(|e| e.start >= from && e.start <= to)
        .collect();
    hits.sort_by_key(|e| e.start);

    Ok(ToolResult {
        ok: true,
        content: json!({
            "range_from": from.to_rfc3339(),
            "range_to":   to.to_rfc3339(),
            "count":      hits.len(),
            "events":     hits,
        }),
        trace: None,
    })
}

/// Add a new event. Returns the assigned id.
#[harness::tool(
    name = "add_event",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "title":            {"type": "string"},
            "start":            {"type": "string", "description": "ISO 8601 / RFC3339 start time."},
            "duration_minutes": {"type": "integer", "default": 60, "minimum": 1},
            "notes":            {"type": "string"}
        },
        "required": ["title", "start"]
    }"#
)]
async fn add_event(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "assistant".into(),
            reason: "title required".into(),
        })?
        .to_string();
    let start_s =
        args.get("start")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "assistant".into(),
                reason: "start required".into(),
            })?;
    let start = parse_iso(start_s).ok_or_else(|| ToolError::InvalidArgs {
        name: "assistant".into(),
        reason: format!("could not parse start `{start_s}` — use RFC3339"),
    })?;
    let duration_minutes = args
        .get("duration_minutes")
        .and_then(|v| v.as_u64())
        .unwrap_or(60) as u32;
    let notes = args.get("notes").and_then(|v| v.as_str()).map(String::from);

    let mut events = load_events();
    let event = Event {
        id: Uuid::new_v4().to_string()[..8].to_string(),
        title,
        start,
        duration_minutes,
        notes,
    };
    events.push(event.clone());
    save_events(&events)?;

    Ok(ToolResult {
        ok: true,
        content: json!({"added": event}),
        trace: None,
    })
}

/// Cancel an event by id.
#[harness::tool(
    name = "cancel_event",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "Event id from list_events / add_event."}
        },
        "required": ["id"]
    }"#
)]
async fn cancel_event(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "assistant".into(),
            reason: "id required".into(),
        })?;
    let mut events = load_events();
    let before = events.len();
    events.retain(|e| e.id != id);
    let removed = before - events.len();
    if removed == 0 {
        return Ok(ToolResult {
            ok: false,
            content: json!({"error": format!("no event with id `{id}`")}),
            trace: None,
        });
    }
    save_events(&events)?;
    Ok(ToolResult {
        ok: true,
        content: json!({"cancelled_count": removed, "remaining": events.len()}),
        trace: None,
    })
}

/// Substring search by title. Case-insensitive.
#[harness::tool(
    name = "find_event",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "query": {"type": "string", "description": "Substring to match in event titles."}
        },
        "required": ["query"]
    }"#
)]
async fn find_event(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let q = args
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "assistant".into(),
            reason: "query required".into(),
        })?
        .to_lowercase();
    let mut hits: Vec<Event> = load_events()
        .into_iter()
        .filter(|e| e.title.to_lowercase().contains(&q))
        .collect();
    hits.sort_by_key(|e| e.start);
    Ok(ToolResult {
        ok: true,
        content: json!({"count": hits.len(), "events": hits}),
        trace: None,
    })
}

/// Move an existing event to a new start time. Optionally change duration.
#[harness::tool(
    name = "move_event",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "id":               {"type": "string"},
            "new_start":        {"type": "string", "description": "ISO 8601 / RFC3339 new start time."},
            "duration_minutes": {"type": "integer", "minimum": 1}
        },
        "required": ["id", "new_start"]
    }"#
)]
async fn move_event(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "assistant".into(),
            reason: "id required".into(),
        })?;
    let new_start_s = args
        .get("new_start")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "assistant".into(),
            reason: "new_start required".into(),
        })?;
    let new_start = parse_iso(new_start_s).ok_or_else(|| ToolError::InvalidArgs {
        name: "assistant".into(),
        reason: format!("could not parse `{new_start_s}`"),
    })?;
    let new_dur = args
        .get("duration_minutes")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    let mut events = load_events();
    let found = events.iter_mut().find(|e| e.id == id);
    let Some(e) = found else {
        return Ok(ToolResult {
            ok: false,
            content: json!({"error": format!("no event with id `{id}`")}),
            trace: None,
        });
    };
    let old_start = e.start;
    e.start = new_start;
    if let Some(d) = new_dur {
        e.duration_minutes = d;
    }
    let snapshot = e.clone();
    save_events(&events)?;
    Ok(ToolResult {
        ok: true,
        content: json!({"moved": snapshot, "from": old_start.to_rfc3339(), "to": new_start.to_rfc3339()}),
        trace: None,
    })
}

// ---------- Todos (todo list, separate from calendar events) ----------

/// Add a todo item.
#[harness::tool(
    name = "add_task",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {
            "title":    {"type": "string"},
            "due":      {"type": "string", "description": "Optional ISO 8601 due time."},
            "priority": {"type": "string", "enum": ["low", "med", "high"], "default": "med"},
            "notes":    {"type": "string"}
        },
        "required": ["title"]
    }"#
)]
async fn add_task(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "assistant".into(),
            reason: "title required".into(),
        })?
        .to_string();
    let due = args.get("due").and_then(|v| v.as_str()).and_then(parse_iso);
    let priority = args
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or("med")
        .to_string();
    let notes = args.get("notes").and_then(|v| v.as_str()).map(String::from);

    let mut tasks = load_tasks();
    let todo = Todo {
        id: Uuid::new_v4().to_string()[..8].to_string(),
        title,
        due,
        priority,
        done: false,
        notes,
    };
    tasks.push(todo.clone());
    save_tasks(&tasks)?;
    Ok(ToolResult {
        ok: true,
        content: json!({"added": todo}),
        trace: None,
    })
}

/// List todos. By default: open (not done), sorted high-priority + earliest-due first.
#[harness::tool(
    name = "list_tasks",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "include_done": {"type": "boolean", "default": false},
            "only_priority": {"type": "string", "enum": ["low", "med", "high"]}
        }
    }"#
)]
async fn list_tasks(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let include_done = args
        .get("include_done")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let only_pri = args
        .get("only_priority")
        .and_then(|v| v.as_str())
        .map(String::from);

    let pri_order = |p: &str| match p {
        "high" => 0,
        "med" => 1,
        "low" => 2,
        _ => 3,
    };
    let mut tasks: Vec<Todo> = load_tasks()
        .into_iter()
        .filter(|t| {
            (include_done || !t.done) && only_pri.as_deref().is_none_or(|op| t.priority == op)
        })
        .collect();
    tasks.sort_by_key(|t| {
        (
            pri_order(&t.priority),
            t.due.unwrap_or_else(|| Utc::now() + Duration::days(3650)),
        )
    });
    Ok(ToolResult {
        ok: true,
        content: json!({"count": tasks.len(), "tasks": tasks}),
        trace: None,
    })
}

/// Mark a todo done.
#[harness::tool(
    name = "complete_task",
    risk = "destructive",
    schema = r#"{
        "type": "object",
        "properties": {"id": {"type": "string"}},
        "required": ["id"]
    }"#
)]
async fn complete_task(args: Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let id = args
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            name: "assistant".into(),
            reason: "id required".into(),
        })?;
    let mut tasks = load_tasks();
    let found = tasks.iter_mut().find(|t| t.id == id);
    let Some(t) = found else {
        return Ok(ToolResult {
            ok: false,
            content: json!({"error": format!("no task with id `{id}`")}),
            trace: None,
        });
    };
    if t.done {
        return Ok(ToolResult {
            ok: true,
            content: json!({"already_done": &t.title}),
            trace: None,
        });
    }
    t.done = true;
    let snapshot = t.clone();
    save_tasks(&tasks)?;
    Ok(ToolResult {
        ok: true,
        content: json!({"completed": snapshot}),
        trace: None,
    })
}

// =================================================================
// Tool registration helper — picks up #[tool]-registered tools by name.
// =================================================================

fn collect_tools() -> Vec<Arc<dyn Tool>> {
    use harness_core::iter_macro_tools;
    let want = [
        "current_time",
        "list_events",
        "add_event",
        "cancel_event",
        "find_event",
        "move_event",
        "add_task",
        "list_tasks",
        "complete_task",
    ];
    iter_macro_tools()
        .filter(|t| want.contains(&t.name()))
        .collect()
}

// =================================================================
// CLI
// =================================================================

#[derive(Parser, Debug)]
#[command(
    name = "assistant",
    about = "Your personal scheduling assistant, powered by harness-rs."
)]
struct Cli {
    /// What to ask the assistant. Passed as a single string.
    #[arg(default_values_t = vec!["What's on my calendar today?".to_string()])]
    task: Vec<String>,

    /// Model tier: flash (fast/cheap) or pro (better reasoning).
    #[arg(long, default_value = "pro")]
    tier: String,

    /// Maximum agent loop iterations.
    #[arg(long, default_value_t = 8)]
    max_iters: u32,

    /// User name to surface to the model (defaults to $HARNESS_USER_NAME).
    #[arg(long)]
    name: Option<String>,

    /// User timezone, IANA id like `Asia/Shanghai` (defaults to $HARNESS_USER_TZ).
    #[arg(long)]
    tz: Option<String>,

    /// User locale BCP-47, like `zh-CN` (defaults to $HARNESS_USER_LOCALE).
    #[arg(long)]
    locale: Option<String>,

    /// Interactive REPL: read a prompt from stdin in a loop, keeping
    /// conversation context across turns. Ignores positional `task`.
    /// Exit with `bye` / `quit` / `exit` / Ctrl-D.
    #[arg(long)]
    repl: bool,

    /// Run the morning brief — auto-summarise today's events + open
    /// high-priority todos. Designed to be triggered by an external
    /// scheduler (e.g. `harness-daemon`). Overrides positional `task`.
    #[arg(long)]
    brief: bool,

    /// Stream live progress (model calls, tool calls, tool results) to stderr
    /// while the agent runs. Also enabled via `HARNESS_PROGRESS=1`.
    #[arg(long)]
    progress: bool,

    /// Record a JSONL session log to this path for offline replay /
    /// post-mortem analysis via `harness trace --verbose`.
    #[arg(long)]
    record: Option<PathBuf>,

    /// Path to a JSONL long-term memory file. When set, the agent recalls
    /// matching prior facts at session start (`MemoryGuide`) and a
    /// synthesizer model distils each session into 1-3 durable facts at
    /// the end (`MemorySynthesizer`).
    #[arg(long)]
    memory: Option<PathBuf>,

    /// Cheap model id used by `MemorySynthesizer`. Defaults to
    /// `deepseek-v4-flash` or `HARNESS_SYNTH_MODEL` env. Uses the same
    /// `HARNESS_BASE_URL` / `HARNESS_API_KEY` as the main model.
    #[arg(long)]
    synth_model: Option<String>,
}

/// App-side policy for "where does the user profile come from?"
/// The harness-rs framework provides the SLOT (`World.profile`) and the
/// INJECTION mechanism (`ProfileGuide`). It does NOT read your home dir.
/// Here we pick: CLI flags first, env vars second, nothing else.
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

const BRIEF_PROMPT: &str = "\
Compose my morning brief. Steps:\n\
1. Call `current_time` to ground today's date and day-of-week.\n\
2. Call `list_events` covering the next ~36 hours (today + tomorrow morning).\n\
3. Call `list_tasks` (open only, highest priority first) — ignore completed.\n\
4. Format ONE plain-text reply, no markdown headers, max 200 words, structured as:\n\
\n\
   早上好 {name}!（or 'Good morning' if locale != zh)\n\
   今天 ({date}, {weekday}):\n\
     • {time} — {event title}\n\
     ...\n\
   明天预告:\n\
     • {time} — {event title} (or '无' if none)\n\
   高优先级 todos:\n\
     • {title}{ — due {time} if set}\n\
     ...\n\
\n\
If nothing scheduled and no high-priority todos: say so cheerfully in one sentence. \
Localise day names and weekday to the user's locale if set.";

const SYSTEM_PROMPT: &str = "\
You are a personal scheduling + todo assistant. Your built-in clock is stale; \
whenever the user uses relative time (today, tomorrow, this evening, next Friday, etc.), \
call the `current_time` tool FIRST to ground yourself, then act.\n\
\n\
You have two stores: calendar events (with start time + duration) and todo tasks \
(with optional due time + priority low/med/high). Pick the right one. \
\"Schedule a meeting\" → event. \"Remind me to fix the bug\" → task.\n\
\n\
For modifications (add / cancel / move / complete), perform the change with the appropriate \
tool, then reply with ONE short sentence confirming what you did with human-readable time.\n\
For read-only requests (list / find), reply with a short bullet list (title + when + id).\n\
\n\
In a multi-turn conversation, the user may refer to previously created items by description \
(\"that Rene meeting\", \"the bug fix\"). Use the `find_event` or `list_tasks` tool to resolve them.";

/// Wrap a user request with the system prompt. In REPL mode, also prepends
/// abbreviated history so the model can resolve "it" / "that meeting".
fn build_task_description(user_request: &str, history: &[(String, String)]) -> String {
    let mut s = SYSTEM_PROMPT.to_string();
    if !history.is_empty() {
        s.push_str("\n\nPrior conversation (oldest first):\n");
        for (role, text) in history {
            // Trim each line to keep prompt short.
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
        "→ assistant\n  model:     {} ({}/{})\n  tools:     {} registered\n  store:     {}",
        info.handle,
        info.provider,
        info.model,
        tools.len(),
        store_path().display(),
    );
    if profile.name.is_some() || profile.tz.is_some() {
        println!("  profile:   {}", profile.summary_line());
    }
    if cli.repl {
        println!("  mode:      REPL (Ctrl-D / bye / quit / exit to leave)");
    }
    let progress = cli.progress
        || std::env::var("HARNESS_PROGRESS")
            .map(|v| !v.is_empty() && v != "0" && v.to_lowercase() != "false")
            .unwrap_or(false);
    if progress {
        println!("  progress:  live (stderr)");
    }
    if let Some(p) = &cli.record {
        println!("  recording: {}", p.display());
    }
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
            progress,
            cli.record,
            memory,
            synth_model_id,
        )
        .await
    } else {
        let user_request = if cli.brief {
            BRIEF_PROMPT.to_string()
        } else {
            cli.task.join(" ")
        };
        run_once(
            &base_url,
            model_id,
            api_key,
            tools,
            profile,
            cli.max_iters,
            user_request,
            progress,
            cli.record,
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
    progress: bool,
    record: Option<PathBuf>,
    memory: Option<Arc<dyn harness_core::Memory>>,
    synth_model_id: String,
) -> anyhow::Result<()> {
    let model = OpenAiCompat::with_key(base_url.to_string(), model_id, api_key.clone());
    let mut loop_ = AgentLoop::new(model).with_guide(Arc::new(ProfileGuide));
    for t in tools {
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
                .with_source("personal-assistant")
                .with_max_facts(3),
        );
        loop_ = loop_.with_hook(synth.clone() as Arc<dyn harness_core::Hook>);
        synth_handle = Some(synth);
    }
    if progress {
        loop_ = loop_.with_hook(Arc::new(harness_loop::LiveProgressHook::new()));
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
            iters, last_text, ..
        } => {
            eprintln!("✗ budget exhausted after {iters} iteration(s)");
            if let Some(t) = last_text {
                eprintln!("\n— forced-synthesis answer (tool-less) —\n{t}");
            }
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
    progress: bool,
    record: Option<PathBuf>,
    memory: Option<Arc<dyn harness_core::Memory>>,
    synth_model_id: String,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    let mut history: Vec<(String, String)> = Vec::new();
    const EXIT_WORDS: &[&str] = &["bye", "quit", "exit", ":q", "\\q"];
    const MAX_HISTORY_TURNS: usize = 20;

    loop {
        stdout.write_all(b"\nyou> ").await?;
        stdout.flush().await?;

        let Some(line) = stdin.next_line().await? else {
            // EOF (Ctrl-D)
            println!("\nbye.");
            break;
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if EXIT_WORDS.contains(&input.to_lowercase().as_str()) {
            println!("bye.");
            break;
        }

        // Cap history to avoid context blowup
        let history_for_call = if history.len() > MAX_HISTORY_TURNS {
            &history[history.len() - MAX_HISTORY_TURNS..]
        } else {
            &history[..]
        };

        let model = OpenAiCompat::with_key(base_url.to_string(), model_id, api_key.clone());
        let mut loop_ = AgentLoop::new(model).with_guide(Arc::new(ProfileGuide));
        for t in tools.iter().cloned() {
            loop_ = loop_.with_tool(t);
        }
        let mut turn_synth: Option<Arc<harness_loop::MemorySynthesizer>> = None;
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
                    .with_source("personal-assistant")
                    .with_max_facts(3),
            );
            loop_ = loop_.with_hook(synth.clone() as Arc<dyn harness_core::Hook>);
            turn_synth = Some(synth);
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
            description: build_task_description(input, history_for_call),
            source: None,
            deadline: None,
        };

        match loop_.run_with_max_iters(task, &mut world, max_iters).await {
            Ok(Outcome::Done { text, iters, .. }) => {
                let response = text.unwrap_or_else(|| "(no response)".into());
                println!("\nasst ({iters} iter)> {response}");
                history.push(("user".into(), input.to_string()));
                history.push(("asst".into(), response));
            }
            Ok(Outcome::BudgetExhausted {
                iters, last_text, ..
            }) => {
                eprintln!("\nasst> ✗ ran out of budget after {iters} iterations.");
                if let Some(t) = last_text {
                    println!("\nasst (forced-synthesis)> {t}");
                }
            }
            Err(e) => eprintln!("\nasst> ✗ error: {e:#}"),
        }
        if let Some(s) = &turn_synth {
            s.flush_pending().await;
        }
    }
    Ok(())
}
