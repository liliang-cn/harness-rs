//! The model: entities, joins, dimensions, metrics — business meaning compiled
//! to SQL once, instead of re-decided in every question.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// An error the model can raise while being loaded or indexed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("entity {0:?}: name, table and primary_key are required")]
    IncompleteEntity(String),
    #[error("dimension {dim:?} references unknown entity {entity:?}")]
    UnknownDimEntity { dim: String, entity: String },
    #[error("dimension {0:?}: type must be categorical or time")]
    BadDimType(String),
    #[error("metric {metric:?} references unknown entity {entity:?}")]
    UnknownMetricEntity { metric: String, entity: String },
    #[error("metric {metric:?}: bad additivity {got:?} (additive|semi_additive|non_additive)")]
    BadAdditivity { metric: String, got: String },
    #[error("metric {0:?}: reset is only valid on a window metric")]
    ResetWithoutWindow(String),
    #[error("metric {metric:?}: bad reset {got:?} (day|week|month|quarter|year)")]
    BadReset { metric: String, got: String },
    #[error("join {from}->{to} references an unknown entity")]
    UnknownJoinEntity { from: String, to: String },
    #[error("join {from}->{to}: bad cardinality {got:?}")]
    BadCardinality {
        from: String,
        to: String,
        got: String,
    },
    #[error(
        "timezone {0:?} is not an IANA zone name (letters, digits, and _ + - / only, e.g. Asia/Shanghai)"
    )]
    BadTimezone(String),
    #[error(
        "metric {metric:?} is declared semi_additive but aggregates with {agg:?} — refused at load. A semi-additive measure is one row per period per entity, so summing it adds yesterday's balance to today's. Use min/max/avg for a point query, or drop the semi_additive declaration if it really is summable."
    )]
    SemiAdditiveSummed { metric: String, agg: String },
    #[error("{0}")]
    Parse(String),
}

/// A real business thing with a primary key the layer joins on — **declared,
/// never guessed**.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entity {
    pub name: String,
    pub table: String,
    pub primary_key: String,
}

/// One declared edge of the join graph: keys + cardinality.
///
/// The compiler only ever traverses declared edges in the safe (many-to-one)
/// direction. A missing edge is refused, never invented — an invented join is
/// how a total silently becomes six times too large.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Join {
    pub from: String,
    pub to: String,
    pub from_key: String,
    pub to_key: String,
    /// `many_to_one` | `one_to_many` | `many_to_many`
    pub cardinality: String,
}

/// A typed attribute to group or filter by, named in business words.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dimension {
    pub name: String,
    pub entity: String,
    pub column: String,
    /// `categorical` | `time`
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub synonyms: Vec<String>,
    /// SQL expression returned when the caller may not see the raw value.
    #[serde(default)]
    pub mask: String,
    /// If set, only these roles may group or filter by this dimension.
    ///
    /// **Masking is not a substitute for this.** A mask changes the *label*; it
    /// does not stop the caller from slicing by the column. `revenue by
    /// customer_email` with a masked email still returns one row per customer —
    /// the cohort structure, the count, and the ordering are the customer list,
    /// spelled with `***` where the name would be. And a filter is worse: given
    /// `where customer_email = ?`, whether the number moves answers the question
    /// directly.
    ///
    /// k-anonymity covers part of this, but only for dimensions someone
    /// remembered to name in `k_anon_dims`, and only when `k > 0` — which
    /// `Policy::default()` is not.
    #[serde(default)]
    pub roles: Vec<String>,
    /// The values this dimension actually takes, when there are few enough to
    /// list. Empty means "not enumerated" — never "no values".
    ///
    /// A dimension declares a *column*; that is enough to group by, and not
    /// enough to understand "只看南区". Without this, the layer above cannot
    /// tell whether 南区 is a region, a product line or a typo, so it drops the
    /// restriction — and the answer covers every region, silently, and is
    /// larger than the truth.
    ///
    /// Only worth filling for low-cardinality columns. A customer id column has
    /// no useful list, and a list nobody can read is not documentation.
    #[serde(default)]
    pub values: Vec<String>,
}

