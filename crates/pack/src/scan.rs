//! Classification pass: decide, for every path under the project root, whether
//! it travels in the pack, is dropped, or is merely reported.
//!
//! The scan is deliberately independent of `.gitignore`. A pack carries the
//! whole project — tracked files, the `.git` directory itself, and the
//! untracked local state that ordinarily never leaves the machine (a
//! `workspace/` directory, journal databases, sandbox snapshots). Consulting
//! ignore rules would drop exactly the material the pack exists to preserve.
//!
//! Four rules decide what does *not* travel verbatim:
//!
//! | rule | effect |
//! |---|---|
//! | cache directory | not packed, recorded with the file count, size, and any credentials that went with it |
//! | secret file | not packed, reported; moving credentials is the operator's own business |
//! | OS debris (`.DS_Store`) | not packed, recorded; nothing to act on, but nothing leaves untraced either |
//! | symlink | packed as a link, never dereferenced; recorded so restore can report dangles |
//! | `no_link_report` path | packed as links, left out of the link report; the rule that did so is recorded |
//!
//! Only the last is opt-in. A symlink breaks when the project is carried
//! elsewhere, so reporting one is the default and suppressing it takes an
//! explicit declaration from the operator, who alone knows which of their
//! directories are links by design.
//!
//! **Every rule that fired is named in the output.** The reports are read in
//! order to act on them — to move a secret out of band, to repair a link, to
//! regenerate a cache — so a rule that quietly changed what a report contains
//! would send the reader after the wrong thing. Suppressing a link report never
//! suppresses the record that it was suppressed.

use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

use crate::error::PackError;
use crate::manifest::{
    CacheRecord, KeptOverSecret, SkipRecord, SymlinkRecord, WorktreeOrigin, WorktreeRecord,
};
use crate::rules::{FileVerdict, PackRules};

/// OS debris that is neither cache nor content: dropped, but recorded.
const NOISE_FILES: &[&str] = &[".DS_Store", "Thumbs.db"];

/// What a scanned path is, for the purpose of writing the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file, packed by content.
    File,
    /// A directory, packed as an entry so empty directories survive.
    Dir,
    /// A symlink, packed as a link without following it.
    Symlink,
}

/// One path that will be written into the archive.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Path relative to the project root, using `/` separators.
    pub rel: String,
    /// Absolute path on disk.
    pub abs: PathBuf,
    /// What kind of entry this is.
    pub kind: EntryKind,
    /// Size in bytes for regular files, `0` otherwise.
    pub size: u64,
}

/// Result of classifying a project root.
#[derive(Debug, Default)]
pub struct Scan {
    /// Everything that will be written to the archive, in walk order.
    pub entries: Vec<Entry>,
    /// Cache directories that were dropped, with the size of what went too.
    pub skipped_cache: Vec<CacheRecord>,
    /// Secret-looking files that were dropped and reported.
    pub skipped_secret: Vec<SkipRecord>,
    /// OS debris that was dropped — recorded so nothing leaves without a trace.
    pub skipped_noise: Vec<SkipRecord>,
    /// Every symlink found, one record each, except those a `no_link_report`
    /// glob covered.
    pub symlinks: Vec<SymlinkRecord>,
    /// `no_link_report` globs that actually suppressed at least one link, in
    /// declaration order.
    ///
    /// A rule that matched nothing does not appear: this records what the scan
    /// did, not what was configured.
    pub no_link_report_applied: Vec<String>,
    /// Files a `keep` rule carried past a secret rule.
    pub kept_over_secret: Vec<KeptOverSecret>,
    /// Registered git worktrees discovered under `.git/worktrees/`.
    pub worktrees: Vec<WorktreeRecord>,
    /// Set when this root is itself a worktree of a repository elsewhere.
    pub worktree_of: Option<WorktreeOrigin>,
}

impl Scan {
    /// Total byte count of regular files to be packed.
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.size).sum()
    }

    /// Number of regular files to be packed.
    pub fn file_count(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .count() as u64
    }

    /// Number of symlinks to be packed.
    pub fn symlink_count(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.kind == EntryKind::Symlink)
            .count() as u64
    }
}

