//! MCP tool parameter structs + shared helpers for the journal module.
//!
//! 17 Params structs (one per tool), the [`ChapterListRow`] / [`JournalInfoResult`]
//! output types, and the [`chapter_replay_to_json`] serialization helper live
//! here. The actual `#[tool]` handlers are defined in the `local-develop-server`
//! main.rs `#[tool_router]` block so the lds tool surface stays in one place.
//!
//! Ported from journal-mcp v0.4.0 `crates/journal-mcp/src/main.rs`. The
//! per-call `project_root: Option<String>` field present on every journal-mcp
//! Params struct is dropped here: lds resolves all paths from the active
//! [`lds_core::Session::root`].

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Parameter structs — one per tool; doc comments become MCP wire descriptions
// ---------------------------------------------------------------------------

/// Parameters for `journal_open_chapter`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalOpenChapterParams {
    /// Chapter name, typically a date slug such as `"2026-06-14"`.
    pub name: String,
    /// Schema ID that governs this chapter (e.g. `"journal-mcp-canonical-v1"`).
    pub schema_id: String,
}

/// Parameters for `journal_append_section`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalAppendSectionParams {
    /// Target chapter ID (the value returned by `journal_open_chapter`).
    pub chapter_id: String,
    /// Name of the section to append (e.g. `"Verified"`).
    pub section_name: String,
    /// Body text of the section row.
    pub body: String,
}

/// Parameters for `journal_append_progress`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalAppendProgressParams {
    /// Target chapter ID.
    pub chapter_id: String,
    /// Single progress line to append (e.g. `"step 3 done"`).
    pub line: String,
}

/// Parameters for `journal_close_chapter`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalCloseChapterParams {
    /// Target chapter ID to close.
    pub chapter_id: String,
}

/// Parameters for `journal_schema_load`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalSchemaLoadParams {
    /// YAML literal conforming to the ChapterSchema format (see journal-mcp `docs/design.md §5`).
    pub yaml: String,
}

/// Parameters for `journal_schema_list` (no fields — lists all schemas).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct JournalSchemaListParams {}

/// Parameters for `journal_schema_show`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalSchemaShowParams {
    /// Registry key to look up (e.g. `"journal-mcp-canonical-v1"`).
    pub key: String,
}

/// Parameters for `journal_tail`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct JournalTailParams {
    /// Maximum number of chapters to return (default 10).
    #[serde(default)]
    pub n: Option<usize>,
}

/// Parameters for `journal_grep`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalGrepParams {
    /// Substring pattern to search for in all section bodies.
    pub pattern: String,
    /// Optional start filter: only chapters opened at or after this Unix epoch ms.
    #[serde(default)]
    pub since: Option<i64>,
    /// Optional end filter: only chapters opened at or before this Unix epoch ms.
    #[serde(default)]
    pub until: Option<i64>,
}

/// Parameters for `journal_chapter_list` (supports pagination).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct JournalChapterListParams {
    /// Maximum number of chapters to return, applied after `offset`.
    /// When `None` (default), all remaining chapters are returned.
    /// Newest chapters first (i.e. position 0 is the most recently opened chapter).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Number of chapters to skip from the start of the list, applied
    /// before `limit`. When `None` (default), no chapters are skipped.
    /// Newest chapters first, so `offset=0` starts at the most recently
    /// opened chapter. An `offset` greater than or equal to the total
    /// chapter count yields an empty result (not an error).
    #[serde(default)]
    pub offset: Option<usize>,
}

/// Parameters for `journal_open_chapters` (no fields — lists all open chapters).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct JournalOpenChaptersParams {}

/// Parameters for `journal_progress_of`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProgressOfParams {
    /// Target chapter ID whose Progress section events to return.
    pub chapter_id: String,
}

/// Parameters for `journal_projection_attach`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProjectionAttachParams {
    /// Stable name of the projection to attach (e.g. `"file"`).
    pub name: String,
    /// Optional per-call output path override. When `None`, falls back to
    /// `LDS_JOURNAL_FILE_OUTPUT_PATH` env (if set), then to
    /// `<session_root>/workspace/journal.md` (default). Relative paths
    /// resolve against `session.root()`; absolute paths are used as-is.
    /// Only meaningful when `name == "file"`.
    #[serde(default)]
    pub output_path: Option<String>,
}

/// Parameters for `journal_projection_detach`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProjectionDetachParams {
    /// Stable name of the projection to detach (e.g. `"file"`).
    pub name: String,
}

