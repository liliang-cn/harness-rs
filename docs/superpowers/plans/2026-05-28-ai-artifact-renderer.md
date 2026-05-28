# AI Artifact Renderer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the dashboard chat assistant return a self-contained React page bound to the user's project data, rendered live in a sandboxed full-screen preview.

**Architecture:** The model calls a new `render_artifact` tool with `{title, data:{source,id}, code}`. The backend streams it to the client as a new `artifact` SSE event and persists it on the assistant message (new `chat_messages.artifacts` JSON column) so it survives reload. The frontend shows a compact card in the chat bubble; clicking opens a full-screen overlay that fetches the data via `ledgerApi` (host holds the token), transpiles the code with sucrase, renders it in an `<iframe sandbox="allow-scripts">` (opaque origin — no token, no same-origin API access), and injects the data via `postMessage` (`window.DATA`). React + recharts load in the iframe via an esm.sh import map.

**Tech Stack:** Rust (axum, rusqlite, harness-rs `#[tool]`/Hook), React 19 + Vite + TypeScript, `sucrase` (host-side JSX transpile), esm.sh (in-iframe modules).

**Spec:** `docs/superpowers/specs/2026-05-28-ai-artifact-renderer-design.md`

**Note on testing:** the Rust crate has `cargo test`; the `user-ui` has **no JS unit-test runner**. Frontend tasks are verified with `npx tsc --noEmit` + `npm run build`, and the whole feature with a Playwright golden-path in Task 8. Don't add a JS test runner — it's out of scope.

---

## File Structure

**Backend (`examples/dashboard/src`):**
- `db.rs` — add `artifacts` column (migration), thread through `append_chat_message` + `get_chat_messages`.
- `model.rs` — `ChatMessage` gains `artifacts: serde_json::Value`.
- `tools.rs` — new `render_artifact` tool.
- `server.rs` — `ChannelHook` gains an artifacts collector + emits the `artifact` SSE event; the stream task persists collected artifacts.
- `main.rs` — `SYSTEM_PROMPT` gains an artifact section.

**Frontend (`examples/dashboard/user-ui/src`):**
- `lib/artifact.ts` — `ArtifactSpec` type + `fetchArtifactData` data-source registry.
- `lib/artifact-sandbox.ts` — `transpile` (sucrase) + `buildSrcdoc`.
- `components/chat/artifact-card.tsx` — compact in-bubble card.
- `components/chat/artifact-view.tsx` — full-screen sandbox overlay (lazy-loaded).
- `components/chat/stream.ts` — map the `artifact` SSE event.
- `components/chat/chat-sheet.tsx` — collect artifacts into the committed message.
- `components/chat/message-list.tsx` — render a message's artifact cards.
- `lib/api.ts` — `ChatMessage` gains `artifacts?: ArtifactSpec[]`.
- `locales/en.json`, `locales/zh.json` — artifact strings.
- `package.json` — add `sucrase`.

---

## Task 1: DB — persist artifacts on chat messages

**Files:**
- Modify: `examples/dashboard/src/db.rs` (migration ~line 406; `get_chat_messages` ~1846; `append_chat_message` ~1888)
- Modify: `examples/dashboard/src/model.rs` (`ChatMessage` ~line 158)
- Test: `examples/dashboard/src/db.rs` (add a `#[test]`)

- [ ] **Step 1: Add the `artifacts` field to `ChatMessage`**

In `model.rs`, inside `pub struct ChatMessage` (after the `attachment_ids` field, before the closing `}`):

```rust
    /// Artifacts the assistant emitted in this turn (render_artifact tool
    /// args). JSON array; empty for turns without artifacts. Stored so the
    /// chat re-hydrates artifact cards on reload.
    #[serde(default)]
    pub artifacts: serde_json::Value,
```

- [ ] **Step 2: Add the column migration**

In `db.rs`, in the `ensure_column` block (right after the `chat_messages`/`attachment_ids` line ~406):

```rust
        // JSON array of render_artifact specs the assistant emitted. NULL for
        // turns/rows without artifacts.
        self.ensure_column("chat_messages", "artifacts", "TEXT")?;
```

- [ ] **Step 3: Read the column in `get_chat_messages`**

In `db.rs` `get_chat_messages`, change the SELECT to include `artifacts`:

```rust
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, role, text, iters, created_at, attachment_ids, artifacts
             FROM chat_messages
             WHERE user_id = ?1 AND session_id = ?2
             ORDER BY created_at ASC
             LIMIT ?3",
        )?;
```

