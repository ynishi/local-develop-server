//! Sandboxed file operations with automatic snapshot and rollback.
//!
//! Every write/edit/append creates a pre-snapshot of the target file,
//! enabling rollback to any prior state without git involvement.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Snapshot {
    pub id: String,
    pub path: String,
    pub timestamp: u64,
    pub size: u64,
}

#[derive(Debug)]
pub struct SandboxFs {
    root: PathBuf,
    snapshots_dir: PathBuf,
    index: Mutex<HashMap<String, Vec<Snapshot>>>,
}

impl SandboxFs {
    pub fn new(root: &Path) -> Result<Self> {
        let snapshots_dir = root.join(".sandbox-snapshots");
        std::fs::create_dir_all(&snapshots_dir)
            .context("failed to create .sandbox-snapshots")?;
        Ok(Self {
            root: root.to_path_buf(),
            snapshots_dir,
            index: Mutex::new(HashMap::new()),
        })
    }

    fn ensure_within_root(&self, path: &Path) -> Result<PathBuf> {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let canon_root = self.root.canonicalize().unwrap_or_else(|_| self.root.clone());
        let canon_path = if resolved.exists() {
            resolved.canonicalize().unwrap_or_else(|_| resolved.clone())
        } else {
            let parent = resolved.parent().unwrap_or(&self.root);
            let canon_parent = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
            canon_parent.join(resolved.file_name().unwrap_or_default())
        };
        if !canon_path.starts_with(&canon_root) {
            bail!(
                "path traversal denied: {} is outside root {}",
                path.display(),
                self.root.display(),
            );
        }
        Ok(resolved)
    }

    fn snapshot_file(&self, path: &Path) -> Result<Option<Snapshot>> {
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read(path)?;
        let id = snap_id();
        let snap_path = self.snapshots_dir.join(format!("{id}.snap"));
        std::fs::write(&snap_path, &content)?;

        let rel = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let snap = Snapshot {
            id,
            path: rel.clone(),
            timestamp: now_epoch(),
            size: content.len() as u64,
        };

        let mut index = self.index.lock().unwrap();
        index.entry(rel).or_default().push(snap.clone());

        Ok(Some(snap))
    }

    pub fn write(&self, path: &str, content: &str) -> Result<WriteResult> {
        let resolved = self.ensure_within_root(Path::new(path))?;
        let snapshot = self.snapshot_file(&resolved)?;

        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&resolved, content)?;

        Ok(WriteResult {
            path: path.to_string(),
            bytes_written: content.len(),
            snapshot_id: snapshot.map(|s| s.id),
        })
    }

    pub fn edit(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<EditResult> {
        let resolved = self.ensure_within_root(Path::new(path))?;
        if !resolved.exists() {
            bail!("file not found: {path}");
        }
        let snapshot = self.snapshot_file(&resolved)?;

        let original = std::fs::read_to_string(&resolved)?;
        if !original.contains(old_string) {
            bail!("old_string not found in {path}");
        }

        let updated = if replace_all {
            original.replace(old_string, new_string)
        } else {
            original.replacen(old_string, new_string, 1)
        };
        std::fs::write(&resolved, &updated)?;

        Ok(EditResult {
            path: path.to_string(),
            replacements: if replace_all {
                original.matches(old_string).count()
            } else {
                1
            },
            snapshot_id: snapshot.map(|s| s.id),
        })
    }

    pub fn append(&self, path: &str, content: &str) -> Result<WriteResult> {
        let resolved = self.ensure_within_root(Path::new(path))?;
        let snapshot = self.snapshot_file(&resolved)?;

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&resolved)?;
        file.write_all(content.as_bytes())?;

        Ok(WriteResult {
            path: path.to_string(),
            bytes_written: content.len(),
            snapshot_id: snapshot.map(|s| s.id),
        })
    }

    pub fn read(&self, path: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String> {
        let resolved = self.ensure_within_root(Path::new(path))?;
        let content = std::fs::read_to_string(&resolved)
            .with_context(|| format!("failed to read {path}"))?;

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.unwrap_or(0).min(lines.len());
        let count = limit.unwrap_or(lines.len());
        let end = (start + count).min(lines.len());

        Ok(lines[start..end].join("\n"))
    }

    pub fn head(&self, path: &str, lines: Option<usize>) -> Result<String> {
        self.read(path, Some(0), Some(lines.unwrap_or(20)))
    }

    pub fn tail(&self, path: &str, lines: Option<usize>) -> Result<String> {
        let resolved = self.ensure_within_root(Path::new(path))?;
        let content = std::fs::read_to_string(&resolved)
            .with_context(|| format!("failed to read {path}"))?;

        let all_lines: Vec<&str> = content.lines().collect();
        let n = lines.unwrap_or(20);
        let start = all_lines.len().saturating_sub(n);

        Ok(all_lines[start..].join("\n"))
    }

    pub fn rollback(&self, path: &str, snapshot_id: Option<&str>) -> Result<RollbackResult> {
        let resolved = self.ensure_within_root(Path::new(path))?;
        let rel = resolved
            .strip_prefix(&self.root)
            .unwrap_or(&resolved)
            .to_string_lossy()
            .to_string();

        let index = self.index.lock().unwrap();
        let snapshots = index
            .get(&rel)
            .ok_or_else(|| anyhow::anyhow!("no snapshots for {path}"))?;

        let target_id = match snapshot_id {
            Some(id) => snapshots
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.id.clone())
                .ok_or_else(|| anyhow::anyhow!("snapshot not found: {id}"))?,
            None => snapshots
                .last()
                .map(|s| s.id.clone())
                .ok_or_else(|| anyhow::anyhow!("no snapshots for {path}"))?,
        };
        drop(index);

        let snap_path = self.snapshots_dir.join(format!("{target_id}.snap"));
        let content = std::fs::read(&snap_path)
            .with_context(|| format!("snapshot file missing: {target_id}"))?;

        self.snapshot_file(&resolved)?;
        std::fs::write(&resolved, &content)?;

        Ok(RollbackResult {
            path: path.to_string(),
            restored_snapshot_id: target_id,
            restored_size: content.len() as u64,
        })
    }

    pub fn history(&self, path: &str) -> Result<Vec<Snapshot>> {
        let resolved = self.ensure_within_root(Path::new(path))?;
        let rel = resolved
            .strip_prefix(&self.root)
            .unwrap_or(&resolved)
            .to_string_lossy()
            .to_string();

        let index = self.index.lock().unwrap();
        let mut snapshots = index.get(&rel).cloned().unwrap_or_default();
        snapshots.reverse();
        Ok(snapshots)
    }
}

