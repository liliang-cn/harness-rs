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

// The SSRF-safe entry point: a caller-built, redirect-disabled client is accepted
// and connects through the same path. (Full SSRF/redirect/DNS-pinning behavior is
// the caller's policy; here we just prove the hardened-client API works + errors
// cleanly on an unreachable host.)
#[tokio::test]
async fn connect_http_with_hardened_client_unreachable_returns_err() {
    use harness_mcp_client::reqwest;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build reqwest client");
    let result = McpClient::connect_http_with_client("http://127.0.0.1:1/mcp", client).await;
    assert!(result.is_err(), "expected Err for unreachable server");
}