And in the row mapping closure, after the `att` block, add and include in the struct literal:

```rust
                let artifacts: serde_json::Value = match r.get::<_, Option<String>>(7)? {
                    Some(s) => serde_json::from_str(&s).unwrap_or(serde_json::Value::Array(vec![])),
                    None => serde_json::Value::Array(vec![]),
                };
                Ok(ChatMessage {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    role: r.get(2)?,
                    text: r.get(3)?,
                    iters: r.get::<_, Option<i64>>(4)?.map(|n| n as u32),
                    created_at: parse_rfc3339(&created_s),
                    attachment_ids: att,
                    artifacts,
                })
```

- [ ] **Step 4: Accept artifacts in `append_chat_message`**

Change the signature and INSERT in `db.rs` `append_chat_message`:

```rust
    pub fn append_chat_message(
        &self,
        user_id: &str,
        session_id: &str,
        role: &str,
        text: &str,
        iters: Option<u32>,
        attachment_ids: &[String],
        artifacts: Option<&str>,
    ) -> SqlResult<String> {
```

Then update the INSERT to add the column + bind:

```rust
        self.conn.execute(
            "INSERT INTO chat_messages(
                id, session_id, user_id, role, text, iters, created_at, attachment_ids, artifacts
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, session_id, user_id, role, text, iters.map(|n| n as i64), now, att_json, artifacts],
        )?;
```

- [ ] **Step 5: Fix the existing `append_chat_message` call sites**

`server.rs` calls it in 3 places (the user-message save ~line 1757, the Done save ~1900, the BudgetExhausted save ~1935) and possibly the legacy `chat_stream_handler`. For NOW pass `None` as the new last arg at every call site so it compiles (Task 3 wires the real value into the two assistant-save sites). Run:

```bash
cd /Users/liliang/Things/courses/harness && grep -rn "append_chat_message(" examples/dashboard/src
```
Add `None,` as the final argument to each call.

- [ ] **Step 6: Write the round-trip test**

Add to `db.rs` test module (find `#[cfg(test)]` and add a test):

```rust
    #[test]
    fn chat_message_artifacts_round_trip() {
        // FK constraints are not enforced (no PRAGMA foreign_keys), so a bare
        // session row is enough — no user row needed.
        let db = Db::open_in_memory().unwrap();
        let uid = "u_test";
        let sid = "s_test";
        db.create_chat_session(uid, sid, None).unwrap();
        let spec = r#"[{"title":"T","data":{"source":"project","id":"p1"},"code":"function App(){return null}"}]"#;
        db.append_chat_message(uid, sid, "asst", "hi", Some(1), &[], Some(spec)).unwrap();
        let msgs = db.get_chat_messages(uid, sid, 10).unwrap();
        let last = msgs.last().unwrap();
        assert_eq!(
            last.artifacts,
            serde_json::from_str::<serde_json::Value>(spec).unwrap()
        );
    }
```

(Signatures confirmed: `create_chat_session(user_id, id, model_id: Option<&str>) -> SqlResult<()>`.)

- [ ] **Step 7: Run the test**

Run: `cd /Users/liliang/Things/courses/harness && cargo test -p dashboard chat_message_artifacts_round_trip`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add examples/dashboard/src/db.rs examples/dashboard/src/model.rs examples/dashboard/src/server.rs
git commit -m "feat(dashboard): persist artifacts JSON on chat_messages"
```

---

## Task 2: `render_artifact` tool

**Files:**
- Modify: `examples/dashboard/src/tools.rs` (add the tool near the other project tools)
- Test: `examples/dashboard/src/tools.rs` (add `#[test]`s if a tool-test harness exists; otherwise validation is covered by the build + Task 8)

- [ ] **Step 1: Add the tool**

Append to `tools.rs`:

