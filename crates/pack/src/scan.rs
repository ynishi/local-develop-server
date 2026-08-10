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
//! | cache directory | not packed, recorded; regenerable by definition |
//! | secret file | not packed, reported; moving credentials is the operator's own business |
//! | symlink | packed as a link, never dereferenced; recorded so restore can report dangles |
//! | `.claude/` | packed verbatim, links aggregated rather than enumerated |

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use walkdir::WalkDir;

use crate::error::PackError;
use crate::manifest::{ClaudeInfo, SkipRecord, SymlinkRecord, WorktreeRecord};

/// Directory names treated as regenerable caches.
///
/// `dist` and `build` are deliberately absent: both are common names for
/// hand-written source in projects that do not use them as output directories,
/// and wrongly dropping source is far worse than carrying a rebuildable tree.
pub const CACHE_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".turbo",
    ".next",
    ".nuxt",
    ".parcel-cache",
    ".gradle",
];

/// File names dropped silently — OS debris that is neither cache nor content.
const NOISE_FILES: &[&str] = &[".DS_Store", "Thumbs.db"];

/// Exact file names treated as secrets.
const SECRET_EXACT: &[&str] = &[
    ".env",
    ".netrc",
    ".npmrc",
    "credentials",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Extensions treated as secrets.
const SECRET_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "jks", "keystore"];

/// `.env.*` suffixes that are templates rather than secrets, so they travel.
const ENV_TEMPLATE_SUFFIXES: &[&str] = &["example", "sample", "template", "dist", "defaults"];

/// The `.claude/` directory name, handled as its own layer.
const CLAUDE_DIR: &str = ".claude";

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
    /// Cache directories that were dropped.
    pub skipped_cache: Vec<SkipRecord>,
    /// Secret-looking files that were dropped and reported.
    pub skipped_secret: Vec<SkipRecord>,
    /// Symlinks outside `.claude/`, recorded individually.
    pub symlinks: Vec<SymlinkRecord>,
    /// Aggregate view of `.claude/`.
    pub claude: ClaudeInfo,
    /// Registered git worktrees discovered under `.git/worktrees/`.
    pub worktrees: Vec<WorktreeRecord>,
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

