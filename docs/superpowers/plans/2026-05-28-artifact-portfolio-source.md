# Portfolio Artifact Data Source — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `portfolio` data source to the dashboard's AI artifact renderer so the assistant can render the user's own investments (holdings/allocation/net-worth) as a sandboxed generated page.

**Architecture:** Pure extension of the existing renderer. `render_artifact` accepts `data.source: "portfolio"` (no `id`); the frontend data-source registry fetches all portfolio + net-worth data host-side and injects it as `window.DATA`. Card → sandbox → injection pipeline is reused unchanged.

**Tech Stack:** Rust (`#[harness::tool]`, axum), React 19 + Vite + TS.

**Spec:** `docs/superpowers/specs/2026-05-28-artifact-portfolio-source-design.md`

**Note (correction vs spec):** the spec says "add 2 helpers (`portfolioSummary`, `portfolioAllocation`)". In reality `ledgerApi.allocation()` already exists (`api.ts:451`), so only **one** new helper is needed: `summary()`. There is no JS unit-test runner; verify with `cargo build`, `tsc --noEmit`, and the live golden path.

---

## Task 1: Backend — `render_artifact` accepts `portfolio`, `id` optional + prompt

**Files:**
- Modify: `examples/dashboard/src/tools.rs` (`render_artifact` ~1930-1986)
- Modify: `examples/dashboard/src/main.rs` (`SYSTEM_PROMPT` ARTIFACTS section ~433-448)

- [ ] **Step 1: Update the tool schema** — in `tools.rs`, change the `source` enum + make `id` optional:

Replace:
```rust
            "source": { "type": "string", "enum": ["project"], "description": "Data source; only 'project' is supported" },
            "id": { "type": "string", "description": "The project id to bind" }
          },
          "required": ["source", "id"]
```
with:
```rust
            "source": { "type": "string", "enum": ["project", "portfolio"], "description": "Data source: 'project' (needs id) or 'portfolio' (the user's whole portfolio, no id)" },
            "id": { "type": "string", "description": "The project id to bind — required when source=project" }
          },
          "required": ["source"]
```

- [ ] **Step 2: Update the validation body** — replace the validation block (from `if source != "project"` through the `get_project` block, i.e. lines ~1957-1980) with:

```rust
    if title.is_empty() || code.is_empty() {
        return Err(ToolError::InvalidArgs {
            name: "render_artifact".into(),
            reason: "title and code are required".into(),
        });
    }
    match source {
        "project" => {
            if id.is_empty() {
                return Err(ToolError::InvalidArgs {
                    name: "render_artifact".into(),
                    reason: "data.id is required when source = project".into(),
                });
            }
            let db = open_db()?;
            let uid = uid_of(w)?;
            if db
                .get_project(&uid, id)
                .map_err(|e| ToolError::Exec(format!("get_project: {e}")))?
                .is_none()
            {
                return Err(ToolError::InvalidArgs {
                    name: "render_artifact".into(),
                    reason: format!("project `{id}` not found"),
                });
            }
        }
        "portfolio" => {
            // The user's whole portfolio; the client fetches it with their own
            // token. Nothing to validate here.
        }
        other => {
            return Err(ToolError::InvalidArgs {
                name: "render_artifact".into(),
                reason: format!("unsupported data source `{other}`"),
            });
        }
    }
```

(Leave the closing `Ok(ToolResult { ... })` as-is.)