```rust
/// Render a data-bound React page to the user. Does no server-side rendering —
/// it validates the request and acks; the client fetches the declared data and
/// renders `code` in a sandboxed iframe. The ChannelHook turns this call into
/// an `artifact` SSE event (see server.rs).
#[harness::tool(
    name = "render_artifact",
    risk = "read-only",
    schema = r#"{
      "type": "object",
      "properties": {
        "title": { "type": "string", "description": "Short title shown on the artifact card" },
        "data": {
          "type": "object",
          "properties": {
            "source": { "type": "string", "enum": ["project"], "description": "Data source; only 'project' is supported" },
            "id": { "type": "string", "description": "The project id to bind" }
          },
          "required": ["source", "id"]
        },
        "code": { "type": "string", "description": "ONE self-contained React component named App that reads window.DATA. No React import needed (automatic JSX runtime). You may import from 'recharts' and 'react'." }
      },
      "required": ["title", "data", "code"]
    }"#
)]
async fn render_artifact(args: Value, w: &mut World) -> Result<ToolResult, ToolError> {
    let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let code = args.get("code").and_then(|v| v.as_str()).unwrap_or("");
    let data = args.get("data").cloned().unwrap_or(Value::Null);
    let source = data.get("source").and_then(|v| v.as_str()).unwrap_or("");
    let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");

    if source != "project" {
        return Err(ToolError::InvalidArgs {
            name: "render_artifact".into(),
            reason: format!("unsupported data source `{source}` (only `project` in Phase 1)"),
        });
    }
    if title.is_empty() || code.is_empty() || id.is_empty() {
        return Err(ToolError::InvalidArgs {
            name: "render_artifact".into(),
            reason: "title, code, and data.id are required".into(),
        });
    }
    let db = open_db()?;
    let uid = crate::tools::uid_of(w)?;
    if db
        .get_project(&uid, id)
        .map_err(|e| ToolError::Exec(e.to_string()))?
        .is_none()
    {
        return Err(ToolError::InvalidArgs {
            name: "render_artifact".into(),
            reason: format!("project `{id}` not found"),
        });
    }
    Ok(ToolResult {
        ok: true,
        content: json!({ "ok": true, "note": "artifact shown to the user" }),
        trace: None,
    })
}
```

- [ ] **Step 2: Verify it compiles + registers**

Run: `cd /Users/liliang/Things/courses/harness && cargo build -p dashboard 2>&1 | tail -3`
Expected: builds (warnings ok). The `#[harness::tool]` macro auto-registers via inventory — no manual wiring.

- [ ] **Step 3: Commit**

```bash
git add examples/dashboard/src/tools.rs
git commit -m "feat(dashboard): render_artifact tool (validates project source/ownership)"
```

---

## Task 3: ChannelHook artifact event + collector + persist + system prompt

**Files:**
- Modify: `examples/dashboard/src/server.rs` (`ChannelHook` ~1508; construction ~1875; save points ~1900 and ~1935)
- Modify: `examples/dashboard/src/main.rs` (`SYSTEM_PROMPT`)

- [ ] **Step 1: Give `ChannelHook` an artifacts collector**

In `server.rs`, change the struct (~1508):

```rust
struct ChannelHook {
    tx: mpsc::UnboundedSender<Value>,
    /// render_artifact specs seen this run, collected for persistence.
    artifacts: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
}
```

- [ ] **Step 2: Emit the `artifact` event for render_artifact in `fire`**

In `ChannelHook::fire`, replace the `Event::PreToolUse { action }` arm with:

```rust
            Event::PreToolUse { action } => {
                if action.tool == "render_artifact" {
                    // Collect for persistence + emit a dedicated artifact event
                    // (instead of a generic tool_start status chip).
                    if let Ok(mut v) = self.artifacts.lock() {
                        v.push(action.args.clone());
                    }
                    let mut ev = serde_json::Map::new();
                    ev.insert("type".into(), json!("artifact"));
                    if let Value::Object(args) = &action.args {
                        for (k, val) in args {
                            ev.insert(k.clone(), val.clone());
                        }
                    }
                    Some(Value::Object(ev))
                } else {
                    Some(json!({
                        "type": "tool_start",
                        "name": action.tool,
                        "args": &action.args,
                    }))
                }
            }
```

- [ ] **Step 3: Create the collector in the stream task + pass to the hook**

In `server.rs` `session_stream_handler`, inside the `tokio::spawn(async move { ... })` block, before the `ChannelHook` is constructed (the hook is added at ~1875 with `loop_ = loop_.with_hook(Arc::new(ChannelHook { tx: tx.clone() }));`), add the collector and use it:

```rust
        let artifacts_acc = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        loop_ = loop_.with_hook(Arc::new(ChannelHook {
            tx: tx.clone(),
            artifacts: artifacts_acc.clone(),
        }));
```

- [ ] **Step 4: Persist collected artifacts at both save points**

