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
///
/// Version 2 dropped the `[claude]` section, which described one hard-coded
/// directory. Readers at version 1 declared `claude` as a required field, so
/// they cannot decode a version 2 manifest; the bump makes them say so instead
/// of failing on a missing key. Version 1 archives are still readable here —
/// their `[claude]` section is ignored, since nothing on it has a counterpart
/// in this shape.
pub const PACK_FORMAT_VERSION: u32 = 2;

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
    /// `no_link_report` globs that actually suppressed at least one link here.
    ///
    /// Without this a reader cannot tell a project with no symlinks from one
    /// whose links were deliberately left out of the report, and would act on
    /// the wrong assumption. A rule that matched nothing is absent, so this
    /// names the processing that happened rather than the configuration that
    /// existed.
    #[serde(default)]
    pub no_link_report_applied: Vec<String>,
    /// Files a `keep` rule carried past a secret rule.
    #[serde(default)]
    pub kept_over_secret: Vec<KeptOverSecret>,
    /// Cache directories that were skipped because they can be regenerated,
    /// each with the size of what it dropped.
    #[serde(default)]
    pub skipped_cache: Vec<CacheRecord>,
    /// Secret-looking files that were skipped and reported instead of packed.
    #[serde(default)]
    pub skipped_secret: Vec<SkipRecord>,
    /// OS debris (`.DS_Store`, `Thumbs.db`) that was dropped.
    ///
    /// Nothing here needs acting on, which is why it used to be dropped in
    /// silence. It is listed because "dropped without a record" is not a
    /// property worth having anywhere in this format: a reader comparing the
    /// source tree against the payload should never find a file that the
    /// manifest cannot account for.
    #[serde(default)]
    pub skipped_noise: Vec<SkipRecord>,
    /// Symlinks recorded verbatim, one entry each — the complete list, so it
    /// can be redirected to a file and processed.
    ///
    /// Excludes only links covered by a `no_link_report` glob, and every such
    /// glob is named in [`Self::no_link_report_applied`].
    #[serde(default)]
    pub symlinks: Vec<SymlinkRecord>,
    /// Registered git worktrees whose pointer files need rewriting on restore.
    #[serde(default)]
    pub worktrees: Vec<WorktreeRecord>,
    /// Set when the packed root is *itself* a worktree of a repository that
    /// lives elsewhere.
    ///
    /// Such a root has no `.git/worktrees/` of its own — its `.git` is a file
    /// naming the parent's admin directory — so without this the pack carries
    /// no trace of the other half of the wiring. Absent in packs written before
    /// this field existed, which read back as `None`.
    #[serde(default)]
    pub worktree_of: Option<WorktreeOrigin>,
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

/// A file that matched a secret rule but was packed anyway, because a `keep`
/// rule outranked it.
///
/// `keep` is the only subtractive list, and the only way a file the secret
/// rules named can end up inside the archive. That is a legitimate thing to
/// ask for — `.env.example` matches `.env.*` and holds placeholders — but it
/// is also how a real credential gets carried by accident, when a glob written
/// for one file turns out to match another. Recording each one keeps the
/// override reviewable instead of silent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeptOverSecret {
    /// Path relative to the project root.
    pub path: String,
    /// The `keep` glob that rescued it.
    pub keep_pattern: String,
    /// The secret glob that would otherwise have excluded it.
    pub secret_pattern: String,
}

/// A path that was left out of the pack, with the reason why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkipRecord {
    /// Path relative to the project root.
    pub path: String,
    /// Why it was skipped (matched pattern or rule name).
    pub reason: String,
}

/// A cache directory that was dropped, and the size of what went with it.
///
/// One record stands in for a whole subtree, which is the right granularity
/// for something regenerable — nobody acts on the individual files inside
/// `target/`. The size is what makes the record checkable: a `cache_dirs` entry
/// aimed at the wrong directory drops hand-written source, and `dist → 412
/// files, 2.1 MB` reads nothing like `target → 38104 files, 4.2 GB`. Without
/// it, a misconfiguration is one indistinguishable line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheRecord {
    /// Path relative to the project root.
    pub path: String,
    /// Which rule named it a cache.
    pub reason: String,
    /// Regular files below it that were not packed.
    pub file_count: u64,
    /// Sum of those files' sizes in bytes.
    pub total_bytes: u64,
    /// Credential-looking files found inside it, attributed to the rule that
    /// swallowed them.
    ///
    /// A cache is pruned before the classification pass, so these were
    /// previously neither packed nor reported: safe, but the operator never
    /// learned that `node_modules/.npmrc` was holding a token. They are still
    /// dropped with the rest of the cache — this only says they were there.
    #[serde(default)]
    pub secrets: Vec<SkipRecord>,
}

