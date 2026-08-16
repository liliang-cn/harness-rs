//! End-to-end: a real Chrome, driven over a real CDP socket, against a real
//! page served over a real TCP connection.
//!
//! ## Why this is gated
//!
//! CI does not have a browser, and a test that needs one is a test that gets
//! deleted the first time it goes red on a machine without Chrome. So every
//! test here returns early — passing, loudly, with a printed reason — unless
//! both of:
//!
//! - the `real-browser` feature is on, or `HARNESS_BROWSER_TESTS=1` is set; and
//! - [`find_browser`] actually finds something.
//!
//! `cargo test -p harness-rs-tools-browser` on a laptop with Chrome installed
//! therefore still runs only the unit tests, and
//! `HARNESS_BROWSER_TESTS=1 cargo test -p harness-rs-tools-browser` runs these.
//!
//! ## Why the fixture server is a hand-rolled TcpListener
//!
//! It needs to be reachable, on a port nobody else has, serving markup this
//! test controls, with no dependency. Forty lines of `tokio::net` is all that
//! takes, and it makes the security assertion below meaningful: the page is on
//! `127.0.0.1`, which the default policy refuses, so the test proves both that
//! the guard fires *and* that it can be deliberately opened for exactly one
//! host.

use harness_core::{Tool, World};
use harness_tools_browser::{
    AllowAllPolicy, BrowserTool, LaunchConfig, PublicOnlyPolicy, UrlPolicy, find_browser,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Skip unless explicitly asked for *and* actually possible.
fn enabled() -> bool {
    let asked = cfg!(feature = "real-browser")
        || std::env::var("HARNESS_BROWSER_TESTS").is_ok_and(|v| v == "1");
    if !asked {
        eprintln!(
            "skipping: set HARNESS_BROWSER_TESTS=1 (or --features real-browser) to run the \
             browser end-to-end tests"
        );
        return false;
    }
    if find_browser().is_none() {
        eprintln!("skipping: no Chrome/Chromium/Brave/Edge found and BROWSER_BIN is unset");
        return false;
    }
    true
}

const FIXTURE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Harness Browser Fixture</title></head>
<body>
  <h1>Fixture</h1>
  <p id="greeting">Hello from the fixture page.</p>
  <form id="f" onsubmit="return false;">
    <label for="q">Search</label>
    <input id="q" name="q" type="text" placeholder="type here">
    <select id="colour">
      <option value="r">Red</option>
      <option value="g">Green</option>
      <option value="b">Blue</option>
    </select>
    <button id="go" type="button">登录</button>
  </form>
  <div id="result"></div>
  <div style="height: 3000px"></div>
  <a id="deep" href="/second.html">Go to the second page</a>
  <script>
    document.getElementById('go').addEventListener('click', function () {
      var v = document.getElementById('q').value;
      var c = document.getElementById('colour').value;
      // Deliberately delayed, so `wait_for` has something to wait for.
      setTimeout(function () {
        document.getElementById('result').textContent = 'submitted:' + v + ':' + c;
      }, 400);
    });
  </script>
</body></html>"#;

const SECOND: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Second</title></head>
<body><h1>Second page</h1><p>You navigated here.</p></body></html>"#;

/// A one-file static server on a random high port. Returns the port; the task
/// lives for the rest of the test process.
async fn serve_fixture() -> u16 {
    // Port 0: the kernel picks a free one. Hard-coding anything invites a
    // collision with whatever else the developer is running.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                let body = if path.starts_with("/second") {
                    SECOND
                } else {
                    FIXTURE
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            });
        }
    });
    port
}

fn world() -> World {
    harness_context::default_world(std::env::temp_dir())
}

async fn call(tool: &BrowserTool, w: &mut World, args: Value) -> (bool, Value) {
    let res = tool
        .invoke(args, w)
        .await
        .expect("tool must not hard-error");
    (res.ok, res.content)
}