In the `Ok(Outcome::Done { .. })` arm (~1900) and the `Ok(Outcome::BudgetExhausted { .. })` arm (~1935), build the artifacts JSON before the `append_chat_message` call and pass it as the new last arg (replacing the `None` added in Task 1, Step 5 for these two sites):

```rust
                let artifacts_json = {
                    let v = artifacts_acc.lock().map(|g| g.clone()).unwrap_or_default();
                    if v.is_empty() { None } else { serde_json::to_string(&v).ok() }
                };
                let _ = db.append_chat_message(
                    &user_id_for_task,
                    &session_id_for_task,
                    "asst",
                    &reply,
                    Some(iters),
                    &[],
                    artifacts_json.as_deref(),
                );
```

(Apply to both arms. The user-message save site and the legacy `chat_stream_handler`'s ChannelHook keep `None`; for the legacy handler, also add an `artifacts` field to its `ChannelHook { tx: tx.clone() }` construction at ~2018 using a throwaway collector: `artifacts: std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))`, since the struct now requires it.)

- [ ] **Step 5: Add the SYSTEM_PROMPT section**

In `main.rs`, inside the `SYSTEM_PROMPT` string, add a rule block (match the existing numbered-rule style):

```text
ARTIFACTS — rendering a data page:
When the user asks to see/visualise a project's progress (or any project data
page), call the `render_artifact` tool. Steps: (1) resolve the project id with
`list_projects` if you only have a name; (2) call `render_artifact` with a SHORT
`title`, `data: { "source": "project", "id": <id> }`, and `code` = ONE
self-contained React component named `App` that reads its data from the global
`window.DATA`, shaped as:
  { project: {id,name,detail,status,target_date,created_at,parent_id,message_count},
    milestones: [{id,name,due_date,status,created_at}],
    reviews:    [{id,progress,next_steps,created_at,next_review_date}] }
Do NOT fetch data inside the component — it is injected. Do NOT import React for
JSX (automatic runtime). You MAY `import { useState } from 'react'` and import
charts from `recharts` (e.g. LineChart/Line/XAxis/YAxis/Tooltip). Keep it ONE
file, dependency-light. After the tool returns, write a one-line confirmation.
```

- [ ] **Step 6: Build**

Run: `cd /Users/liliang/Things/courses/harness && cargo build -p dashboard 2>&1 | tail -3`
Expected: builds clean (warnings ok).

- [ ] **Step 7: Commit**

```bash
git add examples/dashboard/src/server.rs examples/dashboard/src/main.rs
git commit -m "feat(dashboard): stream + persist render_artifact as an artifact event; prompt rule"
```

---

## Task 4: Frontend types — ArtifactSpec + data-source registry + ChatMessage field

**Files:**
- Create: `examples/dashboard/user-ui/src/lib/artifact.ts`
- Modify: `examples/dashboard/user-ui/src/lib/api.ts` (`ChatMessage` ~line 330)

- [ ] **Step 1: Create `lib/artifact.ts`**

```ts
import { ledgerApi } from '@/lib/api';

/** A page the AI asked us to render. Mirrors the render_artifact tool args. */
export interface ArtifactSpec {
  title: string;
  data: { source: string; id: string };
  code: string;
}

/** Narrow an unknown (from SSE / persisted JSON) into an ArtifactSpec. */
export function asArtifactSpec(v: unknown): ArtifactSpec | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  const data = o.data as Record<string, unknown> | undefined;
  if (
    typeof o.title === 'string' &&
    typeof o.code === 'string' &&
    data &&
    typeof data.source === 'string' &&
    typeof data.id === 'string'
  ) {
    return { title: o.title, code: o.code, data: { source: data.source, id: data.id } };
  }
  return null;
}

/** Fetch the data a spec binds to. Host-side (uses the user's token); the
 *  result is postMessage'd into the sandbox as window.DATA. Extend this
 *  registry to add sources (e.g. a macro source for the investor bot). */
export async function fetchArtifactData(spec: ArtifactSpec): Promise<unknown> {
  switch (spec.data.source) {
    case 'project':
      return await ledgerApi.project(spec.data.id);
    default:
      throw new Error(`unknown artifact data source: ${spec.data.source}`);
  }
}
```

- [ ] **Step 2: Add `artifacts` to the `ChatMessage` type**

In `lib/api.ts`, inside `export interface ChatMessage`, before the closing `}` (after `truncated?`):

```ts
  /** Artifacts (render_artifact specs) the assistant emitted this turn.
   *  Hydrated from the server on reload; appended live during streaming. */
  artifacts?: import('@/lib/artifact').ArtifactSpec[];
```

- [ ] **Step 3: Type-check**

Run: `cd /Users/liliang/Things/courses/harness/examples/dashboard/user-ui && npx tsc --noEmit`
Expected: exit 0 (no errors).

- [ ] **Step 4: Commit**

```bash
git add examples/dashboard/user-ui/src/lib/artifact.ts examples/dashboard/user-ui/src/lib/api.ts
git commit -m "feat(dashboard/ui): ArtifactSpec type + data-source registry"
```

---

## Task 5: Sandbox builder — sucrase transpile + iframe srcdoc

**Files:**
- Create: `examples/dashboard/user-ui/src/lib/artifact-sandbox.ts`
- Modify: `examples/dashboard/user-ui/package.json` (add `sucrase`)

- [ ] **Step 1: Add the `sucrase` dependency**

Run:
```bash
cd /Users/liliang/Things/courses/harness/examples/dashboard/user-ui && npm install sucrase
```
Expected: `sucrase` added to `dependencies` in `package.json`.

- [ ] **Step 2: Create `lib/artifact-sandbox.ts`**

```ts
import { transform } from 'sucrase';

// esm.sh module map for the iframe. React pinned to the host's major; recharts
// shares that React so hooks work across the boundary.
const IMPORT_MAP = {
  imports: {
    react: 'https://esm.sh/react@19.2.6',
    'react/jsx-runtime': 'https://esm.sh/react@19.2.6/jsx-runtime',
    'react-dom/client': 'https://esm.sh/react-dom@19.2.6/client',
    recharts: 'https://esm.sh/recharts@3.8.0?deps=react@19.2.6',
  },
};

/** Compile the AI's JSX/TSX to JS (automatic runtime → no React import needed).
 *  Throws on syntax errors; callers show the message instead of mounting. */
export function transpile(code: string): string {
  return transform(code, {
    transforms: ['jsx', 'typescript'],
    jsxRuntime: 'automatic',
    production: true,
  }).code;
}

/** Build a self-contained sandbox document. The iframe is rendered with
 *  sandbox="allow-scripts" and NO allow-same-origin, so this runs in an opaque
 *  origin: it cannot read the parent, localStorage, cookies, or call same-origin
 *  APIs. Data arrives only via postMessage({type:'artifact-data', data}). */
export function buildSrcdoc(code: string): string {
  const compiled = transpile(code);
  return `<!doctype html>
<html>
<head>
<meta charset="utf-8" />
<script type="importmap">${JSON.stringify(IMPORT_MAP)}</script>
<style>
  :root { color-scheme: light dark; }
  body { margin: 0; padding: 16px; font: 14px/1.5 system-ui, -apple-system, sans-serif; }
  #root { min-height: 100vh; }
  .__err { color: #b91c1c; white-space: pre-wrap; font-family: ui-monospace, monospace; font-size: 12px; }
</style>
</head>
<body>
<div id="root"></div>
<script type="module">
${compiled}
window.App = (typeof App !== 'undefined') ? App : (window.App || null);
</script>
<script type="module">
  import React from 'react';
  import { createRoot } from 'react-dom/client';
  const post = (m) => { try { parent.postMessage(m, '*'); } catch {} };
  window.onerror = (msg) => post({ type: 'artifact-error', message: String(msg) });
  const root = createRoot(document.getElementById('root'));
  function mount() {
    try {
      const C = window.App;
      // Mount via React so hooks (useState etc.) work — never call C() directly.
      root.render(C ? React.createElement(C) : null);
    } catch (e) {
      post({ type: 'artifact-error', message: String((e && e.stack) || e) });
    }
  }
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'artifact-data') {
      window.DATA = e.data.data;
      mount();
    }
  });
  post({ type: 'artifact-ready' });
