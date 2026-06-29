# ecommerce-ops-agent

The **complex** example — an autonomous e-commerce *operations* agent that
composes nearly the entire harness-rs stack over a **live PostgreSQL database**
(reads *and* writes), in three governed stages.

```
[1] ANALYZE   sales ┐                          (harness-orchestrator)
              inv   ┼─► (dynamic replan: scan DB for anomalies)
              rev   ┘        │
                             ├─► deepdive-reorder    (retries through a
                             ├─► deepdive-liquidation  simulated transient
                             └─► deepdive-quality      failure → backoff)
                                       │
                                       ▼
                                  synthesize ──► JSON action list

[2] GOVERN    each action → blast-radius level → HumanGate    (harness-loop-engine)
              ├─ reorder / markdown ≤20%  (L3) ─► ActionExecutor → real DB write
              └─ markdown >20% / pause    (L2) ─► escalate → escalations table

[3] REMEMBER  applied actions → memory; next run recalls them   (harness-core::Memory)
              and avoids re-proposing what was already done.
```

## Features exercised

- **orchestrator**: concurrent Job DAG · **dynamic replanning** (a `Planner`
  that queries the DB and adds only the deep-dives the data warrants) ·
  **retry + exponential backoff + dead-letter** · run-level **token budget** ·
  **resumable** state (`FileRunStore`, `--resume`).
- **loop-engine**: maturity levels **L1/L2/L3** · `AllowlistGate` · **`ActionExecutor`**
  doing real DB writes · `ActionReceipt` · blast-radius classification.
- **core / context**: custom **tools** (`sql_query` read, `market_signal` flaky)
  with **risk levels** · **`Memory`** recall+write (`FileMemory`).
- **models**: `ApiKind::OpenAI` against DashScope qwen.
- a **live PostgreSQL** database — real reads and real writes.

## Run it

```sh
# 1. Start Postgres + seed realistic shop data (reuses the ecommerce-analyst setup).
../ecommerce-analyst/setup.sh
export DATABASE_URL=postgres://postgres:ecom@localhost:38520/shop

# 2. Run the ops agent (auto-seeds shop data + creates ops tables on first run).
DASHSCOPE_API_KEY=sk-... cargo run -p ecommerce-ops-agent

# Run again — it recalls what it did and won't repeat itself.
DASHSCOPE_API_KEY=sk-... cargo run -p ecommerce-ops-agent

# Resume a previously-interrupted run from its persisted state.
DASHSCOPE_API_KEY=sk-... cargo run -p ecommerce-ops-agent -- --resume
```

Inspect the side effects:

```sh
docker exec harness-ecom-pg psql -U postgres -d shop \
  -c "SELECT * FROM purchase_orders; SELECT * FROM escalations;"
```

## Files

| File | Role |
|---|---|
| `src/main.rs` | the 3-stage orchestration |
| `src/planner.rs` | dynamic-replanning `AnomalyPlanner` (DB-driven) |
| `src/runner.rs` | `OpsJobRunner` — injects one transient failure to exercise retry |
| `src/govern.rs` | gate + `DbActionExecutor` (real DB writes) + escalation |
| `src/action.rs` | proposed-action model, lenient JSON parsing, blast-radius classification |
| `src/tools.rs` | the flaky `market_signal` tool |
| `src/schema.rs` | ops write-tables |
| `src/memory.rs` | cross-run recall + persist |

The shop data and the `sql_query` tool are reused from the sibling
`ecommerce-analyst` example.
