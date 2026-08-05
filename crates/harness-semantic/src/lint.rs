//! The metadata contract a grounding agent relies on.

use crate::model::{Model, NON_ADDITIVE};
use std::fmt;

/// One finding. `error` should fail a CI gate; `warn` is advisory — the model
/// still compiles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub severity: &'static str,
    pub target: String,
    pub message: &'static str,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:<5} {}: {}", self.severity, self.target, self.message)
    }
}

/// Checks that every metric says what it means and offers something to route
/// to, and that any roll-up an agent could get wrong is classified.
///
/// Metadata is the agent's only map. **An undescribed metric is an invitation
/// to guess**, and a guess about what a number includes is indistinguishable
/// from an answer.
///
/// Issues come back in model order; callers gate on the error count.
pub fn lint(m: &Model) -> Vec<Issue> {
    let mut out = Vec::new();
    for mt in &m.metrics {
        if mt.description.is_empty() {
            out.push(Issue {
                severity: "error",
                target: mt.name.clone(),
                message: "missing description (the agent's only map of what this includes/excludes)",
            });
        }
        if mt.synonyms.is_empty() {
            out.push(Issue {
                severity: "warn",
                target: mt.name.clone(),
                message: "no synonyms — natural-language asks may not route here",
            });
        }
        // A metric the layer would infer as non-summable, but which nobody
        // declared, is a roll-up trap waiting to happen. Ask for it explicitly.
        if mt.additivity.is_empty() && m.additivity(&mt.name) == NON_ADDITIVE && !mt.is_window() {
            out.push(Issue {
                severity: "warn",
                target: mt.name.clone(),
                message: "inferred non_additive (ratio/distinct) but not declared — set additivity: non_additive",
            });
        }
    }
    out
}

/// Only the error-severity issues.
pub fn lint_errors(m: &Model) -> Vec<Issue> {
    lint(m).into_iter().filter(|i| i.severity == "error").collect()
}
