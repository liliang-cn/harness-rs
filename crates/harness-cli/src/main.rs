use clap::{Parser, Subcommand};
use harness_core::Skill;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(name = "harness", version, about = "Harness agent framework CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Scaffold a new agent project that depends on `harness` and demonstrates
    /// one `#[skill]`, one `#[tool]`, and a minimal `AgentLoop` in `main`.
    New {
        /// Directory name (and cargo package name) of the new project.
        name: String,
        /// Parent directory (defaults to the current dir).
        #[arg(long)]
        path: Option<PathBuf>,
        /// Auto-wire `[patch.crates-io]` pointing at this local harness workspace
        /// (the directory containing the top-level `Cargo.toml` of the harness
        /// workspace, e.g. `/Users/me/code/harness`). Use for local development
        /// against an unpublished framework. If the CLI binary itself was
        /// installed from a local checkout, that path is used as the default
        /// when `--workspace` is omitted but `--local` is passed.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Shorthand: auto-detect the harness workspace from the running binary's
        /// build context and inject [patch.crates-io] for it. Useful right after
        /// `cargo install --path crates/harness-cli`.
        #[arg(long)]
        local: bool,
    },
    /// Pretty-print a recorded session JSONL log (from `SessionRecorder`).
    Trace {
        /// Path to the .jsonl log file.
        file: PathBuf,
        /// Just print the SessionStats summary; skip per-event lines.
        #[arg(long)]
        summary: bool,
        /// Multi-line verbose format: include model text, full tool args,
        /// tool result preview, and failure reasons (errors / hint / message).
        #[arg(long, short)]
        verbose: bool,
    },
    /// Run an MCP server over stdio. Exposes the framework's built-in tool
    /// registry (read_file, write_file, edit_file, list_dir, shell_read) to
    /// any MCP-compatible client (Claude Code, Cursor, Codex, …).
    Mcp {
        #[command(subcommand)]
        cmd: McpCmd,
    },
    /// Run an agent against a prompt. Model from --model/--base-url or the
    /// HARNESS_* / DEEPSEEK_API_KEY env vars. Read-only by default; opt into
    /// writes with --write and shell with --shell.
    Run {
        /// The task / prompt for the agent.
        prompt: String,
        /// Agent world root — where fs tools read/write (default: current dir).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Model id (default: $HARNESS_MODEL, else "deepseek-chat").
        #[arg(long)]
        model: Option<String>,
        /// OpenAI-compatible base URL (default: $HARNESS_BASE_URL, else
        /// https://api.deepseek.com).
        #[arg(long)]
        base_url: Option<String>,
        /// Max agent iterations (default: 12).
        #[arg(long, default_value_t = 12)]
        max_iters: u32,
        /// Add write/edit filesystem tools (off by default — read-only).
        #[arg(long)]
        write: bool,
        /// Add the risk-gated read-only shell tool.
        #[arg(long)]
        shell: bool,
        /// Stream a live progress trace to stderr.
        #[arg(long)]
        progress: bool,
        /// Print the full Outcome as JSON instead of just the final text.
        #[arg(long)]
        json: bool,
    },
    /// Schedule agents to run on a recurring schedule. Jobs persist to a JSON
    /// file (default ~/.harness/jobs.json). `serve` runs the tick loop; `run`
    /// fires every due job once (for an external cron); the rest are CRUD.
    Sched {
        #[command(subcommand)]
        cmd: SchedCmd,
    },
    /// Interactive coding agent — a multi-turn REPL with file, search, and
    /// shell tools, streaming output. NORMAL mode gates every write/shell behind
    /// a y/N prompt; `--yolo` runs unattended (no approval). `/exit` to quit.
    Code {
        /// Workspace root the agent reads/writes/searches (default: current dir).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Model id (default: $HARNESS_MODEL, else "deepseek-chat").
        #[arg(long)]
        model: Option<String>,
        /// OpenAI-compatible base URL (default: $HARNESS_BASE_URL).
        #[arg(long)]
        base_url: Option<String>,
        /// Max agent iterations per turn (default: 30 — coding needs headroom).
        #[arg(long, default_value_t = 30)]
        max_iters: u32,
        /// YOLO: skip the approval gate — apply writes/edits/shell with no
        /// confirmation. Default is NORMAL (confirm each mutating action).
        #[arg(long)]
        yolo: bool,
        /// Run shell commands inside an OS sandbox (macOS Seatbelt / Linux
        /// bubblewrap): network denied, writes confined to the workspace. No-op
        /// if the OS sandbox tool is unavailable.
        #[arg(long)]
        sandbox: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SchedCmd {
    /// Add a scheduled job.
    Add {
        /// Human-readable name (shown in listings and delivered output).
        name: String,
        /// Schedule: "daily 08:00" | "weekly mon 09:30" | "every 15m".
        schedule: String,
        /// The agent prompt to run each time the job fires.
        prompt: String,
        /// Delivery channel: "stdout" (default) or "email" (needs RESEND_API_KEY).
        #[arg(long, default_value = "stdout")]
        channel: String,
        /// Channel recipient (email address, chat id, …) when relevant.
        #[arg(long)]
        target: Option<String>,
        /// Jobs store file (default: ~/.harness/jobs.json).
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// List all scheduled jobs.
    List {
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Remove a job by id.
    Rm {
        id: String,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Enable a job by id.
    Enable {
        id: String,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Disable a job by id (kept in the store, skipped when due).
    Disable {
        id: String,
        #[arg(long)]
        store: Option<PathBuf>,
    },
    /// Fire every currently-due job once, then exit. Point an OS cron at this.
    Run {
        #[arg(long)]
        store: Option<PathBuf>,
        /// Agent world root (default: current dir).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Model id (default: $HARNESS_MODEL, else "deepseek-chat").
        #[arg(long)]
        model: Option<String>,
        /// OpenAI-compatible base URL (default: $HARNESS_BASE_URL).
        #[arg(long)]
        base_url: Option<String>,
    },
    /// Run the scheduler loop forever, ticking on an interval.
    Serve {
        #[arg(long)]
        store: Option<PathBuf>,
        /// Agent world root (default: current dir).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Model id (default: $HARNESS_MODEL, else "deepseek-chat").
        #[arg(long)]
        model: Option<String>,
        /// OpenAI-compatible base URL (default: $HARNESS_BASE_URL).
        #[arg(long)]
        base_url: Option<String>,
        /// Tick interval in seconds (default: 60).
        #[arg(long, default_value_t = 60)]
        tick: u64,
    },
}

#[derive(Subcommand, Debug)]
enum McpCmd {
    /// Start the stdio JSON-RPC server. Reads requests on stdin, writes
    /// responses on stdout, one line each.
    Serve {
        /// Workspace root the tools operate inside. Defaults to current dir.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Directory of agentskills.io-compliant skills to expose as MCP
        /// resources (resources/list + resources/read). Each subdirectory
        /// containing a SKILL.md becomes a `harness://skill/<name>` resource.
        #[arg(long)]
        skills: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum SkillsCmd {
    Validate {
        path: PathBuf,
    },
    List {
        dir: PathBuf,
    },
    /// Export every registered skill (filesystem + `#[skill]` macro) to a
    /// spec-compliant directory tree that Claude Code / Cursor / Codex can
    /// consume.
    Export {
        /// Target directory; created if missing.
        target: PathBuf,
        /// Pull skills from this filesystem directory too (optional).
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Lint a directory of skills for configuration smells beyond what the
    /// spec validator requires (short descriptions, missing trigger language,
    /// duplicate / overlapping skills).
    Lint {
        dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber_init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Skills {
            cmd: SkillsCmd::Validate { path },
        } => match harness_skills::load_skill_dir(&path) {
            Ok(s) => {
                println!(
                    "✓ valid: {} — {}",
                    s.manifest().name,
                    s.manifest().description
                );
                Ok(())
            }
            Err(e) => {
                eprintln!("✗ invalid: {e}");
                std::process::exit(1);
            }
        },
        Cmd::Skills {
            cmd: SkillsCmd::List { dir },
        } => {
            let skills = harness_skills::scan_skills_root(&dir)?;
            for s in &skills {
                println!("{}  —  {}", s.manifest().name, s.manifest().description);
            }
            println!("\n{} skill(s)", skills.len());
            Ok(())
        }
        Cmd::Skills {
            cmd: SkillsCmd::Lint { dir },
        } => {
            let findings = harness_skills::lint_dir(&dir)?;
            if findings.is_empty() {
                println!("✓ no lint findings in {}", dir.display());
                return Ok(());
            }
            let mut errors = 0;
            let mut warnings = 0;
            let mut infos = 0;
            for f in &findings {
                let tag = match f.severity {
                    harness_skills::LintSeverity::Error => {
                        errors += 1;
                        "ERROR"
                    }
                    harness_skills::LintSeverity::Warning => {
                        warnings += 1;
                        "WARN "
                    }
                    harness_skills::LintSeverity::Info => {
                        infos += 1;
                        "INFO "
                    }
                };
                println!("[{tag}] {}: {}", f.skill_name, f.message);
            }
            println!("\n{errors} error(s), {warnings} warning(s), {infos} info");
            if errors > 0 {
                std::process::exit(1);
            }
            return Ok(());
        }
        Cmd::Skills {
            cmd: SkillsCmd::Export { target, from },
        } => {
            let mut registry = harness_skills::SkillRegistry::new().with_macro_skills()?;
            if let Some(p) = from {
                registry = registry.with_filesystem_root(&p)?;
            }
            let paths = harness_skills::export_registry(&registry, &target)?;
            for p in &paths {
                println!("✓ {}", p.display());
            }
            println!(
                "\nexported {} skill(s) to {}",
                paths.len(),
                target.display()
            );
            Ok(())
        }
        Cmd::New {
            name,
            path,
            workspace,
            local,
        } => scaffold_new_project(name, path, workspace, local),
        Cmd::Trace {
            file,
            summary,
            verbose,
        } => print_session_trace(file, summary, verbose),
        Cmd::Mcp {
            cmd: McpCmd::Serve { workspace, skills },
        } => run_mcp_server(workspace, skills).await,
        Cmd::Run {
            prompt,
            workspace,
            model,
            base_url,
            max_iters,
            write,
            shell,
            progress,
            json,
        } => {
            run_agent(RunOpts {
                prompt,
                workspace,
                model,
                base_url,
                max_iters,
                write,
                shell,
                progress,
                json,
            })
            .await
        }
        Cmd::Sched { cmd } => run_sched(cmd).await,
        Cmd::Code {
            workspace,
            model,
            base_url,
            max_iters,
            yolo,
            sandbox,
        } => run_code(workspace, model, base_url, max_iters, yolo, sandbox).await,
    }
}

struct RunOpts {
    prompt: String,
    workspace: Option<PathBuf>,
    model: Option<String>,
    base_url: Option<String>,
    max_iters: u32,
    write: bool,
    shell: bool,
    progress: bool,
    json: bool,
}

async fn run_agent(opts: RunOpts) -> anyhow::Result<()> {
    use harness_core::Task;
    use harness_loop::{AgentLoop, LiveProgressHook, Outcome};
    use harness_models::OpenAiCompat;
    use std::sync::Arc;

    let (base_url, model_id, key) = resolve_endpoint(opts.model, opts.base_url)?;

    let root = opts
        .workspace
        .unwrap_or_else(|| std::env::current_dir().unwrap());
    let mut world = harness_context::default_world(root);

    let model = OpenAiCompat::with_key(base_url, model_id, key);
    let mut loop_ = AgentLoop::new(model)
        .with_tool(Arc::new(harness_tools_fs::ReadFile))
        .with_tool(Arc::new(harness_tools_fs::ListDir));
    if opts.write {
        loop_ = loop_
            .with_tool(Arc::new(harness_tools_fs::WriteFile))
            .with_tool(Arc::new(harness_tools_fs::EditFile));
    }
    if opts.shell {
        loop_ = loop_.with_tool(Arc::new(harness_tools_shell::ShellRead));
    }
    if opts.progress {
        loop_ = loop_.with_hook(Arc::new(LiveProgressHook::new()));
    }

    let task = Task {
        description: opts.prompt,
        source: None,
        deadline: None,
    };
    let outcome = loop_
        .run_with_max_iters(task, &mut world, opts.max_iters)
        .await
        .map_err(|e| anyhow::anyhow!("agent run failed: {e}"))?;

    if opts.json {
        // Best-effort structured dump of the outcome.
        let (kind, text, iters, tools, ti, to) = match &outcome {
            Outcome::Done {
                text,
                iters,
                tools_called,
                usage,
                ..
            } => (
                "done",
                text.clone(),
                *iters,
                *tools_called,
                usage.input_tokens,
                usage.output_tokens,
            ),
            Outcome::BudgetExhausted {
                last_text,
                iters,
                tools_called,
                usage,
                ..
            } => (
                "budget_exhausted",
                last_text.clone(),
                *iters,
                *tools_called,
                usage.input_tokens,
                usage.output_tokens,
            ),
            Outcome::Stuck {
                last_text,
                iters,
                tools_called,
                usage,
                ..
            } => (
                "stuck",
                last_text.clone(),
                *iters,
                *tools_called,
                usage.input_tokens,
                usage.output_tokens,
            ),
        };
        println!(
            "{}",
            serde_json::json!({
                "outcome": kind,
                "text": text,
                "iters": iters,
                "tools_called": tools,
                "input_tokens": ti,
                "output_tokens": to,
            })
        );
    } else {
        match outcome {
            Outcome::Done { text, .. } => {
                println!("{}", text.unwrap_or_default().trim());
            }
            Outcome::BudgetExhausted {
                last_text, iters, ..
            } => {
                eprintln!("(budget exhausted after {iters} iters)");
                if let Some(t) = last_text {
                    println!("{}", t.trim());
                }
            }
            Outcome::Stuck {
                last_text,
                iters,
                reason,
                ..
            } => {
                eprintln!("(stuck after {iters} iters: {reason})");
                if let Some(t) = last_text {
                    println!("{}", t.trim());
                }
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------
// `harness code` — interactive coding-agent REPL
// ------------------------------------------------------------------

/// One hook drives the whole terminal UX: it streams the model's text to
/// stdout token-by-token, prints a compact activity line before each tool call,
/// and — in NORMAL mode — blocks on a y/N prompt before any mutating tool runs,
/// returning `Deny` (which the loop surfaces to the model) if the user declines.
struct ReplHook {
    yolo: bool,
}

impl harness_core::Hook for ReplHook {
    fn name(&self) -> &str {
        "repl-ui"
    }
    fn matches(&self, ev: &harness_core::Event<'_>) -> bool {
        matches!(
            ev,
            harness_core::Event::ModelTokenDelta { .. } | harness_core::Event::PreToolUse { .. }
        )
    }
    fn fire(
        &self,
        ev: &harness_core::Event<'_>,
        _w: &mut harness_core::World,
    ) -> harness_core::HookOutcome {
        use harness_core::{Event, HookOutcome};
        use std::io::Write;
        match ev {
            Event::ModelTokenDelta { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                HookOutcome::Allow
            }
            Event::PreToolUse { action } => {
                let risky = matches!(
                    action.tool.as_str(),
                    "write_file" | "edit_file" | "shell_exec"
                );
                // Dim activity line to stderr so it doesn't pollute streamed text.
                eprintln!(
                    "\n  \x1b[2m⚙ {}{}\x1b[0m",
                    action.tool,
                    fmt_tool_args(&action.tool, &action.args)
                );
                if self.yolo || !risky {
                    return HookOutcome::Allow;
                }
                eprint!("  apply this {}? [y/N] ", action.tool);
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                let ok = std::io::stdin().read_line(&mut line).is_ok()
                    && matches!(line.trim(), "y" | "Y" | "yes");
                if ok {
                    HookOutcome::Allow
                } else {
                    eprintln!("  \x1b[33m✗ skipped\x1b[0m");
                    HookOutcome::Deny {
                        reason: format!(
                            "user declined the {} action; do not retry it — ask or try another approach",
                            action.tool
                        ),
                    }
                }
            }
            _ => HookOutcome::Allow,
        }
    }
}

/// Compact, human-readable one-liner for a tool call's args.
fn fmt_tool_args(tool: &str, args: &serde_json::Value) -> String {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let trunc = |t: &str, n: usize| {
        let t = t.replace('\n', "⏎");
        if t.chars().count() > n {
            format!("{}…", t.chars().take(n).collect::<String>())
        } else {
            t
        }
    };
    match tool {
        "write_file" => format!(" {} ({} bytes)", s("path"), s("content").len()),
        "edit_file" => format!(
            " {}  {:?}→{:?}",
            s("path"),
            trunc(s("old_string"), 30),
            trunc(s("new_string"), 30)
        ),
        "shell_exec" | "shell_read" => format!(" $ {}", trunc(s("command"), 80)),
        "read_file" | "list_dir" | "glob" => format!(" {}", s("path")),
        "grep" => format!(" /{}/ {}", trunc(s("pattern"), 40), s("path")),
        _ => format!(" {}", trunc(&args.to_string(), 80)),
    }
}

/// Spawn the platform OS sandbox (macOS Seatbelt / Linux bubblewrap) rooted at
/// `root` and return its `World` — its `runner.exec` confines shell commands
/// (network denied, writes limited to the workspace). Falls back to a normal
/// world (with a note) if the OS sandbox tool isn't available.
async fn os_sandbox_world(root: &std::path::Path) -> (harness_core::World, String) {
    use harness_sandbox::Sandbox;
    #[cfg(target_os = "macos")]
    let backend = harness_sandbox::SeatbeltSandbox::new(root).with_confine_writes(true);
    #[cfg(target_os = "linux")]
    let backend = harness_sandbox::BubblewrapSandbox::new(root);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        return (
            harness_context::default_world(root.to_path_buf()),
            "unsupported OS".into(),
        );
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    match backend.spawn().await {
        Ok(handle) => {
            let desc = format!("on ({})", handle.label());
            (handle.into_world(), desc)
        }
        Err(e) => {
            eprintln!("\x1b[33m(sandbox unavailable: {e}; running without)\x1b[0m");
            (harness_context::default_world(root.to_path_buf()), "off".into())
        }
    }
}

async fn run_code(
    workspace: Option<PathBuf>,
    model: Option<String>,
    base_url: Option<String>,
    max_iters: u32,
    yolo: bool,
    sandbox: bool,
) -> anyhow::Result<()> {
    use harness_core::{Block, Task, Turn, TurnRole};
    use harness_loop::{AgentLoop, Outcome};
    use harness_models::OpenAiCompat;
    use std::io::Write;
    use std::sync::Arc;

    let (base_url, model_id, key) = resolve_endpoint(model, base_url)?;
    let root = workspace.unwrap_or_else(|| std::env::current_dir().unwrap());
    // With --sandbox, shell commands run inside an OS sandbox (network denied,
    // writes confined to the workspace); falls back to a normal world if the OS
    // sandbox tool is unavailable.
    let (mut world, sandbox_desc) = if sandbox {
        os_sandbox_world(&root).await
    } else {
        (harness_context::default_world(root.clone()), "off".to_string())
    };
    let model = OpenAiCompat::with_key(base_url, model_id.clone(), key);

    let loop_ = AgentLoop::new(model)
        .with_streaming(true)
        .with_tool(Arc::new(harness_tools_fs::ReadFile))
        .with_tool(Arc::new(harness_tools_fs::ListDir))
        .with_tool(Arc::new(harness_tools_fs::Grep))
        .with_tool(Arc::new(harness_tools_fs::Glob))
        .with_tool(Arc::new(harness_tools_fs::WriteFile))
        .with_tool(Arc::new(harness_tools_fs::EditFile))
        .with_tool(Arc::new(harness_tools_shell::ShellRead))
        .with_tool(Arc::new(harness_tools_shell::ShellExec))
        .with_hook(Arc::new(ReplHook { yolo }));

    let mode = if yolo {
        "\x1b[31mYOLO\x1b[0m (no approval)"
    } else {
        "\x1b[32mNORMAL\x1b[0m (approve writes)"
    };
    println!(
        "\x1b[1mharness code\x1b[0m — {model_id}  ·  {}  ·  {mode}  ·  sandbox: {sandbox_desc}",
        root.display()
    );
    println!("tools: read · write · edit · list · grep · glob · shell    /reset · /exit\n");

    let mut seed: Vec<Turn> = Vec::new();
    let stdin = std::io::stdin();
    loop {
        print!("\x1b[1;36m› \x1b[0m");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break; // EOF / Ctrl-D
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        match input {
            "/exit" | "/quit" | ":q" => break,
            "/reset" => {
                seed.clear();
                println!("\x1b[2m(context cleared)\x1b[0m");
                continue;
            }
            _ => {}
        }

        let task = Task {
            description: input.to_string(),
            source: None,
            deadline: None,
        };
        let outcome = loop_
            .run_with_seed_history(task, seed.clone(), &mut world, max_iters)
            .await;
        println!(); // terminate the streamed line
        let reply = match outcome {
            Ok(Outcome::Done { text, .. }) => text.unwrap_or_default(),
            Ok(Outcome::BudgetExhausted {
                last_text, iters, ..
            }) => {
                eprintln!("\x1b[33m(stopped at {iters}-iter budget)\x1b[0m");
                last_text.unwrap_or_default()
            }
            Ok(Outcome::Stuck {
                last_text, iters, reason, ..
            }) => {
                eprintln!("\x1b[33m(stuck after {iters} iters: {reason})\x1b[0m");
                last_text.unwrap_or_default()
            }
            Err(e) => {
                eprintln!("\x1b[31merror:\x1b[0m {e}");
                continue;
            }
        };
        // Grow the conversation so the next turn has context. Files on disk are
        // the real shared state; this keeps the dialogue coherent.
        seed.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(input.to_string())],
        });
        seed.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::Text(reply)],
        });
    }
    println!("bye.");
    Ok(())
}

/// Resolve (base_url, model_id, api_key) from flags → env → defaults. Shared by
/// `run` and `sched`. Flags override env; env overrides sane defaults.
fn resolve_endpoint(
    model: Option<String>,
    base_url: Option<String>,
) -> anyhow::Result<(String, String, String)> {
    let key = std::env::var("HARNESS_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .map_err(|_| anyhow::anyhow!("set HARNESS_API_KEY (or DEEPSEEK_API_KEY)"))?;
    let base_url = base_url
        .or_else(|| std::env::var("HARNESS_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.deepseek.com".to_string());
    let model_id = model
        .or_else(|| std::env::var("HARNESS_MODEL").ok())
        .unwrap_or_else(|| "deepseek-chat".to_string());
    Ok((base_url, model_id, key))
}

/// Default jobs store: `~/.harness/jobs.json` (or `./harness-jobs.json` if HOME
/// is unset). Overridable per-command with `--store`.
fn sched_store_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".harness").join("jobs.json"),
        None => PathBuf::from("harness-jobs.json"),
    }
}

async fn run_sched(cmd: SchedCmd) -> anyhow::Result<()> {
    use harness_daemon::Schedule;
    use harness_scheduler::{FileJobStore, Job, JobStore};

    match cmd {
        SchedCmd::Add {
            name,
            schedule,
            prompt,
            channel,
            target,
            store,
        } => {
            // Validate the schedule up front so a typo fails now, not at fire time.
            let parsed = Schedule::parse(&schedule)
                .map_err(|e| anyhow::anyhow!("bad schedule {schedule:?}: {e}"))?;
            let store = FileJobStore::open(sched_store_path(store))?;
            let now = chrono::Local::now();
            let next = parsed.next_after(now).timestamp_millis();
            let job = Job::new(name, schedule, prompt, channel, now.timestamp_millis())
                .with_target(target)
                .with_next_run(Some(next));
            let id = job.id.clone();
            store.add(&job).await?;
            println!("✓ added job {id}  (next run: {})", parsed.next_after(now));
            Ok(())
        }
        SchedCmd::List { store } => {
            let store = FileJobStore::open(sched_store_path(store))?;
            let jobs = store.list().await?;
            if jobs.is_empty() {
                println!("(no scheduled jobs)");
                return Ok(());
            }
            for j in &jobs {
                let state = if j.enabled { "on " } else { "off" };
                let next = j
                    .next_run_ms
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map(|dt| {
                        dt.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_else(|| "due".into());
                println!(
                    "[{state}] {}  {}  ({}) → {}  next {}",
                    j.id, j.name, j.schedule, j.channel, next
                );
            }
            println!("\n{} job(s)", jobs.len());
            Ok(())
        }
        SchedCmd::Rm { id, store } => {
            let store = FileJobStore::open(sched_store_path(store))?;
            if store.remove(&id).await? {
                println!("✓ removed {id}");
                Ok(())
            } else {
                anyhow::bail!("no job with id {id}");
            }
        }
        SchedCmd::Enable { id, store } => {
            let store = FileJobStore::open(sched_store_path(store))?;
            if store.set_enabled(&id, true).await? {
                println!("✓ enabled {id}");
                Ok(())
            } else {
                anyhow::bail!("no job with id {id}");
            }
        }
        SchedCmd::Disable { id, store } => {
            let store = FileJobStore::open(sched_store_path(store))?;
            if store.set_enabled(&id, false).await? {
                println!("✓ disabled {id}");
                Ok(())
            } else {
                anyhow::bail!("no job with id {id}");
            }
        }
        SchedCmd::Run {
            store,
            workspace,
            model,
            base_url,
        } => {
            let sched = build_scheduler(store, workspace, model, base_url)?;
            let fired = sched.tick_once().await;
            eprintln!("fired {fired} due job(s)");
            Ok(())
        }
        SchedCmd::Serve {
            store,
            workspace,
            model,
            base_url,
            tick,
        } => {
            let sched = build_scheduler(store, workspace, model, base_url)?
                .with_tick(std::time::Duration::from_secs(tick));
            eprintln!("scheduler serving; ticking every {tick}s (Ctrl-C to stop)");
            // Run the tick loop inline. The Subagent future is !Send, so we drive
            // it on the main runtime's block_on rather than tokio::spawn.
            loop {
                let _ = sched.tick_once().await;
                tokio::time::sleep(std::time::Duration::from_secs(tick)).await;
            }
        }
    }
}

/// Build a `Scheduler` wired with the resolved model, fs read tools, and the
/// stdout channel (plus email if RESEND_API_KEY is set).
fn build_scheduler(
    store: Option<PathBuf>,
    workspace: Option<PathBuf>,
    model: Option<String>,
    base_url: Option<String>,
) -> anyhow::Result<harness_scheduler::Scheduler> {
    use harness_models::OpenAiCompat;
    use harness_scheduler::{EmailChannel, FileJobStore, JobStore, Scheduler, StdoutChannel};
    use std::sync::Arc;

    let (base_url, model_id, key) = resolve_endpoint(model, base_url)?;
    let model: Arc<dyn harness_core::Model> =
        Arc::new(OpenAiCompat::with_key(base_url, model_id, key));
    let store: Arc<dyn JobStore> = Arc::new(FileJobStore::open(sched_store_path(store))?);
    let root = workspace.unwrap_or_else(|| std::env::current_dir().unwrap());

    let mut sched = Scheduler::new(store, model)
        .with_repo_root(root)
        .with_tool(Arc::new(harness_tools_fs::ReadFile))
        .with_tool(Arc::new(harness_tools_fs::ListDir))
        .with_tool(Arc::new(harness_tools_shell::ShellRead))
        .with_channel(Arc::new(StdoutChannel::new()));
    if let Some(email) = EmailChannel::from_env() {
        sched = sched.with_channel(Arc::new(email));
    }
    Ok(sched)
}

async fn run_mcp_server(workspace: Option<PathBuf>, skills: Option<PathBuf>) -> anyhow::Result<()> {
    use std::sync::Arc;
    let root = workspace.unwrap_or_else(|| std::env::current_dir().unwrap());
    let mut world = harness_context::default_world(root);
    let mut server = harness_mcp::McpServer::new("harness-mcp", env!("CARGO_PKG_VERSION"))
        .with_tools(vec![
            Arc::new(harness_tools_fs::ReadFile),
            Arc::new(harness_tools_fs::WriteFile),
            Arc::new(harness_tools_fs::EditFile),
            Arc::new(harness_tools_fs::ListDir),
            Arc::new(harness_tools_shell::ShellRead),
        ]);

    // Load skills from a directory if --skills <path> was given.
    if let Some(skills_root) = skills {
        let loaded = harness_skills::scan_skills_root(&skills_root)
            .map_err(|e| anyhow::anyhow!("scan skills root {}: {e}", skills_root.display()))?;
        let arc_skills: Vec<Arc<dyn harness_core::Skill>> = loaded
            .into_iter()
            .map(|s| Arc::new(s) as Arc<dyn harness_core::Skill>)
            .collect();
        server = server.with_skills(arc_skills);
    }

    server.serve_stdio(&mut world).await?;
    Ok(())
}

fn print_session_trace(file: PathBuf, summary_only: bool, verbose: bool) -> anyhow::Result<()> {
    let events = harness_loop::read_session(&file)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", file.display()))?;
    let stats = harness_loop::SessionStats::from(&events);

    if !summary_only {
        for e in &events {
            let formatted = if verbose {
                harness_loop::format_event_verbose(e)
            } else {
                harness_loop::format_event_short(e)
            };
            println!("{formatted}");
        }
        println!();
    }
    println!("── summary ────────────────────────────────────────────────");
    println!("  events:        {}", stats.events);
    println!("  model calls:   {}", stats.model_calls);
    println!("  tool calls:    {}", stats.tool_calls);
    println!("  compactions:   {}", stats.stages_run);
    println!("  iterations:    {}", stats.iters);
    println!(
        "  tokens:        {} in / {} out",
        stats.input_tokens, stats.output_tokens
    );
    println!("  duration:      {} ms", stats.duration_ms);
    Ok(())
}

fn scaffold_new_project(
    name: String,
    parent: Option<PathBuf>,
    workspace: Option<PathBuf>,
    local: bool,
) -> anyhow::Result<()> {
    // Validate name as a cargo package identifier — same rules as agentskills.io
    // (lowercase, hyphens, no leading/trailing hyphen) plus Rust's no-leading-digit.
    if name.is_empty() {
        anyhow::bail!("project name must not be empty");
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        anyhow::bail!("project name must not start with a digit");
    }
    if name.starts_with('-') || name.ends_with('-') {
        anyhow::bail!("project name must not start or end with `-`");
    }
    for c in name.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            anyhow::bail!("project name contains invalid char `{c}` — use [a-z0-9_-]");
        }
    }

    let root = parent.unwrap_or_else(|| std::env::current_dir().unwrap());
    let dir = root.join(&name);
    if dir.exists() {
        anyhow::bail!("`{}` already exists", dir.display());
    }
    std::fs::create_dir_all(dir.join("src"))?;

    // Resolve which local harness workspace to [patch] against, if any.
    let patch_root: Option<PathBuf> = match (workspace.as_ref(), local) {
        (Some(p), _) => Some(canon_workspace_root(p)?),
        (None, true) => Some(canon_workspace_root(&detect_local_workspace()?)?),
        (None, false) => None,
    };

    let patch_section = if let Some(root) = &patch_root {
        format!(
            r#"
[patch.crates-io]
harness-rs                = {{ path = "{root}/crates/harness" }}
harness-rs-core           = {{ path = "{root}/crates/harness-core" }}
harness-rs-loop           = {{ path = "{root}/crates/harness-loop" }}
harness-rs-models         = {{ path = "{root}/crates/harness-models" }}
harness-rs-tools-fs       = {{ path = "{root}/crates/harness-tools-fs" }}
harness-rs-tools-shell    = {{ path = "{root}/crates/harness-tools-shell" }}
harness-rs-context        = {{ path = "{root}/crates/harness-context" }}
harness-rs-skills         = {{ path = "{root}/crates/harness-skills" }}
harness-rs-macros         = {{ path = "{root}/crates/harness-macros" }}
harness-rs-hooks          = {{ path = "{root}/crates/harness-hooks" }}
harness-rs-compactor      = {{ path = "{root}/crates/harness-compactor" }}
harness-rs-blueprint      = {{ path = "{root}/crates/harness-blueprint" }}
harness-rs-sandbox        = {{ path = "{root}/crates/harness-sandbox" }}
harness-rs-sensors-rust   = {{ path = "{root}/crates/harness-sensors-rust" }}
harness-rs-sensors-common = {{ path = "{root}/crates/harness-sensors-common" }}
harness-rs-templates      = {{ path = "{root}/crates/harness-templates" }}
harness-rs-mcp            = {{ path = "{root}/crates/harness-mcp" }}
"#,
            root = root.display()
        )
    } else {
        String::new()
    };

    let cargo_toml = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
description = "An agent built with the harness framework."

[[bin]]
name = "{name}"
path = "src/main.rs"

[dependencies]
harness-rs           = "0.0.4"
harness-rs-core      = "0.0.4"
harness-rs-loop      = "0.0.4"
harness-rs-models    = "0.0.4"
harness-rs-tools-fs  = "0.0.4"
harness-rs-context   = "0.0.4"
tokio                = {{ version = "1", features = ["macros", "rt-multi-thread"] }}
anyhow               = "1"
serde_json           = "1"
{patch_section}"#
    );
    std::fs::write(dir.join("Cargo.toml"), cargo_toml)?;

    let main_rs = r###"//! Minimal harness-rs agent. Lists the current directory, then summarises.
//!
//! Run:
//!   DEEPSEEK_API_KEY=sk-…              cargo run    # default DeepSeek
//!   HARNESS_BASE_URL=https://… \
//!   HARNESS_MODEL=gpt-5.4 \
//!   HARNESS_API_KEY=sk-…                cargo run    # any OpenAI-compatible
//!   HARNESS_PROGRESS=1 cargo run                     # live stderr trace

use harness::prelude::*;
use harness_context::default_world;
use harness_loop::{AgentLoop, LiveProgressHook};
use harness_models::OpenAiCompat;
use harness_tools_fs::{ListDir, ReadFile};
use std::sync::Arc;

/// Echo any text the user provides. Use when the user asks the agent to repeat something verbatim.
#[harness::skill(
    name = "echo",
    harness(kind = "computational", risk = "read-only"),
)]
async fn echo(_ctx: &mut Context, _w: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

/// Reverse a string. Demonstrates a #[tool] with explicit JSON schema.
#[harness::tool(
    name = "reverse",
    risk = "read-only",
    schema = r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#,
)]
async fn reverse(
    args: serde_json::Value,
    _w: &mut harness::World,
) -> Result<harness::ToolResult, harness::ToolError> {
    let t = args["text"].as_str().unwrap_or("");
    Ok(harness::ToolResult {
        ok: true,
        content: serde_json::json!({ "reversed": t.chars().rev().collect::<String>() }),
        trace: None,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Env-var-driven endpoint config (works with any OpenAI-compatible API).
    let key = std::env::var("HARNESS_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .map_err(|_| anyhow::anyhow!("set HARNESS_API_KEY or DEEPSEEK_API_KEY"))?;
    let base_url = std::env::var("HARNESS_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com".to_string());
    let model_id =
        std::env::var("HARNESS_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into());

    let model = OpenAiCompat::with_key(base_url, model_id, key);
    let mut world = default_world(".");

    let mut loop_ = AgentLoop::new(model)
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(ReadFile));

    // Optional: live progress to stderr.
    if std::env::var("HARNESS_PROGRESS").is_ok() {
        loop_ = loop_.with_hook(Arc::new(LiveProgressHook::new()));
    }

    let task = Task {
        description: "List the top-level files here, then describe what you find in one sentence.".into(),
        source: None,
        deadline: None,
    };

    let outcome = loop_.run_with_max_iters(task, &mut world, 6).await?;
    println!("{outcome:?}");
    Ok(())
}
"###;
    std::fs::write(dir.join("src/main.rs"), main_rs)?;

    println!("✓ created {}/", dir.display());
    println!("  └─ Cargo.toml");
    println!("  └─ src/main.rs   (one #[skill], one #[tool], a minimal AgentLoop)");
    if let Some(root) = &patch_root {
        println!("  └─ [patch.crates-io] → {}", root.display());
    } else {
        println!();
        println!("Note: the framework isn't on crates.io yet. To build this project");
        println!("today, either re-run with `--local` (auto-detects the harness");
        println!("workspace from the installed binary) or `--workspace <path>`,");
        println!("or add a [patch.crates-io] section manually.");
    }
    println!();
    println!("Next steps:");
    println!("  cd {}", dir.display());
    println!("  export DEEPSEEK_API_KEY=…");
    println!("  cargo run");
    Ok(())
}

/// Canonicalize the user-supplied harness workspace path and verify it really
/// is a harness checkout (has `crates/harness-core` and `crates/harness`).
fn canon_workspace_root(p: &Path) -> anyhow::Result<PathBuf> {
    let c = p
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("workspace `{}`: {e}", p.display()))?;
    let core = c.join("crates/harness-core/Cargo.toml");
    let facade = c.join("crates/harness/Cargo.toml");
    if !core.exists() || !facade.exists() {
        anyhow::bail!(
            "{} does not look like a harness workspace (missing crates/harness-core or crates/harness)",
            c.display()
        );
    }
    Ok(c)
}

/// Try to find the harness workspace this binary was built from, by walking up
/// from `CARGO_MANIFEST_DIR` (captured at build time). Falls back to walking up
/// from the current dir, which catches the common case of running `--local`
/// while in a sibling shell of the checkout.
fn detect_local_workspace() -> anyhow::Result<PathBuf> {
    // Cargo captures the per-crate manifest dir at build time. For the
    // installed binary that's typically `…/crates/harness-cli`. Two `parent()`
    // calls take us to the workspace root.
    let cli_manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    if let Some(crates) = cli_manifest.parent()
        && let Some(root) = crates.parent()
        && root.join("crates/harness-core/Cargo.toml").exists()
    {
        return Ok(root.to_path_buf());
    }

    // Fallback: walk up from CWD looking for the marker.
    let mut cur = std::env::current_dir()?;
    loop {
        if cur.join("crates/harness-core/Cargo.toml").exists() {
            return Ok(cur);
        }
        if !cur.pop() {
            anyhow::bail!(
                "could not auto-detect a harness workspace; pass --workspace <path> explicitly"
            );
        }
    }
}

fn tracing_subscriber_init() {
    use tracing_subscriber::{EnvFilter, fmt};
    // Default to `warn` so real failures (model 401s, delivery errors, dropped
    // jobs) surface instead of vanishing; override verbosity with RUST_LOG,
    // e.g. `RUST_LOG=harness_scheduler=info,harness_loop=debug`. Logs go to
    // stderr so they never pollute `run --json` / MCP stdout.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
