//! Finding a browser, starting it, keeping it, and — the part that actually
//! matters on a server — reliably getting rid of it again.
//!
//! ## Why a long-lived process at all
//!
//! The one-shot approach (`chrome --headless=new --dump-dom <url>`, read stdout,
//! kill) is genuinely nice: no state, nothing to leak, nothing to clean up. It
//! also cannot click. Every invocation is a new process with a new profile and
//! an empty cookie jar, so "log in, then read the dashboard" is not expressible
//! — not because the flags are missing but because there is nothing for the
//! second call to be *inside of*. Interactivity is the same thing as session
//! lifetime; you cannot have one without the other.
//!
//! What that buys costs three problems, and this module is the three of them:
//!
//! - **Discovery.** `--remote-debugging-port=0` means Chrome picks the port, so
//!   we have to find out which. It writes it to `DevToolsActivePort` in the
//!   profile directory. Parsing stderr for "DevTools listening on …" works too
//!   and is what most tutorials do; it also breaks the moment anything else
//!   logs to stderr first, and forces us to keep a pipe drained forever.
//! - **Crash.** A browser that has been alive for an hour has been asked to
//!   render an hour of arbitrary web content. It will die. That must surface as
//!   "your session ended, here it is again" and not as a wall of timeouts.
//! - **Leaks.** A leaked Chrome is ~200 MB of RSS and a leaked profile is a few
//!   MB of disk, *per session*, and on a multi-tenant server there are many
//!   sessions. So: `kill_on_drop`, an explicit [`BrowserSession::close`], a
//!   `Drop` that kills and unlinks anyway, and an idle reaper in the tool.

use crate::cdp::{CdpClient, CdpError};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long to wait for Chrome to write `DevToolsActivePort`. Cold start on a
/// loaded machine with a cold page cache is seconds, not milliseconds.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Default viewport. 1280x900 is wide enough that responsive sites serve the
/// desktop layout — a 800x600 default silently puts the agent on the mobile
/// site, where the navigation is behind a hamburger menu it then cannot find.
pub const DEFAULT_VIEWPORT: (u32, u32) = (1280, 900);

