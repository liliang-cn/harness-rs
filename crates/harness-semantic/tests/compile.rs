//! Ported from semantic-go's test suite. Each case here is a failure that was
//! found the expensive way — a query that ran clean and returned a wrong
//! number — so they are the specification, not a formality.

use harness_semantic::compile::{Filter, Value};
use harness_semantic::model::{ADDITIVE, NON_ADDITIVE};
use harness_semantic::{Model, Query, compile, dialect, lint};

fn q(metrics: &[&str], group_by: &[&str]) -> Query {
    Query {
        metrics: metrics.iter().map(|s| s.to_string()).collect(),
        group_by: group_by.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// An additive base, a non-additive ratio, and window metrics with and without
/// a grain-to-date reset.
fn test_model() -> Model {
    Model::from_yaml(
        r#"
entities:
  - {name: order_item, table: order_items, primary_key: id}
  - {name: order,      table: orders,      primary_key: order_id}
joins:
  - {from: order_item, to: order, from_key: order_id, to_key: order_id, cardinality: many_to_one}
dimensions:
  - {name: order_date, entity: order, column: order_date, type: time}
metrics:
  - {name: revenue,     description: d, synonyms: [sales],     entity: order_item, agg: sum,            expr: "qty * price"}
  - {name: order_count, description: d, synonyms: [orders],    entity: order,      agg: count_distinct, expr: order_id}
  - {name: aov,         description: d, synonyms: [avg order], formula: "revenue / nullif(order_count, 0)"}
  - {name: rev_cumulative, description: d, synonyms: [running], of: revenue, window: cumulative}
  - {name: rev_ytd,        description: d, synonyms: [ytd],     of: revenue, window: cumulative, reset: year}
"#,
    )
    .expect("model")
}

/// A `reset: year` cumulative must partition by the truncated year so the
/// running total restarts; a plain cumulative must not.
#[test]
fn grain_to_date_resets_and_plain_cumulative_does_not() {
    let m = test_model();
    let mut query = q(&["rev_ytd"], &["order_date"]);
    query.time_grain = "month".into();

    let c = compile(&m, &query, &dialect::Postgres).expect("rev_ytd");
    assert!(
        c.sql.contains("PARTITION BY date_trunc('year'"),
        "rev_ytd missing year-reset partition:\n{}",
        c.sql
    );

    query.metrics = vec!["rev_cumulative".into()];
    let c = compile(&m, &query, &dialect::Postgres).expect("rev_cumulative");
    assert!(
        !c.sql.contains("PARTITION BY"),
        "plain cumulative must not reset:\n{}",
        c.sql
    );
}

/// Summing a non-additive measure over time is refused at compile time — not
/// silently turned into a wrong number.
#[test]
fn summing_a_ratio_over_time_is_refused() {
    let mut m = test_model();
    m.metrics.push(harness_semantic::Metric {
        name: "bad_aov_roll".into(),
        description: "d".into(),
        synonyms: vec!["x".into()],
        of: "aov".into(),
        window: "cumulative".into(),
        ..Default::default()
    });
    m.index().unwrap();

    let mut query = q(&["bad_aov_roll"], &["order_date"]);
    query.time_grain = "month".into();
    let err = compile(&m, &query, &dialect::Postgres).unwrap_err();
    assert!(
        err.to_string().contains("refused"),
        "unexpected error: {err}"
    );
}

#[test]
fn additivity_is_inferred_from_shape() {
    let m = test_model();
    assert_eq!(m.additivity("revenue"), ADDITIVE);
    assert_eq!(m.additivity("order_count"), NON_ADDITIVE); // count_distinct
    assert_eq!(m.additivity("aov"), NON_ADDITIVE); // ratio formula
    assert_eq!(m.additivity("rev_cumulative"), NON_ADDITIVE); // window
}

#[test]
fn lint_flags_a_metric_with_no_description() {
    let mut m = test_model();
    m.metrics[0].description = String::new();
    m.index().unwrap();
    let errs: Vec<_> = lint(&m).into_iter().filter(|i| i.severity == "error").collect();
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(errs[0].target, "revenue");
}

/// A ratio of two integer measures must not divide as an integer.
///
/// `SUM()` over an integer column returns an integer on Postgres, SQLite and
/// SQL Server, and integer division truncates: a defect rate of 2198/149815
/// comes back as 0. The query succeeds, returns a number, and the number is
/// wrong — the exact failure this layer exists to make impossible, so it is
/// worth a case per dialect rather than one for the shape.
#[test]
fn integer_measures_divide_as_decimals_on_every_dialect() {
    let m = Model::from_yaml(
        r#"
entities:
  - {name: inspection, table: inspection, primary_key: id}
metrics:
  - {name: defects,     entity: inspection, agg: sum, expr: defect_qty}
  - {name: checked,     entity: inspection, agg: sum, expr: checked_qty}
  - {name: defect_rate, formula: "defects / nullif(checked, 0)", additivity: non_additive}
"#,
    )
    .unwrap();

    for name in [
        "postgres",
        "mysql",
        "sqlite",
        "sqlserver",
        "snowflake",
        "databricks",
        "duckdb",
        "ansi",
    ] {
        let d = dialect::by_name(name).unwrap_or_else(|| panic!("{name}: no dialect"));
        let got = compile(&m, &q(&["defect_rate"], &[]), d.as_ref())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        // Both operands, not just the numerator: casting one side is enough for
        // the arithmetic but leaves the other in place for a reader to copy.
        let casts = got.sql.matches("CAST(").count();
        assert!(
            casts >= 2,
            "{name}: {casts} cast(s), want both operands cast\n{}",
            got.sql
        );
        if name == "sqlite" {
            assert!(
                got.sql.contains("AS REAL"),
                "sqlite: DECIMAL keeps NUMERIC affinity and still divides as an integer\n{}",
                got.sql
            );
        }
    }
}

/// A metric selected on its own is not a division and keeps its natural type.
#[test]
fn a_plain_metric_is_not_cast() {
    let m = Model::from_yaml(
        r#"
entities:
  - {name: inspection, table: inspection, primary_key: id}
metrics:
  - {name: defects, entity: inspection, agg: sum, expr: defect_qty}
"#,
    )
    .unwrap();
    let got = compile(&m, &q(&["defects"], &[]), &dialect::Postgres).unwrap();
    assert!(
        !got.sql.contains("CAST("),
        "plain metric should keep its type:\n{}",
        got.sql
    );
}

/// The same physical table joined twice under two roles. Each role must be
/// aliased distinctly and keyed by its own foreign column, or the query is an
/// ambiguous table reference.
#[test]
fn role_playing_dimensions_alias_the_same_table_twice() {
    let m = Model::from_yaml(
        r#"
entities:
  - {name: order_item, table: order_items, primary_key: id}
  - {name: order,      table: orders,      primary_key: order_id}
  - {name: sale_store, table: stores,      primary_key: store_id}
  - {name: ship_store, table: stores,      primary_key: store_id}
joins:
  - {from: order_item, to: order,      from_key: order_id,      to_key: order_id, cardinality: many_to_one}
  - {from: order,      to: sale_store, from_key: store_id,      to_key: store_id, cardinality: many_to_one}
  - {from: order,      to: ship_store, from_key: ship_store_id, to_key: store_id, cardinality: many_to_one}
dimensions:
  - {name: sale_region, entity: sale_store, column: region, type: categorical}
  - {name: ship_region, entity: ship_store, column: region, type: categorical}
metrics:
  - {name: revenue, description: d, synonyms: [r], entity: order_item, agg: sum, expr: "qty*price"}
"#,
    )
    .unwrap();

    let c = compile(
        &m,
        &q(&["revenue"], &["sale_region", "ship_region"]),
        &dialect::Postgres,
    )
    .unwrap();
    for want in [
        r#""stores" AS "sale_store""#,
        r#""stores" AS "ship_store""#,
        r#""order"."store_id" = "sale_store"."store_id""#,
        r#""order"."ship_store_id" = "ship_store"."store_id""#,
        r#""sale_store"."region""#,
        r#""ship_store"."region""#,
    ] {
        assert!(c.sql.contains(want), "missing {want:?} in:\n{}", c.sql);
    }
}

/// A measure on one side of a bridge may not be sliced by the other side —
/// there is no safe path, and inventing one multiplies the grain.
#[test]
fn slicing_across_a_bridge_is_refused_not_invented() {
    let m = Model::from_yaml(
        r#"
entities:
  - {name: enrollment, table: enrollments, primary_key: id}
  - {name: student,    table: students,    primary_key: student_id}
  - {name: course,     table: courses,     primary_key: course_id}
joins:
  - {from: enrollment, to: student, from_key: student_id, to_key: student_id, cardinality: many_to_one}
  - {from: enrollment, to: course,  from_key: course_id,  to_key: course_id,  cardinality: many_to_one}
dimensions:
  - {name: course_name, entity: course,  column: name, type: categorical}
  - {name: student_name, entity: student, column: name, type: categorical}
metrics:
  - {name: credits,      description: d, synonyms: [c], entity: enrollment, agg: sum, expr: credits}
  - {name: student_fees, description: d, synonyms: [f], entity: student,    agg: sum, expr: fees}
"#,
    )
    .unwrap();

    // The bridge measure can be sliced by either side — both are one hop up.
    compile(&m, &q(&["credits"], &["course_name"]), &dialect::Postgres)
        .expect("bridge measure by either side is safe");

    // A measure on the student side cannot reach course: refuse.
    let err = compile(&m, &q(&["student_fees"], &["course_name"]), &dialect::Postgres)
        .unwrap_err();
    assert!(
        err.to_string().contains("no declared join path"),
        "expected a refusal, got: {err}"
    );

    // And the same fact drives get_dimensions, so an agent self-corrects.
    let dims = m.dimensions_for("student_fees").unwrap();
    assert!(dims.contains(&"student_name".to_string()));
    assert!(!dims.contains(&"course_name".to_string()));
}

/// A time grain that matched no time dimension used to be dropped in silence:
/// you asked for revenue by month and got daily rows, correctly computed and
/// not what you asked for.
#[test]
fn a_grain_that_matches_no_time_dimension_is_refused() {
    let m = Model::from_yaml(
        r#"
entities:
  - {name: order, table: orders, primary_key: id}
dimensions:
  - {name: status, entity: order, column: status, type: categorical}
metrics:
  - {name: revenue, entity: order, agg: sum, expr: amount}
"#,
    )
    .unwrap();
    let mut query = q(&["revenue"], &["status"]);
    query.time_grain = "month".into();
    let err = compile(&m, &query, &dialect::Postgres).unwrap_err();
    assert!(
        err.to_string().contains("is declared type: time"),
        "got: {err}"
    );
}

/// Filters bind, never inline. The values come back in order so the driver can
/// pass them positionally.
#[test]
fn filters_bind_and_never_inline() {
    let m = test_model();
    let mut query = q(&["revenue"], &[]);
    query.where_ = vec![Filter {
        dimension: "order_date".into(),
        op: ">=".into(),
        values: vec![Value::Str("2026-01-01".into())],
    }];
    let c = compile(&m, &query, &dialect::Postgres).unwrap();
    assert!(c.sql.contains("$1"), "{}", c.sql);
    assert!(!c.sql.contains("2026-01-01"), "value was inlined:\n{}", c.sql);
    assert_eq!(c.args, vec![Value::Str("2026-01-01".into())]);
}

/// Each measure aggregates in its own CTE before anything is joined. This is
/// the whole technique — two metrics at different grains in one query must not
/// multiply each other.
#[test]
fn every_base_metric_aggregates_in_its_own_cte() {
    let m = test_model();
    let c = compile(&m, &q(&["revenue", "order_count"], &["order_date"]), &dialect::Postgres)
        .unwrap();
    assert!(c.sql.contains(r#""m_revenue" AS ("#), "{}", c.sql);
    assert!(c.sql.contains(r#""m_order_count" AS ("#), "{}", c.sql);
    // The spine is UNION-ed and LEFT JOINed, never a FULL JOIN (Postgres
    // rejects null-safe FULL JOIN conditions) and never a bare comma join.
    assert!(c.sql.contains("UNION"), "{}", c.sql);
    assert!(c.sql.contains("IS NOT DISTINCT FROM"), "{}", c.sql);
}

/// `keys` is a reserved word in MySQL; an unquoted spine alias turned every
/// multi-metric query into a syntax error.
#[test]
fn the_generated_spine_alias_is_quoted() {
    let m = test_model();
    let c = compile(&m, &q(&["revenue", "order_count"], &["order_date"]), &dialect::MySql).unwrap();
    assert!(c.sql.contains("`keys`"), "{}", c.sql);
}

/// A name always beats another metric's synonym.
#[test]
fn a_canonical_name_beats_a_synonym() {
    let m = test_model();
    assert_eq!(m.resolve_metric("sales"), Some("revenue"));
    assert_eq!(m.resolve_metric("Revenue"), Some("revenue"));
    assert_eq!(m.resolve_metric("order_count"), Some("order_count"));
    assert_eq!(m.resolve_metric("nope"), None);
}
