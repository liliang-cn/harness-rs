//! Conversation persistence across requests.
//!
//! A single [`ChatService`](crate::ChatService) is shared by many employees and
//! many concurrent conversations; the [`SessionStore`] keeps each conversation's
//! history so multi-turn context survives between stateless requests. The
//! history is seeded into `ctx.history` (where the compactor can see it), not
//! concatenated into the prompt.

use harness_core::{Block, Turn, TurnRole};
use std::collections::HashMap;
use std::sync::Mutex;

/// Per-session conversation history. Implement against a DB for durability;
/// [`InMemorySessions`] is the zero-config default.
pub trait SessionStore: Send + Sync + 'static {
    /// Prior turns for `session_id` (empty for a new conversation).
    fn history(&self, session_id: &str) -> Vec<Turn>;
    /// Append one completed exchange (user message + assistant answer).
    fn append(&self, session_id: &str, user: &str, assistant: &str);
}

/// In-process, non-durable session history. Fine for a single-machine service
/// that tolerates losing history on restart; swap for a DB-backed store when it
/// must persist.
#[derive(Default)]
pub struct InMemorySessions {
    inner: Mutex<HashMap<String, Vec<Turn>>>,
}

impl InMemorySessions {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for InMemorySessions {
    fn history(&self, session_id: &str) -> Vec<Turn> {
        self.inner
            .lock()
            .unwrap()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    fn append(&self, session_id: &str, user: &str, assistant: &str) {
        let mut g = self.inner.lock().unwrap();
        let hist = g.entry(session_id.to_string()).or_default();
        hist.push(Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(user.to_string())],
        });
        hist.push(Turn {
            role: TurnRole::Assistant,
            blocks: vec![Block::Text(assistant.to_string())],
        });
    }
}
