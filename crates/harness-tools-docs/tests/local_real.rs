//! Opt-in real-file extraction check. Point `HARNESS_DOC_TEST` at a real
//! document (pdf/docx/xlsx/…) and this asserts the local parser pulls text out
//! of it. Skipped when the env var is unset, so CI stays hermetic.
//!
//! ```sh
//! textutil -convert docx -output /tmp/s.docx <(echo "HELLO FROM DOCX")
//! HARNESS_DOC_TEST=/tmp/s.docx cargo test -p harness-rs-tools-docs --test local_real -- --nocapture
//! ```

use harness_context::default_world;
use harness_core::Tool;
use harness_tools_docs::ReadDocument;
use serde_json::json;

#[tokio::test]
async fn extracts_real_document_locally() {
    let Ok(path) = std::env::var("HARNESS_DOC_TEST") else {
        eprintln!("HARNESS_DOC_TEST unset — skipping real-file extraction test");
        return;
    };
    let p = std::path::Path::new(&path);
    let dir = p.parent().unwrap().to_path_buf();
    let name = p.file_name().unwrap().to_string_lossy().to_string();
    let mut world = default_world(&dir);

    let out = ReadDocument::new()
        .invoke(json!({ "path": name }), &mut world)
        .await
        .expect("local extraction should succeed");

    assert_eq!(out.content["source"], "local");
    let text = out.content["text"].as_str().unwrap();
    eprintln!("extracted [{}]: {:?}", out.content["format"], text);
    assert!(!text.trim().is_empty(), "extracted text must be non-empty");
}
