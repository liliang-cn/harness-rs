//! Reconciliation: pinning each metric to something outside itself.
//!
//! This is the check that catches the failure the whole semantic layer exists
//! to prevent — **the compiler produced a number, cleanly, and it is the wrong
//! number.** A control written by a person who knows the business is the only
//! thing that can say so, because the compiler cannot check itself.
//!
//! The distinction this crate refuses to blur: a control query written by
//! whoever wrote the metric, from the same misunderstanding, agrees with it.
//! Agreement is not correctness. Both kinds of check are worth having.
//! Reporting them as the same thing is not.

use harness_semantic::compile::Filter;
use harness_semantic::Model;
use serde::{Deserialize, Serialize};

/// Where an expected number came from. **The difference between verification
/// and theatre.**
pub const SOURCE_CUSTOMER_REPORT: &str = "customer-report";
pub const SOURCE_CUSTOMER_SYSTEM: &str = "customer-system";
pub const SOURCE_ENGINEER: &str = "engineer";

/// One metric pinned to a control.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Case {
    // 缺 metric 交给 validate 报,不让 serde 先拒:一个文件里一半错误说
    // 「case 3」、另一半说「line 17 column 5」,读的人要在两套坐标之间换算。
    #[serde(default)]
    pub metric: String,
    /// SQL returning one scalar.
    #[serde(default)]
    pub control: String,
    /// Why this definition, in the customer's words.
    #[serde(default)]
    pub note: String,

    /// A figure the customer already publishes. **Replaces** the control query
    /// rather than accompanying one.
    ///
    /// A control query written to reproduce a customer's number is not evidence
    /// of anything — and if it were generated from the model it would be the
    /// compiler checking itself, which passes by construction and proves
    /// nothing. When the customer has published the number, the number *is* the
    /// control.
    #[serde(default)]
    pub value: Option<f64>,

    /// Restricts the metric to the scope the published figure covers.
    ///
    /// "94.2%" is never the whole warehouse: it is one quarter, or one plant,
    /// or both. A case comparing it against an all-time total fails for a reason
    /// that has nothing to do with the model being wrong, and that failure costs
    /// an afternoon every time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub where_: Vec<Filter>,

    /// `customer-report` | `customer-system` | `engineer`
    #[serde(default)]
    pub source: String,
    /// Tolerance. For a published value it is absolute.
    #[serde(default)]
    pub tol: f64,
}

impl Case {
    /// Whether this case is anchored to something outside the model.
    pub fn anchored(&self) -> bool {
        !self.source.is_empty() && self.source != SOURCE_ENGINEER
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Set {
    #[serde(default)]
    pub cases: Vec<Case>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SetError {
    #[error("case {index}: metric is required")]
    NoMetric { index: usize },
    #[error("case {index} ({metric}): needs either a control query or a published value")]
    NoControl { index: usize, metric: String },
    #[error(
        "case {index} ({metric}): has both a control query and a published value — which one is the evidence?"
    )]
    BothControls { index: usize, metric: String },
    #[error("{0}")]
    Parse(String),
}

impl Set {
    pub fn from_yaml(src: &str) -> Result<Self, SetError> {
        let set: Set = serde_yaml::from_str(src).map_err(|e| SetError::Parse(e.to_string()))?;
        set.validate()?;
        Ok(set)
    }

