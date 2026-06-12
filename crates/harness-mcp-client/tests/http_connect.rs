#![cfg(feature = "http")]

use harness_mcp_client::McpClient;

#[tokio::test]
async fn connect_http_unreachable_returns_err() {
    let url = "http://127.0.0.1:1/mcp";
    let result = McpClient::connect_http(url).await;
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected Err for unreachable server, got Ok"),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("127.0.0.1") || msg.contains("mcp"),
        "error message should mention url or context, got: {msg}"
    );
}