- [ ] **Step 3: Extend the SYSTEM_PROMPT** — in `main.rs`, replace the final line of the ARTIFACTS rule:
```rust
   file, dependency-light. After the tool returns, write a one-line confirmation.";
```
with:
```rust
   file, dependency-light. After the tool returns, write a one-line confirmation.\n\
   For INVESTMENTS / 资产配置 / 净值走势, call `render_artifact` with \
   `data: { \"source\": \"portfolio\" }` (NO id) and a component reading `window.DATA`:\n\
     { positions:[{symbol,qty,avg_cost,market_value,unrealized_pl,currency,asset_class}],\n\
       assets:[…], trades:[…], summary:{…by currency/class…}, allocation:{…},\n\
       netWorth:{…current snapshot…}, netWorthSeries:[{…history…}] }\n\
   Use recharts for an allocation pie / net-worth line. Numbers may be strings.";
```
(Note: interior quotes are escaped `\"`; the block ends the `&str` with a bare `";` exactly once. Keep the `\n\` line-continuation style used throughout the prompt.)

- [ ] **Step 4: Build**

Run: `cd /Users/liliang/Things/courses/harness && cargo build -p dashboard 2>&1 | tail -3`
Expected: builds clean (2 pre-existing warnings, no errors). If the string literal is malformed you'll get a parse error → fix the `\n\` continuations / closing `";`.

- [ ] **Step 5: Commit**

```bash
git add examples/dashboard/src/tools.rs examples/dashboard/src/main.rs
git commit -m "feat(dashboard): render_artifact accepts portfolio source (id optional)"
```
No Co-Authored-By. Do NOT push.

---

## Task 2: Frontend — registry `portfolio` case + optional id + `summary` helper

**Files:**
- Modify: `examples/dashboard/user-ui/src/lib/api.ts` (`ledgerApi`, near `allocation:` ~451)
- Modify: `examples/dashboard/user-ui/src/lib/artifact.ts` (whole file)

- [ ] **Step 1: Add the `summary` helper** — in `api.ts`, immediately after the `allocation:` helper block, add:

```ts
  summary: () => api<Record<string, unknown>>('/api/portfolio/summary'),
```

- [ ] **Step 2: Make `id` optional + add the portfolio case** — replace the entire contents of `lib/artifact.ts` with:

```ts
import { ledgerApi } from '@/lib/api';

/** A page the AI asked us to render. Mirrors the render_artifact tool args. */
export interface ArtifactSpec {
  title: string;
  data: { source: string; id?: string };
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
    (data.id === undefined || typeof data.id === 'string')
  ) {
    return {
      title: o.title,
      code: o.code,
      data: { source: data.source, id: typeof data.id === 'string' ? data.id : undefined },
    };
  }
  return null;
}

/** Fetch the data a spec binds to. Host-side (uses the user's token); the
 *  result is postMessage'd into the sandbox as window.DATA. Extend this
 *  registry to add sources (e.g. a macro source for the investor bot). */
export async function fetchArtifactData(spec: ArtifactSpec): Promise<unknown> {
  switch (spec.data.source) {
    case 'project':
      if (!spec.data.id) throw new Error('project artifact missing id');
      return await ledgerApi.project(spec.data.id);
    case 'portfolio': {
      const [pos, assets, trades, summary, allocation, nw, nwSeries] = await Promise.all([
        ledgerApi.positions(),
        ledgerApi.assets(),
        ledgerApi.trades(undefined, 200),
        ledgerApi.summary(),
        ledgerApi.allocation(),
        ledgerApi.netWorth(),
        ledgerApi.netWorthSeries(),
      ]);
      return {
        positions: pos.positions,
        assets: assets.assets,
        trades: trades.trades,
        summary,
        allocation,
        netWorth: nw.snapshot,
        netWorthSeries: nwSeries.series,
      };
    }
    default:
      throw new Error(`unknown artifact data source: ${spec.data.source}`);
  }
}
```

- [ ] **Step 3: Type-check**

Run: `cd /Users/liliang/Things/courses/harness/examples/dashboard/user-ui && npx tsc --noEmit`
Expected: exit 0. (If `ledgerApi.summary`/`allocation`/`netWorth`/`netWorthSeries` shapes differ, the destructure may error — adjust the property reads to match the real return types; all are existing helpers except `summary` from Step 1.)

- [ ] **Step 4: Commit**

```bash
git add examples/dashboard/user-ui/src/lib/api.ts examples/dashboard/user-ui/src/lib/artifact.ts
git commit -m "feat(dashboard/ui): portfolio artifact data source (optional id + full bundle)"
```
No Co-Authored-By. Do NOT push.

---

## Task 3: Build, deploy, live golden-path verification

**Files:** none (verify + deploy only)

- [ ] **Step 1: Rebuild UI + full build**

Run: `cd /Users/liliang/Things/courses/harness/examples/dashboard/user-ui && npm run build 2>&1 | tail -3` then `cd /Users/liliang/Things/courses/harness && cargo build -p dashboard 2>&1 | tail -2`.
Expected: vite built; cargo clean.

- [ ] **Step 2: musl build (UI changed → touch server.rs so dist re-embeds)**

```bash
docker exec ai-ledger-builder bash -lc 'export PATH=/usr/local/cargo/bin:$PATH CARGO_TARGET_DIR=target-musl && cd /work && touch examples/dashboard/src/server.rs && cargo build --release --target x86_64-unknown-linux-musl -p dashboard 2>&1 | tail -3'
```
If the container shows a truncated/stale file (VirtioFS), `docker cp` the changed files in first: `docker cp examples/dashboard/src/main.rs ai-ledger-builder:/work/examples/dashboard/src/main.rs` (and tools.rs) then rebuild.

- [ ] **Step 3: Deploy**

```bash
docker cp ai-ledger-builder:/work/target-musl/x86_64-unknown-linux-musl/release/dashboard /tmp/dashboard.new
scp -q /tmp/dashboard.new qc-jp:/tmp/dashboard.new
ssh qc-jp 'sudo install -m 0755 /tmp/dashboard.new /opt/dashboard/dashboard && sudo systemctl restart dashboard && sleep 3 && systemctl is-active dashboard'
```
Expected: `active`.

- [ ] **Step 4: Live golden path (prod, paid account)**

In `dashboard.superleo.app` chat (a paid/admin account): first ensure some holdings exist (if none, add via chat: "记一笔交易：100 股 AAPL @ 190" → `record_trade`). Then ask: **"把我的投资做成一个图表页面"**.
Expect: assistant calls `render_artifact` with `source:"portfolio"` → a card appears → clicking opens the sandbox → renders allocation/holdings/net-worth from the injected `window.DATA`. Confirm the iframe is `sandbox="allow-scripts"` (no token).

- [ ] **Step 5: Commit any verification fixes (if needed)**

```bash
git add -A && git commit -m "fix(dashboard): portfolio artifact verification fixes"
```

---

## Done criteria
- `cargo build -p dashboard` + `tsc --noEmit` + `npm run build` clean.
- `render_artifact` accepts `{source:"portfolio"}` with no `id`; still rejects unknown sources; project path unchanged.
- Live: "把我的投资做成图表页面" → card → sandboxed page rendered from real portfolio data.
- After this: use **superpowers:finishing-a-development-branch** (or continue to the investor-bot brainstorm).
