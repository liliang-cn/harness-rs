//! The model-facing tool.
//!
//! ## One tool, nine actions — not nine tools
//!
//! The obvious alternative is `browser_open`, `browser_click`, `browser_type`
//! and so on. This crate deliberately does not do that, for three reasons:
//!
//! 1. **The contract is shared and it is the hard part.** Handles expire after
//!    every action; `target` may be a handle, a selector or visible text; the
//!    response always carries a fresh element list. That paragraph has to be
//!    understood before *any* of the actions can be used correctly. Split
//!    across nine descriptions it is either repeated nine times (nine times the
//!    tokens, in a list the model re-reads every turn) or stated once and
//!    missed.
//! 2. **They are one stateful conversation, not nine capabilities.** `click`
//!    without a prior `open` is meaningless. An enum makes the sequencing
//!    legible; nine sibling tools present nine equally-valid-looking entry
//!    points.
//! 3. **Tool-list pressure is real.** An agent already carrying file, shell,
//!    search and memory tools pays for every extra name in every request.
//!
//! The cost of the choice is a wider parameter object with conditionally
//! required fields, which the schema cannot express and the description
//! therefore states in words, per action. That is a real cost — but it is paid
//! once, in one place, by a model that is reading the description anyway.
//!
//! ## Risk, honestly
//!
//! [`ToolRisk::Network`] is what the one-shot predecessor declared, and for
//! *reading* a page it is right. But this tool can also submit the form that
//! places the order, and can do it on a site the user is logged into. That is
//! not "network", it is [`ToolRisk::Destructive`] — a side effect that must not
//! be blindly retried. Since `risk()` is one static value per tool, the
//! resolution is two constructors: [`BrowserTool::new`] is `Destructive`, and
//! [`BrowserTool::read_only`] refuses the mutating actions outright and is
//! honestly `Network`. A host that wants unattended browsing registers the
//! second one.
//!
//! ## Multi-tenancy
//!
//! Sessions are keyed by `World::session` — actor *and* conversation id. A
//! browser holds cookies; sharing one across tenants would share their logins.
//! When the host leaves `World::session` as `None` (a CLI run, a test) every
//! caller shares a single browser, which is correct for those cases and wrong
//! for a server. That is stated here rather than hidden because a serving host
//! that forgets to set `session` gets a cross-tenant cookie jar and no warning.

use crate::page::{self, ElementInfo, PageError};
use crate::policy::{PublicOnlyPolicy, UrlPolicy};
use crate::session::{BrowserSession, LaunchConfig, LaunchError, NO_BROWSER_HELP, find_browser};
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Close a browser nobody has touched for this long. A logged-in session is
/// worth keeping between turns; it is not worth keeping between conversations,
/// and 200 MB of resident Chrome per abandoned chat adds up fast.
const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(10 * 60);
/// Concurrent browsers per process. Past this the least-recently-used one is
/// evicted rather than the host being OOM-killed.
const DEFAULT_MAX_SESSIONS: usize = 8;
/// Ceiling on any caller-supplied `timeout_ms`.
const MAX_TIMEOUT_MS: u64 = 60_000;

/// A browser session plus the snapshot its handles refer to.
struct Tab {
    session: BrowserSession,
    /// The *complete* element list from the last snapshot, not the budgeted
    /// slice — so `target` by visible text can still reach an element that was
    /// cut from the response.
    last_elements: Vec<ElementInfo>,
}

/// Interactive headless browser, backed by a persistent CDP session.
pub struct BrowserTool {
    schema: ToolSchema,
    policy: Arc<dyn UrlPolicy>,
    config: LaunchConfig,
    read_only: bool,
    idle_ttl: Duration,
    max_sessions: usize,
    tabs: std::sync::Mutex<HashMap<String, Arc<Mutex<Tab>>>>,
}

impl Default for BrowserTool {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserTool {
    /// Full interactive browser. [`ToolRisk::Destructive`].
    pub fn new() -> Self {
        Self::build(false)
    }

