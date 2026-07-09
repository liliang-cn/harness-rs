//! `cap-tui` — a standalone ratatui front-end for the CAP core.
//!
//! Same agent, different skin: a full-screen terminal UI with a scrolling
//! conversation, a live-streaming reply, a tool-activity feed, and an input box.
//! It shares 100% of its brain with the `cap` CLI via the `cap` library crate —
//! the only difference is this file's UI hook, which bridges the (sync, !Send)
//! agent loop to the render loop over channels.
//!
//! Architecture: the agent runs on its own OS thread (its futures are !Send), a
//! `TuiHook` forwards tokens/tool-calls to the main thread over an `mpsc`
//! channel, and the main thread owns the terminal + input. Runs in YOLO mode
//! (the CLI is where you get the approval gate).

use cap::agent::{LoopParts, build_loop, cap_home, resolve_endpoint};
use cap::sensor::LspSensor;
use cap::session::Session;
use cap::tools::{HashRead, TaskTool};
use clap::Parser;
use crossterm::event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{execute, terminal};
use harness_context::{FileMemory, default_world};
use harness_core::{DynModel, Event, Hook, HookOutcome, Memory, Model, Skill, Task, World};
use harness_cortexdb::CortexdbMemory;
use harness_experience::ExperienceRecorder;
use harness_loop::Outcome;
use harness_mcp_client::McpClient;
use harness_models::OpenAiCompat;
use harness_tools_fs::{Glob, Grep, ListDir};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// CAP TUI — a ratatui coding agent (YOLO). Core is the `cap` crate.
#[derive(Parser)]
#[command(name = "cap-tui", version, about, long_about = None)]
struct Cli {
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Continue the most recent session for this workspace.
    #[arg(short = 'c', long = "continue")]
    cont: bool,
    /// Resume a specific session by id or path.
    #[arg(long, value_name = "PATH|ID")]
    resume: Option<String>,
    /// Use (or create) a named session.
    #[arg(long, value_name = "NAME")]
    session: Option<String>,
}

/// Messages from the UI thread to the agent thread.
enum Ctrl {
    Prompt(String),
    /// Branch off into a fresh session (reset the agent's seed + transcript).
    NewSession,
    /// Switch to a stored session by id (reload its transcript into the agent).
    Resume(String),
}

/// Messages from the agent thread to the UI thread.
enum UiEvent {
    Token(String),
    Tool(String),
    /// The run finished; carries the authoritative final reply (or an `[error]`
    /// string), so a non-streamed reply or a failure is always shown.
    Done(String),
    /// Replace the transcript view (e.g. after `/resume`): (role, text) pairs.
    Load(Vec<(String, String)>),
    Fatal(String),
}

/// The UI hook installed into the agent loop: forwards streamed tokens and
/// tool-activity lines to the render thread. `Mutex` makes the `Sender` `Sync`
/// (a `Hook` must be `Send + Sync`). YOLO — never gates.
struct TuiHook {
    tx: Mutex<Sender<UiEvent>>,
}

impl Hook for TuiHook {
    fn name(&self) -> &str {
        "tui-ui"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::ModelTokenDelta { .. } | Event::PreToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        let tx = self.tx.lock().unwrap();
        match ev {
            Event::ModelTokenDelta { text } => {
                let _ = tx.send(UiEvent::Token(text.to_string()));
            }
            Event::PreToolUse { action } => {
                let path = action.args["path"].as_str().unwrap_or("");
                let extra = if path.is_empty() {
                    String::new()
                } else {
                    format!(" {path}")
                };
                let _ = tx.send(UiEvent::Tool(format!("{}{extra}", action.tool)));
            }
            _ => {}
        }
        HookOutcome::Allow
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = cli
        .workspace
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    // Resolve the session up front (errors before we touch the terminal).
    let sess = if let Some(r) = &cli.resume {
        cap::session::load(r)?
    } else if let Some(name) = &cli.session {
        Session::named(name, &root)
    } else if cli.cont {
        cap::session::latest_for(&root).unwrap_or_else(|| Session::new(&root))
    } else {
        Session::new(&root)
    };
    // Fail fast on missing creds, and grab the model name for the status bar.
    let (_, model_id, _) = resolve_endpoint()?;
    let session_id = sess.id.clone();

    // Channels between the UI thread and the agent thread.
    let (prompt_tx, prompt_rx) = std::sync::mpsc::channel::<Ctrl>();
    let (ui_tx, ui_rx) = std::sync::mpsc::channel::<UiEvent>();

    // Agent thread: owns the !Send loop on its own current-thread runtime.
    let agent_root = root.clone();
    let agent = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build agent runtime");
        rt.block_on(agent_loop(agent_root, sess, prompt_rx, ui_tx));
    });

    // UI thread owns the terminal.
    let res = run_ui(&model_id, &session_id, prompt_tx, ui_rx);

    // The agent thread exits when prompt_tx drops (UI returns). Join best-effort.
    let _ = agent.join();
    res
}

