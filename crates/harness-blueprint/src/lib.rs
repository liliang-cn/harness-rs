//! Blueprint — deterministic + agent hybrid state machine (Stripe Minions pattern).
//!
//! Each node is either:
//! - **Deterministic** — runs an `async FnMut(&mut World) -> Result<NodeOutput>`
//! - **Agent** — placeholder; in v0.1 we accept an opaque async closure so users
//!   can plug in `AgentLoop` themselves (see `examples/`). A first-class
//!   `Node::Agent { AgentLoop }` is sketched in DESIGN.md §8 and lands once
//!   we resolve the variance/lifetime story for `&mut AgentLoop` in a `Box`.
//!
//! Control flow:
//! - Edges are followed by `NodeOutput.transition`.
//! - Failure of a node can branch to a retry/fallback node when `branch_on_failure`
//!   is set; up to `retry_cap` retries.

use harness_core::{HarnessError, World};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

pub type NodeId = String;

/// What a node returns. `transition` decides what edge to follow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeOutput {
    pub transition: Transition,
    #[serde(default)]
    pub data:       serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transition {
    /// Follow the named edge.
    Edge(String),
    /// Use the default ("next") edge if any.
    Next,
    /// Mark the run complete; ignore any downstream edges.
    Done,
    /// Abort the blueprint with this reason.
    Abort(String),
}

/// One executable node.
pub enum Node {
    /// Deterministic Rust closure. Receives `&mut World`. Cheap; no tokens.
    Deterministic(BoxedDetermFn),
    /// User-supplied async closure that knows how to run an agent loop.
    /// Receives `&mut World`. This is the v0.1 escape hatch.
    Agent(BoxedAgentFn),
}

type BoxedDetermFn = Box<
    dyn for<'a> Fn(
            &'a mut World,
        ) -> Pin<Box<dyn Future<Output = Result<NodeOutput, HarnessError>> + Send + 'a>>
        + Send
        + Sync,
>;

type BoxedAgentFn = Box<
    dyn for<'a> Fn(
            &'a mut World,
        ) -> Pin<Box<dyn Future<Output = Result<NodeOutput, HarnessError>> + Send + 'a>>
        + Send
        + Sync,
>;

impl Node {
    pub fn deterministic<F, Fut>(f: F) -> Self
    where
        F: for<'a> Fn(&'a mut World) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<NodeOutput, HarnessError>> + Send + 'static,
    {
        Node::Deterministic(Box::new(move |w| Box::pin(f(w))))
    }

    pub fn agent<F, Fut>(f: F) -> Self
    where
        F: for<'a> Fn(&'a mut World) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<NodeOutput, HarnessError>> + Send + 'static,
    {
        Node::Agent(Box::new(move |w| Box::pin(f(w))))
    }
}

/// An edge — either named or default.
#[derive(Debug, Clone)]
struct EdgeDef {
    /// `None` means the default "next" edge.
    name: Option<String>,
    target: NodeId,
}

/// Per-node failure policy.
#[derive(Debug, Clone, Default)]
struct FailurePolicy {
    fallback:  Option<NodeId>,
    retry_cap: u32,
}

pub struct Blueprint {
    nodes:    HashMap<NodeId, Node>,
    edges:    HashMap<NodeId, Vec<EdgeDef>>,
    failure:  HashMap<NodeId, FailurePolicy>,
    start:    Option<NodeId>,
}

impl Default for Blueprint {
    fn default() -> Self { Self::new() }
}