/// Classify every path under `root` using the default rules.
///
/// Convenience wrapper over [`scan_with`] for callers that do not customize
/// classification.
///
/// # Errors
///
/// Same as [`scan_with`].
pub fn scan(root: &Path) -> Result<Scan, PackError> {
    scan_with(root, &PackRules::default())
}

/// Classify every path under `root`.
///
/// # Arguments
///
/// * `root` — Absolute path to the project root.
/// * `rules` — Which names count as secrets and caches (see [`PackRules`]).
///
/// # Returns
///
/// A [`Scan`] listing what to pack and what was deliberately left out.
///
/// # Errors
///
/// - [`PackError::NotADirectory`] if `root` is not a directory.
/// - [`PackError::Io`] if the tree cannot be walked or a link cannot be read.
pub fn scan_with(root: &Path, rules: &PackRules) -> Result<Scan, PackError> {
    if !root.is_dir() {
        return Err(PackError::NotADirectory(root.to_path_buf()));
    }
    // Resolve the root up front so paths recorded by git (which may name the
    // same directory through a different symlinked prefix) can be compared
    // against it. Without this, a worktree inside the project reads as being
    // outside it and its contents silently stop travelling.
    let root = &canonicalize_or(root);

    let mut scan = Scan::default();

    let walker = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter();

    // `filter_entry` prunes whole subtrees, so a skipped cache directory costs
    // one stat rather than a full descent into it.
    let it = walker.filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        // Never descend into a symlinked directory: it is packed as a link.
        if e.file_type().is_symlink() {
            return true;
        }
        if !e.file_type().is_dir() {
            return true;
        }
        // A path-scoped rule needs the path; one that cannot be made relative
        // is outside the root and no rule can be about it.
        let Some(rel) = rel_path(root, e.path()) else {
            return true;
        };
        !rules.is_cache_dir(name.as_ref(), &rel)
    });

    // Cache directories are pruned above, which also hides them from the
    // record; walk their parents separately so each dropped cache is named.
    collect_cache_records(root, rules, &mut scan)?;

    for next in it {
        let entry = next?;
        let abs = entry.path().to_path_buf();
        let Some(rel) = rel_path(root, &abs) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();

        if NOISE_FILES.contains(&name.as_str()) {
            scan.skipped_noise.push(SkipRecord {
                path: rel,
                reason: format!("os debris: {name}"),
            });
            continue;
        }

        let file_type = entry.file_type();

        if file_type.is_symlink() {
            let target = std::fs::read_link(&abs)?;
            // A link the operator declared expected is packed like any other,
            // just not reported. The rule that made that call is recorded, so
            // "no links here" and "links hidden here" stay distinguishable.
            match rules.no_link_report_match(&name, &rel) {
                Some(glob) => {
                    if !scan.no_link_report_applied.iter().any(|g| g == glob) {
                        scan.no_link_report_applied.push(glob.to_string());
                    }
                }
                None => scan.symlinks.push(SymlinkRecord {
                    path: rel.clone(),
                    target: target.to_string_lossy().into_owned(),
                    outside_root: resolves_outside(root, &abs, &target),
                }),
            }
            scan.entries.push(Entry {
                rel,
                abs,
                kind: EntryKind::Symlink,
                size: 0,
            });
            continue;
        }

        if file_type.is_dir() {
            scan.entries.push(Entry {
                rel,
                abs,
                kind: EntryKind::Dir,
                size: 0,
            });
            continue;
        }

        match rules.classify(&name, &rel) {
            FileVerdict::Secret { pattern } => {
                scan.skipped_secret.push(SkipRecord {
                    path: rel,
                    reason: format!("secret pattern: {pattern}"),
                });
                continue;
            }
            // Packed, because the operator asked for it — and recorded, because
            // this is the only way a file the secret rules named gets in.
            FileVerdict::KeptOverSecret {
                keep_pattern,
                secret_pattern,
            } => scan.kept_over_secret.push(KeptOverSecret {
                path: rel.clone(),
                keep_pattern,
                secret_pattern,
            }),
            FileVerdict::Ordinary => {}
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        scan.entries.push(Entry {
            rel,
            abs,
            kind: EntryKind::File,
            size,
        });
    }

    scan.worktrees = discover_worktrees(root)?;
    scan.worktree_of = discover_worktree_origin(root);

    Ok(scan)
}

