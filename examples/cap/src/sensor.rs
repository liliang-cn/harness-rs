//! `LspSensor` — feed language-server diagnostics back into the loop after edits.

use crate::jail::resolve;
use crate::lsp;
use async_trait::async_trait;
use harness_core::{
    Action, CodeSpan, Execution, Sensor, SensorError, SensorId, Severity, Signal, Stage, World,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// After each `hash_edit` / `write_file`, re-check the touched file with a warm
/// language server and surface its diagnostics as `Signal`s — errors block (the
/// model must fix), warnings/hints inform. Opt-in via the `CAP_LSP` env var
/// (e.g. `CAP_LSP=rust-analyzer` or `CAP_LSP="gopls"`).
pub struct LspSensor {
    pub id: SensorId,
    pub cmd: Vec<String>,
    /// The warm server, started lazily on first edit (cached `None` if the
    /// binary is missing, so we don't retry every edit).
    pub session: tokio::sync::OnceCell<Option<Arc<lsp::LspSession>>>,
}

#[async_trait]
impl Sensor for LspSensor {
    fn id(&self) -> &SensorId {
        &self.id
    }
    fn kind(&self) -> Execution {
        Execution::Computational
    }
    fn stage(&self) -> Stage {
        Stage::SelfCorrect
    }
    async fn observe(&self, action: &Action, world: &World) -> Result<Vec<Signal>, SensorError> {
        if !matches!(action.tool.as_str(), "hash_edit" | "write_file") {
            return Ok(vec![]);
        }
        let path = action.args["path"].as_str().unwrap_or_default();
        if path.is_empty() {
            return Ok(vec![]);
        }
        let Ok(abs) = resolve(&world.repo.root, path) else {
            return Ok(vec![]);
        };
        // Warm session: start the server once, reuse it across every edit.
        let session = self
            .session
            .get_or_init(|| async {
                lsp::LspSession::start(&self.cmd, &world.repo.root)
                    .await
                    .ok()
            })
            .await;
        let Some(session) = session.as_ref() else {
            return Ok(vec![]);
        };
        let diags = session.diagnostics(&abs, Duration::from_secs(10)).await;
        let mut signals = Vec::new();
        for d in diags {
            let severity = match d.severity {
                1 => Severity::Block,
                2 => Severity::Warn,
                _ => Severity::Hint,
            };
            let where_ = format!("{}:{}", d.line + 1, d.character + 1);
            let src = d
                .source
                .as_ref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            let msg = format!("{path}:{where_}: {}{src}", d.message);
            let agent_hint = matches!(severity, Severity::Block)
                .then(|| format!("Fix this diagnostic at {path}:{where_} — {}", d.message));
            signals.push(Signal {
                severity,
                origin: self.id.clone(),
                message: msg,
                agent_hint,
                auto_fix: None,
                location: Some(CodeSpan {
                    path: PathBuf::from(path),
                    line: d.line + 1,
                    column: d.character + 1,
                    length: 0,
                }),
            });
        }
        if !signals.is_empty() {
            let blocking = signals.iter().filter(|s| s.is_blocking()).count();
            eprintln!(
                "\n  \x1b[33m⚠ lsp: {} diagnostic(s) ({} error) on {path}\x1b[0m",
                signals.len(),
                blocking
            );
        }
        Ok(signals)
    }
}
