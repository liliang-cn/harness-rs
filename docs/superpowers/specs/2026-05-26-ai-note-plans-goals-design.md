# ai-note Plans & Goals subsystem

**Date:** 2026-05-26
**Status:** approved (design), pending implementation plan

## Goal

Extend ai-note from note-capture into a **planning / goal-tracking** assistant.
The user states a goal or a standing rule in natural language; the agent
records it, can decompose it into sub-goals, and drives **periodic review
(复盘)**. All authoring (capture / decompose / review) happens through the chat
agent — natural language, no forms. A new **Plans** page surfaces goals for
browsing and shows a "到期复盘" (due-for-review) list.

Motivating examples (from the user):
- "今年9月成为企业级高可用的架构专家" → an aspirational goal with a target date,
  later decomposed into sub-goals, reviewed monthly.
- "股票不要操作" → a standing rule (no deadline) that surfaces as a principle.

## Decisions (locked during brainstorming)

1. **Reminders are phased.** Phase 1 (this spec) = **in-app surface only** — when
   the user opens ai-note, the Plans page shows goals whose review is due. No
   push/email. Phase 2 (NOT this spec) = `harness-rs-daemon` scheduled review
   generation + optional email.
2. **Goals are first-class** — a new `goals` table (not notes-with-tags).
3. **Per-goal review cadence** — each goal carries `review_interval_days` +
   `next_review_at`; the due list is `next_review_at <= now`. Logging a review
   advances `next_review_at` by the interval.
4. **Reviews + recursive decomposition** — a `goal_reviews` table records each
   review (progress + next steps); sub-goals are `goals` rows with `parent_id`.

## Non-goals (YAGNI / phase 2)

- No daemon-scheduled reviews, email, or push notifications in v1.
- No calendar integration, sharing, or gamification.
- No cross-space goal view (goals belong to exactly one space, like notes).
- No separate "habit/streak" kind — only `goal` and `rule` in v1.

## Data model

Both tables are **space-scoped** (`work`/`life`) like notes, and migrated via
the existing `ensure_column` helper / `CREATE TABLE IF NOT EXISTS`.

### `goals`
| column | type | notes |
|---|---|---|
| `id` | TEXT PK | `random_id()` |
| `user_id` | TEXT | owner |
| `space` | TEXT NOT NULL DEFAULT 'life' | 'work' \| 'life' |
| `kind` | TEXT NOT NULL DEFAULT 'goal' | 'goal' \| 'rule' |
| `title` | TEXT NOT NULL | short headline |
| `detail` | TEXT NOT NULL DEFAULT '' | markdown |
| `status` | TEXT NOT NULL DEFAULT 'active' | 'active' \| 'done' \| 'dropped' \| 'paused' |
| `parent_id` | TEXT NULL | → `goals.id`; NULL = top-level. Recursive decomposition. |
| `target_date` | TEXT NULL | RFC3339 date (e.g. `2026-09-30`); NULL for rules |
| `review_interval_days` | INTEGER NULL | e.g. 7 / 30; NULL = no cadence (rules) |
| `next_review_at` | TEXT NULL | RFC3339; drives the due list; NULL = never auto-due |
| `created_at` / `updated_at` | TEXT NOT NULL | RFC3339 |

Indexes: `(user_id, space, status)` for listing; `(user_id, next_review_at)`
for the due query; `(parent_id)` for subgoal fetch.

`kind='goal'`: aspirational; normally has `target_date` + `review_interval_days`
(→ `next_review_at` seeded at creation = now + interval).
`kind='rule'`: standing constraint; `target_date`/`review_interval_days`/
`next_review_at` typically NULL; surfaces in a 原则/规矩 section, never in the
due list.

### `goal_reviews`
| column | type | notes |
|---|---|---|
| `id` | TEXT PK | |
| `goal_id` | TEXT NOT NULL | → `goals.id` (CASCADE delete) |
| `user_id` | TEXT NOT NULL | |
| `progress` | TEXT NOT NULL | markdown: what happened / self-assessment |
| `next_steps` | TEXT NOT NULL DEFAULT '' | markdown |
| `created_at` | TEXT NOT NULL | RFC3339 |

