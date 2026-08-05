//! The compiler: a semantic query in, fan-out/chasm-safe SQL out.
//!
//! **The one technique this whole crate exists for:** aggregate each measure to
//! its base grain inside its own CTE *first*, and only then join dimensions.
//! That single move makes fan-out and chasm joins impossible by construction,
//! rather than something a reviewer has to notice.

use crate::dialect::Dialect;
use crate::model::{ADDITIVE, Model};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// A semantic query: pure intent, zero table names, zero join keywords.
#[derive(Clone, Debug, Default)]
pub struct Query {
    pub metrics: Vec<String>,
    pub group_by: Vec<String>,
    pub where_: Vec<Filter>,
    /// day | week | month | quarter | year — applied to time dimensions.
    pub time_grain: String,
    pub order_by: String,
    pub descending: bool,
    pub limit: usize,
}

/// One predicate against a dimension — never against a raw column.
///
/// `PartialEq` because callers that *build* filters — grounding a question into
/// a time window, replaying a recon case — have to be able to assert on the
/// filter they produced. A predicate that can only be compared by rendering it
/// to SQL is a predicate whose tests all go through the compiler.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Filter {
    pub dimension: String,
    /// `=` | `!=` | `>` | `>=` | `<` | `<=` | `in`
    pub op: String,
    #[serde(default)]
    pub values: Vec<Value>,
}

/// A bind value. Deliberately small: the compiler never inlines a literal into
/// SQL, so this is only what a placeholder can carry.
/// Untagged on the wire, so a filter reads as it would be written by hand:
/// `values: ["east"]`, `values: [5]`. A tagged form would make every recon file
/// and every API call carry a type name the caller already conveyed by writing
/// the literal.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

/// SQL plus ordered bind arguments.
#[derive(Clone, Debug)]
pub struct Compiled {
    pub sql: String,
    pub args: Vec<Value>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CompileError {
    #[error("query has no metrics")]
    NoMetrics,
    #[error("unknown metric {0:?}")]
    UnknownMetric(String),
    #[error("unknown dimension {0:?}")]
    UnknownDimension(String),
    #[error("unknown dimension {0:?} in filter")]
    UnknownFilterDimension(String),
    #[error("metric {0:?} has a cyclic definition")]
    Cyclic(String),
    #[error("window metric {0:?} needs `of`")]
    WindowNeedsOf(String),
    #[error("window metric {0:?} needs a time dimension in group_by")]
    WindowNeedsTime(String),
    #[error(
        "window metric {metric:?} sums {class} metric {of:?} over time — refused ({class} measures cannot be summed; use a point-in-time pick or a ratio metric)"
    )]
    WindowSumsNonAdditive {
        metric: String,
        of: String,
        class: String,
    },
    #[error("metric {metric:?}: bad window {got:?} (use rolling:N|cumulative|prior:N|delta:N)")]
    BadWindow { metric: String, got: String },
    #[error("metric {metric:?}: unsupported agg {got:?}")]
    BadAgg { metric: String, got: String },
    #[error(
        "no declared join path from {from:?} to {to:?} (refused — add the relationship edge)"
    )]
    NoJoinPath { from: String, to: String },
    #[error(
        "time grain {grain:?} was requested but none of the group-by dimensions {dims:?} is declared type: time — fix the dimension's type in the model, or drop the grain"
    )]
    GrainMatchedNothing { grain: String, dims: Vec<String> },
}

static IDENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").unwrap());
static WINDOW_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(rolling|prior|delta):(\d+)$").unwrap());

/// Identifiers in an expression, in order — how a formula's metric references
/// are found.
pub(crate) fn idents(s: &str) -> Vec<String> {
    IDENT_RE.find_iter(s).map(|m| m.as_str().to_string()).collect()
}

/// Rewrites every identifier in `s` through `f`.
fn replace_idents(s: &str, mut f: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for m in IDENT_RE.find_iter(s) {
        out.push_str(&s[last..m.start()]);
        out.push_str(&f(m.as_str()));
        last = m.end();
    }
    out.push_str(&s[last..]);
    out
}

#[derive(Clone, Debug)]
struct ResolvedDim {
    name: String,
    entity: String,
    kind: String,
    /// `date_trunc(grain, raw)` for time dims, else the qualified column.
    sql: String,
}

struct JoinStep {
    right_table: String,
    right_alias: String,
    left_alias: String,
    left_col: String,
    right_col: String,
}

