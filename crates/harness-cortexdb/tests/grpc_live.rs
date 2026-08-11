//! Live round-trip against a running CortexDB. Skipped unless
//! `CORTEXDB_GRPC` names one, e.g.
//!
//! ```sh
//! CORTEXDB_GRPC=http://127.0.0.1:47821 cargo test -p harness-rs-cortexdb --features grpc --test grpc_live
//! ```
//!
//! It exists because the sibling MCP `connect_http` shipped without ever
//! reaching a real server: the transport compiled, the feature was wired, and
//! nothing had proved a memory could be written and read back. A constructor
//! that has never spoken to the thing it names is a guess.
#![cfg(feature = "grpc")]

use harness_core::{Memory, MemoryEntry};
use harness_cortexdb::CortexdbGrpcMemory;

#[tokio::test]
async fn a_memory_written_over_grpc_comes_back() {
    let Ok(endpoint) = std::env::var("CORTEXDB_GRPC") else {
        eprintln!("set CORTEXDB_GRPC to run this");
        return;
    };

    let mem = CortexdbGrpcMemory::connect(endpoint)
        .await
        .expect("connect to CortexDB")
        .with_scope("session")
        .with_session_id("probe")
        .with_namespace("harness-grpc-live");

    // A phrase unlikely to collide with anything already in the brain.
    let marker = format!("harness grpc live probe {}", std::process::id());
    mem.write(
        MemoryEntry::new(&marker)
            .with_source("harness-test")
            .with_tags(["role:user", "session:probe", "grpc"]),
    )
    .await
    .expect("write");

    let hits = mem.recall(&marker, 5).await.expect("recall");
    let found = hits.iter().find(|e| e.content == marker);
    let found = found.unwrap_or_else(|| {
        panic!("the memory just written was not recalled; got {hits:#?}");
    });

    // The round-trip has to preserve what the caller put in, including the tags
    // that were promoted into CortexDB's typed fields on the way out.
    assert_eq!(found.source.as_deref(), Some("harness-test"));
    assert!(found.tags.contains(&"grpc".to_string()), "{:?}", found.tags);
    assert!(
        found.tags.contains(&"role:user".to_string()),
        "role must come back as a tag: {:?}",
        found.tags
    );
}
