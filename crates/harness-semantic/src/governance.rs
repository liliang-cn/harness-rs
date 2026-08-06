//! Who may ask what, and what they get to see.
//!
//! Everything here is a pure function of the model, the query and the returned
//! rows — nothing opens a database. That matters for a reason beyond tidiness:
//! **these rules must be decided in exactly one place.** The moment a product
//! re-implements "can this role see this metric" on its own side, there are two
//! sets of rules, and two sets of rules are eventually different sets of rules.

use crate::compile::{Filter, Query, Value};
use crate::model::Model;
use std::collections::{HashMap, HashSet};

/// Who is asking.
#[derive(Clone, Debug, Default)]
pub struct Principal {
    pub user: String,
    pub role: String,
    pub attrs: HashMap<String, String>,
    /// The natural-language question this came from, when there was one.
    ///
    /// Recording it is what makes the trail answerable: *"who ran what"* is a
    /// compliance answer, and *"what did people actually ask"* is the one that
    /// tells you which metrics were worth building. It is also the only honest
    /// source for an eval set — questions written by the engineer are the
    /// questions the engineer already knows the system handles.
    pub question: String,
    /// Scopes the row to one customer. One deployment serving several customers
    /// writes all their trails into one table, and **an audit that cannot be
    /// filtered to a customer cannot be shown to that customer.**
    pub engagement: String,
}

/// Row-level security: for callers in `roles`, scope the query to rows where
/// `dimension` equals the caller's `attrs[attr_key]` — bound to the live
/// security context, never inlined as a literal.
#[derive(Clone, Debug)]
pub struct RowFilter {
    pub dimension: String,
    pub attr_key: String,
    pub roles: Vec<String>,
}

/// Masking, row-level security and k-anonymity.
#[derive(Clone, Debug, Default)]
pub struct Policy {
    /// Roles allowed to see masked dimensions raw.
    pub unmask: HashSet<String>,
    pub row_filters: Vec<RowFilter>,
    /// k-anonymity threshold; 0 turns it off.
    pub k: usize,
    /// Dimensions whose small cohorts must be suppressed.
    pub k_anon_dims: Vec<String>,
    pub k_anon_exempt: HashSet<String>,
    /// The metric used as the cohort-size measure.
    pub count_metric: String,
    /// 每个租户在当前计费窗口里的字节上限（0 = 不限）。
    ///
    /// 和单条查询的行数上限是**两件事**：行数上限管「一条查询不能太贵」，这个管
    /// 「一个人不能一整天都在发不太贵的查询」。只有前者的系统，会被一个每分钟发
    /// 一次全表扫描的看板刷爆,而每一条查询单看都在限额内。
    pub tenant_budget_bytes: i64,
}

