//! Terminal UX + approval gate: streams model text, prints tool-activity lines,
//! and confirms mutating tools in NORMAL mode. One `Hook` drives the whole thing.

use harness_core::{Event, Hook, HookOutcome, World};
use std::collections::HashSet;
use std::sync::Mutex;

/// Whether a tool call should pass through the approval gate. Covers CAP's own
/// mutating tools plus any external MCP tool whose name carries a mutating verb,
/// so plugging in an MCP server can't silently bypass the gate.
pub fn is_risky(tool: &str) -> bool {
    matches!(tool, "hash_edit" | "write_file")
        || [
            "write", "save", "delete", "create", "exec", "update", "remove", "insert", "patch",
            "apply", "put", "post", "commit",
        ]
        .iter()
        .any(|v| tool.contains(v))
}

/// Streams model text, prints a tool-activity line, and — unless `yolo` —
/// previews then confirms every mutating tool (`hash_edit`, `write_file`, and
/// mutating MCP tools). `a` at the prompt approves that tool for the session.
pub struct CapUi {
    yolo: bool,
    allow_all: Mutex<HashSet<String>>,
}

impl CapUi {
    pub fn new(yolo: bool) -> Self {
        Self {
            yolo,
            allow_all: Mutex::new(HashSet::new()),
        }
    }
}

impl Hook for CapUi {
    fn name(&self) -> &str {
        "cap-ui"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::ModelTokenDelta { .. } | Event::PreToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _w: &mut World) -> HookOutcome {
        use std::io::Write;
        match ev {
            Event::ModelTokenDelta { text } => {
                print!("{text}");
                let _ = std::io::stdout().flush();
                HookOutcome::Allow
            }
            Event::PreToolUse { action } => {
                let risky = is_risky(&action.tool);
                eprintln!(
                    "\n  \x1b[2m⚙ {}{}\x1b[0m",
                    action.tool,
                    activity(&action.tool, &action.args)
                );
                if self.yolo || !risky || self.allow_all.lock().unwrap().contains(&action.tool) {
                    return HookOutcome::Allow;
                }
                eprint!("  apply {}? [y/N/a=always] ", action.tool);
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                match line.trim() {
                    "a" | "A" => {
                        self.allow_all.lock().unwrap().insert(action.tool.clone());
                        HookOutcome::Allow
                    }
                    "y" | "Y" | "yes" => HookOutcome::Allow,
                    _ => {
                        eprintln!("  \x1b[33m✗ skipped\x1b[0m");
                        HookOutcome::Deny {
                            reason: format!(
                                "user declined the {} action; do not retry it",
                                action.tool
                            ),
                        }
                    }
                }
            }
            _ => HookOutcome::Allow,
        }
    }
}

/// Compact preview of a tool call for the activity line / approval prompt.
fn activity(tool: &str, args: &serde_json::Value) -> String {
    let trunc = |t: &str, n: usize| {
        let t = t.replace('\n', "⏎");
        if t.chars().count() > n {
            format!("{}…", t.chars().take(n).collect::<String>())
        } else {
            t
        }
    };
    match tool {
        "hash_read" | "glob" | "list_dir" => format!(" {}", args["path"].as_str().unwrap_or("")),
        "grep" => format!(" /{}/", trunc(args["pattern"].as_str().unwrap_or(""), 40)),
        "write_file" => format!(
            " {} ({} bytes)",
            args["path"].as_str().unwrap_or(""),
            args["content"].as_str().map(|s| s.len()).unwrap_or(0)
        ),
        "hash_edit" => {
            let path = args["path"].as_str().unwrap_or("");
            let n = args["edits"].as_array().map(|a| a.len()).unwrap_or(0);
            let ops: Vec<String> = args["edits"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .take(4)
                        .map(|e| {
                            format!(
                                "{}@{}",
                                e["op"].as_str().unwrap_or("?"),
                                e["anchor"].as_str().unwrap_or("?")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!(" {path}  {n} op(s): {}", ops.join(", "))
        }
        _ => String::new(),
    }
}