/// Walk the tree a second time, shallowly, to name every pruned cache directory.
///
/// `filter_entry` removes cache directories before they are yielded, so they
/// would otherwise vanish without a record. This pass descends normally but
/// stops at each cache directory it names, so the cost stays proportional to
/// the surviving tree.
fn collect_cache_records(root: &Path, rules: &PackRules, scan: &mut Scan) -> Result<(), PackError> {
    let walker = WalkDir::new(root)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name()
        .into_iter();

    let mut it = walker.filter_entry(|e| {
        if e.file_type().is_symlink() {
            return false;
        }
        if !e.file_type().is_dir() {
            return false;
        }
        true
    });

    while let Some(next) = it.next() {
        let entry = next?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rel) = rel_path(root, entry.path()) else {
            continue;
        };
        if !rules.is_cache_dir(&name, &rel) {
            continue;
        }
        let (file_count, total_bytes, secrets) = measure_cache(entry.path(), root, rules);
        scan.skipped_cache.push(CacheRecord {
            path: rel,
            reason: format!("cache directory: {name}"),
            file_count,
            total_bytes,
            secrets,
        });
        it.skip_current_dir();
    }

    Ok(())
}

/// Measure a cache directory that is about to be dropped: how many files, how
/// many bytes, and which of them look like credentials.
///
/// One record stands in for the whole subtree, so this is what makes that
/// record checkable: a `cache_dirs` entry aimed at hand-written source shows up
/// as a small file count next to `target/`'s enormous one, where the path alone
/// would read the same either way. Walking a pruned tree costs a stat per file
/// and no reads, which is cheap next to compressing everything else.
///
/// The secret scan rides along on that same walk. Pruning happens before the
/// classification pass, so a `node_modules/.npmrc` was previously neither
/// packed nor reported — safe, but the operator never learned that a token was
/// sitting there. Nothing here changes what travels; these files are dropped
/// with the rest of the cache either way.
///
/// Unreadable entries contribute nothing rather than aborting the pack: these
/// figures exist to be eyeballed, and a cache directory is regenerable, so
/// failing a whole pack over a permission error inside one would trade a real
/// capability for a rounding error.
fn measure_cache(dir: &Path, root: &Path, rules: &PackRules) -> (u64, u64, Vec<SkipRecord>) {
    let mut file_count = 0;
    let mut total_bytes = 0;
    let mut secrets = Vec::new();

    for entry in WalkDir::new(dir)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        if let Ok(meta) = entry.metadata() {
            file_count += 1;
            total_bytes += meta.len();
        }

        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rel) = rel_path(root, entry.path()) else {
            continue;
        };
        // Only an unrescued secret is worth naming. A `keep` rule matching in
        // here means the operator already called the file safe to carry, and it
        // is being dropped with the cache regardless.
        if let FileVerdict::Secret { pattern } = rules.classify(&name, &rel) {
            secrets.push(SkipRecord {
                path: rel,
                reason: format!("secret pattern: {pattern}"),
            });
        }
    }

    (file_count, total_bytes, secrets)
}

/// Convert an absolute path to a `/`-separated path relative to `root`.
fn rel_path(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let s = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if s.is_empty() { None } else { Some(s) }
}

/// Whether a link target escapes the project root.
///
/// Relative targets are resolved against the link's own directory. The result
/// is resolved against the filesystem where possible, so a link written through
/// one symlinked prefix is not mistaken for pointing outside a root named
/// through another. A dangling target cannot be resolved and falls back to
/// lexical normalization, which is enough to classify it.
fn resolves_outside(root: &Path, link_path: &Path, target: &Path) -> bool {
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        match link_path.parent() {
            Some(parent) => parent.join(target),
            None => return true,
        }
    };
    !canonicalize_or(&joined).starts_with(canonicalize_or(root))
}

