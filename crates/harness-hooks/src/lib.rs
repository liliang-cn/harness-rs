//! `HookBus` — dispatch the 27 lifecycle events to registered [`Hook`]s.
//!
//! Per DESIGN.md §10, a hook is a *cheap* synchronous policy that decides what
//! happens around tool calls, model calls, compaction, sensor runs, and so on.
//! Long async work belongs in a sensor or tool, not a hook.

#[cfg(feature = "otel")]
pub mod otel;

#[cfg(feature = "otel")]
pub use otel::OtelHook;

use harness_core::{Event, Hook, HookOutcome, World, iter_macro_hooks};
use std::sync::Arc;

/// Ordered list of hooks; fires events through every match.
#[derive(Default)]
pub struct HookBus {
    hooks: Vec<Arc<dyn Hook>>,
}

impl HookBus {
    pub fn new() -> Self { Self::default() }

    /// Pull in every `#[hook]`-registered hook.
    pub fn with_macro_hooks(mut self) -> Self {
        for h in iter_macro_hooks() {
            self.hooks.push(h);
        }
        self
    }

    /// Consume + return (used by `AgentLoop::with_macro_hooks`).
    pub fn with_macro_hooks_take(self) -> Self {
        self.with_macro_hooks()
    }

    pub fn register(&mut self, h: Arc<dyn Hook>) {
        self.hooks.push(h);
    }

    pub fn len(&self) -> usize { self.hooks.len() }
    pub fn is_empty(&self) -> bool { self.hooks.is_empty() }

    /// Fire `ev` through all matching hooks in registration order.
    ///
    /// Aggregation:
    /// - First `Deny` short-circuits, return the `Deny`.
    /// - All `Inject` payloads concatenate; if any present and no Deny, return `Inject(joined)`.
    /// - Otherwise `Allow`.
    pub fn fire(&self, ev: &Event<'_>, world: &mut World) -> HookOutcome {
        let mut injects = Vec::<String>::new();
        for h in self.hooks.iter().filter(|h| h.matches(ev)) {
            match h.fire(ev, world) {
                HookOutcome::Deny { reason } => {
                    tracing::warn!(hook = h.name(), event = ev.name(), %reason, "hook denied");
                    return HookOutcome::Deny { reason };
                }
                HookOutcome::Inject(s) => injects.push(s),
                HookOutcome::Mutate(_) => {
                    // Mutation semantics are event-specific and not yet honoured by the runtime.
                    tracing::debug!(hook = h.name(), event = ev.name(), "hook mutation ignored (not yet wired)");
                }
                HookOutcome::Allow => {}
                // HookOutcome is `#[non_exhaustive]`; treat any future variant as Allow.
                _ => {
                    tracing::warn!(hook = h.name(), "unrecognised HookOutcome variant — treating as Allow");
                }
            }
        }
        if injects.is_empty() {
            HookOutcome::Allow
        } else {
            HookOutcome::Inject(injects.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Event, World};

    struct AlwaysDeny;
    impl Hook for AlwaysDeny {
        fn name(&self) -> &str { "always-deny" }
        fn matches(&self, _: &Event<'_>) -> bool { true }
        fn fire(&self, _: &Event<'_>, _: &mut World) -> HookOutcome {
            HookOutcome::Deny { reason: "nope".into() }
        }
    }

    struct Counter(std::sync::atomic::AtomicU32);
    impl Hook for Counter {
        fn name(&self) -> &str { "counter" }
        fn matches(&self, _: &Event<'_>) -> bool { true }
        fn fire(&self, _: &Event<'_>, _: &mut World) -> HookOutcome {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            HookOutcome::Allow
        }
    }

    fn mock_world() -> World {
        use harness_core::{RepoView, Clock, ProcessRunner, ProcessOutput, KvStore};
        use std::path::Path;

        struct NoopClock;
        impl Clock for NoopClock {
            fn now_ms(&self) -> i64 { 0 }
        }
        struct NoopRunner;
        #[async_trait::async_trait]
        impl ProcessRunner for NoopRunner {
            async fn exec(&self, _: &str, _: &[&str], _: Option<&Path>) -> std::io::Result<ProcessOutput> {
                Ok(ProcessOutput { status: 0, stdout: String::new(), stderr: String::new() })
            }
        }
        struct NoopKv;
        #[async_trait::async_trait]
        impl KvStore for NoopKv {
            async fn get(&self, _: &str) -> Option<Vec<u8>> { None }
            async fn set(&self, _: &str, _: Vec<u8>) {}
            async fn delete(&self, _: &str) {}
        }

        World {
            repo:    RepoView { root: ".".into() },
            runner:  Arc::new(NoopRunner),
            clock:   Arc::new(NoopClock),
            kv:      Arc::new(NoopKv),
            profile: harness_core::UserProfile::default(),
        }
    }

    #[test]
    fn deny_short_circuits() {
        let counter = Arc::new(Counter(0.into()));
        let mut bus = HookBus::new();
        bus.register(Arc::new(AlwaysDeny));
        bus.register(counter.clone());
        let mut world = mock_world();
        let outcome = bus.fire(&Event::Stop, &mut world);
        assert!(matches!(outcome, HookOutcome::Deny { .. }));
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn all_match_fire_in_order() {
        let counter = Arc::new(Counter(0.into()));
        let mut bus = HookBus::new();
        bus.register(counter.clone());
        bus.register(counter.clone());
        bus.register(counter.clone());
        let mut world = mock_world();
        let outcome = bus.fire(&Event::Stop, &mut world);
        assert!(matches!(outcome, HookOutcome::Allow));
        assert_eq!(counter.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
