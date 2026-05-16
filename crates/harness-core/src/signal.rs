use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Severity {
    /// Informational; agent may ignore.
    Hint,
    /// Should fix, but not blocking.
    Warn,
    /// Must address before proceeding.
    Block,
}

/// A feedback signal from a sensor — **optimised for LLM consumption**.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub severity: Severity,
    pub origin: String,
    /// Human-readable description of the problem.
    pub message: String,
    /// Direct correction instruction for the model (required if `severity == Block`).
    pub agent_hint: Option<String>,
    /// Computational fix that bypasses the model — applied in `auto_fix` channel.
    pub auto_fix: Option<FixPatch>,
    pub location: Option<CodeSpan>,
}

impl Signal {
    pub fn is_blocking(&self) -> bool {
        matches!(self.severity, Severity::Block)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSpan {
    pub path: PathBuf,
    pub line: u32,
    pub column: u32,
    pub length: u32,
}

/// A direct patch a sensor can apply without going through the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FixPatch {
    /// Replace the entire file content.
    ReplaceFile { path: PathBuf, content: String },
    /// Apply a unified diff.
    UnifiedDiff { diff: String },
    /// Run a deterministic shell command (e.g. `cargo fmt`).
    RunCommand {
        program: String,
        args: Vec<String>,
        cwd: Option<PathBuf>,
    },
}

/// A bundle of signals, with helpers for the agent loop.
#[derive(Debug, Default)]
pub struct SignalSet {
    pub signals: Vec<Signal>,
}

impl SignalSet {
    pub fn new(signals: Vec<Signal>) -> Self {
        Self { signals }
    }

    pub fn has_blocking(&self) -> bool {
        self.signals.iter().any(Signal::is_blocking)
    }

    /// Partition into (auto-fix patches, signals that still need model attention).
    pub fn partition_auto_fix(self) -> (Vec<FixPatch>, SignalSet) {
        let mut patches = Vec::new();
        let mut remaining = Vec::new();
        for s in self.signals {
            if let Some(p) = s.auto_fix.clone() {
                patches.push(p);
            } else {
                remaining.push(s);
            }
        }
        (patches, SignalSet { signals: remaining })
    }

    pub fn is_clean(&self) -> bool {
        self.signals.is_empty()
    }
}
