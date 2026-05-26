# ai-note long-term memory

**Date:** 2026-05-26
**Status:** approved (design), pending implementation plan

## Goal

Give ai-note's chat agent cross-session **long-term memory**, using harness-rs's
existing memory subsystem (the same stack ai-ledger already runs). The agent
should remember durable personal facts/preferences/goals/working-style across
conversations ("偏好月度复盘", "在攻高可用方向", "喜欢简洁回复"), surface them
into future prompts, and let the user inspect/forget them.

Reference implementation: `examples/ai-ledger/src/server.rs` (the memory block
in `session_stream_handler` + `memory_path_for` + the `/api/me/memories`
endpoints).

## Decisions (locked during brainstorming)

1. **Shared per-user memory** — one JSONL file per user, NOT split by
   work/life space (most preferences are person-level; simplest; matches ledger).
2. **Management UI** — add a Profile "AI 记得我什么" section (list / delete one /
   clear all), backed by REST endpoints.
3. **Synthesizer ON** — auto-distill durable facts via `deepseek-v4-flash`
   after each turn (skip gracefully if no deepseek key).
4. **Streaming path only** — memory is wired into `session_stream_handler`. The
   one-shot `/api/chat` fallback stays memory-less.

## Non-goals (YAGNI)

- No space-split memory (one shared store per user).
- No semantic/vector recall — `FileMemory`'s keyword recall is enough at
  personal scale; the `Memory` trait stays swappable later.
- No memory on the one-shot `/api/chat` endpoint.

## Backend

### Cargo.toml
Add `harness-tools-memory = { workspace = true }`. (`harness-core`,
`harness-loop`, `harness-context` are already deps.)

### server.rs

**`memory_path_for(user_id) -> PathBuf`** — `{db_path parent}/memory/{user_id}.jsonl`.
One file per user gives strict isolation without the `Memory` trait knowing
about users.

**`NOTE_MEMORY_INSTRUCTIONS: &str`** — extra prompt for the synthesizer:
- DO store: durable personal preferences, goals/direction, working style, recurring
  context ("偏好月度复盘", "在攻企业级高可用", "喜欢简短中文回复").
- DON'T store: raw note content (notes live in the `notes` table), secrets/
  passwords, transient/session-scoped task params.

**`session_stream_handler`** — inside the spawned task (after the loop is built,
before tools are added), mirror ledger:
- `FileMemory::open(memory_path_for(user_id))` → `Arc`.
- `GuardedMemory::new(file_arc.clone()).with_dedup_threshold(0.6)` (+ block
  obvious secret substrings like "password"/"token" if the builder supports it;
  otherwise rely on the default sensitivity patterns) → `Arc<dyn Memory>`.
- `loop_ = loop_.with_guide(Arc::new(MemoryGuide::new(guarded.clone())
    .with_top_k(5).with_min_score(0.25)
    .with_excluded_tags(["synth-raw", "transient"])))`.
- Three tools wired to this user's store:
  `RememberThisTool::with_source(guarded.clone(), "ai-note/user-{uid}/explicit")`,
  `ListMemoriesTool::new(guarded.clone())`,
  `ForgetMemoryTool::new(file_arc.clone() as Arc<dyn MemoryDelete>)`.
- Synthesizer: `if let Ok(synth) = s.build_model_for("deepseek-v4-flash")` →
  `loop_.with_hook(Arc::new(MemorySynthesizer::new(guarded.clone(), Arc::new(synth))
    .with_source("ai-note/user-{uid}").with_max_facts(3)
    .with_extra_instructions(NOTE_MEMORY_INSTRUCTIONS)))`. If the synth model
  can't be built, log a warning and skip (chat + recall still work).
- If `FileMemory::open` fails, log a warning and skip the whole memory block
  (chat still works) — never fail the request because memory is unavailable.

**Endpoints** (port ledger's handlers, reading/writing the JSONL directly):
- `GET /api/me/memories` → `{count, memories: [...]}`, newest first (sort by
  `created_ms` desc).
- `DELETE /api/me/memories/:id` → remove the entry with that id; 400 if absent.
- `DELETE /api/me/memories` → clear all (truncate the file); returns `{deleted: N}`.

(`MemorySynthesizer`/`MemoryGuide`/`FileMemory`/`GuardedMemory` come from
`harness_loop` / `harness_context`; `RememberThisTool`/`ListMemoriesTool`/
`ForgetMemoryTool`/`MemoryDelete` from `harness_tools_memory`. `build_model_for`
already exists on `AppState`.)

## Frontend

### api.ts
```ts
export interface Memory {
  id: string; content: string; tags?: string[];
  source?: string | null; created_ms: number; expires_ms?: number | null;
}
```
Helpers on `noteApi`: `memories()` → `{count, memories: Memory[]}`,
`forgetMemory(id)`, `clearMemories()`.

### Profile.tsx
Add a card **"AI 记得我什么" / "What the assistant remembers"**:
- Fetch on mount; show each memory's `content` + relative date, with a per-item
  delete (Trash) button.
- A "清空 / Clear all" button (confirm dialog) → `clearMemories()`.
- Empty state when none.
- i18n keys under `profile.memory.*` (title/empty/clear/clearConfirm/deleted) in
  both en/zh.

## Data flow

1. User sends a chat message (streaming path).
2. `MemoryGuide` recalls top-5 relevant past facts for this user → injects into
   the system prompt → the agent "remembers".
3. The agent answers; if the user said "记住X" it calls `remember_this`; "你记得
   我什么" → `list_memories`; "忘记X" → `forget_memory`.
4. After the turn, `MemorySynthesizer` distills ≤3 durable facts via
   deepseek-v4-flash; `GuardedMemory` dedups + blocks secrets; appends to the
   user's JSONL.
5. User can review/delete everything in Profile → AI 记得我什么.

## Testing / verification

- `cargo build -p ai-note` + `cargo test -p ai-note` clean; `npm run build` +
  `tsc --noEmit` clean.
- Manual (real keys): chat "记住我偏好每月复盘一次" → new session, ask "你记得我
  什么" → the agent recalls it; Profile → AI 记得我什么 lists it; delete it →
  gone; confirm a normal chat still works if no memory file yet.
- Regression: notes / search / goals / existing chat still work; one-shot
  `/api/chat` unaffected; `/admin` loads.

## Rollout

Same as the rest of ai-note: musl cross-compile → qc-jp (`note.superleo.app`,
`127.0.0.1:6755`, Caddy). The per-user JSONL files live under the DB's parent
dir (`/var/lib/ai-note/memory/` on the server) — created on first write, no
migration needed.
