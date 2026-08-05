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
    BadCardinality { from: String, to: String, got: String },
    #[error("{0}")]
    Parse(String),
}

/// A real business thing with a primary key the layer joins on — **declared,
/// never guessed**.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
}

/// An aggregated number with grain and aggregation locked in.
///
/// A *base* metric aggregates `expr` over its `entity`. A *derived* metric is a
/// formula over other metric names — which is how chasm traps are avoided: each
/// base metric aggregates in its own CTE before anything is combined.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
