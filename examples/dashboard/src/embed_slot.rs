//! Process-global slot for the active `Embedder`. Set once at startup;
//! tools read it via `get()` since they can't carry an `Arc<dyn Embedder>`
//! through `world.profile.extra` (which is a JSON map).

use harness_core::Embedder;
use std::sync::{Arc, OnceLock};

static SLOT: OnceLock<Arc<dyn Embedder>> = OnceLock::new();

pub fn set(e: Arc<dyn Embedder>) {
    let _ = SLOT.set(e);
}

pub fn get() -> Option<Arc<dyn Embedder>> {
    SLOT.get().cloned()
}
