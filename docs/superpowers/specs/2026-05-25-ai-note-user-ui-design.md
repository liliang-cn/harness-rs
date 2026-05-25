# ai-note user-ui — ledger parity + 工作/生活 spaces

**Date:** 2026-05-25
**Status:** approved (design), pending implementation plan

## Goal

Give ai-note a modern user-facing SPA matching ai-ledger's `user-ui`
(Vite + React + shadcn/Radix + Tailwind v4 + react-i18next, mobile-first,
streaming chat with persisted sessions, chat FAB on every page). Replace the
hand-written `src/index.html` as the app's `/` surface.

Add **工作 / 生活 (work / life)** as the product's primary navigation
dimension: a dedicated `space` field on notes (and chat sessions), surfaced as
a header toggle. Notes list, semantic search, and chat all scope to the active
space, and the chat agent is space-aware (new notes default to the active
space; recall is space-scoped).

## Non-goals (YAGNI)

- **No attachments / receipts** — notes don't need photo upload; drop ledger's
  `attachment-button`, attachment thumbnails, and `/api/chat/attachments`.
- **No long-term memory synthesizer** — the notes *are* the durable store;
  skip ledger's `MemoryGuide` / `MemorySynthesizer` / `remember_this` wiring.
- **No marketing redesign** — port ledger's Marketing page shape + SEO/GEO and
  reword for note-taking; not a from-scratch landing.
- **No cross-space view** — a note belongs to exactly one space; no "all
  spaces" aggregate list (the chat agent still only sees the active space).

## Decisions (locked)

1. **Space model:** dedicated `space` column, agent-aware. (Not tags.)
2. **Chat parity:** full SSE streaming + persisted session CRUD ported to the
   ai-note backend; frontend reuses ledger's `chat/` components.
3. **Sessions are per-space:** switching to 工作 shows only work chats.
4. **Default space** for new notes and the legacy backfill = `life`.

## Architecture

```
ai-note/
  src/                     # Rust backend (axum + harness-rs)
    db.rs                  # + space column, + chat session/message helpers, + ensure_column
    server.rs              # + serve user-ui dist at /, + chat session/stream routes,
                           #   + /api/me/model, + ?space= on note routes
    tools.rs               # space-aware create_note / search_notes / list_recent_notes
  user-ui/                 # NEW — copied from ai-ledger/user-ui, domain-swapped
    src/
      components/ui/*      # reuse verbatim
      components/chat/*    # reuse (minus attachment-button); api wiring re-pointed
      lib/{utils,markdown,i18n,api}.ts
      pages/{Marketing,Login,Notes,Search,Profile}.tsx
      components/app-shell.tsx   # + 工作/生活 toggle, nav = Notes/Search/Profile
      components/space-context.tsx  # NEW — active space provider (localStorage-backed)
  admin-ui/                # untouched (antd)
```

Build: `cd user-ui && npm run build` → `user-ui/dist` embedded via
`include_dir!("$CARGO_MANIFEST_DIR/user-ui/dist")`, same pattern as
`admin-ui/dist`.

## Backend changes

### db.rs

- Add idempotent `ensure_column(&self, table, col, decl)` helper (swallows the
  "duplicate column" error), mirroring ai-ledger.
- `notes`: add `space TEXT NOT NULL DEFAULT 'life'`. Migrate existing rows via
  `ensure_column` (existing rows default to `life`).
  - `create_note` takes `space: &str`.
  - `list_recent_notes`, `list_notes_in_range`, `list_embeddings` (search path),
    `count_notes` gain an optional `space: Option<&str>` filter.
  - `Note` struct gains `space: String`.
