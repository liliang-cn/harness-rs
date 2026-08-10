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

    // A model with time dimensions and no declared zone buckets in whatever zone
    // the session happens to be in. That is a legal deployment — a warehouse
    // storing local time in a single-zone business needs nothing else — so this
    // is a warning, not an error. It is here because the failure is otherwise
    // invisible: the numbers are plausible, the boundaries are eight hours off,
    // and nothing in the answer says which zone it used.
    if m.timezone.is_empty() && m.dimensions.iter().any(|d| d.kind == "time") {
        out.push(Issue {
            severity: "warn",
            target: "(model)".into(),
            message: "time dimensions but no timezone: — buckets follow the database session's zone, so period boundaries move with it",
        });
    }

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
    lint(m)
        .into_iter()
        .filter(|i| i.severity == "error")
        .collect()
}
