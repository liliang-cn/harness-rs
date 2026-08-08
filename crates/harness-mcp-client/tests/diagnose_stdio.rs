//! A stdio MCP server that dies during the handshake used to fail with rmcp's `connection closed:
//! initialize response` and nothing else — no exit status, no stderr, no way to tell a broken binary
//! from a transient start. These pin the sentence that now says which it was.
#![cfg(unix)]

use harness_mcp_client::McpClient;
use std::io::Write;

/// A throwaway executable script, so each case can choose exactly how the server dies.
///
/// Written to a scratch name and *renamed* into place, and the name carries a counter rather than
/// only the pid and the body length. Both exist for the same failure: on Linux, exec'ing a file that
/// any process still holds open for writing is `ETXTBSY` — "Text file busy" — which surfaced here as
/// a CI failure on ubuntu that macOS never reproduces, because macOS does not enforce it. Renaming
/// means the inode that gets executed was never the one open for writing, and a unique name means
/// two concurrent tests cannot land on the same path however their bodies are edited later.
fn server_that(body: &str) -> ScriptPath {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("harness-diagnose-{}-{n}.sh", std::process::id()));
    let scratch = path.with_extension("tmp");

    let mut file = std::fs::File::create(&scratch).expect("write the stand-in server");
    write!(file, "#!/bin/sh\n{body}\n").expect("write the script");
    file.sync_all().expect("flush the script");
    drop(file);
    std::fs::set_permissions(
        &scratch,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o755),
    )
    .expect("make it executable");
    std::fs::rename(&scratch, &path).expect("put the script in place");
    ScriptPath(path)
}

/// Removes the script when the test ends, whether it passed or failed.
struct ScriptPath(std::path::PathBuf);

impl ScriptPath {
    fn as_str(&self) -> &str {
        self.0.to_str().expect("utf-8 path")
    }
}

impl Drop for ScriptPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The connect error, insisting there was one. `McpClient` is not `Debug`, so `expect_err` is out.
async fn connect_error(program: &str) -> String {
    match McpClient::connect_stdio(program, &[]).await {
        Ok(_) => panic!("`{program}` is not a working MCP server, yet connecting to it succeeded"),
        Err(e) => e.to_string(),
    }
}

#[tokio::test]
async fn a_server_that_exits_reports_its_status_and_what_it_said() {
    // The case that cost an hour: CortexDB refuses a database it cannot open, says so on stderr and
    // exits 1. That sentence is the whole diagnosis, and it used to be thrown away.
    let server = server_that(
        "echo 'open cortexdb: unable to open database file: out of memory (14)' >&2\nexit 1",
    );

    let err = connect_error(server.as_str()).await;

    assert!(
        err.contains("exited with status 1"),
        "the error does not say how the server ended: {err}"
    );
    assert!(
        err.contains("unable to open database file"),
        "the error does not quote what the server said: {err}"
    );
}

#[tokio::test]
async fn a_server_that_is_killed_names_the_signal() {
    // What a binary killed on exec looks like — macOS does this to a freshly copied one. It writes
    // nothing, so without the signal there is nothing at all to go on.
    let server = server_that("kill -9 $$");

    let err = connect_error(server.as_str()).await;

    assert!(
        err.contains("killed by signal 9"),
        "the error does not name the signal: {err}"
    );
    assert!(
        err.contains("wrote nothing to stderr"),
        "the error should say the server was silent rather than leave it open: {err}"
    );
}

#[tokio::test]
async fn a_server_that_says_nothing_and_hangs_is_called_a_hang() {
    // Neither answers nor exits. Reported as hanging, because retrying a hang is pointless while
    // retrying a transient start is not.
    let server = server_that("sleep 30");

    let err = connect_error(server.as_str()).await;

    assert!(
        err.contains("hanging rather than crashing"),
        "a server that neither answers nor exits should be called a hang: {err}"
    );
}

#[tokio::test]
async fn a_program_that_is_not_there_says_so_once() {
    let missing = "/nonexistent/harness-diagnose-no-such-server";

    let text = connect_error(missing).await;
    assert!(
        text.contains("No such file") || text.contains("not found"),
        "a missing program should be reported as missing: {text}"
    );
}
