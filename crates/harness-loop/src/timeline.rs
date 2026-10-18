//! A recorded run as a self-contained HTML timeline — one file, no server, no
//! scripts or fonts fetched from anywhere. `harness trace run.jsonl --html
//! out.html` writes one; [`render_html`] is the same thing as a function.
//!
//! Each model call, tool call and compaction stage is a bar placed by when it
//! started and how long it took, so parallel tool calls overlap and the slow
//! one is obvious. A row expands to the call's details: tokens and reply for a
//! model call, arguments and result preview for a tool call.
//!
//! Tool bars come from [`SessionEvent::ToolTimed`], the interval the call
//! really ran. A log recorded before that event existed falls back to the
//! `PreTool` → `PostTool` gap, which is accurate for calls dispatched one at a
//! time and near zero for parallel ones.

use crate::replay::{SessionEvent, SessionStats};
use std::collections::HashMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Model,
    Tool,
    ToolFailed,
    Unfinished,
    Compact,
}

impl Kind {
    fn class(self) -> &'static str {
        match self {
            Kind::Model => "model",
            Kind::Tool => "tool",
            Kind::ToolFailed => "fail",
            Kind::Unfinished => "open",
            Kind::Compact => "compact",
        }
    }
}

#[derive(Debug, Clone)]
struct Bar {
    kind: Kind,
    label: String,
    note: String,
    start: i64,
    end: i64,
    details: Vec<(&'static str, String)>,
}

/// A tool call while its events are being collected.
#[derive(Default)]
struct ToolRow {
    name: String,
    args: String,
    pre: Option<i64>,
    ran: Option<(i64, i64)>,
    post: Option<(i64, bool, String)>,
}

fn bars(events: &[SessionEvent]) -> (Vec<Bar>, Vec<(i64, u32)>) {
    let mut bars = Vec::new();
    let mut iters = Vec::new();
    let mut model_open: Option<(i64, usize)> = None;
    let mut compact_open: Option<(i64, String)> = None;
    let mut tools: HashMap<String, ToolRow> = HashMap::new();
    let mut tool_order: Vec<String> = Vec::new();
    let mut model_n = 0usize;
    let last_ts = events.iter().map(ts_of).max().unwrap_or(0);

    for e in events {
        match e {
            SessionEvent::Heartbeat { ts_ms, iter } => iters.push((*ts_ms, *iter)),
            SessionEvent::PreModel {
                ts_ms, history_len, ..
            } => model_open = Some((*ts_ms, *history_len)),
            SessionEvent::PostModel { ts_ms, output } => {
                let (start, history) = model_open.take().unwrap_or((*ts_ms, 0));
                model_n += 1;
                let u = &output.usage;
                let mut details = vec![
                    (
                        "tokens in / out",
                        format!("{} / {}", u.input_tokens, u.output_tokens),
                    ),
                    ("history turns", history.to_string()),
                    ("stop", format!("{:?}", output.stop_reason)),
                ];
                if u.cached_input_tokens > 0 {
                    details.push(("cached input", u.cached_input_tokens.to_string()));
                }
                if !output.tool_calls.is_empty() {
                    let names: Vec<&str> =
                        output.tool_calls.iter().map(|c| c.name.as_str()).collect();
                    details.push(("asked for", names.join(", ")));
                }
                if let Some(t) = output.text.as_deref().filter(|t| !t.is_empty()) {
                    details.push(("reply", clip(t, 1200)));
                }
                bars.push(Bar {
                    kind: Kind::Model,
                    label: format!("model call {model_n}"),
                    note: format!("{} in · {} out", u.input_tokens, u.output_tokens),
                    start,
                    end: *ts_ms,
                    details,
                });
            }
            SessionEvent::PreTool { ts_ms, action } => {
                if !tools.contains_key(&action.call_id) {
                    tool_order.push(action.call_id.clone());
                }
                let row = tools.entry(action.call_id.clone()).or_default();
                row.name = action.tool.clone();
                row.args = clip(&action.args.to_string(), 1200);
                row.pre = Some(*ts_ms);
            }
            SessionEvent::ToolTimed {
                call_id,
                started_ms,
                finished_ms,
                ..
            } => {
                if !tools.contains_key(call_id) {
                    tool_order.push(call_id.clone());
                }
                tools.entry(call_id.clone()).or_default().ran = Some((*started_ms, *finished_ms));
            }
            SessionEvent::PostTool {
                ts_ms,
                call_id,
                result,
            } => {
                if !tools.contains_key(call_id) {
                    tool_order.push(call_id.clone());
                }
                let preview = clip(&result.content.to_string(), 1200);
                tools.entry(call_id.clone()).or_default().post = Some((*ts_ms, result.ok, preview));
            }
            SessionEvent::PreCompact { ts_ms, stage } => {
                compact_open = Some((*ts_ms, format!("{stage:?}")));
            }
            SessionEvent::PostCompact {
                ts_ms,
                stage,
                before,
                after,
            } => {
                let start = compact_open.take().map(|(t, _)| t).unwrap_or(*ts_ms);
                bars.push(Bar {
                    kind: Kind::Compact,
                    label: format!("compact {stage:?}"),
                    note: format!("−{} tokens", before.saturating_sub(*after)),
                    start,
                    end: *ts_ms,
                    details: vec![("tokens before / after", format!("{before} / {after}"))],
                });
            }
            _ => {}
        }
    }

    // A model call the log never closed: cancelled, or the log was cut short.
    if let Some((start, _)) = model_open {
        bars.push(Bar {
            kind: Kind::Unfinished,
            label: format!("model call {}", model_n + 1),
            note: "did not finish".into(),
            start,
            end: last_ts,
            details: vec![],
        });
    }
    if let Some((start, stage)) = compact_open {
        bars.push(Bar {
            kind: Kind::Unfinished,
            label: format!("compact {stage}"),
            note: "did not finish".into(),
            start,
            end: last_ts,
            details: vec![],
        });
    }

    for id in tool_order {
        let row = tools.remove(&id).unwrap_or_default();
        let name = if row.name.is_empty() {
            id.clone()
        } else {
            row.name.clone()
        };
        let (kind, start, end, note) = match (&row.post, row.ran, row.pre) {
            (Some((post, ok, _)), ran, pre) => {
                let (s, e) = ran.unwrap_or((pre.unwrap_or(*post), *post));
                let kind = if *ok { Kind::Tool } else { Kind::ToolFailed };
                (kind, s, e, if *ok { "ok" } else { "failed" })
            }
            // PreTool and nothing after it: a hook denied the call, or the run
            // was cancelled while it was in flight.
            (None, ran, pre) => {
                let (s, e) = ran.unwrap_or((pre.unwrap_or(last_ts), pre.unwrap_or(last_ts)));
                (Kind::Unfinished, s, e, "no result (denied or cancelled)")
            }
        };
        let mut details = vec![("call id", id.clone())];
        if !row.args.is_empty() {
            details.push(("args", row.args.clone()));
        }
        if let Some((_, _, preview)) = &row.post {
            details.push(("result", preview.clone()));
        }
        if row.ran.is_none() && row.post.is_some() {
            details.push((
                "timing",
                "PreTool → PostTool (log has no ToolTimed); a parallel call reads as ~0".into(),
            ));
        }
        bars.push(Bar {
            kind,
            label: name,
            note: note.into(),
            start,
            end,
            details,
        });
    }

    bars.sort_by_key(|b| (b.start, b.end));
    (bars, iters)
}

fn ts_of(e: &SessionEvent) -> i64 {
    match e {
        SessionEvent::Start { ts_ms, .. }
        | SessionEvent::PreModel { ts_ms, .. }
        | SessionEvent::PostModel { ts_ms, .. }
        | SessionEvent::PreTool { ts_ms, .. }
        | SessionEvent::PostTool { ts_ms, .. }
        | SessionEvent::ToolTimed { ts_ms, .. }
        | SessionEvent::Sensor { ts_ms, .. }
        | SessionEvent::PreCompact { ts_ms, .. }
        | SessionEvent::PostCompact { ts_ms, .. }
        | SessionEvent::Heartbeat { ts_ms, .. }
        | SessionEvent::BudgetWarning { ts_ms, .. }
        | SessionEvent::End { ts_ms } => *ts_ms,
    }
}

fn clip(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{head}… ({} chars)", s.chars().count())
    } else {
        head
    }
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn ms(d: i64) -> String {
    if d >= 10_000 {
        format!("{:.1}s", d as f64 / 1000.0)
    } else {
        format!("{d}ms")
    }
}

/// Render `events` (from [`read_session`](crate::read_session)) as one HTML
/// page. `title` heads it — the log's file name is a good choice.
pub fn render_html(events: &[SessionEvent], title: &str) -> String {
    let stats = SessionStats::from(events);
    let (bars, iters) = bars(events);
    let t0 = events.iter().map(ts_of).min().unwrap_or(0);
    let t1 = events.iter().map(ts_of).max().unwrap_or(0);
    let total = (t1 - t0).max(1) as f64;
    let pct = |t: i64| ((t - t0) as f64 / total * 100.0).clamp(0.0, 100.0);
    let failures = bars.iter().filter(|b| b.kind == Kind::ToolFailed).count();

    let mut rows = String::new();
    for b in &bars {
        let left = pct(b.start);
        let width = (pct(b.end) - left).max(0.0);
        let _ = write!(
            rows,
            r#"<details class="row {cls}"><summary><span class="label" title="{label}">{label}</span><span class="track"><span class="bar" style="left:{left:.3}%;width:{width:.3}%"></span></span><span class="dur">{dur}</span><span class="note">{note}</span></summary><dl>"#,
            cls = b.kind.class(),
            label = esc(&b.label),
            dur = ms(b.end - b.start),
            note = esc(&b.note),
        );
        let _ = write!(
            rows,
            "<dt>at</dt><dd>+{} → +{}</dd>",
            ms(b.start - t0),
            ms(b.end - t0)
        );
        for (k, v) in &b.details {
            let _ = write!(rows, "<dt>{}</dt><dd><pre>{}</pre></dd>", esc(k), esc(v));
        }
        rows.push_str("</dl></details>\n");
    }

    let mut guides = String::new();
    for (t, i) in &iters {
        let _ = write!(
            guides,
            r#"<span class="iter" style="left:{:.3}%"><b>iter {}</b></span>"#,
            pct(*t),
            i
        );
    }
    let mut axis = String::new();
    for q in 0..=4 {
        let _ = write!(
            axis,
            r#"<span style="left:{}%">{}</span>"#,
            q * 25,
            ms((t1 - t0) * q / 4)
        );
    }

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · run timeline</title>
<link rel="icon" href="data:,">
<style>
:root {{
  --bg: #ffffff; --panel: #f6f7f9; --text: #1d2330; --muted: #69707d; --line: #e3e6eb;
  --model: #3b6fd8; --tool: #2f9e6b; --fail: #d64545; --open: #a0a6b0; --compact: #c98a16;
}}
:root[data-theme="dark"] {{
  --bg: #15181e; --panel: #1d2128; --text: #e4e7ec; --muted: #9098a5; --line: #2c313a;
  --model: #6d97ec; --tool: #4cc28f; --fail: #ee6b6b; --open: #6b717c; --compact: #e0a83c;
}}
* {{ box-sizing: border-box; }}
body {{ margin: 0; background: var(--bg); color: var(--text);
  font: 14px/1.45 -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Noto Sans SC", sans-serif; }}
main {{ max-width: 1200px; margin: 0 auto; padding: 24px 16px 48px; }}
header {{ display: flex; align-items: baseline; justify-content: space-between; gap: 16px; flex-wrap: wrap; }}
h1 {{ font-size: 18px; margin: 0; word-break: break-all; }}
button {{ font: inherit; color: var(--text); background: var(--panel); border: 1px solid var(--line);
  border-radius: 6px; padding: 4px 10px; cursor: pointer; }}
.stats {{ display: flex; flex-wrap: wrap; gap: 8px 20px; margin: 14px 0 18px; color: var(--muted); }}
.stats b {{ color: var(--text); font-variant-numeric: tabular-nums; }}
.legend {{ display: flex; flex-wrap: wrap; gap: 14px; color: var(--muted); font-size: 12px; margin-bottom: 10px; }}
.legend i {{ display: inline-block; width: 10px; height: 10px; border-radius: 2px; margin-right: 5px; vertical-align: -1px; }}
.grid {{ position: relative; padding-top: 16px; }}
.axis, .guides {{ position: relative; margin-left: 200px; margin-right: 200px; }}
.axis {{ height: 18px; color: var(--muted); font-size: 11px; border-bottom: 1px solid var(--line); }}
.axis span {{ position: absolute; transform: translateX(-50%); white-space: nowrap; }}
.axis span:first-child {{ transform: none; }}
.axis span:last-child {{ transform: translateX(-100%); }}
.guides {{ position: absolute; inset: 0; pointer-events: none; }}
.iter {{ position: absolute; top: 0; bottom: 0; border-left: 1px dashed var(--line); }}
.iter b {{ position: absolute; top: 0; left: 3px; font-size: 10px; font-weight: 500; color: var(--muted); white-space: nowrap; }}
details.row {{ border-bottom: 1px solid var(--line); }}
summary {{ display: grid; grid-template-columns: 200px 1fr 70px 130px; align-items: center;
  min-height: 28px; cursor: pointer; list-style: none; }}
summary::-webkit-details-marker {{ display: none; }}
summary:hover {{ background: var(--panel); }}
.label {{ overflow: hidden; text-overflow: ellipsis; white-space: nowrap; padding-right: 10px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12.5px; }}
.track {{ position: relative; height: 14px; }}
.bar {{ position: absolute; top: 0; bottom: 0; min-width: 3px; border-radius: 3px; background: var(--c); }}
.dur {{ text-align: right; font-variant-numeric: tabular-nums; color: var(--muted); padding-right: 10px; }}
.note {{ color: var(--muted); font-size: 12px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
.model {{ --c: var(--model); }} .tool {{ --c: var(--tool); }} .fail {{ --c: var(--fail); }}
.open {{ --c: var(--open); }} .compact {{ --c: var(--compact); }}
.fail .note {{ color: var(--fail); }}
dl {{ margin: 0 0 10px 200px; padding: 10px 12px; background: var(--panel); border-radius: 6px;
  display: grid; grid-template-columns: max-content 1fr; gap: 4px 14px; font-size: 12.5px; }}
dt {{ color: var(--muted); }}
dd {{ margin: 0; min-width: 0; }}
pre {{ margin: 0; white-space: pre-wrap; word-break: break-word; font: 12px/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; }}
.empty {{ color: var(--muted); padding: 24px 0; }}
@media (max-width: 720px) {{
  summary {{ grid-template-columns: 120px 1fr 60px; }}
  .note {{ display: none; }}
  .axis, .guides {{ margin-left: 120px; margin-right: 60px; }}
  .axis span:not(:first-child):not(:last-child) {{ display: none; }}
  dl {{ margin-left: 0; }}
}}
</style>
</head>
<body>
<main>
<header>
  <h1>{title}</h1>
  <button id="theme" type="button">Dark</button>
</header>
<div class="stats">
  <span>duration <b>{duration}</b></span>
  <span>model calls <b>{model_calls}</b></span>
  <span>tool calls <b>{tool_calls}</b></span>
  <span>failed tools <b>{failures}</b></span>
  <span>tokens in / out <b>{tin} / {tout}</b></span>
  <span>iterations <b>{iters_n}</b></span>
</div>
<div class="legend">
  <span><i style="background:var(--model)"></i>model call</span>
  <span><i style="background:var(--tool)"></i>tool call</span>
  <span><i style="background:var(--fail)"></i>tool failed</span>
  <span><i style="background:var(--compact)"></i>compaction</span>
  <span><i style="background:var(--open)"></i>unfinished</span>
  <span>click a row for details</span>
</div>
<div class="grid">
<div class="axis">{axis}</div>
<div class="guides">{guides}</div>
{rows}
</div>
{empty}
</main>
<script>
(function () {{
  var root = document.documentElement, btn = document.getElementById("theme");
  btn.addEventListener("click", function () {{
    var dark = root.getAttribute("data-theme") !== "dark";
    root.setAttribute("data-theme", dark ? "dark" : "light");
    btn.textContent = dark ? "Light" : "Dark";
  }});
}})();
</script>
</body>
</html>
"##,
        title = esc(title),
        duration = ms(t1 - t0),
        model_calls = stats.model_calls,
        tool_calls = stats.tool_calls,
        tin = stats.input_tokens,
        tout = stats.output_tokens,
        iters_n = stats.iters,
        empty = if bars.is_empty() {
            r#"<p class="empty">No model or tool calls in this log.</p>"#
        } else {
            ""
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Action, ModelOutput, ToolResult};
    use serde_json::json;

    fn action(id: &str, tool: &str, args: serde_json::Value) -> Action {
        Action {
            tool: tool.into(),
            call_id: id.into(),
            args,
        }
    }

    fn result(ok: bool) -> ToolResult {
        ToolResult {
            ok,
            content: json!({"ok": ok}),
            trace: None,
        }
    }

    /// Two parallel reads (0–100 and 0–400 of a 1000ms run): both stamped
    /// after the fact by PreTool/PostTool, placed correctly by ToolTimed.
    fn parallel_log() -> Vec<SessionEvent> {
        vec![
            SessionEvent::Start {
                ts_ms: 0,
                source: "Startup".into(),
            },
            SessionEvent::Heartbeat { ts_ms: 0, iter: 0 },
            SessionEvent::PreModel {
                ts_ms: 0,
                history_len: 1,
                tools_count: 2,
            },
            SessionEvent::PostModel {
                ts_ms: 0,
                output: ModelOutput::default(),
            },
            SessionEvent::PreTool {
                ts_ms: 400,
                action: action("a", "fast", json!({})),
            },
            SessionEvent::ToolTimed {
                ts_ms: 400,
                call_id: "a".into(),
                started_ms: 0,
                finished_ms: 100,
            },
            SessionEvent::PostTool {
                ts_ms: 400,
                call_id: "a".into(),
                result: result(true),
            },
            SessionEvent::PreTool {
                ts_ms: 400,
                action: action("b", "slow", json!({"q": "<script>alert(1)</script>"})),
            },
            SessionEvent::ToolTimed {
                ts_ms: 400,
                call_id: "b".into(),
                started_ms: 0,
                finished_ms: 400,
            },
            SessionEvent::PostTool {
                ts_ms: 400,
                call_id: "b".into(),
                result: result(false),
            },
            SessionEvent::End { ts_ms: 1000 },
        ]
    }

    #[test]
    fn parallel_calls_are_drawn_where_they_ran() {
        let html = render_html(&parallel_log(), "run.jsonl");
        assert!(html.contains("left:0.000%;width:10.000%"), "fast: 0–100ms");
        assert!(html.contains("left:0.000%;width:40.000%"), "slow: 0–400ms");
        assert!(
            html.contains(r#"class="row fail""#),
            "the failed one is marked"
        );
    }

    #[test]
    fn a_log_without_tool_timing_falls_back_to_pre_and_post() {
        let log: Vec<SessionEvent> = parallel_log()
            .into_iter()
            .filter(|e| !matches!(e, SessionEvent::ToolTimed { .. }))
            .collect();
        let html = render_html(&log, "old.jsonl");
        assert!(html.contains("left:40.000%;width:0.000%"), "{html}");
        assert!(html.contains("log has no ToolTimed"));
    }

    #[test]
    fn recorded_content_cannot_inject_markup() {
        let html = render_html(&parallel_log(), "<b>title</b>");
        assert!(!html.contains("<script>alert"), "args are escaped");
        assert!(html.contains("&lt;script&gt;alert"));
        assert!(!html.contains("<b>title</b>"));
    }

    #[test]
    fn the_page_is_self_contained() {
        let html = render_html(&parallel_log(), "run");
        assert!(!html.contains("http://") && !html.contains("https://"));
    }

    #[test]
    fn a_denied_call_is_shown_unfinished() {
        let log = vec![
            SessionEvent::Start {
                ts_ms: 0,
                source: "Startup".into(),
            },
            SessionEvent::PreTool {
                ts_ms: 10,
                action: action("x", "shell_exec", json!({"cmd": "rm -rf /"})),
            },
            SessionEvent::End { ts_ms: 20 },
        ];
        let html = render_html(&log, "run");
        assert!(html.contains(r#"class="row open""#));
        assert!(html.contains("denied or cancelled"));
    }
}
