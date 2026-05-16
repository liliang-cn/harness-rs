# harness-rs recipes

Copy-pasteable patterns. Each section is self-contained.

## 1. Typed `#[tool]` with serde-deserialised args

```rust
use harness_rs::{World, ToolError};
use harness_rs::ToolResult;
use serde::Deserialize;

#[derive(Deserialize)]
struct EchoArgs {
    msg: String,
    #[serde(default)]
    upper: bool,
}

#[harness_rs::tool(
    name = "echo",
    risk = "read-only",
    schema = r#"{
        "type": "object",
        "properties": {
            "msg":   {"type": "string"},
            "upper": {"type": "boolean", "default": false}
        },
        "required": ["msg"]
    }"#,
)]
async fn echo(args: serde_json::Value, _w: &mut World) -> Result<ToolResult, ToolError> {
    let a: EchoArgs = serde_json::from_value(args)
        .map_err(|e| ToolError::InvalidArgs(e.to_string()))?;
    let out = if a.upper { a.msg.to_uppercase() } else { a.msg };
    Ok(ToolResult {
        ok: true,
        content: serde_json::json!({"echoed": out}),
        trace: None,
    })
}
```

## 2. Self-correcting loop — sensor produces a `FixPatch`

The framework applies `FixPatch::{ReplaceFile, UnifiedDiff, RunCommand}` automatically without going through the model. After application, it pushes a Hint signal so the model is aware the file changed.

```rust
use harness_rs::{Action, Signal, Severity, FixPatch, World};
use harness_rs::SensorError;

#[harness_rs::sensor(id = "cargo-fmt", stage = "self-correct", kind = "computational")]
async fn cargo_fmt_sensor(action: &Action, w: &World) -> Result<Vec<Signal>, SensorError> {
    if action.tool != "write_file" && action.tool != "edit_file" {
        return Ok(vec![]);
    }
    // Always emit a RunCommand that runs `cargo fmt`. The loop applies it,
    // the model sees a Hint signal next iteration.
    Ok(vec![Signal {
        severity:   Severity::Hint,
        origin:     "cargo-fmt".into(),
        message:    "ensuring rustfmt".into(),
        agent_hint: None,
        auto_fix:   Some(FixPatch::RunCommand {
            program: "cargo".into(),
            args:    vec!["fmt".into(), "--all".into()],
            cwd:     None,
        }),
        location: None,
    }])
}
```

## 3. Blueprint: deterministic + agent hybrid (Stripe Minions pattern)

```rust
use harness_rs_blueprint::{Blueprint, Node, NodeOutput, Transition, FailurePolicy};
use harness_rs_sandbox::WorktreeSandbox;
use serde_json::json;

let bp = Blueprint::new(WorktreeSandbox::new(repo, "feat/x"))
    .add("read",       Node::deterministic(|w| Box::pin(async move {
        let out = w.runner.exec("git", &["log","--oneline","-20"], Some(w.repo.root.as_path())).await?;
        Ok(NodeOutput { transition: Transition::Next, data: json!({"recent": out.stdout}) })
    })))
    .add("implement",  Node::agent(/* tools + sensors + budget */))
    .add("fmt",        Node::deterministic(|w| Box::pin(async move {
        let _ = w.runner.exec("cargo", &["fmt","--all"], Some(w.repo.root.as_path())).await;
        Ok(NodeOutput { transition: Transition::Next, data: json!({}) })
    })))
    .add("test",       Node::deterministic(|w| Box::pin(async move {
        let out = w.runner.exec("cargo", &["nextest","run"], Some(w.repo.root.as_path())).await?;
        let t = if out.status == 0 { Transition::Next } else { Transition::Edge("fail".into()) };
        Ok(NodeOutput { transition: t, data: json!({}) })
    })))
    .add("commit",     Node::deterministic(|_w| Box::pin(async { todo!() })))
    .edge("read",      "implement")
    .edge("implement", "fmt")
    .edge("fmt",       "test")
    .edge("test",      "commit")
    .branch_on_failure("test", FailurePolicy { retry_cap: 2, fallback: Some("implement".into()) });

bp.run(task).await?;
```

