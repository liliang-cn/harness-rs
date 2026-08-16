//! Driving the page, and describing it back to a model that cannot see it.
//!
//! ## The summary is the whole product
//!
//! A tool that can click is useless if the caller has to guess what is
//! clickable. Handing a model rendered text and asking it to produce a CSS
//! selector is asking it to hallucinate one — it has never seen the markup.
//! So every action here returns the same thing an accessibility tree gives a
//! screen reader: an enumerated list of *interactable* elements, each with a
//! role, a visible label, and a handle that can be passed straight back to the
//! next call. The model's job becomes selection from a list, which it is good
//! at, instead of authoring a selector, which it is not.
//!
//! ## Handles, and why they expire
//!
//! A handle is `e<N>`: the index of the element in `window.__harnessRefs`, an
//! array of live `Element` references that the snapshot script rebuilds each
//! time it runs. That gives an *identity*, not a query — clicking `e7` clicks
//! exactly the node that was listed as `e7`, even if three other rows now match
//! the same selector, and even if the element has no id, no stable class and a
//! label that appears forty times on the page.
//!
//! The cost is that the array dies with the document. So handles are valid
//! until the next action, and every action returns a fresh snapshot to replace
//! them. This is stated bluntly in the tool description, because a model that
//! caches `e7` across a navigation will click something at random.
//!
//! Two escape hatches exist for when a handle is not what the caller has: a CSS
//! selector, and visible text. The visible-text path is resolved *here*, in
//! Rust, against the last snapshot — see [`resolve_by_text`] — rather than in
//! injected JavaScript, so its ranking is a unit test rather than a page-
//! dependent mystery.
//!
//! ## Budgeting
//!
//! A search-results page has 300 links; a table has one "Edit" per row. Sending
//! all of them costs more context than the page is worth and buries the three
//! that matter. [`budget_elements`] keeps a bounded, viewport-first slice and
//! says out loud how many it dropped, so the model knows to scroll rather than
//! concluding the element does not exist.

use crate::cdp::CdpError;
use crate::policy::{PolicyDenied, UrlPolicy};
use crate::session::BrowserSession;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default cap on elements per summary. Sized against a real context budget: at
/// roughly 15 tokens per row this is ~900 tokens, which is affordable on every
/// action, and it comfortably covers the interactive surface of an ordinary
/// page. Callers can raise it per call.
pub const DEFAULT_MAX_ELEMENTS: usize = 60;
/// Hard ceiling regardless of what the caller asks for.
pub const MAX_MAX_ELEMENTS: usize = 200;
/// Labels longer than this are a paragraph that happens to be inside a link.
const MAX_LABEL_CHARS: usize = 80;
/// Default cap on rendered page text.
pub const DEFAULT_MAX_TEXT: usize = 8_000;

#[derive(Debug)]
pub enum PageError {
    Cdp(CdpError),
    Policy(PolicyDenied),
    /// The injected script threw.
    Js(String),
    /// The target did not resolve to an element.
    NoSuchElement {
        target: String,
        hint: String,
    },
    Io(String),
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageError::Cdp(e) => write!(f, "{e}"),
            PageError::Policy(e) => write!(f, "{e}"),
            PageError::Js(s) => write!(f, "page script failed: {s}"),
            PageError::NoSuchElement { target, hint } => {
                write!(f, "no element matched `{target}`. {hint}")
            }
            PageError::Io(s) => write!(f, "{s}"),
        }
    }
}

impl From<CdpError> for PageError {
    fn from(e: CdpError) -> Self {
        PageError::Cdp(e)
    }
}

impl From<PolicyDenied> for PageError {
    fn from(e: PolicyDenied) -> Self {
        PageError::Policy(e)
    }
}

/// One interactable thing on the page.
///
/// Serialisation is aggressively sparse — every `Some`/`true` field costs
/// tokens on every one of up to 60 rows, on every action, for the whole
/// conversation. Defaults are simply absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ElementInfo {
    /// `e<N>` — pass this back as `target`.
    pub handle: String,
    /// ARIA role if declared, else derived from the tag: `link`, `button`,
    /// `textbox`, `checkbox`, `radio`, `select`, `clickable`, …
    pub role: String,
    /// What a person would read on it.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub checked: bool,
    /// True for elements that exist but are scrolled off screen. Kept in the
    /// output because "not visible" is exactly why a click did nothing.
    #[serde(default, skip_serializing_if = "is_false")]
    pub offscreen: bool,
    /// Extra discriminator: a link's target host, an input's type, a select's
    /// options. Omitted when there is nothing useful to add.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Position in DOM order, used for ranking and for restoring order after
    /// the viewport-first cut. Read from the snapshot, never sent to the model —
    /// the handle already encodes it.
    #[serde(default, skip_serializing)]
    pub index: usize,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde requires the &T signature
fn is_false(b: &bool) -> bool {
    !*b
}

/// What one action hands back.
#[derive(Debug, Clone, Serialize)]
pub struct PageSummary {
    pub url: String,
    pub title: String,
    /// The bounded slice.
    pub elements: Vec<ElementInfo>,
    /// How many interactables the page actually has.
    pub element_count: usize,
    /// Set when `element_count` exceeded the budget, phrased for the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elements_note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scroll: Option<ScrollState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScrollState {
    /// 0 at the top, 100 at the bottom. Tells the model whether scrolling can
    /// still reveal anything.
    pub percent: u32,
    pub at_bottom: bool,
}

// ─────────────────────────────────────────────────────────────────────
// Target resolution — the pure, testable half
// ─────────────────────────────────────────────────────────────────────

/// How a `target` string should be interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// `e12` from a previous summary.
    Handle,
    /// A CSS selector.
    Css,
    /// Visible text, matched against the last snapshot's labels.
    Text,
}