#[derive(Debug)]
pub enum LaunchError {
    /// No Chromium-family browser on this machine. Carries the message the
    /// model should see: it is actionable, and it is not worth a retry.
    NoBrowser(String),
    Spawn(String),
    /// Chrome started but never published a debugging port — nearly always a
    /// sandbox or profile-permission problem, and the exit status says which.
    Startup(String),
    Cdp(CdpError),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::NoBrowser(m) | LaunchError::Spawn(m) | LaunchError::Startup(m) => {
                write!(f, "{m}")
            }
            LaunchError::Cdp(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LaunchError {}

impl From<CdpError> for LaunchError {
    fn from(e: CdpError) -> Self {
        LaunchError::Cdp(e)
    }
}

/// The message handed back when no browser exists. Phrased as an instruction
/// because it goes to a model, which will otherwise retry the same call.
pub const NO_BROWSER_HELP: &str = "no headless browser found. Install Google Chrome / Chromium / \
     Brave / Edge, or set BROWSER_BIN to the browser executable's path. For a page that does not \
     need JavaScript or interaction, web_fetch may work instead.";

/// Locate a Chromium-family browser.
///
/// Order: the `BROWSER_BIN` override, then the well-known install paths, then a
/// `PATH` scan. Same list and same order as the one-shot implementation this
/// crate replaces, so a deployment that was working keeps working.
pub fn find_browser() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BROWSER_BIN") {
        let p = p.trim();
        if !p.is_empty() && Path::new(p).exists() {
            return Some(PathBuf::from(p));
        }
    }
    const CANDIDATES: [&str; 12] = [
        // macOS app bundles
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        // Linux
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/brave-browser",
        "/usr/bin/microsoft-edge",
        "/snap/bin/chromium",
        "/opt/google/chrome/chrome",
    ];
    for c in CANDIDATES {
        if Path::new(c).exists() {
            return Some(PathBuf::from(c));
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        const NAMES: [&str; 6] = [
            "google-chrome",
            "chromium",
            "chromium-browser",
            "brave-browser",
            "microsoft-edge",
            "chrome",
        ];
        for dir in path.split(':') {
            if dir.is_empty() {
                continue;
            }
            for name in NAMES {
                let p = Path::new(dir).join(name);
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    None
}

/// Knobs the host may want; every one of them has a defensible default.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub viewport: (u32, u32),
    /// Pass `--no-sandbox`.
    ///
    /// Chrome's sandbox is a real security boundary against the page, and
    /// turning it off in a process that renders attacker-chosen HTML is a bad
    /// trade — so this is off by default. It is auto-enabled for uid 0 only,
    /// where Chrome refuses to start at all without it: the alternative there
    /// is not "a sandboxed browser", it is "no browser".
    pub no_sandbox: bool,
    /// Extra flags, appended last so they win.
    pub extra_args: Vec<String>,
    /// Where throwaway profile directories are created. `None` means the system
    /// temp dir.
    ///
    /// Worth overriding when temp is a small tmpfs (a Chrome profile is tens of
    /// megabytes), when the deployment wants browser state on a specific volume,
    /// or — as the leak tests here do — to get an isolated directory whose
    /// contents can be asserted about without racing anything else on the box.
    pub profile_root: Option<PathBuf>,
}

impl Default for LaunchConfig {
    fn default() -> Self {
        Self {
            viewport: DEFAULT_VIEWPORT,
            no_sandbox: running_as_root(),
            extra_args: Vec::new(),
            profile_root: None,
        }
    }
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: getuid() is always safe — no arguments, no allocation, cannot fail.
    unsafe { libc_getuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

#[cfg(unix)]
unsafe extern "C" {
    // Declared directly rather than pulling in the `libc` crate for two calls.
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// One browser, one page, alive across tool calls.
pub struct BrowserSession {
    child: Option<tokio::process::Child>,
    profile_dir: PathBuf,
    cdp: CdpClient,
    /// The flattened target session every page-scoped command is addressed to.
    session_id: String,
    target_id: String,
    binary: PathBuf,
    config: LaunchConfig,
    /// Drives the idle reaper; the tool touches it on every action.
    pub(crate) last_used: Instant,
}

impl BrowserSession {
    pub async fn launch(config: LaunchConfig) -> Result<Self, LaunchError> {
        let binary =
            find_browser().ok_or_else(|| LaunchError::NoBrowser(NO_BROWSER_HELP.to_string()))?;
        Self::launch_with(binary, config).await
    }

    /// Owns the child + profile dir for the window between `spawn` and a fully
    /// constructed [`BrowserSession`]. Without it, an error in port discovery or
    /// the CDP handshake — the two most likely places to fail — leaks exactly
    /// the process and directory this module exists to not leak.
    async fn launch_with(binary: PathBuf, config: LaunchConfig) -> Result<Self, LaunchError> {
        // Clear anything a previously-killed agent process left behind. Once
        // per process, and never touching a directory whose owner is alive.
        sweep_stale_profiles();

        // A throwaway profile, always. Pointing Chrome at the user's real
        // profile would hand every page the agent visits that user's live
        // cookies — and would fail anyway, because Chrome single-instance-locks
        // a profile directory.
        let profile_dir = config
            .profile_root
            .clone()
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("{PROFILE_PREFIX}{}", unique_token()));
        std::fs::create_dir_all(&profile_dir)
            .map_err(|e| LaunchError::Spawn(format!("cannot create browser profile dir: {e}")))?;

        let (w, h) = config.viewport;
        let mut args: Vec<String> = vec![
            "--headless=new".into(),
            // 0 = "pick a free port and tell me". A fixed port collides the
            // moment two sessions exist, which on a server is immediately.
            "--remote-debugging-port=0".into(),
            format!("--user-data-dir={}", profile_dir.display()),
            format!("--window-size={w},{h}"),
            "--disable-gpu".into(),
            "--disable-dev-shm-usage".into(),
            "--hide-scrollbars".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-extensions".into(),
            "--disable-background-networking".into(),
            "--disable-sync".into(),
            "--disable-default-apps".into(),
            "--mute-audio".into(),
            "--metrics-recording-only".into(),
            // Chrome's own first-run/promo surfaces would otherwise show up in
            // the element summary as things the model tries to click.
            "--disable-features=Translate,MediaRouter,OptimizationHints".into(),
        ];
        if config.no_sandbox {
            args.push("--no-sandbox".into());
        }
        args.extend(config.extra_args.iter().cloned());

        let child = tokio::process::Command::new(&binary)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            // Belt to Drop's braces: if this struct is forgotten rather than
            // dropped cleanly, tokio still reaps the child.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&profile_dir);
                LaunchError::Spawn(format!("cannot start `{}`: {e}", binary.display()))
            })?;

        let mut guard = StartupGuard {
            child: Some(child),
            profile_dir: profile_dir.clone(),
        };

        let ws_url = await_devtools_endpoint(guard.child.as_mut(), &profile_dir).await?;
        let cdp = CdpClient::connect(&ws_url).await?;

        // From here the session owns the child, so the guard must not kill it.
        let mut me = Self {
            child: guard.child.take(),
            profile_dir,
            cdp,
            session_id: String::new(),
            target_id: String::new(),
            binary,
            config,
            last_used: Instant::now(),
        };
        // `me` has its own Drop now, so a failure here still cleans up.
        me.open_page().await?;
        Ok(me)
    }

    /// Create the one page this session drives and attach to it.
    ///
    /// `flatten: true` multiplexes the target's traffic onto the browser socket
    /// with a `sessionId` tag, instead of making us open a second WebSocket per
    /// target. One socket, one pump, one place for correlation to be correct.
    async fn open_page(&mut self) -> Result<(), LaunchError> {
        let created = self
            .cdp
            .call("Target.createTarget", json!({ "url": "about:blank" }), None)
            .await?;
        let target_id = created["targetId"]
            .as_str()
            .ok_or_else(|| CdpError::Malformed("Target.createTarget returned no targetId".into()))?
            .to_string();

        let attached = self
            .cdp
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        let session_id = attached["sessionId"]
            .as_str()
            .ok_or_else(|| {
                CdpError::Malformed("Target.attachToTarget returned no sessionId".into())
            })?
            .to_string();

        self.target_id = target_id;
        self.session_id = session_id;

        // Page events back `wait_for`; Runtime backs everything else.
        self.call("Page.enable", json!({})).await?;
        self.call("Runtime.enable", json!({})).await?;
        self.call("Page.setLifecycleEventsEnabled", json!({ "enabled": true }))
            .await?;
        let (w, h) = self.config.viewport;
        // Without an explicit override the headless viewport does not always
        // match --window-size, and element visibility is computed against the
        // viewport — so a mismatch makes the summary lie about what is on screen.
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": w, "height": h, "deviceScaleFactor": 1, "mobile": false }),
        )
        .await?;
        Ok(())
    }

    /// Send any CDP command, scoped to this session's page.
    ///
    /// Public on purpose: this crate implements the dozen or so methods an
    /// agent needs, and CDP has hundreds. A host that wants
    /// `Network.setCookie`, `Emulation.setGeolocationOverride` or
    /// `Page.printToPDF` should not have to fork the crate to get them.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CdpError> {
        self.cdp.call(method, params, Some(&self.session_id)).await
    }

    pub async fn call_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CdpError> {
        self.cdp
            .call_with_timeout(method, params, Some(&self.session_id), timeout)
            .await
    }

    /// Raw CDP events from this browser, for a host that wants to observe
    /// `Network.responseReceived`, console output, or dialogs.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<crate::cdp::CdpEvent> {
        self.cdp.subscribe()
    }

