//! CAP — Computer-Aided Programming.
//!
//! A coding agent that reimplements the *core* of oh-my-pi on top of harness-rs:
//! **hashline editing** (content-hash line anchors instead of line numbers),
//! plus subagent fan-out, Hindsight memory, a persistent-LSP self-correction
//! sensor, MCP tool mounting, skills, and planner/worker model routing.
//!
//! ```sh
//! HARNESS_API_KEY=… HARNESS_BASE_URL=… HARNESS_MODEL=… cargo run -p cap
//! cargo run -p cap -- "add a doc comment to src/lib.rs"      # single-shot
//! cargo run -p cap -- --yolo                                 # no approval gate
//! ```
//!
//! The `cap` CLI/REPL front-end. Core lives in the `cap` library crate; this
//! binary supplies the terminal streaming + approval-gate hook (`CapUi`).

use cap::agent::{LoopParts, build_loop, cap_home, resolve_endpoint};
use cap::sensor::LspSensor;
use cap::session;
use cap::tools::{HashRead, TaskTool};
use cap::ui::CapUi;
use clap::Parser;
use harness_context::{FileMemory, default_world};
use harness_core::{Block, DynModel, Memory, Model, Skill, Task, Turn, TurnRole};
use harness_cortexdb::CortexdbMemory;
use harness_experience::ExperienceRecorder;
use harness_loop::Outcome;
use harness_mcp_client::McpClient;
use harness_models::OpenAiCompat;
use harness_tools_fs::{Glob, Grep, ListDir};
use std::path::PathBuf;
use std::sync::Arc;

/// CAP — Computer-Aided Programming: a hashline coding agent on harness-rs.
#[derive(Parser)]
#[command(name = "cap", version, about, long_about = None)]
struct Cli {
    /// Task for the agent (single-shot). Omit for an interactive REPL.
    #[arg(trailing_var_arg = true)]
    prompt: Vec<String>,

    /// Workspace root the agent reads/writes/searches (default: current dir).
    #[arg(long)]
    workspace: Option<PathBuf>,

    /// Skip the approval gate — apply writes/edits/shell with no confirmation.
    #[arg(long)]
    yolo: bool,

    /// Continue the most recent session for this workspace.
    #[arg(short = 'c', long = "continue")]
    cont: bool,

    /// Resume a specific session by id or path.
    #[arg(long, value_name = "PATH|ID")]
    resume: Option<String>,

    /// Use (or create) a named session.
    #[arg(long, value_name = "NAME")]
    session: Option<String>,

    /// List stored sessions and exit.
    #[arg(long)]
    sessions: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let yolo = cli.yolo;
    let workspace = cli.workspace;
    let cont = cli.cont;
    let resume = cli.resume;
    let session_name = cli.session;
    let list_sessions = cli.sessions;
    let oneshot = (!cli.prompt.is_empty()).then(|| cli.prompt.join(" "));

    // `--sessions` just lists what's stored, then exits.
    if list_sessions {
        let all = session::list();
        if all.is_empty() {
            eprintln!("(no sessions yet)");
        } else {
            for s in &all {
                let msg = s.first_prompt();
                let msg: String = msg.chars().take(50).collect();
                println!(
                    "{}  {} turn(s)  {}  \"{}\"",
                    s.id,
                    s.turns.len(),
                    s.workspace,
                    msg
                );
            }
            eprintln!(
                "\n{} session(s). Resume with `cap --resume <id>`.",
                all.len()
            );
        }
        return Ok(());
    }

    let (base, model_id, key) = resolve_endpoint()?;
    let root = workspace.unwrap_or_else(|| std::env::current_dir().unwrap());
    let mut world = default_world(root.clone());

    // Resolve which session this run belongs to.
    let mut sess = if let Some(r) = &resume {
        session::load(r)?
    } else if let Some(name) = &session_name {
        session::Session::named(name, &root)
    } else if cont {
        session::latest_for(&root).unwrap_or_else(|| session::Session::new(&root))
    } else {
        session::Session::new(&root)
    };

