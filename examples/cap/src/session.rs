//! On-disk session store: conversation transcripts under `~/.cap/sessions`, so a
//! run can be continued (`--continue`), resumed by id/path (`--resume`), named
//! (`--session <name>`), and listed (`--sessions`).
//!
//! We persist only the user/assistant *text* turns (which is all CAP's REPL
//! feeds back), so the store doesn't depend on the framework's block serde and
//! stays a small, stable JSON shape.

use crate::agent::cap_home;
use harness_core::{Block, Turn, TurnRole};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionTurn {
    pub role: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Session {
    pub id: String,
    pub workspace: String,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub turns: Vec<SessionTurn>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn canon(p: &Path) -> String {
    p.canonicalize()
        .unwrap_or_else(|_| p.to_path_buf())
        .display()
        .to_string()
}

/// `~/.cap/sessions`, created if missing.
pub fn sessions_dir() -> PathBuf {
    let d = cap_home().join("sessions");
    let _ = std::fs::create_dir_all(&d);
    d
}

impl Session {
    /// A fresh session for `workspace` with a time-based id.
    pub fn new(workspace: &Path) -> Self {
        let ms = now_ms();
        Self {
            id: format!("cap-{ms}"),
            workspace: canon(workspace),
            created_ms: ms,
            updated_ms: ms,
            turns: Vec::new(),
        }
    }

    /// A named session: load it if `<name>.json` exists, else create it.
    pub fn named(name: &str, workspace: &Path) -> Self {
        load(name).unwrap_or_else(|_| Self {
            id: name.to_string(),
            workspace: canon(workspace),
            created_ms: now_ms(),
            updated_ms: now_ms(),
            turns: Vec::new(),
        })
    }

    pub fn path(&self) -> PathBuf {
        sessions_dir().join(format!("{}.json", self.id))
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let buf = serde_json::to_string_pretty(self)?;
        std::fs::write(self.path(), buf)?;
        Ok(())
    }

    pub fn push(&mut self, role: &str, text: &str) {
        self.turns.push(SessionTurn {
            role: role.to_string(),
            text: text.to_string(),
        });
        self.updated_ms = now_ms();
    }

    /// Reconstruct the framework `Turn` history to seed the loop.
    pub fn seed(&self) -> Vec<Turn> {
        self.turns
            .iter()
            .map(|t| Turn {
                role: if t.role == "assistant" {
                    TurnRole::Assistant
                } else {
                    TurnRole::User
                },
                blocks: vec![Block::Text(t.text.clone())],
            })
            .collect()
    }

    /// First user message, for listings.
    pub fn first_prompt(&self) -> &str {
        self.turns
            .iter()
            .find(|t| t.role == "user")
            .map(|t| t.text.as_str())
            .unwrap_or("")
    }
}

/// Load a session by explicit path, or by id/name under `~/.cap/sessions`.
pub fn load(path_or_id: &str) -> anyhow::Result<Session> {
    let p = Path::new(path_or_id);
    let file = if p.exists() {
        p.to_path_buf()
    } else {
        sessions_dir().join(format!("{path_or_id}.json"))
    };
    let s = std::fs::read_to_string(&file)
        .map_err(|e| anyhow::anyhow!("read session {}: {e}", file.display()))?;
    Ok(serde_json::from_str(&s)?)
}

/// The most recently updated session for `workspace`, if any.
pub fn latest_for(workspace: &Path) -> Option<Session> {
    let ws = canon(workspace);
    list()
        .into_iter()
        .filter(|s| s.workspace == ws)
        .max_by_key(|s| s.updated_ms)
}

/// Every stored session, newest first.
pub fn list() -> Vec<Session> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("json")
                && let Ok(s) = std::fs::read_to_string(e.path())
                && let Ok(sess) = serde_json::from_str::<Session>(&s)
            {
                out.push(sess);
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.updated_ms));
    out
}
