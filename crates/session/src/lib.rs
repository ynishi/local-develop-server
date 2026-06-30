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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MAX_OUTPUT: usize = 102_400; // 100KB

/// Configuration passed to [`Session::new`]. Optional fields fall back
/// to sensible defaults (60s timeout, 100KB output limit).
#[derive(Debug, Default)]
pub struct SessionConfig {
    pub root: PathBuf,
    pub timeout_secs: Option<u64>,
    pub max_output: Option<usize>,
    /// Optional human-readable alias for this session.
    ///
    /// Used by callers (MainAI / SubAgent) to dispatch by a stable label
    /// instead of the opaque `session_id`. Aliases are case-sensitive and
    /// must be unique within an [`LdsState`] ledger.
    pub alias: Option<String>,
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
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("alias already in use: {0}")]
    AliasConflict(String),
}

/// Immutable session state created by `session_start` / `session_create`.
///
/// Cloned (via `Arc`) into each module. Holds the project root and
/// cross-cutting concerns that every module may need.
///
/// `alias` and `last_used_at` are interior-mutable (RwLock) so the ledger
/// can rename or touch sessions without invalidating outstanding `Arc<Session>`
/// handles held by tool handlers.
#[derive(Debug)]
pub struct Session {
    root: PathBuf,
    session_id: String,
    alias: RwLock<Option<String>>,
    timeout: Duration,
    max_output: usize,
    global_recipe_dirs: Vec<PathBuf>,
    created_at: u64,
    last_used_at: RwLock<u64>,
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
        let now = epoch_secs();
        tracing::info!(
            root = %root.display(),
            session_id = %session_id,
            alias = ?config.alias,
            timeout_secs = timeout.as_secs(),
            max_output,
            "session started"
        );
        let global_recipe_dirs = config.global_recipe_dirs;
        Ok(Self {
            root,
            session_id,
            alias: RwLock::new(config.alias),
            timeout,
            max_output,
            global_recipe_dirs,
            created_at: now,
            last_used_at: RwLock::new(now),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn alias(&self) -> Option<String> {
        self.alias.read().expect("alias lock poisoned").clone()
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn last_used_at(&self) -> u64 {
        *self.last_used_at.read().expect("last_used lock poisoned")
    }

    pub fn touch(&self) {
        if let Ok(mut g) = self.last_used_at.write() {
            *g = epoch_secs();
        }
    }

    pub(crate) fn set_alias(&self, alias: Option<String>) {
        if let Ok(mut g) = self.alias.write() {
            *g = alias;
        }
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
/// Holds a ledger of all live [`Session`]s, indexed by `session_id` (opaque
/// hash) and `alias` (human-readable label). One session is designated the
/// **default session**, returned by [`Self::session`] for backward-compatible
/// tool calls that pre-date per-call `session_id` addressing.
///
/// The MCP handler wraps this in `Arc<RwLock<LdsState>>` for concurrent tool
/// access. Mutations (create / close / alias) take the write lock; reads
/// (resolve / list / describe / doctor) take the read lock.
#[derive(Debug, Clone)]
pub struct LdsState {
    /// id -> Arc<Session>
    sessions: HashMap<String, Arc<Session>>,
    /// alias -> id (alias resolution map)
    aliases: HashMap<String, String>,
    /// The implicit default session for backward-compat callers.
    /// `None` until the first session_start / session_create.
    default_id: Option<String>,
}

/// Snapshot of a single session entry, returned by ledger introspection APIs.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub session_id: String,
    pub alias: Option<String>,
    pub root: PathBuf,
    pub created_at: u64,
    pub last_used_at: u64,
    pub is_default: bool,
}

impl SessionEntry {
    fn from_session(s: &Session, is_default: bool) -> Self {
        Self {
            session_id: s.id().to_string(),
            alias: s.alias(),
            root: s.root().to_path_buf(),
            created_at: s.created_at(),
            last_used_at: s.last_used_at(),
            is_default,
        }
    }
}

impl LdsState {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            aliases: HashMap::new(),
            default_id: None,
        }
    }

    /// Create a new session and register it in the ledger.
    ///
    /// If `make_default` is true (or no default exists yet), the new session
    /// becomes the implicit default.
    ///
    /// Returns [`CoreError::AliasConflict`] if `config.alias` is already
    /// taken by another session.
    pub fn create_session(
        &mut self,
        config: SessionConfig,
        make_default: bool,
    ) -> Result<Arc<Session>, CoreError> {
        if let Some(alias) = &config.alias
            && self.aliases.contains_key(alias)
        {
            return Err(CoreError::AliasConflict(alias.clone()));
        }
        let alias_clone = config.alias.clone();
        let session = Arc::new(Session::new(config)?);
        let id = session.id().to_string();
        self.sessions.insert(id.clone(), Arc::clone(&session));
        if let Some(alias) = alias_clone {
            self.aliases.insert(alias, id.clone());
        }
        if make_default || self.default_id.is_none() {
            self.default_id = Some(id);
        }
        Ok(session)
    }

    /// Backward-compatible entry point used by the legacy `session_start`
    /// MCP tool. Always replaces the default session.
    pub fn start_session(&mut self, config: SessionConfig) -> Result<Arc<Session>, CoreError> {
        self.create_session(config, true)
    }

    /// Look up a session by id or alias.
    ///
    /// `key` is tried as an alias first, then as a session_id. Returns
    /// [`CoreError::SessionNotFound`] if neither matches.
    pub fn resolve(&self, key: &str) -> Result<Arc<Session>, CoreError> {
        if let Some(id) = self.aliases.get(key)
            && let Some(s) = self.sessions.get(id)
        {
            return Ok(Arc::clone(s));
        }
        if let Some(s) = self.sessions.get(key) {
            return Ok(Arc::clone(s));
        }
        Err(CoreError::SessionNotFound(key.to_string()))
    }

    /// Return the default session for backward-compatible tool calls.
    pub fn session(&self) -> Result<Arc<Session>, CoreError> {
        let id = self.default_id.as_ref().ok_or(CoreError::NoSession)?;
        let s = self
            .sessions
            .get(id)
            .ok_or_else(|| CoreError::SessionNotFound(id.clone()))?;
        Ok(Arc::clone(s))
    }

    pub fn default_session_id(&self) -> Option<&str> {
        self.default_id.as_deref()
    }

    /// Snapshot every session in the ledger.
    pub fn list_sessions(&self) -> Vec<SessionEntry> {
        let mut out: Vec<SessionEntry> = self
            .sessions
            .values()
            .map(|s| {
                let is_default = self.default_id.as_deref() == Some(s.id());
                SessionEntry::from_session(s, is_default)
            })
            .collect();
        out.sort_by_key(|e| e.created_at);
        out
    }

    /// Describe a single session by id or alias.
    pub fn describe(&self, key: &str) -> Result<SessionEntry, CoreError> {
        let s = self.resolve(key)?;
        let is_default = self.default_id.as_deref() == Some(s.id());
        Ok(SessionEntry::from_session(&s, is_default))
    }

    /// Assign or change an alias on an existing session.
    ///
    /// Returns [`CoreError::AliasConflict`] if the new alias is already held
    /// by another session.
    pub fn set_alias(&mut self, key: &str, alias: String) -> Result<(), CoreError> {
        let target = self.resolve(key)?;
        if let Some(owner_id) = self.aliases.get(&alias) {
            if owner_id != target.id() {
                return Err(CoreError::AliasConflict(alias));
            }
            return Ok(());
        }
        // Drop the session's previous alias, if any.
        if let Some(prev) = target.alias() {
            self.aliases.remove(&prev);
        }
        target.set_alias(Some(alias.clone()));
        self.aliases.insert(alias, target.id().to_string());
        Ok(())
    }

    /// Remove an alias from the ledger. The session itself remains.
    pub fn unset_alias(&mut self, alias: &str) -> Result<(), CoreError> {
        let id = self
            .aliases
            .remove(alias)
            .ok_or_else(|| CoreError::SessionNotFound(alias.to_string()))?;
        if let Some(s) = self.sessions.get(&id) {
            s.set_alias(None);
        }
        Ok(())
    }

    /// Close (drop) a session by id or alias.
    ///
    /// If the closed session was the default, the default is cleared and
    /// must be re-set by a subsequent `session_start` / `session_create`
    /// with `make_default=true`.
    pub fn close(&mut self, key: &str) -> Result<(), CoreError> {
        let target = self.resolve(key)?;
        let id = target.id().to_string();
        if let Some(alias) = target.alias() {
            self.aliases.remove(&alias);
        }
        self.sessions.remove(&id);
        if self.default_id.as_deref() == Some(&id) {
            self.default_id = None;
        }
        Ok(())
    }
}

impl Default for LdsState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Doctor ────────────────────────────────────────────────────────────────

/// Per-check verdict returned by [`LdsState::doctor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
}

impl CheckStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckStatus::Ok => "ok",
            CheckStatus::Warn => "warn",
            CheckStatus::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoctorCheck {
    pub name: &'static str,
    pub status: CheckStatus,
    pub evidence: String,
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub session_id: String,
    pub alias: Option<String>,
    pub verdict: CheckStatus,
    pub checks: Vec<DoctorCheck>,
}

const IDLE_WARN_SECS: u64 = 60 * 60 * 6; // 6h idle → ledger-leak WARN

impl LdsState {
    /// Run health checks on a single session (by id / alias).
    pub fn doctor(&self, key: &str) -> Result<DoctorReport, CoreError> {
        let s = self.resolve(key)?;
        let mut checks: Vec<DoctorCheck> = Vec::new();

        // C1 root-exists
        let root = s.root();
        if root.is_dir() {
            checks.push(DoctorCheck {
                name: "root-exists",
                status: CheckStatus::Ok,
                evidence: format!("root={}", root.display()),
            });
        } else {
            checks.push(DoctorCheck {
                name: "root-exists",
                status: CheckStatus::Fail,
                evidence: format!("root missing: {}", root.display()),
            });
        }

        // C2 git-bound (presence of .git, file or dir = git worktree)
        let git_path = root.join(".git");
        if git_path.exists() {
            checks.push(DoctorCheck {
                name: "git-bound",
                status: CheckStatus::Ok,
                evidence: format!("{}/.git present", root.display()),
            });
        } else {
            checks.push(DoctorCheck {
                name: "git-bound",
                status: CheckStatus::Warn,
                evidence: "no .git in root; git_* tools will fail".into(),
            });
        }

        // C3 journal-db-writable
        let journal_dir = root.join("workspace");
        let journal_status = if !journal_dir.exists() {
            CheckStatus::Warn
        } else {
            // Probe writability by attempting to create a temp marker.
            match tempfile::NamedTempFile::new_in(&journal_dir) {
                Ok(_) => CheckStatus::Ok,
                Err(_) => CheckStatus::Fail,
            }
        };
        checks.push(DoctorCheck {
            name: "journal-db-writable",
            status: journal_status,
            evidence: format!("probe dir = {}", journal_dir.display()),
        });

        // C4 stale-lock (heuristic: look for .lock files older than 1h)
        let mut stale = Vec::new();
        for candidate in ["workspace/.journal.db.lock", ".journal.db.lock"] {
            let p = root.join(candidate);
            if let Ok(meta) = std::fs::metadata(&p)
                && let Ok(modified) = meta.modified()
                && let Ok(age) = SystemTime::now().duration_since(modified)
                && age.as_secs() > 3600
            {
                stale.push(p.display().to_string());
            }
        }
        checks.push(DoctorCheck {
            name: "stale-lock",
            status: if stale.is_empty() {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            evidence: if stale.is_empty() {
                "no stale lock found".into()
            } else {
                format!("stale: {}", stale.join(","))
            },
        });

        // C5 ownership-drift: detect multiple sessions claiming the same root.
        let conflict_count = self
            .sessions
            .values()
            .filter(|other| other.root() == root && other.id() != s.id())
            .count();
        checks.push(DoctorCheck {
            name: "ownership-drift",
            status: if conflict_count == 0 {
                CheckStatus::Ok
            } else {
                CheckStatus::Warn
            },
            evidence: if conflict_count == 0 {
                "exclusive owner".into()
            } else {
                format!("{conflict_count} other session(s) share root")
            },
        });

        // C6 root-conflict: same as C5 but raises to FAIL when ≥2 conflicts.
        checks.push(DoctorCheck {
            name: "root-conflict",
            status: match conflict_count {
                0 => CheckStatus::Ok,
                1 => CheckStatus::Warn,
                _ => CheckStatus::Fail,
            },
            evidence: format!("conflicts={conflict_count}"),
        });

        // C7 ledger-leak: idle for > IDLE_WARN_SECS.
        let idle = epoch_secs().saturating_sub(s.last_used_at());
        checks.push(DoctorCheck {
            name: "ledger-leak",
            status: if idle > IDLE_WARN_SECS {
                CheckStatus::Warn
            } else {
                CheckStatus::Ok
            },
            evidence: format!("idle_secs={idle}"),
        });

        let verdict = checks
            .iter()
            .map(|c| c.status.clone())
            .fold(CheckStatus::Ok, |acc, s| match (&acc, &s) {
                (CheckStatus::Fail, _) | (_, CheckStatus::Fail) => CheckStatus::Fail,
                (CheckStatus::Warn, _) | (_, CheckStatus::Warn) => CheckStatus::Warn,
                _ => CheckStatus::Ok,
            });

        Ok(DoctorReport {
            session_id: s.id().to_string(),
            alias: s.alias(),
            verdict,
            checks,
        })
    }
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

    // ── Ledger (multi-session) ───────────────────────────────────────────────

    fn mk_state_with_root(alias: Option<&str>) -> (LdsState, tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = LdsState::new();
        let session = state
            .create_session(
                SessionConfig {
                    root: tmp.path().to_path_buf(),
                    alias: alias.map(|s| s.to_string()),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        let id = session.id().to_string();
        (state, tmp, id)
    }

    #[test]
    fn ledger_create_sets_default_when_first_session() {
        let (state, _tmp, id) = mk_state_with_root(None);
        assert_eq!(state.default_session_id(), Some(id.as_str()));
        assert_eq!(state.list_sessions().len(), 1);
    }

    #[test]
    fn ledger_second_session_preserves_default_unless_requested() {
        let (mut state, _tmp1, id1) = mk_state_with_root(None);
        let tmp2 = tempfile::tempdir().unwrap();
        let _ = state
            .create_session(
                SessionConfig {
                    root: tmp2.path().to_path_buf(),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        assert_eq!(state.default_session_id(), Some(id1.as_str()));
        assert_eq!(state.list_sessions().len(), 2);
    }

    #[test]
    fn ledger_resolve_by_id_or_alias() {
        let (state, _tmp, id) = mk_state_with_root(Some("worker-1"));
        let by_id = state.resolve(&id).unwrap();
        let by_alias = state.resolve("worker-1").unwrap();
        assert_eq!(by_id.id(), by_alias.id());
    }

    #[test]
    fn ledger_resolve_unknown_returns_not_found() {
        let (state, _tmp, _id) = mk_state_with_root(None);
        let err = state.resolve("does-not-exist").unwrap_err();
        assert!(matches!(err, CoreError::SessionNotFound(_)), "got {err:?}");
    }

    #[test]
    fn ledger_alias_conflict_rejected_on_create() {
        let (mut state, _tmp, _id) = mk_state_with_root(Some("dup"));
        let tmp2 = tempfile::tempdir().unwrap();
        let err = state
            .create_session(
                SessionConfig {
                    root: tmp2.path().to_path_buf(),
                    alias: Some("dup".to_string()),
                    ..Default::default()
                },
                false,
            )
            .unwrap_err();
        assert!(matches!(err, CoreError::AliasConflict(_)), "got {err:?}");
    }

    #[test]
    fn ledger_set_alias_assigns_and_replaces() {
        let (mut state, _tmp, id) = mk_state_with_root(None);
        state.set_alias(&id, "main".into()).unwrap();
        assert_eq!(state.resolve("main").unwrap().id(), id);
        // Reassign to a new alias — previous one is freed.
        state.set_alias(&id, "renamed".into()).unwrap();
        assert!(state.resolve("main").is_err());
        assert_eq!(state.resolve("renamed").unwrap().id(), id);
    }

    #[test]
    fn ledger_unset_alias_removes_mapping_but_keeps_session() {
        let (mut state, _tmp, id) = mk_state_with_root(Some("tmp-alias"));
        state.unset_alias("tmp-alias").unwrap();
        assert!(state.resolve("tmp-alias").is_err());
        assert!(state.resolve(&id).is_ok());
    }

    #[test]
    fn ledger_close_drops_session_and_clears_default() {
        let (mut state, _tmp, id) = mk_state_with_root(Some("main"));
        state.close(&id).unwrap();
        assert!(state.resolve(&id).is_err());
        assert_eq!(state.default_session_id(), None);
        assert_eq!(state.list_sessions().len(), 0);
    }

    #[test]
    fn ledger_list_sorted_by_created_at() {
        let (mut state, _tmp1, id1) = mk_state_with_root(None);
        // Force a measurable timestamp gap (epoch_secs is second-resolution).
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let tmp2 = tempfile::tempdir().unwrap();
        let s2 = state
            .create_session(
                SessionConfig {
                    root: tmp2.path().to_path_buf(),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        let entries = state.list_sessions();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id, id1);
        assert_eq!(entries[1].session_id, s2.id());
    }

    // ── Doctor checks ────────────────────────────────────────────────────────

    #[test]
    fn doctor_root_exists_passes_on_live_root() {
        let (state, _tmp, id) = mk_state_with_root(None);
        let report = state.doctor(&id).unwrap();
        let root_check = report
            .checks
            .iter()
            .find(|c| c.name == "root-exists")
            .unwrap();
        assert_eq!(root_check.status, CheckStatus::Ok);
    }

    #[test]
    fn doctor_root_exists_fails_when_root_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().to_path_buf();
        let mut state = LdsState::new();
        let s = state
            .create_session(
                SessionConfig {
                    root: path.clone(),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        let report = state.doctor(s.id()).unwrap();
        let root_check = report
            .checks
            .iter()
            .find(|c| c.name == "root-exists")
            .unwrap();
        assert_eq!(root_check.status, CheckStatus::Fail);
        assert_eq!(report.verdict, CheckStatus::Fail);
    }

    #[test]
    fn doctor_detects_root_conflict_between_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = LdsState::new();
        let s1 = state
            .create_session(
                SessionConfig {
                    root: tmp.path().to_path_buf(),
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        let _s2 = state
            .create_session(
                SessionConfig {
                    root: tmp.path().to_path_buf(),
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        let report = state.doctor(s1.id()).unwrap();
        let conflict = report
            .checks
            .iter()
            .find(|c| c.name == "root-conflict")
            .unwrap();
        assert_eq!(conflict.status, CheckStatus::Warn);
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
