//! Filesystem tools.
//!
//! All paths are resolved relative to `world.repo.root`. Attempts to escape
//! the repo root (via `..` or absolute paths) are rejected — this keeps the
//! tool surface safe by default without needing a full sandbox.

use async_trait::async_trait;
use harness_core::{Tool, ToolError, ToolResult, ToolRisk, ToolSchema, World};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

// ---------- read_file ----------

#[derive(Deserialize)]
struct ReadArgs {
    path:   String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit:  Option<usize>,
}

pub struct ReadFile;
static READ_FILE_SCHEMA: Lazy<ToolSchema> = Lazy::new(|| ToolSchema {
    name: "read_file".into(),
    description: "Read a UTF-8 text file from the workspace, optionally a line range. \
                  Returns up to 2000 lines unless `limit` is set."
        .into(),
    input: json!({
        "type": "object",
        "properties": {
            "path":   {"type": "string", "description": "Path relative to the workspace root."},
            "offset": {"type": "integer", "minimum": 0, "description": "1-based line offset"},
            "limit":  {"type": "integer", "minimum": 1, "description": "Max lines to return"}
        },
        "required": ["path"]
    }),
});

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str { "read_file" }
    fn schema(&self) -> &ToolSchema { &READ_FILE_SCHEMA }
    fn risk(&self) -> ToolRisk { ToolRisk::ReadOnly }

    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let a: ReadArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs { name: self.name().into(), reason: e.to_string() })?;
        let abs = resolve(&world.repo.root, &a.path)?;
        let content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| ToolError::Exec(format!("{}: {e}", abs.display())))?;

        let offset = a.offset.unwrap_or(0);
        let limit  = a.limit.unwrap_or(2000);
        let lines:  Vec<&str> = content.lines().collect();
        let take    = lines.iter().skip(offset).take(limit).copied().collect::<Vec<&str>>();
        let total   = lines.len();
        let snippet = take.join("\n");

        Ok(ToolResult {
            ok: true,
            content: json!({
                "path":       abs.strip_prefix(&world.repo.root).unwrap_or(&abs).display().to_string(),
                "lines":      take.len(),
                "total":      total,
                "offset":     offset,
                "limit":      limit,
                "content":    snippet,
            }),
            trace: None,
        })
    }
}

// ---------- write_file ----------

#[derive(Deserialize)]
struct WriteArgs {
    path:    String,
    content: String,
}

pub struct WriteFile;
static WRITE_FILE_SCHEMA: Lazy<ToolSchema> = Lazy::new(|| ToolSchema {
    name: "write_file".into(),
    description: "Create or overwrite a UTF-8 text file in the workspace.".into(),
    input: json!({
        "type": "object",
        "properties": {
            "path":    {"type": "string"},
            "content": {"type": "string"}
        },
        "required": ["path", "content"]
    }),
});

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str { "write_file" }
    fn schema(&self) -> &ToolSchema { &WRITE_FILE_SCHEMA }
    fn risk(&self) -> ToolRisk { ToolRisk::Destructive }

    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let a: WriteArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs { name: self.name().into(), reason: e.to_string() })?;
        let abs = resolve(&world.repo.root, &a.path)?;
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Exec(format!("mkdir {}: {e}", parent.display())))?;
        }
        let bytes = a.content.len();
        tokio::fs::write(&abs, &a.content)
            .await
            .map_err(|e| ToolError::Exec(format!("write {}: {e}", abs.display())))?;
        Ok(ToolResult {
            ok: true,
            content: json!({
                "path":  abs.strip_prefix(&world.repo.root).unwrap_or(&abs).display().to_string(),
                "bytes": bytes,
            }),
            trace: None,
        })
    }
}

// ---------- list_dir ----------

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

pub struct ListDir;
static LIST_DIR_SCHEMA: Lazy<ToolSchema> = Lazy::new(|| ToolSchema {
    name: "list_dir".into(),
    description: "List entries of a directory relative to the workspace root.".into(),
    input: json!({
        "type": "object",
        "properties": { "path": {"type": "string", "description": "Empty for repo root."} }
    }),
});

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str { "list_dir" }
    fn schema(&self) -> &ToolSchema { &LIST_DIR_SCHEMA }
    fn risk(&self) -> ToolRisk { ToolRisk::ReadOnly }

    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let a: ListArgs = serde_json::from_value(args).unwrap_or(ListArgs { path: None });
        let rel = a.path.unwrap_or_default();
        let abs = if rel.is_empty() {
            world.repo.root.clone()
        } else {
            resolve(&world.repo.root, &rel)?
        };
        let mut entries = Vec::new();
        let mut rd = tokio::fs::read_dir(&abs)
            .await
            .map_err(|e| ToolError::Exec(format!("read_dir {}: {e}", abs.display())))?;
        while let Some(e) = rd
            .next_entry()
            .await
            .map_err(|e| ToolError::Exec(e.to_string()))?
        {
            let ft = e.file_type().await.ok();
            let kind = if ft.map_or(false, |f| f.is_dir()) {
                "dir"
            } else if ft.map_or(false, |f| f.is_file()) {
                "file"
            } else {
                "other"
            };
            entries.push(json!({
                "name": e.file_name().to_string_lossy(),
                "kind": kind,
            }));
        }
        entries.sort_by(|a, b| {
            a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
        });
        Ok(ToolResult {
            ok: true,
            content: json!({ "path": abs.display().to_string(), "entries": entries }),
            trace: None,
        })
    }
}

// ---------- edit_file (replace exact substring) ----------

#[derive(Deserialize)]
struct EditArgs {
    path:        String,
    old_string:  String,
    new_string:  String,
    #[serde(default)]
    replace_all: bool,
}