- `chat_sessions`: add `space TEXT NOT NULL DEFAULT 'life'`.
- Port chat helpers from ai-ledger (tables already exist in ai-note):
  - `create_chat_session(user_id, id, model_id, space)`
  - `list_chat_sessions(user_id, space)` → filtered by space
  - `get_chat_session(user_id, id)` / `get_chat_messages(user_id, id, limit)`
  - `append_chat_message(user_id, session_id, role, text, iters)` — bumps
    `message_count`, sets title from first user message, touches `updated_at`.
    (No `attachment_ids` param — ai-note has no attachments.)
  - `delete_chat_session`, `update_chat_session_model`
  - `ChatSession` / `ChatMessage` serializable structs.

### server.rs

- Embed + serve `user-ui/dist`:
  - `GET /` → user-ui `index.html`.
  - `GET /assets/*` etc. → static with hashed-asset long cache.
  - SPA fallback for `/app`, `/app/*`, `/login`, `/search`, `/profile` → index.
  - Remove `serve_index`/`INDEX_HTML` (legacy) + `serve_marked_js` if unused by
    the new UI (the new UI bundles its own markdown renderer like ledger).
  - Keep `/admin` routes exactly as-is.
- Note routes: `GET /api/notes?space=` and `GET /api/notes/search?...&space=`
  filter by space; `POST /api/notes` accepts `space` (defaults `life`).
