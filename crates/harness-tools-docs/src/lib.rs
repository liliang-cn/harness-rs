//! `read_document` — let an agent *read* external file formats (PDF, Word,
//! Excel, PowerPoint, …), not just plain text.
//!
//! **Hybrid, in priority order:**
//! 1. **Local, pure Rust** (feature `local`, on by default) — `pdf-extract` for
//!    PDF and `office_oxide` for docx/xlsx/pptx/doc/xls/ppt. No native deps,
//!    zero tokens, deterministic.
//! 2. **LLM fallback** — whatever local parsing can't handle (unknown formats,
//!    empty extraction from a scanned/image doc, a file the parser choked on) is
//!    handed to a `Model` you inject via [`ReadDocument::with_llm_fallback`].
//!
//! Construct it local-only, or with a fallback model:
//! ```ignore
//! let tool = ReadDocument::new();                       // local only
//! let tool = ReadDocument::with_llm_fallback(model);    // local, then LLM
//! ```
//!
//! Read-only and **jailed** to the workspace: the path is resolved under
//! `world.repo.root` and any escape (`..`, absolute, symlink out) is rejected.

use async_trait::async_trait;
use harness_core::{
    Block, Context, Model, Task, Tool, ToolError, ToolResult, ToolRisk, ToolSchema, Turn, TurnRole,
    World,
};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Deserialize)]
struct ReadDocArgs {
    path: String,
    /// Cap on characters returned (default 100_000).
    #[serde(default)]
    max_chars: Option<usize>,
}

/// Extract text from a document. Tries local parsers first, then an optional LLM.
pub struct ReadDocument {
    fallback: Option<Arc<dyn Model>>,
}

impl Default for ReadDocument {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadDocument {
    /// Local extraction only. If a format can't be parsed locally, the tool
    /// returns an error telling the caller to wire an LLM fallback.
    pub fn new() -> Self {
        Self { fallback: None }
    }

    /// Local extraction first; hand anything it can't parse to `model`.
    pub fn with_llm_fallback(model: Arc<dyn Model>) -> Self {
        Self {
            fallback: Some(model),
        }
    }
}

static READ_DOCUMENT_SCHEMA: Lazy<ToolSchema> = Lazy::new(|| ToolSchema {
    name: "read_document".into(),
    description: "Extract text from a document file (PDF, Word/docx, Excel/xlsx, \
                  PowerPoint/pptx, and legacy doc/xls/ppt) in the workspace. Use \
                  this instead of read_file for non-plain-text formats. Returns \
                  the extracted text, its source (local parser or llm), and format."
        .into(),
    input: json!({
        "type": "object",
        "properties": {
            "path":      {"type": "string", "description": "Path to the document, relative to the workspace root."},
            "max_chars": {"type": "integer", "minimum": 1, "description": "Max characters to return (default 100000)."}
        },
        "required": ["path"]
    }),
});

/// Resolve `rel` under `root`, rejecting anything that escapes the workspace.
fn resolve_in_jail(root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let root = root
        .canonicalize()
        .map_err(|e| ToolError::Exec(format!("workspace root: {e}")))?;
    let full = root
        .join(rel)
        .canonicalize()
        .map_err(|e| ToolError::Exec(format!("{rel}: {e}")))?;
    if !full.starts_with(&root) {
        return Err(ToolError::Permission(format!(
            "{rel}: path escapes the workspace"
        )));
    }
    Ok(full)
}

#[async_trait]
impl Tool for ReadDocument {
    fn name(&self) -> &str {
        "read_document"
    }
    fn schema(&self) -> &ToolSchema {
        &READ_DOCUMENT_SCHEMA
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::ReadOnly
    }

    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let a: ReadDocArgs = serde_json::from_value(args).map_err(|e| ToolError::InvalidArgs {
            name: self.name().into(),
            reason: e.to_string(),
        })?;
        let max_chars = a.max_chars.unwrap_or(100_000);
        let full = resolve_in_jail(&world.repo.root, &a.path)?;
        let ext = full
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        // 1) Local, pure-Rust extraction.
        if let Some(text) = try_local(full.clone(), ext.clone()).await {
            let text = cap(text, max_chars);
            return Ok(result(&a.path, "local", &ext, text));
        }

        // 2) LLM fallback.
        if let Some(model) = &self.fallback {
            let text = llm_extract(model, &a.path, &full, &ext, max_chars).await?;
            return Ok(result(&a.path, "llm", &ext, text));
        }

        Err(ToolError::Exec(format!(
            "could not parse `{}` locally (format: {}) and no LLM fallback is \
             configured — construct the tool with ReadDocument::with_llm_fallback(model)",
            a.path,
            if ext.is_empty() { "unknown" } else { &ext }
        )))
    }
}

fn cap(mut s: String, max_chars: usize) -> String {
    if s.chars().count() > max_chars {
        s = s.chars().take(max_chars).collect();
    }
    s
}

fn result(path: &str, source: &str, ext: &str, text: String) -> ToolResult {
    let chars = text.chars().count();
    ToolResult {
        ok: true,
        content: json!({
            "path":   path,
            "source": source,
            "format": ext,
            "chars":  chars,
            "text":   text,
        }),
        trace: None,
    }
}

