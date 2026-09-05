//! Ask another agent.
//!
//! Claude Code, Codex and the rest are agents in their own right: they read a
//! codebase, run commands, and write files. When the work in front of this
//! agent would be better done by one of them — a refactor across a tree it does
//! not know, a second opinion on a bug, a language its model is weak in — the
//! useful move is to ask, not to reimplement.
//!
//! Each is driven the same way: one non-interactive invocation that takes a
//! prompt and prints an answer. What differs is the flags, and **the flags are
//! the whole difficulty**. Every one of these CLIs is built for a person at a
//! terminal, and each has a way of stopping to ask that person something. In an
//! unattended run there is nobody to ask, so a missing flag does not produce an
//! error — it produces a process that waits forever, or one that quietly does
//! less than you asked. What is recorded in [`builtin`] was measured against
//! the installed CLIs, not read off `--help`, because `--help` does not mention
//! any of it.
//!
//! Two layers, so a host can take either:
//!
//! * [`run_agent`] — spawn one, wait, read the verdict. No policy of its own.
//! * [`AskAgentTool`] — that runner as a tool the model can call.
//!
//! A host with its own approval flow should wrap [`run_agent`] rather than use
//! the tool: this asks another agent to run commands and write files, which is
//! not a gentler thing than running them yourself.
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How to run one external agent once, non-interactively.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalAgent {
    /// What the model asks for.
    pub name: String,
    /// The executable. Looked up on `PATH` unless it contains a `/`.
    pub program: String,
    /// Its arguments. Exactly one must be the literal `{prompt}`, replaced by
    /// the question **as one whole argument** — never pasted into a shell
    /// string, so a prompt containing quotes or `$(…)` stays a prompt.
    pub args: Vec<String>,
    /// One line for the tool description: what this agent is good for.
    #[serde(default)]
    pub about: String,
    /// How to read its answer, and whether it worked.
    #[serde(default)]
    pub verdict: Verdict,
}

/// Where the truth about a run lives.
///
/// Exit status is not always it. A `claude` whose token has been revoked writes
/// "Failed to authenticate" as its answer, marks the result frame `is_error`,
/// and **exits zero**. A caller reading only the exit code hands an
/// authentication failure back to the model as the answer to its question —
/// which the model will then act on. So each agent says how to be read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// stdout is the answer, the exit status is the verdict. For agents with
    /// no machine-readable output.
    #[default]
    ExitCode,
    /// stdout is a JSON array of frames whose last one carries `is_error` and
    /// `result` — Claude Code's `--output-format json`.
    ClaudeJson,
}

/// The invocations we know, measured against the installed CLIs.
///
/// Three of these flags exist because of a specific measured refusal, and none
/// of them is suggested by `--help`:
///
/// * An agent's working directory is often a scratch directory: not a git
///   repository, and not one any of these CLIs has been told to trust.
///   `codex exec` answers *"Not inside a trusted directory and
///   --skip-git-repo-check was not specified"*; `gemini -p` answers *"not
///   running in a trusted directory"*. Both refuse before doing any work.
/// * Nobody can answer a permission prompt in an unattended run. Without
///   `--permission-mode bypassPermissions`, Claude Code stops at the first file
///   it wants to write and looks like it hung — which a probe that only asks a
///   question will never reveal.
/// * Codex takes `--dangerously-bypass-approvals-and-sandbox`, not
///   `--full-auto`: the latter gates MCP tool calls behind an approval that
///   cannot be answered without a terminal, so they silently never fire and the
///   agent works with fewer tools than it was given, without saying so.
pub fn builtin() -> Vec<ExternalAgent> {
    let a =
        |name: &str, program: &str, args: &[&str], about: &str, verdict: Verdict| ExternalAgent {
            name: name.into(),
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            about: about.into(),
            verdict,
        };
    vec![
        a(
            "claude",
            "claude",
            &[
                "-p",
                "--permission-mode",
                "bypassPermissions",
                "--output-format",
                "json",
                "{prompt}",
            ],
            "Claude Code — reads and edits a codebase, runs commands; good at large refactors \
             and at explaining unfamiliar code",
            Verdict::ClaudeJson,
        ),
        a(
            "codex",
            "codex",
            &[
                "exec",
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "{prompt}",
            ],
            "OpenAI Codex CLI — a second opinion on a bug, or an implementation done another way",
            Verdict::ExitCode,
        ),
        a(
            "gemini",
            "gemini",
            &[
                "--skip-trust",
                "--approval-mode",
                "auto_edit",
                "-p",
                "{prompt}",
            ],
            "Gemini CLI — long-context reading, cheap for sweeping a large tree",
            Verdict::ExitCode,
        ),
        a(
            "cursor",
            "cursor-agent",
            &["-p", "{prompt}"],
            "Cursor's agent, headless",
            Verdict::ExitCode,
        ),
        a(
            "opencode",
            "opencode",
            &["run", "{prompt}"],
            "OpenCode — an open-source coding agent",
            Verdict::ExitCode,
        ),
    ]
}

