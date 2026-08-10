//! Pack a whole project into one portable archive, and put it back.
//!
//! An `lds` pack is not a source tarball and not a `git bundle`. It carries the
//! project as it actually exists on the machine:
//!
//! - the **`.git` directory itself**, copied wholesale — so stashes, the
//!   reflog, local-only branches, and objects no ref points at all come along.
//!   A bundle is assembled from an enumerated set of refs, and everything
//!   outside that set is quietly left behind; copying the directory means there
//!   is no set to enumerate and nothing to forget.
//! - the **untracked local state** that ordinarily never leaves the machine:
//!   a `workspace/` tree, journal databases, sandbox snapshots, `.mcp.json`,
//!   `.claude/`.
//!
//! Three things are treated specially:
//!
//! | | treatment |
//! |---|---|
//! | secrets (`.env`, `*.pem`, …) | **not packed**, reported instead — moving credentials is the operator's own business |
//! | caches (`target/`, `node_modules/`, …) | **not packed**, recorded — regenerable by definition |
//! | symlinks | packed as links, never followed; reported so a restore elsewhere can name what dangles |
//!
//! `.claude/` is its own layer: packed verbatim, with its links counted and
//! their shared roots summarized rather than enumerated one by one.
//!
//! # Example
//!
//! ```no_run
//! use lds_pack::{CreateOptions, RestoreOptions, create, restore};
//!
//! let report = create(&CreateOptions::new("/path/to/proj", "proj.pack", "0.13.3"))?;
//! println!("packed {} files", report.manifest.stats.file_count);
//!
//! let restored = restore(&RestoreOptions::new("proj.pack", "/tmp/proj"))?;
//! if restored.needs_attention() {
//!     println!("{} symlinks dangle here", restored.dangling_symlinks.len());
//! }
//! # Ok::<(), lds_pack::PackError>(())
//! ```

pub mod create;
pub mod error;
pub mod inspect;
pub mod manifest;
pub mod restore;
pub mod rules;
pub mod scan;

pub use create::{CreateOptions, CreateReport, DEFAULT_COMPRESSION_LEVEL, create};
pub use error::PackError;
pub use inspect::{inspect, list_payload_paths, verify};
pub use manifest::{
    ClaudeInfo, MANIFEST_NAME, Manifest, PACK_FORMAT_VERSION, SkipRecord, Stats, SymlinkRecord,
    WorktreeRecord,
};
pub use restore::{RestoreOptions, RestoreReport, restore};
pub use rules::{DEFAULT_CACHE_DIRS, DEFAULT_KEEP, DEFAULT_SECRET_GLOBS, PackRules, RuleOverrides};
pub use scan::{Scan, scan, scan_with};
