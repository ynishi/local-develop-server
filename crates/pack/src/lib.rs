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
//!   a `workspace/` tree, journal databases, sandbox snapshots, agent and
//!   editor dotfiles.
//!
//! Three things are treated specially:
//!
//! | | treatment |
//! |---|---|
//! | secrets (`.env`, `*.pem`, …) | **not packed**, reported instead — moving credentials is the operator's own business |
//! | caches (`target/`, `node_modules/`, …) | **not packed**, recorded — regenerable by definition |
//! | symlinks | packed as links, never followed; reported so a restore elsewhere can name what dangles |
//!
//! A symlink is reported because it is a problem: it breaks when the project
//! lands somewhere else. The exception is a directory that is *meant* to be
//! links — a shared dotfile tree, say — where the operator already knows and
//! the entries are noise hiding the links that do need attention. Name such a
//! path in `[pack] no_link_report` and its links are packed without being
//! reported. There is no default and no directory name this crate treats as
//! special.
//!
//! Every rule that fired is named in the manifest, including that one. The
//! reports exist to be acted on and scripted against, so a rule that quietly
//! changed what a report contains would send the reader after the wrong thing;
//! `no_link_report_applied` keeps "no links here" and "links hidden here"
//! distinguishable, and `kept_over_secret` names any file a `keep` glob carried
//! past the secret list.
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
    CacheRecord, KeptOverSecret, MANIFEST_NAME, Manifest, PACK_FORMAT_VERSION, SkipRecord, Stats,
    SymlinkRecord, WorktreeOrigin, WorktreeRecord,
};
pub use restore::{RestoreOptions, RestoreReport, restore};
pub use rules::{DEFAULT_CACHE_DIRS, DEFAULT_KEEP, DEFAULT_SECRET_GLOBS, PackRules, RuleOverrides};
pub use scan::{Scan, scan, scan_with};
