# Dashboard — Phase 1: fold note into ledger as the `project` model

**Date:** 2026-05-26
**Status:** approved (design), pending implementation plan
**Part of:** the unified "Dashboard" product (a cockpit for independent developers /
one-person companies / small teams). This spec covers **Phase 1 only**.

## Vision (north star)

One product — a cockpit ("飞机驾驶舱") for solo operators — where money and work
live on one surface and an AI co-pilot reasons across both. Built by growing
**ai-ledger** into it (ledger is the base; note's notes/goals/复盘 fold in).
Four top tabs: **dashboard · income/cost · project · profile**.

## The core entity: `project`

A **project** = a thing you're doing, with **goal · input · progress · outcome**.
It is note's `goals` entity *elevated and renamed* (goals already carry
objective + target_date + status + 复盘 reviews + sub-items):

- **goal** — the project's objective: `name`, `detail` (markdown), `target_date`.
- **progress** — `status` (active/paused/done/dropped) + 复盘 review timeline +
  **milestones** (the old sub-goals, shown as a checklist).
- **input / outcome** — (LATER phase) money tagged to the project for per-project
  P&L. Phase 1 leaves these implicit (no money↔project link yet).
- **notes** attach to a project via `notes.project_id` (nullable → "Unfiled").

This `project` replaces note's work/life **space** concept entirely.

## Decisions locked (from brainstorming)

1. **Grow ledger into Dashboard** — ledger is the base; fold note in; rebrand.
2. **Rename in place** — `ai-ledger` becomes `dashboard`; it keeps serving (its
   existing URL/data); `note.superleo.app` is migrated then retired (migration is
   a later phase).
3. **Drop work/life spaces** — replaced by `project`.
4. **`project` is a first-class entity** — notes + reviews + milestones belong to it.
5. **4 tabs**: dashboard / income-cost / project / profile.
6. Micro-calls: notes can be **Unfiled** (project optional); a project has **one
   objective + many milestones** (milestones = child project rows via `parent_id`);
   **money↔project P&L is a later phase**.

## Non-goals (later phases — NOT Phase 1)

- The cockpit **synthesis home** (Phase 2): the dashboard tab stays ledger's
  current finance `Dashboard.tsx` for now.
- **money↔project attribution** / per-project P&L (Phase 2/3).
- **note→dashboard data migration** (Phase 4).
- note's **"rule" kind** (戒律/principles, e.g. "股票不要操作") is **dropped** in
  Phase 1 — it doesn't fit "a thing you're doing." May return as a cockpit
  "principles" widget later.

## Architecture

Work happens in `examples/ai-ledger` (the base), renamed to `examples/dashboard`:
crate/package + binary + systemd unit + UI branding → "Dashboard". Same single
binary (axum + harness-rs + SQLite + embedded shadcn user-ui), same musl→qc-jp
deploy. The existing ledger SQLite schema is **extended** (new tables added; no
destructive changes), so existing ledger data keeps working.

## Backend (Rust)

### db.rs — new tables (extend the ledger schema)
- **`projects`**: `id, user_id, name, detail(md), status('active'|'paused'|'done'|'dropped'),
  parent_id(NULL=top-level; set=milestone of parent), target_date?,
  review_interval_days?, next_review_at?, created_at, updated_at`.
  (This is note's `goals` shape minus `space`, `kind` renamed concept → top-level
  = project, child = milestone.)
