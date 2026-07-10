//! Offline OCR for scanned / image-only PDFs, gated behind the
//! `ocr-tesseract` feature.
//!
//! `pdf-extract` only pulls a PDF's *text layer*; a scanned document has none,
//! so extraction comes back empty. This module fills that gap the deterministic,
//! zero-token way: rasterise each page to PNG with `pdftoppm` (poppler), then
//! run `tesseract` over each image and stitch the pages back together.
//!
//! It shells out rather than linking `libtesseract` / `libpdfium`, so the crate
//! still *compiles* everywhere — OCR only needs the two binaries present at
//! *runtime*. Missing binaries surface as a clear, actionable error.

use harness_core::ToolError;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// Rasterise `pdf` and OCR every page. `lang` is a tesseract language spec
/// (e.g. `"eng"`, `"eng+chi_sim"`). Pages are joined with a blank line. The
/// result is capped to `max_chars`.
pub async fn ocr_pdf(pdf: &Path, lang: &str, max_chars: usize) -> Result<String, ToolError> {
    let dir = make_scratch_dir(pdf)?;
    let res = run(pdf, lang, max_chars, &dir).await;
    // Best-effort cleanup on every path, success or failure.
    let _ = tokio::fs::remove_dir_all(&dir).await;
    res
}

async fn run(pdf: &Path, lang: &str, max_chars: usize, dir: &Path) -> Result<String, ToolError> {
    let prefix = dir.join("page");
    rasterise(pdf, &prefix).await?;

    let mut pages = collect_pngs(dir).await?;
    if pages.is_empty() {
        return Err(ToolError::Exec(
            "pdftoppm produced no page images (empty or unreadable PDF)".into(),
        ));
    }
    pages.sort_by_key(|p| page_number(p));

    let mut out = String::new();
    for png in &pages {
        let text = tesseract(png, lang).await?;
        let text = text.trim();
        if !text.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(text);
            if out.chars().count() >= max_chars {
                break;
            }
        }
    }
    Ok(out)
}

/// `pdftoppm -png -r 300 <pdf> <prefix>` → `<prefix>-1.png`, `-2.png`, …
async fn rasterise(pdf: &Path, prefix: &Path) -> Result<(), ToolError> {
    let status = Command::new("pdftoppm")
        .arg("-png")
        .arg("-r")
        .arg("300")
        .arg(pdf)
        .arg(prefix)
        .status()
        .await
        .map_err(|e| missing_binary("pdftoppm", "poppler-utils", e))?;
    if !status.success() {
        return Err(ToolError::Exec(format!(
            "pdftoppm exited with {status} while rasterising the PDF"
        )));
    }
    Ok(())
}

/// `tesseract <png> stdout -l <lang>` → recognised text on stdout.
async fn tesseract(png: &Path, lang: &str) -> Result<String, ToolError> {
    let output = Command::new("tesseract")
        .arg(png)
        .arg("stdout")
        .arg("-l")
        .arg(lang)
        .output()
        .await
        .map_err(|e| missing_binary("tesseract", "tesseract-ocr + language packs", e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::Exec(format!(
            "tesseract failed on {}: {}",
            png.display(),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn collect_pngs(dir: &Path) -> Result<Vec<PathBuf>, ToolError> {
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| ToolError::Exec(format!("read scratch dir: {e}")))?;
    let mut pngs = Vec::new();
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| ToolError::Exec(format!("scan scratch dir: {e}")))?
    {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("png") {
            pngs.push(p);
        }
    }
    Ok(pngs)
}

/// Extract the trailing page index from `…/page-12.png` → 12, so pages sort
/// numerically even when poppler doesn't zero-pad (page-10 after page-2).
fn page_number(p: &Path) -> u32 {
    p.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.rsplit('-').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// A per-invocation scratch dir under the system temp dir. Name is derived from
/// pid + a hash of the PDF path (no RNG needed, and unique enough per call).
fn make_scratch_dir(pdf: &Path) -> Result<PathBuf, ToolError> {
    let mut h = DefaultHasher::new();
    pdf.hash(&mut h);
    let dir = std::env::temp_dir().join(format!(
        "harness-ocr-{}-{:x}",
        std::process::id(),
        h.finish()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| ToolError::Exec(format!("scratch dir: {e}")))?;
    Ok(dir)
}

fn missing_binary(bin: &str, install: &str, e: std::io::Error) -> ToolError {
    if e.kind() == std::io::ErrorKind::NotFound {
        ToolError::Exec(format!(
            "`{bin}` not found on PATH — OCR needs it installed (e.g. `{install}`). \
             Disable the `ocr-tesseract` feature to skip OCR."
        ))
    } else {
        ToolError::Exec(format!("running `{bin}`: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_number_sorts_numerically() {
        let mut v: Vec<PathBuf> = ["page-10.png", "page-2.png", "page-1.png"]
            .iter()
            .map(PathBuf::from)
            .collect();
        v.sort_by_key(|p| page_number(p));
        assert_eq!(
            v,
            vec![
                PathBuf::from("page-1.png"),
                PathBuf::from("page-2.png"),
                PathBuf::from("page-10.png"),
            ]
        );
    }

    #[test]
    fn page_number_handles_zero_padded() {
        assert_eq!(page_number(Path::new("page-07.png")), 7);
        assert_eq!(page_number(Path::new("page-01.png")), 1);
    }
}