</script>
</body>
</html>`;
}
```

Notes: (1) the user component **must** be a top-level `function App(...)`; it's exposed as `window.App`. (2) The user module runs **first** so `window.App` is set before the bootstrap's message handler fires. (3) The bootstrap imports React in its **own** module scope and mounts with `React.createElement(window.App)` so hooks work; the user module uses the automatic JSX runtime and needs no React import — no duplicate-binding collision. (4) The host waits for the `artifact-ready` ping before posting data (Task 6).

- [ ] **Step 3: Type-check**

Run: `cd /Users/liliang/Things/courses/harness/examples/dashboard/user-ui && npx tsc --noEmit`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add examples/dashboard/user-ui/src/lib/artifact-sandbox.ts examples/dashboard/user-ui/package.json examples/dashboard/user-ui/package-lock.json
git commit -m "feat(dashboard/ui): sandbox builder (sucrase transpile + iframe srcdoc)"
```

---

## Task 6: Artifact card + full-screen sandbox view

**Files:**
- Create: `examples/dashboard/user-ui/src/components/chat/artifact-card.tsx`
- Create: `examples/dashboard/user-ui/src/components/chat/artifact-view.tsx`

- [ ] **Step 1: Create `artifact-card.tsx`**

```tsx
import { useState, lazy, Suspense } from 'react';
import { useTranslation } from 'react-i18next';
import { LayoutDashboard, Maximize2 } from 'lucide-react';
import { Button } from '@/components/ui/button';
import type { ArtifactSpec } from '@/lib/artifact';

