use harness_context::FileRecall;
use harness_core::{RecallStore, recall_contract};
use std::sync::Arc;

#[tokio::test]
async fn file_recall_satisfies_contract() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "harness-recall-contract-file-{}-{nanos}",
        std::process::id()
    ));
    let store: Arc<dyn RecallStore> = Arc::new(FileRecall::open(&root).unwrap());
    recall_contract(store).await;
    let _ = std::fs::remove_dir_all(&root);
}