impl Blueprint {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: HashMap::new(),
            failure: HashMap::new(),
            start: None,
        }
    }

    /// Add a node. The first node added becomes the start (overridable via `start()`).
    pub fn add(mut self, id: impl Into<NodeId>, node: Node) -> Self {
        let id = id.into();
        if self.start.is_none() {
            self.start = Some(id.clone());
        }
        self.nodes.insert(id, node);
        self
    }

    pub fn start(mut self, id: impl Into<NodeId>) -> Self {
        self.start = Some(id.into());
        self
    }

    /// Default ("next") edge from `from` → `to`.
    pub fn edge(mut self, from: impl Into<NodeId>, to: impl Into<NodeId>) -> Self {
        self.edges.entry(from.into()).or_default().push(EdgeDef {
            name:   None,
            target: to.into(),
        });
        self
    }

    /// Named edge from `from` →(name)→ `to`. Matches `Transition::Edge("name")`.
    pub fn edge_named(
        mut self,
        from: impl Into<NodeId>,
        name: impl Into<String>,
        to: impl Into<NodeId>,
    ) -> Self {
        self.edges.entry(from.into()).or_default().push(EdgeDef {
            name:   Some(name.into()),
            target: to.into(),
        });
        self
    }

    /// Fallback node + retry cap when `from` fails.
    pub fn branch_on_failure(
        mut self,
        from: impl Into<NodeId>,
        fallback: impl Into<NodeId>,
        retry_cap: u32,
    ) -> Self {
        self.failure.insert(
            from.into(),
            FailurePolicy { fallback: Some(fallback.into()), retry_cap },
        );
        self
    }

    /// Execute the blueprint.
    pub async fn run(&self, world: &mut World) -> Result<BlueprintOutcome, HarnessError> {
        let mut current = self
            .start
            .clone()
            .ok_or_else(|| HarnessError::Other("blueprint has no start node".into()))?;
        let mut visited: Vec<NodeId> = Vec::new();
        let mut retries: HashMap<NodeId, u32> = HashMap::new();

        loop {
            tracing::debug!(node = %current, "blueprint enter");
            let node = self.nodes.get(&current).ok_or_else(|| {
                HarnessError::Other(format!("blueprint: node `{current}` not found"))
            })?;
            let result = match node {
                Node::Deterministic(f) => f(world).await,
                Node::Agent(f)         => f(world).await,
            };
            visited.push(current.clone());
            let out = match result {
                Ok(o) => o,
                Err(e) => {
                    if let Some(pol) = self.failure.get(&current).cloned()
                        && let Some(fallback) = pol.fallback.clone()
                    {
                        let count = retries.entry(current.clone()).or_insert(0);
                        if *count < pol.retry_cap {
                            *count += 1;
                            tracing::warn!(node = %current, retry = *count, "node failed; rerunning");
                            continue;
                        }
                        tracing::warn!(node = %current, fallback = %fallback, "retries exhausted, branching");
                        current = fallback;
                        continue;
                    }
                    return Err(e);
                }
            };
            match out.transition {
                Transition::Done           => return Ok(BlueprintOutcome { visited, last: out.data }),
                Transition::Abort(reason)  => return Err(HarnessError::Policy(reason)),
                Transition::Edge(name) => {
                    let next = self
                        .edges
                        .get(&current)
                        .and_then(|es| es.iter().find(|e| e.name.as_deref() == Some(&name)))
                        .map(|e| e.target.clone())
                        .ok_or_else(|| {
                            HarnessError::Other(format!(
                                "blueprint: node `{current}` has no edge named `{name}`"
                            ))
                        })?;
                    current = next;
                }
                Transition::Next => {
                    let next = self
                        .edges
                        .get(&current)
                        .and_then(|es| es.first())
                        .map(|e| e.target.clone());
                    match next {
                        Some(t) => current = t,
                        None    => return Ok(BlueprintOutcome { visited, last: out.data }),
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct BlueprintOutcome {
    pub visited: Vec<NodeId>,
    pub last:    serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mk_world() -> World { harness_context::default_world(".") }

    #[tokio::test]
    async fn linear_chain_runs_in_order() {
        let bp = Blueprint::new()
            .add("a", Node::deterministic(|_w| async move {
                Ok(NodeOutput { transition: Transition::Next, data: json!({"step":"a"}) })
            }))
            .add("b", Node::deterministic(|_w| async move {
                Ok(NodeOutput { transition: Transition::Next, data: json!({"step":"b"}) })
            }))
            .add("c", Node::deterministic(|_w| async move {
                Ok(NodeOutput { transition: Transition::Done, data: json!({"step":"c"}) })
            }))
            .edge("a", "b")
            .edge("b", "c");
        let mut w = mk_world();
        let out = bp.run(&mut w).await.unwrap();
        assert_eq!(out.visited, vec!["a", "b", "c"]);
        assert_eq!(out.last["step"], "c");
    }

    #[tokio::test]
    async fn branch_on_failure_recovers() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts2 = attempts.clone();
        let bp = Blueprint::new()
            .add("flaky", Node::deterministic(move |_w| {
                let attempts = attempts2.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n < 1 {
                        Err(HarnessError::Other("transient".into()))
                    } else {
                        Ok(NodeOutput { transition: Transition::Done, data: json!({}) })
                    }
                }
            }))
            .add("fallback", Node::deterministic(|_w| async move {
                Ok(NodeOutput { transition: Transition::Done, data: json!({"recovered": true}) })
            }))
            .branch_on_failure("flaky", "fallback", 2);
        let mut w = mk_world();
        let out = bp.run(&mut w).await.unwrap();
        // First attempt failed, second succeeded — we should have visited "flaky" twice.
        assert_eq!(out.visited.iter().filter(|n| n.as_str() == "flaky").count(), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn named_edges_route_via_transition() {
        let bp = Blueprint::new()
            .add("router", Node::deterministic(|_w| async move {
                Ok(NodeOutput {
                    transition: Transition::Edge("left".into()),
                    data: json!({}),
                })
            }))
            .add("left",  Node::deterministic(|_w| async move {
                Ok(NodeOutput { transition: Transition::Done, data: json!({"branch":"left"}) })
            }))
            .add("right", Node::deterministic(|_w| async move {
                Ok(NodeOutput { transition: Transition::Done, data: json!({"branch":"right"}) })
            }))
            .edge_named("router", "left",  "left")
            .edge_named("router", "right", "right");
        let mut w = mk_world();
        let out = bp.run(&mut w).await.unwrap();
        assert_eq!(out.visited, vec!["router", "left"]);
    }
}
