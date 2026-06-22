//! Journal module for local-develop-server (lds).
//!
//! Wraps `journal-mcp-core::JournalCore` as a session-scoped lds module.
//! The EventLog database path is derived from [`lds_core::Session::root`]
//! as `<root>/workspace/.journal.db` — literal join, no env override / cwd
//! fallback (consistent with other lds modules: session root is the single
//! source of truth).
//!
//! See journal-mcp v0.4.0 `docs/design.md` for the underlying chapter /
//! section / progress / projection state-machine semantics.

pub mod module;
pub mod tools;

pub use module::JournalModule;

/// Re-export the underlying SDK crate so downstream consumers
/// (e.g. `local-develop-server` main.rs) can reference `ChapterId`,
/// `SchemaRegistry`, `FileProjection`, and the `JournalProjection` trait
/// without taking a direct path dep.
pub use journal_mcp_core;
pub use tools::{
    ChapterListRow, JournalAppendProgressParams, JournalAppendSectionParams,
    JournalChapterListParams, JournalCloseChapterParams, JournalGrepParams, JournalImportParams,
    JournalInfoParams, JournalInfoResult, JournalOpenChapterParams, JournalOpenChaptersParams,
    JournalProgressOfParams, JournalProjectionAttachParams, JournalProjectionDetachParams,
    JournalProjectionRebuildParams, JournalSchemaListParams, JournalSchemaLoadParams,
    JournalSchemaShowParams, JournalTailParams, chapter_replay_to_json, paginate,
};
