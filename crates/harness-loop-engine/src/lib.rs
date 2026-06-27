//! # harness-loop-engine — loop engineering for harness-rs
//!
//! *Agent = Model + Harness.* A **harness** wraps a single agent call. A
//! **loop** wraps the harness: it runs that call again and again, on a
//! cadence, with state, verification, budgets, and gates — driving toward a
//! goal over time instead of in one shot. This crate is harness-rs's loop
//! layer.
//!
//! > "Loop engineering is replacing yourself as the person who prompts the
//! > agent. You design the system that does it instead." — and you stay the
//! > engineer responsible for that system.
//!
//! The building blocks already live elsewhere in harness-rs — scheduling
//! (`harness-scheduler`), worktrees (`harness-sandbox`), sub-agents
//! (`harness-loop`), memory (`harness-core`), MCP (`harness-mcp`). What this
//! crate adds is the **orchestration discipline** that turns those parts
//! into a loop you can trust:
//!
//! - [`LoopLevel`] — maturity levels **L1 (report) → L2 (assisted) → L3
//!   (unattended)**. A loop earns autonomy in stages.
//! - [`HumanGate`] — the proceed-or-escalate decision, tied to the level.
//!   Built-ins: [`AlwaysEscalate`], [`AllowlistGate`], [`CallbackGate`].
//! - [`TokenBudget`] — a per-round spend ceiling, because unattended loops
//!   spend without bound if you let them.
//! - [`LoopSpec`] — the inert, serializable description of a loop.
//! - [`LoopEngine`] — the runner: recall state → isolate → **maker**
//!   sub-agent → **checker** sub-agent → gate → record state.
//! - [`LoopScheduler`] — runs loops on their cadence.
//! - [`patterns`] — the seven named production loops (daily triage, PR
//!   babysitter, CI sweeper, …), each a ready-made [`LoopSpec`].
//!
//! ## The anatomical loop
//!
//! ```text
//!   schedule (cadence)
//!        │
//!        ▼
//!   recall STATE / memory ──► isolated worktree (sandbox)
//!        │                          │
//!        │                          ▼
//!        │                  maker sub-agent  (proposes)
//!        │                          │
//!        │                          ▼
//!        │                  checker sub-agent  (tests + gates)
//!        │                          │
//!        │                          ▼
//!        │                     human gate? ──┬─ safe/allowlisted ─► proceed
//!        │                                   └─ risky/ambiguous ──► escalate
//!        ▼                                            │
//!   write STATE / memory  ◄────────────────────────── recurse next tick
//! ```
//!
//! ## Two debts to watch
//!
//! Loop engineering names two failure modes that accrue silently. This
//! crate makes them *visible* rather than solving them — they are
//! engineering responsibilities, not features:
//!
//! - **Intent debt** — the drift between what a loop was *meant* to do and
//!   what it actually does. Antidote: [`LoopSpec::intent`] is a required,
//!   one-sentence statement of purpose, injected into every maker turn and
//!   printed in every report. Review it as the loop evolves.
//! - **Comprehension debt** — the gap between what the loop ships and what
//!   humans still understand about its behaviour. Antidote: the
//!   maker/checker split, the recorded state spine, and rendered reports
//!   keep a legible trail of every round.
//!
//! ## Safety stance
//!
//! Verification stays on you — unattended loops make unattended mistakes.
//! Defaults are conservative: L1 makers are strictly read-only, the default
//! gate for every level is [`AlwaysEscalate`], and L3 auto-proceed requires
//! an explicit [`AllowlistGate`]. Graduate a loop's level only as you build
//! trust in it.

mod budget;
mod engine;
mod level;
pub mod patterns;
mod scheduler;
mod spec;

pub use budget::{BudgetLimit, BudgetState, TokenBudget};
pub use engine::{LoopEngine, RoundOutcome, RoundReport};
pub use level::{
    AllowlistGate, AlwaysEscalate, CallbackGate, GateDecision, HumanGate, LoopLevel,
    ProposedAction, default_gate_for,
};
pub use scheduler::{LoopScheduler, LoopSink, StdoutSink};
pub use spec::LoopSpec;
