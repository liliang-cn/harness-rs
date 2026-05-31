#![cfg(feature = "test-server")]

use harness_mcp_client::McpClient;
use serde_json::json;

#[tokio::test]
async fn connects_lists_and_calls_echo() {
    let bin = env!("CARGO_BIN_EXE_mcp-echo-server");
    let client = McpClient::connect_stdio(bin, &[]).await.unwrap();

    assert!(client.tool_names().contains(&"echo".to_string()));

    let tools = client.tools();
    let echo = tools.iter().find(|t| t.name() == "echo").unwrap();

    let mut world = harness_context::default_world(".");
    let res = echo
        .invoke(json!({ "text": "hello mcp" }), &mut world)
        .await
        .unwrap();

    assert!(res.ok);
    let body = serde_json::to_string(&res.content).unwrap();
    assert!(
        body.contains("hello mcp"),
        "result did not echo payload: {body}"
    );
}