Logging a review: insert the row, then set the goal's
`next_review_at = now + review_interval_days` (if the goal has an interval) and
bump `updated_at`.

When a goal is deleted, its subgoals (by `parent_id`) and reviews are deleted
too (handled in the delete helper, not necessarily FK CASCADE).

## Backend (Rust)

### db.rs
- `ensure_column` migrations + `CREATE TABLE IF NOT EXISTS` for `goals`,
  `goal_reviews`.
- Structs: `Goal`, `GoalReview` (serde Serialize; dates via `ser_rfc3339`).
- Helpers (all `user_id`-scoped):
  - `create_goal(user_id, &NewGoal) -> Goal` (NewGoal carries kind/title/detail/
    space/parent_id/target_date/review_interval_days/next_review_at)
  - `list_goals(user_id, space, status: Option<&str>, only_due: bool) -> Vec<Goal>`
    (`only_due` → `next_review_at IS NOT NULL AND next_review_at <= now`)
  - `get_goal(user_id, id) -> Option<Goal>`
  - `list_subgoals(user_id, parent_id) -> Vec<Goal>`
  - `update_goal(user_id, id, patch fields…) -> u32` (COALESCE pattern)
  - `delete_goal(user_id, id)` — deletes the goal + its direct subgoals + its
    reviews
  - `count_due_goals(user_id, space) -> u32` (for the nav badge)
  - `add_review(user_id, goal_id, progress, next_steps) -> GoalReview` + advance
    `next_review_at`
  - `list_reviews(user_id, goal_id, limit) -> Vec<GoalReview>`

### server.rs
Routes (space-aware, behind `AuthCtx`):
- `GET  /api/goals?space=&filter=active|due|all` (default `active`)
- `POST /api/goals`  (body: kind/title/detail/space/parent_id?/target_date?/
  review_interval_days?)
- `GET  /api/goals/:id` → `{ goal, subgoals, reviews }`
- `PATCH /api/goals/:id` (status/title/detail/target_date/review_interval_days)
- `DELETE /api/goals/:id`
- `POST /api/goals/:id/reviews` (body: progress, next_steps?, next_review_in_days?)

These power the Plans page (browsing + manual status edits + the due list). The
nav badge uses `count_due_goals`.

### tools.rs + SYSTEM_PROMPT (the NL surface — primary authoring path)
New `#[harness::tool]`s, scoped to the active space via the existing
`space_of(w)` helper:
- `create_goal` — args: `kind` ("goal"|"rule"), `title`, `detail?`, `target_date?`
  (RFC3339), `review_interval_days?`, `parent_id?`. Reads space from context.
- `decompose_goal` — args: `parent_id`, `subgoals` (array of `{title, detail?}`).
  Bulk-creates child goals (kind='goal', same space, inherit nothing else).
- `update_goal` — args: `id`, optional `status`/`title`/`detail`/`target_date`/
  `review_interval_days`.
- `list_goals` — args: `status?`, `due_for_review?` (bool), `parent_id?`. Returns
  goals in the active space.
- `log_review` — args: `goal_id`, `progress`, `next_steps?`, `next_review_in_days?`.

SYSTEM_PROMPT additions (new rules, after the existing space rule):
- When the user states an aspiration with or without a date ("我要…", "今年X月…",
  "三个月内…"), call `current_time` FIRST to resolve relative dates, then
  `create_goal(kind='goal', target_date=…, review_interval_days=…)`. Default
  cadence: monthly (30) unless the user implies otherwise.
- When the user states a standing rule / 戒律 ("股票不要操作", "每天早睡", "不要…"),
  call `create_goal(kind='rule')` (no target_date / interval).
- When the user asks to break a goal down ("拆解一下", "分解成几步"), call
  `decompose_goal` with sensible sub-goals.
