//! Enumerating the scopes a published figure might have been computed over.
//!
//! The search runs by grouped query rather than by period: one query per
//! dimension returns every value of it at once, so the cost is proportional to
//! the number of dimensions, not to the number of quarters in the warehouse.

use harness_semantic::compile::{Filter, Value};
use harness_semantic::Model;
use serde::Serialize;

/// One predicate of a scope, in the form a delivery report can print.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Predicate {
    pub dimension: String,
    pub op: String,
    pub values: Vec<String>,
}

impl From<&Filter> for Predicate {
    fn from(f: &Filter) -> Self {
        Predicate {
            dimension: f.dimension.clone(),
            op: f.op.clone(),
            values: f.values.iter().map(show).collect(),
        }
    }
}

fn show(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
    }
}

/// A caption for a scope. The caption *is* the finding — "your 2,610.3万 is
/// revenue, unfiltered, 2026" is what goes in the report, so it has to read as
/// a sentence rather than as a filter dump.
pub fn describe(where_: &[Filter]) -> String {
    if where_.is_empty() {
        return "the whole warehouse, unfiltered".into();
    }
    where_
        .iter()
        .map(|f| {
            let vals: Vec<String> = f.values.iter().map(show).collect();
            format!("{} {} {}", f.dimension, f.op, vals.join(", "))
        })
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Half-open bounds for a period, so the recon file states what `= Q2` only
/// implies.
///
/// Half-open matters: `>= 2026-04-01 AND < 2026-07-01` includes every timestamp
/// on the last day, and `<= 2026-06-30` does not. A quarter that silently drops
/// its final day is a reconciliation that fails by a rounding error nobody can
/// find.
pub fn window(start: &str, grain: &str) -> Option<(String, String)> {
    let (y, m, d) = parse_date(start)?;
    let (ey, em) = match grain {
        "year" => (y + 1, m),
        "quarter" => add_months(y, m, 3),
        "month" => add_months(y, m, 1),
        "week" => return Some((fmt(y, m, d), add_days(y, m, d, 7)?)),
        _ => return Some((fmt(y, m, d), add_days(y, m, d, 1)?)),
    };
    // Clamp the day into the target month. A period boundary is always the 1st,
    // so this only fires on input that wasn't one — and producing "2026-02-31"
    // there would be a date string that parses nowhere and fails much later,
    // in a WHERE clause, as an engine error nobody can trace back to here.
    let ed = d.min(days_in(ey, em));
    Some((fmt(y, m, d), fmt(ey, em, ed)))
}

/// The time dimensions and categorical dimensions a metric can be scoped by —
/// only those that can slice it without a fan-out.
pub fn scopable_dimensions(m: &Model, metric: &str) -> Vec<String> {
    m.dimensions_for(metric).unwrap_or_default()
}

fn parse_date(s: &str) -> Option<(i32, u32, u32)> {
    let s = s.get(..10).unwrap_or(s);
    let mut it = s.split('-');
    let y = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

fn add_months(y: i32, m: u32, n: u32) -> (i32, u32) {
    let total = (m - 1) + n;
    (y + (total / 12) as i32, total % 12 + 1)
}

fn days_in(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        _ => 28,
    }
}

fn add_days(y: i32, m: u32, d: u32, n: u32) -> Option<String> {
    let (mut y, mut m, mut d) = (y, m, d + n);
    while d > days_in(y, m) {
        d -= days_in(y, m);
        m += 1;
        if m > 12 {
            m = 1;
            y += 1;
        }
    }
    Some(fmt(y, m, d))
}

fn fmt(y: i32, m: u32, d: u32) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Half-open bounds, so the recon file says what "= Q2" only implies.
    #[test]
    fn window_bounds_are_half_open() {
        for (input, grain, from, to) in [
            ("2026-04-01", "quarter", "2026-04-01", "2026-07-01"),
            ("2026-04-01", "month", "2026-04-01", "2026-05-01"),
            ("2026-01-01", "year", "2026-01-01", "2027-01-01"),
            ("2026-12-01", "month", "2026-12-01", "2027-01-01"),
            ("2026-12-29", "week", "2026-12-29", "2027-01-05"),
            // Not a period start. Never produced by the search, but it must not
            // emit a date that parses nowhere.
            ("2026-01-31", "month", "2026-01-31", "2026-02-28"),
        ] {
            let (f, t) = window(input, grain).expect("window");
            assert_eq!((f.as_str(), t.as_str()), (from, to), "window({input}, {grain})");
        }
    }

    /// A leap day must not roll a week into the wrong month.
    #[test]
    fn week_arithmetic_crosses_a_leap_february() {
        let (f, t) = window("2028-02-26", "week").unwrap();
        assert_eq!((f.as_str(), t.as_str()), ("2028-02-26", "2028-03-04"));
    }

    #[test]
    fn a_scope_reads_as_a_sentence() {
        assert_eq!(describe(&[]), "the whole warehouse, unfiltered");
        let w = vec![Filter {
            dimension: "store_region".into(),
            op: "=".into(),
            values: vec![Value::Str("east".into())],
        }];
        assert_eq!(describe(&w), "store_region = east");
    }
}
