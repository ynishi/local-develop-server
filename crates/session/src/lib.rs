//! Session lifecycle primitives shared across all lds modules.
//!
//! Every module (git, recipe, sandbox) receives an `Arc<Session>` that
//! anchors operations to a single project root. Shared concerns — timeout,
//! output truncation, global recipe dirs — live here so modules don't
//! duplicate configuration.
//!
//! This crate was split out of `lds-core` to give downstream consumers
//! (current: lds workspace modules; future: session-mcp / KV primitives)
//! a self-contained session contract independent of the broader core
//! utilities (binary probing, output truncation, config files).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MAX_OUTPUT: usize = 102_400; // 100KB

/// Configuration passed to [`Session::new`]. Optional fields fall back
/// to sensible defaults (60s timeout, 100KB output limit).
#[derive(Debug, Default)]
pub struct SessionConfig {
    pub root: PathBuf,
    pub timeout_secs: Option<u64>,
    pub max_output: Option<usize>,
    /// Additional global recipe directories, in precedence order (lowest first).
    ///
    /// The default `~/.config/lds` is always consulted by `build_resolve_chain`
    /// regardless of this list. Entries here are pushed after the default and before
    /// the project justfile. Populate via `LDS_RECIPE_GLOBAL_DIRS` (colon-separated)
    /// and/or the `global_recipe_dir` MCP wire argument.
    pub global_recipe_dirs: Vec<PathBuf>,
}

/// Errors that can occur during a [`Session`]'s post-construction lifecycle.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(
        "session root path no longer exists, please call session_start again: {}",
        _0.display()
    )]
    RootGone(PathBuf),
}

/// Errors that can occur during session construction or access.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("session root does not exist: {}", _0.display())]
    RootNotFound(PathBuf),
    #[error("no active session — call session_start first")]
    NoSession,
}

/// Immutable session state created by `session_start`.
///
/// Cloned (via `Arc`) into each module. Holds the project root and
/// cross-cutting concerns that every module may need.
#[derive(Debug, Clone)]
pub struct Session {
    root: PathBuf,
    session_id: String,
    timeout: Duration,
    max_output: usize,
    global_recipe_dirs: Vec<PathBuf>,
}

impl Session {
    pub fn new(config: SessionConfig) -> Result<Self, CoreError> {
        let root = config.root;
        if !root.is_dir() {
            tracing::warn!(root = %root.display(), "session root does not exist");
            return Err(CoreError::RootNotFound(root));
        }
        let session_id = session_id_new();
        let timeout = Duration::from_secs(config.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let max_output = config.max_output.unwrap_or(DEFAULT_MAX_OUTPUT);
        tracing::info!(
            root = %root.display(),
            session_id = %session_id,
            timeout_secs = timeout.as_secs(),
            max_output,
            "session started"
        );
        let global_recipe_dirs = config.global_recipe_dirs;
        Ok(Self {
            root,
            session_id,
            timeout,
            max_output,
            global_recipe_dirs,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn max_output(&self) -> usize {
        self.max_output
    }

    pub fn global_recipe_dirs(&self) -> &[PathBuf] {
        &self.global_recipe_dirs
    }

    /// Check that the session root directory still exists.
    ///
    /// Call this at each entry point (e.g. `list`, `run`) to detect a deleted
    /// root before attempting I/O. Returns [`SessionError::RootGone`] if the
    /// directory no longer exists.
    pub fn ensure_alive(&self) -> Result<(), SessionError> {
        if !self.root.is_dir() {
            tracing::warn!(root = %self.root.display(), "session root no longer exists");
            return Err(SessionError::RootGone(self.root.clone()));
        }
        Ok(())
    }
}

/// Top-level mutable state for the MCP server.
///
/// Holds the current [`Session`] (if started). The MCP handler wraps
/// this in `Arc<RwLock<LdsState>>` for concurrent tool access.
#[derive(Debug, Clone)]
pub struct LdsState {
    session: Option<Arc<Session>>,
}

impl LdsState {
    pub fn new() -> Self {
        Self { session: None }
    }

    pub fn start_session(&mut self, config: SessionConfig) -> Result<Arc<Session>, CoreError> {
        let session = Arc::new(Session::new(config)?);
        self.session = Some(Arc::clone(&session));
        Ok(session)
    }

    pub fn session(&self) -> Result<&Arc<Session>, CoreError> {
        self.session.as_ref().ok_or(CoreError::NoSession)
    }
}

impl Default for LdsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a session uniqueness identifier as `{nanos_hex}-{pid_hex}`.
///
/// This is NOT an RFC 4122 UUID — it is a lightweight identifier used
/// for session ownership tracking and log correlation. It carries no
/// cryptographic randomness guarantees. Use the `uuid` crate if a
/// RFC 4122 v4 UUID is required.
fn session_id_new() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{ts:x}-{pid:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── CoreError Display invariants (I1 / I2) ───────────────────────────────

    #[test]
    fn core_error_root_not_found_display_contains_prefix_and_path() {
        let path = PathBuf::from("/some/missing/root");
        let err = CoreError::RootNotFound(path.clone());
        let msg = err.to_string();
        assert!(
            msg.contains("session root does not exist: "),
            "I1: message must start with invariant prefix, got: {msg}"
        );
        assert!(
            msg.contains("/some/missing/root"),
            "I1: message must contain the path, got: {msg}"
        );
    }

    #[test]
    fn core_error_no_session_display_matches_invariant() {
        let err = CoreError::NoSession;
        let msg = err.to_string();
        assert_eq!(
            msg, "no active session \u{2014} call session_start first",
            "I2: message must exactly match invariant string"
        );
    }

    // ── SessionError / Session::ensure_alive ─────────────────────────────────

    #[test]
    fn session_error_root_gone_message_contains_invariant_substring() {
        use std::path::PathBuf;
        let path = PathBuf::from("/tmp/gone");
        let err = SessionError::RootGone(path.clone());
        let msg = err.to_string();
        assert!(
            msg.contains("session root path no longer exists, please call session_start again"),
            "error message must contain the K-239 recovery substring, got: {msg}"
        );
        assert!(
            msg.contains("/tmp/gone"),
            "error message must include the path, got: {msg}"
        );
    }

    #[test]
    fn ensure_alive_ok_when_root_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let session = Session::new(SessionConfig {
            root: tmp.path().to_path_buf(),
            ..Default::default()
        })
        .unwrap();
        assert!(session.ensure_alive().is_ok());
    }

    #[test]
    fn ensure_alive_err_when_root_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        let session = Session::new(SessionConfig {
            root: path.clone(),
            ..Default::default()
        })
        .unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        let err = session.ensure_alive().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("session root path no longer exists, please call session_start again"),
            "expected K-239 substring, got: {msg}"
        );
    }
}