- When the user says "复盘" / "review" / "进展如何", first `list_goals(due_for_review)`,
  walk the due goals, then `log_review` for each discussed, summarizing progress
  + next steps in the user's words.
- All goal ops are scoped to the current space; never mix spaces.

## Frontend

### api.ts
Types `Goal`, `GoalReview`; helpers: `goals(space, filter)`, `goal(id)`,
`createGoal`, `updateGoal`, `deleteGoal`, `addReview`, plus the due count comes
from the `due` filter length. (Authoring is NL-first via chat; these power the
Plans page + light edits.)

### Nav + Plans page
- Add a **4th nav item 计划/Plans** (`/app/plans`) to `app-shell.tsx` NAV
  (icon e.g. `Target`), both desktop tabs and mobile bottom nav. The nav label
  may show a small badge when due-count > 0.
- `pages/Plans.tsx` (space-scoped via `useSpace`):
  - **到期复盘 (N)** section at top — goals where `next_review_at <= now`. Each
    has a "复盘" button that opens the chat sheet prefilled with e.g.
    "复盘：{goal.title}" (reuse the chat FAB/sheet; prefill via a shared opener).
  - **目标 (Goals)** — active `kind='goal'` rows: title, target-date countdown,
    subgoal-completion progress (e.g. 2/5), status chip. Tap → detail.
  - **原则 / 规矩 (Rules)** — `kind='rule'` rows.
- `components/plans/goal-detail.tsx` (Sheet): goal title/detail (markdown via the
  existing `renderMarkdown`), **subgoal list** with check-off (PATCH status→done),
  **review timeline** (goal_reviews newest-first), and actions: edit (status/
  target_date/cadence), mark done, delete. Quick "add subgoal" + "复盘" buttons
  open the chat prefilled (NL path) rather than forms.
- Optional: the Notes page header or Plans nav shows a due-review badge.

### Chat prefill hook
The chat sheet (`chat-sheet.tsx`) gains a way to open with prefilled composer
text (e.g. via a small module-level event/store or a context). Plans page "复盘"
/ "add subgoal" buttons call it. Keep minimal — a `useChatPrefill()` store with
`openWith(text)` that the ChatFab/ChatSheet subscribes to.

## Data flow (the user's example)

1. Chat (work space): "今年9月成为企业级高可用的架构专家" → agent `current_time`
   → `create_goal(kind='goal', space='work', target_date='2026-09-30',
   review_interval_days=30)` → `next_review_at = now+30d`.
2. "股票不要操作" → `create_goal(kind='rule', space=…)`.
3. "把架构专家这个目标拆解一下" → `decompose_goal(parent_id, [打牢分布式系统基础,
   主导一次高可用改造, 考取相关认证, 沉淀方法论文档])`.
4. A month later the goal's `next_review_at` passes → it appears in **到期复盘**
   on the Plans page (and the nav badge). User taps 复盘 → chat → discusses →
   `log_review(progress, next_steps)` → `next_review_at += 30d`.
5. Plans detail shows the subgoal tree + the growing review timeline.

## Testing / verification

- `cargo test -p ai-note` — db unit tests: create goal, list active vs due
  (time-based), decompose (parent_id), add_review advances next_review_at,
  delete cascades subgoals+reviews, space scoping.
- `cargo build -p ai-note` + `cd user-ui && npm run build` + `tsc --noEmit` clean.
- Manual (golden path) via the running app: chat-capture a goal in work space →
  appears on Plans → decompose via chat → subgoals show → set a short cadence,
  confirm it lands in 到期复盘 → 复盘 via chat → review logged + due advances →
  rule capture shows under 原则; switch to life space → work goals hidden.
- Regression: notes / search / existing chat tools still work; `/admin` loads.

## Rollout

Same as the rest of ai-note: musl cross-compile → qc-jp (`note.superleo.app`,
`127.0.0.1:6755`, Caddy). In-app only — no new infra. Phase 2 (daemon/email) is
a separate spec.