    /// The flattened target session id, for commands issued through
    /// [`CdpClient`] directly.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Is the browser still there?
    ///
    /// Two independent signals, because they fail independently: the process
    /// can be gone while the socket has not noticed yet, and the socket can be
    /// wedged while the process is technically alive.
    pub fn is_alive(&mut self) -> bool {
        if !self.cdp.is_alive() {
            return false;
        }
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// Replace a dead browser in place, keeping the same handle.
    ///
    /// Deliberately *not* automatic-and-silent: page state, cookies and history
    /// are gone, so the tool tells the model the session restarted rather than
    /// letting it believe its login survived.
    pub async fn relaunch(&mut self) -> Result<(), LaunchError> {
        // Reap the corpse first: on a crash the child is a zombie and the
        // profile dir is still on disk, and if the new launch fails we must not
        // have leaked them while trying.
        self.hard_stop();
        let fresh = Self::launch_with(self.binary.clone(), self.config.clone()).await?;
        // Assigning through `*self` drops the old value, running its `Drop` —
        // which is `hard_stop` again, and idempotent (child already taken, dir
        // already unlinked). No manual field surgery, no `unsafe`.
        *self = fresh;
        Ok(())
    }

    /// Ask nicely, then stop asking. Returns whether the browser was still up.
    pub async fn close(&mut self) -> bool {
        let was_alive = self.cdp.is_alive();
        if was_alive {
            if !self.target_id.is_empty() {
                let _ = self
                    .cdp
                    .call_with_timeout(
                        "Target.closeTarget",
                        json!({ "targetId": self.target_id }),
                        None,
                        Duration::from_secs(2),
                    )
                    .await;
            }
            // Browser.close lets Chrome flush and exit on its own, which is the
            // difference between a clean profile dir and a "profile in use"
            // warning on the next launch out of the same temp root.
            let _ = self
                .cdp
                .call_with_timeout("Browser.close", json!({}), None, Duration::from_secs(3))
                .await;
            self.cdp.shutdown().await;
        }
        if let Some(child) = self.child.as_mut() {
            // Give the graceful exit a moment before the hammer.
            let waited = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            if waited.is_err() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        self.child = None;
        remove_profile_dir(&self.profile_dir);
        was_alive
    }

    /// The synchronous half of teardown, usable from `Drop`.
    fn hard_stop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
        self.child = None;
        remove_profile_dir(&self.profile_dir);
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        // Last line of defence. A dropped-without-close session must still not
        // leave 200 MB of Chrome and a profile directory behind — on a
        // multi-tenant server that is a slow-motion outage.
        self.hard_stop();
    }
}

/// Kills and unlinks unless disarmed by taking the child. See
/// [`BrowserSession::launch_with`].
struct StartupGuard {
    child: Option<tokio::process::Child>,
    profile_dir: PathBuf,
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
            remove_profile_dir(&self.profile_dir);
        }
    }
}

