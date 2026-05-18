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
    }
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
harness            = {{ path = "{root}/crates/harness" }}
harness-core       = {{ path = "{root}/crates/harness-core" }}
harness-loop       = {{ path = "{root}/crates/harness-loop" }}
harness-models     = {{ path = "{root}/crates/harness-models" }}
harness-tools-fs   = {{ path = "{root}/crates/harness-tools-fs" }}
harness-tools-shell= {{ path = "{root}/crates/harness-tools-shell" }}
harness-context    = {{ path = "{root}/crates/harness-context" }}
harness-skills     = {{ path = "{root}/crates/harness-skills" }}
harness-macros     = {{ path = "{root}/crates/harness-macros" }}
harness-hooks      = {{ path = "{root}/crates/harness-hooks" }}
harness-compactor  = {{ path = "{root}/crates/harness-compactor" }}
harness-blueprint  = {{ path = "{root}/crates/harness-blueprint" }}
harness-sandbox    = {{ path = "{root}/crates/harness-sandbox" }}
harness-sensors-rust   = {{ path = "{root}/crates/harness-sensors-rust" }}
harness-sensors-common = {{ path = "{root}/crates/harness-sensors-common" }}
harness-templates  = {{ path = "{root}/crates/harness-templates" }}
harness-mcp        = {{ path = "{root}/crates/harness-mcp" }}
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
harness        = "0.0.1"
harness-core   = "0.0.1"
harness-loop   = "0.0.1"
harness-models = "0.0.1"
harness-tools-fs = "0.0.1"
harness-context = "0.0.1"
tokio          = {{ version = "1", features = ["macros", "rt-multi-thread"] }}
anyhow         = "1"
serde_json     = "1"
{patch_section}"#
    );
    std::fs::write(dir.join("Cargo.toml"), cargo_toml)?;

    let main_rs = r###"//! Minimal harness agent. Reads a file the model asks for, then summarises.

use harness::prelude::*;
use harness_context::default_world;
use harness_loop::AgentLoop;
use harness_models::{OpenAiCompat, providers};
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
    let key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| anyhow::anyhow!("set DEEPSEEK_API_KEY first"))?;
    let model = OpenAiCompat::with_key(providers::DEEPSEEK, "deepseek-v4-flash", key);
    let mut world = default_world(".");

    let task = Task {
        description: "List the top-level files here, then describe what you find in one sentence.".into(),
        source: None,
        deadline: None,
    };

    let outcome = AgentLoop::new(model)
        .with_tool(Arc::new(ListDir))
        .with_tool(Arc::new(ReadFile))
        .run_with_max_iters(task, &mut world, 6)
        .await?;

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
    // intentionally minimal; replace with tracing-subscriber later
    let _ = tracing::subscriber::set_global_default(tracing::subscriber::NoSubscriber::default());
}
