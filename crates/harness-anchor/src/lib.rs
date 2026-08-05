//! Anchor a model against a figure the customer already publishes.
//!
//! # Why this exists
//!
//! Every control query written to check a semantic model is written by whoever
//! wrote the metric, from the same understanding. So they agree — and
//! **agreement is not correctness**. A delivery report backed only by those can
//! honestly say SELF-CONSISTENT and nothing more.
//!
//! A number the customer already published is the one piece of evidence that
//! did not come from us. If some scope of the metric reproduces it, the model
//! and the business agree about what the number *means*, and the report can say
//! VERIFIED.
//!
//! The search runs backwards from the usual direction, because the customer
//! rarely knows how their own figure was scoped: enumerate the scopes the model
//! can express, and report which ones land on the figure. The answer is a
//! caption — *"your 97.3% is this metric, restricted to Q2 2026"* — and the
//! caption is the finding.

pub mod scope;

use serde::Serialize;

/// One scope that lands on the figure.
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    /// A human caption for the scope: what to write in the delivery report.
    pub label: String,
    pub value: f64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub where_: Vec<scope::Predicate>,
}

/// What the search found.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Result {
    pub metric: String,
    pub target: f64,
    pub matches: Vec<Candidate>,
    /// Reported when nothing matched: the nearest scope found, so the engineer
    /// can see whether they are one filter away or in the wrong metric.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closest: Option<Candidate>,
    pub scopes_searched: usize,
    pub queries: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,

    pub tol: f64,
    /// The observed spread of the metric across every scope tried. It is what
    /// separates "ambiguous" from "too coarse", and those need different advice.
    pub lo: f64,
    pub hi: f64,
    /// Set when the figure only matches after a unit conversion — the customer
    /// publishes 97.3 and the metric is the fraction 0.973, or the figure is in
    /// thousands. Percent-versus-fraction is the commonest reconciliation
    /// failure and it looks exactly like a wrong model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
}

impl Result {
    /// Reports that the published figure cannot distinguish between scopes
    /// because the metric barely varies across them.
    ///
    /// **This is the failure that looks most like success.** 97.3% matched
    /// seventy-five scopes not because the model is ambiguous, but because every
    /// scope of that metric lands between 96.8% and 97.3% and one decimal place
    /// cannot tell them apart. The answer is not a tighter search; it is to ask
    /// for more digits, or to anchor on a metric that actually moves.
    pub fn too_coarse(&self) -> bool {
        self.matches.len() > 1 && self.hi - self.lo <= 2.0 * self.tol
    }

    /// The verdict, in the words the delivery report uses.
    pub fn verdict(&self) -> Verdict {
        match self.matches.len() {
            1 => Verdict::Anchored,
            0 => Verdict::NoMatch,
            _ if self.too_coarse() => Verdict::TooCoarse,
            _ => Verdict::Ambiguous,
        }
    }
}

/// What the search concluded. Four outcomes, because they call for four
/// different next moves — collapsing them into "found / not found" throws away
/// the only part a person can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Exactly one scope reproduces the figure. This is the one that earns
    /// VERIFIED.
    Anchored,
    /// Nothing matched: either the figure is not this metric, or the model's
    /// definition differs from the customer's.
    NoMatch,
    /// Several scopes matched and the metric genuinely moves — two different
    /// slices happen to land on the same number. More decimals will not help.
    Ambiguous,
    /// Several scopes matched because the metric barely varies. Ask for more
    /// digits, or anchor on something else.
    TooCoarse,
}

/// Reads the precision a written figure declares.
///
/// `"97.3"` says three significant decimals were known, so the true value lies
/// within half of the last digit. Taking that literally is the whole point: it
/// is the customer's own statement of how precisely they know their number, and
/// it is strictly better than any default this code could pick.
///
/// A fixed tolerance is wrong in both directions — too tight and the real scope
/// never appears; too loose and everything matches, which is the same as
/// nothing matching but reads like success.
pub fn tolerance_of(written: &str) -> f64 {
    let mut w = written.trim();
    if let Some(i) = w.find(['e', 'E']) {
        w = &w[..i];
    }
    match w.find('.') {
        // "94" means somewhere in [93.5, 94.5).
        None => 0.5,
        Some(dot) => {
            let decimals = w.len() - dot - 1;
            0.5 * 10f64.powi(-(decimals as i32))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A customer who writes 97.3 has told you the precision they have. Any
    /// default this code picked instead would be wrong in one direction.
    #[test]
    fn tolerance_comes_from_the_figures_own_precision() {
        for (written, want) in [
            ("97.3", 0.05),
            ("97.30", 0.005),
            ("0.973", 0.0005),
            ("94", 0.5),
            (" 1842.30", 0.005),
            ("1.5e3", 0.05),
        ] {
            let got = tolerance_of(written);
            assert!(
                (got - want).abs() < 1e-12,
                "tolerance_of({written:?}) = {got}, want {want}"
            );
        }
    }

    fn cand(v: f64) -> Candidate {
        Candidate { label: String::new(), value: v, where_: vec![] }
    }

    /// Everything matching is the failure that looks most like success: the
    /// search did not find the customer's scope, it found that the figure
    /// cannot identify one.
    #[test]
    fn too_coarse_is_distinct_from_ambiguous() {
        let coarse = Result {
            tol: 0.0005,
            lo: 0.962,
            hi: 0.9625,
            matches: vec![cand(0.962), cand(0.9625)],
            ..Default::default()
        };
        assert!(
            coarse.too_coarse(),
            "a metric that barely varies cannot be anchored by a rounded figure"
        );
        assert_eq!(coarse.verdict(), Verdict::TooCoarse);

        // Genuinely ambiguous: the metric moves, and two distinct scopes happen
        // to land on the same number. Asking for more decimals will not help.
        let ambiguous = Result {
            tol: 0.0005,
            lo: 0.10,
            hi: 0.99,
            matches: vec![cand(0.973), cand(0.973)],
            ..Default::default()
        };
        assert!(
            !ambiguous.too_coarse(),
            "a metric with real spread is ambiguous, not coarse — different advice"
        );
        assert_eq!(ambiguous.verdict(), Verdict::Ambiguous);

        // One match is an anchor, whatever the spread.
        let one = Result { tol: 1.0, lo: 0.0, hi: 1.0, matches: vec![cand(1.0)], ..Default::default() };
        assert!(!one.too_coarse(), "a single match is not ambiguous");
        assert_eq!(one.verdict(), Verdict::Anchored);

        assert_eq!(Result::default().verdict(), Verdict::NoMatch);
    }
}
