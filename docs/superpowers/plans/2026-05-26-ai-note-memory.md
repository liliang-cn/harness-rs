# ai-note long-term memory Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire harness-rs's long-term memory subsystem into ai-note's streaming chat (per-user JSONL, dedup/guarded, auto-distilled via a cheap model, recalled into the prompt, plus 3 chat tools), and add a Profile "AI 记得我什么" management view. Mirrors ai-ledger's proven wiring.

**Architecture:** Per-user `FileMemory` (one JSONL/user, no space split) → `GuardedMemory` → `MemoryGuide` (recall→inject) + `MemorySynthesizer` (deepseek-v4-flash) + `remember_this`/`list_memories`/`forget_memory` tools, wired into `session_stream_handler`'s spawned task. REST endpoints + a Profile card let the user inspect/forget memories.

**Tech Stack:** Rust (axum, harness-rs: `harness-context`/`harness-loop`/`harness-tools-memory`), React 19 + Vite + shadcn.

**Spec:** `docs/superpowers/specs/2026-05-26-ai-note-memory-design.md`
**Reference (port from):** `examples/ai-ledger/src/server.rs` — memory block in `session_stream_handler` (lines ~1793-1847), `memory_path_for` (~1686), `LEDGER_MEMORY_INSTRUCTIONS` (~52), memory endpoints `list_memories_handler`/`delete_memory_handler`/`delete_all_memories_handler` (~1548-1621), routes (~272-275).

**Working dir:** `examples/ai-note/`. Repo root: `/Users/liliang/Things/courses/harness`.

**Key adaptation vs ledger:** ai-ledger uses a global `ledger_path()`; ai-note has no such global — pass `s.db_path` explicitly. ai-note's `AppState::build_model_for(id)` already returns `Arc<dyn Model>` (no extra `Arc::new` needed). ai-note's `session_stream_handler` spawned task already binds `uid` (user id), `s` (moved AppState), and builds `let mut loop_ = AgentLoop::new(crate::AnyModelHandle(model)).with_streaming(true).with_guide(Arc::new(SystemPromptGuide));` then `for t in harness_core::iter_macro_tools() { loop_ = loop_.with_tool(t); }` then the `ChannelHook` hook — READ the current handler before editing to confirm these names.

---

## Task 1: Backend — wire memory into chat + inspection endpoints

**Files:** Modify `examples/ai-note/Cargo.toml`, `examples/ai-note/src/server.rs`

- [ ] **Step 1: Add the dependency**

In `examples/ai-note/Cargo.toml`, under `[dependencies]` (next to the other `harness-*` lines):

```toml
harness-tools-memory = { workspace = true }
```

- [ ] **Step 2: Add `memory_path_for` + `NOTE_MEMORY_INSTRUCTIONS`**

In `server.rs` (near the top-level helpers / consts), add:

```rust
/// Per-user JSONL path for `harness-core::Memory`. One file per user → strict
/// isolation without the trait knowing about users. Lives next to the DB.
pub(crate) fn memory_path_for(db_path: &std::path::Path, user_id: &str) -> std::path::PathBuf {
    let base = db_path.parent().map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("memory").join(format!("{user_id}.jsonl"))
}

/// Guidance prepended to the MemorySynthesizer prompt: what counts as a durable
/// fact for a personal note/plan app, and what to skip.
const NOTE_MEMORY_INSTRUCTIONS: &str = "\
This is a personal note-taking + planning agent. The user's NOTES and GOALS are \
ALREADY stored in their own tables — DO NOT re-store individual note bodies or \
goal text as memory facts; that's noise.\n\
\n\
ONLY emit facts in these categories:\n\
- **stable preferences**: how the user likes to work or be replied to \
  ('偏好简短中文回复', '喜欢用 markdown 列表整理'), review cadence preferences \
  ('偏好月度复盘')\n\
- **long-term direction / focus**: what the user is working toward at a high \
  level ('在攻企业级高可用架构', '今年重点是转架构')\n\
- **recurring personal context** (mentioned ≥2 times or clearly durable): \
  role, domain, tools they live in\n\
\n\
NEVER store: raw note content, secrets/passwords/tokens, or transient \
session-scoped details. Keep each fact one concise sentence.";
```