/// Compiles a semantic query into SQL for `d`.
pub fn compile(m: &Model, q: &Query, d: &dyn Dialect) -> Result<Compiled, CompileError> {
    if q.metrics.is_empty() {
        return Err(CompileError::NoMetrics);
    }
    let mut c = Compiler {
        m,
        q,
        d,
        args: Vec::new(),
        base_order: Vec::new(),
        base_seen: HashSet::new(),
        dims: Vec::new(),
        single: false,
    };

    c.resolve_dims(&q.group_by)?;

    // Pass 1 — classify: collect the base metrics everything depends on,
    // through derived formulas and window `of`, without emitting any SQL.
    for name in &q.metrics {
        c.collect_bases(name, &mut Vec::new())?;
    }
    c.single = c.base_order.len() == 1;

    // Pass 2 — the outer-SELECT expression for each requested metric.
    let mut out_cols = Vec::with_capacity(q.metrics.len());
    for name in &q.metrics {
        let expr = c.expr_for(name, &mut Vec::new())?;
        out_cols.push(format!("{expr} AS {}", d.quote_ident(name)));
    }

    // One CTE per base metric — aggregate to grain first.
    let mut ctes = Vec::with_capacity(c.base_order.len());
    for bm in c.base_order.clone() {
        ctes.push(c.build_cte(&bm)?);
    }

    let sql = c.assemble(&ctes, &out_cols);
    Ok(Compiled { sql, args: c.args })
}

struct Compiler<'a> {
    m: &'a Model,
    q: &'a Query,
    d: &'a dyn Dialect,
    args: Vec<Value>,
    base_order: Vec<String>,
    base_seen: HashSet<String>,
    dims: Vec<ResolvedDim>,
    single: bool,
}

impl<'a> Compiler<'a> {
    /// Names the per-metric CTE. Generated identifiers go through the dialect's
    /// quoting too: `keys` is a reserved word in MySQL, and leaving it unquoted
    /// turned every multi-metric query into a syntax error.
    fn cte(&self, metric: &str) -> String {
        self.d.quote_ident(&format!("m_{metric}"))
    }
    fn keys_alias(&self) -> String {
        self.d.quote_ident("keys")
    }

    /// How a dimension is referenced in the outer SELECT: from the single base
    /// CTE, or from the UNION-ed key spine.
    fn dim_ref(&self, d: &ResolvedDim) -> String {
        if self.single {
            format!("{}.{}", self.cte(&self.base_order[0]), self.d.quote_ident(&d.name))
        } else {
            format!("{}.{}", self.keys_alias(), self.d.quote_ident(&d.name))
        }
    }

    fn ph(&mut self, v: Value) -> String {
        self.args.push(v);
        self.d.placeholder(self.args.len())
    }

    fn use_base(&mut self, name: &str) {
        if self.base_seen.insert(name.to_string()) {
            self.base_order.push(name.to_string());
        }
    }

    fn collect_bases(&mut self, name: &str, visiting: &mut Vec<String>) -> Result<(), CompileError> {
        let Some(mt) = self.m.metric(name) else {
            return Err(CompileError::UnknownMetric(name.into()));
        };
        if visiting.iter().any(|v| v == name) {
            return Err(CompileError::Cyclic(name.into()));
        }
        visiting.push(name.to_string());
        let r = if mt.is_window() {
            if mt.of.is_empty() {
                Err(CompileError::WindowNeedsOf(name.into()))
            } else {
                let of = mt.of.clone();
                self.collect_bases(&of, visiting)
            }
        } else if mt.is_derived() {
            let toks = idents(&mt.formula);
            let mut r = Ok(());
            for t in toks {
                if self.m.metric(&t).is_some() {
                    r = self.collect_bases(&t, visiting);
                    if r.is_err() {
                        break;
                    }
                }
            }
            r
        } else {
            self.use_base(name);
            Ok(())
        };
        visiting.pop();
        r
    }