/// Try to extract text with a local pure-Rust parser. `None` means "no local
/// parser for this format, or it produced nothing" — the caller then falls back
/// to the LLM. Runs on a blocking thread (the parsers are synchronous).
async fn try_local(path: PathBuf, ext: String) -> Option<String> {
    tokio::task::spawn_blocking(move || local_extract(&path, &ext))
        .await
        .ok()
        .flatten()
}

#[cfg(feature = "local")]
fn local_extract(path: &Path, ext: &str) -> Option<String> {
    let res: Result<String, String> = match ext {
        "pdf" => pdf_extract::extract_text(path).map_err(|e| e.to_string()),
        "docx" | "xlsx" | "pptx" | "doc" | "xls" | "ppt" => {
            office_oxide::extract_text(path).map_err(|e| e.to_string())
        }
        _ => return None, // no local parser → LLM fallback
    };
    match res {
        // Empty output usually means a scanned/image doc → let the LLM try.
        Ok(t) if !t.trim().is_empty() => Some(t),
        _ => None,
    }
}

#[cfg(not(feature = "local"))]
fn local_extract(_path: &Path, _ext: &str) -> Option<String> {
    None
}

/// MIME type for an image extension, or `None` if it isn't a known image.
fn image_media_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => return None,
    })
}

/// Hand the file to the model. For images, send a real `Block::Image` (vision);
/// for anything else, send the raw content as text and ask for extraction.
async fn llm_extract(
    model: &Arc<dyn Model>,
    filename: &str,
    path: &Path,
    ext: &str,
    max_chars: usize,
) -> Result<String, ToolError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| ToolError::Exec(format!("read {filename}: {e}")))?;

    // Image → multimodal vision request.
    let user_blocks = if let Some(media_type) = image_media_type(ext) {
        vec![
            Block::Text(format!(
                "Extract and return ONLY the readable text/content of the image \
                 `{filename}`. If there is no legible text, briefly describe the image."
            )),
            Block::image_bytes(media_type, &bytes),
        ]
    } else {
        // Non-image → raw text extraction.
        let raw: String = String::from_utf8_lossy(&bytes).chars().take(max_chars).collect();
        vec![Block::Text(format!(
            "Local parsers could not extract text from `{filename}`. Below is its \
             raw content. Return ONLY the readable text; if it is unreadable binary, \
             reply exactly: (no extractable text).\n\n---- RAW ----\n{raw}\n---- END ----"
        ))]
    };

    let mut ctx = Context::new(Task {
        description: format!("extract content from {filename}"),
        source: None,
        deadline: None,
    });
    ctx.history.push(Turn {
        role: TurnRole::User,
        blocks: user_blocks,
    });

    let out = model
        .complete(&ctx)
        .await
        .map_err(|e| ToolError::Exec(format!("llm fallback: {e}")))?;
    Ok(out.text.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_context::default_world;
    use harness_models::{MockModel, MockResponse};

    fn ws_with(name: &str, bytes: &[u8]) -> (PathBuf, World) {
        let dir = std::env::temp_dir().join(format!("docs-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(name), bytes).unwrap();
        let world = default_world(&dir);
        (dir, world)
    }

    #[test]
    fn jail_rejects_escape() {
        let tmp = std::env::temp_dir();
        assert!(resolve_in_jail(&tmp, "../../../etc/hosts").is_err());
    }

    #[tokio::test]
    async fn unparseable_without_fallback_errors() {
        let (_d, mut world) = ws_with("note.bin", b"\x00\x01raw");
        let err = ReadDocument::new()
            .invoke(json!({"path": "note.bin"}), &mut world)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("LLM fallback"));
    }

    #[tokio::test]
    async fn falls_back_to_llm_when_local_cannot_parse() {
        let (_d, mut world) = ws_with("note.bin", b"some bytes with no local parser");
        let model = Arc::new(MockModel::new().script(MockResponse::text("EXTRACTED-TEXT")))
            as Arc<dyn Model>;
        let out = ReadDocument::with_llm_fallback(model.clone())
            .invoke(json!({"path": "note.bin"}), &mut world)
            .await
            .unwrap();
        assert_eq!(out.content["source"], "llm");
        assert_eq!(out.content["text"], "EXTRACTED-TEXT");
    }

    #[tokio::test]
    async fn image_is_sent_to_the_model_as_vision() {
        let (_d, mut world) = ws_with("photo.png", b"\x89PNG\r\n\x1a\nfake-bytes");
        let mock = Arc::new(MockModel::new().script(MockResponse::text("a cat")));
        let out = ReadDocument::with_llm_fallback(mock.clone() as Arc<dyn Model>)
            .invoke(json!({"path": "photo.png"}), &mut world)
            .await
            .unwrap();
        assert_eq!(out.content["source"], "llm");
        // The model must have actually received an image block (vision), not text.
        let calls = mock.calls();
        let saw_image = calls.iter().any(|c| {
            c.history_summary
                .iter()
                .any(|h| h.kinds.iter().any(|k| *k == "image"))
        });
        assert!(saw_image, "the image file must reach the model as a Block::Image");
    }
}