/// An aggregated number with grain and aggregation locked in.
///
/// A *base* metric aggregates `expr` over its `entity`. A *derived* metric is a
/// formula over other metric names — which is how chasm traps are avoided: each
/// base metric aggregates in its own CTE before anything is combined.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metric {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub synonyms: Vec<String>,

    // base
    #[serde(default)]
    pub entity: String,
    /// sum | count | count_distinct | avg | min | max
    #[serde(default)]
    pub agg: String,
    /// SQL expression at base grain, e.g. `quantity * unit_price`
    #[serde(default)]
    pub expr: String,

    // derived
    #[serde(default)]
    pub formula: String,

    // time-window: a transform of metric `of` over the time dimension.
    #[serde(default)]
    pub of: String,
    /// `rolling:N` | `cumulative` | `prior:N` | `delta:N`
    #[serde(default)]
    pub window: String,
    /// Restarts the accumulation at each boundary of this period (year → YTD).
    #[serde(default)]
    pub reset: String,

    /// How the measure may be rolled up. Empty means "infer from shape".
    #[serde(default)]
    pub additivity: String,

    /// If set, only these roles may resolve the metric.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl Metric {
    pub fn is_derived(&self) -> bool {
        !self.formula.is_empty()
    }
    pub fn is_window(&self) -> bool {
        !self.window.is_empty()
    }
}

/// Additivity classes.
pub const ADDITIVE: &str = "additive";
pub const SEMI_ADDITIVE: &str = "semi_additive";
pub const NON_ADDITIVE: &str = "non_additive";

fn valid_period(s: &str) -> bool {
    matches!(s, "day" | "week" | "month" | "quarter" | "year")
}

/// Normalizes a name for synonym lookup: case- and space-insensitive.
pub(crate) fn norm_name(s: &str) -> String {
    s.trim().to_lowercase().replace(['_', '-', ' '], "")
}

/// The single source of truth.
///
/// **顶层刻意不加 `deny_unknown_fields`，四个内层结构刻意加。**
///
/// 拼写风险在叶子字段：把 `synonyms:` 写成 `synonym:`，那个维度解析得干干净净，
/// 然后中文问句永远匹配不上它，而 lint 说的是「这个维度有问题」，不是「你拼错
/// 了一个键」。所以 Entity / Join / Dimension / Metric 一律严格。
///
/// 扩展点在顶层：一份模型文件合法地带着别的段（`governance:` 之类），那些段属于
/// 别的工具。在这里拒绝它们，等于要求整个平台的每一个键都由编译器认识 —— 而
/// di-writeback 往模型里加一个指标时，正是靠保留这些段才没把它们删掉。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Model {
    #[serde(default)]
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub joins: Vec<Join>,
    #[serde(default)]
    pub dimensions: Vec<Dimension>,
    #[serde(default)]
    pub metrics: Vec<Metric>,

    /// The timezone the business is transacted in, as an IANA name
    /// (`Asia/Shanghai`). Empty means **the database session's timezone**,
    /// whatever that happens to be.
    ///
    /// Every time bucket in this layer is a `date_trunc`, and `date_trunc` has no
    /// opinion about timezones — it truncates in whatever zone the session is in.
    /// A warehouse storing `timestamptz` in UTC therefore cuts "this month" at
    /// 08:00 on the 1st for a business in UTC+8: the last eight hours of every
    /// month land in the next one. Both months are off, both look plausible, and
    /// nothing in the answer says which zone it was bucketed in.
    ///
    /// Declared here rather than per query because it is a property of the
    /// business, not of the question. Two people asking "本月流水" must not get
    /// two different months because one of them passed the parameter.
    #[serde(default)]
    pub timezone: String,

    #[serde(skip)]
    entity_ix: HashMap<String, usize>,
    #[serde(skip)]
    dim_ix: HashMap<String, usize>,
    #[serde(skip)]
    metric_ix: HashMap<String, usize>,
    #[serde(skip)]
    metric_syn: HashMap<String, String>,
    #[serde(skip)]
    dim_syn: HashMap<String, String>,
}