/// Guess what the caller meant.
///
/// The ambiguity that matters is `button`: a valid CSS type selector *and* a
/// plausible thing to say. Guessing CSS there sends the model to the first
/// `<button>` on the page, which is usually wrong and always confusing, so the
/// rule is conservative — CSS only when the string contains punctuation that
/// has no business being in a label. `by: "css"` overrides it.
pub fn classify_target(target: &str) -> TargetKind {
    let t = target.trim();
    if is_handle(t) {
        return TargetKind::Handle;
    }
    // `#id`, `.class`, `[attr=…]` and any combinator/pseudo are unambiguous.
    let starts_css = t.starts_with('#') || t.starts_with('.') || t.starts_with('[');
    let has_css_syntax = t.contains('[')
        || t.contains(']')
        || t.contains('>')
        || t.contains("::")
        || (t.contains('.') && !t.contains(' '))
        || (t.contains('#') && !t.contains(' '));
    if starts_css || has_css_syntax {
        TargetKind::Css
    } else {
        TargetKind::Text
    }
}

fn is_handle(t: &str) -> bool {
    t.len() >= 2 && t.starts_with('e') && t[1..].chars().all(|c| c.is_ascii_digit())
}

/// The index inside `window.__harnessRefs`, for a `e<N>` handle.
pub fn handle_index(t: &str) -> Option<usize> {
    if !is_handle(t) {
        return None;
    }
    t[1..].parse().ok()
}

/// Collapse whitespace and case so "  Sign  In " and "sign in" compare equal.
fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Find the element a human-language target refers to.
///
/// Scoring, best first: exact label, label is a prefix of the query, the query
/// contains the whole label (which is what "click the button that says 登录"
/// looks like), the label contains the query. Ties go to the shorter label —
/// between "Log in" and "Log in with a different account", the plain one is
/// what "log in" meant — then to whatever is on screen, then to DOM order.
///
/// Returns `None` rather than a bad guess: a wrong click is worse than an
/// honest miss, because the model can recover from a miss by reading the list.
pub fn resolve_by_text<'a>(elements: &'a [ElementInfo], query: &str) -> Option<&'a ElementInfo> {
    let q = norm(query);
    if q.is_empty() {
        return None;
    }
    let mut best: Option<(u32, &ElementInfo)> = None;
    for el in elements {
        let label = norm(&el.label);
        let value = el.value.as_deref().map(norm).unwrap_or_default();
        if label.is_empty() && value.is_empty() {
            continue;
        }
        let score = if label == q {
            100
        } else if !value.is_empty() && value == q {
            95
        } else if label.starts_with(&q) {
            80
        } else if !label.is_empty() && q.contains(&label) {
            // The query is a sentence wrapped around the label.
            70
        } else if label.contains(&q) {
            60
        } else if !value.is_empty() && value.contains(&q) {
            40
        } else {
            continue;
        };
        // A disabled control is rarely what was meant; keep it as a last resort
        // so the caller at least learns the button exists and is greyed out.
        let score = if el.disabled { score - 30 } else { score };
        let better = match best {
            None => true,
            Some((bs, be)) => {
                let bl = norm(&be.label).chars().count();
                let el_len = label.chars().count();
                (
                    score,
                    !el.offscreen,
                    std::cmp::Reverse(el_len),
                    std::cmp::Reverse(el.index),
                ) > (
                    bs,
                    !be.offscreen,
                    std::cmp::Reverse(bl),
                    std::cmp::Reverse(be.index),
                )
            }
        };
        if better {
            best = Some((score, el));
        }
    }
    best.map(|(_, el)| el)
}