/// A symlink recorded individually.
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

/// The repository a packed worktree checkout belongs to.
///
/// This is the mirror image of [`WorktreeRecord`]: that one is written by the
/// repository about its worktrees, this one by a worktree about its repository.
/// Both halves of the wiring are absolute, so both packs need to know about the
/// other end to be restorable somewhere new.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeOrigin {
    /// This worktree's name under the parent's `.git/worktrees/`.
    pub name: String,
    /// Original absolute path of the parent's admin directory
    /// (`<parent_root>/.git/worktrees/<name>`), as recorded at pack time.
    pub admin_path: String,
    /// Original absolute path of the parent repository root.
    pub parent_root: String,
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
            no_link_report_applied: vec![".zsh/**".to_string()],
            kept_over_secret: vec![KeptOverSecret {
                path: ".env.example".to_string(),
                keep_pattern: ".env.example".to_string(),
                secret_pattern: ".env.*".to_string(),
            }],
            skipped_cache: vec![CacheRecord {
                path: "target".to_string(),
                reason: "cache directory: target".to_string(),
                file_count: 38104,
                total_bytes: 4_200_000_000,
                secrets: vec![SkipRecord {
                    path: "target/tmp/.npmrc".to_string(),
                    reason: "secret pattern: .npmrc".to_string(),
                }],
            }],
            skipped_secret: vec![SkipRecord {
                path: ".env".to_string(),
                reason: "secret pattern: .env".to_string(),
            }],
            skipped_noise: vec![SkipRecord {
                path: "sub/.DS_Store".to_string(),
                reason: "os debris: .DS_Store".to_string(),
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
            worktree_of: None,
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
        assert_eq!(parsed.no_link_report_applied, vec![".zsh/**".to_string()]);
        assert_eq!(parsed.kept_over_secret[0].path, ".env.example");
        assert_eq!(parsed.kept_over_secret[0].secret_pattern, ".env.*");
        assert_eq!(parsed.skipped_cache[0].file_count, 38104);
        assert_eq!(parsed.skipped_cache[0].total_bytes, 4_200_000_000);
        assert_eq!(parsed.skipped_cache[0].secrets[0].path, "target/tmp/.npmrc");
        assert_eq!(parsed.skipped_noise[0].path, "sub/.DS_Store");
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
"#;
        let parsed = Manifest::from_toml(text).expect("minimal manifest should parse");
        assert!(parsed.skipped_cache.is_empty());
        assert!(parsed.skipped_noise.is_empty());
        assert!(parsed.skipped_secret.is_empty());
        assert!(parsed.symlinks.is_empty());
        assert!(parsed.worktrees.is_empty());
        assert!(
            parsed.no_link_report_applied.is_empty(),
            "an operator who suppressed nothing gets no suppression record"
        );
        assert!(parsed.kept_over_secret.is_empty());
    }

    /// A version 1 manifest still decodes here, `[claude]` section and all.
    ///
    /// That section described one hard-coded directory and has no successor, so
    /// it is ignored rather than translated — but it must not make the archive
    /// unreadable, since packs written by 0.14.0 are still out there.
    #[test]
    fn test_manifest_reads_legacy_v1_claude_section() {
        let text = r#"
format_version = 1
created_at = "2026-08-10T00:00:00Z"
source_root = "/tmp/proj"
project_name = "proj"
lds_version = "0.14.0"

[stats]
file_count = 0
symlink_count = 0
total_bytes = 0

[claude]
present = true
symlink_count = 154
link_roots = ["/mnt/links/shared"]
"#;
        let parsed = Manifest::from_toml(text).expect("a v1 manifest must still parse");

        assert_eq!(parsed.format_version, 1);
        assert!(
            parsed.no_link_report_applied.is_empty(),
            "the legacy section is dropped, not translated into a suppression"
        );
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
