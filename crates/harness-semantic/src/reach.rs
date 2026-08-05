//! Reachability: which dimensions can slice a metric *without* fanning it out.

use crate::model::Model;
use std::collections::{HashMap, HashSet};

/// One safe (many-to-one) traversal step.
#[derive(Clone, Debug)]
pub struct Edge {
    pub to: String,
    /// column on the "from" entity's table
    pub left_col: String,
    /// column on the "to" entity's table
    pub right_col: String,
}

impl Model {
    /// The many-to-one traversal graph. Shared by join planning and
    /// valid-dimension discovery, so the two can never disagree about what is
    /// safe to traverse.
    pub(crate) fn adjacency(&self) -> HashMap<String, Vec<Edge>> {
        let mut adj: HashMap<String, Vec<Edge>> = HashMap::new();
        for j in &self.joins {
            match j.cardinality.as_str() {
                // many side = To, one side = From → safe edge To -> From
                "one_to_many" => adj.entry(j.to.clone()).or_default().push(Edge {
                    to: j.from.clone(),
                    left_col: j.to_key.clone(),
                    right_col: j.from_key.clone(),
                }),
                // many side = From, one side = To → safe edge From -> To
                "many_to_one" => adj.entry(j.from.clone()).or_default().push(Edge {
                    to: j.to.clone(),
                    left_col: j.from_key.clone(),
                    right_col: j.to_key.clone(),
                }),
                // many_to_many is not traversable directly — it must go through
                // a declared bridge entity, or the grain multiplies.
                _ => {}
            }
        }
        adj
    }

    /// Entities reachable from `base` by many-to-one edges only — the joins
    /// that cannot multiply the base grain.
    pub fn reachable_entities(&self, base: &str) -> HashSet<String> {
        let adj = self.adjacency();
        let mut seen: HashSet<String> = HashSet::from([base.to_string()]);
        let mut queue = vec![base.to_string()];
        while let Some(cur) = queue.pop() {
            for e in adj.get(&cur).into_iter().flatten() {
                if seen.insert(e.to.clone()) {
                    queue.push(e.to.clone());
                }
            }
        }
        seen
    }

    fn base_entities_of(&self, name: &str, visiting: &mut Vec<String>) -> Option<Vec<String>> {
        let mt = self.metric(name)?;
        if !mt.is_derived() {
            return Some(vec![mt.entity.clone()]);
        }
        if visiting.iter().any(|v| v == name) {
            return Some(Vec::new());
        }
        visiting.push(name.to_string());
        let mut out = Vec::new();
        for tok in crate::compile::idents(&mt.formula) {
            if self.metric(&tok).is_none() {
                continue; // SQL function or literal
            }
            out.extend(self.base_entities_of(&tok, visiting)?);
        }
        visiting.pop();
        Some(out)
    }

    /// The dimensions that can slice `metric` without a fan-out: those whose
    /// entity is reachable from **every** base metric it depends on.
    ///
    /// This is why `net_revenue by product_category` is refused — refunds can't
    /// reach product. Powers `get_dimensions`, so an agent self-corrects instead
    /// of asking for a number that would have been quietly wrong.
    pub fn dimensions_for(&self, metric: &str) -> Option<Vec<String>> {
        let bases = self.base_entities_of(metric, &mut Vec::new())?;
        if bases.is_empty() {
            return Some(Vec::new());
        }
        let mut inter: Option<HashSet<String>> = None;
        for e in &bases {
            let r = self.reachable_entities(e);
            inter = Some(match inter {
                None => r,
                Some(prev) => prev.intersection(&r).cloned().collect(),
            });
        }
        let inter = inter.unwrap_or_default();
        Some(
            self.dimensions
                .iter()
                .filter(|d| inter.contains(&d.entity))
                .map(|d| d.name.clone())
                .collect(),
        )
    }
}
