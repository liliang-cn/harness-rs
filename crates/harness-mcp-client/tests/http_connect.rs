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

/// Regression: the HTTP transport must be able to speak `https`.
///
/// Without a TLS backend, reqwest rejects an https URL *at the connector* with
/// `invalid URL, scheme is not http` — before opening a socket. Every remote MCP
/// server is https, so the transport was unusable in exactly the case it exists
/// for, and the error named the scheme rather than the missing feature, which
/// sent debugging in the wrong direction.
///
/// Port 1 on loopback refuses instantly, so this needs no network: reaching a
/// *connect* failure proves TLS was configured, since a missing backend would
/// have failed earlier with the scheme error.
#[tokio::test]
async fn https_urls_are_supported_by_the_http_transport() {
    use harness_mcp_client::reqwest;
    let err = reqwest::Client::new()
        .get("https://127.0.0.1:1/mcp")
        .send()
        .await
        .expect_err("nothing listens on port 1");

    let detail = format!("{err} | {err:?}");
    assert!(
        !detail.contains("scheme is not http"),
        "no TLS backend compiled in — enable the `tls-rustls` (or `tls-native`) feature: {detail}"
    );
    assert!(
        err.is_connect(),
        "expected a connection failure, got: {detail}"
    );
}

/// End-to-end proof against a real public MCP server, over https.
///
/// `#[ignore]` because it depends on a third party being up — run it by hand
/// (`cargo test -p harness-rs-mcp-client --test http_connect -- --ignored`)
/// when touching the transport or its TLS wiring.
#[tokio::test]
#[ignore = "network: talks to a public MCP server"]
async fn connects_to_a_real_https_mcp_server() {
    let client = McpClient::connect_http("https://weather.datakoot.com/mcp")
        .await
        .expect("connect to a public https MCP server");
    let tools = client.tools();
    assert!(!tools.is_empty(), "expected the server to advertise tools");
}
