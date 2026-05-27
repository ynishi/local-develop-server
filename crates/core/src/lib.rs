use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};

/// Shared session state injected by `session_start`.
/// Every module receives this to anchor operations to the project root.
#[derive(Debug, Clone)]
pub struct Session {
    root: PathBuf,
    session_id: String,
}

impl Session {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            bail!("session root does not exist: {}", root.display());
        }
        let session_id = uuid_v4();
        tracing::info!(root = %root.display(), session_id = %session_id, "session started");
        Ok(Self { root, session_id })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }
}

/// Shared state across all modules, behind Arc for MCP handler cloning.
#[derive(Debug, Clone)]
pub struct LdsState {
    session: Option<Arc<Session>>,
}

impl LdsState {
    pub fn new() -> Self {
        Self { session: None }
    }

    pub fn start_session(&mut self, root: impl Into<PathBuf>) -> Result<Arc<Session>> {
        let session = Arc::new(Session::new(root)?);
        self.session = Some(Arc::clone(&session));
        Ok(session)
    }

    pub fn session(&self) -> Result<&Arc<Session>> {
        self.session
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no active session — call session_start first"))
    }
}

impl Default for LdsState {
    fn default() -> Self {
        Self::new()
    }
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{ts:x}-{pid:x}")
}