const ArtifactView = lazy(() =>
  import('@/components/chat/artifact-view').then((m) => ({ default: m.ArtifactView })),
);

/** Compact card shown in an assistant bubble; opens the full-screen preview. */
export function ArtifactCard({ spec }: { spec: ArtifactSpec }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="border-border hover:bg-accent mt-2 flex w-full items-center gap-2 rounded-lg border p-2.5 text-left"
      >
        <LayoutDashboard className="text-muted-foreground size-4 shrink-0" />
        <span className="min-w-0 flex-1 truncate text-sm font-medium">{spec.title}</span>
        <span className="text-muted-foreground flex items-center gap-1 text-xs">
          <Maximize2 className="size-3.5" /> {t('artifact.open')}
        </span>
      </button>
      {open && (
        <Suspense fallback={null}>
          <ArtifactView spec={spec} open={open} onOpenChange={setOpen} />
        </Suspense>
      )}
    </>
  );
}
```

- [ ] **Step 2: Create `artifact-view.tsx`**

```tsx
import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { RotateCw, X, Loader2 } from 'lucide-react';
import { Sheet, SheetContent, SheetTitle, SheetDescription } from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import { fetchArtifactData, type ArtifactSpec } from '@/lib/artifact';
import { buildSrcdoc } from '@/lib/artifact-sandbox';

interface Props {
  spec: ArtifactSpec;
  open: boolean;
  onOpenChange: (v: boolean) => void;
}

/** Full-screen sandboxed preview. Fetches data host-side (with the token),
 *  renders the AI code in an opaque-origin iframe, and injects the data via
 *  postMessage once the sandbox signals ready. */
export function ArtifactView({ spec, open, onOpenChange }: Props) {
  const { t } = useTranslation();
  const iframeRef = useRef<HTMLIFrameElement | null>(null);
  const [srcdoc, setSrcdoc] = useState<string | null>(null);
  const [data, setData] = useState<unknown>(null);
  const [status, setStatus] = useState<'loading' | 'ready' | 'error'>('loading');
  const [errorMsg, setErrorMsg] = useState('');

  const load = useCallback(async () => {
    setStatus('loading');
    setErrorMsg('');
    try {
      const d = await fetchArtifactData(spec);
      setData(d);
      // transpile can throw on bad code → caught here, shown as error.
      const doc = buildSrcdoc(spec.code);
      setSrcdoc(doc);
      setStatus('ready');
    } catch (e) {
      setErrorMsg((e as Error).message || 'failed to render');
      setStatus('error');
    }
  }, [spec]);

  useEffect(() => {
    if (open) load();
  }, [open, load]);

  // Receive ready/error signals from the sandbox; inject data on ready.
  useEffect(() => {
    function onMsg(e: MessageEvent) {
      if (e.source !== iframeRef.current?.contentWindow) return;
      const m = e.data;
      if (m?.type === 'artifact-ready') {
        iframeRef.current?.contentWindow?.postMessage({ type: 'artifact-data', data }, '*');
      } else if (m?.type === 'artifact-error') {
        setErrorMsg(String(m.message));
        setStatus('error');
      }
    }
    window.addEventListener('message', onMsg);
    return () => window.removeEventListener('message', onMsg);
  }, [data]);

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent side="right" showCloseButton={false} className="flex w-full flex-col gap-0 p-0 sm:max-w-full">
        <SheetTitle className="sr-only">{spec.title}</SheetTitle>
        <SheetDescription className="sr-only">{spec.title}</SheetDescription>
        <div className="border-border flex h-12 items-center gap-2 border-b px-3">
          <span className="min-w-0 flex-1 truncate text-sm font-medium">{spec.title}</span>
          <Button variant="ghost" size="icon-sm" aria-label={t('artifact.refresh')} onClick={load}>
            <RotateCw className="size-4" />
          </Button>
          <Button variant="ghost" size="icon-sm" aria-label={t('chat.close', { defaultValue: 'close' })} onClick={() => onOpenChange(false)}>
            <X className="size-4" />
          </Button>
        </div>
        <div className="relative flex-1 overflow-hidden">
          {status === 'loading' && (
            <div className="text-muted-foreground absolute inset-0 flex items-center justify-center gap-2 text-sm">
              <Loader2 className="size-4 animate-spin" /> {t('common.loading')}
            </div>
          )}
          {status === 'error' && (
            <div className="absolute inset-0 overflow-auto p-4">
              <p className="text-destructive mb-2 text-sm font-medium">{t('artifact.error')}</p>
              <pre className="text-muted-foreground text-xs whitespace-pre-wrap">{errorMsg}</pre>
            </div>
          )}
          {status === 'ready' && srcdoc && (
            <iframe
              ref={iframeRef}
              title={spec.title}
              sandbox="allow-scripts"
              srcDoc={srcdoc}
              className="h-full w-full border-0 bg-white"
            />
          )}
        </div>
      </SheetContent>
    </Sheet>
  );
}
```

- [ ] **Step 3: Type-check**

Run: `cd /Users/liliang/Things/courses/harness/examples/dashboard/user-ui && npx tsc --noEmit`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add examples/dashboard/user-ui/src/components/chat/artifact-card.tsx examples/dashboard/user-ui/src/components/chat/artifact-view.tsx
git commit -m "feat(dashboard/ui): artifact card + sandboxed full-screen view"
```

