//! End-to-end coverage for all five proc-macros:
//! `#[skill]` `#[tool]` `#[guide]` `#[sensor]` `#[hook]`.

use harness::prelude::*;
use harness_core::{
    Action, GuideEntry, HookEntry, SensorEntry, ToolEntry, iter_macro_guides, iter_macro_hooks,
    iter_macro_sensors, iter_macro_tools,
};
use std::collections::BTreeMap;

// inventory::submit! works through linker magic — the entries get pulled in
// because the test binary references the module they live in.

// ---------- skill ----------

/// Greet someone politely. Use for friendly user interactions.
#[harness::skill(
    name = "polite-hello",
    harness(kind = "inferential", risk = "read-only")
)]
async fn polite_hello(_ctx: &mut Context, _w: &mut World) -> Result<(), harness::SkillError> {
    Ok(())
}

// ---------- tool ----------

/// Reverse a string.
#[harness::tool(
    name = "reverse",
    risk = "read-only",
    schema = r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#
)]
async fn reverse(
    args: serde_json::Value,
    _w: &mut World,
) -> Result<harness::ToolResult, harness::ToolError> {
    let t = args["text"].as_str().unwrap_or("").to_string();
    let rev: String = t.chars().rev().collect();
    Ok(harness::ToolResult {
        ok: true,
        content: serde_json::json!({"reversed": rev}),
        trace: None,
    })
}

// ---------- guide ----------

/// Inject a one-liner about the project.
#[harness::guide(id = "project-intro", scope = "always", kind = "inferential")]
async fn project_intro(ctx: &mut Context, _w: &harness::World) -> Result<(), harness::GuideError> {
    ctx.guides
        .push(harness::Block::Text("harness framework is loaded".into()));
    Ok(())
}

// ---------- sensor ----------

#[harness::sensor(id = "noop-sensor", stage = "self-correct", kind = "computational")]
async fn noop_sensor(
    _action: &Action,
    _w: &harness::World,
) -> Result<Vec<harness::Signal>, harness::SensorError> {
    Ok(Vec::new())
}

// ---------- hook ----------

#[harness::hook(name = "stop-watcher", event = "Stop")]
fn stop_watcher(_ev: &harness::Event<'_>, _w: &mut harness::World) -> harness::HookOutcome {
    harness::HookOutcome::Allow
}

#[test]
fn all_macros_register_via_inventory() {
    let tool_names: Vec<_> = iter_macro_tools().map(|t| t.name().to_string()).collect();
    let guide_ids: Vec<_> = iter_macro_guides().map(|g| g.id().clone()).collect();
    let sensor_ids: Vec<_> = iter_macro_sensors().map(|s| s.id().clone()).collect();
    let hook_names: Vec<_> = iter_macro_hooks().map(|h| h.name().to_string()).collect();

    assert!(
        tool_names.contains(&"reverse".to_string()),
        "tool registered: {tool_names:?}"
    );
    assert!(
        guide_ids.contains(&"project-intro".to_string()),
        "guide registered: {guide_ids:?}"
    );
    assert!(
        sensor_ids.contains(&"noop-sensor".to_string()),
        "sensor registered: {sensor_ids:?}"
    );
    assert!(
        hook_names.contains(&"stop-watcher".to_string()),
        "hook registered: {hook_names:?}"
    );

    // proof that all entries also implement std-extension count
    assert!(inventory::iter::<ToolEntry>().count() >= 1);
    assert!(inventory::iter::<GuideEntry>().count() >= 1);
    assert!(inventory::iter::<SensorEntry>().count() >= 1);
    assert!(inventory::iter::<HookEntry>().count() >= 1);
}

#[tokio::test]
async fn tool_macro_invocation_works() {
    let tool = iter_macro_tools()
        .find(|t| t.name() == "reverse")
        .expect("reverse tool registered");
    let mut world = harness_context::default_world(".");
    let out = tool
        .invoke(serde_json::json!({"text": "hello"}), &mut world)
        .await
        .expect("invoke succeeds");
    assert_eq!(out.content["reversed"], "olleh");
}

#[tokio::test]
async fn guide_macro_apply_works() {
    let g = iter_macro_guides()
        .find(|g| g.id() == "project-intro")
        .expect("guide registered");
    let world = harness_context::default_world(".");
    let mut ctx = Context::new(harness::Task {
        description: "anything".into(),
        source: None,
        deadline: None,
    });
    // metadata default + ensure compile
    let _ = BTreeMap::<String, serde_json::Value>::new();
    g.apply(&mut ctx, &world).await.expect("guide applies");
    let injected = ctx
        .guides
        .iter()
        .any(|b| matches!(b, harness::Block::Text(t) if t.contains("harness framework")));
    assert!(injected);
}

#[test]
fn skill_macro_still_works() {
    let s = harness::skills::SkillRegistry::new()
        .with_macro_skills()
        .expect("registers");
    let _ = s.get("polite-hello").expect("polite-hello registered");
}
