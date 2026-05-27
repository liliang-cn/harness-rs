# Dashboard Phase 1 (fold note → ledger as `project` model) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Grow ai-ledger into the unified "Dashboard" product: fold note's notes/goals/复盘 in as a first-class `project` entity (goal · progress · notes), drop work/life spaces, expose a 4-tab nav (dashboard · income/cost · project · profile), and rebrand. Existing ledger money features stay intact.

**Architecture:** Work in `examples/ai-ledger` (the base), renamed to `examples/dashboard` at the end. Extend the ledger SQLite schema additively (`projects`, `project_reviews`, `notes`); port note's semantic search + Gemini embeddings (ledger has none today); fold note's agent tools (reframed goal→project, space removed); add REST + UI. Single binary, same musl→qc-jp deploy.

**Tech Stack:** Rust (axum, rusqlite, harness-rs), React 19 + Vite + shadcn + react-i18next, MDXEditor, Gemini embeddings.

**Spec:** `docs/superpowers/specs/2026-05-26-dashboard-phase1-design.md`

**Port sources (read these — most tasks port from them):**
- note backend: `examples/ai-note/src/{db.rs,search.rs,embed_slot.rs,embed_worker.rs,tools.rs,server.rs,main.rs}`
- note frontend: `examples/ai-note/user-ui/src/{lib/api.ts,lib/chat-prefill.ts,components/confirm-dialog.tsx,pages/{Notes,Search,NoteView,NoteEditor,Plans,GoalView}.tsx,components/space-context.tsx}`
- ledger is the target; mirror its existing patterns (`ledgerApi`, AppState in `src/server.rs:100`, `ensure_column` in db.rs).

**Global rename rules when porting note → ledger:**
- Drop the `space` concept entirely: remove `space` columns/params/filters, `space_of()`, `SpaceContext`, the work/life toggle, and all `?space=` query params.
- Reframe goal → project: `goals`→`projects`, `goal_reviews`→`project_reviews`, tools `create_goal`→`create_project` etc. (see Task 3); a top-level row = project, a `parent_id` child = **milestone**.
- Notes gain `project_id` (nullable = Unfiled); drop `space`.
- Frontend: note's `noteApi.*` calls → merge into ledger's `ledgerApi`.

**Working dir:** `/Users/liliang/Things/courses/harness`. All paths under `examples/ai-ledger` until Task 9 renames it.

---

## Task 1: db.rs — projects + project_reviews + notes tables, structs, helpers, tests

**Files:** Modify `examples/ai-ledger/src/db.rs`

- [ ] **Step 1: Add the tables to the schema**

Inside ledger's `init`/`execute_batch` (where the other `CREATE TABLE IF NOT EXISTS` live), add:

```sql
CREATE TABLE IF NOT EXISTS projects (
    id                   TEXT PRIMARY KEY,
    user_id              TEXT NOT NULL,
    name                 TEXT NOT NULL,
    detail               TEXT NOT NULL DEFAULT '',
    status               TEXT NOT NULL DEFAULT 'active',
    parent_id            TEXT,
    target_date          TEXT,
    review_interval_days INTEGER,
    next_review_at       TEXT,
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_projects_user_status ON projects(user_id, status);
CREATE INDEX IF NOT EXISTS idx_projects_parent ON projects(parent_id);
CREATE INDEX IF NOT EXISTS idx_projects_due ON projects(user_id, next_review_at);

CREATE TABLE IF NOT EXISTS project_reviews (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL,
    user_id     TEXT NOT NULL,
    progress    TEXT NOT NULL,
    next_steps  TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_project_reviews ON project_reviews(project_id, created_at);

CREATE TABLE IF NOT EXISTS notes (
    id            TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    project_id    TEXT,
    title         TEXT NOT NULL DEFAULT '',
    body          TEXT NOT NULL,
    tags          TEXT,
    embedding     BLOB,
    embedding_dim INTEGER,
    embedding_at  TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_notes_user_updated ON notes(user_id, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_notes_project ON notes(user_id, project_id);
CREATE INDEX IF NOT EXISTS idx_notes_pending_embed ON notes(user_id) WHERE embedding IS NULL;
```

- [ ] **Step 2: Add structs**

