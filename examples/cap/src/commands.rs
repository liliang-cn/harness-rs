//! Slash commands, shared by both front-ends. This module only *parses* them
//! and holds the help text; each front-end acts on the parsed `Cmd` its own way.

/// A parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Help,
    Exit,
    /// Branch off into a fresh session.
    New,
    /// List stored sessions.
    Sessions,
    /// Switch to a stored session by id/path.
    Resume(String),
    /// List available skills.
    Skills,
    /// Show the active model(s).
    Model,
    /// Clear the conversation view.
    Clear,
    Unknown(String),
}

/// Every command as `(name, description)` — drives both `/help` and the TUI's
/// autocomplete popup, so they never drift apart.
pub const LIST: &[(&str, &str)] = &[
    ("/help", "show this help"),
    ("/new", "start a fresh session (branch off)"),
    ("/sessions", "list saved sessions"),
    ("/resume", "switch to a saved session — /resume <id>"),
    ("/skills", "list available skills"),
    ("/model", "show the active model(s)"),
    ("/clear", "clear the conversation view"),
    ("/exit", "quit  (also /quit, :q, Ctrl-C)"),
];

/// Commands whose name starts with `prefix` (prefix includes the leading `/`).
pub fn matching(prefix: &str) -> Vec<(&'static str, &'static str)> {
    LIST.iter()
        .filter(|(n, _)| n.starts_with(prefix))
        .copied()
        .collect()
}

/// Help text listing every command (shown by `/help`).
pub fn help_text() -> String {
    LIST.iter()
        .map(|(n, d)| format!("{n:<14} {d}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse a line as a slash command. Returns `None` for ordinary prompts.
pub fn parse(line: &str) -> Option<Cmd> {
    let l = line.trim();
    if l == ":q" {
        return Some(Cmd::Exit);
    }
    let rest = l.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let name = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").to_string();
    Some(match name {
        "help" | "h" | "?" => Cmd::Help,
        "exit" | "quit" | "q" => Cmd::Exit,
        "new" | "reset" => Cmd::New,
        "sessions" | "ls" => Cmd::Sessions,
        "resume" | "open" => Cmd::Resume(arg),
        "skills" => Cmd::Skills,
        "model" | "models" => Cmd::Model,
        "clear" | "cls" => Cmd::Clear,
        other => Cmd::Unknown(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_common_forms() {
        assert_eq!(parse("hello world"), None);
        assert_eq!(parse("/help"), Some(Cmd::Help));
        assert_eq!(parse("  /new "), Some(Cmd::New));
        assert_eq!(parse(":q"), Some(Cmd::Exit));
        assert_eq!(parse("/quit"), Some(Cmd::Exit));
        assert_eq!(
            parse("/resume cap-123"),
            Some(Cmd::Resume("cap-123".into()))
        );
        assert_eq!(parse("/resume"), Some(Cmd::Resume(String::new())));
        assert_eq!(parse("/nope"), Some(Cmd::Unknown("nope".into())));
    }
}