/// Classify every path under `root`.
///
/// # Arguments
///
/// * `root` — Absolute path to the project root.
///
/// # Returns
///
/// A [`Scan`] listing what to pack and what was deliberately left out.
///
/// # Errors
///
/// - [`PackError::NotADirectory`] if `root` is not a directory.
/// - [`PackError::Io`] if the tree cannot be walked or a link cannot be read.
pub fn scan(root: &Path) -> Result<Scan, PackError> {
    if !root.is_dir() {
        return Err(PackError::NotADirectory(root.to_path_buf()));
    }
    // Resolve the root up front so paths recorded by git (which may name the
    // same directory through a different symlinked prefix) can be compared
    // against it. Without this, a worktree inside the project reads as being
    // outside it and its contents silently stop travelling.
    let root = &canonicalize_or(root);

    let mut scan = Scan::default();
    let mut claude_link_targets: Vec<PathBuf> = Vec::new();

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
        if e.file_type().is_dir() && CACHE_DIRS.contains(&name.as_ref()) {
            return false;
        }
        true
    });

    // Cache directories are pruned above, which also hides them from the
    // record; walk their parents separately so each dropped cache is named.
    collect_cache_records(root, &mut scan)?;

    for next in it {
        let entry = next?;
        let abs = entry.path().to_path_buf();
        let Some(rel) = rel_path(root, &abs) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();

        if NOISE_FILES.contains(&name.as_str()) {
            continue;
        }

        let file_type = entry.file_type();
        let in_claude = rel == CLAUDE_DIR || rel.starts_with(&format!("{CLAUDE_DIR}/"));

        if file_type.is_symlink() {
            let target = std::fs::read_link(&abs)?;
            if in_claude {
                // `.claude/` links are aggregated, not enumerated.
                scan.claude.symlink_count += 1;
                claude_link_targets.push(target);
            } else {
                scan.symlinks.push(SymlinkRecord {
                    path: rel.clone(),
                    target: target.to_string_lossy().into_owned(),
                    outside_root: resolves_outside(root, &abs, &target),
                });
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
            if rel == CLAUDE_DIR {
                scan.claude.present = true;
            }
            scan.entries.push(Entry {
                rel,
                abs,
                kind: EntryKind::Dir,
                size: 0,
            });
            continue;
        }

        if let Some(reason) = secret_reason(&name) {
            scan.skipped_secret.push(SkipRecord { path: rel, reason });
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        scan.entries.push(Entry {
            rel,
            abs,
            kind: EntryKind::File,
            size,
        });
    }

    scan.claude.link_roots = summarize_link_roots(&claude_link_targets);
    scan.worktrees = discover_worktrees(root)?;

    Ok(scan)
}

/// Walk the tree a second time, shallowly, to name every pruned cache directory.
///
/// `filter_entry` removes cache directories before they are yielded, so they
/// would otherwise vanish without a record. This pass descends normally but
/// stops at each cache directory it names, so the cost stays proportional to
/// the surviving tree.
fn collect_cache_records(root: &Path, scan: &mut Scan) -> Result<(), PackError> {
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
        if !CACHE_DIRS.contains(&name.as_str()) {
            continue;
        }
        if let Some(rel) = rel_path(root, entry.path()) {
            scan.skipped_cache.push(SkipRecord {
                path: rel,
                reason: format!("cache directory: {name}"),
            });
        }
        it.skip_current_dir();
    }

    Ok(())
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

/// Decide whether a file name looks like a secret, and say which rule matched.
///
/// # Arguments
///
/// * `name` — File name (not a full path).
///
/// # Returns
///
/// `Some(reason)` when the file must not be packed, `None` otherwise.
pub fn secret_reason(name: &str) -> Option<String> {
    if SECRET_EXACT.contains(&name) {
        return Some(format!("secret pattern: {name}"));
    }

    if let Some(rest) = name.strip_prefix(".env.") {
        // `.env.example` and friends are checked-in templates, not secrets.
        if ENV_TEMPLATE_SUFFIXES.contains(&rest) {
            return None;
        }
        return Some(format!("secret pattern: .env.* ({name})"));
    }

    if let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) {
        let lower = ext.to_ascii_lowercase();
        if SECRET_EXTENSIONS.contains(&lower.as_str()) {
            return Some(format!("secret extension: .{lower}"));
        }
    }

    None
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
fn canonicalize_or(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| normalize(path))
}