    /// Navigation and reading only — `click`, `type` and `select` are refused
    /// with an explanation. [`ToolRisk::Network`].
    pub fn read_only() -> Self {
        Self::build(true)
    }

    fn build(read_only: bool) -> Self {
        Self {
            schema: schema_for(read_only),
            policy: Arc::new(PublicOnlyPolicy::new()),
            config: LaunchConfig::default(),
            read_only,
            idle_ttl: DEFAULT_IDLE_TTL,
            max_sessions: DEFAULT_MAX_SESSIONS,
            tabs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Replace the SSRF guard. Takes anything implementing [`UrlPolicy`],
    /// including a plain closure.
    ///
    /// The default ([`PublicOnlyPolicy`]) is public-internet-only. Overriding it
    /// with something laxer is the single most dangerous thing a host can do
    /// with this crate — see [`crate::policy`] for what it is protecting.
    pub fn with_policy<P: UrlPolicy>(mut self, policy: P) -> Self {
        self.policy = Arc::new(policy);
        self
    }

    pub fn with_launch_config(mut self, config: LaunchConfig) -> Self {
        self.config = config;
        self
    }

    /// How long an untouched browser survives before being reaped.
    pub fn with_idle_ttl(mut self, ttl: Duration) -> Self {
        self.idle_ttl = ttl;
        self
    }

    pub fn with_max_sessions(mut self, n: usize) -> Self {
        self.max_sessions = n.max(1);
        self
    }

    /// Cookies are per-browser, so the key must be per-tenant *and* per
    /// conversation. `actor` is included separately from `id` so two hosts that
    /// generate conversation ids independently cannot collide into one another's
    /// browser.
    fn key_for(world: &World) -> String {
        match &world.session {
            Some(s) => format!("{}\u{1}{}", s.actor, s.id),
            // Documented at the top of this module: shared, and only correct
            // outside a served conversation.
            None => "\u{1}__ambient__".to_string(),
        }
    }

    /// Drop tabs nobody is using and nobody has touched recently.
    ///
    /// Runs on every invocation rather than on a timer: a tool has no lifecycle
    /// hook to hang a background task on, and doing it inline means the reaper
    /// cannot outlive the tool or keep the process alive.
    fn reap_idle(&self) {
        let ttl = self.idle_ttl;
        let mut doomed = Vec::new();
        {
            let mut tabs = self.tabs.lock().expect("tab registry");
            tabs.retain(|_, arc| {
                // A tab another task is mid-action on is busy by definition;
                // `try_lock` failing is the cheapest way to know that.
                let Ok(guard) = arc.try_lock() else {
                    return true;
                };
                let idle = guard.session.last_used.elapsed();
                drop(guard);
                if idle > ttl {
                    doomed.push(arc.clone());
                    false
                } else {
                    true
                }
            });
        }
        // Dropped outside the registry lock; `BrowserSession::drop` kills the
        // process and unlinks the profile directory.
        drop(doomed);
    }

    /// Evict the least-recently-used free tab to stay under the cap.
    fn enforce_cap(&self) {
        let mut victim = None;
        {
            let mut tabs = self.tabs.lock().expect("tab registry");
            while tabs.len() >= self.max_sessions {
                let oldest = tabs
                    .iter()
                    .filter_map(|(k, v)| {
                        v.try_lock().ok().map(|g| (k.clone(), g.session.last_used))
                    })
                    .min_by_key(|(_, t)| *t)
                    .map(|(k, _)| k);
                match oldest {
                    Some(k) => {
                        victim = tabs.remove(&k);
                    }
                    // Everything is busy; refusing to evict is better than
                    // yanking a browser out from under a running action.
                    None => break,
                }
            }
        }
        drop(victim);
    }

    fn get_tab(&self, key: &str) -> Option<Arc<Mutex<Tab>>> {
        self.tabs.lock().expect("tab registry").get(key).cloned()
    }

    /// Get the tab for this caller, launching a browser if there is none.
    async fn open_tab(&self, key: &str) -> Result<Arc<Mutex<Tab>>, LaunchError> {
        if let Some(t) = self.get_tab(key) {
            return Ok(t);
        }
        self.enforce_cap();
        let session = BrowserSession::launch(self.config.clone()).await?;
        let tab = Arc::new(Mutex::new(Tab {
            session,
            last_elements: Vec::new(),
        }));
        let mut tabs = self.tabs.lock().expect("tab registry");
        // Another task may have won the race while we were launching; keep
        // theirs and let ours drop (which kills it cleanly).
        Ok(tabs.entry(key.to_string()).or_insert(tab).clone())
    }
}

/// Actions that change the page. Refused by [`BrowserTool::read_only`].
const MUTATING: [&str; 3] = ["click", "type", "select"];

#[async_trait::async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    fn risk(&self) -> ToolRisk {
        if self.read_only {
            ToolRisk::Network
        } else {
            // Clicking "Place order" is not idempotent and not merely network.
            ToolRisk::Destructive
        }
    }