- [ ] **Step 3: Wire the memory block into `session_stream_handler`**

First READ `session_stream_handler` in `server.rs` to confirm the spawned-task variable names (`uid`, `s`, and the `let mut loop_ = AgentLoop::new(...)...` line). Then, inside the spawned task, AFTER the `loop_` is created with `.with_guide(Arc::new(SystemPromptGuide))` and BEFORE the `for t in harness_core::iter_macro_tools()` loop, insert (adapted from ledger):

```rust
        // ─── Long-term memory: per-user FileMemory + write guards ───
        let mem_path = memory_path_for(&s.db_path, &uid);
        if let Ok(file_mem) = harness_context::FileMemory::open(&mem_path) {
            let file_arc = std::sync::Arc::new(file_mem);
            let guarded: std::sync::Arc<dyn harness_core::Memory> = std::sync::Arc::new(
                harness_context::GuardedMemory::new(file_arc.clone())
                    .with_dedup_threshold(0.6),
            );
            loop_ = loop_.with_guide(std::sync::Arc::new(
                harness_loop::MemoryGuide::new(guarded.clone())
                    .with_top_k(5)
                    .with_min_score(0.25)
                    .with_excluded_tags(["synth-raw", "transient"]),
            ));
            loop_ = loop_
                .with_tool(std::sync::Arc::new(harness_tools_memory::RememberThisTool::with_source(
                    guarded.clone(),
                    format!("ai-note/user-{uid}/explicit"),
                )))
                .with_tool(std::sync::Arc::new(harness_tools_memory::ListMemoriesTool::new(
                    guarded.clone(),
                )))
                .with_tool(std::sync::Arc::new(harness_tools_memory::ForgetMemoryTool::new(
                    file_arc.clone() as std::sync::Arc<dyn harness_tools_memory::MemoryDelete>,
                )));
            // Cheap synth model for auto-distillation; skip if unavailable.
            if let Ok(synth_model) = s.build_model_for("deepseek-v4-flash") {
                loop_ = loop_.with_hook(std::sync::Arc::new(
                    harness_loop::MemorySynthesizer::new(guarded.clone(), synth_model)
                        .with_source(format!("ai-note/user-{uid}"))
                        .with_max_facts(3)
                        .with_extra_instructions(NOTE_MEMORY_INSTRUCTIONS),
                ));
            }
        } else {
            tracing::warn!(path = %mem_path.display(), "memory open failed; chat will not persist facts");
        }
```