/// Where a host keeps its own agent definitions: `<dir>/agents.json`, a JSON
/// array of [`ExternalAgent`].
pub fn config_path(dir: &Path) -> PathBuf {
    dir.join("agents.json")
}

/// The agents this machine can actually run: ours, plus whatever
/// `<dir>/agents.json` adds or replaces.
///
/// A file entry with the same name **replaces** the built-in outright. That is
/// how a CLI whose flags changed gets fixed without waiting for a release —
/// and these flags do change.
///
/// Anything not installed is dropped. An agent named in a tool description but
/// missing from `PATH` costs the model a turn to discover.
pub fn available(dir: &Path) -> Vec<ExternalAgent> {
    let mut all = builtin();
    if let Ok(bytes) = std::fs::read(config_path(dir)) {
        match serde_json::from_slice::<Vec<ExternalAgent>>(&bytes) {
            Ok(extra) => {
                for e in extra {
                    match all.iter_mut().find(|a| a.name == e.name) {
                        Some(slot) => *slot = e,
                        None => all.push(e),
                    }
                }
            }
            Err(e) => {
                eprintln!("harness: agents.json unreadable ({e}); using the built-in agents")
            }
        }
    }
    all.retain(|a| which(&a.program).is_some());
    all
}

/// Resolve a program on `PATH` ourselves rather than letting the spawn fail:
/// "not installed" is a different answer from "it ran and said no", and the
/// caller should be told which one it got.
pub fn which(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// Output past this is cut from the FRONT: an agent's answer is at the end and
/// its progress chatter is at the beginning.
pub const MAX_OUTPUT: usize = 40_000;

fn tail(s: &str) -> (String, bool) {
    if s.len() <= MAX_OUTPUT {
        return (s.to_string(), false);
    }
    let cut = s.len() - MAX_OUTPUT;
    // Land on a char boundary — output is arbitrary UTF-8.
    let start = (cut..s.len())
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(s.len());
    (s[start..].to_string(), true)
}

/// What one external agent did.
#[derive(Debug, Clone)]
pub struct AgentRun {
    /// Whether the run is to be believed — see [`Verdict`].
    pub ok: bool,
    /// The answer, cut from the front if it was very long.
    pub answer: String,
    /// Its stderr, likewise. Progress chatter when things went well; the
    /// reason when they did not.
    pub stderr: String,
    pub exit_code: i32,
    pub truncated: bool,
    pub seconds: u64,
    /// Set when the agent was still running at the deadline and was killed.
    pub timed_out: bool,
}

/// Run one agent, once, and wait for it.
///
/// `cwd` is the directory it works in — it can read what is there and leave
/// files behind. `timeout` is a hard stop: on expiry the whole process group
/// is killed, because these agents start compilers and test runners of their
/// own and killing the direct child alone leaves those behind.
pub async fn run_agent(
    agent: &ExternalAgent,
    prompt: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<AgentRun, ToolError> {
    let argv = argv_for(agent, prompt);
    let mut cmd = tokio::process::Command::new(&agent.program);
    cmd.args(&argv)
        .current_dir(cwd)
        // Closed, never inherited. Some of these read stdin when it is a
        // terminal, and an unattended run would wait on input that is never
        // coming — a hang with no error anywhere.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);

    let started = std::time::Instant::now();
    let child = cmd
        .spawn()
        .map_err(|e| ToolError::Exec(format!("could not start {}: {e}", agent.program)))?;
    // Armed for the whole tree the agent starts; disarmed only if the agent
    // exits on its own. Both a timeout and a dropped future (a cancelled
    // run) take the group down through the guard's `Drop`.
    #[cfg(unix)]
    let guard = harness_context::GroupKill::arm(&child);

    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(r) => {
            let out = r.map_err(|e| ToolError::Exec(format!("{} failed: {e}", agent.name)))?;
            #[cfg(unix)]
            guard.disarm();
            out
        }
        Err(_) => {
            // The child is dropped with the timed-out future; the guard is
            // dropped at this return and takes the rest of the group with it.
            return Ok(AgentRun {
                ok: false,
                answer: String::new(),
                stderr: String::new(),
                exit_code: -1,
                truncated: false,
                seconds: started.elapsed().as_secs(),
                timed_out: true,
            });
        }
    };

    let raw = String::from_utf8_lossy(&out.stdout).into_owned();
    let (answer, believed) = match agent.verdict {
        Verdict::ExitCode => (raw, out.status.success()),
        Verdict::ClaudeJson => match read_claude_json(&raw) {
            Some((text, failed)) => (text, out.status.success() && !failed),
            // It did not answer in the shape it promised. Hand back what it did
            // say rather than nothing, and do not call it a success: from an
            // agent that always emits JSON, this is usually the CLI failing
            // before it started.
            None => (raw, false),
        },
    };
    let (answer, truncated) = tail(&answer);
    let (stderr, _) = tail(&String::from_utf8_lossy(&out.stderr));
    Ok(AgentRun {
        ok: believed,
        answer,
        stderr,
        exit_code: out.status.code().unwrap_or(-1),
        truncated,
        seconds: started.elapsed().as_secs(),
        timed_out: false,
    })
}

/// The exact command line an agent will be run with.
///
/// Exposed because a host that asks the user to approve this should show them
/// what they are approving, down to the flags.
pub fn argv_for(agent: &ExternalAgent, prompt: &str) -> Vec<String> {
    agent
        .args
        .iter()
        .map(|a| {
            if a == "{prompt}" {
                prompt.to_string()
            } else {
                a.clone()
            }
        })
        .collect()
}

/// Pull the answer and the verdict out of Claude Code's `--output-format json`:
/// an array of frames whose last one carries `result` and `is_error`.
///
/// `None` when it is not that shape at all.
pub fn read_claude_json(raw: &str) -> Option<(String, bool)> {
    let v: Value = serde_json::from_str(raw.trim()).ok()?;
    let last = match &v {
        Value::Array(a) => a.last()?,
        other => other,
    };
    let failed = last
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = last
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some((text, failed))
}

/// The default ceiling on one call. These are agents, not commands — a real
/// refactor takes minutes — but an unbounded wait inside an unattended task is
/// a task that never ends.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(900);

/// `ask_agent` — put a question, or a piece of work, to another agent.
///
/// Runs in `world.repo.root`, so the other agent sees the files this one wrote
/// and can leave files behind for it.
pub struct AskAgentTool {
    agents: Vec<ExternalAgent>,
    timeout: Duration,
    schema: ToolSchema,
}

impl AskAgentTool {
    /// `None` when no agent is available — a tool whose only possible answer is
    /// "nothing is installed" is worse than no tool, because the model has to
    /// spend a turn to find that out.
    pub fn new(agents: Vec<ExternalAgent>) -> Option<Self> {
        if agents.is_empty() {
            return None;
        }
        let menu = agents
            .iter()
            .map(|a| format!("`{}` — {}", a.name, a.about))
            .collect::<Vec<_>>()
            .join("; ");
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        Some(Self {
            schema: ToolSchema {
                name: "ask_agent".into(),
                description: format!(
                    "Hand a piece of work to another AI agent running on this machine and get \
                     its answer back. They work in the SAME directory you do, with their own \
                     model and their own tools — so they can read the files you wrote and leave \
                     files behind for you. Reach for this when the work is better done by one of \
                     them than by you: a second opinion on a bug you cannot pin down, a language \
                     or framework you are weak in, or a large mechanical change. Give a \
                     complete, self-contained brief — they cannot see this conversation and \
                     cannot ask you a follow-up question. Available here: {menu}. One call can \
                     take several minutes."
                ),
                input: json!({
                    "type": "object",
                    "properties": {
                        "agent": {"type": "string", "enum": names,
                                  "description": "Which agent to ask."},
                        "prompt": {"type": "string",
                                   "description": "The complete brief. Self-contained: the goal, \
                                                   the files involved, and what a good answer \
                                                   looks like."}
                    },
                    "required": ["agent", "prompt"]
                }),
            },
            agents,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// The agent this call names, or an error naming the ones that exist.
    pub fn pick(&self, want: &str) -> Result<&ExternalAgent, ToolError> {
        self.agents
            .iter()
            .find(|a| a.name == want)
            .ok_or_else(|| ToolError::InvalidArgs {
                name: "ask_agent".into(),
                reason: format!(
                    "no agent {want:?} here; available: {}",
                    self.agents
                        .iter()
                        .map(|a| a.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            })
    }
}

/// Read `agent` and `prompt` out of a tool call, or say what is missing.
pub fn parse_call(args: &Value) -> Result<(&str, &str), ToolError> {
    let agent = args.get("agent").and_then(Value::as_str).unwrap_or("");
    let prompt = args.get("prompt").and_then(Value::as_str).unwrap_or("");
    if prompt.trim().is_empty() {
        return Err(ToolError::InvalidArgs {
            name: "ask_agent".into(),
            reason: "prompt is required — the other agent cannot see this conversation".into(),
        });
    }
    Ok((agent, prompt))
}

/// One agent's run, as the model reads it.
pub fn describe(agent: &ExternalAgent, run: &AgentRun) -> ToolResult {
    ToolResult {
        ok: run.ok,
        content: json!({
            "agent": agent.name,
            "exit_code": run.exit_code,
            "answer": run.answer,
            // Only when it went wrong: an agent's stderr is progress chatter,
            // and handing that back on success is noise paid for by the token.
            "stderr": if run.ok { String::new() } else { run.stderr.clone() },
            "timed_out": run.timed_out,
            "truncated_from_the_start": run.truncated,
            "seconds": run.seconds,
        }),
        trace: Some(format!(
            "asked {} ({}s, exit {})",
            agent.name, run.seconds, run.exit_code
        )),
    }
}

#[async_trait::async_trait]
impl Tool for AskAgentTool {
    fn name(&self) -> &str {
        &self.schema.name
    }
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    fn risk(&self) -> ToolRisk {
        // It starts a program that writes files and runs commands of its own.
        ToolRisk::Destructive
    }

    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError> {
        let (want, prompt) = parse_call(&args)?;
        let agent = self.pick(want)?;
        let run = run_agent(agent, prompt, &world.repo.root, self.timeout).await?;
        Ok(describe(agent, &run))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_takes_the_prompt_as_exactly_one_argument() {
        for a in builtin() {
            assert_eq!(
                a.args.iter().filter(|x| *x == "{prompt}").count(),
                1,
                "{}: the prompt must be one argument, never interpolated into a string",
                a.name
            );
        }
    }

    // Flags that are invisible in a probe and fatal in a real task: without
    // them the agent stops at the first file it wants to write, with nobody
    // able to answer, and looks like it hung.
    #[test]
    fn the_headless_agents_do_not_wait_for_an_approval_nobody_can_give() {
        let by = |n: &str| builtin().into_iter().find(|a| a.name == n).unwrap();
        let claude = by("claude");
        assert!(claude.args.iter().any(|a| a == "bypassPermissions"));
        assert_eq!(claude.verdict, Verdict::ClaudeJson);
        let codex = by("codex");
        assert!(
            codex
                .args
                .iter()
                .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"),
            "--full-auto would gate its MCP calls behind an unanswerable prompt"
        );
        assert!(codex.args.iter().any(|a| a == "--skip-git-repo-check"));
        assert!(by("gemini").args.iter().any(|a| a == "--skip-trust"));
    }

    // The defect this exists for: an expired login is reported by Claude Code
    // as an ordinary answer, with `is_error` set and an exit status of zero.
    #[test]
    fn a_claude_auth_failure_is_a_failure_even_though_it_exited_zero() {
        let raw = r#"[{"type":"system"},
                      {"type":"result","subtype":"error_during_execution",
                       "is_error":true,"result":"Failed to authenticate"}]"#;
        assert_eq!(
            read_claude_json(raw),
            Some(("Failed to authenticate".into(), true))
        );
    }

    #[test]
    fn a_claude_success_yields_the_answer_alone() {
        let raw = r#"[{"type":"system"},{"type":"result","is_error":false,"result":"PROBE-OK"}]"#;
        assert_eq!(read_claude_json(raw), Some(("PROBE-OK".into(), false)));
    }

    #[test]
    fn output_that_is_not_json_at_all_is_not_mistaken_for_a_verdict() {
        assert_eq!(read_claude_json("command not found"), None);
        assert_eq!(read_claude_json(""), None);
    }

    // A prompt is data. If it were pasted into a command line, one containing
    // `$(rm -rf ~)` would be a command.
    #[test]
    fn the_prompt_stays_one_argument_however_it_is_written() {
        let claude = builtin().into_iter().find(|a| a.name == "claude").unwrap();
        let nasty = "hi; rm -rf ~ $(whoami) `id` \"quoted\"";
        let argv = argv_for(&claude, nasty);
        assert_eq!(argv.iter().filter(|a| a.as_str() == nasty).count(), 1);
        assert!(!argv.iter().any(|a| a.contains("rm -rf") && a != nasty));
    }

    #[test]
    fn truncation_keeps_the_end_and_says_so() {
        let long = "x".repeat(MAX_OUTPUT + 500) + "THE ANSWER";
        let (t, cut) = tail(&long);
        assert!(cut);
        assert!(t.ends_with("THE ANSWER"));
        assert!(t.len() <= MAX_OUTPUT);
        assert_eq!(tail("hello"), ("hello".into(), false));
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        let long = "语".repeat(MAX_OUTPUT);
        let (t, cut) = tail(&long);
        assert!(cut);
        assert!(t.starts_with('语'), "cut landed inside a character");
    }

    #[test]
    fn an_agent_that_is_not_installed_is_not_offered() {
        for a in available(&std::env::temp_dir()) {
            assert!(which(&a.program).is_some(), "{} is not on PATH", a.name);
        }
    }

    // agents.json is how a user fixes a CLI whose flags changed, so a same-name
    // entry has to win outright rather than sit alongside ours.
    #[test]
    fn a_user_entry_replaces_the_builtin_of_the_same_name() {
        let dir = std::env::temp_dir().join(format!("harness-agents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            config_path(&dir),
            serde_json::to_vec(&vec![ExternalAgent {
                name: "claude".into(),
                // Something that certainly exists, so it survives the
                // installed-only filter.
                program: "sh".into(),
                args: vec!["-c".into(), "{prompt}".into()],
                about: "overridden".into(),
                verdict: Verdict::ExitCode,
            }])
            .unwrap(),
        )
        .unwrap();
        let got = available(&dir);
        let claude = got.iter().find(|a| a.name == "claude").unwrap();
        assert_eq!(claude.program, "sh");
        assert_eq!(got.iter().filter(|a| a.name == "claude").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_run_reports_the_answer_and_the_exit_status() {
        let agent = ExternalAgent {
            name: "echo".into(),
            program: "sh".into(),
            args: vec!["-c".into(), "{prompt}".into()],
            about: String::new(),
            verdict: Verdict::ExitCode,
        };
        let run = run_agent(
            &agent,
            "printf hello",
            &std::env::temp_dir(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(run.ok);
        assert_eq!(run.answer, "hello");
        assert_eq!(run.exit_code, 0);

        let bad = run_agent(
            &agent,
            "exit 7",
            &std::env::temp_dir(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert!(!bad.ok);
        assert_eq!(bad.exit_code, 7);
    }

    // A wedged agent must not wedge the task with it, and the timeout has to
    // reach the whole tree it started, not just the process we can see.
    #[tokio::test]
    async fn a_run_that_overruns_is_killed_and_says_so() {
        let agent = ExternalAgent {
            name: "sleeper".into(),
            program: "sh".into(),
            args: vec!["-c".into(), "{prompt}".into()],
            about: String::new(),
            verdict: Verdict::ExitCode,
        };
        let run = run_agent(
            &agent,
            "sleep 30",
            &std::env::temp_dir(),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert!(run.timed_out);
        assert!(!run.ok);
    }

    // A cancelled run drops `run_agent`'s future mid-flight. That has to
    // reach the agent's whole process tree the same way the timeout does, or
    // the compilers it started keep running under a run that says it stopped.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_dropped_run_kills_the_agents_process_group() {
        let pidfile = std::env::temp_dir().join(format!(
            "harness-agent-drop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agent = ExternalAgent {
            name: "forker".into(),
            program: "sh".into(),
            args: vec!["-c".into(), "{prompt}".into()],
            about: String::new(),
            verdict: Verdict::ExitCode,
        };
        let prompt = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let handle = tokio::spawn(async move {
            run_agent(
                &agent,
                &prompt,
                &std::env::temp_dir(),
                Duration::from_secs(30),
            )
            .await
        });

        let grandchild: i32 = loop {
            if let Some(p) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let alive = |pid: i32| unsafe { libc::kill(pid, 0) == 0 };
        assert!(
            alive(grandchild),
            "grandchild should be running before the drop"
        );

        handle.abort();
        let _ = handle.await;

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while alive(grandchild) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_file(&pidfile);
        assert!(
            !alive(grandchild),
            "the grandchild survived the drop: only the direct child was killed"
        );
    }

    #[test]
    fn a_call_without_a_prompt_is_refused_before_anything_is_started() {
        assert!(parse_call(&json!({"agent": "claude"})).is_err());
        assert!(parse_call(&json!({"agent": "claude", "prompt": "   "})).is_err());
        assert_eq!(
            parse_call(&json!({"agent": "claude", "prompt": "do it"})).unwrap(),
            ("claude", "do it")
        );
    }
}
