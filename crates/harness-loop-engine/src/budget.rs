//! Per-loop token & iteration budgets.
//!
//! Loop engineering's hard-won lesson: *token consumption explodes with
//! sub-agents and long-running loops*. A loop that runs every five minutes
//! forever will, without a ceiling, spend without bound. `TokenBudget` is
//! that ceiling, enforced per round; `BudgetState` accumulates spend across
//! the maker and checker sub-agents and reports when a limit is crossed.

use harness_core::Usage;
use serde::{Deserialize, Serialize};

/// A declarative spend ceiling for a single round of a loop.
///
/// `None` on a field means "no limit on this axis". `max_iters_per_round`
/// caps how many tool-using iterations each sub-agent may take and is the
/// one limit that is always present (sub-agents need a finite iteration
/// budget regardless).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenBudget {
    /// Max input (prompt) tokens summed across all sub-agents in a round.
    pub max_input_tokens: Option<u64>,
    /// Max output (completion) tokens summed across the round.
    pub max_output_tokens: Option<u64>,
    /// Max total (input + output) tokens for the round.
    pub max_total_tokens: Option<u64>,
    /// Iteration cap handed to each sub-agent's loop.
    pub max_iters_per_round: u32,
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self {
            max_input_tokens: None,
            max_output_tokens: None,
            max_total_tokens: None,
            max_iters_per_round: 12,
        }
    }
}

impl TokenBudget {
    /// A budget with only an iteration cap (no token ceilings).
    pub fn iters(max_iters_per_round: u32) -> Self {
        Self {
            max_iters_per_round,
            ..Default::default()
        }
    }

    pub fn with_max_total_tokens(mut self, n: u64) -> Self {
        self.max_total_tokens = Some(n);
        self
    }
    pub fn with_max_input_tokens(mut self, n: u64) -> Self {
        self.max_input_tokens = Some(n);
        self
    }
    pub fn with_max_output_tokens(mut self, n: u64) -> Self {
        self.max_output_tokens = Some(n);
        self
    }
    pub fn with_max_iters_per_round(mut self, n: u32) -> Self {
        self.max_iters_per_round = n;
        self
    }
}

/// Which ceiling a round crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLimit {
    Input,
    Output,
    Total,
}

impl BudgetLimit {
    pub fn label(self) -> &'static str {
        match self {
            BudgetLimit::Input => "input-tokens",
            BudgetLimit::Output => "output-tokens",
            BudgetLimit::Total => "total-tokens",
        }
    }
}

/// Running tally of spend within a round, checked against a [`TokenBudget`].
#[derive(Debug, Clone, Copy)]
pub struct BudgetState {
    budget: TokenBudget,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl BudgetState {
    pub fn new(budget: TokenBudget) -> Self {
        Self {
            budget,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    /// Fold one sub-agent's usage into the tally.
    pub fn add(&mut self, usage: &Usage) {
        self.input_tokens += usage.input_tokens as u64;
        self.output_tokens += usage.output_tokens as u64;
    }

    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    /// The iteration cap each sub-agent should run under.
    pub fn max_iters(&self) -> u32 {
        self.budget.max_iters_per_round
    }

    /// Returns the first limit that has been crossed, if any. The engine
    /// checks this after each sub-agent so it can stop before spawning the
    /// next one.
    pub fn exceeded(&self) -> Option<BudgetLimit> {
        if let Some(m) = self.budget.max_input_tokens
            && self.input_tokens > m
        {
            return Some(BudgetLimit::Input);
        }
        if let Some(m) = self.budget.max_output_tokens
            && self.output_tokens > m
        {
            return Some(BudgetLimit::Output);
        }
        if let Some(m) = self.budget.max_total_tokens
            && self.total_tokens() > m
        {
            return Some(BudgetLimit::Total);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u32, output: u32) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: 0,
        }
    }

    #[test]
    fn no_limits_never_exceeds() {
        let mut s = BudgetState::new(TokenBudget::iters(8));
        s.add(&usage(1_000_000, 1_000_000));
        assert!(s.exceeded().is_none());
        assert_eq!(s.max_iters(), 8);
    }

    #[test]
    fn total_limit_trips() {
        let mut s = BudgetState::new(TokenBudget::iters(8).with_max_total_tokens(100));
        s.add(&usage(60, 30)); // 90 — under
        assert!(s.exceeded().is_none());
        s.add(&usage(20, 0)); // 110 — over
        assert_eq!(s.exceeded(), Some(BudgetLimit::Total));
    }

    #[test]
    fn input_and_output_limits_trip_independently() {
        let mut s = BudgetState::new(
            TokenBudget::iters(8)
                .with_max_input_tokens(50)
                .with_max_output_tokens(50),
        );
        s.add(&usage(51, 1));
        assert_eq!(s.exceeded(), Some(BudgetLimit::Input));

        let mut s2 = BudgetState::new(TokenBudget::iters(8).with_max_output_tokens(50));
        s2.add(&usage(1, 51));
        assert_eq!(s2.exceeded(), Some(BudgetLimit::Output));
    }
}