/// Resolve a path against the filesystem, falling back to lexical
/// normalization when it does not exist.
pub(crate) fn canonicalize_or(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

/// Lexically normalize a path, collapsing `.` and `..` without touching disk.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Read `.git/worktrees/` to learn which worktrees this repository has.
///
/// Each admin directory holds a `gitdir` file whose contents are the absolute
/// path of the worktree's own `.git` file; the worktree root is that file's
/// parent. Worktrees inside the project root travel with the pack, and their
/// pointers are rewritten on restore. Worktrees outside it are recorded but
/// their contents are not collected — reaching outside the root to pull in an
/// arbitrary directory is a different decision than packing a project.
fn discover_worktrees(root: &Path) -> Result<Vec<WorktreeRecord>, PackError> {
    let admin = root.join(".git").join("worktrees");
    if !admin.is_dir() {
        return Ok(Vec::new());
    }

    let mut records = Vec::new();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&admin)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let Some(name) = dir.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let gitdir_file = dir.join("gitdir");
        let Ok(contents) = std::fs::read_to_string(&gitdir_file) else {
            continue;
        };
        // `gitdir` holds the path of the worktree's `.git` file; its parent is
        // the worktree root.
        let dot_git = PathBuf::from(contents.trim());
        let Some(worktree_root) = dot_git.parent() else {
            continue;
        };
        // git records this path as it saw it, which need not match the resolved
        // root; resolve both sides before deciding whether it lives inside.
        let resolved = canonicalize_or(worktree_root);
        let rel = rel_path(root, &resolved);
        records.push(WorktreeRecord {
            name,
            included: rel.is_some(),
            path: rel,
            // Resolved rather than verbatim, so it can be compared against
            // `source_root` — which is canonical — when restore works out where
            // an outside worktree moved to.
            source_path: resolved.to_string_lossy().into_owned(),
        });
    }

    Ok(records)
}

