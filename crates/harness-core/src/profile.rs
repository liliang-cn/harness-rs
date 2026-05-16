//! User profile — ambient context every agent inherits.
//!
//! The framework keeps this deliberately small: the three things that almost
//! every coding/scheduling/personal agent needs (name, timezone, locale) plus
//! a free-form `extra` map for agent-specific preferences.
//!
//! Persistence is up to the runtime layer (see `harness_context::profile`).
//! Tools read [`World::profile`]; an opt-in [`profile::ProfileGuide`] from
//! `harness_loop` automatically renders it into the agent's system prompt.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Ambient information about who the agent is working for.
///
/// All fields are optional; an empty profile is the documented default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserProfile {
    /// Display name, e.g. "李亮".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// IANA timezone identifier, e.g. "Asia/Shanghai", "Europe/Vienna".
    /// When unset, agents should fall back to the system clock's local tz.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,

    /// BCP-47 locale, e.g. "zh-CN", "en-US". Affects reply language + date formatting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,

    /// Free-form agent-specific preferences. Examples:
    /// `default_meeting_duration_min: 60`, `preferred_linter: "clippy"`, …
    ///
    /// Keys should be namespaced when shared across agents (e.g.
    /// `"scheduler.default_meeting_duration_min"`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl UserProfile {
    /// Build a one-line prompt-friendly summary. Used by `ProfileGuide`.
    pub fn summary_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(n) = &self.name {
            parts.push(format!("name={n}"));
        }
        parts.push(match &self.tz {
            Some(z) => format!("tz={z}"),
            None => "tz=(system clock)".into(),
        });
        if let Some(l) = &self.locale {
            parts.push(format!("locale={l}"));
        }
        parts.join(", ")
    }

    /// Read an `extra` key as a typed value.
    pub fn extra<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        self.extra
            .get(key)
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
    }

    /// Set an `extra` key, replacing any existing value.
    pub fn set_extra<T: Serialize>(&mut self, key: impl Into<String>, value: T) {
        if let Ok(v) = serde_json::to_value(value) {
            self.extra.insert(key.into(), v);
        }
    }
}