/// The whole flow: open, inspect, type, select, click, wait, screenshot, back.
#[tokio::test(flavor = "multi_thread")]
async fn drives_a_real_page_end_to_end() {
    if !enabled() {
        return;
    }
    let port = serve_fixture().await;
    let base = format!("http://127.0.0.1:{port}");

    // The policy hole is per-host and explicit — see the dedicated test below
    // for the proof that it is needed and that it is narrow.
    let tool = BrowserTool::new()
        .with_policy(PublicOnlyPolicy::literal_only().allow_host("127.0.0.1"))
        .with_idle_ttl(Duration::from_secs(120));
    let mut w = world();

    // ── open ─────────────────────────────────────────────────────────
    let (ok, body) = call(&tool, &mut w, json!({"action": "open", "url": base})).await;
    assert!(ok, "open failed: {body}");
    assert_eq!(body["title"], "Harness Browser Fixture");
    let elements = body["elements"].as_array().expect("an element list");
    assert!(
        !elements.is_empty(),
        "no interactive elements found: {body}"
    );

    // The accessibility summary must actually describe the page: an input, a
    // select and a button with its Chinese label.
    let roles: Vec<&str> = elements.iter().filter_map(|e| e["role"].as_str()).collect();
    assert!(roles.contains(&"textbox"), "no textbox in {elements:?}");
    assert!(roles.contains(&"select"), "no select in {elements:?}");
    assert!(
        elements.iter().any(|e| e["label"] == "登录"),
        "the button's visible label is missing from {elements:?}"
    );
    // Every element must carry a handle the model can pass straight back.
    for e in elements {
        let h = e["handle"].as_str().expect("handle");
        assert!(
            h.starts_with('e') && h[1..].parse::<usize>().is_ok(),
            "bad handle {h}"
        );
    }
    assert!(
        body["handles_note"]
            .as_str()
            .unwrap()
            .contains("invalidated")
    );

    // ── read ─────────────────────────────────────────────────────────
    let (ok, body) = call(&tool, &mut w, json!({"action": "read"})).await;
    assert!(ok, "read failed: {body}");
    let text = body["text"].as_str().unwrap();
    assert!(text.contains("Hello from the fixture page."), "got: {text}");
    // innerText, not a DOM dump: the <script> body must not be in there.
    assert!(
        !text.contains("addEventListener"),
        "script leaked into the text: {text}"
    );

    // ── type, by CSS ─────────────────────────────────────────────────
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "type", "target": "#q", "text": "hello 世界"}),
    )
    .await;
    assert!(ok, "type failed: {body}");
    // The value must have reached the DOM — including the non-ASCII half,
    // which a keycode-per-character implementation could not have entered.
    let typed = body["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["role"] == "textbox")
        .and_then(|e| e["value"].as_str())
        .unwrap_or_default()
        .to_string();
    assert_eq!(typed, "hello 世界");

    // ── select, by visible option text ───────────────────────────────
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "select", "target": "#colour", "value": "Blue"}),
    )
    .await;
    assert!(ok, "select failed: {body}");
    assert!(body["did"].as_str().unwrap().contains("Blue"), "{body}");

    // ── click, by visible text, in Chinese ───────────────────────────
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "click", "target": "the button that says 登录"}),
    )
    .await;
    assert!(ok, "click failed: {body}");
    assert!(
        body["resolved"]
            .as_str()
            .unwrap_or_default()
            .contains("登录"),
        "the text→element resolution should be reported: {body}"
    );

    // ── wait_for the delayed DOM update ──────────────────────────────
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "wait_for", "target": "submitted:hello 世界:b", "timeout_ms": 5000}),
    )
    .await;
    assert!(ok, "wait_for failed: {body}");

    // ── scroll ───────────────────────────────────────────────────────
    let (ok, before) = call(
        &tool,
        &mut w,
        json!({"action": "scroll", "direction": "bottom"}),
    )
    .await;
    assert!(ok, "scroll failed: {before}");
    assert_eq!(before["scroll"]["at_bottom"], true, "{before}");
    let (ok, after) = call(
        &tool,
        &mut w,
        json!({"action": "scroll", "direction": "top"}),
    )
    .await;
    assert!(ok);
    assert_eq!(after["scroll"]["percent"], 0, "{after}");

    // ── screenshot ───────────────────────────────────────────────────
    let (ok, body) = call(&tool, &mut w, json!({"action": "screenshot"})).await;
    assert!(ok, "screenshot failed: {body}");
    let file = w.repo.root.join(body["saved_file"].as_str().unwrap());
    let png = std::fs::read(&file).expect("the screenshot must exist on disk");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");
    let _ = std::fs::remove_file(&file);

    // ── navigate by clicking a link, then back ───────────────────────
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "click", "target": "Go to the second page"}),
    )
    .await;
    assert!(ok, "link click failed: {body}");
    assert!(body["url"].as_str().unwrap().contains("second"), "{body}");

    let (ok, body) = call(&tool, &mut w, json!({"action": "back"})).await;
    assert!(ok, "back failed: {body}");
    assert_eq!(body["title"], "Harness Browser Fixture");

    let (ok, body) = call(&tool, &mut w, json!({"action": "current_url"})).await;
    assert!(ok);
    assert!(body["url"].as_str().unwrap().starts_with(&base));
    // current_url is the cheap probe: no element list.
    assert!(body.get("elements").is_none(), "{body}");

    // ── close ────────────────────────────────────────────────────────
    let (ok, body) = call(&tool, &mut w, json!({"action": "close"})).await;
    assert!(ok, "close failed: {body}");
}

