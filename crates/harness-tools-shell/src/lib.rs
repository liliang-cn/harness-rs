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

/// Per-program safe-argument matchers. `ShellRead` will refuse any program
/// not listed here AND any args that fail the matcher.
///
/// Returns `Ok(())` if `args` are safe for `program`, else an error message.
fn check_safe_args(program: &str, args: &[String]) -> Result<(), String> {
    match program {
        "cargo" => match args.first().map(String::as_str) {
            Some("check" | "test" | "build" | "fmt" | "clippy" | "doc" | "tree"
                 | "metadata" | "search" | "audit" | "deny" | "outdated"
                 | "bench" | "nextest" | "vendor") => Ok(()),
            Some("install" | "uninstall" | "publish" | "yank" | "owner"
                 | "login" | "logout" | "package") => {
                Err(format!("`cargo {}` is not read-only", args[0]))
            }
            Some(s) => Err(format!(
                "`cargo {s}` not in shell_read subcommand allowlist (use shell_exec for writes)"
            )),
            None => Err("cargo needs a subcommand".into()),
        },
        "git" => match args.first().map(String::as_str) {
            Some("status" | "log" | "show" | "diff" | "blame" | "rev-parse"
                 | "ls-files" | "ls-tree" | "describe" | "branch"
                 | "remote" | "config" | "shortlog" | "tag") => {
                // Reject `git config <key> <value>` (write) — accept `git config <key>` (read).
                if args[0] == "config" && args.len() >= 3 && !args[1].starts_with('-') {
                    Err("`git config <k> <v>` is a write — use shell_exec".into())
                } else {
                    Ok(())
                }
            }
            Some(s) => Err(format!("`git {s}` is not in the read-only subcommand list")),
            None => Err("git needs a subcommand".into()),
        },
        // Language tool-chain inspectors: `--version`, `info`, `list`, etc.
        // are universally safe; install/publish/run are NOT (use shell_exec).
        "npm" | "pnpm" | "yarn" | "bun" => match args.first().map(String::as_str) {
            Some("ls" | "list" | "view" | "info" | "config" | "outdated" | "audit"
                 | "doctor" | "search" | "ping" | "whoami" | "--version" | "-v") => Ok(()),
            Some("install" | "i" | "add" | "uninstall" | "remove" | "rm"
                 | "publish" | "pack" | "run" | "exec" | "dlx" | "create"
                 | "link" | "unlink" | "version" | "deprecate" | "owner"
                 | "login" | "logout" | "init") =>
                Err(format!("`{program} {}` mutates state; use shell_exec", args[0])),
            Some(s) => Err(format!("`{program} {s}` not in shell_read allowlist")),
            None    => Err(format!("`{program}` needs a subcommand")),
        },
        "python" | "python3" | "uv" | "pip" | "pip3" => match args.first().map(String::as_str) {
            // Read-only enquiry forms only.
            Some("--version" | "-V") => Ok(()),
            // `pip list / show / check / config get` are read; install/uninstall/wheel are NOT.
            Some("list" | "show" | "check" | "freeze" | "config" | "search" | "index"
                 | "--help") if program.starts_with("pip") => {
                if args[0] == "config"
                    && args.iter().skip(1).any(|a| matches!(a.as_str(), "set" | "unset" | "edit")) {
                    Err("`pip config set/unset/edit` mutates state".into())
                } else { Ok(()) }
            }
            // Reject everything else for python/uv — `python -c` and `python script.py` execute arbitrary code.
            Some(_s) => Err(format!(
                "`{program}` runs arbitrary code via shell_read — use shell_exec or wrap in a Rust tool"
            )),
            None => Err(format!("`{program}` needs a subcommand")),
        },
        "node" | "deno" | "bun" if false => unreachable!(), // covered below for node/deno separately
        "node" | "deno" => match args.first().map(String::as_str) {
            Some("--version" | "-v") => Ok(()),
            _ => Err(format!("`{program}` evaluates arbitrary code — use shell_exec")),
        },
        "go" => match args.first().map(String::as_str) {
            Some("version" | "env" | "list" | "vet" | "doc" | "fmt" | "mod") => {
                if args[0] == "mod"
                    && args.iter().skip(1).any(|a| matches!(a.as_str(), "init" | "tidy" | "edit" | "download" | "vendor")) {
                    Err("`go mod init/tidy/...` mutates state".into())
                } else { Ok(()) }
            }
            Some("test" | "build" | "run" | "install" | "get" | "generate") =>
                Err(format!("`go {}` builds/installs; use shell_exec", args[0])),
            Some(s) => Err(format!("`go {s}` not in shell_read allowlist")),
            None    => Err("go needs a subcommand".into()),
        },
        "make" => match args.first().map(String::as_str) {
            Some("--version" | "-n" | "--dry-run") => Ok(()),
            _ => Err("`make` runs arbitrary targets — use shell_exec".into()),
        },
        "docker" | "podman" | "kubectl" => match args.first().map(String::as_str) {
            // Read-only container/k8s inspection.
            Some("ps" | "images" | "version" | "info" | "history" | "inspect"
                 | "logs" | "stats" | "top" | "port" | "diff" | "search") => Ok(()),
            Some("get" | "describe" | "explain" | "config" | "version" | "api-resources"
                 | "api-versions" | "cluster-info" | "top" | "events") if program == "kubectl" => {
                if args[0] == "config"
                    && args.iter().skip(1).any(|a| matches!(a.as_str(), "set" | "set-cluster" | "set-context" | "delete-context" | "use-context")) {
                    Err("`kubectl config set/...` mutates state".into())
                } else { Ok(()) }
            }
            Some(s) => Err(format!("`{program} {s}` not in shell_read allowlist")),
            None    => Err(format!("`{program}` needs a subcommand")),
        },
        // Pure read commands — no arg filter beyond not allowing -exec
        "ls" | "pwd" | "rustc" | "rustup" | "rg" | "fd" | "wc"
        | "head" | "tail" | "cat" | "grep" | "tree" | "stat" | "file"
        | "du" | "df" | "ps" | "uname" | "hostname" | "date" | "env" | "which" | "whereis" => {
            // Block xargs-style execution hand-offs.
            if args.iter().any(|a| a.contains("-exec") || a.contains("--exec")) {
                Err(format!("`{program}` with -exec is not allowed via shell_read"))
            } else {
                Ok(())
            }
        }
        "find" => {
            // `find -exec`, `-delete`, `-fprint` are write-equivalent.
            for a in args {
                let lower = a.as_str();
                if matches!(lower, "-exec" | "-execdir" | "-delete" | "-fprint" | "-fprintf" | "-ok" | "-okdir") {
                    return Err(format!("`find {lower}` mutates state; use shell_exec"));
                }
            }
            Ok(())
        }
        other => Err(format!("`{other}` is not in the read program allowlist")),
    }
}

