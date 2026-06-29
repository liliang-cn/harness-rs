# ecommerce-analyst

A realistic e-commerce analysis agent built on **`harness-orchestrator`**.

Three analyst sub-agents — **sales**, **inventory**, **reviews** — run
*concurrently*, each writing **real SQL** against a live PostgreSQL database
via a read-only `sql_query` tool. A fourth **report** job depends on all three
and synthesizes their findings into an executive brief with a prioritized
action list.

```
sales     ┐  (revenue, margins, bestsellers/dead products, channel split, spike)
inventory ┼─► report   (executive brief + prioritized actions, each with a data point)
reviews   ┘  (avg ratings, reputation-risk SKUs, complaint themes)
```

It exercises the whole orchestrator: concurrent Job DAG, dependency fan-in
(upstream results are injected into `report`), a run-level token budget, and
crash-resumable state (`FileRunStore`).

## Run it

```sh
# 1. Start a throwaway Postgres in Docker (host port 38520).
./setup.sh

# 2. Seed realistic synthetic data (reproducible; ~47 products, 3000 orders,
#    7.5k order-items, 1.3k reviews — with bestsellers, dead stock, low-stock
#    fast-movers, a promo-weekend spike, and reputation-risk products).
export DATABASE_URL=postgres://postgres:ecom@localhost:38520/shop
cargo run -p ecommerce-analyst --bin seed

# 3. Run the analyst (uses DashScope qwen3.7-plus; any OpenAI-compatible
#    endpoint works — change MODEL / DASHSCOPE_BASE in src/main.rs).
DASHSCOPE_API_KEY=sk-... cargo run -p ecommerce-analyst
```

The analyst auto-seeds on first run if the database is empty, so step 2 is
optional. To reset the data, drop the tables (or `docker rm -f harness-ecom-pg`
and re-run `./setup.sh`).

## What's where

| File | Role |
|---|---|
| `setup.sh` | starts/wais-for Postgres in Docker |
| `src/db.rs` | schema + the seeded, realistic data generator |
| `src/sqltool.rs` | the read-only `sql_query` tool (SELECT-only, rows as JSON) |
| `src/bin/seed.rs` | standalone seeder |
| `src/main.rs` | the orchestrator DAG + model wiring |

The data is **synthetic but realistic** — generated with a fixed RNG seed so
runs are reproducible, and shaped so the analysis surfaces genuine business
patterns (e.g. a top-revenue product that is also the worst-rated).
