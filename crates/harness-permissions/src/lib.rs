//! Permission modes + per-tool allow/deny rules.
//!
//! `harness-rs` already ships a `Hook` system that can deny tool calls
//! one-off — but writing one hook per tier per tool gets repetitive fast.
//! This crate gives you a single `PermissionHook` that takes a declarative
//! `PermissionRules` and decides per `PreToolUse` event whether the call
//! goes through.
//!
//! Three top-level modes:
//! - `Default` — pass through; only existing hooks gate.
//! - `Plan` — read-only / idempotent / network tools allowed; destructive
//!   tools denied unless `allow_tools` lists them. Anything with unknown
//!   risk also denied (safe default).
//! - `AutoApprove` — allow everything except entries in `deny_tools`.
//!
//! Risk classification comes from each tool's `Tool::risk()` method
//! (which the `#[tool(risk = "destructive")]` proc-macro sets). Populate
//! the `risk_map` automatically via `PermissionRules::with_tools(...)`
//! against your tool registry.
//!
//! # Example
//!
//! ```ignore
//! use harness_permissions::{PermissionMode, PermissionRules, PermissionHook};
//! use std::sync::Arc;
//!
//! let tools = my_tools();  // Vec<Arc<dyn Tool>>
//! let mode = match user_tier.as_str() {
//!     "trial" => PermissionMode::Plan,
//!     "admin" => PermissionMode::AutoApprove,
//!     _       => PermissionMode::Default,
//! };
//! let rules = PermissionRules::new(mode)
//!     .with_tools(&tools)
//!     .allow("safe_thing")   // override Plan-mode default
//!     .deny("dangerous_thing");
//! loop_ = loop_.with_hook(Arc::new(PermissionHook::new(rules)));
//! ```

use harness_core::{Action, Event, Hook, HookOutcome, Tool, ToolRisk, World};
use std::collections::HashMap;
use std::sync::Arc;

/// Top-level policy mode. See crate docs for semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PermissionMode {
    /// No additional gating beyond the loop's other hooks. Useful as a
    /// disabled / passthrough baseline.
    #[default]
    Default,
    /// Conservative: allow read-only / idempotent / network tools; deny
    /// destructive ones unless explicitly listed in `allow_tools`. Tools
    /// missing from `risk_map` are denied (safe default).
    Plan,
    /// Permissive: allow every tool except those in `deny_tools`. Use this
    /// for trusted admin invocations or unattended scheduled runs.
    AutoApprove,
}

/// `HARNESS_YOLO=1` — run without asking.
///
/// The mode already existed; what didn't was one agreed way to switch it on,
/// so every host invented its own env var with its own spelling and its own
/// idea of what it waives. One definition, read once — a policy this
/// consequential must not change under a running turn.
///
/// What it means: no interactive gate. It is NOT a licence to skip a host's
/// hard refusals (catastrophic commands, path jails, SSRF guards); those exist
/// because nobody would knowingly approve them either.
pub fn yolo() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = matches!(
            std::env::var("HARNESS_YOLO").as_deref(),
            Ok("1") | Ok("true") | Ok("yes") | Ok("yolo")
        );
        if on {
            tracing::warn!("HARNESS_YOLO=1 — tools run without asking");
        }
        on
    })
}

impl PermissionMode {
    /// Mode from the environment: `HARNESS_YOLO=1` wins outright, else
    /// `HARNESS_PERMISSION_MODE=plan|auto|default`, else `Default`.
    ///
    /// An unrecognised value is `Default` (the safe end) and says so, rather
    /// than being read as permission by a typo.
    pub fn from_env() -> Self {
        if yolo() {
            return PermissionMode::AutoApprove;
        }
        match std::env::var("HARNESS_PERMISSION_MODE").as_deref().map(str::trim) {
            Ok("plan") => PermissionMode::Plan,
            Ok("auto") | Ok("auto-approve") | Ok("autoapprove") => PermissionMode::AutoApprove,
            Ok("default") | Err(_) => PermissionMode::Default,
            Ok(other) => {
                tracing::warn!(
                    value = other,
                    "HARNESS_PERMISSION_MODE not recognised — falling back to Default"
                );
                PermissionMode::Default
            }
        }
    }
}

/// Declarative rules consumed by `PermissionHook`.
#[derive(Debug, Clone, Default)]
pub struct PermissionRules {
    pub mode: PermissionMode,
    /// Override mode default: ALWAYS allow these tool names.
    pub allow_tools: Vec<String>,
    /// ALWAYS deny these tool names regardless of mode or allow_tools.
    pub deny_tools: Vec<String>,
    /// Tool name → risk level. Populated automatically by
    /// `PermissionRules::with_tools()` from a `Vec<Arc<dyn Tool>>`.
    pub risk_map: HashMap<String, ToolRisk>,
}

impl PermissionRules {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }

    /// Build / extend `risk_map` from your tool registry. Call once after
    /// collecting tools and before constructing the `PermissionHook`.
    pub fn with_tools(mut self, tools: &[Arc<dyn Tool>]) -> Self {
        for t in tools {
            self.risk_map.insert(t.name().into(), t.risk());
        }
        self
    }

    pub fn allow(mut self, name: impl Into<String>) -> Self {
        self.allow_tools.push(name.into());
        self
    }

    pub fn deny(mut self, name: impl Into<String>) -> Self {
        self.deny_tools.push(name.into());
        self
    }
}

/// Hook that consults `PermissionRules` on every `PreToolUse` event.
///
/// `deny_tools` is checked first (hard deny). Then `allow_tools` (hard
/// allow). Then mode-default logic runs.
pub struct PermissionHook {
    rules: PermissionRules,
}