/// Programs that pass the program-name gate (the args are still validated per-program).
const READ_PROGRAMS: &[&str] = &[
    "cargo", "git", "ls", "pwd", "rustc", "rustup", "rg", "fd",
    "wc", "find", "head", "tail", "cat", "grep", "tree", "stat", "file",
    "du", "df", "ps", "uname", "hostname", "date", "env", "which", "whereis",
    "npm", "pnpm", "yarn", "bun",
    "python", "python3", "uv", "pip", "pip3",
    "node", "deno",
    "go", "make",
    "docker", "podman", "kubectl",
];

static SHELL_READ_SCHEMA: Lazy<ToolSchema> = Lazy::new(|| ToolSchema {
    name: "shell_read".into(),
    description: format!(
        "Run a read-only program. Allowed programs: {}. Each program has a \
         curated allowlist of safe subcommands (cargo check/test/clippy/fmt; \
         git status/log/diff/blame; etc.). Write-equivalents like \
         `cargo install`, `git config <k> <v>`, `find -exec/-delete` are rejected.",
        READ_PROGRAMS.join(", ")
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
        check_safe_args(&a.program, &a.args)
            .map_err(ToolError::Permission)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cargo_check_is_safe() {
        check_safe_args("cargo", &args(&["check"])).unwrap();
        check_safe_args("cargo", &args(&["test", "--all"])).unwrap();
        check_safe_args("cargo", &args(&["clippy", "--", "-D", "warnings"])).unwrap();
    }

    #[test]
    fn cargo_install_blocked() {
        assert!(check_safe_args("cargo", &args(&["install", "ripgrep"])).is_err());
        assert!(check_safe_args("cargo", &args(&["publish"])).is_err());
        assert!(check_safe_args("cargo", &args(&["yank", "0.1.0"])).is_err());
    }

    #[test]
    fn git_config_read_vs_write() {
        // Read: `git config user.email`
        check_safe_args("git", &args(&["config", "user.email"])).unwrap();
        // Write: `git config user.email evil@x` → blocked
        assert!(
            check_safe_args("git", &args(&["config", "user.email", "evil@x"])).is_err()
        );
        // Flag-prefixed (like --list) is allowed
        check_safe_args("git", &args(&["config", "--list"])).unwrap();
    }

    #[test]
    fn find_exec_blocked() {
        assert!(check_safe_args(
            "find",
            &args(&[".", "-name", "*.rs", "-exec", "rm", "{}", ";"])
        )
        .is_err());
        check_safe_args("find", &args(&[".", "-name", "*.rs"])).unwrap();
    }

    #[test]
    fn unknown_program_blocked() {
        assert!(check_safe_args("sudo", &args(&["rm", "-rf", "/"])).is_err());
        assert!(check_safe_args("curl", &args(&["evil.com"])).is_err());
    }
}
