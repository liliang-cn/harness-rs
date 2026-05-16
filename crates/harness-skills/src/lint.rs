//! Linter that flags configuration smells in a directory of skills.
//!
//! Beyond what `validate` requires (which is the spec minimum), the linter
//! complains about descriptions that won't help an agent decide *when* to
//! activate a skill, duplicate keyword overlap that confuses routing, and
//! missing trigger-style guidance.

use crate::FileSkill;
use harness_core::Skill;
use std::collections::HashMap;
use std::path::Path;

/// Lint severity. `Error` will return non-zero from the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSeverity { Error, Warning, Info }

#[derive(Debug, Clone)]
pub struct LintFinding {
    pub skill_name: String,
    pub severity:   LintSeverity,
    pub message:    String,
}

pub fn lint_dir(root: &Path) -> Result<Vec<LintFinding>, harness_core::SkillError> {
    let skills = crate::loader::scan_skills_root(root)?;
    Ok(lint_skills(&skills))
}

pub fn lint_skills(skills: &[FileSkill]) -> Vec<LintFinding> {
    let mut findings = Vec::new();

    // Per-skill checks
    for s in skills {
        let m = s.manifest();
        let body = s.body();

        // R1: description too short → agent has nothing to route on
        if m.description.len() < 30 {
            findings.push(LintFinding {
                skill_name: m.name.clone(),
                severity:   LintSeverity::Warning,
                message:    format!(
                    "description is only {} chars; aim for ≥30 with a clear 'use when…' clause",
                    m.description.len()
                ),
            });
        }

        // R2: missing trigger-style language. Accept a broad set of phrasings —
        // the spec ([agentskills.io](https://agentskills.io/specification)) just
        // says "describe both what the skill does AND when to use it", so any
        // clear routing cue counts. The list below was widened after lint
        // wrongly flagged Anthropic's own `pptx` skill (uses "Use this skill
        // any time…" + "Trigger whenever the user mentions…").
        let lower = m.description.to_ascii_lowercase();
        let routing_cues = [
            "use when",
            "use for",
            "use this skill",
            "use any time",
            "any time a ",
            "any time the ",
            "whenever the user",
            "when the user",
            "trigger when",
            "trigger whenever",
            "activate when",
            "activate whenever",
            "invoke for",
            "invoke when",
            "call when",
            "call this skill",
            "applies when",
            "use after",
            "use before",
        ];
        if !routing_cues.iter().any(|p| lower.contains(p)) {
            findings.push(LintFinding {
                skill_name: m.name.clone(),
                severity:   LintSeverity::Info,
                message:    "description lacks 'use when…' trigger language; agents may struggle to route".into(),
            });
        }

        // R3: body too short — skills with substantial bodies are more useful
        if body.trim().len() < 50 {
            findings.push(LintFinding {
                skill_name: m.name.clone(),
                severity:   LintSeverity::Info,
                message:    "SKILL.md body is very short; consider adding step-by-step guidance".into(),
            });
        }

        // R4: description ≈ name (low signal)
        if m.description.to_lowercase().contains(&m.name.replace('-', " "))
            && m.description.len() < 60
        {
            findings.push(LintFinding {
                skill_name: m.name.clone(),
                severity:   LintSeverity::Info,
                message:    "description mostly restates the name; describe behaviour, not identity".into(),
            });
        }
    }

    // Cross-skill checks

    // X1: duplicate names already caught at registration; here we flag near-duplicates
    let mut seen_names: HashMap<String, Vec<String>> = HashMap::new();
    for s in skills {
        let key = s.manifest().name.replace('-', "").to_ascii_lowercase();
        seen_names.entry(key).or_default().push(s.manifest().name.clone());
    }
    for (_, group) in seen_names.iter().filter(|(_, g)| g.len() > 1) {
        findings.push(LintFinding {
            skill_name: group.join(", "),
            severity:   LintSeverity::Warning,
            message:    "near-duplicate skill names — agent will struggle to disambiguate".into(),
        });
    }

    // X2: significant keyword overlap between two descriptions
    let mut tokens_per_skill: HashMap<String, Vec<String>> = HashMap::new();
    for s in skills {
        let tokens: Vec<String> = s.manifest().description
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() >= 5)
            .map(|w| w.to_ascii_lowercase())
            .collect();
        tokens_per_skill.insert(s.manifest().name.clone(), tokens);
    }
    let names: Vec<&String> = tokens_per_skill.keys().collect();
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            let a = &tokens_per_skill[names[i]];
            let b = &tokens_per_skill[names[j]];
            if a.is_empty() || b.is_empty() { continue; }
            let set_a: std::collections::HashSet<&String> = a.iter().collect();
            let set_b: std::collections::HashSet<&String> = b.iter().collect();
            let inter: usize = set_a.intersection(&set_b).count();
            let union: usize = set_a.union(&set_b).count();
            if union == 0 { continue; }
            let jaccard = inter as f32 / union as f32;
            if jaccard > 0.5 && inter >= 4 {
                findings.push(LintFinding {
                    skill_name: format!("{} & {}", names[i], names[j]),
                    severity:   LintSeverity::Warning,
                    message:    format!(
                        "{}/{} keyword overlap ({:.0}% Jaccard) — model may confuse activation",
                        inter, union, jaccard * 100.0
                    ),
                });
            }
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileSkill;
    use harness_core::{Resource, SkillManifest};

    fn skill(name: &str, desc: &str, body: &str) -> FileSkill {
        FileSkill::new(
            SkillManifest {
                name: name.into(),
                description: desc.into(),
                license: None,
                compatibility: None,
                metadata: Default::default(),
                allowed_tools: None,
            },
            body.into(),
            Vec::<Resource>::new(),
        )
    }

    #[test]
    fn short_description_warns() {
        let s = vec![skill("a", "too short.", "body")];
        let findings = lint_skills(&s);
        assert!(findings.iter().any(|f| f.message.contains("description is only")));
    }

    #[test]
    fn missing_trigger_language_gives_info() {
        let s = vec![skill("x", "This skill formats Rust code and runs clippy with strict warnings.", "body")];
        let findings = lint_skills(&s);
        assert!(findings.iter().any(|f| f.message.contains("trigger language")));
    }

    #[test]
    fn good_skill_passes_clean() {
        let s = vec![skill(
            "format-rust",
            "Run cargo fmt across the workspace. Use when Rust files have been edited or before committing changes.",
            "step-by-step instructions for formatting rust code go here, with examples",
        )];
        let findings = lint_skills(&s);
        // Allow info-level findings; assert no warnings or errors.
        assert!(findings.iter().all(|f| f.severity == LintSeverity::Info), "{:?}", findings);
    }

    #[test]
    fn near_duplicate_names_flagged() {
        let s = vec![
            skill(
                "format-rust",
                "Run cargo fmt across the workspace. Use when files have been edited.",
                "body",
            ),
            skill(
                "formatrust",
                "Run cargo fmt across the workspace. Use when files have been edited.",
                "body",
            ),
        ];
        let findings = lint_skills(&s);
        assert!(findings.iter().any(|f| f.message.contains("near-duplicate")));
    }

    #[test]
    fn high_keyword_overlap_flagged() {
        let s = vec![
            skill(
                "alpha",
                "Format Rust source files using cargo fmt before commit when files changed.",
                "body",
            ),
            skill(
                "beta",
                "Format Rust source files using rustfmt before commit when files changed.",
                "body",
            ),
        ];
        let findings = lint_skills(&s);
        assert!(
            findings.iter().any(|f| f.message.contains("keyword overlap")),
            "{:#?}",
            findings
        );
    }
}
