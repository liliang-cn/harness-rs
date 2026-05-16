//! Rust-specific sensors.
//!
//! Each sensor runs the corresponding `cargo` subcommand and converts its
//! diagnostic JSON stream into [`Signal`]s. The `agent_hint` field is filled
//! with imperative correction language so the model can act on it directly.

use async_trait::async_trait;
use harness_core::{
    Action, CodeSpan, Execution, Sensor, SensorError, SensorId, Severity, Signal, Stage, World,
};
use serde::Deserialize;

/// `cargo check` — fast type / borrow-check sensor. Self-correct stage.
pub struct CargoCheck {
    id: SensorId,
}

impl CargoCheck {
    pub fn new() -> Self {
        Self {
            id: "cargo-check".into(),
        }
    }
}

impl Default for CargoCheck {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Sensor for CargoCheck {
    fn id(&self) -> &SensorId {
        &self.id
    }
    fn kind(&self) -> Execution {
        Execution::Computational
    }
    fn stage(&self) -> Stage {
        Stage::SelfCorrect
    }

    async fn observe(&self, _action: &Action, world: &World) -> Result<Vec<Signal>, SensorError> {
        let out = world
            .runner
            .exec(
                "cargo",
                &["check", "--message-format=json", "--quiet"],
                Some(world.repo.root.as_path()),
            )
            .await
            .map_err(|e| SensorError::Failed {
                id: self.id.clone(),
                reason: e.to_string(),
            })?;
        Ok(parse_cargo_messages(&out.stdout, &self.id))
    }
}

/// `cargo clippy --message-format=json -- -D warnings` — lint sensor.
pub struct Clippy {
    id: SensorId,
}

impl Clippy {
    pub fn new() -> Self {
        Self {
            id: "clippy".into(),
        }
    }
}

impl Default for Clippy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Sensor for Clippy {
    fn id(&self) -> &SensorId {
        &self.id
    }
    fn kind(&self) -> Execution {
        Execution::Computational
    }
    fn stage(&self) -> Stage {
        Stage::PreCommit
    }

    async fn observe(&self, _action: &Action, world: &World) -> Result<Vec<Signal>, SensorError> {
        let out = world
            .runner
            .exec(
                "cargo",
                &[
                    "clippy",
                    "--message-format=json",
                    "--quiet",
                    "--",
                    "-D",
                    "warnings",
                ],
                Some(world.repo.root.as_path()),
            )
            .await
            .map_err(|e| SensorError::Failed {
                id: self.id.clone(),
                reason: e.to_string(),
            })?;
        Ok(parse_cargo_messages(&out.stdout, &self.id))
    }
}

// ---------- cargo JSON diagnostic parsing ----------

#[derive(Debug, Deserialize)]
struct CargoMsg {
    reason: String,
    #[serde(default)]
    message: Option<RustcDiag>,
}

#[derive(Debug, Deserialize)]
struct RustcDiag {
    message: String,
    level: String,
    #[serde(default)]
    spans: Vec<Span>,
    #[serde(default)]
    code: Option<DiagCode>,
}

#[derive(Debug, Deserialize)]
struct Span {
    file_name: String,
    line_start: u32,
    column_start: u32,
    #[serde(default)]
    is_primary: bool,
}

#[derive(Debug, Deserialize)]
struct DiagCode {
    code: String,
}

fn parse_cargo_messages(stdout: &str, origin: &str) -> Vec<Signal> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let msg: CargoMsg = match serde_json::from_str(line) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if msg.reason != "compiler-message" {
            continue;
        }
        let diag = match msg.message {
            Some(d) => d,
            None => continue,
        };
        let severity = match diag.level.as_str() {
            "error" | "error: internal compiler error" => Severity::Block,
            "warning" => Severity::Warn,
            _ => Severity::Hint,
        };
        let primary = diag
            .spans
            .iter()
            .find(|s| s.is_primary)
            .or_else(|| diag.spans.first());
        let location = primary.map(|s| CodeSpan {
            path: s.file_name.clone().into(),
            line: s.line_start,
            column: s.column_start,
            length: 0,
        });
        let code_str = diag
            .code
            .as_ref()
            .map(|c| format!(" [{}]", c.code))
            .unwrap_or_default();
        let agent_hint = build_hint(&diag.message, &diag.level);
        out.push(Signal {
            severity,
            origin: origin.to_string(),
            message: format!("{}{code_str}", diag.message),
            agent_hint: Some(agent_hint),
            auto_fix: None,
            location,
        });
    }
    out
}

fn build_hint(message: &str, level: &str) -> String {
    match level {
        "error" => {
            format!("Fix this compilation error: {message}. Edit the file and re-run cargo check.")
        }
        "warning" => format!("Address this warning: {message}. Prefer fixing over silencing."),
        _ => message.to_string(),
    }
}