Port from `ai-note/src/db.rs` the `Note`, `Goal`, `GoalReview`, `PendingEmbed`, `NoteEmbedding` structs, renamed/adapted:
- `Goal` → `Project` (fields: id, name, detail, status, parent_id: Option<String>, target_date: Option<String>, review_interval_days: Option<i64>, next_review_at: Option<String>, created_at, updated_at — all serializable; drop `space`, `kind`; rename `title`→`name`).
- `GoalReview` → `ProjectReview` (id, project_id, progress, next_steps, created_at).
- `Note` (id, project_id: Option<String>, title, body, tags: Vec<String>, created_at, updated_at; drop `space`).
- `PendingEmbed`, `NoteEmbedding` as in note.

Use ledger's existing serde-date conventions. If ledger lacks `random_id()`, copy it from `ai-note/src/db.rs` (the `OsRng` 8-byte hex fn).

- [ ] **Step 3: Add `impl Db` helpers**

Port note's helpers, applying the rename rules. Required methods (signatures):
- Projects: `create_project(user_id, name, detail, parent_id: Option<&str>, target_date: Option<&str>, review_interval_days: Option<i64>) -> Project` (seeds `next_review_at = now + interval` for top-level when interval Some); `get_project(user_id, id) -> Option<Project>`; `list_projects(user_id, status: Option<&str>, only_due: bool) -> Vec<Project>`; `list_milestones(user_id, parent_id) -> Vec<Project>`; `update_project(user_id, id, status, name, detail, target_date, review_interval_days: Option<i64>) -> u32` (COALESCE); `delete_project(user_id, id)` (tx: delete project_reviews for it, its milestones by parent_id, then it); `count_due_projects(user_id) -> u32`; `add_project_review(user_id, project_id, progress, next_steps, override_days: Option<i64>) -> ProjectReview` (+ advance next_review_at); `list_project_reviews(user_id, project_id, limit) -> Vec<ProjectReview>`.
- Notes: `create_note(user_id, project_id: Option<&str>, title, body, tags: &[String]) -> Note`; `get_note(user_id, id)`; `list_recent_notes(user_id, project_id: Option<&str>, limit) -> Vec<Note>` (project_id filter optional); `list_notes_in_range(user_id, project_id: Option<&str>, since, until, limit)`; `update_note(user_id, id, title, body, tags)` (clears embedding); `delete_note(user_id, id)`; `count_notes(user_id) -> u32` (for any trial cap; ledger may not cap — keep simple); embedding helpers `pending_embeds(batch)`, `write_embedding(id, dim, vec)`, `list_embeddings(user_id, project_id: Option<&str>) -> Vec<NoteEmbedding>`.

These are direct ports of `ai-note/src/db.rs`'s goal/note helpers with `space` removed and `project_id` threaded into the note ones. Read that file for the exact bodies.

- [ ] **Step 4: Tests**

