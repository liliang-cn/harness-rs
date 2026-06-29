//! Ops write-tables layered on top of the shop schema (from `ecommerce-analyst`).
//! These record the side effects the agent's governed action phase performs.

use sqlx::PgPool;

const OPS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS purchase_orders (
    id         SERIAL PRIMARY KEY,
    sku        TEXT NOT NULL,
    qty        INT  NOT NULL,
    reason     TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS price_changes (
    id              SERIAL PRIMARY KEY,
    sku             TEXT NOT NULL,
    pct             INT  NOT NULL,
    reason          TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS escalations (
    id         SERIAL PRIMARY KEY,
    kind       TEXT NOT NULL,
    sku        TEXT NOT NULL,
    detail     TEXT NOT NULL,
    reason     TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS action_log (
    id         SERIAL PRIMARY KEY,
    kind       TEXT NOT NULL,
    sku        TEXT NOT NULL,
    decision   TEXT NOT NULL,
    detail     TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
"#;

pub async fn ensure_ops_schema(pool: &PgPool) -> anyhow::Result<()> {
    sqlx::raw_sql(OPS_SCHEMA).execute(pool).await?;
    Ok(())
}