/// Cut the element list down to something a context window can afford.
///
/// Returns the kept slice (restored to DOM order) and a note when anything was
/// dropped. The note matters: silently truncating teaches the model that the
/// element it wanted does not exist, and it stops looking.
pub fn budget_elements(
    mut elements: Vec<ElementInfo>,
    max: usize,
) -> (Vec<ElementInfo>, Option<String>) {
    let max = max.clamp(1, MAX_MAX_ELEMENTS);
    let total = elements.len();

    for el in &mut elements {
        el.label = truncate_chars(&el.label, MAX_LABEL_CHARS);
        if let Some(v) = &el.value {
            el.value = Some(truncate_chars(v, MAX_LABEL_CHARS));
        }
        if let Some(d) = &el.detail {
            el.detail = Some(truncate_chars(d, MAX_LABEL_CHARS));
        }
    }

    if total <= max {
        return (elements, None);
    }

    // On-screen first — that is what the model is being asked about — but keep
    // DOM order inside each group so the reading order still makes sense.
    let mut kept: Vec<ElementInfo> = Vec::with_capacity(max);
    for el in elements.iter().filter(|e| !e.offscreen) {
        if kept.len() == max {
            break;
        }
        kept.push(el.clone());
    }
    for el in elements.iter().filter(|e| e.offscreen) {
        if kept.len() == max {
            break;
        }
        kept.push(el.clone());
    }
    kept.sort_by_key(|e| e.index);

    let dropped = total - kept.len();
    let note = format!(
        "{dropped} of {total} interactive elements were omitted to fit the budget \
         (on-screen ones were kept first). Scroll, or raise max_elements, or target \
         by visible text — a handle you were not shown will not resolve."
    );
    (kept, Some(note))
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Char-wise, not byte-wise: a byte cut lands mid-codepoint on any CJK label
    // and panics or produces mojibake.
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ─────────────────────────────────────────────────────────────────────
// The injected script
// ─────────────────────────────────────────────────────────────────────

/// Builds `window.__harnessRefs` and returns the snapshot.
///
/// Everything here is one expression so `Runtime.evaluate` can return it by
/// value in a single round trip. Kept deliberately dumb — no framework
/// awareness, no shadow-DOM piercing — because every clever rule is one more
/// way for the list to disagree with what a click actually hits.
const SNAPSHOT_JS: &str = r#"
(function () {
  var SEL = [
    'a[href]', 'button', 'input', 'select', 'textarea', 'summary', 'label[for]',
    '[role=button]', '[role=link]', '[role=checkbox]', '[role=radio]', '[role=tab]',
    '[role=menuitem]', '[role=switch]', '[role=combobox]', '[role=textbox]',
    '[role=option]', '[role=searchbox]',
    '[contenteditable=""]', '[contenteditable="true"]',
    '[onclick]', '[tabindex]:not([tabindex="-1"])'
  ].join(',');

  function txt(v) { return (v == null ? '' : String(v)).replace(/\s+/g, ' ').trim(); }

  function labelledBy(el) {
    var ids = el.getAttribute('aria-labelledby');
    if (!ids) return '';
    return txt(ids.split(/\s+/).map(function (id) {
      var n = document.getElementById(id);
      return n ? n.innerText : '';
    }).join(' '));
  }

  function labelOf(el) {
    var byFor = '';
    try { if (el.labels && el.labels.length) byFor = txt(el.labels[0].innerText); } catch (e) {}
    var cands = [
      txt(el.getAttribute('aria-label')),
      labelledBy(el),
      txt(el.innerText),
      byFor,
      txt(el.getAttribute('placeholder')),
      txt(el.getAttribute('title')),
      txt(el.getAttribute('alt')),
      txt(el.getAttribute('value')),
      txt(el.getAttribute('name'))
    ];
    for (var i = 0; i < cands.length; i++) if (cands[i]) return cands[i];
    return '';
  }

  function roleOf(el) {
    var r = el.getAttribute('role');
    if (r) return r.toLowerCase();
    var tag = el.tagName.toLowerCase();
    if (tag === 'a') return 'link';
    if (tag === 'button') return 'button';
    if (tag === 'select') return 'select';
    if (tag === 'textarea') return 'textbox';
    if (tag === 'summary') return 'disclosure';
    if (tag === 'label') return 'label';
    if (tag === 'input') {
      var t = (el.getAttribute('type') || 'text').toLowerCase();
      if (t === 'checkbox' || t === 'radio' || t === 'file' || t === 'range' || t === 'color') return t;
      if (t === 'submit' || t === 'button' || t === 'reset' || t === 'image') return 'button';
      return 'textbox';
    }
    if (el.isContentEditable) return 'textbox';
    return 'clickable';
  }

  function detailOf(el, role) {
    var tag = el.tagName.toLowerCase();
    if (tag === 'a') {
      var h = el.getAttribute('href') || '';
      if (!h || h === '#' || h.indexOf('javascript:') === 0) return null;
      try { return '-> ' + new URL(el.href, location.href).host; } catch (e) { return null; }
    }
    if (tag === 'input' && role === 'textbox') {
      var t = (el.getAttribute('type') || 'text').toLowerCase();
      return t === 'text' ? null : 'type=' + t;
    }
    if (tag === 'select') {
      var opts = [];
      for (var i = 0; i < el.options.length && i < 12; i++) opts.push(txt(el.options[i].text));
      var more = el.options.length > 12 ? ', …' : '';
      return 'options: ' + opts.join(' | ') + more;
    }
    return null;
  }

  var refs = [];
  window.__harnessRefs = refs;
  var out = [];
  var vw = window.innerWidth, vh = window.innerHeight;
  var nodes = document.querySelectorAll(SEL);
  var seen = new Set();

  for (var i = 0; i < nodes.length; i++) {
    var el = nodes[i];
    if (seen.has(el)) continue;
    seen.add(el);
    var tag = el.tagName.toLowerCase();
    if (tag === 'input' && (el.getAttribute('type') || '').toLowerCase() === 'hidden') continue;

    var cs;
    try { cs = window.getComputedStyle(el); } catch (e) { continue; }
    if (!cs || cs.display === 'none' || cs.visibility === 'hidden') continue;
    if (parseFloat(cs.opacity || '1') === 0) continue;
    var r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) continue;

    var role = roleOf(el);
    var label = labelOf(el);
    var value = '';
    if ('value' in el && typeof el.value === 'string') value = txt(el.value);
    // A password field's contents are not something to ship to a model.
    if (tag === 'input' && (el.getAttribute('type') || '').toLowerCase() === 'password') {
      value = value ? '(' + value.length + ' characters)' : '';
    }
    // Unlabelled, valueless generic containers are noise, not affordances.
    if (!label && !value && role !== 'textbox' && role !== 'select' && role !== 'checkbox' && role !== 'radio') continue;

    var idx = refs.length;
    refs.push(el);
    out.push({
      handle: 'e' + idx,
      role: role,
      label: label,
      value: value || null,
      disabled: !!(el.disabled || el.getAttribute('aria-disabled') === 'true'),
      checked: !!el.checked,
      offscreen: !(r.top < vh && r.bottom > 0 && r.left < vw && r.right > 0),
      detail: detailOf(el, role),
      index: idx
    });
  }

  var doc = document.scrollingElement || document.documentElement;
  var maxScroll = Math.max(0, doc.scrollHeight - doc.clientHeight);
  return {
    url: location.href,
    title: document.title || '',
    elements: out,
    scroll_percent: maxScroll === 0 ? 100 : Math.round((doc.scrollTop / maxScroll) * 100),
    at_bottom: maxScroll === 0 || doc.scrollTop >= maxScroll - 2
  };
})()
"#;

/// Resolves a handle/selector to `window.__harnessTarget` and reports its
/// geometry, so Rust can decide between a real mouse event and a fallback.
fn resolve_js(kind: TargetKind, target: &str) -> String {
    let lookup = match kind {
        TargetKind::Handle => format!(
            "(window.__harnessRefs || [])[{}]",
            handle_index(target).unwrap_or(usize::MAX)
        ),
        // Both other kinds arrive here already turned into a concrete handle or
        // selector by the caller.
        TargetKind::Css | TargetKind::Text => {
            format!("document.querySelector({})", js_string(target))
        }
    };
    format!(
        r#"(function () {{
  var el = {lookup};
  if (!el) return {{ found: false }};
  window.__harnessTarget = el;
  try {{ el.scrollIntoView({{ block: 'center', inline: 'center' }}); }} catch (e) {{}}
  var r = el.getBoundingClientRect();
  var cx = r.left + r.width / 2, cy = r.top + r.height / 2;
  // Something else on top (a cookie banner, a modal) means a mouse event at
  // these coordinates would hit that instead — worth knowing before clicking.
  var top = null;
  try {{ top = document.elementFromPoint(cx, cy); }} catch (e) {{}}
  return {{
    found: true,
    tag: el.tagName.toLowerCase(),
    x: cx, y: cy, w: r.width, h: r.height,
    in_view: r.top < window.innerHeight && r.bottom > 0 && r.left < window.innerWidth && r.right > 0,
    obscured: !!(top && top !== el && !el.contains(top) && !top.contains(el)),
    label: (el.innerText || el.value || el.getAttribute('aria-label') || '').replace(/\s+/g, ' ').trim().slice(0, 80)
  }};
}})()"#
    )
}

