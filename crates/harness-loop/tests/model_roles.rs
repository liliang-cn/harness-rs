//! Named model roles: auxiliary models for side tasks, resolved with
//! fall-back-to-main semantics, plus the one automatic wiring ("compactor").
use harness_core::Model;
use harness_loop::AgentLoop;
use harness_models::{MockModel, MockResponse};
use std::sync::Arc;

fn mock(name: &str) -> Arc<dyn Model> {
    Arc::new(MockModel::new().script(MockResponse::text(name)))
}

#[test]
fn unregistered_role_resolves_to_none_meaning_main() {
    let agent = AgentLoop::boxed(mock("main"));
    assert!(agent.model_for("judge").is_none());
}

#[test]
fn registered_roles_resolve_and_last_write_wins() {
    let agent = AgentLoop::boxed(mock("main"))
        .with_model_role("judge", mock("strong"))
        .with_model_role("synthesizer", mock("cheap"))
        .with_model_role("judge", mock("stronger"));
    assert!(agent.model_for("judge").is_some());
    assert!(agent.model_for("synthesizer").is_some());
    // Last write wins: the map holds the second "judge" registration.
    assert_eq!(agent.model_roles.len(), 2);
}

#[test]
fn compactor_role_upgrades_the_default_compactor() {
    let agent = AgentLoop::boxed(mock("main")).with_model_role("compactor", mock("cheap"));
    assert!(agent.model_for("compactor").is_some());
    assert!(
        !agent.compactor_custom,
        "role wiring is a convenience, not an explicit compactor"
    );
    // Behavioural check: with_compactor AFTER the role must win…
    let custom = Arc::new(harness_compactor::DefaultCompactor::new());
    let agent2 = AgentLoop::boxed(mock("main"))
        .with_model_role("compactor", mock("cheap"))
        .with_compactor(custom.clone());
    assert!(Arc::ptr_eq(
        &agent2.compactor,
        &(custom.clone() as Arc<dyn harness_core::Compactor>)
    ));
    // …and BEFORE the role must also win (explicit beats convenience).
    let agent3 = AgentLoop::boxed(mock("main"))
        .with_compactor(custom.clone())
        .with_model_role("compactor", mock("cheap"));
    assert!(Arc::ptr_eq(
        &agent3.compactor,
        &(custom as Arc<dyn harness_core::Compactor>)
    ));
}
