//! What `#[tool]` actually expands to, compiled.
//!
//! The macro is the front door — the README leads with it — but nothing in the
//! workspace exercised it, so its documented shape drifted from the generated
//! one. These tests are the executable version of that documentation: the
//! annotated function takes `(args, world)` and returns a `ToolResult`, and the
//! tool reaches the loop through `inventory`, not through a constructor named
//! after the function.

use harness_core::{ToolError, ToolResult, World, iter_macro_tools};
use harness_rs_macros::tool;
use serde_json::json;

/// Add two integers.
#[tool(name = "add", risk = "read-only")]
async fn add(args: serde_json::Value, _world: &mut World) -> Result<ToolResult, ToolError> {
    let a = args["a"].as_i64().unwrap_or(0);
    let b = args["b"].as_i64().unwrap_or(0);
    Ok(ToolResult {
        ok: true,
        content: json!({ "sum": a + b }),
        trace: None,
    })
}

/// Delete something. Declares a risk other than the default.
#[tool(name = "wipe", risk = "destructive")]
async fn wipe(_args: serde_json::Value, _world: &mut World) -> Result<ToolResult, ToolError> {
    Ok(ToolResult {
        ok: true,
        content: json!({}),
        trace: None,
    })
}

fn find(name: &str) -> std::sync::Arc<dyn harness_core::Tool> {
    iter_macro_tools()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("`{name}` did not register via inventory"))
}

#[test]
fn annotated_fns_register_themselves() {
    let add = find("add");
    assert_eq!(add.name(), "add");
    // The doc-comment becomes the description the model sees.
    assert_eq!(add.schema().description, "Add two integers.");
    assert_eq!(add.risk(), harness_core::ToolRisk::ReadOnly);

    assert_eq!(find("wipe").risk(), harness_core::ToolRisk::Destructive);
}

#[tokio::test]
async fn the_registered_tool_runs() {
    let ws = std::env::temp_dir().join(format!("tool-macro-{}", std::process::id()));
    std::fs::create_dir_all(&ws).expect("workspace");
    let mut world = harness_context::default_world(&ws);

    let out = find("add")
        .invoke(json!({"a": 2, "b": 3}), &mut world)
        .await
        .expect("invoke");

    let _ = std::fs::remove_dir_all(&ws);
    assert!(out.ok);
    assert_eq!(out.content["sum"], 5);
}