    fn expr_for(&mut self, name: &str, visiting: &mut Vec<String>) -> Result<String, CompileError> {
        let Some(mt) = self.m.metric(name) else {
            return Err(CompileError::UnknownMetric(name.into()));
        };
        if mt.is_window() {
            return self.window_expr(name);
        }
        if mt.is_derived() {
            if visiting.iter().any(|v| v == name) {
                return Err(CompileError::Cyclic(name.into()));
            }
            visiting.push(name.to_string());
            let formula = mt.formula.clone();
            // Collect substitutions first: replace_idents takes a closure that
            // cannot itself fail, and swallowing an error inside it is exactly
            // how a bad sub-metric would become silent text.
            let mut subs: HashMap<String, String> = HashMap::new();
            for tok in idents(&formula) {
                if self.m.metric(&tok).is_some() && !subs.contains_key(&tok) {
                    let sub = self.expr_for(&tok, visiting)?;
                    // Cast at the point of substitution, not at the metric's own
                    // definition: a formula is where the division happens, and
                    // where a count of integers stops being a count and becomes
                    // a numerator. A metric selected on its own keeps its type.
                    subs.insert(tok, self.d.cast_decimal(&sub));
                }
            }
            visiting.pop();
            let out = replace_idents(&formula, |tok| {
                subs.get(tok).cloned().unwrap_or_else(|| tok.to_string())
            });
            return Ok(format!("({out})"));
        }
        Ok(format!(
            "COALESCE({}.{}, 0)",
            self.cte(name),
            self.d.quote_ident(name)
        ))
    }

    fn window_expr(&mut self, name: &str) -> Result<String, CompileError> {
        let mt = self.m.metric(name).unwrap().clone();
        let value = self.expr_for(&mt.of, &mut Vec::new())?;

        let mut time_ref = String::new();
        let mut parts: Vec<String> = Vec::new();
        for d in &self.dims.clone() {
            if d.kind == "time" && time_ref.is_empty() {
                time_ref = self.dim_ref(d);
            } else {
                parts.push(self.dim_ref(d));
            }
        }
        if time_ref.is_empty() {
            return Err(CompileError::WindowNeedsTime(name.into()));
        }
        // Grain-to-date: restart the window at each boundary of `reset` by
        // partitioning on the truncated period. Applying date_trunc to an
        // already-truncated timeRef is exact, since
        // date_trunc(year, date_trunc(month, x)) = date_trunc(year, x).
        if !mt.reset.is_empty() {
            parts.push(self.d.date_trunc(&mt.reset, &time_ref));
        }
        let mut over = String::from("OVER (");
        if !parts.is_empty() {
            over.push_str(&format!("PARTITION BY {} ", parts.join(", ")));
        }
        over.push_str(&format!("ORDER BY {time_ref}"));

        // rolling/cumulative re-sum the value across rows: refuse when the
        // underlying measure is not safe to sum. A structural guard against a
        // clean-running but wrong roll-up.
        let is_sum = mt.window == "cumulative" || mt.window.starts_with("rolling:");
        if is_sum {
            let add = self.m.additivity(&mt.of);
            if add != ADDITIVE {
                return Err(CompileError::WindowSumsNonAdditive {
                    metric: name.into(),
                    of: mt.of.clone(),
                    class: add.into(),
                });
            }
        }

        if mt.window == "cumulative" {
            return Ok(format!("SUM({value}) {over} ROWS UNBOUNDED PRECEDING)"));
        }
        let Some(caps) = WINDOW_RE.captures(&mt.window) else {
            return Err(CompileError::BadWindow {
                metric: name.into(),
                got: mt.window.clone(),
            });
        };
        let kind = &caps[1];
        let n: i64 = caps[2].parse().unwrap_or(1);
        Ok(match kind {
            // rolling:N spans N rows = N-1 preceding + current.
            "rolling" => format!(
                "SUM({value}) {over} ROWS BETWEEN {} PRECEDING AND CURRENT ROW)",
                (n - 1).max(0)
            ),
            "prior" => format!("LAG({value}, {n}) {over})"),
            _ => format!("({value} - LAG({value}, {n}) {over}))"),
        })
    }

    fn qualify(&self, entity: &str, col: &str) -> String {
        format!("{}.{}", self.d.quote_ident(entity), self.d.quote_ident(col))
    }

    fn resolve_dims(&mut self, names: &[String]) -> Result<(), CompileError> {
        let mut out = Vec::with_capacity(names.len());
        let mut applied = false;
        for n in names {
            let Some(dim) = self.m.dimension(n) else {
                return Err(CompileError::UnknownDimension(n.clone()));
            };
            let raw = self.qualify(&dim.entity, &dim.column);
            let mut expr = raw.clone();
            if dim.kind == "time" && !self.q.time_grain.is_empty() {
                expr = self.d.date_trunc(&self.q.time_grain, &raw);
                applied = true;
            }
            out.push(ResolvedDim {
                name: n.clone(),
                entity: dim.entity.clone(),
                kind: dim.kind.clone(),
                sql: expr,
            });
        }
        // A grain that matched no time dimension used to be dropped in silence:
        // ask for revenue by month against a dimension the model calls
        // categorical and you got daily rows — correctly computed, and not what
        // you asked for, with nothing in the answer saying so. Refuse instead:
        // a caller can act on an error.
        if !self.q.time_grain.is_empty() && !applied {
            return Err(CompileError::GrainMatchedNothing {
                grain: self.q.time_grain.clone(),
                dims: names.to_vec(),
            });
        }
        self.dims = out;
        Ok(())
    }