Add a `#[cfg(test)] mod tests` (or extend ledger's) with a `tmp_db()` helper, covering: create_project + list (active/due), milestones via parent_id, add_project_review advances next_review_at, delete cascades milestones+reviews, create_note with/without project_id + list filtered by project_id.

```rust
#[test]
fn projects_and_notes_basic() {
    let db = tmp_db();
    let p = db.create_project("u1", "上线 SaaS", "", None, Some("2026-09-30"), Some(30)).unwrap();
    assert!(p.next_review_at.is_some());
    let m = db.create_project("u1", "做落地页", "", Some(&p.id), None, None).unwrap();
    assert_eq!(db.list_milestones("u1", &p.id).unwrap().len(), 1);
    assert_eq!(m.parent_id.as_deref(), Some(p.id.as_str()));
    let n = db.create_note("u1", Some(&p.id), "想法", "正文", &["idea".into()]).unwrap();
    assert_eq!(n.project_id.as_deref(), Some(p.id.as_str()));
    assert_eq!(db.list_recent_notes("u1", Some(&p.id), 50).unwrap().len(), 1);
    assert_eq!(db.list_recent_notes("u1", None, 50).unwrap().len(), 1);
    db.add_project_review("u1", &p.id, "做了线框", "下周写代码", None).unwrap();
    assert_eq!(db.list_project_reviews("u1", &p.id, 10).unwrap().len(), 1);
    assert_eq!(db.delete_project("u1", &p.id).unwrap(), 1);
    assert!(db.get_project("u1", &p.id).unwrap().is_none());
}
```

- [ ] **Step 5: Run + commit**

Run: `cd /Users/liliang/Things/courses/harness && cargo test -p ai-ledger 2>&1 | tail -15` (expect pass; later-task callers don't exist yet, so other files compile unchanged — these are all NEW methods, so the crate compiles).
```bash
git add examples/ai-ledger/src/db.rs
git commit -m "feat(dashboard): projects + project_reviews + notes tables/helpers (folded from note, space dropped, project_id added)"
```

---

## Task 2: Embeddings — semantic search + Gemini embedder + worker

**Files:** Create `examples/ai-ledger/src/{search.rs,embed_slot.rs,embed_worker.rs}`; Modify `examples/ai-ledger/src/{main.rs,server.rs,lib path module list}`

- [ ] **Step 1: Port the three files**

Copy `ai-note/src/embed_slot.rs` and `ai-note/src/embed_worker.rs` verbatim (they're user-agnostic). Copy `ai-note/src/search.rs` and adapt `semantic_search` to take `project_id: Option<&str>` instead of `space` (thread it to `list_embeddings(user_id, project_id)` and the grep fallback `list_recent_notes(user_id, project_id, 5000)`).

Register the modules in `main.rs` (`mod search; mod embed_slot; mod embed_worker;`).

- [ ] **Step 2: Add the embedder to AppState + boot**

In `server.rs` `AppState` (line ~100), add `pub embedder: std::sync::Arc<dyn harness_core::Embedder>`.
In `main.rs` where `AppState` is built: construct `let embedder: Arc<dyn harness_core::Embedder> = Arc::new(harness_models::GeminiEmbed::with_key(gemini_key.clone()))` (ledger already resolves a Gemini key for vision; reuse it — error only if absent and notes/search are used). Call `embed_slot::set(embedder.clone())`. Spawn the worker (mirror `ai-note/src/main.rs`'s `EmbedWorker { db_path, embedder, batch_size:32, idle_pause:5s, busy_pause:250ms }.spawn()`). Pass `embedder` into `AppState`.

- [ ] **Step 3: Build + commit**

Run: `cd /Users/liliang/Things/courses/harness && cargo build -p ai-ledger 2>&1 | tail -20` (0 errors). Boot smoke optional.
```bash
git add examples/ai-ledger/src/search.rs examples/ai-ledger/src/embed_slot.rs examples/ai-ledger/src/embed_worker.rs examples/ai-ledger/src/main.rs examples/ai-ledger/src/server.rs
git commit -m "feat(dashboard): semantic search + Gemini embedder + embed worker (ported from note)"
```

---

## Task 3: tools.rs — project + note agent tools

**Files:** Modify `examples/ai-ledger/src/tools.rs`, `examples/ai-ledger/src/server.rs` (SYSTEM_PROMPT)

- [ ] **Step 1: Add the tools**

Port note's `#[harness::tool]`s into ledger's tools.rs (they auto-register via inventory alongside ledger's ~23). Apply rename rules (no `space_of`; goal→project; notes get optional `project_id`). Tools to add:
- `create_project` (args: name, detail?, target_date?, review_interval_days?, parent_id?) — note's `create_goal` reframed.
- `add_milestones` (parent_id, milestones: [{name, detail?}]) — note's `decompose_goal`.
- `update_project` (id, status?/name?/detail?/target_date?/review_interval_days?).
- `list_projects` (status?, parent_id?, due_for_review?).
- `log_project_review` (project_id, progress, next_steps?, next_review_in_days?).
- `create_note` (title, body, tags?, project_id?), `search_notes` (query, top_k?), `list_recent_notes` (limit?, since?, until?), `update_note` (id,…), `delete_note` (id).

Schemas must stay **Gemini-safe** (optional fields omitted from `required`; no `["type","null"]`). Read `ai-note/src/tools.rs` for the bodies; helpers `uid_of`/`db_path_of`/`open_db`/`embedder()` exist or port them (note's `embedder()` reads `embed_slot::get()`).

- [ ] **Step 2: Merge SYSTEM_PROMPT**

Append note's notes/projects/复盘 rules to ledger's SYSTEM_PROMPT, with: space rule removed; "goal"→"project"; rule for `create_note` to attach `project_id` when the user is clearly working within a project; keep all existing ledger (money) rules. Drop note's "rule/戒律" rule (kind dropped).

- [ ] **Step 3: Build + test + commit**

Run: `cargo build -p ai-ledger 2>&1 | tail -10 && cargo test -p ai-ledger 2>&1 | tail -5` (compiles; tests pass).
```bash
git add examples/ai-ledger/src/tools.rs examples/ai-ledger/src/server.rs
git commit -m "feat(dashboard): project + note agent tools + prompt rules (folded from note)"
```

---

## Task 4: server.rs — REST endpoints for projects + notes

**Files:** Modify `examples/ai-ledger/src/server.rs`

- [ ] **Step 1: Add handlers + routes**

Port note's project (was goal) + note REST handlers, adapting to ledger's `AuthCtx`/`open_db`/`ApiError` patterns and the rename rules:
- `GET /api/projects?filter=active|due|all`, `POST /api/projects`, `GET /api/projects/:id` → `{project, milestones, reviews}`, `PATCH /api/projects/:id`, `DELETE /api/projects/:id`, `POST /api/projects/:id/reviews`.
- `GET /api/notes?project_id=`, `POST /api/notes`, `GET/PATCH/DELETE /api/notes/:id`, `GET /api/notes/:id/export.md`, `GET /api/notes/export.zip`, `GET /api/notes/search?q=&project_id=`.
- Validation: project name non-empty; status ∈ {active,paused,done,dropped}; note body non-empty. No `space` validation.

Reference: `ai-note/src/server.rs` goal/note handlers + the chat space-planting (remove the space plant; instead the agent tools resolve project from args).

- [ ] **Step 2: Build + smoke + commit**

Run: `cargo build -p ai-ledger 2>&1 | tail -15`. Smoke: boot with keys, register, `POST /api/projects {name}`, `GET /api/projects` → returns it; `POST /api/notes {title,body,project_id}` → `GET /api/notes?project_id=` → returns it.
```bash
git add examples/ai-ledger/src/server.rs
git commit -m "feat(dashboard): REST endpoints for projects + notes"
```

---

## Task 5: Frontend foundation — deps, libs, api merge, drop spaces

**Files:** Modify `examples/ai-ledger/user-ui/package.json`, `src/lib/api.ts`; Create `src/lib/chat-prefill.ts`, `src/components/confirm-dialog.tsx`; Modify chat components

- [ ] **Step 1: Add deps**

```bash
cd /Users/liliang/Things/courses/harness/examples/ai-ledger/user-ui && npm install @mdxeditor/editor prismjs
```

- [ ] **Step 2: Prism global fix (learned in note)**

In `src/main.tsx`, add `import 'prismjs';` as the FIRST import — `@lexical/code` (via MDXEditor) reads a bare global `Prism` that Vite won't otherwise initialize (else the editor throws "Prism is not defined").

- [ ] **Step 3: Port libs**

Copy `ai-note/user-ui/src/lib/chat-prefill.ts` and `ai-note/user-ui/src/components/confirm-dialog.tsx` verbatim. Wrap ledger's `AppShell` return in `<ConfirmProvider>…</ConfirmProvider>` (mirror `ai-note/user-ui/src/components/app-shell.tsx`). Ensure `common.confirm/cancel/confirmTitle` i18n keys exist in ledger's locales (add if missing).

- [ ] **Step 4: Merge note's api helpers into `ledgerApi`**

In `src/lib/api.ts`, add the `Project`/`ProjectReview`/`Note` types and helpers onto the existing `ledgerApi` object: `projects(filter)`, `project(id)`, `createProject(body)`, `updateProject(id,patch)`, `deleteProject(id)`, `addProjectReview(id,body)`, `notes(projectId?)`, `note(id)`, `createNote(body)`, `updateNote(id,patch)`, `deleteNote(id)`, `searchNotes(q, projectId?)`. (Adapt note's `noteApi` helper bodies; URLs per Task 4; no `space` params.)

- [ ] **Step 5: Build + commit**

Run: `npx tsc --noEmit 2>&1 | tail -15 && npm run build 2>&1 | tail -5` (green).
```bash
git add examples/ai-ledger/user-ui/src examples/ai-ledger/user-ui/package.json examples/ai-ledger/user-ui/package-lock.json examples/ai-ledger/user-ui/dist
git commit -m "feat(dashboard/ui): foundation — mdxeditor+prismjs, confirm dialog, chat-prefill, project/note api helpers"
```

---

## Task 6: Frontend nav — 4 tabs + routing

**Files:** Modify `src/components/app-shell.tsx`, `src/App.tsx`, locales

- [ ] **Step 1: NAV → 4 tabs**

Set `NAV` to: `{to:'/app', key:'dashboard', icon: Home}`, `{to:'/app/money', key:'money', icon: Wallet}`, `{to:'/app/projects', key:'project', icon: Target}`, `{to:'/app/profile', key:'profile', icon: User}`. Remove the separate portfolio tab (folded into money).

- [ ] **Step 2: Routes**

In `App.tsx` under `/app`: keep `index → Dashboard` (finance overview, unchanged for now); `money → Ledger` (transactions; add a link/section to Portfolio within it, or route `money/portfolio → Portfolio`); `projects → Projects` (Task 7), `projects/:id → ProjectView`, `notes/:id → NoteView`, `notes/:id/edit → NoteEditor` (lazy), `notes/new → NoteEditor` (lazy); `profile → Profile`. Remove the old top-level `ledger`/`portfolio` routes (or alias `/app/ledger`→`/app/money`).

- [ ] **Step 3: i18n labels**

Add/adjust `nav.dashboard / nav.money / nav.project / nav.profile` in en/zh (e.g. en: Dashboard / Income & Cost / Projects / Profile; zh: 仪表盘 / 收支 / 项目 / 我的).

- [ ] **Step 4: Build + commit**

Run: `npx tsc --noEmit 2>&1 | tail -10 && npm run build 2>&1 | tail -5` (it's OK if it fails only on missing Projects/ProjectView/NoteView pages — add 1-line stubs to keep green, replaced in Task 7).
```bash
git add examples/ai-ledger/user-ui/src examples/ai-ledger/user-ui/dist
git commit -m "feat(dashboard/ui): 4-tab nav (dashboard/money/project/profile) + routing"
```

---

## Task 7: Frontend pages — projects + notes

**Files:** Create `src/pages/{Projects,ProjectView,NoteView,NoteEditor}.tsx`; Modify `money` view to fold Portfolio

- [ ] **Step 1: Projects list + ProjectView**

Port `ai-note/user-ui/src/pages/Plans.tsx` → `Projects.tsx` (drop spaces/`useSpace`; "due for review" + projects list; cards `flex flex-row` to avoid the Card flex-col centering bug; New project button → chat prefill `openChatWith('我想开一个新项目：')`; card → `navigate('/app/projects/:id')`).
Port `ai-note/user-ui/src/pages/GoalView.tsx` → `ProjectView.tsx` (`/app/projects/:id`): goal/objective, progress (reviews + milestone checklist), the project's notes list (fetch `ledgerApi.notes(projectId)`), Review/Mark-done/Delete (styled confirm), "Break down"/"Review" via chat-prefill, "+ note" → `navigate('/app/notes/new?project=<id>')`.

- [ ] **Step 2: NoteView + NoteEditor**

Port `ai-note/user-ui/src/pages/{NoteView,NoteEditor}.tsx` (the full-page note read/edit built earlier). Drop spaces; read `project` from the `?project=` query param on new; show/keep the note's `project_id`. NoteEditor lazy (MDXEditor). Search box on the Projects (or money?) — put global semantic search on the Projects tab header → `ledgerApi.searchNotes(q)`.

- [ ] **Step 3: Fold Portfolio into the money tab**

Make the `money` view surface transactions (existing `Ledger.tsx`) plus a link/section to net-worth/portfolio (existing `Portfolio.tsx`) — e.g. a secondary nav or a section. Keep both components; just regroup so they live under the single "income/cost" tab.

- [ ] **Step 4: Build + commit**

Run: `npx tsc --noEmit 2>&1 | tail -15 && npm run build 2>&1 | tail -5` (green).
```bash
git add examples/ai-ledger/user-ui/src examples/ai-ledger/user-ui/dist
git commit -m "feat(dashboard/ui): project list + project view + note pages + money tab regroup"
```

---

## Task 8: Rebrand strings (Ledger → Dashboard)

**Files:** Modify `user-ui/src/pages/{Marketing,Login}.tsx`, `user-ui/index.html`, locales, any "Ledger" UI strings

- [ ] **Step 1: Rebrand UI**

Replace user-facing "Ledger"/"ai-ledger" → "Dashboard" (brand in header, Marketing copy → solo-operator cockpit pitch, Login, `index.html` title/meta, i18n `brand`). Keep finance terminology inside the money tab.

- [ ] **Step 2: Build + commit**

Run: `npm run build 2>&1 | tail -5` → confirm `<title>` updated.
```bash
git add examples/ai-ledger/user-ui/src examples/ai-ledger/user-ui/index.html examples/ai-ledger/user-ui/dist
git commit -m "feat(dashboard/ui): rebrand Ledger → Dashboard"
```

---

## Task 9: Rename crate / dir / binary / service

**Files:** `git mv examples/ai-ledger examples/dashboard`; Modify root `Cargo.toml` workspace members, `examples/dashboard/Cargo.toml`, `examples/dashboard/deploy/*`

- [ ] **Step 1: Move + rename**

```bash
cd /Users/liliang/Things/courses/harness
git mv examples/ai-ledger examples/dashboard
```
Edit `examples/dashboard/Cargo.toml`: `[package] name = "dashboard"`, `[[bin]] name = "dashboard"`. Update the root workspace `Cargo.toml` members (`examples/ai-ledger` → `examples/dashboard`). `include_dir!("$CARGO_MANIFEST_DIR/...")` auto-resolves — no change.

- [ ] **Step 2: Deploy files**

In `examples/dashboard/deploy/`: rename the systemd unit to `dashboard.service` (`Description`, `ExecStart=/opt/dashboard/dashboard --bind 127.0.0.1 --port <same as ledger>`, `EnvironmentFile=/etc/dashboard.env` or keep `/etc/ai-ledger.env` — decide; simplest: keep the env file path, just rename unit+binary). Update `install.sh` paths (`/opt/dashboard`).

- [ ] **Step 3: Build + commit**

Run: `cargo build -p dashboard 2>&1 | tail -10 && cargo test -p dashboard 2>&1 | tail -5` (green under the new name).
```bash
git add -A examples/dashboard Cargo.toml
git commit -m "chore(dashboard): rename crate/binary/dir/service ai-ledger → dashboard"
```

---

## Task 10: Build, manual verification, deploy

**Files:** none (integration)

- [ ] **Step 1: Full build + test**

```bash
cd /Users/liliang/Things/courses/harness/examples/dashboard/user-ui && npm run build 2>&1 | tail -4
cd /Users/liliang/Things/courses/harness && cargo build -p dashboard 2>&1 | tail -3 && cargo test -p dashboard 2>&1 | tail -4
```

- [ ] **Step 2: Manual golden path (local, real keys)**

Boot `dashboard` on a fresh DB. Verify: 4-tab nav (dashboard/money/project/profile); chat "开一个项目：上线 SaaS，9月底，每月复盘" → project appears under **project** tab; open it → add a note → note shows under the project; semantic search finds the note; 复盘 the project (review logged, cadence advances); **income/cost** tab still shows transactions + portfolio; **dashboard** tab still shows the finance overview; all existing ledger features (budgets/subscriptions/loans/receipts/net-worth/chat/memory/admin) still work. Styled confirm on deletes. Mobile + desktop.

- [ ] **Step 3: Deploy to qc-jp**

musl build (`dashboard`) in the builder container → scp → install `/opt/dashboard/dashboard` → install+enable `dashboard.service` (point `EnvironmentFile` at the existing ledger env so keys + DB path carry over, OR copy to `/etc/dashboard.env`) → restart → Caddy: keep `ledger.superleo.app` serving + add `dashboard.superleo.app` → the same upstream. Verify `/api/info` + the 4-tab app loads + existing ledger data still present.

---

## Self-review notes (for the executor)

- **Backend gate** after Task 4 (`cargo test -p ai-ledger` + endpoint smoke). Tasks 1-4 are additive — the crate compiles throughout (all new symbols).
- **Frontend** Tasks 5→7 build on each other (Task 6 may need 1-line page stubs until Task 7). Task 9's rename flips the package name from `ai-ledger` to `dashboard` — all `-p ai-ledger` commands become `-p dashboard` after Task 9.
- **Spec coverage:** projects/project_reviews/notes(project_id) ✔(T1); embeddings/search ✔(T2); project+note tools + prompt ✔(T3); REST ✔(T4); space dropped ✔ (throughout); 4-tab nav ✔(T6); project + note pages ✔(T7); rebrand ✔(T8); rename/deploy ✔(T9,T10); cockpit/P&L/migration correctly deferred (not in any task).
- **Type/name consistency:** `Project`/`ProjectReview`/`Note` (Rust) ↔ `Project`/`ProjectReview`/`Note` (TS) field names match; `ledgerApi.projects/project/createProject/...` consistent between T5 (def) and T7 (use); tool names `create_project`/`add_milestones`/`update_project`/`list_projects`/`log_project_review` + note tools consistent T3↔prompt.
- **Gemini-safe tool schemas** (no `["type","null"]`; optional omitted from `required`).
- **Reuse, don't reinvent:** every ported file has a concrete source path under `examples/ai-note/`; read it and apply the rename rules rather than writing from scratch.
- **The Prism eager-import** (T5 Step 2) and the **Card `flex-row`** fix (T7 Step 1) are known gotchas from the note build — don't skip them.