/// The agent-side loop: build the model + loop once, then serve prompts.
async fn agent_loop(
    root: PathBuf,
    mut sess: Session,
    prompt_rx: Receiver<Ctrl>,
    ui_tx: Sender<UiEvent>,
) {
    macro_rules! fatal {
        ($($a:tt)*) => {{ let _ = ui_tx.send(UiEvent::Fatal(format!($($a)*))); return; }};
    }

    let (base, model_id, key) = match resolve_endpoint() {
        Ok(v) => v,
        Err(e) => fatal!("endpoint: {e}"),
    };
    let worker_id = std::env::var("CAP_WORKER_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| model_id.clone());
    let planner: Arc<dyn Model> = Arc::new(OpenAiCompat::with_key(
        base.clone(),
        model_id.clone(),
        key.clone(),
    ));
    let worker: Arc<dyn Model> = if worker_id == model_id {
        planner.clone()
    } else {
        Arc::new(OpenAiCompat::with_key(base, worker_id, key))
    };

    let memory: Arc<dyn Memory> =
        match CortexdbMemory::connect_stdio("cortexdb-mcp-stdio", &[]).await {
            Ok(m) => Arc::new(m.with_namespace("cap")),
            Err(_) => match FileMemory::open(cap_home().join("experience.jsonl")) {
                Ok(m) => Arc::new(m),
                Err(e) => fatal!("memory: {e}"),
            },
        };
    let recorder = ExperienceRecorder::new(memory);

    let task_tool = TaskTool {
        model: worker,
        tools: vec![
            Arc::new(HashRead),
            Arc::new(ListDir),
            Arc::new(Grep),
            Arc::new(Glob),
        ],
    };
    let skills_dir = cap_home().join("skills");
    let _ = std::fs::create_dir_all(&skills_dir);

    // Optional LSP self-correction sensor (CAP_LSP) and external MCP tools
    // (CAP_MCP) — same env wiring as the CLI. `_mcp` stays alive for the session.
    let lsp = std::env::var("CAP_LSP")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| LspSensor {
            id: "lsp".into(),
            cmd: s.split_whitespace().map(|x| x.to_string()).collect(),
            session: tokio::sync::OnceCell::new(),
        });
    let (mcp_tools, mcp_desc, _mcp) = match std::env::var("CAP_MCP")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        Some(spec) => {
            let parts: Vec<String> = spec.split_whitespace().map(|s| s.to_string()).collect();
            let args: Vec<&str> = parts[1..].iter().map(|s| s.as_str()).collect();
            match McpClient::connect_stdio(&parts[0], &args).await {
                Ok(client) => {
                    let tools = client.tools();
                    let d = format!("{} ({} tools)", parts[0], tools.len());
                    (tools, d, Some(client))
                }
                Err(e) => (vec![], format!("failed: {e}"), None),
            }
        }
        None => (vec![], "off".into(), None),
    };
    let lsp_desc = lsp
        .as_ref()
        .map(|s| s.cmd.join(" "))
        .unwrap_or_else(|| "off".into());

    let loop_ = build_loop(
        DynModel(planner),
        LoopParts {
            ui_hook: Arc::new(TuiHook {
                tx: Mutex::new(ui_tx.clone()),
            }),
            task_tool,
            trace_hook: recorder.tool_trace_hook(),
            exp_guide: Arc::new(recorder.guide()),
            lsp,
            mcp_tools,
            skills_dir,
        },
    );
    let _ = ui_tx.send(UiEvent::Tool(format!(
        "ready · mcp: {mcp_desc} · lsp: {lsp_desc}"
    )));

    let mut world = default_world(root.clone());
    let mut seed = sess.seed();
    while let Ok(ctrl) = prompt_rx.recv() {
        let prompt = match ctrl {
            Ctrl::Prompt(p) => p,
            Ctrl::NewSession => {
                sess = Session::new(&root);
                seed.clear();
                continue;
            }
            Ctrl::Resume(id) => {
                match cap::session::load(&id) {
                    Ok(s) => {
                        sess = s;
                        seed = sess.seed();
                        let turns: Vec<(String, String)> = sess
                            .turns
                            .iter()
                            .map(|t| (t.role.clone(), t.text.clone()))
                            .collect();
                        let _ = ui_tx.send(UiEvent::Load(turns));
                    }
                    Err(e) => {
                        let _ = ui_tx.send(UiEvent::Fatal(format!("resume {id}: {e}")));
                    }
                }
                continue;
            }
        };
        let task = Task {
            description: prompt.clone(),
            source: None,
            deadline: None,
        };
        let reply = match loop_
            .run_with_seed_history(task, seed.clone(), &mut world, 30)
            .await
        {
            Ok(Outcome::Done { text, .. }) => text.unwrap_or_default(),
            Ok(Outcome::BudgetExhausted { last_text, .. })
            | Ok(Outcome::Stuck { last_text, .. }) => last_text.unwrap_or_default(),
            Err(e) => format!("[error] {e}"),
        };
        recorder.record(prompt.clone(), reply.clone()).await;
        sess.push("user", &prompt);
        sess.push("assistant", &reply);
        let _ = sess.save();
        seed = sess.seed();
        let _ = ui_tx.send(UiEvent::Done(reply));
    }
}