Deterministic nodes never burn LLM tokens. The agent only runs when judgement is needed.

## 4. ModelBackedCompactor — real semantic compaction

Stage 3 (Microcompact) and Stage 5 (AutoCompact) call a cheap model to summarise old context, instead of the default structural-only collapsing.

```rust
use harness_rs_compactor::ModelBackedCompactor;
use harness_rs_models::{OpenAiCompat, providers::DEEPSEEK};
use std::sync::Arc;

let cheap = Arc::new(
    OpenAiCompat::with_key(DEEPSEEK, "deepseek-v4-flash", std::env::var("DEEPSEEK_API_KEY")?)
);
let compactor = ModelBackedCompactor::new(cheap);

AgentLoop::new(main_model)
    .with_compactor(Arc::new(compactor))
    .run(task, &mut world).await?;
```

## 5. Hook that denies destructive tool calls

```rust
use harness_rs::{Event, HookOutcome, World};

#[harness_rs::hook(event = "PreToolUse", name = "deny-destructive")]
fn deny_destructive(ev: &Event<'_>, _w: &mut World) -> HookOutcome {
    if let Event::PreToolUse { action } = ev {
        // Action carries Tool name; you can also look up the Tool's risk()
        if action.tool == "shell_exec" || action.tool == "edit_file" {
            // For shell_exec example: check args
            if let Some(prog) = action.args.get("program").and_then(|v| v.as_str()) {
                if prog == "rm" || prog == "git" {
                    return HookOutcome::Deny {
                        reason: format!("hook policy: {prog} blocked"),
                    };
                }
            }
        }
    }
    HookOutcome::Allow
}
```

The agent receives a tool-result with `denied_by_hook: <reason>` and continues.

## 6. Subagent isolation — delegate a focused task

```rust
use harness_rs_loop::{Subagent, SubagentSpec, SubagentStatus, AgentLoop};

let sub = Subagent::new(
    review_model.clone(),
    SubagentSpec {
        name:      "code-reviewer".into(),
        task:      Task::from("Review the diff in /tmp/diff.patch for security issues."),
        tools:     vec![Arc::new(ReadFile)],   // restricted toolset
        sensors:   vec![],
        max_iters: 8,
    },
);

let report = sub.run(&mut world).await?;
match report.status {
    SubagentStatus::Done             => println!("subagent ok: {:?}", report.text),
    SubagentStatus::DoneWithConcerns => println!("done with concerns: {:?}", report.text),
    SubagentStatus::Blocked          => eprintln!("subagent blocked"),
    SubagentStatus::NeedsContext     => eprintln!("subagent wants more context"),
}
```

Subagent runs an isolated AgentLoop with restricted tools; the parent sees a structured report.

## 7. Anthropic — thinking-block round-trip works automatically

```rust
use harness_rs_models::AnthropicNative;

let model = AnthropicNative::with_key("claude-opus-4-7", std::env::var("ANTHROPIC_API_KEY")?);
// AgentLoop uses this just like any other Model.
```

The adapter parses Anthropic's `thinking` and `redacted_thinking` content blocks and stores them as `Block::Reasoning(...)` in the conversation history. On the next turn, they're echoed back as proper `thinking` blocks so Anthropic's API doesn't reject the request. Same for DeepSeek's `reasoning_content` field via `OpenAiCompat`.

## 8. Live recording for production runs

```rust
use harness_rs_loop::SessionRecorder;
use std::sync::Arc;

let log_path = std::path::PathBuf::from(format!(
    "logs/agent-{}.jsonl",
    chrono::Local::now().format("%Y%m%d-%H%M%S")
));
let recorder = SessionRecorder::new(&log_path)?;

AgentLoop::new(model)
    .with_hook(Arc::new(recorder))
    .with_tool(Arc::new(ReadFile))
    .run(task, &mut world).await?;
```

