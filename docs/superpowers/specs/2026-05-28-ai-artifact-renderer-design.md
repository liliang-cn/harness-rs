# AI Artifact Renderer — data-driven pages (Phase 1)

**Date:** 2026-05-28
**Status:** design, pending user review
**Part of:** the Dashboard product. This is **Spec 1** (the rendering substrate).
The **investor bot** (macro indicators: sovereign bond yields, CPI, etc. with
charts + trend analysis) is **Spec 2** — a separate sub-project *inside the same
dashboard* that plugs a macro-data source into this renderer.

## Vision

The chat assistant can return a **self-contained React page bound to the user's
own dashboard data** — rendered live in a **sandboxed, token-isolated**
full-screen preview. The motivating case: ask "看看 SaaS 项目的进度" and get a
generated progress page (milestones, reviews, target date) drawn from
`/api/projects/:id`, with the data always fresh on each open.

Not every page needs hard-coded React; the AI composes ad-hoc data views on
demand.

## Decisions locked (from brainstorming)

1. **Scope:** data-driven pages, designed/proven against the **`project`** source
   first. The renderer itself is data-source-agnostic.
2. **Data model — host injects:** the host app (which holds the auth token)
   fetches the data via the existing `ledgerApi`, then `postMessage`s it into the
   sandbox. The sandbox **never** sees the token or calls `/api` directly.
3. **Persistence:** in-chat — the artifact re-renders when the chat reopens, with
   the host re-injecting current data (always fresh). No separate save/route.
4. **Layout:** a compact **card** in the assistant bubble → click → **full-screen
   preview** overlay (mobile-friendly). Side-by-side workspace is a later option.
5. **Renderer:** **DIY** — host-side `sucrase` transpile (JSX→JS) + a sandboxed
   `<iframe srcdoc sandbox="allow-scripts">` (no `allow-same-origin`); React and
   any imported libs load via an **esm.sh import map**. Preview-only (no editor).
6. **Delivery:** the AI emits artifacts via a **`render_artifact` tool call**
   (not a fenced text block). This requires persisting the tool's args so the
   artifact survives a chat reload (see Persistence).

## Non-goals (Phase 1)

- The **investor bot** / macro-data source (Spec 2, same dashboard).
- Additional data sources beyond `project` (transactions, net-worth, macro).
- **Multi-file** artifacts, an in-sandbox **code editor**, or **pinning**
  artifacts as standalone routes/saved-views.
- Server-side execution of the generated code (it only runs client-side in the
  sandbox).

## The `window.DATA` contract (project source)

The sandbox receives, via `postMessage`, exactly the shape `GET /api/projects/:id`
returns today:

```ts
window.DATA = {
  project:   { id, name, detail, status, target_date, created_at, parent_id, message_count },
  milestones: Array<{ id, name, due_date, status, created_at }>,
  reviews:    Array<{ id, progress, next_steps, created_at, next_review_date }>,
}
```

The AI writes a component reading `window.DATA`; it does **not** fetch data
itself. It uses the existing `list_projects` tool only to resolve a name → `id`.

## Backend (Rust, `examples/dashboard/src`)

### `render_artifact` tool (`tools.rs`)
A `#[tool]` (auto-registered via inventory) the model calls to emit an artifact.
It performs **no rendering** server-side — it validates the request and returns a
short ack so the model knows it succeeded. Args (Gemini-safe schema; only
`title`, `code`, `data` required, `data` = `{ source, id }`):

```jsonc
render_artifact({
  "title": "SaaS 进度",
  "data":  { "source": "project", "id": "<projId>" },
  "code":  "function App(){ const {project,milestones,reviews}=window.DATA; return (<div>…</div>) }"
})
```

Validation: `source` must be `"project"` (Phase 1); `id` must be a project owned
by the calling user (reuse `uid_of` + `open_db` + `get_project`). On bad input
return an error string so the model can retry. On success return
`{"ok": true, "note": "artifact shown to the user"}`.

### Streaming + persistence (`server.rs`)
The existing `ChannelHook` (≈1508–1565) already sees `PreToolUse{name,args}`. Extend it:
- When `name == "render_artifact"`: emit a **new SSE event**
  `{"type":"artifact","title":…,"data":…,"code":…}` **instead of** the generic
  `tool_start` (so the UI shows the card, not a redundant tool-status chip), and
  **append the raw args to a shared collector** (`Arc<Mutex<Vec<serde_json::Value>>>`)
  owned by the stream task.
- When the spawned stream task saves the assistant reply (≈1900–1910 and the
  budget-exhausted path ≈1930–1936), serialize the collected artifacts to JSON
  and store them on the assistant `chat_messages` row.

### DB (`db.rs`)
- Add column via the existing `ensure_column` pattern (next to line 406):
  `self.ensure_column("chat_messages", "artifacts", "TEXT")?;` (JSON array, NULL
  for old rows).
- `add_chat_message` (≈1890) gains an `artifacts: Option<&str>` param; INSERT
  writes it.
- `get_chat_messages` (≈1850) selects `artifacts` and includes it on
  `ChatMessage`; the Rust `ChatMessage` struct gains `artifacts: serde_json::Value`
  (the parsed JSON array, default `[]`) — no strict Rust type needed. The
  frontend types it as `ArtifactSpec[]`.

