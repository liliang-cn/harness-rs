# harness-rs-tools-sql

Safe **read-only SQL** for [harness-rs](https://github.com/liliang-cn/harness-rs)
agents — the reusable core of "connect the agent to an ERP / MES / BI database"
without letting it touch data it shouldn't.

Built for the on-prem SMB deployment where an agent answers production / BI
questions ("今天 BRK-2049 的良率是多少?") against a live ERP or MES — a place
where a stray `UPDATE` is unacceptable.

## What it gives you (the same-everywhere 20%)

- **Read-only guard** — `check_read_only` rejects anything that isn't a single
  `SELECT`/`WITH`: no writes, no DDL, no multiple statements. `enforce_limit`
  appends a `LIMIT` so results are always bounded.
- **Driver-pluggable executor** — implement `SqlExecutor` once per backend.
  MySQL/PostgreSQL are a `sqlx` pool; SQL Server needs a separate driver
  (`tiberius` — `sqlx` dropped MSSQL); a vendor REST API or a nightly export into
  an intermediate table works too. `SqliteExecutor` (feature `sqlite`) is the
  in-CI reference.
- **`SqlQueryTool`** — the agent-facing tool: guard + LIMIT + optional PII
  **redaction** of results, with the executor's schema doc folded into the tool
  description so NL2SQL lands. Bad SQL comes back as `ok:false` so the agent
  self-corrects.

```rust,ignore
use harness_tools_sql::{SqlQueryTool, SqliteExecutor};
use harness_redact::Redactor;
use std::sync::Arc;

let exec = SqliteExecutor::connect("sqlite:///data/mes.db", MES_SCHEMA_DOC).await?;
let tool = SqlQueryTool::new(Arc::new(exec)).with_redactor(Redactor::new());
// agent.with_tool(Arc::new(tool)) — now the agent can answer BI questions.
```

## What it deliberately does NOT do (the billable 80%)

The vendor schema knowledge (用友 U8/T+, 金蝶 K3/云星空, and each MES differ wildly
and are often undocumented), read access to the customer's system, and NL2SQL
accuracy tuning on that schema — that's per-deployment consulting work, not a
library.

## Safety

The **primary** control is a least-privilege **read-only database account** (and
ideally a read replica). This crate's guard is defense-in-depth on top of that,
and errs strict — a rejected query is an annoyance; a write reaching a customer's
live system is not acceptable.

## License

MIT OR Apache-2.0
