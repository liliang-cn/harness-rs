//! The production-loop catalogue.
//!
//! Loop engineering names seven recurring loops that teams actually run.
//! Each is just a [`LoopSpec`] constructor — sensible defaults for level,
//! cadence, budget, and the maker/checker prompts — that you then bind to a
//! model, tools, sandbox, and gate via [`crate::LoopEngine`], or hand to the
//! [`crate::LoopScheduler`] to run on its cadence.
//!
//! | Pattern             | Default cadence | Level | Action     |
//! |---------------------|-----------------|-------|------------|
//! | `daily_triage`      | daily 09:00     | L1    | report     |
//! | `pr_babysitter`     | every 10m       | L1    | comment    |
//! | `ci_sweeper`        | every 10m       | L2    | apply-patch|
//! | `dependency_sweeper`| daily 04:00     | L2    | open-pr    |
//! | `changelog_drafter` | daily 18:00     | L1    | draft      |
//! | `post_merge_cleanup`| every 6h        | L1    | report     |
//! | `issue_triage`      | every 2h        | L1    | comment    |
//!
//! Defaults are deliberately conservative — start a loop where the table
//! says and graduate its level as you build trust.

use crate::budget::TokenBudget;
use crate::level::LoopLevel;
use crate::spec::LoopSpec;

/// **Daily Triage** — scan the project on a daily cadence and surface what
/// needs a human's attention. Report-only; the cheapest loop to start with.
pub fn daily_triage() -> LoopSpec {
    LoopSpec::new(
        "daily-triage",
        "Surface anything in the project that needs human attention today.",
        LoopLevel::L1Report,
    )
    .with_cadence("daily 09:00")
    .with_budget(TokenBudget::iters(10))
    .with_action_kind("report")
    .with_maker_prompt(
        "Review recent activity (open issues, failing checks, stale branches, \
         TODOs) and list the few items that most need attention today, with a \
         one-line reason each. Be terse.",
    )
    .with_checker_prompt(
        "Confirm each surfaced item is real and not already resolved. Drop \
         anything stale.",
    )
}

/// **PR Babysitter** — watch open pull requests and report when one needs a
/// nudge (failing CI, requested changes, merge conflicts, gone quiet).
/// High cadence, so keep the budget tight.
pub fn pr_babysitter() -> LoopSpec {
    LoopSpec::new(
        "pr-babysitter",
        "Keep open PRs moving; flag any that are stuck.",
        LoopLevel::L1Report,
    )
    .with_cadence("every 10m")
    .with_budget(TokenBudget::iters(8))
    .with_action_kind("comment")
    .with_maker_prompt(
        "For each open PR, determine if it is blocked (red CI, conflicts, \
         unanswered review, idle > 24h). List only the blocked ones and the \
         single next action each needs.",
    )
    .with_checker_prompt("Verify each flagged PR is genuinely blocked right now.")
}

/// **CI Sweeper** — when CI is red, investigate and propose a fix.
/// Assisted: the maker may patch inside a sandbox, but a human gates the
/// change. Cautious and potentially expensive — budget accordingly.
pub fn ci_sweeper() -> LoopSpec {
    LoopSpec::new(
        "ci-sweeper",
        "Keep the default branch green by proposing fixes for CI failures.",
        LoopLevel::L2Assisted,
    )
    .with_cadence("every 10m")
    .with_budget(TokenBudget::iters(20).with_max_total_tokens(400_000))
    .with_action_kind("apply-patch")
    .with_maker_prompt(
        "If CI is failing, reproduce the failure, find the root cause, and \
         make the smallest change that fixes it. If you cannot fix it \
         confidently, explain what you found and stop.",
    )
    .with_checker_prompt(
        "Run the build and the failing tests. Confirm they now pass and that \
         nothing else regressed. Report DoneWithConcerns if the fix looks \
         risky or broad.",
    )
}

