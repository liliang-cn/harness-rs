//! Lexical workspace path jail, shared by the file tools and the LSP sensor.

use harness_core::ToolError;
use std::path::{Path, PathBuf};

/// Resolve a workspace-relative path, rejecting escapes. Simple lexical jail —
/// the framework's `harness-tools-fs` has the hardened version; this keeps the
/// example self-contained.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(ToolError::Permission(format!("absolute path: {rel}")));
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out = root.clone();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if !out.starts_with(&root) {
        return Err(ToolError::Permission(format!("escapes workspace: {rel}")));
    }
    Ok(out)
}
