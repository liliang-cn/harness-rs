# Artifact renderer — `portfolio` data source

**Date:** 2026-05-28
**Status:** design, pending user review
**Part of:** the Dashboard AI artifact renderer (extends
`2026-05-28-ai-artifact-renderer-design.md`). This is the first follow-on data
source after `project`; the investor-bot macro source is a later, separate spec.

## Goal

Let the AI render the user's **own investments** as a generated page — "看看我
的投资 / 资产配置 / 净值走势" → a sandboxed React page (allocation pie, holdings
table, net-worth trend, gains) drawn from the user's live portfolio data. Pure
extension of the existing renderer; no new UI, no new backend data (the
portfolio + net-worth REST endpoints already exist and are deployed).

## The `window.DATA` contract (`source: "portfolio"`)

The host fetches **all** portfolio-relevant data and injects it. No `id` — a
portfolio is the user's whole portfolio.

```ts
window.DATA = {
  positions:      Position[],          // GET /api/portfolio/positions → .positions
  assets:         AssetWithPrice[],     // GET /api/portfolio/assets → .assets (incl. current price)
  trades:         Trade[],              // GET /api/portfolio/trades?limit=200 → .trades
  summary:        <portfolio summary>,  // GET /api/portfolio/summary (totals, cost, gains)
  allocation:     <allocation>,         // GET /api/portfolio/allocation (by class/%)
  netWorth:       NetWorthSnapshot,     // GET /api/me/net-worth → .snapshot (current)
  netWorthSeries: NetWorthSnapshot[],   // GET /api/me/net-worth/series → .series (history)
}
```

All seven are fetched **host-side with the user's token** (token never enters the
sandbox), in parallel, then `postMessage`'d in — same model as `project`.

## Changes

### Backend — `src/tools.rs` (`render_artifact` tool)
- Add `"portfolio"` to the `data.source` enum (currently `["project"]`).
- Make `data.id` **optional**: remove `id` from `data.required` in the schema.
- Validation logic:
  - `source == "project"` → `id` required + ownership check via `get_project` (unchanged).
  - `source == "portfolio"` → `id` ignored; no per-item ownership check (the
    client fetches the caller's own portfolio with their token).
  - any other `source` → `InvalidArgs`.

### Backend — `src/main.rs` (`SYSTEM_PROMPT`)
- Extend the ARTIFACTS rule: when the user asks to see their investments /
  allocation / net-worth, call `render_artifact` with
  `data: { "source": "portfolio" }` (no id) and a component reading the
  `window.DATA` shape above. Charts via `recharts`. Document the shape briefly.

### Frontend — `user-ui/src/lib/api.ts`
- Add two `ledgerApi` helpers for the endpoints lacking one:
  - `portfolioSummary: () => api(`/api/portfolio/summary`)`
  - `portfolioAllocation: () => api(`/api/portfolio/allocation`)`
  (Return types may be `unknown`/loose — the sandbox consumes them as data.)

### Frontend — `user-ui/src/lib/artifact.ts`
- `ArtifactSpec.data.id` becomes optional (`id?: string`); `asArtifactSpec`
  accepts a missing `id` (require `source` string; `id` string if present).
- `fetchArtifactData` gains:
  ```ts
  case 'portfolio': {
    const [pos, assets, trades, summary, allocation, nw, nwSeries] = await Promise.all([
      ledgerApi.positions(), ledgerApi.assets(), ledgerApi.trades(undefined, 200),
      ledgerApi.portfolioSummary(), ledgerApi.portfolioAllocation(),
      ledgerApi.netWorth(), ledgerApi.netWorthSeries(),
    ]);
    return {
      positions: pos.positions, assets: assets.assets, trades: trades.trades,
      summary, allocation, netWorth: nw.snapshot, netWorthSeries: nwSeries.series,
    };
  }
  ```

That's the whole change. The card → sandbox → injection → render pipeline,
security model, persistence, and error handling are all unchanged/reused.

## Non-goals
- The investor-bot **macro** data source (separate spec; needs an external data source).
- Any new UI, route, or backend data — only a new registry source + tool/prompt wiring.
- Per-asset detail drill-downs (the AI composes whatever it wants from the bundle).

## Testing / verification
- `cargo build -p dashboard` + `npx tsc --noEmit` clean.
- Backend: `render_artifact` accepts `{source:"portfolio"}` with no `id` (no error);
  still rejects unknown sources; project path unchanged (id required).
- Live golden path (prod, paid account with some holdings): ask "把我的投资做成一
  个图表页面" → card → open → renders allocation/holdings/net-worth from injected
  data. (If the test account has no holdings, add a couple of trades via chat first.)

## Rollout
Same musl → qc-jp flow. Backend (tools.rs + main.rs) + frontend changes; if only
backend changed, no dist rebuild needed, but this touches the UI too → rebuild
dist + `touch server.rs` before the musl build.