- Chat session routes (port from ledger):
  - `GET /api/chat/sessions?space=` (list) · `POST /api/chat/sessions` (create,
    body `{space}`)
  - `GET /api/chat/sessions/:id` · `DELETE /api/chat/sessions/:id`
  - `POST /api/chat/sessions/:id/stream` — SSE. Body `{message, lang}`.
    Persists user msg, builds history from DB, runs `AgentLoop` with
    `.with_streaming(true)` + a `ChannelHook` emitting:
    - `{"type":"start"}`
    - token deltas (`{"type":"delta","text":...}` — match the event shape
      ledger's `stream.ts` already parses)
    - tool events (`{"type":"tool_start"|"tool_end", ...}`)
    - `{"type":"done","ok",iters,"reply"}` / `{"type":"error","message"}`
    Persists assistant reply on completion. Plants `space` (+ `user_id`,
    `db_path`, `tier`, tz, embedder slot) on `profile.extra`.
  - Keep one-shot `POST /api/chat` as a fallback (unchanged).
- `POST /api/me/model` — set `preferred_model`; tier-gated (paid/admin only),
  validated against the allowlist. Mirror ledger's `set_model_handler`.
- `AppState::build_model_for(&self, model_id) -> anyhow::Result<Arc<dyn Model>>`
  — maps an allowlisted model id to provider + key from `AppConfig`:
  - `deepseek-v4-flash`, `deepseek-v4-pro` → `OpenAiCompat(DEEPSEEK, key)`
  - `gemini-3.5-flash` → `GeminiNative(key)`
  - `effective_model_for(user)` → user's `preferred_model` if allowlisted, else
    the configured default.
- `build_task_description` gains a `space` arg → inject a
  `[system] space: work|life` line (same mechanism ledger uses for `lang` /
  attachments, since `profile.extra` is dropped by the guide).

### tools.rs + SYSTEM_PROMPT

- `create_note`: read active `space` from `profile.extra` (fallback `life`);
  store it. The space is NOT a user-supplied tool arg — it's ambient context.
- `search_notes` / `list_recent_notes`: filter to the active space.
- SYSTEM_PROMPT: add a rule — "All note operations are scoped to the user's
  current space ({work|life}); never mix spaces. New notes go in the current
  space unless the user explicitly says otherwise."

## Frontend

### Routing (App.tsx — same shape as ledger)

- `/` → `Marketing` (always)
- `/login` → `Login`
- `/app` (RequireAuth → `AppShell`):
  - index → `Notes`
  - `search` → `Search`
  - `profile` → `Profile`
- `*` → redirect `/`

### SpaceContext (new)

`components/space-context.tsx` — `{ space: 'work'|'life', setSpace }`, backed by
`localStorage('ai-note-space')`, default `life`. Provider wraps `AppShell`.
Consumed by Notes, Search, ChatSheet, sessions-list (all pass `space` to the
API and refetch on change).

### AppShell

Copy ledger's `app-shell.tsx`; changes:
- Nav = `Notes (/app)`, `Search (/app/search)`, `Profile (/app/profile)` with
  appropriate lucide icons (e.g. `NotebookPen`, `Search`, `User`).
- Header center: a **工作 / 生活 segmented toggle** (shadcn `Tabs` or two
  buttons) wired to `SpaceContext`.
- Keep `LangSwitch`, logout, `ChatFab`, `Toaster`.

### Pages

- **Notes** (`/app` index): fetch `/api/notes?space=`; card list (title or
  body-head, tag chips, relative time); "+ new note" and row-tap open an editor
  **Sheet** (title input + markdown `Textarea` body + tag input); save →
  `POST`/`PATCH /api/notes`; delete with confirm. Empty state.
- **Search** (`/app/search`): query box → `GET /api/notes/search?q=&space=`;
  ranked hit cards (score + snippet); tap → editor Sheet. Debounced.
- **Profile** (`/app/profile`): account email + tier, change-password form,
  invites (paid+), **model picker** (`POST /api/me/model`, paid-gated:
  deepseek-v4-flash / deepseek-v4-pro / gemini-3.5-flash), export `.zip`
  (`/api/notes/export.zip`), language switch. Reuse ledger's `password-form`,
  `account-card`, `model-picker` shapes.

### Chat (reused, minus attachments)

- Copy `chat/` folder; **remove** `attachment-button.tsx` and the attachment
  thumbnail/dialog code in `message-list.tsx`, the paperclip in `composer.tsx`,
  and attachment fields in `api.ts`.
- `ChatSheet`: on send, lazily `POST /api/chat/sessions {space}` if no active
  session (ledger's "drafting" pattern), then stream
  `POST /api/chat/sessions/:id/stream {message, lang}`. Sessions list fetches
  `/api/chat/sessions?space=` and refetches when space changes. Keep the
  reopen-refresh, truncated-marker, and unread-badge behavior ledger has.
- `stream.ts` reused as-is (backend emits the same event shapes).

### api.ts

Re-point to ai-note endpoints. Types: `Note { id,title,body,tags,space,
created_at,updated_at }`, `SearchHit`, `ChatSession { id,title,space,
message_count,model_id,updated_at }`, `ChatMessage { id,session_id,role,text,
created_at,truncated? }`. Helpers: notes CRUD, search, chat session CRUD +
stream URL, `setModel`, `me`, auth. Drop all attachment helpers.

### i18n

Copy ledger `locales/{en,zh}.json`; replace ledger-domain keys (ledger /
portfolio / net-worth) with note keys (notes / search / spaces.work /
spaces.life / editor.*). Keep shared keys (nav, chat, common, auth).

## Data flow — capture path

1. User in 工作 space taps FAB.
2. Types "记一下：明天和供应商对账".
3. `ChatSheet` creates a session `{space:'work'}` (if none), POSTs to
   `/sessions/:id/stream {message, lang}`.
4. Backend plants `space=work`, runs the agent; `create_note` stores the note
   with `space=work`. Token deltas + a `create_note` tool event stream back and
   render live.
5. Reply persisted; closing/reopening the sheet shows the canonical log.
6. Notes (work) list shows the new note.

## Testing / verification

- `cargo build -p ai-note` clean; `cd user-ui && npm run build` clean.
- Manual (golden path): register/login → toggle 工作/生活 → create a note in
  each space → confirm list is space-filtered → semantic search scoped to
  space → FAB chat in 工作 captures a work note via streaming → switch to 生活
  and confirm the work note + work chats are hidden → model picker (paid) →
  export zip. Mobile viewport + desktop.
- Regression: `/admin` still loads; one-shot `/api/chat` still answers.

## Rollout

Local build + musl cross-compile (existing `ai-note` build path) → deploy to
qc-jp (`note.superleo.app`, binds `127.0.0.1:6755`, Caddy reverse proxy).