    // Model routing: a strong PLANNER drives the main loop (reasoning,
    // orchestration); a fast WORKER drives the `task` fan-out subagents. Same
    // endpoint/key; `CAP_WORKER_MODEL` picks the worker, defaulting to planner.
    let planner_id = model_id.clone();
    let worker_id = std::env::var("CAP_WORKER_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| planner_id.clone());
    let planner: Arc<dyn Model> = Arc::new(OpenAiCompat::with_key(
        base.clone(),
        planner_id.clone(),
        key.clone(),
    ));
    let worker: Arc<dyn Model> = if worker_id == planner_id {
        planner.clone()
    } else {
        Arc::new(OpenAiCompat::with_key(
            base.clone(),
            worker_id.clone(),
            key.clone(),
        ))
    };

    // Experience memory (Hindsight): prefer the shared CortexDB brain for
    // semantic recall; fall back to a local JSONL file when it isn't running.
    let (memory, mem_kind): (Arc<dyn Memory>, String) =
        match CortexdbMemory::connect_stdio("cortexdb-mcp-stdio", &[]).await {
            Ok(m) => (
                Arc::new(m.with_namespace("cap")),
                "cortexdb (semantic)".into(),
            ),
            Err(_) => {
                let p = cap_home().join("experience.jsonl");
                (
                    Arc::new(FileMemory::open(&p)?),
                    format!("file {}", p.display()),
                )
            }
        };
    let recorder = ExperienceRecorder::new(memory);

    // `task` fan-out subagents run on the fast WORKER model.
    let task_tool = TaskTool {
        model: worker.clone(),
        tools: vec![
            Arc::new(HashRead),
            Arc::new(ListDir),
            Arc::new(Grep),
            Arc::new(Glob),
        ],
    };

    // Optional LSP diagnostics sensor, e.g. CAP_LSP="rust-analyzer".
    let lsp = std::env::var("CAP_LSP")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| LspSensor {
            id: "lsp".into(),
            cmd: s.split_whitespace().map(|x| x.to_string()).collect(),
            session: tokio::sync::OnceCell::new(),
        });
    let lsp_desc = lsp
        .as_ref()
        .map(|s| s.cmd.join(" "))
        .unwrap_or_else(|| "off".into());

