//! Reusable conformance suite for [`crate::RecallStore`] backends. Each backend
//! crate calls `recall_contract(store)` from a `#[tokio::test]` so all impls are
//! held to identical behaviour — including the privacy-critical owner isolation.

use crate::{RecallMessage, RecallStore, SessionMeta};
use std::sync::Arc;

/// Run the full contract against a fresh, empty `store`.
pub async fn recall_contract(store: Arc<dyn RecallStore>) {
    // ── append + search round-trip ──
    store
        .ensure_session("alice", "s1", &SessionMeta::new("s1", 100))
        .await
        .unwrap();
    store
        .append(
            "alice",
            "s1",
            &RecallMessage::new("user", "refactor the auth module today", 100),
        )
        .await
        .unwrap();
    store
        .append(
            "alice",
            "s1",
            &RecallMessage::new("assistant", "starting the auth refactor now", 101),
        )
        .await
        .unwrap();
    store
        .append(
            "alice",
            "s1",
            &RecallMessage::new("tool", "edited auth.rs", 102).with_tool_name("edit"),
        )
        .await
        .unwrap();

    let hits = store.search("alice", "auth refactor", 5).await.unwrap();
    assert_eq!(hits.len(), 1, "search should find the session");
    assert_eq!(hits[0].session.session_id, "s1");
    assert!(!hits[0].bookend_start.is_empty(), "hit carries bookends");

    // ── scroll window ──
    let scrolled = store.scroll("alice", "s1", 2, 1).await.unwrap();
    assert!(scrolled.iter().all(|m| (m.id - 2).abs() <= 1));
    assert!(scrolled.iter().any(|m| m.id == 2));

    // ── recent ordering ──
    store
        .ensure_session("alice", "s2", &SessionMeta::new("s2", 200))
        .await
        .unwrap();
    store
        .append(
            "alice",
            "s2",
            &RecallMessage::new("user", "a newer session", 200),
        )
        .await
        .unwrap();
    let recent = store.recent("alice", 10).await.unwrap();
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].session_id, "s2", "newest first");

    // ── OWNER ISOLATION (privacy-critical) ──
    let bob_search = store.search("bob", "auth refactor", 5).await.unwrap();
    assert!(bob_search.is_empty(), "bob must not see alice's sessions");
    let bob_recent = store.recent("bob", 10).await.unwrap();
    assert!(bob_recent.is_empty(), "bob has no sessions");
    let bob_scroll = store.scroll("bob", "s1", 1, 5).await.unwrap();
    assert!(bob_scroll.is_empty(), "bob cannot scroll alice's session");

    // ── empty query is not an error ──
    let empty = store.search("alice", "", 5).await.unwrap();
    assert!(empty.is_empty());
}