/// JSON-encode a Rust string into a JavaScript string literal.
///
/// Concatenating a selector into a script without this is a script-injection
/// bug in a tool whose input comes from a model reading attacker-controlled
/// pages. `serde_json` produces exactly the escaping JS needs.
fn js_string(s: &str) -> String {
    Value::String(s.to_string()).to_string()
}

// ─────────────────────────────────────────────────────────────────────
// Page operations
// ─────────────────────────────────────────────────────────────────────

/// A resolved target: what to hand to `resolve_js`, plus how we got there (for
/// the trace the model sees, so a fuzzy text match is never silent).
#[derive(Debug)]
pub(crate) struct Resolved {
    pub kind: TargetKind,
    pub selector: String,
    pub note: Option<String>,
}

/// Turn the caller's `target` into something resolvable, consulting the last
/// snapshot for the visible-text case.
pub(crate) fn resolve_target(
    target: &str,
    by: Option<&str>,
    last: &[ElementInfo],
) -> Result<Resolved, PageError> {
    let kind = match by {
        Some("handle") => TargetKind::Handle,
        Some("css") => TargetKind::Css,
        Some("text") => TargetKind::Text,
        _ => classify_target(target),
    };
    match kind {
        TargetKind::Handle => {
            if handle_index(target).is_none() {
                return Err(PageError::NoSuchElement {
                    target: target.into(),
                    hint: "handles look like `e12` and come from the `elements` list of the \
                           previous response."
                        .into(),
                });
            }
            Ok(Resolved {
                kind,
                selector: target.to_string(),
                note: None,
            })
        }
        TargetKind::Css => Ok(Resolved {
            kind,
            selector: target.to_string(),
            note: None,
        }),
        TargetKind::Text => match resolve_by_text(last, target) {
            Some(el) => Ok(Resolved {
                kind: TargetKind::Handle,
                selector: el.handle.clone(),
                note: Some(format!(
                    "matched text `{target}` to {} `{}` ({})",
                    el.role, el.label, el.handle
                )),
            }),
            None => Err(PageError::NoSuchElement {
                target: target.into(),
                hint: if last.is_empty() {
                    "no page snapshot yet — open a page first.".into()
                } else {
                    format!(
                        "no visible element's text matches. {} interactive elements were listed \
                         in the previous response; use one of their handles, or a CSS selector \
                         with by=\"css\".",
                        last.len()
                    )
                },
            }),
        },
    }
}