/// **Dependency Sweeper** — find safe dependency updates and open a PR for
/// them. Assisted, patch-only, low cadence.
pub fn dependency_sweeper() -> LoopSpec {
    LoopSpec::new(
        "dependency-sweeper",
        "Keep dependencies current via small, verified update PRs.",
        LoopLevel::L2Assisted,
    )
    .with_cadence("daily 04:00")
    .with_budget(TokenBudget::iters(16).with_max_total_tokens(300_000))
    .with_action_kind("open-pr")
    .with_maker_prompt(
        "Identify outdated dependencies with low-risk updates (patch/minor). \
         Update them and adjust any code the update requires. One coherent \
         batch only.",
    )
    .with_checker_prompt(
        "Build and test against the updated dependencies. Confirm green. Flag \
         any major-version or behaviour-changing update for human review.",
    )
}

/// **Changelog Drafter** — draft release notes from recent merges.
/// Report-only; runs in the evening or on tag.
pub fn changelog_drafter() -> LoopSpec {
    LoopSpec::new(
        "changelog-drafter",
        "Draft accurate, readable changelog entries from recent changes.",
        LoopLevel::L1Report,
    )
    .with_cadence("daily 18:00")
    .with_budget(TokenBudget::iters(10))
    .with_action_kind("draft")
    .with_maker_prompt(
        "Summarize changes merged since the last release into changelog \
         entries grouped by Added / Changed / Fixed. User-facing language.",
    )
    .with_checker_prompt(
        "Confirm each entry maps to a real change and nothing significant is \
         missing.",
    )
}

/// **Post-Merge Cleanup** — after merges, look for follow-ups: dead code,
/// stale branches, leftover TODOs. Report-only, off-peak.
pub fn post_merge_cleanup() -> LoopSpec {
    LoopSpec::new(
        "post-merge-cleanup",
        "Catch loose ends left behind by recent merges.",
        LoopLevel::L1Report,
    )
    .with_cadence("every 6h")
    .with_budget(TokenBudget::iters(10))
    .with_action_kind("report")
    .with_maker_prompt(
        "Look for cleanup opportunities from recently merged work: merged \
         branches not deleted, newly dead code, TODOs introduced, docs that \
         drifted. List concrete items.",
    )
    .with_checker_prompt("Confirm each cleanup item is still applicable.")
}

/// **Issue Triage** — label and route new issues, propose-only.
pub fn issue_triage() -> LoopSpec {
    LoopSpec::new(
        "issue-triage",
        "Label, prioritize, and route incoming issues consistently.",
        LoopLevel::L1Report,
    )
    .with_cadence("every 2h")
    .with_budget(TokenBudget::iters(8))
    .with_action_kind("comment")
    .with_maker_prompt(
        "For each new, untriaged issue, propose labels, a priority, and an \
         owner/area, with a one-line justification. Propose only.",
    )
    .with_checker_prompt(
        "Sanity-check the proposed triage against the issue text; flag any \
         that need a human.",
    )
}

/// Every built-in pattern, in catalogue order. Handy for listing or for
/// registering a whole suite with the scheduler.
pub fn catalogue() -> Vec<LoopSpec> {
    vec![
        daily_triage(),
        pr_babysitter(),
        ci_sweeper(),
        dependency_sweeper(),
        changelog_drafter(),
        post_merge_cleanup(),
        issue_triage(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_has_seven_uniquely_named_loops() {
        let cat = catalogue();
        assert_eq!(cat.len(), 7);
        let mut names: Vec<_> = cat.iter().map(|s| s.name.clone()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 7, "loop names must be unique");
    }

    #[test]
    fn sweepers_are_assisted_everything_else_reports() {
        assert_eq!(ci_sweeper().level, LoopLevel::L2Assisted);
        assert_eq!(dependency_sweeper().level, LoopLevel::L2Assisted);
        assert_eq!(daily_triage().level, LoopLevel::L1Report);
        assert_eq!(issue_triage().level, LoopLevel::L1Report);
    }

    #[test]
    fn every_pattern_cadence_parses() {
        for spec in catalogue() {
            assert!(
                harness_daemon::Schedule::parse(&spec.cadence).is_ok(),
                "cadence `{}` for `{}` must parse",
                spec.cadence,
                spec.name
            );
        }
    }
}