impl Model {
    /// Parses a model from YAML and indexes it.
    pub fn from_yaml(src: &str) -> Result<Self, ModelError> {
        let mut m: Model =
            serde_yaml::from_str(src).map_err(|e| ModelError::Parse(e.to_string()))?;
        m.index()?;
        Ok(m)
    }

    /// Builds lookups and validates references. Must be called after loading.
    pub fn index(&mut self) -> Result<(), ModelError> {
        self.entity_ix.clear();
        self.dim_ix.clear();
        self.metric_ix.clear();

        // The zone name is inlined into SQL by the dialect — `AT TIME ZONE 'x'`
        // takes no bind parameter on most engines — so it is validated here
        // instead. This is the only string in the model that reaches SQL without
        // going through a placeholder, and the check is what keeps that true.
        if !self.timezone.is_empty()
            && !self
                .timezone
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '/'))
        {
            return Err(ModelError::BadTimezone(self.timezone.clone()));
        }

        for (i, e) in self.entities.iter().enumerate() {
            if e.name.is_empty() || e.table.is_empty() || e.primary_key.is_empty() {
                return Err(ModelError::IncompleteEntity(e.name.clone()));
            }
            self.entity_ix.insert(e.name.clone(), i);
        }
        for (i, d) in self.dimensions.iter().enumerate() {
            if !self.entity_ix.contains_key(&d.entity) {
                return Err(ModelError::UnknownDimEntity {
                    dim: d.name.clone(),
                    entity: d.entity.clone(),
                });
            }
            if d.kind != "categorical" && d.kind != "time" {
                return Err(ModelError::BadDimType(d.name.clone()));
            }
            self.dim_ix.insert(d.name.clone(), i);
        }
        for (i, mt) in self.metrics.iter().enumerate() {
            if !mt.is_derived() && !mt.is_window() && !self.entity_ix.contains_key(&mt.entity) {
                return Err(ModelError::UnknownMetricEntity {
                    metric: mt.name.clone(),
                    entity: mt.entity.clone(),
                });
            }
            match mt.additivity.as_str() {
                "" | ADDITIVE | SEMI_ADDITIVE | NON_ADDITIVE => {}
                got => {
                    return Err(ModelError::BadAdditivity {
                        metric: mt.name.clone(),
                        got: got.to_string(),
                    });
                }
            }
            // A declared `semi_additive` used to be pure decoration: the
            // compiler consults `additivity` only on the window path, so a base
            // metric declared semi-additive was summed like any other. Sliced by
            // month, a stock level came back as the sum of every day's balance —
            // a number roughly thirty times the truth, in the right units, with
            // no error anywhere. Refuse the combination at load: it is decidable
            // from the model alone, and a defect that only appears for *some*
            // group-bys is one nobody finds by querying.
            if mt.additivity == SEMI_ADDITIVE
                && !mt.is_derived()
                && !mt.is_window()
                && matches!(mt.agg.to_lowercase().as_str(), "sum" | "count")
            {
                return Err(ModelError::SemiAdditiveSummed {
                    metric: mt.name.clone(),
                    agg: mt.agg.clone(),
                });
            }
            if !mt.reset.is_empty() {
                if !mt.is_window() {
                    return Err(ModelError::ResetWithoutWindow(mt.name.clone()));
                }
                if !valid_period(&mt.reset) {
                    return Err(ModelError::BadReset {
                        metric: mt.name.clone(),
                        got: mt.reset.clone(),
                    });
                }
            }
            self.metric_ix.insert(mt.name.clone(), i);
        }

        // Synonym index. Declared synonyms first (first declaration wins a
        // shared synonym), then canonical names last — so a name always beats
        // a synonym, and no metric can be shadowed by another's alias.
        self.metric_syn.clear();
        for mt in &self.metrics {
            for syn in &mt.synonyms {
                let k = norm_name(syn);
                if !k.is_empty() {
                    self.metric_syn.entry(k).or_insert_with(|| mt.name.clone());
                }
            }
        }
        for mt in &self.metrics {
            self.metric_syn.insert(norm_name(&mt.name), mt.name.clone());
        }
        self.dim_syn.clear();
        for d in &self.dimensions {
            for syn in &d.synonyms {
                let k = norm_name(syn);
                if !k.is_empty() {
                    self.dim_syn.entry(k).or_insert_with(|| d.name.clone());
                }
            }
        }
        for d in &self.dimensions {
            self.dim_syn.insert(norm_name(&d.name), d.name.clone());
        }

        for j in &self.joins {
            if !self.entity_ix.contains_key(&j.from) || !self.entity_ix.contains_key(&j.to) {
                return Err(ModelError::UnknownJoinEntity {
                    from: j.from.clone(),
                    to: j.to.clone(),
                });
            }
            match j.cardinality.as_str() {
                "many_to_one" | "one_to_many" | "many_to_many" => {}
                got => {
                    return Err(ModelError::BadCardinality {
                        from: j.from.clone(),
                        to: j.to.clone(),
                        got: got.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn entity(&self, name: &str) -> Option<&Entity> {
        self.entity_ix.get(name).map(|&i| &self.entities[i])
    }
    pub fn dimension(&self, name: &str) -> Option<&Dimension> {
        self.dim_ix.get(name).map(|&i| &self.dimensions[i])
    }
    pub fn metric(&self, name: &str) -> Option<&Metric> {
        self.metric_ix.get(name).map(|&i| &self.metrics[i])
    }

    /// Resolves a metric name or declared synonym to the canonical name.
    pub fn resolve_metric(&self, spoken: &str) -> Option<&str> {
        self.metric_syn.get(&norm_name(spoken)).map(String::as_str)
    }
    /// Resolves a dimension name or declared synonym to the canonical name.
    pub fn resolve_dimension(&self, spoken: &str) -> Option<&str> {
        self.dim_syn.get(&norm_name(spoken)).map(String::as_str)
    }

    /// Metric names in definition order (for `list_metrics`).
    pub fn metric_names(&self) -> Vec<&str> {
        self.metrics.iter().map(|m| m.name.as_str()).collect()
    }

    /// How a metric may be rolled up, honouring an explicit `additivity:` and
    /// otherwise inferring it:
    ///
    /// - base: sum/count/min/max → additive; count_distinct/avg → non-additive
    /// - derived: a ratio (uses `/` or `%`) → non-additive; else the least
    ///   additive of its parts
    /// - window: always non-additive — a rolling total is never re-summable
    pub fn additivity(&self, name: &str) -> &'static str {
        self.additivity_inner(name, &mut Vec::new())
    }

    fn additivity_inner(&self, name: &str, visiting: &mut Vec<String>) -> &'static str {
        let Some(mt) = self.metric(name) else {
            return ADDITIVE;
        };
        if !mt.additivity.is_empty() {
            return match mt.additivity.as_str() {
                SEMI_ADDITIVE => SEMI_ADDITIVE,
                NON_ADDITIVE => NON_ADDITIVE,
                _ => ADDITIVE,
            };
        }
        if mt.is_window() {
            return NON_ADDITIVE;
        }
        if mt.is_derived() {
            if mt.formula.contains('/') || mt.formula.contains('%') {
                return NON_ADDITIVE;
            }
            if visiting.iter().any(|v| v == name) {
                return ADDITIVE; // cycle: the compiler reports it, don't loop here
            }
            visiting.push(name.to_string());
            let mut worst = ADDITIVE;
            for tok in crate::compile::idents(&mt.formula) {
                if self.metric(&tok).is_some() {
                    worst = least_additive(worst, self.additivity_inner(&tok, visiting));
                }
            }
            visiting.pop();
            return worst;
        }
        match mt.agg.to_lowercase().as_str() {
            "count_distinct" | "avg" => NON_ADDITIVE,
            _ => ADDITIVE,
        }
    }
}

fn least_additive(a: &'static str, b: &'static str) -> &'static str {
    let rank = |s: &str| match s {
        NON_ADDITIVE => 0,
        SEMI_ADDITIVE => 1,
        _ => 2,
    };
    if rank(b) < rank(a) { b } else { a }
}
