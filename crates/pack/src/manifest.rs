//! `pack.toml` — the manifest carried at the head of every `.pack` archive.
//!
//! The manifest is what makes a pack *inspectable without unpacking*: it is
//! written as the first entry of the tar stream, so [`crate::inspect`] can stop
//! reading as soon as it has been decoded.
//!
//! It records three classes of information:
//!
//! 1. **provenance** — where the pack came from and when.
//! 2. **what was deliberately left out** — cache directories (regenerable) and
//!    secrets (never packed; carrying those is the operator's own business).
//! 3. **what needs attention on restore** — symlinks that point outside the
//!    project, and worktrees whose absolute `gitdir` pointers were rewritten.

use serde::{Deserialize, Serialize};

/// File name of the manifest inside the archive.
pub const MANIFEST_NAME: &str = "pack.toml";

/// Current pack format version.
///
/// Bumped when the on-disk layout changes in a way older readers cannot
/// interpret. [`crate::restore`] refuses archives newer than this.
pub const PACK_FORMAT_VERSION: u32 = 1;

/// Top-level manifest written to `pack.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Pack format version (see [`PACK_FORMAT_VERSION`]).
    pub format_version: u32,
    /// RFC 3339 timestamp of when the pack was created.
    pub created_at: String,
    /// Absolute path of the project root the pack was created from.
    ///
    /// Restoring into a different path is expected and supported; this field
    /// exists so worktree pointer rewriting can compute the delta, and so a
    /// human reading the manifest can tell where the pack came from.
    pub source_root: String,
    /// Directory name of the source root (used as the default restore name).
    pub project_name: String,
    /// Version of the `lds` binary that produced the pack.
    pub lds_version: String,
    /// Aggregate counts for the payload.
    pub stats: Stats,
    /// The `.claude/` directory, tracked separately from ordinary content.
    pub claude: ClaudeInfo,
    /// Cache directories that were skipped because they can be regenerated.
    #[serde(default)]
    pub skipped_cache: Vec<SkipRecord>,
    /// Secret-looking files that were skipped and reported instead of packed.
    #[serde(default)]
    pub skipped_secret: Vec<SkipRecord>,
    /// Symlinks found outside `.claude/`, recorded verbatim for the report.
    #[serde(default)]
    pub symlinks: Vec<SymlinkRecord>,
    /// Registered git worktrees whose pointer files need rewriting on restore.
    #[serde(default)]
    pub worktrees: Vec<WorktreeRecord>,
}

/// Aggregate payload counts, filled in as the archive is written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stats {
    /// Number of regular files packed.
    pub file_count: u64,
    /// Number of symlinks packed (stored as links, never dereferenced).
    pub symlink_count: u64,
    /// Sum of regular-file sizes in bytes, before compression.
    pub total_bytes: u64,
}

/// State of the `.claude/` directory.
///
/// `.claude/` is handled as its own layer: it is packed verbatim, symlinks and
/// all, and its links are *not* enumerated individually. In a profile-managed
/// setup it is commonly a tree of a hundred-plus links into a profiles
/// repository, and listing each one would bury the rest of the manifest while
/// telling the reader nothing they can act on per entry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeInfo {
    /// Whether a `.claude/` directory was present at the source root.
    pub present: bool,
    /// How many symlinks live under `.claude/` (aggregate only).
    pub symlink_count: u64,
    /// Distinct roots those symlinks point into, deduplicated.
    ///
    /// Enough for a restore-side reader to see "these links want
    /// `<profiles-root>` to exist" without a per-file list.
    #[serde(default)]
    pub link_roots: Vec<String>,
}

/// A path that was left out of the pack, with the reason why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkipRecord {
    /// Path relative to the project root.
    pub path: String,
    /// Why it was skipped (matched pattern or rule name).
    pub reason: String,
}

/// A symlink encountered outside `.claude/`.
///
/// Symlinks are packed as links, never dereferenced — following them would
/// drag unrelated trees (and whatever they contain) into the archive. The
/// record exists so restore can report what is dangling rather than silently
/// producing a broken tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymlinkRecord {
    /// Path of the link itself, relative to the project root.
    pub path: String,
    /// Raw link target, exactly as stored on disk.
    pub target: String,
    /// Whether the target resolves outside the project root.
    ///
    /// Links that stay inside the root travel fine; only the ones pointing out
    /// of it can dangle after a restore somewhere else.
    pub outside_root: bool,
}