/// Prefix every throwaway profile directory shares, so the sweeper below can
/// recognise its own litter and nothing else's.
const PROFILE_PREFIX: &str = "harness-browser-";

/// Delete a profile directory, retrying briefly.
///
/// `SIGKILL` is asynchronous. For a second or so after it, Chrome's renderer
/// and network processes are still flushing caches into the profile, and a
/// `remove_dir_all` that races them removes most of the tree and then fails
/// `ENOTEMPTY` on the parent — leaving a fully-populated 30 MB profile behind.
/// Observed, not theorised: it is what the leak-check test caught. Unlinking
/// open files is fine on unix; it is only *new* files appearing mid-walk that
/// break it, and a few short retries outlast that.
///
/// Deliberately synchronous with a bounded worst case (~140 ms) because the
/// only caller that needs it is `Drop`, which cannot await.
fn remove_profile_dir(dir: &Path) {
    for attempt in 0..5 {
        if !dir.exists() || std::fs::remove_dir_all(dir).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10 * (1 << attempt)));
    }
    // Not fatal, and not silent: the sweeper will get it on the next launch.
    tracing::warn!(dir = %dir.display(), "could not remove browser profile dir");
}

/// Delete profile directories abandoned by *dead* processes.
///
/// `Drop` handles every orderly exit, and the retry above handles the kill
/// race. Neither can help when the agent process itself is `SIGKILL`ed or
/// panics the runtime out from under a live browser — and on a long-running
/// server that eventually happens. Since the pid is baked into the directory
/// name, a dead owner is cheap to recognise.
///
/// Runs once per process, on the first launch. Conservative on purpose: it
/// skips anything whose owning pid still exists, and anything younger than an
/// hour, so a pid that has been recycled onto an unrelated process cannot cost
/// another agent its live browser.
fn sweep_stale_profiles() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        let me = std::process::id();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix(PROFILE_PREFIX) else {
                continue;
            };
            let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
                continue;
            };
            if pid == me || process_exists(pid) {
                continue;
            }
            let old_enough = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().map(|d| d.as_secs() > 3600).unwrap_or(false))
                .unwrap_or(false);
            if old_enough {
                tracing::debug!(dir = %name, "sweeping an abandoned browser profile");
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    });
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 performs the permission and existence check
    // without delivering anything. No pointers, no allocation.
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    // Without a cheap way to ask, assume alive and never sweep. Failing to
    // clean up is a leak; cleaning up a live browser is data loss.
    true
}