    /// Rejects a case that cannot be evidence of anything.
    ///
    /// Both a control query *and* a published value is refused rather than
    /// silently preferring one: whichever this code picked, the other would sit
    /// in the file looking like it had been checked.
    pub fn validate(&self) -> Result<(), SetError> {
        for (i, c) in self.cases.iter().enumerate() {
            let index = i + 1;
            if c.metric.is_empty() {
                return Err(SetError::NoMetric { index });
            }
            match (c.control.is_empty(), c.value.is_none()) {
                (true, true) => {
                    return Err(SetError::NoControl { index, metric: c.metric.clone() });
                }
                (false, false) => {
                    return Err(SetError::BothControls { index, metric: c.metric.clone() });
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// One metric compared against its control.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    pub metric: String,
    pub ok: bool,
    pub got: f64,
    pub want: f64,
    pub tol: f64,
    pub anchored: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub scope: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// Compares one pair of numbers.
///
/// A zero tolerance means exact, and floating point makes exact a trap, so a
/// tiny epsilon rides along: two runs of the same SUM over the same rows can
/// differ in the last bit, and a control that fails intermittently gets
/// switched off — which is worse than not having it.
pub fn agrees(got: f64, want: f64, tol: f64) -> bool {
    let t = if tol > 0.0 { tol } else { 1e-9 * want.abs().max(1.0) };
    (got - want).abs() <= t
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    pub results: Vec<Outcome>,
    pub total: usize,
    pub passed: usize,
    /// Metrics declared in the model.
    pub declared: usize,
    /// Metrics with a control.
    pub covered: usize,
    /// Covered by a customer figure rather than by a derivation.
    pub anchored: usize,
}

impl Report {
    pub fn of(results: Vec<Outcome>, model: &Model) -> Self {
        let total = results.len();
        let passed = results.iter().filter(|r| r.ok).count();
        let anchored = results.iter().filter(|r| r.anchored).count();
        Report {
            declared: model.metrics.len(),
            covered: total,
            total,
            passed,
            anchored,
            results,
        }
    }

    /// Model metrics with no control query.
    ///
    /// Coverage is reported rather than assumed: eleven passing checks over
    /// forty metrics is a different statement than eleven over eleven, and only
    /// one of them is worth showing a customer.
    pub fn uncovered(&self, m: &Model) -> Vec<String> {
        let have: Vec<&str> = self.results.iter().map(|r| r.metric.as_str()).collect();
        m.metrics
            .iter()
            .filter(|mt| !have.contains(&mt.name.as_str()))
            .map(|mt| mt.name.clone())
            .collect()
    }

    /// The one line someone reads first.
    ///
    /// "not verified" when nothing was reconciled, rather than a vacuous 0/0
    /// pass: **a model nobody checked and a model that checked out must not
    /// print the same word.**
    pub fn verdict(&self, m: &Model) -> String {
        if self.total == 0 {
            return "NOT VERIFIED — no metric has a control query".into();
        }
        if self.passed < self.total {
            return format!(
                "FAILING — {} of {} metrics disagree with their control query",
                self.total - self.passed,
                self.total
            );
        }

        // Coverage and anchoring are separate gaps and neither may hide the
        // other. "Half the metrics are unchecked" and "nothing was checked
        // against anything outside the model" are both true of the same report,
        // and a reader told only one of them has been misled by omission.
        let uncovered = self.uncovered(m);
        let mut gaps: Vec<String> = Vec::new();
        if !uncovered.is_empty() {
            gaps.push(format!("{} have no control query", uncovered.len()));
        }
        if self.anchored == 0 {
            gaps.push("none is anchored to a customer figure".into());
        } else if self.anchored < self.total {
            gaps.push(format!(
                "{} of {} anchored to customer figures",
                self.anchored, self.total
            ));
        }

        let label = if !uncovered.is_empty() {
            "PARTIAL"
        } else if self.anchored == 0 {
            // Everything agrees, and only with itself.
            "SELF-CONSISTENT"
        } else {
            "VERIFIED"
        };
        let head = format!(
            "{label} — {}/{} metrics reconcile",
            self.passed,
            m.metrics.len()
        );
        if gaps.is_empty() {
            head
        } else {
            format!("{head}, {}", gaps.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(n: usize) -> Model {
        let mut y = String::from(
            "entities:\n  - {name: order, table: orders, primary_key: id}\nmetrics:\n",
        );
        for i in 0..n {
            y.push_str(&format!("  - {{name: m{i}, entity: order, agg: sum, expr: a}}\n"));
        }
        Model::from_yaml(&y).unwrap()
    }

    fn outcome(metric: &str, ok: bool, anchored: bool) -> Outcome {
        Outcome { metric: metric.into(), ok, anchored, ..Default::default() }
    }

    /// A case that cannot be evidence of anything is refused at load.
    #[test]
    fn a_case_needs_exactly_one_control() {
        assert!(Set::from_yaml("cases:\n  - {metric: revenue, control: \"SELECT 1\"}").is_ok());
        assert!(Set::from_yaml("cases:\n  - {metric: revenue, value: 1.0}").is_ok());

        let err = Set::from_yaml("cases:\n  - {metric: revenue}").unwrap_err();
        assert!(err.to_string().contains("needs either"), "{err}");

        // Refused rather than silently preferring one: whichever this code
        // picked, the other would sit in the file looking checked.
        let err =
            Set::from_yaml("cases:\n  - {metric: revenue, control: \"SELECT 1\", value: 1.0}")
                .unwrap_err();
        assert!(err.to_string().contains("which one is the evidence"), "{err}");

        let err = Set::from_yaml("cases:\n  - {control: \"SELECT 1\"}").unwrap_err();
        assert!(err.to_string().contains("metric is required"), "{err}");
    }

    #[test]
    fn only_an_external_source_counts_as_anchored() {
        let anchored = Case { source: SOURCE_CUSTOMER_REPORT.into(), ..Default::default() };
        assert!(anchored.anchored());
        let system = Case { source: SOURCE_CUSTOMER_SYSTEM.into(), ..Default::default() };
        assert!(system.anchored());

        // Derived from the same schema by the same person: a consistency check,
        // not evidence.
        let engineer = Case { source: SOURCE_ENGINEER.into(), ..Default::default() };
        assert!(!engineer.anchored());
        assert!(!Case::default().anchored(), "unstated source is not an anchor");
    }

    /// Nothing checked must not print the same word as everything checked.
    #[test]
    fn nothing_reconciled_is_not_verified() {
        let m = model(3);
        let r = Report::of(vec![], &m);
        assert_eq!(r.verdict(&m), "NOT VERIFIED — no metric has a control query");
    }

    #[test]
    fn agreement_with_only_itself_is_self_consistent_not_verified() {
        let m = model(2);
        let r = Report::of(
            vec![outcome("m0", true, false), outcome("m1", true, false)],
            &m,
        );
        let v = r.verdict(&m);
        assert!(v.starts_with("SELF-CONSISTENT"), "{v}");
        assert!(v.contains("none is anchored to a customer figure"), "{v}");
    }

    #[test]
    fn an_anchored_full_pass_is_verified() {
        let m = model(2);
        let r = Report::of(vec![outcome("m0", true, true), outcome("m1", true, true)], &m);
        assert_eq!(r.verdict(&m), "VERIFIED — 2/2 metrics reconcile");
    }

    /// Coverage and anchoring are separate gaps; neither may hide the other.
    #[test]
    fn partial_coverage_is_reported_even_when_everything_checked_passes() {
        let m = model(5);
        let r = Report::of(vec![outcome("m0", true, true)], &m);
        let v = r.verdict(&m);
        assert!(v.starts_with("PARTIAL"), "{v}");
        assert!(v.contains("4 have no control query"), "{v}");
    }

    #[test]
    fn a_disagreement_outranks_every_other_finding() {
        let m = model(2);
        let r = Report::of(vec![outcome("m0", true, true), outcome("m1", false, true)], &m);
        assert_eq!(
            r.verdict(&m),
            "FAILING — 1 of 2 metrics disagree with their control query"
        );
    }

    /// A control that fails intermittently gets switched off, which is worse
    /// than not having it.
    #[test]
    fn exact_comparison_tolerates_a_last_bit_difference() {
        assert!(agrees(0.1 + 0.2, 0.3, 0.0));
        assert!(agrees(1_000_000.0, 1_000_000.0000001, 0.0));
        assert!(!agrees(100.0, 101.0, 0.0));
        assert!(agrees(97.3, 97.34, 0.05));
        assert!(!agrees(97.3, 97.4, 0.05));
    }
}