#[derive(Debug, serde::Serialize)]
pub struct WriteResult {
    pub path: String,
    pub bytes_written: usize,
    pub snapshot_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct EditResult {
    pub path: String,
    pub replacements: usize,
    pub snapshot_id: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct RollbackResult {
    pub path: String,
    pub restored_snapshot_id: String,
    pub restored_size: u64,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn snap_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("snap-{ts:x}-{pid:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("file.txt"), "original").unwrap();
        let result = fs.write("file.txt", "updated").unwrap();
        assert!(result.snapshot_id.is_some());
        assert_eq!(result.bytes_written, 7);

        let content = std::fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(content, "updated");
    }

    #[test]
    fn write_new_file_no_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        let result = fs.write("new.txt", "content").unwrap();
        assert!(result.snapshot_id.is_none());
    }

    #[test]
    fn edit_creates_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("file.txt"), "hello world").unwrap();
        let result = fs.edit("file.txt", "world", "rust", false).unwrap();
        assert!(result.snapshot_id.is_some());
        assert_eq!(result.replacements, 1);

        let content = std::fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(content, "hello rust");
    }

    #[test]
    fn edit_replace_all() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("file.txt"), "aa bb aa").unwrap();
        let result = fs.edit("file.txt", "aa", "cc", true).unwrap();
        assert_eq!(result.replacements, 2);

        let content = std::fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(content, "cc bb cc");
    }

    #[test]
    fn append_creates_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("file.txt"), "line1\n").unwrap();
        let result = fs.append("file.txt", "line2\n").unwrap();
        assert!(result.snapshot_id.is_some());

        let content = std::fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(content, "line1\nline2\n");
    }

    #[test]
    fn read_with_offset_and_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("file.txt"), "a\nb\nc\nd\ne").unwrap();
        let content = fs.read("file.txt", Some(1), Some(2)).unwrap();
        assert_eq!(content, "b\nc");
    }

    #[test]
    fn head_and_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        let lines: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        std::fs::write(tmp.path().join("file.txt"), &lines).unwrap();

        let head = fs.head("file.txt", Some(3)).unwrap();
        assert_eq!(head, "line1\nline2\nline3");

        let tail = fs.tail("file.txt", Some(3)).unwrap();
        assert_eq!(tail, "line8\nline9\nline10");
    }

    #[test]
    fn rollback_restores_content() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("file.txt"), "v1").unwrap();
        fs.write("file.txt", "v2").unwrap();
        fs.write("file.txt", "v3").unwrap();

        let result = fs.rollback("file.txt", None).unwrap();
        let content = std::fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(content, "v2");
        assert!(!result.restored_snapshot_id.is_empty());
    }

    #[test]
    fn rollback_by_id() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("file.txt"), "v1").unwrap();
        let r1 = fs.write("file.txt", "v2").unwrap();
        fs.write("file.txt", "v3").unwrap();

        let first_snap_id = r1.snapshot_id.unwrap();
        fs.rollback("file.txt", Some(&first_snap_id)).unwrap();
        let content = std::fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(content, "v1");
    }

    #[test]
    fn history_newest_first() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        std::fs::write(tmp.path().join("file.txt"), "v1").unwrap();
        fs.write("file.txt", "v2").unwrap();
        fs.write("file.txt", "v3").unwrap();

        let history = fs.history("file.txt").unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[0].timestamp >= history[1].timestamp);
    }

    #[test]
    fn path_traversal_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let fs = SandboxFs::new(tmp.path()).unwrap();

        let err = fs.write("../../etc/passwd", "hacked");
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("path traversal denied"));
    }
}
