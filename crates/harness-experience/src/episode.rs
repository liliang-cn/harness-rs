//! An `Episode` — one unit of experience: the situation faced, the tools used
//! to handle it, and the outcome. Episodes are what the experience layer
//! records and recalls.
//!
//! ## Two late additions, and why they are where they are
//!
//! `skills` and `success` were added for the closed learning loop (see
//! [`crate::SkillDistiller`] / [`crate::SkillReviser`]): the distiller only
//! fires on runs that *worked*, and the reviser only fires on a skill the run
//! actually *followed*. Both are recorded per-run and are useless if they can't
//! survive the memory round-trip.
//!
//! They therefore ride in [`Episode::render`] as extra lines placed **before**
//! `Outcome:`, never after. `Outcome:` is the terminal field — [`Episode::parse`]
//! takes everything from its prefix to end-of-text — so a field appended after
//! it would be swallowed into the outcome string. Putting them before keeps the
//! text format compatible in *both* directions: old text (no `Skills used:` /
//! `Result:` lines) parses into `skills: []` / `success: None`, and text written
//! by this version still parses correctly under the previous prefix-scanning
//! parser. On the serde side both fields are `#[serde(default)]`, so episodes
//! already sitting in a JSONL/SQLite memory deserialize unchanged.

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
    /// Names of the skills the run followed, if any. Populated by the host (or
    /// by [`crate::SkillUseTrace`]); the framework emits no skill-activation
    /// event of its own, so nothing fills this automatically.
    ///
    /// This is the join key the reviser needs: "skill `deploy-runbook` was in
    /// play when this run went wrong".
    #[serde(default)]
    pub skills: Vec<String>,
    /// Did the run achieve what it was asked to? `None` = the host never said.
    ///
    /// Deliberately three-valued. Distillation treats "unknown" like "no": a
    /// skill minted from a run nobody confirmed worked is exactly how bad
    /// procedural memory is born.
    #[serde(default)]
    pub success: Option<bool>,
}

const SITUATION_PREFIX: &str = "Situation: ";
const TOOLS_PREFIX: &str = "Tools used: ";
const SKILLS_PREFIX: &str = "Skills used: ";
const RESULT_PREFIX: &str = "Result: ";
const OUTCOME_PREFIX: &str = "Outcome: ";

impl Episode {
    pub fn new(situation: impl Into<String>, outcome: impl Into<String>) -> Self {
        Self {
            situation: situation.into(),
            tools: Vec::new(),
            outcome: outcome.into(),
            tags: Vec::new(),
            skills: Vec::new(),
            success: None,
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

    /// Names of the skills the run followed. See [`Episode::skills`].
    pub fn with_skills<I, S>(mut self, skills: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.skills = skills.into_iter().map(Into::into).collect();
        self
    }

    /// Whether the run achieved what it was asked to. See [`Episode::success`].
    pub fn with_success(mut self, success: bool) -> Self {
        self.success = Some(success);
        self
    }

    /// Convenience for the distiller's gate: `true` only when the host
    /// explicitly said the run worked. Unknown counts as "no".
    pub fn succeeded(&self) -> bool {
        self.success == Some(true)
    }

    /// Natural-language rendering used as the searchable memory content.
    /// Keyword/semantic backends index this text; [`Episode::parse`] reverses it.
    ///
    /// `Skills used:` / `Result:` are emitted only when set, and always *before*
    /// `Outcome:` — see the module docs for why that ordering is load-bearing.
    pub fn render(&self) -> String {
        let tools = if self.tools.is_empty() {
            "(none)".to_string()
        } else {
            self.tools.join(", ")
        };
        let mut s = format!(
            "{SITUATION_PREFIX}{}\n{TOOLS_PREFIX}{tools}\n",
            self.situation.trim(),
        );
        if !self.skills.is_empty() {
            s.push_str(&format!("{SKILLS_PREFIX}{}\n", self.skills.join(", ")));
        }
        if let Some(ok) = self.success {
            s.push_str(&format!(
                "{RESULT_PREFIX}{}\n",
                if ok { "success" } else { "failure" }
            ));
        }
        s.push_str(&format!("{OUTCOME_PREFIX}{}", self.outcome.trim()));
        s
    }

    /// Best-effort reconstruction from [`Episode::render`] output. Unknown /
    /// malformed text yields `None`.
    ///
    /// Text written before `skills`/`success` existed parses fine: the two new
    /// prefixes simply aren't found, and the fields come back empty/`None`.
    pub fn parse(text: &str) -> Option<Episode> {
        let situation = line_after(text, SITUATION_PREFIX)?;
        let tools = comma_list(line_after(text, TOOLS_PREFIX).unwrap_or_default());
        let skills = comma_list(line_after(text, SKILLS_PREFIX).unwrap_or_default());
        let success = line_after(text, RESULT_PREFIX).and_then(|v| match v.trim() {
            "success" => Some(true),
            "failure" => Some(false),
            _ => None,
        });
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
            skills,
            success,
        })
    }
}