/// Lexically normalize a path, collapsing `.` and `..` without touching disk.
fn normalize(path: &Path) -> PathBuf {
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

/// Reduce a set of link targets to the smallest useful set of roots.
///
/// When every target shares a deep prefix — the usual shape for a
/// profile-managed `.claude/` — that single prefix is reported. When the
/// targets scatter, their parent directories are reported instead, capped so
/// the manifest cannot be flooded.
fn summarize_link_roots(targets: &[PathBuf]) -> Vec<String> {
    const MAX_ROOTS: usize = 10;
    /// Below this depth a shared prefix is too generic to be informative
    /// (`/`, `/Users`, `/Users/name`).
    const MIN_SHARED_DEPTH: usize = 4;

    let parents: BTreeSet<PathBuf> = targets
        .iter()
        .filter(|t| t.is_absolute())
        .filter_map(|t| t.parent().map(normalize))
        .collect();

    if parents.is_empty() {
        return Vec::new();
    }

    let parents: Vec<PathBuf> = parents.into_iter().collect();
    if let Some(shared) = common_prefix(&parents)
        && shared.components().count() >= MIN_SHARED_DEPTH
    {
        return vec![shared.to_string_lossy().into_owned()];
    }

    parents
        .iter()
        .take(MAX_ROOTS)
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Longest path prefix shared by every input, or `None` for an empty input.
fn common_prefix(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut iter = paths.iter();
    let mut prefix: Vec<_> = iter.next()?.components().collect();

    for path in iter {
        let comps: Vec<_> = path.components().collect();
        let shared = prefix
            .iter()
            .zip(comps.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(shared);
        if prefix.is_empty() {
            return None;
        }
    }

    Some(prefix.iter().collect())
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
            source_path: worktree_root.to_string_lossy().into_owned(),
        });
    }

    Ok(records)
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
    // secret classification
    // ------------------------------------------------------------------

    /// Exact-name secrets are recognized.
    #[test]
    fn test_secret_reason_exact_names() {
        assert!(secret_reason(".env").is_some());
        assert!(secret_reason("id_rsa").is_some());
        assert!(secret_reason(".netrc").is_some());
    }

    /// Extension-based secrets are recognized, case-insensitively.
    #[test]
    fn test_secret_reason_extensions() {
        assert!(secret_reason("server.pem").is_some());
        assert!(secret_reason("SERVER.PEM").is_some());
        assert!(secret_reason("store.jks").is_some());
    }

    /// `.env.*` templates are content, not secrets.
    #[test]
    fn test_secret_reason_env_templates_travel() {
        assert!(secret_reason(".env.example").is_none());
        assert!(secret_reason(".env.sample").is_none());
        assert!(secret_reason(".env.template").is_none());
    }

    /// A real `.env.<stage>` file is still a secret.
    #[test]
    fn test_secret_reason_env_stage_is_secret() {
        assert!(secret_reason(".env.production").is_some());
        assert!(secret_reason(".env.local").is_some());
    }

    /// Ordinary files are not secrets.
    #[test]
    fn test_secret_reason_ordinary_files() {
        assert!(secret_reason("main.rs").is_none());
        assert!(secret_reason("README.md").is_none());
        assert!(secret_reason(".mcp.json").is_none());
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

    /// Symlinks outside `.claude/` are recorded individually and packed as links.
    #[cfg(unix)]
    #[test]
    fn test_scan_records_symlinks_outside_claude() {
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

    /// `.claude/` links are counted and summarized, never enumerated.
    #[cfg(unix)]
    #[test]
    fn test_scan_aggregates_claude_links() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        let profiles = TempDir::new().expect("tempdir");
        let agents = profiles.path().join("sets/coding/agents");
        let rules = profiles.path().join("sets/base/rules");
        fs::create_dir_all(&agents).expect("mkdir");
        fs::create_dir_all(&rules).expect("mkdir");
        touch(&agents.join("a.md"));
        touch(&rules.join("b.md"));

        fs::create_dir_all(root.join(".claude/agents")).expect("mkdir");
        fs::create_dir_all(root.join(".claude/rules")).expect("mkdir");
        std::os::unix::fs::symlink(agents.join("a.md"), root.join(".claude/agents/a.md"))
            .expect("symlink");
        std::os::unix::fs::symlink(rules.join("b.md"), root.join(".claude/rules/b.md"))
            .expect("symlink");

        let scan = scan(root).expect("scan should succeed");

        assert!(scan.claude.present);
        assert_eq!(scan.claude.symlink_count, 2);
        assert!(
            scan.symlinks.is_empty(),
            "`.claude` links must not appear in the per-link list"
        );
        assert_eq!(
            scan.claude.link_roots.len(),
            1,
            "a shared profiles root collapses to one entry, got {:?}",
            scan.claude.link_roots
        );
        assert!(rels(&scan).contains(&".claude/agents/a.md".to_string()));
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

    /// A deep shared prefix collapses to a single root.
    #[test]
    fn test_summarize_link_roots_collapses_shared_prefix() {
        let targets = vec![
            PathBuf::from("/home/u/.config/profiles/sets/coding/agents/a.md"),
            PathBuf::from("/home/u/.config/profiles/sets/base/rules/b.md"),
        ];
        let roots = summarize_link_roots(&targets);
        assert_eq!(roots, vec!["/home/u/.config/profiles/sets".to_string()]);
    }

    /// Scattered targets are listed by parent instead of collapsing to `/`.
    #[test]
    fn test_summarize_link_roots_keeps_scattered_parents() {
        let targets = vec![PathBuf::from("/opt/a/x.md"), PathBuf::from("/srv/b/y.md")];
        let roots = summarize_link_roots(&targets);
        assert_eq!(roots.len(), 2);
    }

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