/// Parameters for `journal_projection_rebuild`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalProjectionRebuildParams {
    /// Stable name of the projection to rebuild (e.g. `"file"`).
    pub name: String,
    /// Optional per-call output path override for a one-shot rebuild.
    /// When `Some(path)`, the projection is rebuilt to this path instead of
    /// the default attached path. Relative paths are resolved against the
    /// session root. Absolute paths are used as-is. The default attached
    /// projection is **not** modified — subsequent `close_chapter` writes
    /// still go to the default attached path. Only meaningful when
    /// `name == "file"`; for other projection names the argument is ignored
    /// and a warning is logged.
    #[serde(default)]
    pub output_path: Option<String>,
}

/// Parameters for `journal_import`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct JournalImportParams {
    /// Filesystem path of the markdown file to import. Relative paths resolve
    /// against the active session root; absolute paths are used as-is.
    pub path: String,
}

/// Parameters for `journal_info` (no fields).
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct JournalInfoParams {}

// ---------------------------------------------------------------------------
// Output structs — MCP wire shapes
// ---------------------------------------------------------------------------

/// A row in the `journal_chapter_list` response table (Microsoft Decision Log format).
#[derive(Debug, Serialize)]
pub struct ChapterListRow {
    /// Chapter date or identifier slug.
    pub chapter_id: String,
    /// Schema used for this chapter.
    pub schema_id: String,
    /// Current state machine state (e.g. `"closed"`, `"open"`).
    pub current_state: String,
    /// Unix epoch milliseconds when the chapter was opened.
    pub opened_at: i64,
    /// Unix epoch milliseconds when the chapter was closed, or `null`.
    pub closed_at: Option<i64>,
    /// First line of the `Decided` section (empty string if absent).
    pub decided_summary: String,
    /// Anchor link for the `Decided` section.
    pub link: String,
}

/// Snapshot of the journal module's runtime state, returned by `journal_info`.
///
/// All path fields are absolute and resolved from the active session.
#[derive(Debug, Serialize, JsonSchema)]
pub struct JournalInfoResult {
    /// Project root path (= active session root, canonicalized at session start).
    pub project_root: PathBuf,
    /// Absolute path to the `.journal.db` file (`<project_root>/workspace/.journal.db`).
    pub db_path: PathBuf,
    /// `true` if `db_path` exists on the filesystem at the time `journal_info` is called.
    pub db_exists: bool,
    /// Absolute path to the WAL companion file (`-wal` suffix).
    pub wal_path: PathBuf,
    /// Absolute path to the shared-memory companion file (`-shm` suffix).
    pub shm_path: PathBuf,
    /// Project-local schema directory (`<project_root>/.journal/schemas`).
    pub schema_registry_path: PathBuf,
    /// All registered schema keys (L1 built-in ∪ L2 project-local, L2 wins, de-duplicated).
    pub available_schemas: Vec<String>,
    /// `lds-journal` crate version (e.g. `"0.3.2"`).
    pub version: String,
    /// `true` when a `FileProjection` is attached (either via env opt-in at
    /// session start or via runtime `journal_projection_attach` call).
    pub file_projection_enabled: bool,
    /// Absolute path of the attached file projection, or `None` when not
    /// attached. Resolved from `LDS_JOURNAL_FILE_OUTPUT_PATH` (if set),
    /// the runtime attach call's `output_path` argument, or
    /// `<project_root>/workspace/journal.md` (default).
    pub file_projection_path: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

/// Convert a [`journal_mcp_core::ChapterReplay`] to a `serde_json::Value`.
///
/// `ChapterReplay` (and `ChapterMeta` / `EventRow`) do not derive `Serialize`
/// in the core crate. This helper provides the MCP-layer projection without
/// polluting the library.
pub fn chapter_replay_to_json(replay: &journal_mcp_core::ChapterReplay) -> serde_json::Value {
    let events: Vec<serde_json::Value> = replay
        .events
        .iter()
        .map(|e| {
            serde_json::json!({
                "event_id": e.event_id.0,
                "event_type": e.event_type,
                "section_name": e.section_name,
                "payload": e.payload,
                "created_at": e.created_at,
            })
        })
        .collect();
    serde_json::json!({
        "chapter_id": replay.meta.chapter_id.0,
        "schema_id": replay.meta.schema_id,
        "current_state": replay.meta.current_state,
        "opened_at": replay.meta.opened_at,
        "closed_at": replay.meta.closed_at,
        "events": events,
    })
}

/// Paginate a `Vec<T>` by `offset` and `limit`. `offset = None` ⇒ 0,
/// `limit = None` ⇒ `usize::MAX` (= return all remaining after offset).
/// `offset >= len` yields an empty `Vec` (not an error).
pub fn paginate<T>(items: Vec<T>, offset: Option<usize>, limit: Option<usize>) -> Vec<T> {
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(usize::MAX);
    items.into_iter().skip(offset).take(limit).collect()
}