impl Policy {
    /// The policy a deployment starts from.
    ///
    /// `Policy::default()` is every rule switched off, which is the right
    /// meaning for a struct literal and the wrong one for a server: a data
    /// plane that boots with no row filters and no k-anonymity looks governed
    /// — it has a policy, the API reports one — and enforces nothing. The
    /// failure is silent in the direction that leaks.
    ///
    /// The names here (`store_region`, `customer_email`, `order_count`) are a
    /// convention, not a discovery: a model that spells its region dimension
    /// differently is *not* scoped by this, and nothing will say so. Deployments
    /// that rename them must pass their own policy.
    pub fn baseline() -> Self {
        Policy {
            // Only admin sees masked dimensions raw.
            unmask: HashSet::from(["admin".to_string()]),
            // Managers see their own region.
            row_filters: vec![RowFilter {
                dimension: "store_region".into(),
                attr_key: "region".into(),
                roles: vec!["manager".into()],
            }],
            k: 5,
            k_anon_dims: vec!["customer_email".into()],
            k_anon_exempt: HashSet::from(["admin".to_string()]),
            count_metric: "order_count".into(),
            // 默认不限。一个默认就有预算上限的部署，会在没有人配过它的地方
            // 突然开始拒绝查询，而现场看到的是「系统坏了」。
            tenant_budget_bytes: 0,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GovernanceError {
    #[error("metric {metric:?} not authorized for role {role:?}")]
    NotAuthorized { metric: String, role: String },
    #[error("row policy: caller {user:?} has no {attr:?} attribute")]
    MissingAttr { user: String, attr: String },
}

/// Refuses a query naming a metric this role may not resolve.
///
/// An unknown metric is *not* refused here — the compiler reports it, and
/// reporting "not authorized" for something that doesn't exist tells a caller
/// probing for metric names that it does.
pub fn authorize(m: &Model, metrics: &[String], role: &str) -> Result<(), GovernanceError> {
    for name in metrics {
        let Some(mt) = m.metric(name) else { continue };
        if mt.roles.is_empty() {
            continue;
        }
        if !mt.roles.iter().any(|r| r == role) {
            return Err(GovernanceError::NotAuthorized {
                metric: name.clone(),
                role: role.into(),
            });
        }
    }
    Ok(())
}

/// Appends the caller's row-level filters to the query.
///
/// A caller in a filtered role with no value for the attribute is an error, not
/// an unfiltered query: silently dropping the filter is how a regional manager
/// sees every region.
pub fn apply_rls(q: &mut Query, p: &Principal, pol: &Policy) -> Result<(), GovernanceError> {
    for rf in &pol.row_filters {
        if !rf.roles.iter().any(|r| r == &p.role) {
            continue;
        }
        let val = p.attrs.get(&rf.attr_key).cloned().unwrap_or_default();
        if val.is_empty() {
            return Err(GovernanceError::MissingAttr {
                user: p.user.clone(),
                attr: rf.attr_key.clone(),
            });
        }
        q.where_.push(Filter {
            dimension: rf.dimension.clone(),
            op: "=".into(),
            values: vec![Value::Str(val)],
        });
    }
    Ok(())
}

/// Replaces masked dimension values in the returned rows.
///
/// Masking happens on the way out rather than in the SQL so the same compiled
/// query serves every role — and so a mask can never be the reason two roles
/// get different *numbers*, only different *labels*.
pub fn mask_columns(m: &Model, columns: &[String], rows: &mut [Vec<Value>], role: &str, pol: &Policy) {
    if pol.unmask.contains(role) {
        return;
    }
    for (ci, col) in columns.iter().enumerate() {
        let Some(d) = m.dimension(col) else { continue };
        if d.mask.is_empty() {
            continue;
        }
        let red = strip_quotes(&d.mask);
        for row in rows.iter_mut() {
            if ci < row.len() {
                row[ci] = Value::Str(red.clone());
            }
        }
    }
}

fn strip_quotes(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && b[0] == b'\'' && b[b.len() - 1] == b'\'' {
        return s[1..s.len() - 1].to_string();
    }
    s.to_string()
}

/// Whether k-anonymity applies to this query.
pub fn k_anon_active(q: &Query, p: &Principal, pol: &Policy) -> bool {
    if pol.k == 0 || pol.count_metric.is_empty() || pol.k_anon_exempt.contains(&p.role) {
        return false;
    }
    q.group_by
        .iter()
        .any(|gb| pol.k_anon_dims.iter().any(|kd| kd == gb))
}

/// Drops rows whose cohort is smaller than k, and removes the count column.
///
/// The count column goes because it is the thing that would let someone
/// reconstruct the suppressed cohorts by subtraction. Returns how many rows
/// were suppressed — a caller that reports "42 rows" without saying three were
/// withheld has published a number that does not add up.
pub fn k_anon_suppress(
    columns: &mut Vec<String>,
    rows: &mut Vec<Vec<Value>>,
    pol: &Policy,
) -> usize {
    let Some(ci) = columns.iter().position(|c| c == &pol.count_metric) else {
        return 0;
    };
    let mut suppressed = 0;
    let mut kept: Vec<Vec<Value>> = Vec::new();
    for row in rows.iter() {
        if row.get(ci).map(as_f64).unwrap_or(0.0) < pol.k as f64 {
            suppressed += 1;
            continue;
        }
        let mut r = row.clone();
        r.remove(ci);
        kept.push(r);
    }
    *rows = kept;
    columns.remove(ci);
    suppressed
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Float(f) => *f,
        Value::Int(i) => *i as f64,
        Value::Bool(b) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        // Numeric columns come back as text on several drivers (Postgres
        // NUMERIC, SQLite's affinity rules). Treating an unparseable string as
        // zero suppresses the row, which is the safe direction for a k-anonymity
        // check — the unsafe direction is publishing a cohort of one.
        Value::Str(s) => s.trim().parse().unwrap_or(0.0),
        Value::Null => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        Model::from_yaml(
            r#"
entities:
  - {name: order, table: orders, primary_key: id}
dimensions:
  - {name: region, entity: order, column: region, type: categorical}
  - {name: customer_email, entity: order, column: email, type: categorical, mask: "'***'"}
metrics:
  - {name: revenue, entity: order, agg: sum, expr: amount, roles: [finance, admin]}
  - {name: order_count, entity: order, agg: count, expr: id}
"#,
        )
        .unwrap()
    }

    #[test]
    fn a_gated_metric_is_refused_for_other_roles() {
        let m = model();
        assert!(authorize(&m, &["revenue".into()], "finance").is_ok());
        assert!(authorize(&m, &["order_count".into()], "clerk").is_ok()); // ungated
        let err = authorize(&m, &["revenue".into()], "ceo").unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"metric "revenue" not authorized for role "ceo""#
        );
    }

    #[test]
    fn an_unknown_metric_is_left_to_the_compiler() {
        // Answering "not authorized" for a metric that doesn't exist tells a
        // caller probing for names that it does.
        let m = model();
        assert!(authorize(&m, &["nope".into()], "ceo").is_ok());
    }

    #[test]
    fn rls_binds_the_callers_own_value_and_refuses_when_it_is_missing() {
        let m = model();
        let _ = &m;
        let pol = Policy {
            row_filters: vec![RowFilter {
                dimension: "region".into(),
                attr_key: "region".into(),
                roles: vec!["manager".into()],
            }],
            ..Default::default()
        };

        let mut q = Query::default();
        let mut p = Principal {
            user: "wang".into(),
            role: "manager".into(),
            ..Default::default()
        };
        p.attrs.insert("region".into(), "east".into());
        apply_rls(&mut q, &p, &pol).unwrap();
        assert_eq!(q.where_.len(), 1);
        assert_eq!(q.where_[0].values, vec![Value::Str("east".into())]);

        // No attribute → refuse. Dropping the filter is how a regional manager
        // quietly sees every region.
        let mut q2 = Query::default();
        let p2 = Principal {
            user: "wang".into(),
            role: "manager".into(),
            ..Default::default()
        };
        assert!(apply_rls(&mut q2, &p2, &pol).is_err());

        // A role with no filter is untouched.
        let mut q3 = Query::default();
        let p3 = Principal {
            role: "finance".into(),
            ..Default::default()
        };
        apply_rls(&mut q3, &p3, &pol).unwrap();
        assert!(q3.where_.is_empty());
    }

    #[test]
    fn masking_replaces_values_except_for_unmasked_roles() {
        let m = model();
        let cols = vec!["customer_email".to_string(), "revenue".to_string()];
        let mut rows = vec![vec![Value::Str("a@b.com".into()), Value::Int(10)]];
        let pol = Policy {
            unmask: HashSet::from(["admin".to_string()]),
            ..Default::default()
        };

        mask_columns(&m, &cols, &mut rows, "finance", &pol);
        assert_eq!(rows[0][0], Value::Str("***".into()));
        assert_eq!(rows[0][1], Value::Int(10), "the number must not change");

        let mut raw = vec![vec![Value::Str("a@b.com".into()), Value::Int(10)]];
        mask_columns(&m, &cols, &mut raw, "admin", &pol);
        assert_eq!(raw[0][0], Value::Str("a@b.com".into()));
    }

    #[test]
    fn k_anonymity_drops_small_cohorts_and_the_count_that_would_rebuild_them() {
        let pol = Policy {
            k: 5,
            k_anon_dims: vec!["customer_email".into()],
            count_metric: "order_count".into(),
            ..Default::default()
        };
        let mut q = Query {
            group_by: vec!["customer_email".into()],
            ..Default::default()
        };
        let p = Principal {
            role: "analyst".into(),
            ..Default::default()
        };
        assert!(k_anon_active(&q, &p, &pol));

        // Exempt roles and unrelated group-bys switch it off.
        let admin = Principal {
            role: "admin".into(),
            ..Default::default()
        };
        let mut exempt = pol.clone();
        exempt.k_anon_exempt = HashSet::from(["admin".to_string()]);
        assert!(!k_anon_active(&q, &admin, &exempt));
        q.group_by = vec!["region".into()];
        assert!(!k_anon_active(&q, &p, &pol));

        let mut cols = vec!["customer_email".to_string(), "order_count".to_string()];
        let mut rows = vec![
            vec![Value::Str("a".into()), Value::Int(9)],
            vec![Value::Str("b".into()), Value::Int(2)],
            // Postgres hands NUMERIC back as text; it still has to count.
            vec![Value::Str("c".into()), Value::Str("7".into())],
        ];
        let n = k_anon_suppress(&mut cols, &mut rows, &pol);
        assert_eq!(n, 1);
        assert_eq!(rows.len(), 2);
        assert_eq!(cols, vec!["customer_email".to_string()], "the count goes too");
        assert_eq!(rows[0].len(), 1);
    }
}

#[cfg(test)]
mod baseline_tests {
    use super::*;

    /// `default()` 是所有规则都关掉 —— 对一个结构体字面量是对的意思，对一个数据面
    /// 是错的：它看上去有策略（API 也会报出来一个），实际上一条都不拦。
    #[test]
    fn the_baseline_is_not_the_empty_policy() {
        let d = Policy::default();
        assert!(d.row_filters.is_empty() && d.k == 0, "default 就该是全关");

        let b = Policy::baseline();
        assert_eq!(b.row_filters.len(), 1, "经理按大区收窄");
        assert_eq!(b.row_filters[0].roles, ["manager"]);
        assert_eq!(b.k, 5);
        assert!(b.unmask.contains("admin"));
        assert!(b.k_anon_exempt.contains("admin"));
        assert!(!b.count_metric.is_empty(), "k 匿名要数群体大小，没有计数指标就不成立");
    }
}