Then offline:
- `harness trace logs/agent-*.jsonl` — human view
- `harness_rs_loop::replay_as_mock` — feed back into AgentLoop for deterministic regression tests

## 9. Custom workspace World

If `default_world(".")` isn't enough — e.g. you want to inject a fake clock or a recording runner — build a World by hand:

```rust
use harness_rs::{World, RepoView};
use harness_rs_context::{SystemClock, TokioRunner, InMemoryKv};
use std::sync::Arc;
use std::path::PathBuf;

let world = World {
    repo:   RepoView { root: PathBuf::from("/srv/projects/web") },
    runner: Arc::new(TokioRunner),
    clock:  Arc::new(SystemClock),
    kv:     Arc::new(InMemoryKv::new()),
};
```

Each field is a trait object — swap in mocks for tests.

## 10. Scheduled / background execution — separate `harness-daemon` binary

The agent binary itself stays request-response. Scheduling lives in a separate
optional crate so a crashed daemon doesn't crash your agent (and vice versa).

```bash
cargo install harness-rs-daemon
```

```toml
# ~/.config/harness/daemon.toml — one [[job]] per recurring task
[[job]]
name = "morning-brief"
schedule = "daily 08:00"          # or "weekly mon 09:30" or "every 5m"
argv = ["assistant", "--brief", "--tier", "flash"]
env  = { DEEPSEEK_API_KEY = "sk-...", HARNESS_USER_TZ = "Asia/Shanghai" }

[[job]]
name = "ingest-logs"
schedule = "every 10m"
argv = ["log-ingestor", "--source", "syslog"]
cwd  = "/srv/log-pipeline"

[[job]]
name = "disabled-for-now"
schedule = "weekly fri 18:00"
argv = ["weekly-summary"]
enabled = false
```

Usage:

```bash
harness-daemon                       # foreground, log to stderr/stdout, Ctrl-C to stop
harness-daemon --dry-run             # print "next fire" for each job, exit
harness-daemon --once morning-brief  # spawn that job RIGHT NOW with its configured argv+env
HARNESS_LOG=debug harness-daemon     # more verbose
```

### Building a Daemon programmatically (instead of TOML)

```rust
use harness_daemon::{Daemon, DaemonConfig, Job};

let cfg = DaemonConfig {
    jobs: vec![
        Job {
            name: "brief".into(),
            schedule: "daily 08:00".into(),
            argv: Some(vec!["assistant".into(), "--brief".into()]),
            command: None,
            env: [("DEEPSEEK_API_KEY".into(), key)].into(),
            cwd: None,
            enabled: true,
        },
    ],
};
Daemon::from_config(cfg)?.run().await?;
```

### Pairing with `launchd` (macOS) or `systemd` (Linux)

Run `harness-daemon` itself under the OS service manager so it survives reboots:

**macOS** `~/Library/LaunchAgents/com.harness.daemon.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.harness.daemon</string>
  <key>ProgramArguments</key>
  <array><string>/Users/me/.cargo/bin/harness-daemon</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/Users/me/.harness/daemon.log</string>
  <key>StandardErrorPath</key><string>/Users/me/.harness/daemon.err</string>
</dict></plist>
```
Then `launchctl load -w ~/Library/LaunchAgents/com.harness.daemon.plist`.

**Linux** `~/.config/systemd/user/harness-daemon.service`:

```ini
[Unit]
Description=harness-rs job scheduler
[Service]
ExecStart=%h/.cargo/bin/harness-daemon
Restart=always
[Install]
WantedBy=default.target
```
Then `systemctl --user enable --now harness-daemon`.

The agent binary doesn't know any of this exists. Clean separation.

