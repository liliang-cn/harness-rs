//! `harness-rs-tools-browser` — an interactive headless browser for harness-rs
//! agents, backed by a **persistent** Chrome DevTools Protocol session.
//!
//! ## What this replaces, and why
//!
//! The usual way to give an agent a browser is a one-shot CLI call:
//! `chrome --headless=new --dump-dom <url>`, read the output, kill the process.
//! It is small, stateless and leak-free, and it can answer "what does this page
//! say" perfectly well.
//!
//! It cannot do anything else. Each invocation is a *new process* with a new
//! profile and an empty cookie jar, so there is no "the page I am on" for a
//! second call to act upon. Not because the flags for clicking are missing —
//! because clicking only means something inside a session, and there isn't one.
//! Every task shaped like "log in, then read the dashboard", "search, then open
//! the third result", "accept the cookie banner, then read the article" is out
//! of reach.
//!
//! So this crate keeps the browser. That decision buys interactivity and costs
//! a process to supervise, a protocol to speak, and a security boundary to
//! defend — which is what the five modules are:
//!
//! | module | responsibility |
//! |---|---|
//! | [`ws`]      | RFC 6455 client, ~200 lines, no dependencies (see the module docs for why not `tungstenite`) |
//! | [`cdp`]     | command-id correlation and event routing over that socket |
//! | [`session`] | find, launch, supervise and reliably destroy the browser |
//! | [`page`]    | the actions, and the accessibility-style summary that makes them usable by a model |
//! | [`policy`]  | where the browser is allowed to go |
//!
//! ## Usage
//!
//! ```ignore
//! use harness_tools_browser::BrowserTool;
//!
//! AgentLoop::boxed(model).with_tool(std::sync::Arc::new(BrowserTool::new()))
//! ```
//!
//! The default [`UrlPolicy`] is [`PublicOnlyPolicy`]: http(s) to public
//! addresses only. A host with its own egress rules supplies a closure:
//!
//! ```ignore
//! BrowserTool::new().with_policy(|url: &str| {
//!     if url.starts_with("https://intranet.example/") { Ok(()) }
//!     else { Err("outside the allowed intranet".to_string()) }
//! })
//! ```
//!
//! ## Two things a host must decide
//!
//! **`World::session` must be set** on a multi-tenant server. Browser sessions
//! are keyed by it, and a browser holds cookies — leaving it `None` gives every
//! caller in the process the same logged-in browser. See [`tool`].
//!
//! **The risk class is `Destructive`** for [`BrowserTool::new`], because a tool
//! that can click can submit an order. [`BrowserTool::read_only`] refuses the
//! mutating actions and declares `Network` instead.
//!
//! ## Dependencies
//!
//! `harness-core`, `serde`, `serde_json`, `tokio`, `tracing`, `async-trait` —
//! all of them already in this workspace. No `chromiumoxide`, no `fantoccini`,
//! no WebDriver, no `tungstenite`. Rationale in [`ws`] and in the crate README
//! section of [`cdp`].

pub mod cdp;
pub mod page;
pub mod policy;
pub mod session;
pub mod tool;
pub mod ws;

pub use page::{
    DEFAULT_MAX_ELEMENTS, DEFAULT_MAX_TEXT, ElementInfo, MAX_MAX_ELEMENTS, PageError, PageSummary,
    TargetKind, budget_elements, classify_target, handle_index, resolve_by_text,
};
pub use policy::{AllowAllPolicy, PolicyDenied, PublicOnlyPolicy, UrlPolicy, parse_host_ip};
pub use session::{
    BrowserSession, DEFAULT_VIEWPORT, LaunchConfig, LaunchError, NO_BROWSER_HELP, find_browser,
};
pub use tool::BrowserTool;