### System prompt (`main.rs`, `SYSTEM_PROMPT`)
Add a section: when the user asks for a data-driven view of a project, call
`render_artifact` with a single self-contained React component (default export
not required; a top-level `App` function) that reads `window.DATA` (document the
`project` shape above). Charts may `import` from `recharts` (resolved by the
sandbox import map). Keep components single-file and dependency-light. Resolve
the project `id` via `list_projects` first.

## Frontend (`user-ui/src`)

### Data + sandbox libs
- `lib/artifact.ts` — `ArtifactSpec` type (`{ title, data:{source,id}, code }`);
  the **data-source registry**: `source → (id) => ledgerApi.project(id)` etc.
  (one entry now; **structured so a macro source slots in for Spec 2**).
- `lib/artifact-sandbox.ts` — `buildSrcdoc(compiledJs)`: returns an HTML string
  with (a) an esm.sh **import map** (`react`, `react-dom/client`, `recharts`),
  (b) the sucrase-compiled module, (c) a bootstrap that mounts `<App/>` into a
  root, **waits for `postMessage({type:'artifact-data',data})`** to set
  `window.DATA` then renders, and (d) `window.onerror` →
  `postMessage({type:'artifact-error',message})` back to the host. Transpile via
  `sucrase` (`transform(code,{transforms:['jsx','typescript']})`) on the host.

### Components
- `components/chat/artifact-card.tsx` — compact card (title + "Open" button)
  shown in the assistant bubble.
- `components/chat/artifact-view.tsx` — full-screen overlay (reuse
  `components/ui/sheet` `side="bottom"`/full): on open, call the registry to
  fetch data, render `<iframe sandbox="allow-scripts">` with `buildSrcdoc(...)`,
  `postMessage` the data once the iframe loads, show loading / data-error /
  runtime-error overlays, and a **Refresh** (re-fetch + re-inject) + **Close**.
  Lazy-loaded (`React.lazy`, like `NoteEditor` in `App.tsx`) so `sucrase` only
  loads when an artifact opens.

### Stream + chat wiring
- `components/chat/stream.ts` — `mapEvent` handles the new `artifact` SSE event →
  a `StreamEvent` of kind `artifact` carrying the spec.
- `components/chat/chat-sheet.tsx` — collect artifacts from the stream into the
  active message (parallel to `toolEvents`); on session load, read
  `message.artifacts` (now returned by the API) so reopened chats re-hydrate
  cards.
- `components/chat/message-list.tsx` — Bubble renders any `artifacts` for the
  message as `ArtifactCard`s beneath the markdown text.
- `lib/api.ts` — `ChatMessage` type gains `artifacts: ArtifactSpec[]`.
- `package.json` — add `sucrase` (lazy-loaded).

## Data flow

```
user asks → AI resolves id via list_projects → AI calls render_artifact(spec)
  → ChannelHook: emit SSE {type:"artifact",…} + push spec to collector
  → frontend: show ArtifactCard live; on "done", assistant msg saved WITH artifacts JSON
click card → artifact-view: ledgerApi.project(id) [host, has token]
  → buildSrcdoc(sucrase(code)) → <iframe sandbox=allow-scripts srcdoc=…>
  → iframe loads → host postMessage(data) → window.DATA set → React mounts
reopen chat → GET session returns messages incl. artifacts → cards re-hydrate (fresh data on open)
```

## Security

- `srcdoc` + `sandbox="allow-scripts"` **without** `allow-same-origin` ⇒ the
  iframe is an **opaque origin**: AI code cannot read the parent DOM,
  `localStorage`, cookies, or the auth token, and cannot make same-origin calls
  to `/api`. Data arrives **only** via `postMessage`.
- The only network the sandbox needs is **esm.sh** (CDN modules, no credentials).
  Optionally hardened with a CSP on the srcdoc limiting `script-src`/`connect-src`
  to `https://esm.sh`.
- The host validates `data.id` ownership in `render_artifact`; the host (not the
  sandbox) performs the authenticated fetch.

## Error handling

- **Transpile error** (sucrase throws on host): `artifact-view` shows the error
  message instead of mounting the iframe.
- **Runtime error** in the iframe: caught by `window.onerror`, posted to the host,
  shown as an error overlay with an **"ask AI to fix"** button that prefills the
  chat (`openChatWith`).
- **Data fetch failure** (host): "couldn't load data" state with Retry.
- **Streaming:** the card appears only on the `artifact` SSE event (a complete
  spec), so no partial/raw code is ever shown.
- **Bad tool args:** `render_artifact` returns an error string; the model can
  correct and retry within the same turn.

## Testing / verification

- **Backend (`cargo test -p dashboard`):** `render_artifact` validates source/id
  (rejects unknown source, rejects a project not owned by the user); the
  `chat_messages.artifacts` round-trip (`add_chat_message` → `get_chat_messages`
  returns the same specs).
- **Frontend unit (if a runner exists; else manual):** `buildSrcdoc` includes the
  import map + bootstrap + compiled code; the data-source registry maps
  `project` → the right `ledgerApi` call.
- **Playwright golden path:** ask for a project progress page → card appears →
  open → renders with injected data; reopen chat → card re-hydrates. **Error
  path:** a deliberately broken component → error overlay. **Security check:**
  assert the iframe has no `allow-same-origin` and cannot reach `/api` (the
  rendered page shows injected data, never a live token call).

## Rollout

Frontend changes + a small backend addition (one tool, one SSE event, one DB
column, one system-prompt section), all in `examples/dashboard`. Ships via the
existing musl build → qc-jp flow (`touch src/server.rs` before the musl build so
the embedded UI re-bundles). No data migration; the `artifacts` column is
additive.