impl PermissionHook {
    pub fn new(rules: PermissionRules) -> Self {
        Self { rules }
    }

    fn decide(&self, action: &Action) -> HookOutcome {
        let name = action.tool.as_str();
        if self.rules.deny_tools.iter().any(|s| s == name) {
            return HookOutcome::Deny {
                reason: format!("tool `{name}` is in deny_tools"),
            };
        }
        if self.rules.allow_tools.iter().any(|s| s == name) {
            return HookOutcome::Allow;
        }
        match self.rules.mode {
            PermissionMode::Default | PermissionMode::AutoApprove => HookOutcome::Allow,
            PermissionMode::Plan => match self.rules.risk_map.get(name) {
                Some(ToolRisk::ReadOnly | ToolRisk::Idempotent | ToolRisk::Network) => {
                    HookOutcome::Allow
                }
                Some(ToolRisk::Destructive) => HookOutcome::Deny {
                    reason: format!(
                        "tool `{name}` is destructive — plan mode requires an explicit \
                         `allow_tools` entry to permit it"
                    ),
                },
                Some(_) => HookOutcome::Allow, // future ToolRisk variants ⇒ allow by default
                None => HookOutcome::Deny {
                    reason: format!(
                        "tool `{name}` has no risk classification — plan mode denies \
                         unknown tools by default (add to risk_map or allow_tools)"
                    ),
                },
            },
        }
    }
}

impl Hook for PermissionHook {
    fn name(&self) -> &str {
        "permissions"
    }
    fn matches(&self, ev: &Event<'_>) -> bool {
        matches!(ev, Event::PreToolUse { .. })
    }
    fn fire(&self, ev: &Event<'_>, _world: &mut World) -> HookOutcome {
        if let Event::PreToolUse { action } = ev {
            let outcome = self.decide(action);
            if let HookOutcome::Deny { reason } = &outcome {
                tracing::info!(tool = %action.tool, %reason, "permissions denied");
            }
            outcome
        } else {
            HookOutcome::Allow
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Action, ToolRisk};
    use serde_json::json;

    fn mk_action(tool: &str) -> Action {
        Action {
            tool: tool.into(),
            call_id: "c1".into(),
            args: json!({}),
        }
    }

    #[test]
    fn auto_approve_allows_everything_except_deny_list() {
        let rules = PermissionRules::new(PermissionMode::AutoApprove).deny("nuke_db");
        let hook = PermissionHook::new(rules);
        assert!(matches!(
            hook.decide(&mk_action("anything")),
            HookOutcome::Allow
        ));
        assert!(matches!(
            hook.decide(&mk_action("nuke_db")),
            HookOutcome::Deny { .. }
        ));
    }

    #[test]
    fn plan_denies_destructive_unless_allowlisted() {
        let mut rules = PermissionRules::new(PermissionMode::Plan);
        rules
            .risk_map
            .insert("delete_thing".into(), ToolRisk::Destructive);
        rules
            .risk_map
            .insert("read_thing".into(), ToolRisk::ReadOnly);
        let hook = PermissionHook::new(rules);
        assert!(matches!(
            hook.decide(&mk_action("delete_thing")),
            HookOutcome::Deny { .. }
        ));
        assert!(matches!(
            hook.decide(&mk_action("read_thing")),
            HookOutcome::Allow
        ));
    }

    #[test]
    fn plan_allowlist_overrides_destructive_deny() {
        let mut rules = PermissionRules::new(PermissionMode::Plan).allow("delete_thing");
        rules
            .risk_map
            .insert("delete_thing".into(), ToolRisk::Destructive);
        let hook = PermissionHook::new(rules);
        assert!(matches!(
            hook.decide(&mk_action("delete_thing")),
            HookOutcome::Allow
        ));
    }

    #[test]
    fn plan_denies_unknown_risk_by_default() {
        let rules = PermissionRules::new(PermissionMode::Plan);
        let hook = PermissionHook::new(rules);
        assert!(matches!(
            hook.decide(&mk_action("unknown_tool")),
            HookOutcome::Deny { .. }
        ));
    }

    #[test]
    fn deny_beats_allow_when_both_set() {
        let rules = PermissionRules::new(PermissionMode::AutoApprove)
            .deny("dangerous")
            .allow("dangerous");
        let hook = PermissionHook::new(rules);
        assert!(matches!(
            hook.decide(&mk_action("dangerous")),
            HookOutcome::Deny { .. }
        ));
    }
}

#[cfg(test)]
mod env_mode_tests {
    use super::*;

    // These read process-global env, so they run as one test rather than
    // racing each other.
    #[test]
    fn the_mode_comes_from_the_environment_and_a_typo_is_not_permission() {
        // Unset → the safe end.
        unsafe {
            std::env::remove_var("HARNESS_PERMISSION_MODE");
        }
        assert_eq!(PermissionMode::from_env(), PermissionMode::Default);

        for (val, want) in [
            ("plan", PermissionMode::Plan),
            ("auto", PermissionMode::AutoApprove),
            ("auto-approve", PermissionMode::AutoApprove),
            ("default", PermissionMode::Default),
            // A typo must not read as permission.
            ("aut0", PermissionMode::Default),
            ("YOLO", PermissionMode::Default),
        ] {
            unsafe {
                std::env::set_var("HARNESS_PERMISSION_MODE", val);
            }
            assert_eq!(PermissionMode::from_env(), want, "for {val:?}");
        }
        unsafe {
            std::env::remove_var("HARNESS_PERMISSION_MODE");
        }
    }
}