---

## Task 7: Wire the stream → message → card

**Files:**
- Modify: `examples/dashboard/user-ui/src/components/chat/stream.ts`
- Modify: `examples/dashboard/user-ui/src/components/chat/chat-sheet.tsx`
- Modify: `examples/dashboard/user-ui/src/components/chat/message-list.tsx`
- Modify: `examples/dashboard/user-ui/src/locales/en.json`, `examples/dashboard/user-ui/src/locales/zh.json`

- [ ] **Step 1: Map the `artifact` SSE event in `stream.ts`**

Add to the `StreamEvent` union:

```ts
  | { type: 'artifact'; spec: import('@/lib/artifact').ArtifactSpec }
```

Add a case in `mapEvent` (before `case 'iter'`):

```ts
    case 'artifact': {
      // The backend spreads the render_artifact args onto the event.
      const spec = asArtifactSpec(obj);
      return spec ? { type: 'artifact', spec } : null;
    }
```

And import the narrower at the top of `stream.ts`:

```ts
import { asArtifactSpec } from '@/lib/artifact';
```

- [ ] **Step 2: Collect artifacts in `chat-sheet.tsx` `handleSend`**

In `handleSend`, declare an accumulator near `let buf = ''`:

```ts
      const artifactsAcc: import('@/lib/artifact').ArtifactSpec[] = [];
```

Add a case in the `switch (ev.type)` (next to `'tool_start'`):

```ts
            case 'artifact':
              artifactsAcc.push(ev.spec);
              break;
```

When committing the assistant message (the `const assistant: ChatMessage = { ... }` block), add:

```ts
          artifacts: artifactsAcc,
```

(The reloaded messages already carry `artifacts` from the API — no change needed to the load path.)

- [ ] **Step 3: Render artifact cards in `message-list.tsx`**

Add the import:

```ts
import { ArtifactCard } from '@/components/chat/artifact-card';
import type { ArtifactSpec } from '@/lib/artifact';
```

Pass `artifacts` from the message into the Bubble in the `messages.map`:

```tsx
          <Bubble
            key={m.id}
            role={m.role}
            text={m.text}
            attachmentIds={m.attachment_ids}
            artifacts={m.artifacts}
            truncated={m.truncated}
            onReload={onReload}
          />
```

Add `artifacts?: ArtifactSpec[]` to the `Bubble` props type, and render them after the assistant markdown div (inside the assistant branch, after the `truncated` block / before the closing `</div>` of the bubble wrapper):

```tsx
      {(artifacts?.length ?? 0) > 0 && (
        <div className="w-full">
          {artifacts!.map((a, i) => (
            <ArtifactCard key={i} spec={a} />
          ))}
        </div>
      )}
```