/// A second-level menu: choices for a command that takes an argument.
#[derive(Clone, Copy)]
enum SubKind {
    Resume,
    Skill,
}
struct SubMenu {
    title: String,
    kind: SubKind,
    items: Vec<(String, String)>, // (label shown, value used)
}

/// State the render loop draws.
struct App {
    lines: Vec<(char, String)>, // (kind, text): u=user, a=assistant, t=tool, e=error
    current: String,            // in-progress assistant text
    input: String,
    running: bool,
    menu_idx: usize,          // highlighted item in whichever popup is open
    submenu: Option<SubMenu>, // active second-level menu, if any
}

/// Slash-command completions for the current input (empty unless the input is a
/// bare `/word` with no argument yet).
fn candidates(input: &str) -> Vec<(&'static str, &'static str)> {
    let t = input.trim();
    if t.starts_with('/') && !t.contains(char::is_whitespace) {
        cap::commands::matching(t)
    } else {
        vec![]
    }
}

/// Build the second-level menu for a command, if it has one.
fn open_submenu(name: &str) -> Option<SubMenu> {
    match name {
        "/resume" => Some(SubMenu {
            title: "resume session".into(),
            kind: SubKind::Resume,
            items: cap::session::list()
                .into_iter()
                .map(|s| (format!("{}  ({} turns)", s.id, s.turns.len()), s.id))
                .collect(),
        }),
        "/skills" => Some(SubMenu {
            title: "open skill".into(),
            kind: SubKind::Skill,
            items: harness_skills::scan_skills_root(&cap_home().join("skills"))
                .unwrap_or_default()
                .into_iter()
                .map(|s| {
                    (
                        format!("{} — {}", s.manifest().name, s.manifest().description),
                        s.manifest().name.clone(),
                    )
                })
                .collect(),
        }),
        _ => None,
    }
}

/// Run a parsed command against the UI state. Returns `true` to quit.
fn apply_command(
    app: &mut App,
    cmd: cap::commands::Cmd,
    model_id: &str,
    tx: &Sender<Ctrl>,
) -> bool {
    use cap::commands::Cmd;
    match cmd {
        Cmd::Exit => return true,
        Cmd::Help => {
            for l in cap::commands::help_text().lines() {
                app.lines.push(('t', l.to_string()));
            }
        }
        Cmd::Clear => {
            app.lines.clear();
            app.current.clear();
        }
        Cmd::New => {
            app.lines.clear();
            app.current.clear();
            app.lines.push(('t', "(new session)".into()));
            let _ = tx.send(Ctrl::NewSession);
        }
        Cmd::Sessions => {
            for s in cap::session::list() {
                app.lines
                    .push(('t', format!("{}  {} turn(s)", s.id, s.turns.len())));
            }
        }
        Cmd::Model => app.lines.push(('t', format!("model {model_id}"))),
        Cmd::Skills => {
            for s in
                harness_skills::scan_skills_root(&cap_home().join("skills")).unwrap_or_default()
            {
                app.lines.push((
                    't',
                    format!("{} — {}", s.manifest().name, s.manifest().description),
                ));
            }
        }
        Cmd::Resume(id) if !id.is_empty() => {
            app.lines.push(('t', format!("resuming {id}…")));
            let _ = tx.send(Ctrl::Resume(id));
        }
        Cmd::Resume(_) => app
            .lines
            .push(('t', "type /resume then Enter to pick a session".into())),
        Cmd::Unknown(u) => app
            .lines
            .push(('e', format!("unknown command /{u} — /help"))),
    }
    false
}