    // Optional external MCP server, e.g. CAP_MCP="cortexdb-mcp-stdio". Its tools
    // join the loop; `_mcp` keeps the connection alive for the session.
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
                    (
                        tools.clone(),
                        format!("{} ({} tools)", parts[0], tools.len()),
                        Some(client),
                    )
                }
                Err(e) => {
                    eprintln!("\x1b[33m(mcp connect failed: {e})\x1b[0m");
                    (vec![], "off".into(), None)
                }
            }
        }
        None => (vec![], "off".into(), None),
    };

    // Skills = procedural memory the agent can read and author, at ~/.cap/skills.
    let skills_dir = cap_home().join("skills");
    let _ = std::fs::create_dir_all(&skills_dir);
    let skill_count = harness_skills::scan_skills_root(&skills_dir)
        .map(|s| s.len())
        .unwrap_or(0);

    let loop_ = build_loop(
        DynModel(planner.clone()),
        LoopParts {
            ui_hook: Arc::new(CapUi::new(yolo)),
            task_tool,
            trace_hook: recorder.tool_trace_hook(),
            exp_guide: Arc::new(recorder.guide()),
            lsp,
            mcp_tools,
            skills_dir,
        },
    );

    let mode = if yolo {
        "\x1b[31mYOLO\x1b[0m"
    } else {
        "\x1b[32mNORMAL\x1b[0m"
    };
    let routing = if worker_id == planner_id {
        planner_id.clone()
    } else {
        format!("planner {planner_id} + worker {worker_id}")
    };
    eprintln!(
        "\x1b[1mCAP\x1b[0m (hashline coding agent) — {routing} · {} · {mode}",
        root.display()
    );
    eprintln!(
        "\x1b[2mexperience: {mem_kind} · subagents: on · lsp: {lsp_desc} · mcp: {mcp_desc} · skills: {skill_count}\x1b[0m"
    );
    if sess.turns.is_empty() {
        eprintln!("\x1b[2msession: {} (new)\x1b[0m", sess.id);
    } else {
        eprintln!(
            "\x1b[2msession: {} (resumed {} turns)\x1b[0m",
            sess.id,
            sess.turns.len()
        );
    }

    // Single-shot mode (still resumes/continues a session if asked).
    if let Some(p) = oneshot {
        let situation = p.clone();
        let task = Task {
            description: p.clone(),
            source: None,
            deadline: None,
        };
        let out = loop_
            .run_with_seed_history(task, sess.seed(), &mut world, 30)
            .await?;
        println!();
        let reply = match &out {
            Outcome::Done { text, .. } => text.clone().unwrap_or_default(),
            Outcome::BudgetExhausted {
                last_text, iters, ..
            } => {
                eprintln!("\x1b[33m(stopped at {iters}-iter budget)\x1b[0m");
                last_text.clone().unwrap_or_default()
            }
        };
        recorder.record(situation, reply.clone()).await;
        sess.push("user", &p);
        sess.push("assistant", &reply);
        sess.save()?;
        return Ok(());
    }

    // Interactive REPL.
    eprintln!(
        "tools: hash_read · hash_edit · write · list · grep · glob · task    ·    /help for commands\n"
    );
    let mut seed: Vec<Turn> = sess.seed();
    let stdin = std::io::stdin();
    loop {
        {
            use std::io::Write;
            print!("\x1b[1;35mcap › \x1b[0m");
            std::io::stdout().flush().ok();
        }
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        // Slash commands.
        if let Some(cmd) = cap::commands::parse(input) {
            use cap::commands::Cmd;
            match cmd {
                Cmd::Exit => break,
                Cmd::Help => eprintln!("{}", cap::commands::help_text()),
                Cmd::New => {
                    seed.clear();
                    sess = session::Session::new(&root);
                    eprintln!("\x1b[2m(new session {})\x1b[0m", sess.id);
                }
                Cmd::Sessions => {
                    for s in session::list() {
                        eprintln!("  {}  {} turn(s)  {}", s.id, s.turns.len(), s.workspace);
                    }
                }
                Cmd::Resume(id) if id.is_empty() => eprintln!("usage: /resume <id|path>"),
                Cmd::Resume(id) => match session::load(&id) {
                    Ok(s) => {
                        sess = s;
                        seed = sess.seed();
                        eprintln!(
                            "\x1b[2m(resumed {} — {} turns)\x1b[0m",
                            sess.id,
                            sess.turns.len()
                        );
                    }
                    Err(e) => eprintln!("\x1b[31mresume failed:\x1b[0m {e}"),
                },
                Cmd::Skills => {
                    for s in harness_skills::scan_skills_root(&cap_home().join("skills"))
                        .unwrap_or_default()
                    {
                        eprintln!("  {} — {}", s.manifest().name, s.manifest().description);
                    }
                }
                Cmd::Model => eprintln!("planner {planner_id} · worker {worker_id}"),
                Cmd::Clear => print!("\x1b[2J\x1b[H"),
                Cmd::Unknown(u) => eprintln!("unknown command /{u} — try /help"),
            }
            continue;
        }
        let task = Task {
            description: input.to_string(),
            source: None,
            deadline: None,
        };
        let reply = match loop_
            .run_with_seed_history(task, seed.clone(), &mut world, 30)
            .await
        {
            Ok(Outcome::Done { text, .. }) => text.unwrap_or_default(),
            Ok(Outcome::BudgetExhausted { last_text, .. }) => last_text.unwrap_or_default(),
            Err(e) => {
                eprintln!("\n\x1b[31merror:\x1b[0m {e}");
                continue;
            }
        };
        println!();
        // Hindsight: record situation → tools used → outcome for later recall.
        recorder.record(input.to_string(), reply.clone()).await;
        // Persist the turn so this session can be resumed later.
        sess.push("user", input);
        sess.push("assistant", &reply);
        if let Err(e) = sess.save() {
            eprintln!("\x1b[33m(session save failed: {e})\x1b[0m");
        }
        seed.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(input.to_string())],
        });
        seed.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::Text(reply)],
        });
    }
    eprintln!("bye.");
    Ok(())
}