Notes:
- `s.build_model_for("deepseek-v4-flash")` returns `Arc<dyn Model>` already → pass directly to `MemorySynthesizer::new(guarded, synth_model)` (do NOT wrap in another `Arc::new`).
- Use fully-qualified `std::sync::Arc` (or rely on the existing `use std::sync::Arc;` already imported in server.rs — if so, drop the `std::sync::` prefix to match the file's style).
- If `uid` isn't the exact binding name in the spawned task, use whatever the handler bound the user id to. Same for `s`.

- [ ] **Step 4: Add the inspection endpoint handlers**

Add to `server.rs` (these read/write the JSONL directly; each takes `State(s)` to resolve the path):

```rust
async fn list_memories_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<Value>, ApiError> {
    let path = memory_path_for(&s.db_path, &auth.user.id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(ApiError::Internal(format!("memory read: {e}"))),
    };
    let mut entries: Vec<Value> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Ok(v) = serde_json::from_str::<Value>(line) { entries.push(v); }
    }
    entries.sort_by(|a, b| {
        b.get("created_ms").and_then(|v| v.as_i64()).unwrap_or(0)
            .cmp(&a.get("created_ms").and_then(|v| v.as_i64()).unwrap_or(0))
    });
    Ok(Json(json!({ "count": entries.len(), "memories": entries })))
}

async fn delete_all_memories_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
) -> Result<Json<Value>, ApiError> {
    let path = memory_path_for(&s.db_path, &auth.user.id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(Json(json!({ "deleted": 0 }))),
    };
    let n = raw.lines().filter(|l| !l.trim().is_empty()).count() as u32;
    std::fs::write(&path, "").map_err(|e| ApiError::Internal(format!("write: {e}")))?;
    Ok(Json(json!({ "deleted": n })))
}

async fn delete_memory_handler(
    State(s): State<AppState>,
    auth: AuthCtx,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let path = memory_path_for(&s.db_path, &auth.user.id);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Err(ApiError::BadRequest("no memories file".into())),
    };
    let mut kept: Vec<String> = Vec::new();
    let mut removed = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let entry_id = serde_json::from_str::<Value>(trimmed).ok()
            .and_then(|v| v.get("id").and_then(|x| x.as_str()).map(String::from))
            .unwrap_or_default();
        if entry_id == id { removed = true; continue; }
        kept.push(line.to_string());
    }
    if !removed { return Err(ApiError::BadRequest(format!("no memory `{id}`"))); }
    let mut new_content = kept.join("\n");
    if !new_content.is_empty() { new_content.push('\n'); }
    std::fs::write(&path, new_content).map_err(|e| ApiError::Internal(format!("write: {e}")))?;
    Ok(Json(json!({ "deleted": id })))
}
```

- [ ] **Step 5: Register the routes**

In `serve()`, next to the other `/api/me/*` routes, add:

```rust
        .route(
            "/api/me/memories",
            get(list_memories_handler).delete(delete_all_memories_handler),
        )
        .route(
            "/api/me/memories/:id",
            axum::routing::delete(delete_memory_handler),
        )
```

- [ ] **Step 6: Build + test + smoke**

```bash
cd /Users/liliang/Things/courses/harness && cargo build -p ai-note 2>&1 | tail -20 && cargo test -p ai-note 2>&1 | tail -5
```
Expected: compiles (0 errors), 9 tests still pass.

Smoke (memory endpoint reachable, empty to start):
```bash
GEMINI_API_KEY=${GEMINI_API_KEY:-dummy} DEEPSEEK_API_KEY=${DEEPSEEK_API_KEY:-dummy} \
  HARNESS_NOTE_DB=/tmp/ainote-mem.db cargo run -p ai-note -- --port 6755 &
sleep 4
TOK=$(curl -s -X POST localhost:6755/api/register -H 'Content-Type: application/json' -d '{"email":"m@t.co","password":"pw123456"}' | python3 -c "import sys,json;print(json.load(sys.stdin)['token'])")
curl -s localhost:6755/api/me/memories -H "Authorization: Bearer $TOK"; echo
kill %1; rm -f /tmp/ainote-mem.db*
```
Expected: `{"count":0,"memories":[]}`.

- [ ] **Step 7: Commit**

```bash
git add examples/ai-note/Cargo.toml examples/ai-note/src/server.rs
git commit -m "feat(ai-note): long-term memory in chat (FileMemory + guide + synthesizer + tools) + inspection endpoints"
```

---

## Task 2: Frontend — memory api + Profile "AI 记得我什么" card

**Files:** Modify `examples/ai-note/user-ui/src/lib/api.ts`, `examples/ai-note/user-ui/src/pages/Profile.tsx`, `examples/ai-note/user-ui/src/locales/{en,zh}.json`

- [ ] **Step 1: api.ts — type + helpers**

Add to `src/lib/api.ts` (type after the other interfaces; methods inside the `noteApi` object):

```ts
export interface Memory {
  id: string;
  content: string;
  tags?: string[];
  source?: string | null;
  created_ms: number;
  expires_ms?: number | null;
}
```
```ts
  memories: () => req<{ count: number; memories: Memory[] }>('/api/me/memories'),
  forgetMemory: (id: string) =>
    req<{ deleted: string }>(`/api/me/memories/${id}`, { method: 'DELETE' }),
  clearMemories: () =>
    req<{ deleted: number }>('/api/me/memories', { method: 'DELETE' }),
```

- [ ] **Step 2: i18n keys**

In both `src/locales/en.json` and `zh.json`, add under the existing `profile` object a `memory` sub-block. en.json:
```json
"memory": {
  "title": "What the assistant remembers",
  "empty": "Nothing remembered yet. Chat naturally and it'll learn your preferences.",
  "clear": "Clear all",
  "clearConfirm": "Forget everything the assistant has remembered?",
  "deleted": "Forgotten"
}
```
zh.json:
```json
"memory": {
  "title": "AI 记得我什么",
  "empty": "还没有记忆。自然地聊，它会慢慢记住你的偏好。",
  "clear": "清空",
  "clearConfirm": "忘记 AI 记住的全部内容？",
  "deleted": "已忘记"
}
```
(Add the comma after the previous `profile` key as needed so the JSON stays valid.)

- [ ] **Step 3: Profile.tsx — memory card**

Add a memory section to the existing Profile page. Read the current `Profile.tsx` first to match its imports/structure, then add (adapt imports — `useEffect`/`useState` likely already imported; add `Trash2` from lucide-react, `toast` from sonner, `format`/`parseISO` from date-fns if you show dates — note `created_ms` is epoch ms, so use `new Date(m.created_ms)`):

```tsx
function MemoryCard() {
  const { t } = useTranslation();
  const [mems, setMems] = useState<Memory[] | null>(null);
  const load = useCallback(() => {
    noteApi.memories().then((j) => setMems(j.memories)).catch(() => setMems([]));
  }, []);
  useEffect(load, [load]);
  async function forget(id: string) {
    try { await noteApi.forgetMemory(id); setMems((c) => c?.filter((m) => m.id !== id) ?? null); toast.success(t('profile.memory.deleted')); }
    catch (e) { toast.error((e as Error).message); }
  }
  async function clearAll() {
    if (!confirm(t('profile.memory.clearConfirm'))) return;
    try { await noteApi.clearMemories(); setMems([]); toast.success(t('profile.memory.deleted')); }
    catch (e) { toast.error((e as Error).message); }
  }
  return (
    <Card className="space-y-2 p-4">
      <div className="flex items-center justify-between">
        <div className="text-sm font-medium">{t('profile.memory.title')}</div>
        {mems && mems.length > 0 && (
          <Button variant="ghost" size="sm" onClick={clearAll}>{t('profile.memory.clear')}</Button>
        )}
      </div>
      {mems === null ? (
        <Skeleton className="h-12 w-full" />
      ) : mems.length === 0 ? (
        <p className="text-muted-foreground text-xs">{t('profile.memory.empty')}</p>
      ) : (
        <ul className="space-y-1.5">
          {mems.map((m) => (
            <li key={m.id} className="flex items-start justify-between gap-2 text-sm">
              <div className="min-w-0 flex-1">
                <div className="break-words">{m.content}</div>
                <div className="text-muted-foreground text-[11px]">
                  {new Date(m.created_ms).toLocaleDateString()}
                </div>
              </div>
              <Button variant="ghost" size="icon-sm" aria-label="forget" onClick={() => forget(m.id)}>
                <Trash2 className="size-4" />
              </Button>
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}
```

Render `<MemoryCard />` inside the Profile page (after the model-picker card). Add the needed imports: `useCallback`, `Skeleton` (`@/components/ui/skeleton`), `Trash2`, `toast`, and `type Memory` from `@/lib/api`. Confirm `Card`/`Button` are already imported in Profile.tsx (they are).

- [ ] **Step 4: Build**

```bash
cd /Users/liliang/Things/courses/harness/examples/ai-note/user-ui && npx tsc --noEmit 2>&1 | tail -15 && npm run build 2>&1 | tail -5
```
Expected: tsc clean, build green.

- [ ] **Step 5: Commit**

```bash
git add examples/ai-note/user-ui/src examples/ai-note/user-ui/dist
git commit -m "feat(ai-note/ui): Profile 'what the assistant remembers' card (view/forget/clear)"
```

---

## Task 3: Build, manual verification, deploy

**Files:** none (integration)

- [ ] **Step 1: Full build + test**

```bash
cd /Users/liliang/Things/courses/harness/examples/ai-note/user-ui && npm run build 2>&1 | tail -4
cd /Users/liliang/Things/courses/harness && cargo build -p ai-note 2>&1 | tail -3 && cargo test -p ai-note 2>&1 | tail -4
```
Expected: all green.

- [ ] **Step 2: Manual golden path (local, real keys)**

```bash
GEMINI_API_KEY=$GEMINI_API_KEY DEEPSEEK_API_KEY=$DEEPSEEK_API_KEY \
  HARNESS_NOTE_DB=/tmp/ainote-mem.db cargo run -p ai-note -- --port 6755
```
In the browser (or Playwright):
- Register/login → chat (FAB): "记住：我偏好每月复盘一次，回复尽量简短中文" → agent calls `remember_this` (or the synthesizer distills it).
- Start a NEW chat → ask "你记得我什么？" → agent recalls the preference (proves recall→inject).
- Profile → **AI 记得我什么** → the preference is listed → delete it → it disappears.
- Confirm a fresh user with no memory file still chats fine (no error).
- Regression: notes / search / goals / one-shot chat still work.

- [ ] **Step 3: Deploy to qc-jp**

```bash
docker start ai-ledger-builder 2>/dev/null; \
docker exec ai-ledger-builder bash -lc 'export PATH=/usr/local/cargo/bin:$PATH && cd /work && cargo build --release --target x86_64-unknown-linux-musl -p ai-note 2>&1 | tail -3'
cd /Users/liliang/Things/courses/harness
scp -q target-musl/x86_64-unknown-linux-musl/release/ai-note qc-jp:/tmp/ai-note.new
ssh qc-jp 'sudo install -m 0755 /tmp/ai-note.new /opt/ai-note/ai-note && sudo systemctl restart ai-note && sleep 3 && systemctl is-active ai-note && rm -f /tmp/ai-note.new'
```
Verify: `curl -s https://note.superleo.app/api/info` ok; load the site, chat "记住X", new chat "你记得我什么", confirm recall + Profile card. (Memory JSONL lives under `/var/lib/ai-note/memory/` on the server — created on first write.)

---

## Self-review notes (for the executor)

- **Backend gate is end of Task 1** (`cargo build` + `cargo test` + the empty-memories smoke).
- **Spec coverage:** FileMemory per-user ✔(T1 memory_path_for); GuardedMemory dedup ✔; MemoryGuide recall→inject ✔; MemorySynthesizer (deepseek-flash, skip if absent) ✔; 3 chat tools ✔; inspection endpoints (list/delete-one/clear) ✔(T1); shared-not-space-split ✔ (path keyed by user only); streaming-path-only ✔ (one-shot `/api/chat` untouched); mgmt UI ✔(T2).
- **Type consistency:** `noteApi.memories/forgetMemory/clearMemories` names match T2 defs ↔ usage; `Memory` fields match the JSONL `MemoryEntry` serialized shape (`id`/`content`/`tags`/`source`/`created_ms`/`expires_ms`).
- **Adaptation reminders:** `memory_path_for(&s.db_path, uid)` (no global path helper in ai-note); `build_model_for` returns `Arc<dyn Model>` (no double Arc); confirm the spawned-task binding names (`uid`, `s`) by reading the handler first; if `use std::sync::Arc;` is already in server.rs, drop the `std::sync::` qualifier to match file style.
- **No DB migration** — memory is file-based JSONL, created on first write.