/// Poll `DevToolsActivePort` until Chrome writes it, or until Chrome dies.
///
/// The file has exactly two lines: the port, and the browser-level WebSocket
/// path. Watching the child at the same time turns "hangs for 30 seconds then
/// times out" into "exited with status 1" — the difference between a debuggable
/// failure and a mystery.
///
/// Reading stderr for `DevTools listening on ws://…` is the other common way to
/// do this. It requires keeping a pipe drained for the life of the browser, and
/// it breaks whenever anything else writes to stderr first — which, with a
/// GPU-less container, is often.
async fn await_devtools_endpoint(
    mut child: Option<&mut tokio::process::Child>,
    profile_dir: &Path,
) -> Result<String, LaunchError> {
    let port_file = profile_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(c) = child.as_mut()
            && let Ok(Some(status)) = c.try_wait()
        {
            return Err(LaunchError::Startup(format!(
                "browser exited immediately ({status}) without opening a debugging port. \
                 If this is a container running as root, the Chrome sandbox needs \
                 --no-sandbox (LaunchConfig::no_sandbox) or a seccomp profile."
            )));
        }
        if let Ok(body) = std::fs::read_to_string(&port_file) {
            let mut lines = body.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next())
                && !port.trim().is_empty()
                && path.starts_with('/')
            {
                return Ok(format!("ws://127.0.0.1:{}{}", port.trim(), path.trim()));
            }
        }
        if Instant::now() >= deadline {
            return Err(LaunchError::Startup(format!(
                "browser did not publish a debugging port within {}s",
                STARTUP_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Unique enough for a temp directory name without a uuid dependency: pid
/// separates processes, the counter separates sessions within one, and the
/// clock separates restarts that reuse a pid.
pub(crate) fn unique_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{:x}-{}", std::process::id(), t, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_names_do_not_collide() {
        let a = unique_token();
        let b = unique_token();
        assert_ne!(a, b);
        assert!(a.starts_with(&format!("{}-", std::process::id())));
    }

    #[test]
    fn browser_bin_override_wins_when_it_exists() {
        // Point at something that certainly exists and is certainly not Chrome;
        // we are testing the override path, not the binary.
        let me = std::env::current_exe().expect("current exe");
        // SAFETY: single-threaded test, and the var is read only by find_browser.
        unsafe { std::env::set_var("BROWSER_BIN", &me) };
        assert_eq!(find_browser().as_deref(), Some(me.as_path()));
        unsafe { std::env::set_var("BROWSER_BIN", "/nonexistent/definitely/not/here") };
        // A bogus override must fall through to the normal search rather than
        // pretending the browser is at a path that does not exist.
        assert_ne!(
            find_browser().as_deref(),
            Some(Path::new("/nonexistent/definitely/not/here"))
        );
        unsafe { std::env::remove_var("BROWSER_BIN") };
    }

    #[test]
    fn the_no_browser_message_tells_the_model_what_to_do() {
        assert!(NO_BROWSER_HELP.contains("BROWSER_BIN"));
        assert!(NO_BROWSER_HELP.contains("web_fetch"));
    }
}