/// Run an expression and return its value, turning a JS throw into an error
/// rather than a silently-null result.
pub(crate) async fn eval(session: &BrowserSession, expr: &str) -> Result<Value, PageError> {
    let res = session
        .call(
            "Runtime.evaluate",
            json!({
                "expression": expr,
                "returnByValue": true,
                "awaitPromise": true,
                // A page can redefine `Array.prototype.map`; running in the
                // page's own world is still required (we need its DOM), but at
                // least surface an exception instead of a bogus value.
                "userGesture": true
            }),
        )
        .await?;
    if let Some(ex) = res.get("exceptionDetails") {
        let msg = ex
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(Value::as_str)
            .or_else(|| ex.get("text").and_then(Value::as_str))
            .unwrap_or("unknown error");
        return Err(PageError::Js(msg.to_string()));
    }
    Ok(res
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// Snapshot the page: URL, title, and the interactable list.
pub(crate) async fn snapshot(
    session: &BrowserSession,
    max_elements: usize,
) -> Result<(PageSummary, Vec<ElementInfo>), PageError> {
    let v = eval(session, SNAPSHOT_JS).await?;
    let all: Vec<ElementInfo> =
        serde_json::from_value(v.get("elements").cloned().unwrap_or(json!([])))
            .map_err(|e| PageError::Js(format!("snapshot did not deserialise: {e}")))?;
    let total = all.len();
    let (kept, note) = budget_elements(all.clone(), max_elements);
    let summary = PageSummary {
        url: v
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: v
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        elements: kept,
        element_count: total,
        elements_note: note,
        scroll: Some(ScrollState {
            percent: v
                .get("scroll_percent")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(100) as u32,
            at_bottom: v.get("at_bottom").and_then(Value::as_bool).unwrap_or(true),
        }),
    };
    // The *full* list is retained in the session for text resolution, so
    // "click the button that says X" still works for an element the budget cut.
    Ok((summary, all))
}

pub(crate) async fn current_url(session: &BrowserSession) -> Result<String, PageError> {
    Ok(eval(session, "location.href")
        .await?
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// Navigate, with the policy consulted before *and* after.
///
/// The second check is the one people forget. `https://public.example/go?to=…`
/// is allowed, returns a 302 to `http://169.254.169.254/`, and the browser
/// follows it without asking anyone. Checking where we landed catches that; on
/// a violation the page is replaced with `about:blank` before any of its
/// content can be read back, so the round trip yields a refusal rather than
/// credentials.
pub(crate) async fn navigate(
    session: &BrowserSession,
    url: &str,
    policy: &Arc<dyn UrlPolicy>,
    timeout: Duration,
) -> Result<(), PageError> {
    policy.check(url)?;
    let res = session
        .call_with_timeout("Page.navigate", json!({ "url": url }), timeout)
        .await?;
    if let Some(err) = res.get("errorText").and_then(Value::as_str) {
        return Err(PageError::Io(format!("navigation failed: {err}")));
    }
    await_ready(session, timeout).await?;
    enforce_landing(session, policy).await
}

/// Re-check where the page actually ended up. Also called after a click, since
/// a click is a navigation the tool did not choose the URL for.
pub(crate) async fn enforce_landing(
    session: &BrowserSession,
    policy: &Arc<dyn UrlPolicy>,
) -> Result<(), PageError> {
    let landed = current_url(session).await?;
    // about: and blob: are the browser's own, never a network target.
    if landed.is_empty() || landed.starts_with("about:") || landed.starts_with("blob:") {
        return Ok(());
    }
    if let Err(denied) = policy.check(&landed) {
        let _ = session
            .call_with_timeout(
                "Page.navigate",
                json!({ "url": "about:blank" }),
                Duration::from_secs(5),
            )
            .await;
        return Err(PageError::Policy(PolicyDenied {
            url: denied.url,
            reason: format!(
                "{} — the page redirected there. The tab has been reset to about:blank and no \
                 content was read.",
                denied.reason
            ),
        }));
    }
    Ok(())
}

/// Poll `document.readyState` rather than racing a `Page.loadEventFired`
/// subscription: by the time we could subscribe, a cached page may already have
/// fired it, and a missed event is an unconditional timeout.
pub(crate) async fn await_ready(
    session: &BrowserSession,
    timeout: Duration,
) -> Result<(), PageError> {
    let deadline = Instant::now() + timeout;
    loop {
        let state = eval(session, "document.readyState").await.ok();
        if state.as_ref().and_then(Value::as_str) == Some("complete") {
            // Frameworks mount after `complete`; a short settle costs little
            // and is the difference between an empty React root and a page.
            tokio::time::sleep(Duration::from_millis(250)).await;
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Not an error: a page that never finishes loading (long-poll,
            // video) is still readable, and failing here would make those
            // sites permanently unusable.
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Rendered, visible text — `innerText`, not a tag-stripped DOM dump.
///
/// `innerText` is what the layout engine decided is on screen: it honours
/// `display:none`, collapses whitespace the way CSS does, and inserts the line
/// breaks a reader sees. Stripping tags from `--dump-dom` output, which is what
/// the one-shot implementation had to do, keeps hidden menus, `aria-hidden`
/// scaffolding and every inline SVG label.
pub(crate) async fn read_text(
    session: &BrowserSession,
    max_chars: usize,
) -> Result<(String, bool), PageError> {
    let v = eval(
        session,
        "(document.body && document.body.innerText) || document.documentElement.innerText || ''",
    )
    .await?;
    let text = v.as_str().unwrap_or_default();
    let full = text.chars().count();
    if full <= max_chars {
        return Ok((text.to_string(), false));
    }
    Ok((text.chars().take(max_chars).collect(), true))
}

pub(crate) async fn screenshot_png(session: &BrowserSession) -> Result<Vec<u8>, PageError> {
    let res = session
        .call_with_timeout(
            "Page.captureScreenshot",
            json!({ "format": "png", "captureBeyondViewport": false }),
            Duration::from_secs(30),
        )
        .await?;
    let b64 = res
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| PageError::Io("screenshot returned no data".into()))?;
    harness_core::b64::base64_decode(b64)
        .map_err(|e| PageError::Io(format!("screenshot is not valid base64: {e}")))
}

/// Geometry of a resolved target.
pub(crate) struct TargetBox {
    pub x: f64,
    pub y: f64,
    pub in_view: bool,
    pub obscured: bool,
    pub tag: String,
}

pub(crate) async fn focus_target(
    session: &BrowserSession,
    resolved: &Resolved,
) -> Result<TargetBox, PageError> {
    let v = eval(session, &resolve_js(resolved.kind, &resolved.selector)).await?;
    if !v.get("found").and_then(Value::as_bool).unwrap_or(false) {
        return Err(PageError::NoSuchElement {
            target: resolved.selector.clone(),
            hint: match resolved.kind {
                TargetKind::Handle => {
                    "that handle is not in the current page. Handles are invalidated by every \
                     navigation and by every action — use one from the most recent `elements` list."
                        .into()
                }
                _ => "the CSS selector matched nothing. Check the `elements` list in the previous \
                      response, or target by visible text."
                    .into(),
            },
        });
    }
    Ok(TargetBox {
        x: v.get("x").and_then(Value::as_f64).unwrap_or(0.0),
        y: v.get("y").and_then(Value::as_f64).unwrap_or(0.0),
        in_view: v.get("in_view").and_then(Value::as_bool).unwrap_or(false),
        obscured: v.get("obscured").and_then(Value::as_bool).unwrap_or(false),
        tag: v
            .get("tag")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Click, preferring a real mouse event.
///
/// `element.click()` is one line and works on most buttons. It also does not
/// fire `mouseover`/`mousedown`, which is how hover menus open, how drag
/// handles arm, and how a good deal of analytics-driven UI decides a click was
/// human. So: dispatch genuine `Input` events at the element's centre, and fall
/// back to the DOM call only when the element cannot be brought on screen.
pub(crate) async fn click(
    session: &BrowserSession,
    resolved: &Resolved,
) -> Result<String, PageError> {
    let bx = focus_target(session, resolved).await?;
    if !bx.in_view {
        eval(
            session,
            "(function(){ var e = window.__harnessTarget; if (e) { e.click(); return true; } return false; })()",
        )
        .await?;
        return Ok(format!(
            "clicked <{}> via the DOM (it could not be scrolled into view)",
            bx.tag
        ));
    }
    let base = json!({ "x": bx.x, "y": bx.y, "button": "left", "clickCount": 1 });
    let mut moved = base.clone();
    moved["type"] = json!("mouseMoved");
    moved["clickCount"] = json!(0);
    session.call("Input.dispatchMouseEvent", moved).await?;
    let mut down = base.clone();
    down["type"] = json!("mousePressed");
    session.call("Input.dispatchMouseEvent", down).await?;
    let mut up = base;
    up["type"] = json!("mouseReleased");
    session.call("Input.dispatchMouseEvent", up).await?;

    Ok(if bx.obscured {
        format!(
            "clicked at the centre of <{}>, but another element overlaps that point — if nothing \
             changed, dismiss the overlay (cookie banner / modal) first",
            bx.tag
        )
    } else {
        format!("clicked <{}>", bx.tag)
    })
}

/// Type into the focused element.
///
/// `Input.insertText` rather than a synthesised keystroke per character: it is
/// one round trip instead of N, and it does not need a keycode table — which
/// matters because there is no keycode for 登, and a per-character
/// `dispatchKeyEvent` simply cannot enter CJK text.
pub(crate) async fn type_text(
    session: &BrowserSession,
    resolved: &Resolved,
    text: &str,
    clear: bool,
    submit: bool,
) -> Result<String, PageError> {
    focus_target(session, resolved).await?;
    let clear_js = if clear {
        // Setting `.value` alone leaves React's shadow state stale; the input
        // event is what tells a controlled component the field changed.
        "if ('value' in e) { e.value = ''; e.dispatchEvent(new Event('input', {bubbles:true})); }"
    } else {
        ""
    };
    eval(
        session,
        &format!(
            "(function(){{ var e = window.__harnessTarget; if(!e) return false; \
             try {{ e.focus(); }} catch(err) {{}} {clear_js} return true; }})()"
        ),
    )
    .await?;
    session
        .call("Input.insertText", json!({ "text": text }))
        .await?;
    let mut what = format!("typed {} characters", text.chars().count());
    if submit {
        for kind in ["keyDown", "keyUp"] {
            session
                .call(
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": kind,
                        "key": "Enter",
                        "code": "Enter",
                        "windowsVirtualKeyCode": 13,
                        "nativeVirtualKeyCode": 13,
                        "text": "\r"
                    }),
                )
                .await?;
        }
        what.push_str(" and pressed Enter");
    }
    Ok(what)
}

/// Choose an option in a `<select>`, by value or by the text a person reads.
pub(crate) async fn select_option(
    session: &BrowserSession,
    resolved: &Resolved,
    wanted: &str,
) -> Result<String, PageError> {
    focus_target(session, resolved).await?;
    let v = eval(
        session,
        &format!(
            r#"(function () {{
  var e = window.__harnessTarget;
  if (!e || !e.options) return {{ ok: false, why: 'not a <select>' }};
  var want = {want};
  var norm = function (s) {{ return String(s == null ? '' : s).replace(/\s+/g, ' ').trim().toLowerCase(); }};
  var w = norm(want);
  var hit = -1;
  for (var i = 0; i < e.options.length; i++) {{
    if (norm(e.options[i].value) === w || norm(e.options[i].text) === w) {{ hit = i; break; }}
  }}
  if (hit < 0) for (var j = 0; j < e.options.length; j++) {{
    if (norm(e.options[j].text).indexOf(w) >= 0) {{ hit = j; break; }}
  }}
  if (hit < 0) {{
    var all = [];
    for (var k = 0; k < e.options.length && k < 25; k++) all.push(e.options[k].text);
    return {{ ok: false, why: 'no matching option', options: all }};
  }}
  e.selectedIndex = hit;
  e.dispatchEvent(new Event('input', {{ bubbles: true }}));
  e.dispatchEvent(new Event('change', {{ bubbles: true }}));
  return {{ ok: true, chosen: e.options[hit].text, value: e.options[hit].value }};
}})()"#,
            want = js_string(wanted)
        ),
    )
    .await?;
    if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(format!(
            "selected `{}`",
            v.get("chosen").and_then(Value::as_str).unwrap_or(wanted)
        ));
    }
    let why = v.get("why").and_then(Value::as_str).unwrap_or("failed");
    let opts = v
        .get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .unwrap_or_default();
    Err(PageError::NoSuchElement {
        target: wanted.to_string(),
        hint: if opts.is_empty() {
            why.to_string()
        } else {
            format!("{why}; available options: {opts}")
        },
    })
}

/// Scroll the page or bring an element into view.
pub(crate) async fn scroll(
    session: &BrowserSession,
    direction: &str,
    amount: Option<f64>,
) -> Result<String, PageError> {
    let expr = match direction {
        "top" => "window.scrollTo({top: 0}); 'top'".to_string(),
        "bottom" => "window.scrollTo({top: document.body.scrollHeight}); 'bottom'".to_string(),
        "up" | "down" => {
            // Default a page at a time, minus a little overlap so nothing falls
            // between two screens unread.
            let sign = if direction == "up" { -1.0 } else { 1.0 };
            match amount {
                Some(px) => format!("window.scrollBy(0, {}); '{direction}'", sign * px.abs()),
                None => {
                    format!("window.scrollBy(0, {sign} * window.innerHeight * 0.85); '{direction}'")
                }
            }
        }
        other => {
            return Err(PageError::Io(format!(
                "unknown scroll direction `{other}`; use up, down, top or bottom"
            )));
        }
    };
    eval(session, &expr).await?;
    // Smooth-scroll CSS and lazy loaders both need a beat.
    tokio::time::sleep(Duration::from_millis(350)).await;
    Ok(format!("scrolled {direction}"))
}

/// Poll until something appears. Used for SPA transitions, where "the page
/// loaded" and "the content exists" are minutes apart.
pub(crate) async fn wait_for(
    session: &BrowserSession,
    target: &str,
    by: Option<&str>,
    timeout: Duration,
) -> Result<String, PageError> {
    let kind = match by {
        Some("css") => TargetKind::Css,
        Some("text") => TargetKind::Text,
        // For waiting, a handle is meaningless (the element already existed),
        // so `auto` chooses only between selector and text.
        _ => match classify_target(target) {
            TargetKind::Css => TargetKind::Css,
            _ => TargetKind::Text,
        },
    };
    let probe = match kind {
        TargetKind::Css => format!(
            "(function(){{ var e = document.querySelector({}); \
             if(!e) return false; var r = e.getBoundingClientRect(); \
             return r.width > 0 && r.height > 0; }})()",
            js_string(target)
        ),
        _ => format!(
            "(function(){{ var t = (document.body && document.body.innerText) || ''; \
             return t.replace(/\\s+/g, ' ').toLowerCase().indexOf({}) >= 0; }})()",
            js_string(&norm(target))
        ),
    };
    let started = Instant::now();
    let deadline = started + timeout;
    loop {
        if eval(session, &probe)
            .await
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return Ok(format!(
                "`{target}` appeared after {}ms",
                started.elapsed().as_millis()
            ));
        }
        if Instant::now() >= deadline {
            return Err(PageError::NoSuchElement {
                target: target.to_string(),
                hint: format!(
                    "still absent after {}s. Read the page to see what is actually there — it may \
                     have failed to load, or be behind a consent dialog.",
                    timeout.as_secs()
                ),
            });
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Go back one history entry.
///
/// Not `history.back()` in the page: that is asynchronous with no completion
/// signal, so the next command races the navigation. The history API gives us
/// the destination URL, which also means the policy gets to see it — a back
/// button can lead somewhere the policy would now refuse.
pub(crate) async fn go_back(
    session: &BrowserSession,
    policy: &Arc<dyn UrlPolicy>,
    timeout: Duration,
) -> Result<String, PageError> {
    let hist = session.call("Page.getNavigationHistory", json!({})).await?;
    let index = hist
        .get("currentIndex")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let entries = hist
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if index <= 0 {
        return Err(PageError::Io(
            "there is no previous page in this tab's history".into(),
        ));
    }
    let prev = &entries[(index - 1) as usize];
    let url = prev.get("url").and_then(Value::as_str).unwrap_or_default();
    if !url.starts_with("about:") {
        policy.check(url)?;
    }
    let id = prev
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| PageError::Io("history entry has no id".into()))?;
    session
        .call("Page.navigateToHistoryEntry", json!({ "entryId": id }))
        .await?;
    await_ready(session, timeout).await?;
    enforce_landing(session, policy).await?;
    Ok(format!("went back to {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(index: usize, role: &str, label: &str) -> ElementInfo {
        ElementInfo {
            handle: format!("e{index}"),
            role: role.into(),
            label: label.into(),
            value: None,
            disabled: false,
            checked: false,
            offscreen: false,
            detail: None,
            index,
        }
    }

    #[test]
    fn handles_parse_and_reject() {
        assert_eq!(handle_index("e0"), Some(0));
        assert_eq!(handle_index("e42"), Some(42));
        assert_eq!(handle_index("e"), None);
        assert_eq!(handle_index("x1"), None);
        assert_eq!(handle_index("e1x"), None);
        assert_eq!(handle_index("#e1"), None);
    }

    #[test]
    fn target_classification_prefers_text_over_a_bare_tag_name() {
        assert_eq!(classify_target("e7"), TargetKind::Handle);
        assert_eq!(classify_target("#login"), TargetKind::Css);
        assert_eq!(classify_target(".btn.primary"), TargetKind::Css);
        assert_eq!(classify_target("input[name=q]"), TargetKind::Css);
        assert_eq!(classify_target("form > button"), TargetKind::Css);
        assert_eq!(classify_target("a::after"), TargetKind::Css);
        // The interesting ones: plain words are labels, not selectors.
        assert_eq!(classify_target("button"), TargetKind::Text);
        assert_eq!(classify_target("Sign in"), TargetKind::Text);
        assert_eq!(classify_target("登录"), TargetKind::Text);
        assert_eq!(classify_target("Add to cart"), TargetKind::Text);
    }

    #[test]
    fn text_resolution_finds_the_obvious_match() {
        let els = vec![
            el(0, "link", "Home"),
            el(1, "button", "Sign in"),
            el(2, "button", "Sign in with Google"),
            el(3, "link", "Sign up"),
        ];
        assert_eq!(resolve_by_text(&els, "Sign in").unwrap().handle, "e1");
        // Case and whitespace are noise.
        assert_eq!(resolve_by_text(&els, "  sign   IN ").unwrap().handle, "e1");
        // A prefix match should still prefer the shorter, exact-ish label.
        assert_eq!(resolve_by_text(&els, "sign in with").unwrap().handle, "e2");
        assert!(resolve_by_text(&els, "checkout").is_none());
    }

    #[test]
    fn text_resolution_handles_a_sentence_around_the_label() {
        // The literal phrasing from the tool's own docs, in both languages.
        let els = vec![el(0, "link", "取消"), el(1, "button", "登录")];
        assert_eq!(
            resolve_by_text(&els, "the button that says 登录")
                .unwrap()
                .handle,
            "e1"
        );
        let els = vec![el(0, "button", "Cancel"), el(1, "button", "Submit order")];
        assert_eq!(
            resolve_by_text(&els, "click the Submit order button")
                .unwrap()
                .handle,
            "e1"
        );
    }

    #[test]
    fn text_resolution_prefers_enabled_and_onscreen() {
        let mut disabled = el(0, "button", "Continue");
        disabled.disabled = true;
        let enabled = el(1, "button", "Continue");
        assert_eq!(
            resolve_by_text(&[disabled.clone(), enabled], "Continue")
                .unwrap()
                .handle,
            "e1"
        );
        // …but a disabled element is still better than nothing, so the model
        // learns why its click will not work.
        assert_eq!(
            resolve_by_text(&[disabled], "Continue").unwrap().handle,
            "e0"
        );

        let mut off = el(0, "link", "Docs");
        off.offscreen = true;
        let on = el(1, "link", "Docs");
        assert_eq!(resolve_by_text(&[off, on], "Docs").unwrap().handle, "e1");
    }

    #[test]
    fn text_resolution_can_match_a_fields_current_value() {
        let mut input = el(0, "textbox", "");
        input.value = Some("hello@example.com".into());
        assert_eq!(
            resolve_by_text(&[input], "hello@example.com")
                .unwrap()
                .handle,
            "e0"
        );
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(resolve_by_text(&[el(0, "button", "OK")], "   ").is_none());
    }

    #[test]
    fn budget_keeps_everything_when_it_fits() {
        let els: Vec<_> = (0..10).map(|i| el(i, "link", "x")).collect();
        let (kept, note) = budget_elements(els, DEFAULT_MAX_ELEMENTS);
        assert_eq!(kept.len(), 10);
        assert!(note.is_none());
    }

    #[test]
    fn budget_truncates_a_three_hundred_element_page() {
        let els: Vec<_> = (0..300)
            .map(|i| el(i, "link", &format!("row {i}")))
            .collect();
        let (kept, note) = budget_elements(els, DEFAULT_MAX_ELEMENTS);
        assert_eq!(kept.len(), DEFAULT_MAX_ELEMENTS);
        let note = note.expect("a truncated list must say so");
        assert!(note.contains("240 of 300"), "unhelpful note: {note}");
        // DOM order is preserved in what survives.
        let idx: Vec<_> = kept.iter().map(|e| e.index).collect();
        let mut sorted = idx.clone();
        sorted.sort_unstable();
        assert_eq!(idx, sorted);
    }

    #[test]
    fn budget_keeps_onscreen_elements_first() {
        // 100 offscreen elements at the top of the document, 5 visible below.
        let mut els: Vec<ElementInfo> = (0..100)
            .map(|i| {
                let mut e = el(i, "link", "hidden");
                e.offscreen = true;
                e
            })
            .collect();
        els.extend((100..105).map(|i| el(i, "button", "visible")));
        let (kept, _) = budget_elements(els, 10);
        assert_eq!(kept.len(), 10);
        let visible = kept.iter().filter(|e| !e.offscreen).count();
        assert_eq!(
            visible, 5,
            "all on-screen elements must survive before any off-screen one"
        );
    }

    #[test]
    fn budget_clamps_an_absurd_request() {
        let els: Vec<_> = (0..500).map(|i| el(i, "link", "x")).collect();
        let (kept, note) = budget_elements(els.clone(), usize::MAX);
        assert_eq!(kept.len(), MAX_MAX_ELEMENTS);
        assert!(note.is_some());
        // Zero is meaningless; one element is the floor.
        let (kept, _) = budget_elements(els, 0);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn budget_truncates_long_labels_on_a_character_boundary() {
        let mut e = el(0, "link", &"登".repeat(300));
        e.value = Some("v".repeat(300));
        let (kept, _) = budget_elements(vec![e], 10);
        assert_eq!(kept[0].label.chars().count(), MAX_LABEL_CHARS + 1); // + the ellipsis
        assert!(kept[0].label.ends_with('…'));
        assert!(kept[0].label.starts_with('登'));
        assert_eq!(
            kept[0].value.as_ref().unwrap().chars().count(),
            MAX_LABEL_CHARS + 1
        );
    }

    #[test]
    fn resolve_target_maps_text_to_a_handle_and_says_so() {
        let last = vec![el(0, "button", "Log in")];
        let r = resolve_target("log in", None, &last).unwrap();
        assert_eq!(r.kind, TargetKind::Handle);
        assert_eq!(r.selector, "e0");
        assert!(r.note.unwrap().contains("Log in"));
    }

    #[test]
    fn resolve_target_explains_a_miss_instead_of_guessing() {
        let last = vec![el(0, "button", "Log in")];
        match resolve_target("Checkout", None, &last) {
            Err(PageError::NoSuchElement { hint, .. }) => {
                assert!(hint.contains("1 interactive elements"), "{hint}");
            }
            other => panic!("expected a miss, got {other:?}"),
        }
        match resolve_target("Checkout", None, &[]) {
            Err(PageError::NoSuchElement { hint, .. }) => {
                assert!(hint.contains("open a page first"), "{hint}")
            }
            other => panic!("expected a miss, got {other:?}"),
        }
    }

    #[test]
    fn by_overrides_the_guess() {
        let last = vec![el(0, "button", "button")];
        // `by: css` forces the bare tag to be read as a selector…
        let r = resolve_target("button", Some("css"), &last).unwrap();
        assert_eq!(r.kind, TargetKind::Css);
        // …and `by: text` forces `#thing` to be read as a label.
        let last = vec![el(0, "link", "#thing")];
        let r = resolve_target("#thing", Some("text"), &last).unwrap();
        assert_eq!(r.kind, TargetKind::Handle);
        assert_eq!(r.selector, "e0");
    }

    #[test]
    fn selectors_are_escaped_into_the_injected_script() {
        // A selector that would otherwise close the string literal and run code.
        let nasty = r#"a"); fetch("http://169.254.169.254"); ("#;
        let js = resolve_js(TargetKind::Css, nasty);
        assert!(
            !js.contains(r#"a"); fetch"#),
            "the raw payload leaked into the script: {js}"
        );
        assert!(
            js.contains(r#"\"); fetch"#),
            "expected escaped form in {js}"
        );
        // Newlines too — an unescaped one breaks the expression apart.
        assert!(js_string("a\nb").contains("\\n"));
    }

    #[test]
    fn a_handle_target_indexes_the_ref_array() {
        let js = resolve_js(TargetKind::Handle, "e12");
        assert!(js.contains("window.__harnessRefs || [])[12]"));
    }

    #[test]
    fn element_json_omits_empty_fields() {
        let e = el(3, "link", "Home");
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["handle"], "e3");
        // Defaults must not cost tokens on every one of 60 rows.
        assert!(v.get("value").is_none());
        assert!(v.get("disabled").is_none());
        assert!(v.get("checked").is_none());
        assert!(v.get("offscreen").is_none());
        assert!(v.get("detail").is_none());
        assert!(v.get("index").is_none(), "index is internal");
    }
}