/// Split a rendered `a, b, c` list, treating the empty/`(none)` placeholder as
/// no entries.
fn comma_list(line: String) -> Vec<String> {
    if line.trim().is_empty() || line.trim() == "(none)" {
        return Vec::new();
    }
    line.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
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

    /// Text written by the pre-`skills`/`success` version of this crate — the
    /// exact three-line shape sitting in every existing memory backend. It must
    /// keep parsing, and the two new fields must come back empty rather than
    /// poisoning the outcome.
    #[test]
    fn old_format_text_still_round_trips() {
        const OLD: &str = "Situation: user asked to deploy the site\n\
                           Tools used: read_file, shell, shell\n\
                           Outcome: ran deploy.sh, site is live";
        let back = Episode::parse(OLD).expect("legacy render must still parse");
        assert_eq!(back.situation, "user asked to deploy the site");
        assert_eq!(back.tools, vec!["read_file", "shell", "shell"]);
        assert_eq!(back.outcome, "ran deploy.sh, site is live");
        assert!(back.skills.is_empty(), "absent field → empty, not garbage");
        assert_eq!(back.success, None, "absent field → unknown, not false");

        // And re-rendering a legacy episode reproduces the legacy text byte for
        // byte: no phantom `Skills used:`/`Result:` lines for old data.
        assert_eq!(back.render(), OLD);
    }

    /// Old *serialized* episodes (JSON without the new keys) must deserialize.
    #[test]
    fn old_format_json_still_deserializes() {
        let json = r#"{"situation":"s","tools":["a"],"outcome":"o","tags":[]}"#;
        let ep: Episode = serde_json::from_str(json).unwrap();
        assert!(ep.skills.is_empty());
        assert_eq!(ep.success, None);
    }

    #[test]
    fn skills_and_success_round_trip() {
        let ep = Episode::new("ship the release", "tagged and pushed")
            .with_tools(["shell", "read_file"])
            .with_skills(["release-runbook", "changelog-writing"])
            .with_success(false);
        let text = ep.render();
        // Outcome must stay terminal, otherwise `parse` swallows later fields.
        assert!(text.trim_end().ends_with("tagged and pushed"));
        let back = Episode::parse(&text).unwrap();
        assert_eq!(back.skills, vec!["release-runbook", "changelog-writing"]);
        assert_eq!(back.success, Some(false));
        assert_eq!(back.outcome, "tagged and pushed");
        assert!(!back.succeeded());
    }

    /// The new render must remain readable by the *old* parser, which located
    /// fields by prefix scan and took everything after `Outcome: `. Re-implemented
    /// here verbatim so the guarantee is tested, not merely asserted in a comment.
    #[test]
    fn new_format_is_readable_by_the_old_parser() {
        let ep = Episode::new("ship the release", "tagged and pushed")
            .with_tools(["shell"])
            .with_skills(["release-runbook"])
            .with_success(true);
        let text = ep.render();

        let legacy_situation = line_after(&text, "Situation: ").unwrap();
        let legacy_tools = line_after(&text, "Tools used: ").unwrap();
        let legacy_outcome = text
            .find("Outcome: ")
            .map(|i| text[i + "Outcome: ".len()..].trim().to_string())
            .unwrap();
        assert_eq!(legacy_situation, "ship the release");
        assert_eq!(legacy_tools, "shell");
        assert_eq!(legacy_outcome, "tagged and pushed");
    }
}
