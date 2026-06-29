//! # harness-orchestrator — single-machine async Run orchestration
//!
//! A **Run** is one user goal executed as a **DAG of Jobs**. Each Job is
//! typically one sub-agent invocation; Jobs declare dependencies, run
//! concurrently once their dependencies succeed, retry with backoff on
//! failure, and persist their state so a crashed Run can resume.
//!
//! This is the *orchestration* half of an agent system — the durable,
//! concurrent task fabric — kept deliberately **single-machine** (no Kafka,
//! no worker pool, no distributed locks; just `tokio` + a state store).
//! It complements the other halves of harness-rs: `harness-loop` runs a
//! single agent, `harness-loop-engine` governs a recurring loop, and this
//! crate fans one goal out across many concurrent, dependent Jobs.
//!
//! ## What it gives you
//!
//! - **Concurrent DAG** — [`Dag`] of [`Job`]s; the [`Orchestrator`] runs every
//!   Job whose dependencies have `Succeeded`, up to a concurrency cap.
//! - **Dynamic replanning** — a [`Planner`] is re-invoked with the results so
//!   far and may merge new Jobs into the running DAG ([`PlanDelta::Add`]).
//!   This is the feedback edge that makes it an *agent* runtime, not a static
//!   plan-then-execute workflow.
//! - **Retry / backoff / dead-letter** — per-Job [`RetryPolicy`] with
//!   [`Backoff`]; exhausted Jobs are `DeadLettered` and block their
//!   dependents (which are then `Cancelled`).
//! - **Resumable state** — a [`RunStore`] persists Run + Job state after every
//!   transition; [`Orchestrator::resume`] restarts a crashed Run from its
//!   succeeded results.
//! - **Run-level token budget** — [`RunBudget`] caps total spend across all
//!   Jobs, the cost governance most async-orchestration designs omit.
//!
//! ## The loop
//!
//! ```text
//!   plan (optional) ─► DAG of Jobs
//!        │
//!        ▼
//!   launch Jobs whose deps Succeeded  (concurrent, capped)
//!        │
//!        ▼
//!   await completions ─┬─ ok        ─► Succeeded, unblock dependents
//!                      ├─ fail<max  ─► Retrying (backoff) ─► relaunch
//!                      └─ fail=max  ─► DeadLettered ─► Cancel dependents
//!        │
//!        ▼
//!   drained? ─► replan (planner) ──► add Jobs / Done
//!        │
//!        ▼
//!   Completed / Failed   (state persisted throughout)
//! ```
//!
//! ## Concurrency note
//!
//! Sub-agent futures are `!Send`, so the orchestrator runs them cooperatively
//! on one thread via `FuturesUnordered` rather than `tokio::spawn`. Each Job
//! gets a fresh [`World`](harness_core::World) from a factory — both to avoid
//! `&mut World` aliasing across concurrent Jobs and to give each Job
//! worker-style isolation.

mod dag;
mod job;
mod orchestrator;
mod planner;
mod run;
mod runner;
mod store;

pub use dag::{Dag, PlanDelta};
pub use job::{Backoff, Job, JobId, JobResult, JobState, RetryPolicy};
pub use orchestrator::Orchestrator;
pub use planner::{Planner, PlannerError, StaticPlanner};
pub use run::{Run, RunBudget, RunId, RunReport, RunState};
pub use runner::{JobError, JobRunner, SubagentJobRunner, WorldFactory};
pub use store::{FileRunStore, InMemoryRunStore, RunStore, StoreError};