/// Work out whether this root is itself a worktree, and of what.
///
/// A worktree checkout has no `.git` *directory*: its `.git` is a file holding
/// `gitdir: <parent>/.git/worktrees/<name>`. That single line is the only trace
/// of the parent in the checkout, and it is an absolute path — so it is
/// recorded here for restore to rebuild rather than lost with the machine.
///
/// Only the layout git itself writes is accepted. Anything else (a `commondir`
/// pointing somewhere unusual, a hand-made `.git` file) yields `None`: guessing
/// a parent from an unrecognized shape would be worse than reporting nothing.
fn discover_worktree_origin(root: &Path) -> Option<WorktreeOrigin> {
    let dot_git = root.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let contents = std::fs::read_to_string(&dot_git).ok()?;
    let admin = PathBuf::from(contents.trim().strip_prefix("gitdir:")?.trim());

    // `<parent_root>/.git/worktrees/<name>` — anything else is not ours to read.
    let name = admin.file_name()?.to_string_lossy().into_owned();
    let worktrees_dir = admin.parent()?;
    if worktrees_dir.file_name()? != "worktrees" {
        return None;
    }
    let git_dir = worktrees_dir.parent()?;
    if git_dir.file_name()? != ".git" {
        return None;
    }
    let parent_root = canonicalize_or(git_dir.parent()?);

    Some(WorktreeOrigin {
        name,
        admin_path: admin.to_string_lossy().into_owned(),
        // Canonical, to be comparable with `source_root` when restore works out
        // where the parent repository moved to.
        parent_root: parent_root.to_string_lossy().into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir should succeed in test");
        }
        fs::write(path, b"x").expect("write should succeed in test");
    }

    fn rels(scan: &Scan) -> Vec<String> {
        scan.entries.iter().map(|e| e.rel.clone()).collect()
    }

    // ------------------------------------------------------------------
    // scan behaviour
    // ------------------------------------------------------------------

    /// Untracked local state travels; `.git` travels; caches and secrets do not.
    #[test]
    fn test_scan_partitions_tree() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();

        touch(&root.join("src/main.rs"));
        touch(&root.join(".git/HEAD"));
        touch(&root.join("workspace/journal.md"));
        touch(&root.join("workspace/.journal.db"));
        touch(&root.join(".mcp.json"));
        touch(&root.join("target/debug/binary"));
        touch(&root.join("crates/inner/target/x.rlib"));
        touch(&root.join(".env"));
        touch(&root.join(".env.example"));
        touch(&root.join("key.pem"));

        let scan = scan(root).expect("scan should succeed");
        let packed = rels(&scan);

        assert!(packed.contains(&"src/main.rs".to_string()));
        assert!(
            packed.contains(&".git/HEAD".to_string()),
            "`.git` must travel"
        );
        assert!(packed.contains(&"workspace/journal.md".to_string()));
        assert!(
            packed.contains(&"workspace/.journal.db".to_string()),
            "journal database is exactly the local state a pack exists to carry"
        );
        assert!(packed.contains(&".mcp.json".to_string()));
        assert!(packed.contains(&".env.example".to_string()));

        assert!(
            !packed.iter().any(|p| p.starts_with("target/")),
            "cache tree must not be packed"
        );
        assert!(
            !packed.iter().any(|p| p.contains("/target/")),
            "nested cache tree must not be packed"
        );
        assert!(!packed.contains(&".env".to_string()));
        assert!(!packed.contains(&"key.pem".to_string()));

        let secrets: Vec<&str> = scan
            .skipped_secret
            .iter()
            .map(|s| s.path.as_str())
            .collect();
        assert!(secrets.contains(&".env"));
        assert!(secrets.contains(&"key.pem"));

        let caches: Vec<&str> = scan.skipped_cache.iter().map(|s| s.path.as_str()).collect();
        assert!(caches.contains(&"target"));
        assert!(caches.contains(&"crates/inner/target"));
    }

    /// A dropped cache carries the size of what it took with it, so a rule
    /// that caught the wrong directory is visible rather than one bare line.
    #[test]
    fn test_cache_record_measures_what_it_dropped() {
        use crate::rules::RuleOverrides;
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();

        fs::create_dir_all(root.join("dist/nested")).expect("mkdir");
        fs::write(root.join("dist/a.js"), "0123456789").expect("write");
        fs::write(root.join("dist/nested/b.js"), "01234").expect("write");

        let rules = PackRules::new(&RuleOverrides {
            cache_dirs: vec!["dist".to_string()],
            ..RuleOverrides::default()
        })
        .expect("compile");
        let scan = scan_with(root, &rules).expect("scan should succeed");

        assert_eq!(scan.skipped_cache.len(), 1);
        let dropped = &scan.skipped_cache[0];
        assert_eq!(dropped.path, "dist");
        assert_eq!(
            dropped.file_count, 2,
            "counts the whole subtree, not depth 1"
        );
        assert_eq!(dropped.total_bytes, 15);
        assert!(dropped.secrets.is_empty());
    }

    /// A credential inside a dropped cache is named. The cache is pruned before
    /// the secret pass, so without this it is neither packed nor reported and
    /// the operator never learns the token is sitting on their disk.
    #[test]
    fn test_secrets_inside_a_dropped_cache_are_named() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();

        fs::create_dir_all(root.join("node_modules/pkg")).expect("mkdir");
        touch(&root.join("node_modules/.npmrc"));
        touch(&root.join("node_modules/pkg/index.js"));
        touch(&root.join("node_modules/.env.example"));

        let scan = scan(root).expect("scan should succeed");

        assert_eq!(scan.skipped_cache.len(), 1);
        let dropped = &scan.skipped_cache[0];
        assert_eq!(dropped.path, "node_modules");

        let named: Vec<&str> = dropped.secrets.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(
            named,
            vec!["node_modules/.npmrc"],
            "a file `keep` already calls safe is not re-flagged as a credential"
        );
        assert!(
            dropped.secrets[0].reason.contains(".npmrc"),
            "the rule that flagged it has to be visible, got {:?}",
            dropped.secrets[0].reason
        );

        // Naming them changes nothing about what travels.
        assert!(rels(&scan).iter().all(|p| !p.starts_with("node_modules/")));
        assert!(scan.skipped_secret.is_empty());
    }

    /// End to end: a path-scoped `keep` rescues its own directory and leaves a
    /// namesake elsewhere excluded, with the override recorded either way.
    #[test]
    fn test_path_scoped_keep_end_to_end() {
        use crate::rules::RuleOverrides;
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();

        fs::create_dir_all(root.join("docs/samples")).expect("mkdir");
        fs::create_dir_all(root.join("deploy")).expect("mkdir");
        touch(&root.join("docs/samples/demo.pem"));
        touch(&root.join("deploy/server.pem"));

        let rules = PackRules::new(&RuleOverrides {
            keep: vec!["docs/samples/*.pem".to_string()],
            ..RuleOverrides::default()
        })
        .expect("compile");
        let scan = scan_with(root, &rules).expect("scan should succeed");

        let packed = rels(&scan);
        assert!(packed.contains(&"docs/samples/demo.pem".to_string()));
        assert!(
            !packed.contains(&"deploy/server.pem".to_string()),
            "the real key must stay out"
        );

        assert_eq!(scan.kept_over_secret.len(), 1);
        assert_eq!(scan.kept_over_secret[0].path, "docs/samples/demo.pem");
        assert_eq!(scan.skipped_secret.len(), 1);
        assert_eq!(scan.skipped_secret[0].path, "deploy/server.pem");
    }

    /// OS debris is dropped, but never without a record: a path in the source
    /// tree and not in the payload must always be explainable.
    #[test]
    fn test_os_debris_is_dropped_but_recorded() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();

        fs::create_dir_all(root.join("sub")).expect("mkdir");
        touch(&root.join(".DS_Store"));
        touch(&root.join("sub/.DS_Store"));
        touch(&root.join("keep.txt"));

        let scan = scan(root).expect("scan should succeed");

        let packed = rels(&scan);
        assert!(packed.contains(&"keep.txt".to_string()));
        assert!(
            !packed.iter().any(|p| p.ends_with(".DS_Store")),
            "debris must not be packed"
        );

        let noise: Vec<&str> = scan.skipped_noise.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(noise, vec![".DS_Store", "sub/.DS_Store"]);
        assert!(scan.skipped_noise[0].reason.contains(".DS_Store"));
    }

    /// Symlinks are recorded individually and packed as links.
    #[cfg(unix)]
    #[test]
    fn test_scan_records_symlinks_individually() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let outside = TempDir::new().expect("tempdir");

        touch(&root.join("real.txt"));
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("inside-link"))
            .expect("symlink");
        std::os::unix::fs::symlink(outside.path().join("far.txt"), root.join("outside-link"))
            .expect("symlink");

        let scan = scan(root).expect("scan should succeed");

        assert_eq!(scan.symlinks.len(), 2);
        let inside = scan
            .symlinks
            .iter()
            .find(|s| s.path == "inside-link")
            .expect("inside link recorded");
        let outside_rec = scan
            .symlinks
            .iter()
            .find(|s| s.path == "outside-link")
            .expect("outside link recorded");
        assert!(!inside.outside_root);
        assert!(outside_rec.outside_root);

        assert!(rels(&scan).contains(&"outside-link".to_string()));
    }

    /// Build a project whose `links/` directory is links by design — the shape
    /// an operator declares `no_link_report` for.
    #[cfg(unix)]
    fn link_farm(root: &Path, shared: &Path) -> (String, String) {
        let first = shared.join("group/alpha");
        let second = shared.join("group/beta");
        fs::create_dir_all(&first).expect("mkdir");
        fs::create_dir_all(&second).expect("mkdir");
        touch(&first.join("a.md"));
        touch(&second.join("b.md"));

        fs::create_dir_all(root.join("links")).expect("mkdir");
        std::os::unix::fs::symlink(first.join("a.md"), root.join("links/a.md")).expect("symlink");
        std::os::unix::fs::symlink(second.join("b.md"), root.join("links/b.md")).expect("symlink");

        ("links/a.md".to_string(), "links/b.md".to_string())
    }

    fn rules_with_no_link_report(globs: &[&str]) -> PackRules {
        use crate::rules::RuleOverrides;
        PackRules::new(&RuleOverrides {
            no_link_report: globs.iter().map(|g| (*g).to_string()).collect(),
            ..RuleOverrides::default()
        })
        .expect("globs should compile")
    }

    /// A declared path drops out of the link report while its links still
    /// travel, and the rule that did it is named.
    #[cfg(unix)]
    #[test]
    fn test_declared_path_leaves_the_link_report() {
        let dir = TempDir::new().expect("tempdir");
        let shared = TempDir::new().expect("tempdir");
        let (linked_a, linked_b) = link_farm(dir.path(), shared.path());

        let scan = scan_with(dir.path(), &rules_with_no_link_report(&["links/**"]))
            .expect("scan should succeed");

        assert!(
            scan.symlinks.is_empty(),
            "a declared link must not be reported, got {:?}",
            scan.symlinks
        );
        assert_eq!(
            scan.no_link_report_applied,
            vec!["links/**".to_string()],
            "the rule that suppressed the report has to be visible"
        );
        // Suppression changes the reporting, never what travels.
        assert!(rels(&scan).contains(&linked_a));
        assert!(rels(&scan).contains(&linked_b));
    }

    /// With nothing declared, every link is reported — this crate has no
    /// directory it treats as expected on its own.
    #[cfg(unix)]
    #[test]
    fn test_every_link_is_reported_by_default() {
        let dir = TempDir::new().expect("tempdir");
        let shared = TempDir::new().expect("tempdir");
        link_farm(dir.path(), shared.path());

        let scan = scan(dir.path()).expect("scan should succeed");

        assert!(scan.no_link_report_applied.is_empty());
        assert_eq!(
            scan.symlinks.len(),
            2,
            "undeclared links are reported one by one"
        );
    }

    /// A rule that matched nothing is not recorded as applied: the manifest
    /// says what the scan did, not what the config said.
    #[cfg(unix)]
    #[test]
    fn test_unmatched_rule_is_not_recorded_as_applied() {
        let dir = TempDir::new().expect("tempdir");
        let shared = TempDir::new().expect("tempdir");
        link_farm(dir.path(), shared.path());

        let scan = scan_with(dir.path(), &rules_with_no_link_report(&["vendor/**"]))
            .expect("scan should succeed");

        assert!(scan.no_link_report_applied.is_empty());
        assert_eq!(scan.symlinks.len(), 2);
    }

    /// Overlapping rules record each applied rule once, not once per link.
    #[cfg(unix)]
    #[test]
    fn test_applied_rules_are_deduplicated() {
        let dir = TempDir::new().expect("tempdir");
        let shared = TempDir::new().expect("tempdir");
        link_farm(dir.path(), shared.path());

        let scan = scan_with(
            dir.path(),
            &rules_with_no_link_report(&["links/a.md", "links/**"]),
        )
        .expect("scan should succeed");

        assert!(scan.symlinks.is_empty());
        assert_eq!(
            scan.no_link_report_applied,
            vec!["links/a.md".to_string(), "links/**".to_string()],
            "both rules fired, each recorded once"
        );
    }

    /// A `keep` glob carrying a file past a secret rule is packed *and*
    /// recorded — the only way a secret-matching file enters the archive.
    #[test]
    fn test_keep_over_secret_is_recorded() {
        use crate::rules::RuleOverrides;
        let dir = TempDir::new().expect("tempdir");
        touch(&dir.path().join(".env.example"));
        touch(&dir.path().join(".env"));

        let scan = scan(dir.path()).expect("scan should succeed");

        assert_eq!(scan.kept_over_secret.len(), 1);
        let kept = &scan.kept_over_secret[0];
        assert_eq!(kept.path, ".env.example");
        assert_eq!(kept.keep_pattern, ".env.example");
        assert_eq!(kept.secret_pattern, ".env.*");
        assert!(rels(&scan).contains(&".env.example".to_string()));

        // The unrescued sibling still goes nowhere.
        assert_eq!(scan.skipped_secret.len(), 1);
        assert_eq!(scan.skipped_secret[0].path, ".env");

        // An operator glob that rescues a real credential is recorded the same
        // way, which is the case the record exists for.
        let rules = PackRules::new(&RuleOverrides {
            keep: vec!["*.pem".to_string()],
            ..RuleOverrides::default()
        })
        .expect("glob should compile");
        touch(&dir.path().join("server.pem"));
        let scan = scan_with(dir.path(), &rules).expect("scan should succeed");

        assert!(
            scan.kept_over_secret
                .iter()
                .any(|k| k.path == "server.pem" && k.secret_pattern == "*.pem"),
            "a keep rule outranking a secret rule must never be silent, got {:?}",
            scan.kept_over_secret
        );
    }

    /// A missing `.git/worktrees` yields no worktree records.
    #[test]
    fn test_scan_without_worktrees() {
        let dir = TempDir::new().expect("tempdir");
        touch(&dir.path().join(".git/HEAD"));
        let scan = scan(dir.path()).expect("scan should succeed");
        assert!(scan.worktrees.is_empty());
    }

    /// A worktree inside the root is discovered and marked as included.
    #[test]
    fn test_scan_discovers_inside_worktree() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let wt = root.join(".worktrees/feature");
        touch(&wt.join("file.txt"));
        fs::write(wt.join(".git"), "gitdir: /ignored\n").expect("write");
        let admin = root.join(".git/worktrees/feature");
        fs::create_dir_all(&admin).expect("mkdir");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .expect("write");

        let scan = scan(root).expect("scan should succeed");

        assert_eq!(scan.worktrees.len(), 1);
        let rec = &scan.worktrees[0];
        assert_eq!(rec.name, "feature");
        assert_eq!(rec.path.as_deref(), Some(".worktrees/feature"));
        assert!(rec.included);
        assert!(rels(&scan).contains(&".worktrees/feature/file.txt".to_string()));
    }

    /// A worktree outside the root is reported but its contents are not collected.
    #[test]
    fn test_scan_reports_outside_worktree_without_including_it() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let elsewhere = TempDir::new().expect("tempdir");
        let wt = elsewhere.path().join("detached");
        touch(&wt.join("file.txt"));

        let admin = root.join(".git/worktrees/detached");
        fs::create_dir_all(&admin).expect("mkdir");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .expect("write");

        let scan = scan(root).expect("scan should succeed");

        assert_eq!(scan.worktrees.len(), 1);
        assert!(!scan.worktrees[0].included);
        assert!(scan.worktrees[0].path.is_none());
        assert!(!rels(&scan).iter().any(|p| p.contains("detached/file.txt")));
    }

    /// A worktree checkout knows which repository it belongs to, and says so —
    /// its `.git` file is the only trace, and it names an absolute path that
    /// will not survive the move on its own.
    #[test]
    fn test_scan_records_the_repository_a_worktree_belongs_to() {
        let dir = TempDir::new().expect("tempdir");
        let parent = dir.path().join("proj");
        let admin = parent.join(".git/worktrees/feature");
        fs::create_dir_all(&admin).expect("mkdir");

        let wt = dir.path().join("proj-feature");
        touch(&wt.join("work.txt"));
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).expect("write");

        let scan = scan(&wt).expect("scan should succeed");

        let origin = scan
            .worktree_of
            .expect("a worktree must know its repository");
        assert_eq!(origin.name, "feature");
        assert_eq!(origin.admin_path, admin.display().to_string());
        assert_eq!(
            origin.parent_root,
            fs::canonicalize(&parent)
                .expect("canonicalize")
                .display()
                .to_string()
        );
        // It has no worktrees of its own.
        assert!(scan.worktrees.is_empty());
    }

    /// An ordinary repository is not a worktree of anything.
    #[test]
    fn test_scan_records_no_origin_for_an_ordinary_repository() {
        let dir = TempDir::new().expect("tempdir");
        touch(&dir.path().join(".git/HEAD"));

        let scan = scan(dir.path()).expect("scan should succeed");

        assert!(scan.worktree_of.is_none());
    }

    /// A `.git` file that is not the shape git writes is left alone rather than
    /// guessed at — inventing a repository path would be worse than none.
    #[test]
    fn test_scan_ignores_an_unrecognized_git_file() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join(".git"), "gitdir: /somewhere/odd\n").expect("write");

        let scan = scan(dir.path()).expect("scan should succeed");

        assert!(scan.worktree_of.is_none());
    }

    /// Scanning a file rather than a directory is an error.
    #[test]
    fn test_scan_rejects_non_directory() {
        let dir = TempDir::new().expect("tempdir");
        let file = dir.path().join("f.txt");
        touch(&file);
        assert!(matches!(scan(&file), Err(PackError::NotADirectory(_))));
    }

    // ------------------------------------------------------------------
    // helpers
    // ------------------------------------------------------------------

    /// Relative link targets are resolved against the link's own directory.
    #[test]
    fn test_resolves_outside_relative_target() {
        let root = Path::new("/proj");
        assert!(!resolves_outside(
            root,
            Path::new("/proj/sub/link"),
            Path::new("../file.txt")
        ));
        assert!(resolves_outside(
            root,
            Path::new("/proj/sub/link"),
            Path::new("../../escape.txt")
        ));
    }
}
