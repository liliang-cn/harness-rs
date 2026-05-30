use harness_core::{recall_contract, RecallStore};
use harness_recall_sqlite::SqliteRecall;
use std::sync::Arc;

#[tokio::test]
async fn sqlite_recall_satisfies_contract() {
    let store: Arc<dyn RecallStore> = Arc::new(SqliteRecall::open_in_memory().unwrap());
    recall_contract(store).await;
}