/// The default policy must refuse the very page the test above reaches, and the
/// override must be the only reason it can be reached.
///
/// This is the test that keeps the escape hatch honest: if `allow_host` ever
/// widened into "allow loopback", the first assertion here would still pass and
/// the second would not.
#[tokio::test(flavor = "multi_thread")]
async fn the_url_policy_is_what_stands_between_the_agent_and_localhost() {
    if !enabled() {
        return;
    }
    let port = serve_fixture().await;
    let base = format!("http://127.0.0.1:{port}");

    // Default policy: refused, with a reason, and without a browser ever being
    // pointed at it.
    let root = scratch_root("policy");
    let tool = BrowserTool::new().with_launch_config(rooted(&root));
    let mut w = world();
    let (ok, body) = call(&tool, &mut w, json!({"action": "open", "url": &base})).await;
    assert!(
        !ok,
        "the default policy let the agent onto loopback: {body}"
    );
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("loopback"), "unhelpful refusal: {err}");
    // A refused URL must not have cost a browser launch — otherwise a model
    // looping on a denied host is a memory-exhaustion attack on its own host.
    assert_eq!(
        profile_dirs_in(&root),
        0,
        "a denied navigation started a browser anyway"
    );
    let _ = std::fs::remove_dir_all(&root);

    // The cloud metadata endpoint, the reason any of this exists.
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "open", "url": "http://169.254.169.254/latest/meta-data/"}),
    )
    .await;
    assert!(!ok, "metadata endpoint was reachable: {body}");
    assert!(body["error"].as_str().unwrap().contains("link-local"));

    // Narrowly overridden: this host, and only this host.
    let tool =
        BrowserTool::new().with_policy(PublicOnlyPolicy::literal_only().allow_host("127.0.0.1"));
    let mut w = world();
    let (ok, body) = call(&tool, &mut w, json!({"action": "open", "url": &base})).await;
    assert!(ok, "the override did not take effect: {body}");
    assert_eq!(body["title"], "Harness Browser Fixture");

    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "open", "url": "http://169.254.169.254/"}),
    )
    .await;
    assert!(
        !ok,
        "the per-host hole widened to all of loopback/link-local: {body}"
    );

    let (_, _) = call(&tool, &mut w, json!({"action": "close"})).await;
}

