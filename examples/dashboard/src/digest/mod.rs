//! Daily digest: opt-in proactive summary (yesterday's spending + wealth
//! movement + a shared gold/BTC/index market take), delivered in-app and/or
//! via Resend email at each user's chosen local time.
//!
//! - `model`    — the serializable `Digest` payload.
//! - `schedule` — the pure `is_due` predicate (timezone-aware, dedup-aware).
//! - `market`   — shared per-UTC-day market brief via Gemini grounding.
//! - `build`    — assembles a `Digest` for one user from ledger + snapshots.
//! - `deliver`  — in-app (notifications row) + email (Resend) adapters.
//! - `cron`     — the in-process tokio loop.

pub mod build;
pub mod cron;
pub mod deliver;
pub mod market;
pub mod model;
pub mod schedule;