pub struct EditFile;
static EDIT_FILE_SCHEMA: Lazy<ToolSchema> = Lazy::new(|| ToolSchema {
    name: "edit_file".into(),
    description: "Replace `old_string` with `new_string` in a workspace file. \
                  If `replace_all` is false, `old_string` must be unique."
        .into(),
    input: json!({
        "type": "object",
        "properties": {
            "path":        {"type": "string"},
            "old_string":  {"type": "string"},
            "new_string":  {"type": "string"},
            "replace_all": {"type": "boolean", "default": false}
        },
        "required": ["path", "old_string", "new_string"]
    }),
});

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str { "edit_file" }
    fn schema(&self) -> &ToolSchema { &EDIT_FILE_SCHEMA }
    fn risk(&self) -> ToolRisk { ToolRisk::Destructive }

    async fn invoke(
        &self,
        args: serde_json::Value,
        world: &mut World,
    ) -> Result<ToolResult, ToolError> {
        let a: EditArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArgs { name: self.name().into(), reason: e.to_string() })?;
        let abs = resolve(&world.repo.root, &a.path)?;
        let content = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| ToolError::Exec(format!("read {}: {e}", abs.display())))?;

        if !content.contains(&a.old_string) {
            return Err(ToolError::Exec(format!(
                "old_string not found in {}",
                abs.display()
            )));
        }
        let new = if a.replace_all {
            content.replace(&a.old_string, &a.new_string)
        } else {
            let n = content.matches(&a.old_string).count();
            if n > 1 {
                return Err(ToolError::Exec(format!(
                    "old_string matches {n} times; pass replace_all or extend old_string for uniqueness"
                )));
            }
            content.replacen(&a.old_string, &a.new_string, 1)
        };
        let count = if a.replace_all {
            content.matches(&a.old_string).count()
        } else {
            1
        };
        tokio::fs::write(&abs, new)
            .await
            .map_err(|e| ToolError::Exec(format!("write {}: {e}", abs.display())))?;
        Ok(ToolResult {
            ok: true,
            content: json!({
                "path":         abs.strip_prefix(&world.repo.root).unwrap_or(&abs).display().to_string(),
                "replacements": count,
            }),
            trace: None,
        })
    }
}

// ---------- helpers ----------

fn resolve(root: &Path, rel: &str) -> Result<PathBuf, ToolError> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(ToolError::Permission(format!(
            "absolute paths not allowed: {rel}"
        )));
    }
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let joined = normalize(&canon_root.join(p));
    if !joined.starts_with(&canon_root) {
        return Err(ToolError::Permission(format!(
            "path escapes workspace root: {rel}"
        )));
    }
    Ok(joined)
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => { out.pop(); }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::RepoView;
    use std::sync::Arc;

    fn tmp_world() -> (tempdir::TestDir, World) {
        let td = tempdir::TestDir::new();
        let w = World {
            repo:   RepoView { root: td.0.clone() },
            runner: Arc::new(NoopRunner),
            clock:  Arc::new(NoopClock),
            kv:     Arc::new(NoopKv),
        };
        (td, w)
    }

    mod tempdir {
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        pub struct TestDir(pub PathBuf);
        impl TestDir {
            pub fn new() -> Self {
                let pid = std::process::id();
                let n = SEQ.fetch_add(1, Ordering::SeqCst);
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let p = std::env::temp_dir()
                    .join(format!("harness-tools-fs-{pid}-{nanos}-{n}"));
                std::fs::create_dir_all(&p).unwrap();
                TestDir(p)
            }
        }
        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    struct NoopRunner;
    #[async_trait]
    impl harness_core::ProcessRunner for NoopRunner {
        async fn exec(
            &self,
            _: &str,
            _: &[&str],
            _: Option<&std::path::Path>,
        ) -> std::io::Result<harness_core::ProcessOutput> {
            Ok(harness_core::ProcessOutput {
                status: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }
    struct NoopClock;
    impl harness_core::Clock for NoopClock {
        fn now_ms(&self) -> i64 { 0 }
    }
    struct NoopKv;
    #[async_trait]
    impl harness_core::KvStore for NoopKv {
        async fn get(&self, _: &str) -> Option<Vec<u8>> { None }
        async fn set(&self, _: &str, _: Vec<u8>) {}
        async fn delete(&self, _: &str) {}
    }

    #[tokio::test]
    async fn write_then_read() {
        let (_td, mut w) = tmp_world();
        let _ = WriteFile
            .invoke(json!({"path": "hello.txt", "content": "hi\nthere\n"}), &mut w)
            .await
            .unwrap();
        let out = ReadFile
            .invoke(json!({"path": "hello.txt"}), &mut w)
            .await
            .unwrap();
        let content = out.content["content"].as_str().unwrap();
        assert!(content.contains("hi"));
        assert!(content.contains("there"));
    }

    #[tokio::test]
    async fn escape_blocked() {
        let (_td, mut w) = tmp_world();
        let err = ReadFile
            .invoke(json!({"path": "../../../etc/passwd"}), &mut w)
            .await;
        assert!(matches!(err, Err(ToolError::Permission(_)) | Err(ToolError::Exec(_))));
    }

    #[tokio::test]
    async fn edit_replaces_unique_substring() {
        let (_td, mut w) = tmp_world();
        WriteFile
            .invoke(json!({"path": "x.txt", "content": "alpha beta gamma"}), &mut w)
            .await
            .unwrap();
        EditFile
            .invoke(
                json!({"path": "x.txt", "old_string": "beta", "new_string": "BETA"}),
                &mut w,
            )
            .await
            .unwrap();
        let out = ReadFile
            .invoke(json!({"path": "x.txt"}), &mut w)
            .await
            .unwrap();
        assert!(out.content["content"].as_str().unwrap().contains("BETA"));
    }
}