    fn build_cte(&mut self, metric_name: &str) -> Result<String, CompileError> {
        let mt = self.m.metric(metric_name).unwrap().clone();
        let base = mt.entity.clone();

        // entities needed = base + dim entities + filter-dim entities
        let mut need: HashSet<String> = HashSet::new();
        need.insert(base.clone());
        for d in &self.dims {
            need.insert(d.entity.clone());
        }
        for f in &self.q.where_ {
            let Some(fd) = self.m.dimension(&f.dimension) else {
                return Err(CompileError::UnknownFilterDimension(f.dimension.clone()));
            };
            need.insert(fd.entity.clone());
        }
        let joins = self.plan_joins(&base, &need)?;

        let agg = agg_expr(&mt.agg, &mt.expr).ok_or_else(|| CompileError::BadAgg {
            metric: metric_name.into(),
            got: mt.agg.clone(),
        })?;

        let mut sel = Vec::new();
        let mut grp = Vec::new();
        for d in &self.dims {
            sel.push(format!("{} AS {}", d.sql, self.d.quote_ident(&d.name)));
            grp.push(d.sql.clone());
        }
        sel.push(format!("{agg} AS {}", self.d.quote_ident(metric_name)));

        // Alias the base table by entity name (FROM "orders" AS "order") so
        // columns qualify against the alias, and role-playing or bridge entities
        // on the same physical table stay distinct.
        let mut b = format!(
            "{} AS (\n  SELECT {}\n  FROM {} AS {}",
            self.cte(metric_name),
            sel.join(", "),
            self.d.quote_ident(&self.m.entity(&base).unwrap().table),
            self.d.quote_ident(&base)
        );
        for j in &joins {
            b.push_str(&format!(
                "\n  JOIN {} AS {} ON {}.{} = {}.{}",
                self.d.quote_ident(&j.right_table),
                self.d.quote_ident(&j.right_alias),
                self.d.quote_ident(&j.left_alias),
                self.d.quote_ident(&j.left_col),
                self.d.quote_ident(&j.right_alias),
                self.d.quote_ident(&j.right_col),
            ));
        }
        let where_ = self.build_where();
        if !where_.is_empty() {
            b.push_str(&format!("\n  WHERE {where_}"));
        }
        if !grp.is_empty() {
            b.push_str(&format!("\n  GROUP BY {}", grp.join(", ")));
        }
        b.push_str("\n)");
        Ok(b)
    }

    fn build_where(&mut self) -> String {
        let filters = self.q.where_.clone();
        let mut preds = Vec::new();
        for f in &filters {
            let Some(fd) = self.m.dimension(&f.dimension) else {
                continue;
            };
            let col = self.qualify(&fd.entity, &fd.column);
            match f.op.to_lowercase().as_str() {
                "in" => {
                    let phs: Vec<String> =
                        f.values.iter().map(|v| self.ph(v.clone())).collect();
                    preds.push(format!("{col} IN ({})", phs.join(", ")));
                }
                op @ ("=" | "!=" | ">" | ">=" | "<" | "<=") => {
                    let v = f.values.first().cloned().unwrap_or(Value::Null);
                    let ph = self.ph(v);
                    preds.push(format!("{col} {op} {ph}"));
                }
                _ => {}
            }
        }
        preds.join(" AND ")
    }