/// A registered git worktree carried in the pack.
///
/// Both halves of a worktree's wiring are absolute paths — `.git/worktrees/<name>/gitdir`
/// points at the worktree's `.git` file, and that `.git` file points back at the
/// admin directory. A plain extract leaves both aimed at the machine the pack
/// came from, so restore rewrites them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRecord {
    /// Worktree name (the directory name under `.git/worktrees/`).
    pub name: String,
    /// Worktree location relative to the project root.
    ///
    /// `None` when the worktree lives outside the root, in which case its
    /// contents are not in the pack and restore only reports it.
    pub path: Option<String>,
    /// Original absolute path of the worktree, as recorded at pack time.
    pub source_path: String,
    /// Whether the worktree's contents were included in the payload.
    pub included: bool,
}

impl Manifest {
    /// Serialize to TOML for embedding as the archive's first entry.
    ///
    /// # Errors
    ///
    /// Returns [`toml::ser::Error`] if serialization fails, which for this
    /// schema means a bug rather than bad input.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Parse a manifest from TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`toml::de::Error`] if the text is not a valid manifest.
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            format_version: PACK_FORMAT_VERSION,
            created_at: "2026-08-10T00:00:00Z".to_string(),
            source_root: "/tmp/proj".to_string(),
            project_name: "proj".to_string(),
            lds_version: "0.13.3".to_string(),
            stats: Stats {
                file_count: 3,
                symlink_count: 1,
                total_bytes: 42,
            },
            claude: ClaudeInfo {
                present: true,
                symlink_count: 154,
                link_roots: vec!["/home/u/.config/profiles".to_string()],
            },
            skipped_cache: vec![SkipRecord {
                path: "target".to_string(),
                reason: "cache directory".to_string(),
            }],
            skipped_secret: vec![SkipRecord {
                path: ".env".to_string(),
                reason: "secret pattern: .env".to_string(),
            }],
            symlinks: vec![SymlinkRecord {
                path: "link".to_string(),
                target: "/elsewhere".to_string(),
                outside_root: true,
            }],
            worktrees: vec![WorktreeRecord {
                name: "wt".to_string(),
                path: Some(".worktrees/wt".to_string()),
                source_path: "/tmp/proj/.worktrees/wt".to_string(),
                included: true,
            }],
        }
    }

    /// A manifest survives a serialize / parse round trip with every field intact.
    #[test]
    fn test_manifest_round_trip() {
        let original = sample();
        let text = original.to_toml().expect("serialize should succeed");
        let parsed = Manifest::from_toml(&text).expect("parse should succeed");

        assert_eq!(parsed.format_version, original.format_version);
        assert_eq!(parsed.source_root, original.source_root);
        assert_eq!(parsed.stats.file_count, 3);
        assert_eq!(parsed.claude.symlink_count, 154);
        assert_eq!(parsed.skipped_secret.len(), 1);
        assert_eq!(parsed.symlinks[0].target, "/elsewhere");
        assert_eq!(parsed.worktrees[0].name, "wt");
    }

    /// Optional list sections may be absent entirely; they default to empty.
    #[test]
    fn test_manifest_parses_without_optional_sections() {
        let text = r#"
format_version = 1
created_at = "2026-08-10T00:00:00Z"
source_root = "/tmp/proj"
project_name = "proj"
lds_version = "0.13.3"

[stats]
file_count = 0
symlink_count = 0
total_bytes = 0

[claude]
present = false
symlink_count = 0
"#;
        let parsed = Manifest::from_toml(text).expect("minimal manifest should parse");
        assert!(parsed.skipped_cache.is_empty());
        assert!(parsed.skipped_secret.is_empty());
        assert!(parsed.symlinks.is_empty());
        assert!(parsed.worktrees.is_empty());
        assert!(parsed.claude.link_roots.is_empty());
    }

    /// A worktree living outside the root carries no relative path.
    #[test]
    fn test_worktree_record_allows_absent_path() {
        let mut m = sample();
        m.worktrees[0].path = None;
        m.worktrees[0].included = false;
        let text = m.to_toml().expect("serialize should succeed");
        let parsed = Manifest::from_toml(&text).expect("parse should succeed");
        assert!(parsed.worktrees[0].path.is_none());
        assert!(!parsed.worktrees[0].included);
    }
}