- **`project_reviews`**: `id, project_id, user_id, progress(md), next_steps(md), created_at`
  (note's `goal_reviews`).
- **`notes`**: `id, user_id, project_id(NULL=Unfiled), title, body, tags, embedding,
  embedding_dim, embedding_at, created_at, updated_at` (note's `notes` minus
  `space`, plus `project_id`).
- Indexes: `(user_id, status)` + `(parent_id)` on projects; `(project_id, created_at)`
  on reviews; `(user_id, updated_at)` + pending-embed partial index on notes.
- Helpers ported from note (space removed, `project_id` added where relevant):
  project CRUD + milestones + reviews + cadence advance; note CRUD + list +
  range + embeddings + `notes.project_id` filter.

### Embeddings (port from note)
- `search.rs` (semantic search; **space filter removed**, optional `project_id`
  filter added), `embed_slot.rs`, `embed_worker.rs`.
- Add `GeminiEmbed` embedder to `AppState` + spawn the embed worker in `main.rs`
  (ledger already configures a Gemini key for vision/extract_receipt).

### tools.rs — agent tools (fold note's in, reframed to project)
Existing ~23 ledger tools stay. Add (auto-register via inventory):
- `create_project(name, detail?, target_date?, review_interval_days?, parent_id?)`
- `update_project(id, {status?, name?, detail?, target_date?, review_interval_days?})`
- `list_projects(status?, parent_id?, due_for_review?)`
- `add_milestones(project_id, milestones[])` (note's `decompose_goal`)
- `log_project_review(project_id, progress, next_steps?, next_review_in_days?)`
- `create_note(title, body, tags?, project_id?)`, `search_notes(query, top_k?)`,
  `list_recent_notes(limit?, since?, until?)`, `update_note(id, …)`, `delete_note(id)`.
- All **space logic removed** (no `space_of`).

### server.rs — REST + prompt
- Routes: `/api/projects` (GET list + POST), `/api/projects/:id`
  (GET{project,milestones,reviews} + PATCH + DELETE), `/api/projects/:id/reviews`
  (POST); `/api/notes` (GET list?project_id= + POST), `/api/notes/:id`
  (GET+PATCH+DELETE), `/api/notes/:id/export.md`, `/api/notes/export.zip`,
  `/api/notes/search?q=&project_id=`.
- SYSTEM_PROMPT: merge note's notes/projects/复盘 rules (space rule removed;
  "goal"→"project" wording). Keep all ledger rules.

## Frontend (`user-ui`, rebranded to Dashboard)

### Nav — 4 tabs (replaces ledger's Dashboard/Ledger/Portfolio/Profile + note's Notes/Plans/Search)
1. **dashboard** (`/app`) — ledger's existing finance `Dashboard.tsx` for now
   (Phase 2 turns this into the cockpit).
2. **income/cost** (`/app/money`) — the money hub: transactions/budgets/
   subscriptions (ledger's `Ledger.tsx`) with net-worth/portfolio reachable from
   here (fold `Portfolio.tsx` in as a section/sub-view). Existing money features
   unchanged; just regrouped under one tab.
3. **project** (`/app/projects`) — list of projects → drill into one
   (`/app/projects/:id`): goal, progress (reviews + milestone checklist), its
   notes. Note read/edit pages live under here (`/app/notes/:id`,
   `/app/notes/:id/edit`, `/app/notes/new?project=`). Semantic **search** folds in
   as a box on this tab (no separate Search tab).
4. **profile** (`/app/profile`) — account, model picker, memory, exports.

### Components
- Port note's pages → project-framed: project list + `ProjectView` (from
  `GoalView`) + `NoteView`/`NoteEditor` (project_id aware) + note list/search.
- Reuse note's niceties already built: styled `useConfirm`, `chat-prefill`,
  MDXEditor. Drop `SpaceContext` + the work/life toggle.
- Merge note's `api.ts` helpers into ledger's (`projects`, `notes`).
- Rebrand all "Ledger"/"ai-ledger" UI strings → "Dashboard"; nav labels +
  i18n (en/zh): `nav.dashboard / nav.money / nav.project / nav.profile`.

## Rename / deploy

- `git mv examples/ai-ledger examples/dashboard`; Cargo `name = "dashboard"`,
  `[[bin]] name = "dashboard"`; update workspace members; `include_dir!` uses
  `$CARGO_MANIFEST_DIR` so paths auto-resolve.
- systemd unit `ai-ledger` → `dashboard`; install dir `/opt/dashboard`.
- Keep serving on the existing port; `ledger.superleo.app` keeps working and a
  `dashboard.superleo.app` alias is added in Caddy. Existing ledger DB reused
  (new tables auto-created on boot).

## Testing / verification

- `cargo test -p dashboard` clean; `npm run build` + `tsc --noEmit` clean.
- Manual golden path (real keys): create a project via chat → it appears under
  the **project** tab → add a note to it → note shows under the project →
  semantic search finds it → 复盘 a project (review logged, cadence advances) →
  income/cost tab still shows transactions/portfolio → dashboard tab still shows
  the finance overview → 4-tab nav works on mobile + desktop.
- Regression: every existing ledger feature (transactions, budgets,
  subscriptions, portfolio, loans, receipts, net-worth, chat, memory, admin)
  still works.

## Rollout

musl cross-compile (existing ledger build path, now `dashboard`) → qc-jp;
systemd unit renamed; serve on the existing ledger port; add the
`dashboard.superleo.app` Caddy site. No data migration in Phase 1 (note data
comes in Phase 4); existing ledger data is preserved (additive schema only).