/// A closure is a policy, and it is consulted on the way in.
#[tokio::test(flavor = "multi_thread")]
async fn a_closure_policy_can_lock_the_agent_to_one_prefix() {
    if !enabled() {
        return;
    }
    let port = serve_fixture().await;
    let allowed = format!("http://127.0.0.1:{port}/");
    let only = allowed.clone();
    let tool = BrowserTool::new().with_policy(move |url: &str| {
        if url.starts_with(&only) {
            Ok(())
        } else {
            Err(format!("this agent may only browse {only}"))
        }
    });
    let mut w = world();

    let (ok, body) = call(&tool, &mut w, json!({"action": "open", "url": &allowed})).await;
    assert!(ok, "{body}");
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "open", "url": "https://example.com/"}),
    )
    .await;
    assert!(!ok);
    assert!(body["error"].as_str().unwrap().contains("may only browse"));

    let (_, _) = call(&tool, &mut w, json!({"action": "close"})).await;
}

/// A page with far more interactive elements than the budget allows must come
/// back bounded, honest about it, and still usable.
#[tokio::test(flavor = "multi_thread")]
async fn a_three_hundred_element_page_does_not_blow_the_budget() {
    if !enabled() {
        return;
    }
    // Serve the big page inline: a data: URL is refused by the policy (rightly),
    // so it gets its own little server.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut body = String::from("<!doctype html><meta charset=utf-8><title>Many</title>");
            for i in 0..300 {
                body.push_str(&format!("<p><a href=\"/x{i}\">Link number {i}</a></p>"));
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let mut scratch = [0u8; 2048];
            let _ = sock.read(&mut scratch).await;
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });

    let tool = BrowserTool::new().with_policy(AllowAllPolicy);
    let mut w = world();
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "open", "url": format!("http://127.0.0.1:{port}/")}),
    )
    .await;
    assert!(ok, "{body}");

    assert_eq!(body["element_count"], 300, "the page really does have 300");
    let listed = body["elements"].as_array().unwrap().len();
    assert_eq!(listed, harness_tools_browser::DEFAULT_MAX_ELEMENTS);
    let note = body["elements_note"]
        .as_str()
        .expect("must admit truncation");
    assert!(note.contains("240 of 300"), "{note}");

    // The cut must not be so aggressive that the response is worthless, nor so
    // generous that it defeats the point. A rough token proxy: the serialised
    // element list.
    let serialised = serde_json::to_string(&body["elements"]).unwrap().len();
    assert!(
        serialised < 12_000,
        "the element list is {serialised} bytes, which is not a budget"
    );

    // An element the budget dropped is still reachable by its visible text,
    // because the full snapshot is retained server-side.
    let (ok, body) = call(
        &tool,
        &mut w,
        json!({"action": "click", "target": "Link number 250"}),
    )
    .await;
    assert!(
        ok,
        "a budgeted-out element should still be targetable by text: {body}"
    );
    assert!(body["url"].as_str().unwrap().ends_with("/x250"), "{body}");

    let (_, _) = call(&tool, &mut w, json!({"action": "close"})).await;
}

/// Killing the browser out from under the tool must produce an explanation and
/// a working session on the next `open`, not a wall of timeouts.
#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_browser_is_reported_and_then_replaced() {
    if !enabled() {
        return;
    }
    let port = serve_fixture().await;
    let base = format!("http://127.0.0.1:{port}");

    // Tag this test's browser with a nonce on its command line, so the kill
    // below hits exactly it. Killing by the shared `harness-browser-` profile
    // prefix would also take out any sibling test running in parallel — and a
    // test that only passes with `--test-threads=1` is a test that will one day
    // fail for reasons nobody can reproduce.
    let nonce = format!("HarnessCrashTest/{}", std::process::id());
    let tool = BrowserTool::new()
        .with_policy(AllowAllPolicy)
        .with_launch_config(LaunchConfig {
            extra_args: vec![format!("--user-agent={nonce}")],
            ..Default::default()
        });
    let mut w = world();

    let (ok, _) = call(&tool, &mut w, json!({"action": "open", "url": &base})).await;
    assert!(ok);

    let killed = std::process::Command::new("pkill")
        .args(["-9", "-f", &nonce])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !killed {
        eprintln!("skipping the crash assertions: pkill matched nothing");
        return;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // A non-open action must say what happened rather than hang.
    let (ok, body) = call(&tool, &mut w, json!({"action": "read"})).await;
    assert!(!ok, "reading a dead browser should not succeed: {body}");
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("died"), "unhelpful crash message: {err}");

    // …and `open` brings it back, admitting that state was lost.
    let (ok, body) = call(&tool, &mut w, json!({"action": "open", "url": &base})).await;
    assert!(ok, "relaunch failed: {body}");
    assert_eq!(body["title"], "Harness Browser Fixture");
    let notes = body["notes"]
        .as_array()
        .expect("the restart must be reported");
    assert!(
        notes
            .iter()
            .any(|n| n.as_str().unwrap_or("").contains("restarted")),
        "{notes:?}"
    );

    let (_, _) = call(&tool, &mut w, json!({"action": "close"})).await;
}

