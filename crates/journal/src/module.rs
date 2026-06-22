use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use journal_mcp_core::{FileProjection, JournalCore, SchemaRegistry};
use lds_core::Session;

/// Journal module — session-scoped wrapper around `journal-mcp-core::JournalCore`.
///
/// One instance per session. The EventLog DB path is resolved literally
/// from [`Session::root`] as `<root>/workspace/.journal.db`. The
/// `workspace/` directory is created if missing so the first write
/// succeeds.
///
/// `JournalCore` is wrapped in [`std::sync::Mutex`] because the underlying
/// rusqlite `Connection` is `Send` but not `Sync`, while the lds Inner
/// state (which holds this module) lives behind `Arc<RwLock<…>>` and so
/// requires every field to be `Sync`. The Mutex is held only for the
/// duration of one operation and is never held across an `.await` point
/// (caller is responsible for not awaiting while holding the guard).
///
/// File projection (renders chapters to `<root>/workspace/journal.md`) is
/// opt-in via [`JournalModule::enable_file_projection`] /
/// [`JournalModule::enable_file_projection_at`]. The attached path (if any)
/// is tracked separately so that the `journal_info` MCP tool can report it.
pub struct JournalModule {
    core: Mutex<JournalCore>,
    session: Arc<Session>,
    file_projection_path: Mutex<Option<PathBuf>>,
}

impl JournalModule {
    /// Construct a new module from a session. Opens the EventLog DB and
    /// builds the schema registry with project-local search rooted at the
    /// session root. No projection is attached.
    pub fn new(session: Arc<Session>) -> Result<Self> {
        let workspace_dir = session.root().join("workspace");
        std::fs::create_dir_all(&workspace_dir).with_context(|| {
            format!(
                "failed to create workspace dir: {}",
                workspace_dir.display()
            )
        })?;
        let db_path = workspace_dir.join(".journal.db");
        let registry = SchemaRegistry::with_project_local(session.root())
            .context("failed to build schema registry")?;
        let core =
            JournalCore::open(&db_path, registry).context("failed to open JournalCore EventLog")?;
        Ok(Self {
            core: Mutex::new(core),
            session,
            file_projection_path: Mutex::new(None),
        })
    }

    /// Lock the inner `JournalCore` for exclusive access.
    ///
    /// **Caller responsibility**: do NOT hold this guard across an `.await`
    /// point. Drop it before any async operation.
    pub fn lock_core(&self) -> MutexGuard<'_, JournalCore> {
        self.core
            .lock()
            .expect("JournalCore Mutex poisoned — another thread panicked while holding it")
    }

    /// Attach a `FileProjection` writing to `<root>/workspace/journal.md`.
    ///
    /// Builds a fresh schema registry (the one inside `JournalCore` was
    /// moved at construction; `FileProjection` needs its own `Arc<SchemaRegistry>`).
    /// Caller decides when to call — typically gated by
    /// `LDS_JOURNAL_FILE_ENABLE` env var or a runtime
    /// `journal_projection_attach` MCP call.
    pub fn enable_file_projection(&self) -> Result<()> {
        let output_path = self.session.root().join("workspace").join("journal.md");
        self.enable_file_projection_at(output_path)
    }

    /// Attach a `FileProjection` writing to an explicit `output_path`.
    /// Used when the caller (env / config / runtime tool call) overrides
    /// the default location.
    pub fn enable_file_projection_at(&self, output_path: PathBuf) -> Result<()> {
        let registry = SchemaRegistry::with_project_local(self.session.root())
            .context("failed to build schema registry for FileProjection")?;
        {
            let mut core = self.lock_core();
            core.add_projection(FileProjection::new(output_path.clone(), Arc::new(registry)));
        }
        let mut tracked = self
            .file_projection_path
            .lock()
            .expect("file_projection_path Mutex poisoned");
        *tracked = Some(output_path);
        Ok(())
    }

    pub fn session(&self) -> &Arc<Session> {
        &self.session
    }

    /// Returns the path of the currently attached file projection, if any.
    /// `None` when no projection is attached.
    pub fn file_projection_path(&self) -> Option<PathBuf> {
        self.file_projection_path
            .lock()
            .expect("file_projection_path Mutex poisoned")
            .clone()
    }
}