    async fn invoke(&self, args: Value, world: &mut World) -> Result<ToolResult, ToolError> {
        self.reap_idle();

        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("read")
            .to_string();
        if self.read_only && MUTATING.contains(&action.as_str()) {
            return Ok(fail(json!({
                "action": action,
                "error": format!(
                    "`{action}` is disabled: this agent has a read-only browser. Navigation, \
                     reading, scrolling and screenshots are available."
                ),
            })));
        }

        // No browser is a permanent, actionable condition, not an error to
        // retry. Same contract as the one-shot tool this replaces.
        if find_browser().is_none() {
            return Ok(fail(json!({ "action": action, "error": NO_BROWSER_HELP })));
        }

        let key = Self::key_for(world);
        let timeout = Duration::from_millis(
            args.get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(20_000)
                .clamp(1_000, MAX_TIMEOUT_MS),
        );
        let max_elements = args
            .get("max_elements")
            .and_then(Value::as_u64)
            .unwrap_or(page::DEFAULT_MAX_ELEMENTS as u64) as usize;

        // Every action except `open` needs a page that is already there.
        let tab = if action == "open" {
            // The policy runs *before* the browser does. `page::navigate` checks
            // again — but if the answer is going to be no, spending 200 MB and a
            // second of start-up to hear it is a denial-of-service a model can
            // trigger by looping on a refused URL.
            let url = args.get("url").and_then(Value::as_str).unwrap_or_default();
            if let Err(denied) = self.policy.check(url) {
                return Ok(fail(json!({
                    "action": action,
                    "error": denied.to_string(),
                })));
            }
            match self.open_tab(&key).await {
                Ok(t) => t,
                Err(e) => {
                    return Ok(fail(json!({ "action": action, "error": e.to_string() })));
                }
            }
        } else {
            match self.get_tab(&key) {
                Some(t) => t,
                None => {
                    return Ok(fail(json!({
                        "action": action,
                        "error": "no browser session yet — call action=\"open\" with a url first.",
                    })));
                }
            }
        };

        let mut tab = tab.lock().await;
        let mut notes: Vec<String> = Vec::new();

        // A browser that has rendered an hour of arbitrary web content will
        // eventually die. Rebuild it, and say so — the model must not go on
        // believing its login survived.
        if !tab.session.is_alive() {
            if action != "open" {
                return Ok(fail(json!({
                    "action": action,
                    "error": "the browser process died. Re-open the page with action=\"open\"; \
                              cookies, history and any login are gone.",
                })));
            }
            if let Err(e) = tab.session.relaunch().await {
                return Ok(fail(json!({ "action": action, "error": e.to_string() })));
            }
            tab.last_elements.clear();
            notes.push(
                "the browser had crashed and was restarted; cookies and history are gone".into(),
            );
        }
        tab.session.last_used = Instant::now();

        let outcome = run_action(
            &mut tab,
            &action,
            &args,
            world,
            &self.policy,
            timeout,
            max_elements,
        )
        .await;

        match outcome {
            Ok(mut body) => {
                if !notes.is_empty()
                    && let Some(obj) = body.as_object_mut()
                {
                    obj.insert("notes".into(), json!(notes));
                }
                let trace = format!(
                    "browser {action} → {}",
                    body.get("url")
                        .and_then(Value::as_str)
                        .unwrap_or("(no url)")
                );
                Ok(ToolResult {
                    ok: true,
                    content: body,
                    trace: Some(trace),
                })
            }
            // Everything below is a page-level "no", which the model can act
            // on: a denied URL, a missing element, a script that threw. Handing
            // these back as `ok: false` keeps the loop running with a usable
            // explanation instead of aborting the turn.
            Err(e) => Ok(fail(json!({
                "action": action,
                "error": e.to_string(),
            }))),
        }
    }
}

fn fail(content: Value) -> ToolResult {
    let trace = content
        .get("error")
        .and_then(Value::as_str)
        .map(|e| format!("browser: {e}"));
    ToolResult {
        ok: false,
        content,
        trace,
    }
}

#[allow(clippy::too_many_arguments)] // one dispatch site; splitting it would only move the arguments
async fn run_action(
    tab: &mut Tab,
    action: &str,
    args: &Value,
    world: &World,
    policy: &Arc<dyn UrlPolicy>,
    timeout: Duration,
    max_elements: usize,
) -> Result<Value, PageError> {
    let str_arg = |k: &str| args.get(k).and_then(Value::as_str);
    let by = str_arg("by");

    // Resolve `target` against the previous snapshot before anything mutates
    // the page — that snapshot is what the handles in it refer to.
    let need_target = |what: &str| -> Result<String, PageError> {
        str_arg("target")
            .map(str::to_string)
            .ok_or_else(|| PageError::Io(format!("`{what}` needs a `target`")))
    };

    let mut did: Option<String> = None;
    let mut extra = serde_json::Map::new();

    match action {
        "open" => {
            let url = str_arg("url").ok_or_else(|| PageError::Io("`open` needs a `url`".into()))?;
            page::navigate(&tab.session, url, policy, timeout).await?;
            did = Some(format!("opened {url}"));
        }
        "read" => {
            let max_text = args
                .get("max_chars")
                .and_then(Value::as_u64)
                .unwrap_or(page::DEFAULT_MAX_TEXT as u64)
                .clamp(200, 100_000) as usize;
            let (text, truncated) = page::read_text(&tab.session, max_text).await?;
            extra.insert("text".into(), json!(text));
            if truncated {
                extra.insert(
                    "text_truncated".into(),
                    json!(format!(
                        "cut at {max_text} characters; scroll or raise max_chars for the rest"
                    )),
                );
            }
        }
        "screenshot" => {
            let png = page::screenshot_png(&tab.session).await?;
            let name = format!("browser-{}.png", crate::session::unique_token());
            let path = world.repo.root.join(&name);
            std::fs::write(&path, &png)
                .map_err(|e| PageError::Io(format!("cannot write {}: {e}", path.display())))?;
            extra.insert("saved_file".into(), json!(name));
            extra.insert("bytes".into(), json!(png.len()));
            did = Some(format!(
                "saved a {} byte PNG to the working directory",
                png.len()
            ));
        }
        "click" => {
            let target = need_target("click")?;
            let resolved = page::resolve_target(&target, by, &tab.last_elements)?;
            if let Some(n) = &resolved.note {
                extra.insert("resolved".into(), json!(n));
            }
            let what = page::click(&tab.session, &resolved).await?;
            // A click is a navigation whose URL nobody chose. Give it a moment
            // to become one, then re-apply the policy to wherever it landed.
            page::await_ready(&tab.session, timeout).await?;
            page::enforce_landing(&tab.session, policy).await?;
            did = Some(what);
        }
        "type" => {
            let target = need_target("type")?;
            let text =
                str_arg("text").ok_or_else(|| PageError::Io("`type` needs `text`".into()))?;
            let resolved = page::resolve_target(&target, by, &tab.last_elements)?;
            if let Some(n) = &resolved.note {
                extra.insert("resolved".into(), json!(n));
            }
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
            let clear = args.get("clear").and_then(Value::as_bool).unwrap_or(true);
            let what = page::type_text(&tab.session, &resolved, text, clear, submit).await?;
            if submit {
                page::await_ready(&tab.session, timeout).await?;
                page::enforce_landing(&tab.session, policy).await?;
            }
            did = Some(what);
        }
        "select" => {
            let target = need_target("select")?;
            let value =
                str_arg("value").ok_or_else(|| PageError::Io("`select` needs a `value`".into()))?;
            let resolved = page::resolve_target(&target, by, &tab.last_elements)?;
            if let Some(n) = &resolved.note {
                extra.insert("resolved".into(), json!(n));
            }
            did = Some(page::select_option(&tab.session, &resolved, value).await?);
        }
        "scroll" => {
            let dir = str_arg("direction").unwrap_or("down");
            let amount = args.get("amount").and_then(Value::as_f64);
            did = Some(page::scroll(&tab.session, dir, amount).await?);
        }
        "wait_for" => {
            let target = need_target("wait_for")?;
            did = Some(page::wait_for(&tab.session, &target, by, timeout).await?);
        }
        "back" => {
            did = Some(page::go_back(&tab.session, policy, timeout).await?);
        }
        "current_url" => {
            // The one action that deliberately returns no element list: it is
            // the cheap "where am I" probe, and a snapshot would defeat that.
            let url = page::current_url(&tab.session).await?;
            return Ok(json!({ "action": "current_url", "url": url }));
        }
        "close" => {
            let was_alive = tab.session.close().await;
            tab.last_elements.clear();
            return Ok(json!({
                "action": "close",
                "did": if was_alive { "browser closed" } else { "browser was already gone" },
            }));
        }
        other => {
            return Err(PageError::Io(format!(
                "unknown action `{other}`. Valid: open, read, screenshot, click, type, select, \
                 scroll, wait_for, back, current_url, close."
            )));
        }
    }

    // Every path that gets here re-describes the page. This is the contract:
    // the response the model reads always matches the handles it may use next.
    let (summary, all) = page::snapshot(&tab.session, max_elements).await?;
    tab.last_elements = all;

    let mut body = serde_json::to_value(&summary).unwrap_or_else(|_| json!({}));
    if let Some(obj) = body.as_object_mut() {
        obj.insert("action".into(), json!(action));
        if let Some(d) = did {
            obj.insert("did".into(), json!(d));
        }
        obj.extend(extra);
        if !summary.elements.is_empty() {
            obj.insert(
                "handles_note".into(),
                json!(
                    "Handles (e0, e1, …) refer to THIS list only and are invalidated by your \
                       next action. Always take them from the most recent response."
                ),
            );
        }
    }
    Ok(body)
}

fn schema_for(read_only: bool) -> ToolSchema {
    let actions: Vec<&str> = if read_only {
        vec![
            "open",
            "read",
            "screenshot",
            "scroll",
            "wait_for",
            "back",
            "current_url",
            "close",
        ]
    } else {
        vec![
            "open",
            "read",
            "screenshot",
            "click",
            "type",
            "select",
            "scroll",
            "wait_for",
            "back",
            "current_url",
            "close",
        ]
    };

    let mut description = String::from(
        "Drive a REAL headless browser that stays open between calls, so a whole flow — open, \
         type, click, read the result — happens in one live session with its cookies and login \
         intact. Use this instead of web_fetch whenever the page runs JavaScript, needs a login, \
         needs a form filled in, or hides what you want behind a button.\n\n\
         HOW TO USE IT. Start with action=\"open\" and a url. Every response describes the page: \
         `url`, `title`, and `elements` — the interactive things on it, each with a `handle` \
         (\"e0\", \"e1\", …), a `role` and the `label` a person would read. Pick what to do next \
         from that list.\n\n\
         TARGETING. `target` accepts three things, auto-detected, or forced with `by`:\n\
         • a handle from the LAST response — `target: \"e7\"` — the most reliable;\n\
         • visible text — `target: \"Sign in\"`, `target: \"登录\"`, even `target: \"the button \
         that says 登录\"` — matched against the labels in that same list;\n\
         • a CSS selector — `target: \"#login\", \"input[name=q]\"` — anything containing `# . [ \
         ] >` is read as CSS, so use by:\"text\" if a label genuinely looks like one.\n\
         HANDLES EXPIRE. e0/e1/… name the elements of the response you just got and nothing else. \
         After any action they are re-issued and may point somewhere different. Never reuse a \
         handle from an earlier turn.\n\n\
         ACTIONS.\n\
         • open — go to `url`. http/https only, public addresses only.\n\
         • read — the page's rendered visible text (`max_chars`, default 8000).\n\
         • screenshot — save a PNG to the working directory, return its filename.\n",
    );
    if !read_only {
        description.push_str(
            "• click — click `target`. Follows any navigation it causes.\n\
             • type — type `text` into `target`. `clear` (default true) empties the field first; \
             `submit: true` presses Enter afterwards, which is how you search or log in.\n\
             • select — choose `value` in a <select>; matches the option's value or its visible text.\n",
        );
    }
    description.push_str(
        "• scroll — `direction`: down (default), up, top, bottom; optional `amount` in pixels. \
         The `scroll` field in each response says how far down the page you are.\n\
         • wait_for — poll until `target` (a selector, or text anywhere on the page) shows up. \
         Use after an action that loads content asynchronously.\n\
         • back — previous page in this tab's history.\n\
         • current_url — where the tab is now, without a full page description.\n\
         • close — end the session and free the browser. Do this when you are finished.\n\n\
         LIMITS. Elements are capped (`max_elements`, default 60) and the response says how many \
         were omitted — scroll or raise the cap rather than assuming something is not there. \
         Private, loopback and link-local addresses are refused, and so is a public page that \
         redirects to one. If a click seems to do nothing, `read` the page: a cookie banner or \
         modal is probably on top of it.",
    );
    if read_only {
        description.push_str(
            "\nThis agent's browser is READ-ONLY: click, type and select are unavailable.",
        );
    }

    ToolSchema {
        name: "browser".into(),
        description,
        input: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": actions,
                    "description": "What to do. Defaults to \"read\"."
                },
                "url": {"type": "string", "description": "For action=open: the http(s) URL."},
                "target": {
                    "type": "string",
                    "description": "Element to act on: a handle from the last response (\"e7\"), \
                                    its visible text (\"Sign in\"), or a CSS selector (\"#login\")."
                },
                "by": {
                    "type": "string",
                    "enum": ["auto", "handle", "css", "text"],
                    "description": "Force how `target` is read. Default auto."
                },
                "text": {"type": "string", "description": "For action=type: the text to enter."},
                "value": {"type": "string", "description": "For action=select: the option's value or visible text."},
                "submit": {"type": "boolean", "description": "For action=type: press Enter afterwards. Default false."},
                "clear": {"type": "boolean", "description": "For action=type: empty the field first. Default true."},
                "direction": {
                    "type": "string",
                    "enum": ["down", "up", "top", "bottom"],
                    "description": "For action=scroll. Default down."
                },
                "amount": {"type": "number", "description": "For action=scroll: pixels. Default about one screen."},
                "max_chars": {"type": "integer", "description": "For action=read: text budget. Default 8000."},
                "max_elements": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "description": "Cap on the elements listed back. Default 60."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Page-load / wait_for budget, 1000–60000. Default 20000."
                }
            },
            "required": ["action"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::SessionRef;

    fn world_with(session: Option<SessionRef>) -> World {
        let mut w = harness_context::default_world(std::env::temp_dir());
        w.session = session;
        w
    }

    #[test]
    fn sessions_are_keyed_per_tenant_and_conversation() {
        let a = BrowserTool::key_for(&world_with(Some(SessionRef {
            id: "c1".into(),
            actor: "alice".into(),
            request: "r1".into(),
        })));
        let b = BrowserTool::key_for(&world_with(Some(SessionRef {
            id: "c1".into(),
            actor: "bob".into(),
            request: "r9".into(),
        })));
        assert_ne!(a, b, "two tenants must not share a cookie jar");

        let a2 = BrowserTool::key_for(&world_with(Some(SessionRef {
            id: "c1".into(),
            actor: "alice".into(),
            // A different turn of the same conversation is the same browser —
            // that is the entire point of a persistent session.
            request: "r2".into(),
        })));
        assert_eq!(a, a2);

        // The separator must make "al" + "icec1" impossible to confuse with
        // "alice" + "c1".
        let x = BrowserTool::key_for(&world_with(Some(SessionRef {
            id: "icec1".into(),
            actor: "al".into(),
            request: String::new(),
        })));
        assert_ne!(a, x);
    }

    #[test]
    fn risk_reflects_what_the_tool_can_actually_do() {
        assert_eq!(BrowserTool::new().risk(), ToolRisk::Destructive);
        assert_eq!(BrowserTool::read_only().risk(), ToolRisk::Network);
    }

    #[test]
    fn the_description_states_the_handle_contract() {
        let s = BrowserTool::new().schema().description.clone();
        // If a model misses this it will click something at random.
        assert!(s.contains("HANDLES EXPIRE"));
        assert!(s.contains("visible text"));
        assert!(s.to_lowercase().contains("css selector"));
        // Every advertised action must be described, not just enumerated.
        for a in [
            "open",
            "read",
            "screenshot",
            "click",
            "type",
            "select",
            "scroll",
            "wait_for",
            "back",
            "current_url",
        ] {
            assert!(
                s.contains(a),
                "action `{a}` is not explained in the description"
            );
        }
    }

    #[test]
    fn read_only_hides_the_mutating_actions_from_the_schema() {
        let s = BrowserTool::read_only();
        let actions = s.schema().input["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        for m in MUTATING {
            assert!(!actions.contains(&m), "read-only must not advertise `{m}`");
        }
        assert!(actions.contains(&"open"));
        assert!(s.schema().description.contains("READ-ONLY"));
    }

    #[tokio::test]
    async fn read_only_refuses_a_click_before_touching_a_browser() {
        let tool = BrowserTool::read_only();
        let mut w = world_with(None);
        let res = tool
            .invoke(json!({ "action": "click", "target": "e0" }), &mut w)
            .await
            .unwrap();
        assert!(!res.ok);
        let err = res.content["error"].as_str().unwrap();
        assert!(err.contains("read-only"), "{err}");
    }

    #[tokio::test]
    async fn acting_without_opening_says_what_to_do() {
        // Only meaningful when a browser exists; otherwise the no-browser
        // message fires first, which is also correct.
        let tool = BrowserTool::new();
        let mut w = world_with(None);
        let res = tool
            .invoke(json!({ "action": "click", "target": "e0" }), &mut w)
            .await
            .unwrap();
        assert!(!res.ok);
        let err = res.content["error"].as_str().unwrap();
        assert!(
            err.contains("action=\"open\"") || err.contains("BROWSER_BIN"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn an_unknown_action_lists_the_real_ones() {
        // Without a browser this short-circuits earlier, so only assert when
        // one is present.
        if find_browser().is_none() {
            return;
        }
        let tool = BrowserTool::new();
        let mut w = world_with(None);
        let res = tool
            .invoke(json!({ "action": "teleport" }), &mut w)
            .await
            .unwrap();
        assert!(!res.ok);
        assert!(res.content["error"].as_str().unwrap().contains("open"));
    }

    #[test]
    fn schema_is_valid_json_schema_shaped() {
        let s = BrowserTool::new();
        let input = &s.schema().input;
        assert_eq!(input["type"], "object");
        assert!(input["properties"].is_object());
        assert_eq!(input["required"][0], "action");
    }
}