    /// Finds a safe (many-to-one only) join path from `base` to every needed
    /// entity. A needed entity with no declared upward path is refused — the
    /// compiler never invents a join.
    fn plan_joins(
        &self,
        base: &str,
        need: &HashSet<String>,
    ) -> Result<Vec<JoinStep>, CompileError> {
        let adj = self.m.adjacency();

        let mut came: HashMap<String, (String, crate::reach::Edge)> = HashMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut queue = vec![base.to_string()];
        let mut seen: HashSet<String> = HashSet::from([base.to_string()]);
        while let Some(cur) = queue.first().cloned() {
            queue.remove(0);
            for e in adj.get(&cur).into_iter().flatten() {
                if !seen.insert(e.to.clone()) {
                    continue;
                }
                came.insert(e.to.clone(), (cur.clone(), e.clone()));
                order.push(e.to.clone());
                queue.push(e.to.clone());
            }
        }

        // Deterministic error: a HashSet iterates in an arbitrary order, and an
        // arbitrary one of several unreachable entities in the message makes the
        // same broken model report a different problem on each run.
        let mut unreachable: Vec<&String> = need
            .iter()
            .filter(|e| e.as_str() != base && !seen.contains(*e))
            .collect();
        unreachable.sort();
        if let Some(ent) = unreachable.first() {
            return Err(CompileError::NoJoinPath {
                from: base.into(),
                to: (*ent).clone(),
            });
        }

        let mut required: HashSet<String> = HashSet::new();
        for ent in need {
            let mut cur = ent.clone();
            while cur != base {
                required.insert(cur.clone());
                cur = came[&cur].0.clone();
            }
        }
        let mut steps = Vec::new();
        for ent in &order {
            if !required.contains(ent) {
                continue;
            }
            let (parent, e) = &came[ent];
            steps.push(JoinStep {
                right_table: self.m.entity(ent).unwrap().table.clone(),
                right_alias: ent.clone(),
                left_alias: parent.clone(),
                left_col: e.left_col.clone(),
                right_col: e.right_col.clone(),
            });
        }
        Ok(steps)
    }

    fn assemble(&self, ctes: &[String], out_cols: &[String]) -> String {
        let mut b = format!("WITH {}\n", ctes.join(",\n"));
        let first = self.cte(&self.base_order[0]);

        let mut sel: Vec<String> = self
            .dims
            .iter()
            .map(|d| format!("{} AS {}", self.dim_ref(d), self.d.quote_ident(&d.name)))
            .collect();
        sel.extend_from_slice(out_cols);
        b.push_str(&format!("SELECT {}\n", sel.join(", ")));

        if self.single {
            b.push_str(&format!("FROM {first}"));
        } else if self.dims.is_empty() {
            // Grand total: each CTE has exactly one row, so CROSS JOIN is safe.
            b.push_str(&format!("FROM {first}"));
            for bm in &self.base_order[1..] {
                b.push_str(&format!(" CROSS JOIN {}", self.cte(bm)));
            }
        } else {
            // A dimension spine (UNION of each CTE's dim tuple), then LEFT JOIN
            // each metric CTE onto it. Avoids FULL JOIN — Postgres rejects
            // null-safe FULL JOIN conditions — while still keeping rows that
            // appear in any one CTE.
            let dim_cols: Vec<String> = self
                .dims
                .iter()
                .map(|d| self.d.quote_ident(&d.name))
                .collect();
            let unions: Vec<String> = self
                .base_order
                .iter()
                .map(|bm| format!("SELECT {} FROM {}", dim_cols.join(", "), self.cte(bm)))
                .collect();
            b.push_str(&format!(
                "FROM ({}) {}",
                unions.join(" UNION "),
                self.keys_alias()
            ));
            for bm in &self.base_order {
                let alias = self.cte(bm);
                let on: Vec<String> = self
                    .dims
                    .iter()
                    .map(|d| {
                        let l = format!("{}.{}", self.keys_alias(), self.d.quote_ident(&d.name));
                        let r = format!("{}.{}", alias, self.d.quote_ident(&d.name));
                        self.d.distinct_from(&l, &r)
                    })
                    .collect();
                b.push_str(&format!(" LEFT JOIN {alias} ON {}", on.join(" AND ")));
            }
        }

        if !self.q.order_by.is_empty() {
            b.push_str(&format!(
                "\nORDER BY {}{}",
                self.d.quote_ident(&self.q.order_by),
                if self.q.descending { " DESC" } else { "" }
            ));
        }
        if self.q.limit > 0 {
            b.push_str(&format!("\nLIMIT {}", self.q.limit));
        }
        b
    }
}

fn agg_expr(agg: &str, expr: &str) -> Option<String> {
    Some(match agg.to_lowercase().as_str() {
        "sum" => format!("SUM({expr})"),
        "count" => format!("COUNT({expr})"),
        "count_distinct" => format!("COUNT(DISTINCT {expr})"),
        "avg" => format!("AVG({expr})"),
        "min" => format!("MIN({expr})"),
        "max" => format!("MAX({expr})"),
        _ => return None,
    })
}