- [ ] **Step 4: Add i18n strings**

In `locales/en.json`, add an `"artifact"` block (sibling of `"chat"`):

```json
  "artifact": {
    "open": "Open",
    "refresh": "Refresh",
    "error": "Couldn't render this page"
  },
```

In `locales/zh.json`:

```json
  "artifact": {
    "open": "打开",
    "refresh": "刷新",
    "error": "无法渲染此页面"
  },
```

- [ ] **Step 5: Type-check + build**

Run: `cd /Users/liliang/Things/courses/harness/examples/dashboard/user-ui && npx tsc --noEmit && npm run build 2>&1 | tail -4`
Expected: tsc exit 0; build succeeds.

- [ ] **Step 6: Commit**

```bash
git add examples/dashboard/user-ui/src/components/chat/stream.ts examples/dashboard/user-ui/src/components/chat/chat-sheet.tsx examples/dashboard/user-ui/src/components/chat/message-list.tsx examples/dashboard/user-ui/src/locales/en.json examples/dashboard/user-ui/src/locales/zh.json
git commit -m "feat(dashboard/ui): stream artifact event → bubble card; i18n"
```

---

## Task 8: Integration verification (local, real keys)

**Files:** none (verification only)

- [ ] **Step 1: Build the debug binary**

Run: `cd /Users/liliang/Things/courses/harness && cargo build -p dashboard 2>&1 | tail -3`
Expected: builds.

- [ ] **Step 2: Boot it on a random port with a temp DB + real keys**

Run (DeepSeek for chat + Gemini optional; use the real DeepSeek key the user supplies at runtime — do NOT hardcode):
```bash
cd /Users/liliang/Things/courses/harness && rm -f /tmp/dashboard-artifact.db /tmp/dashboard-artifact.db-wal /tmp/dashboard-artifact.db-shm
GEMINI_API_KEY=$GEMINI_API_KEY DEEPSEEK_API_KEY=$DEEPSEEK_API_KEY HARNESS_LEDGER_DB=/tmp/dashboard-artifact.db ./target/debug/dashboard --serve --bind 127.0.0.1 --port 6791 --tier flash
```
(Run in background.) Then start the dev server: `cd examples/dashboard/user-ui && VITE_API_TARGET=http://localhost:6791 npm run dev` (background).

- [ ] **Step 3: Golden path via Playwright (or manual browser)**

1. Open `http://localhost:5779/login`, register a user (first user = admin).
2. Create a project via chat: open the chat FAB, send `开一个项目：上线 SaaS，目标 9 月底`.
3. Ask for a page: send `给我看看 SaaS 项目的进度，用图表`.
4. **Expect:** the assistant emits an artifact → a card appears in the bubble.
5. Click the card → full-screen preview opens → renders a component reading the injected `{project,milestones,reviews}`.
6. Reopen the chat (close + reopen the FAB / reload the page) → the card re-hydrates from the persisted `artifacts`.

- [ ] **Step 4: Error path**

Ask the AI to render a deliberately broken component (or temporarily inject bad code), open it → confirm the **error overlay** shows the message and the app does not crash.

- [ ] **Step 5: Security check**

In the open artifact, confirm via DevTools that the iframe element has `sandbox="allow-scripts"` with **no** `allow-same-origin`, and that the rendered page only shows the injected data (it never reads `localStorage`/token or calls `/api`). Optionally run in the iframe console: `localStorage` → should throw / be inaccessible (opaque origin).

- [ ] **Step 6: Clean up**

```bash
lsof -ti tcp:5779 | xargs kill 2>/dev/null; lsof -ti tcp:6791 | xargs kill 2>/dev/null
rm -f /tmp/dashboard-artifact.db /tmp/dashboard-artifact.db-wal /tmp/dashboard-artifact.db-shm
```

- [ ] **Step 7: Final commit (if any verification fixes were needed)**

```bash
git add -A && git commit -m "fix(dashboard): artifact renderer verification fixes"
```

---

## Done criteria

- `cargo build -p dashboard` + `cargo test -p dashboard` green.
- `npx tsc --noEmit` + `npm run build` green.
- Golden path works: ask → card → full-screen render with injected project data → re-hydrates on reload.
- Sandbox confirmed opaque-origin (no token/localStorage/same-origin API access).
- After all tasks: use **superpowers:finishing-a-development-branch**.