/// Closing must leave nothing behind: no process, no temp profile.
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_left_on_disk_or_in_the_process_table() {
    if !enabled() {
        return;
    }
    // Its own profile root, so the count below is about this test's browsers
    // and cannot be perturbed by a sibling test running in parallel.
    let root = scratch_root("leak");

    // Orderly path: open, then close.
    {
        let tool = BrowserTool::new()
            .with_policy(AllowAllPolicy)
            .with_launch_config(rooted(&root));
        let mut w = world();
        let port = serve_fixture().await;
        let (ok, _) = call(
            &tool,
            &mut w,
            json!({"action": "open", "url": format!("http://127.0.0.1:{port}")}),
        )
        .await;
        assert!(ok);
        assert_eq!(
            profile_dirs_in(&root),
            1,
            "a profile directory should exist while the browser is running"
        );
        let (ok, _) = call(&tool, &mut w, json!({"action": "close"})).await;
        assert!(ok);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        profile_dirs_in(&root),
        0,
        "a throwaway profile directory survived an explicit close"
    );

    // The harder path: never call `close`, just drop the tool. This is the one
    // that actually leaked — SIGKILL is asynchronous, so a `remove_dir_all`
    // issued immediately after it races Chrome's still-flushing child processes
    // and loses on the parent directory.
    {
        let tool = BrowserTool::new()
            .with_policy(AllowAllPolicy)
            .with_launch_config(rooted(&root));
        let mut w = world();
        let port = serve_fixture().await;
        let (ok, _) = call(
            &tool,
            &mut w,
            json!({"action": "open", "url": format!("http://127.0.0.1:{port}")}),
        )
        .await;
        assert!(ok);
        assert_eq!(profile_dirs_in(&root), 1);
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        profile_dirs_in(&root),
        0,
        "dropping the tool without closing leaked a profile directory"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// An empty directory of this test's own, under temp.
fn scratch_root(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "harness-browser-test-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("scratch root");
    p
}

fn rooted(root: &std::path::Path) -> LaunchConfig {
    LaunchConfig {
        profile_root: Some(root.to_path_buf()),
        ..Default::default()
    }
}

fn profile_dirs_in(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("harness-browser-")
                })
                .count()
        })
        .unwrap_or(0)
}

/// Directly exercising the policy trait object the tool stores, with no browser
/// involved — cheap enough to run unconditionally.
#[test]
fn policy_trait_objects_compose_the_way_the_tool_uses_them() {
    let boxed: std::sync::Arc<dyn UrlPolicy> =
        std::sync::Arc::new(PublicOnlyPolicy::literal_only());
    assert!(boxed.check("http://169.254.169.254/").is_err());
    assert!(boxed.check("https://example.com/").is_ok());

    let boxed: std::sync::Arc<dyn UrlPolicy> = std::sync::Arc::new(|u: &str| {
        if u.len() < 30 {
            Ok(())
        } else {
            Err("too long".into())
        }
    });
    assert!(boxed.check("https://a.example/").is_ok());
    assert!(
        boxed
            .check(&format!("https://a.example/{}", "x".repeat(50)))
            .is_err()
    );
}