fn run_ui(
    model_id: &str,
    session_id: &str,
    prompt_tx: Sender<Ctrl>,
    ui_rx: Receiver<UiEvent>,
) -> anyhow::Result<()> {
    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, terminal::EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    let mut app = App {
        lines: vec![(
            't',
            format!(
                "CAP TUI — {model_id} · session {session_id} · YOLO · type / for commands · Esc to quit"
            ),
        )],
        current: String::new(),
        input: String::new(),
        running: false,
        menu_idx: 0,
        submenu: None,
    };

    let result = (|| -> anyhow::Result<()> {
        loop {
            term.draw(|f| render(f, &app, model_id, session_id))?;

            if event::poll(Duration::from_millis(50))?
                && let CEvent::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            // ignore key Release/Repeat (some terminals emit them → double input)
            {
                // ── second-level menu takes priority when open ──
                if let Some(sm) = &app.submenu {
                    let n = sm.items.len();
                    let sel = app.menu_idx.min(n.saturating_sub(1));
                    match k.code {
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Esc => {
                            app.submenu = None;
                            app.menu_idx = 0;
                        }
                        KeyCode::Up => app.menu_idx = sel.saturating_sub(1),
                        KeyCode::Down => app.menu_idx = (sel + 1).min(n.saturating_sub(1)),
                        KeyCode::Enter if n > 0 => {
                            let value = sm.items[sel].1.clone();
                            let kind = sm.kind;
                            app.submenu = None;
                            app.menu_idx = 0;
                            match kind {
                                SubKind::Resume => {
                                    app.lines.push(('t', format!("resuming {value}…")));
                                    let _ = prompt_tx.send(Ctrl::Resume(value));
                                }
                                SubKind::Skill => {
                                    app.lines.push(('t', format!("── skill: {value} ──")));
                                    if let Some(sk) =
                                        harness_skills::scan_skills_root(&cap_home().join("skills"))
                                            .unwrap_or_default()
                                            .into_iter()
                                            .find(|s| s.manifest().name == value)
                                    {
                                        for l in sk.body().lines() {
                                            app.lines.push(('a', l.to_string()));
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                let cands = candidates(&app.input);
                let menu = !app.running && !cands.is_empty();
                let sel = app.menu_idx.min(cands.len().saturating_sub(1));
                match k.code {
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Esc => {
                        if app.input.is_empty() {
                            break;
                        }
                        app.input.clear();
                        app.menu_idx = 0;
                    }
                    // ── command popup navigation ──
                    KeyCode::Up if menu => app.menu_idx = sel.saturating_sub(1),
                    KeyCode::Down if menu => app.menu_idx = (sel + 1).min(cands.len() - 1),
                    KeyCode::Tab if menu => {
                        app.input = cands[sel].0.to_string();
                        app.menu_idx = 0;
                    }
                    KeyCode::Enter if menu => {
                        let name = cands[sel].0.to_string();
                        app.input.clear();
                        app.menu_idx = 0;
                        // A command with a second level opens its submenu; the
                        // rest run immediately.
                        if let Some(sm) = open_submenu(&name) {
                            app.lines.push(('u', name));
                            if sm.items.is_empty() {
                                app.lines.push(('t', "(nothing to pick)".into()));
                            } else {
                                app.submenu = Some(sm);
                            }
                        } else {
                            app.lines.push(('u', name.clone()));
                            if let Some(cmd) = cap::commands::parse(&name)
                                && apply_command(&mut app, cmd, model_id, &prompt_tx)
                            {
                                break;
                            }
                        }
                    }
                    // ── ordinary submit ──
                    KeyCode::Enter if !app.running && !app.input.trim().is_empty() => {
                        let p = std::mem::take(&mut app.input);
                        app.menu_idx = 0;
                        app.lines.push(('u', p.clone()));
                        if let Some(cmd) = cap::commands::parse(&p) {
                            if apply_command(&mut app, cmd, model_id, &prompt_tx) {
                                break;
                            }
                        } else {
                            app.running = true;
                            let _ = prompt_tx.send(Ctrl::Prompt(p));
                        }
                    }
                    KeyCode::Backspace if !app.running => {
                        app.input.pop();
                        app.menu_idx = 0;
                    }
                    KeyCode::Char(c) if !app.running => {
                        app.input.push(c);
                        app.menu_idx = 0;
                    }
                    _ => {}
                }
            }

            while let Ok(ev) = ui_rx.try_recv() {
                match ev {
                    UiEvent::Token(t) => app.current.push_str(&t),
                    UiEvent::Tool(s) => app.lines.push(('t', format!("⚙ {s}"))),
                    UiEvent::Done(reply) => {
                        // The final reply is authoritative — replace the live
                        // stream buffer with it (so errors / non-streamed replies
                        // always land, and a streamed reply shows exactly once).
                        app.current.clear();
                        if !reply.trim().is_empty() {
                            let kind = if reply.starts_with("[error]") {
                                'e'
                            } else {
                                'a'
                            };
                            app.lines.push((kind, reply));
                        }
                        app.running = false;
                    }
                    UiEvent::Load(turns) => {
                        // A resume reloaded the transcript — redraw it.
                        app.lines.clear();
                        app.current.clear();
                        for (role, text) in turns {
                            let kind = if role == "assistant" { 'a' } else { 'u' };
                            app.lines.push((kind, text));
                        }
                        app.lines.push(('t', "(resumed)".into()));
                    }
                    UiEvent::Fatal(m) => {
                        app.lines.push(('e', format!("[fatal] {m}")));
                        app.running = false;
                    }
                }
            }
        }
        Ok(())
    })();

    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen)?;
    term.show_cursor()?;
    result
}

fn render(f: &mut Frame, app: &App, model_id: &str, session_id: &str) {
    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(f.area());

    // Conversation.
    let mut lines: Vec<Line> = app
        .lines
        .iter()
        .map(|(kind, text)| match kind {
            'u' => Line::from(vec![
                Span::styled("› ", Style::new().magenta().bold()),
                Span::raw(text),
            ]),
            't' => Line::styled(format!("  {text}"), Style::new().dim()),
            'e' => Line::styled(text.clone(), Style::new().red()),
            _ => Line::raw(text.clone()),
        })
        .collect();
    if !app.current.is_empty() {
        lines.push(Line::raw(app.current.clone()));
    }
    let viewport = chunks[0].height.saturating_sub(2) as usize;
    let scroll = lines.len().saturating_sub(viewport) as u16;
    let convo = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" cap "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(convo, chunks[0]);

    // Input.
    let prompt = if app.running {
        Span::styled("… working ", Style::new().yellow())
    } else {
        Span::styled("› ", Style::new().cyan().bold())
    };
    let input = Paragraph::new(Line::from(vec![prompt, Span::raw(&app.input)]))
        .block(Block::default().borders(Borders::ALL).title(" input "));
    f.render_widget(input, chunks[1]);

    // Status bar.
    let status = format!(" {model_id} · session {session_id} · Esc/Ctrl-C to quit ");
    f.render_widget(Paragraph::new(status).style(Style::new().dim()), chunks[2]);

    // Popup, floating just above the input box. Second-level menu wins.
    let (title, rows): (String, Vec<String>) = if let Some(sm) = &app.submenu {
        (
            format!(" {}  ↑↓ select · Enter open · Esc back ", sm.title),
            sm.items
                .iter()
                .map(|(label, _)| format!(" {label}"))
                .collect(),
        )
    } else if !app.running {
        let cands = candidates(&app.input);
        (
            " commands  ↑↓ select · Tab complete · Enter run ".into(),
            cands
                .iter()
                .map(|(name, desc)| format!(" {name:<10} {desc}"))
                .collect(),
        )
    } else {
        (String::new(), vec![])
    };

    if !rows.is_empty() {
        let sel = app.menu_idx.min(rows.len() - 1);
        let h = (rows.len() as u16 + 2).min(f.area().height.saturating_sub(4));
        let w = 64.min(f.area().width.saturating_sub(4));
        let area = Rect {
            x: chunks[1].x + 1,
            y: chunks[1].y.saturating_sub(h),
            width: w,
            height: h,
        };
        let items: Vec<Line> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let style = if i == sel {
                    Style::new().black().on_cyan()
                } else {
                    Style::new()
                };
                Line::styled(r.clone(), style)
            })
            .collect();
        let popup =
            Paragraph::new(items).block(Block::default().borders(Borders::ALL).title(title));
        f.render_widget(Clear, area);
        f.render_widget(popup, area);
    }
}
