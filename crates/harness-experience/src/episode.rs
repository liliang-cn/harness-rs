//! An `Episode` — one unit of experience: the situation faced, the tools used
//! to handle it, and the outcome. Episodes are what the experience layer
//! records and recalls.

use serde::{Deserialize, Serialize};

/// One remembered experience.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    /// What the agent was asked / the situation it faced (the recall key).
    pub situation: String,
    /// Names of the tools called while handling it, in first-seen order.
    pub tools: Vec<String>,
    /// How it turned out — the approach / final answer, summarized.
    pub outcome: String,
    /// Extra retrieval tags (beyond the automatic ones).
    #[serde(default)]
    pub tags: Vec<String>,
}

const SITUATION_PREFIX: &str = "Situation: ";
const TOOLS_PREFIX: &str = "Tools used: ";
const OUTCOME_PREFIX: &str = "Outcome: ";

impl Episode {
    pub fn new(situation: impl Into<String>, outcome: impl Into<String>) -> Self {
        Self {
            situation: situation.into(),
            tools: Vec::new(),
            outcome: outcome.into(),
            tags: Vec::new(),
        }
    }

    pub fn with_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_tags<I, S>(mut self, tags: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.tags = tags.into_iter().map(Into::into).collect();
        self
    }

    /// Natural-language rendering used as the searchable memory content.
    /// Keyword/semantic backends index this text; [`Episode::parse`] reverses it.
    pub fn render(&self) -> String {
        let tools = if self.tools.is_empty() {
            "(none)".to_string()
        } else {
            self.tools.join(", ")
        };
        format!(
            "{SITUATION_PREFIX}{}\n{TOOLS_PREFIX}{tools}\n{OUTCOME_PREFIX}{}",
            self.situation.trim(),
            self.outcome.trim(),
        )
    }

    /// Best-effort reconstruction from [`Episode::render`] output. Unknown /
    /// malformed text yields `None`.
    pub fn parse(text: &str) -> Option<Episode> {
        let situation = line_after(text, SITUATION_PREFIX)?;
        let tools_line = line_after(text, TOOLS_PREFIX).unwrap_or_default();
        let tools = if tools_line.trim().is_empty() || tools_line.trim() == "(none)" {
            Vec::new()
        } else {
            tools_line
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        // Outcome is the last field: take everything after its prefix.
        let outcome = text
            .find(OUTCOME_PREFIX)
            .map(|i| text[i + OUTCOME_PREFIX.len()..].trim().to_string())
            .unwrap_or_default();
        Some(Episode {
            situation,
            tools,
            outcome,
            tags: Vec::new(),
        })
    }
}

/// Return the content of the first line starting with `prefix`.
fn line_after(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find(|l| l.starts_with(prefix))
        .map(|l| l[prefix.len()..].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_parse_roundtrip() {
        let ep = Episode::new(
            "user asked to deploy the site",
            "ran deploy.sh, site is live",
        )
        .with_tools(["read_file", "shell", "shell"]);
        let text = ep.render();
        assert!(text.contains("Tools used: read_file, shell, shell"));
        let back = Episode::parse(&text).unwrap();
        assert_eq!(back.situation, "user asked to deploy the site");
        assert_eq!(back.tools, vec!["read_file", "shell", "shell"]);
        assert_eq!(back.outcome, "ran deploy.sh, site is live");
    }

    #[test]
    fn parse_no_tools() {
        let ep = Episode::new("simple question", "answered directly");
        let back = Episode::parse(&ep.render()).unwrap();
        assert!(back.tools.is_empty());
    }
}
