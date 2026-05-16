//! Risk-classified shell tools.
//!
//! Two flavours:
//! - `ShellRead` — restricted to a readable allowlist (`cargo check`, `git status`, …)
//! - `ShellExec` — full subprocess. `Destructive` risk; surface explicitly.
//!
//! Both run via `world.runner` so they're trivially mockable in tests.

use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct ShellArgs {
    program: String,
    #[serde(default)]
    args:    Vec<String>,
    #[serde(default)]
    cwd:     Option<String>,
}

// ---------- ShellRead ----------

pub struct ShellRead;

/// Allowlist of program names that are safe to invoke through `ShellRead`.
/// `args` are not inspected — callers should still write tight schemas if they
/// pass `ShellRead` to an agent.
const READ_ALLOWLIST: &[&str] = &[
    "cargo",
    "git",
    "ls",
    "pwd",
    "rustc",
    "rustup",
    "rg",
    "fd",
    "wc",
    "find",
    "head",
    "tail",
    "cat",
    "grep",
];

static SHELL_READ_SCHEMA: Lazy<ToolSchema> = Lazy::new(|| ToolSchema {
    name: "shell_read".into(),
    description: format!(
        "Run an allowlisted read-only program. Allowed programs: {}. \
         Returns stdout/stderr/status.",
        READ_ALLOWLIST.join(", ")
    ),
    input: json!({
        "type": "object",
        "properties": {
            "program": {"type": "string"},
            "args":    {"type": "array", "items": {"type": "string"}},
            "cwd":     {"type": "string", "description": "Path relative to workspace root"}
        },
        "required": ["program"]
    }),
});

#[async_trait]
impl Tool for ShellRead {
    fn name(&self) -> &str { "shell_read" }
    fn schema(&self) -> &ToolSchema { &SHELL_READ_SCHEMA }
    fn risk(&self) -> ToolRisk { ToolRisk::ReadOnly }

    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let a: ShellArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs { name: self.name().into(), reason: e.to_string() })?;
        if !READ_ALLOWLIST.contains(&a.program.as_str()) {
            return Err(ToolError::Permission(format!(
                "`{}` is not in the read allowlist: {}",
                a.program,
                READ_ALLOWLIST.join(", ")
            )));
        }
        run(&a, world).await
    }
}

// ---------- ShellExec (destructive) ----------

pub struct ShellExec;

static SHELL_EXEC_SCHEMA: Lazy<ToolSchema> = Lazy::new(|| ToolSchema {
    name: "shell_exec".into(),
    description: "Run an arbitrary command in the workspace. Destructive — use sparingly. \
                  Returns stdout/stderr/status."
        .into(),
    input: json!({
        "type": "object",
        "properties": {
            "program": {"type": "string"},
            "args":    {"type": "array", "items": {"type": "string"}},
            "cwd":     {"type": "string"}
        },
        "required": ["program"]
    }),
});

#[async_trait]
impl Tool for ShellExec {
    fn name(&self) -> &str { "shell_exec" }
    fn schema(&self) -> &ToolSchema { &SHELL_EXEC_SCHEMA }
    fn risk(&self) -> ToolRisk { ToolRisk::Destructive }

    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let a: ShellArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs { name: self.name().into(), reason: e.to_string() })?;
        run(&a, world).await
    }
}

// ---------- shared dispatch ----------

async fn run(a: &ShellArgs, world: &mut World) -> Result<ToolResult, ToolError> {
    let args_ref: Vec<&str> = a.args.iter().map(String::as_str).collect();
    let cwd_buf;
    let cwd = if let Some(c) = &a.cwd {
        cwd_buf = world.repo.root.join(c);
        Some(cwd_buf.as_path())
    } else {
        Some(world.repo.root.as_path())
    };

    let out = world
        .runner
        .exec(&a.program, &args_ref, cwd)
        .await
        .map_err(|e| ToolError::Exec(format!("spawn `{}`: {e}", a.program)))?;

    // Truncate giant output so the model isn't drowned. Keep first 80 lines + last 40.
    let stdout = clip_for_model(&out.stdout);
    let stderr = clip_for_model(&out.stderr);

    Ok(ToolResult {
        ok: out.status == 0,
        content: json!({
            "status": out.status,
            "stdout": stdout,
            "stderr": stderr,
        }),
        trace: None,
    })
}

fn clip_for_model(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= 120 {
        return s.to_string();
    }
    let head = lines.iter().take(80).copied().collect::<Vec<&str>>().join("\n");
    let tail = lines
        .iter()
        .rev()
        .take(40)
        .copied()
        .collect::<Vec<&str>>()
        .into_iter()
        .rev()
        .collect::<Vec<&str>>()
        .join("\n");
    format!(
        "{head}\n... [{} lines clipped] ...\n{tail}",
        lines.len() - 120
    )
}
