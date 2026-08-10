//! Restore a pack into a directory, then repair what a plain extract leaves
//! broken and report what the pack could not carry.
//!
//! Two things survive extraction only with help:
//!
//! - **worktree pointers.** A registered worktree is wired with two absolute
//!   paths — `.git/worktrees/<name>/gitdir` names the worktree's `.git` file,
//!   and that file names the admin directory back. Both still point at the
//!   machine the pack came from, so they are rewritten here.
//! - **symlinks leaving the project.** They are restored verbatim; the ones
//!   whose targets do not exist on this machine are reported rather than
//!   silently left broken.
//!
//! Restoring over an existing directory requires `force`, and that flag means
//! *overwrite*, not *wipe*: files already present that the pack does not carry
//! survive the restore. Nothing is deleted on the operator's behalf.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Component, Path, PathBuf};

use crate::create::PAYLOAD_PREFIX;
use crate::error::PackError;
use crate::manifest::{Manifest, SkipRecord, SymlinkRecord};

/// Inputs for [`restore`].
#[derive(Debug, Clone)]
pub struct RestoreOptions {
    /// Archive to restore.
    pub archive: PathBuf,
    /// Directory to create and unpack into.
    pub dest: PathBuf,
    /// Unpack into `dest` even if it already exists.
    ///
    /// This overwrites, it does not wipe: entries in the pack replace their
    /// counterparts in `dest`, and files already there that the pack does not
    /// contain are left untouched. Restoring over a working copy therefore
    /// recovers everything the pack holds without deleting anything it does
    /// not know about — at the cost of leaving unrelated leftovers in place.
    pub force: bool,
    /// Predict the restore and report it, writing nothing.
    ///
    /// A restore has consequences a listing of the archive cannot show: which
    /// files in the destination would be overwritten, which would survive
    /// untouched, which symlinks would land dangling on *this* machine, and
    /// which worktree pointers would be rewritten. A dry run answers those
    /// before the first byte is written.
    ///
    /// An existing destination is not an error during a dry run — reporting
    /// what would collide is precisely the point — so `force` is not required
    /// to preview one.
    pub dry_run: bool,
}

impl RestoreOptions {
    /// Build options that refuse to touch an existing destination.
    pub fn new(archive: impl Into<PathBuf>, dest: impl Into<PathBuf>) -> Self {
        Self {
            archive: archive.into(),
            dest: dest.into(),
            force: false,
            dry_run: false,
        }
    }
}

/// What [`restore`] did — or, on a dry run, what it would do.
#[derive(Debug, Clone)]
pub struct RestoreReport {
    /// Where the project was restored, or would be.
    pub dest: PathBuf,
    /// Manifest read from the archive.
    pub manifest: Manifest,
    /// Whether this was a prediction rather than a restore.
    pub dry_run: bool,
    /// Number of entries written, or that would be written.
    pub entries_written: u64,
    /// Whether the destination already exists.
    ///
    /// Only meaningful on a dry run; a real restore either refused or was
    /// given `force`.
    pub destination_exists: bool,
    /// Existing files the restore would replace.
    ///
    /// Populated on a dry run only. Empty when the destination is new.
    pub would_overwrite: Vec<String>,
    /// Existing files the pack does not carry, which would survive the restore.
    ///
    /// This is the concrete form of "`--force` overwrites, it does not wipe":
    /// everything listed here is still there afterwards. Populated on a dry
    /// run only.
    pub would_remain: Vec<String>,
    /// Worktrees whose pointer files were rewritten for the new location.
    pub rewritten_worktrees: Vec<String>,
    /// Worktrees that were registered at pack time but live outside the root,
    /// so their contents are not in the pack.
    pub missing_worktrees: Vec<String>,
    /// Restored symlinks whose targets do not exist on this machine.
    pub dangling_symlinks: Vec<SymlinkRecord>,
    /// `.claude/` link roots that are absent here, if any.
    pub missing_claude_link_roots: Vec<String>,
    /// Cache directories the pack deliberately dropped; regenerate as needed.
    pub regenerable_caches: Vec<SkipRecord>,
    /// Secrets the pack deliberately did not carry; move them out of band.
    pub secrets_not_carried: Vec<SkipRecord>,
}

impl RestoreReport {
    /// Whether anything needs operator attention after the restore.
    ///
    /// On a dry run this reads as "would need attention", and additionally
    /// counts a destination that already holds files: proceeding there needs
    /// `--force`, and whatever the pack does not carry stays behind.
    pub fn needs_attention(&self) -> bool {
        !self.dangling_symlinks.is_empty()
            || !self.missing_claude_link_roots.is_empty()
            || !self.missing_worktrees.is_empty()
            || !self.secrets_not_carried.is_empty()
            || !self.would_overwrite.is_empty()
            || !self.would_remain.is_empty()
    }
}

/// Restore a pack.
///
/// # Arguments
///
/// * `opts` — Archive path, destination, and whether to reuse an existing
///   destination directory.
///
/// # Returns
///
/// A [`RestoreReport`] describing the repairs made and the follow-up the
/// operator still owns.
///
/// # Errors
///
/// - [`PackError::DestinationExists`] if `dest` exists and `force` is unset.
/// - [`PackError::UnsupportedFormat`] if the pack is newer than this build.
/// - [`PackError::Io`] on read or write failure.
pub fn restore(opts: &RestoreOptions) -> Result<RestoreReport, PackError> {
    let manifest = crate::inspect::verify(&opts.archive)?;
    let destination_exists = opts.dest.exists();

    if opts.dry_run {
        return predict(opts, manifest, destination_exists);
    }

    if destination_exists && !opts.force {
        return Err(PackError::DestinationExists(opts.dest.clone()));
    }
    std::fs::create_dir_all(&opts.dest)?;
    let dest = std::fs::canonicalize(&opts.dest).unwrap_or_else(|_| opts.dest.clone());

    let entries_written = unpack_payload(&opts.archive, &dest)?;
    let rewritten_worktrees = rewrite_worktree_pointers(&dest, &manifest)?;

    let missing_worktrees = manifest
        .worktrees
        .iter()
        .filter(|w| !w.included)
        .map(|w| w.name.clone())
        .collect();

    let dangling_symlinks = manifest
        .symlinks
        .iter()
        .filter(|s| is_dangling(&dest, s))
        .cloned()
        .collect();

    let missing_claude_link_roots = manifest
        .claude
        .link_roots
        .iter()
        .filter(|r| !Path::new(r).exists())
        .cloned()
        .collect();

    Ok(RestoreReport {
        dest,
        dry_run: false,
        entries_written,
        destination_exists,
        would_overwrite: Vec::new(),
        would_remain: Vec::new(),
        rewritten_worktrees,
        missing_worktrees,
        dangling_symlinks,
        missing_claude_link_roots,
        regenerable_caches: manifest.skipped_cache.clone(),
        secrets_not_carried: manifest.skipped_secret.clone(),
        manifest,
    })
}

/// Work out what a restore would do, touching nothing.
///
/// Everything here is derived from the archive plus the current state of the
/// destination, so the prediction is about *this* machine — a symlink is
/// reported as dangling because its target is absent here, not because it was
/// absent where the pack was made.
fn predict(
    opts: &RestoreOptions,
    manifest: Manifest,
    destination_exists: bool,
) -> Result<RestoreReport, PackError> {
    let dest = std::fs::canonicalize(&opts.dest).unwrap_or_else(|_| opts.dest.clone());
    let payload = crate::inspect::list_payload_paths(&opts.archive)?;
    let payload_set: BTreeSet<&str> = payload.iter().map(|s| s.as_str()).collect();

    let (would_overwrite, would_remain) = if destination_exists {
        compare_destination(&dest, &payload_set)
    } else {
        (Vec::new(), Vec::new())
    };

    let rewritten_worktrees = manifest
        .worktrees
        .iter()
        .filter(|w| w.included)
        .map(|w| w.name.clone())
        .collect();

    let missing_worktrees = manifest
        .worktrees
        .iter()
        .filter(|w| !w.included)
        .map(|w| w.name.clone())
        .collect();

    let dangling_symlinks = manifest
        .symlinks
        .iter()
        .filter(|s| would_dangle(&dest, s, &payload_set))
        .cloned()
        .collect();

    let missing_claude_link_roots = manifest
        .claude
        .link_roots
        .iter()
        .filter(|r| !Path::new(r).exists())
        .cloned()
        .collect();

    Ok(RestoreReport {
        dest,
        dry_run: true,
        entries_written: payload.len() as u64,
        destination_exists,
        would_overwrite,
        would_remain,
        rewritten_worktrees,
        missing_worktrees,
        dangling_symlinks,
        missing_claude_link_roots,
        regenerable_caches: manifest.skipped_cache.clone(),
        secrets_not_carried: manifest.skipped_secret.clone(),
        manifest,
    })
}

/// Split the destination's existing files into "would be replaced" and
/// "would survive".
///
/// Directories are ignored on both sides: only file contents can be lost, and
/// a directory that exists in both places is not a collision worth reporting.
fn compare_destination(dest: &Path, incoming: &BTreeSet<&str>) -> (Vec<String>, Vec<String>) {
    let mut overwrite = Vec::new();
    let mut remain = Vec::new();

    let walker = walkdir::WalkDir::new(dest)
        .follow_links(false)
        .min_depth(1)
        .sort_by_file_name();

    for entry in walker.into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_dir() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(dest) else {
            continue;
        };
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if rel.is_empty() {
            continue;
        }
        if incoming.contains(rel.as_str()) {
            overwrite.push(rel);
        } else {
            remain.push(rel);
        }
    }

    (overwrite, remain)
}

/// Whether a symlink from the archive would land dangling here.
///
/// The link does not exist yet, so its target is resolved as it *would* be.
/// An absolute target is checked as written — it keeps pointing at the machine
/// it named, which is exactly why moving a project can break it. A relative
/// target is resolved inside the restored tree, and may well be satisfied by
/// the pack itself: the target file does not exist on disk yet, but it is in
/// the payload and will be there by the time the link is. Checking the
/// filesystem alone would report every such link as dangling.
fn would_dangle(dest: &Path, record: &SymlinkRecord, payload: &BTreeSet<&str>) -> bool {
    let target = Path::new(&record.target);

    if target.is_absolute() {
        return !target.exists();
    }

    let link_parent = Path::new(&record.path).parent().unwrap_or(Path::new(""));
    let resolved = crate::scan::normalize(&link_parent.join(target));

    let as_key = resolved
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if payload.contains(as_key.as_str()) {
        // The pack supplies it; it will exist alongside the link.
        return false;
    }

    !dest.join(&resolved).exists()
}

/// Extract every `payload/` entry into `dest`, stripping the prefix.
fn unpack_payload(archive: &Path, dest: &Path) -> Result<u64, PackError> {
    let file = File::open(archive)?;
    let decoder = zstd::stream::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);

    let mut written = 0u64;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        let Ok(rel) = path.strip_prefix(PAYLOAD_PREFIX) else {
            // The manifest entry, and anything else outside the payload.
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        // Refuse anything that would escape the destination. A pack is normally
        // produced by this same crate, but an archive is an untrusted input the
        // moment it arrives from elsewhere.
        if rel
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            tracing::warn!("skipping unsafe archive path: {}", rel.display());
            continue;
        }

        let out = dest.join(rel);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Re-extracting over an existing link fails on some platforms; clear it.
        if out.is_symlink() {
            std::fs::remove_file(&out)?;
        }
        entry.unpack(&out)?;
        written += 1;
    }

    Ok(written)
}

/// Point every included worktree at its new location.
///
/// Returns the names of the worktrees whose pointers were rewritten.
fn rewrite_worktree_pointers(dest: &Path, manifest: &Manifest) -> Result<Vec<String>, PackError> {
    let mut rewritten = Vec::new();

    for record in &manifest.worktrees {
        let Some(rel) = record.path.as_deref() else {
            continue;
        };
        let admin = dest.join(".git").join("worktrees").join(&record.name);
        let worktree_root = dest.join(rel);
        if !admin.is_dir() || !worktree_root.is_dir() {
            // The admin directory or the worktree itself did not make it into
            // the payload; nothing to repair.
            continue;
        }

        let dot_git = worktree_root.join(".git");
        std::fs::write(admin.join("gitdir"), format!("{}\n", dot_git.display()))?;
        std::fs::write(&dot_git, format!("gitdir: {}\n", admin.display()))?;
        rewritten.push(record.name.clone());
    }

    Ok(rewritten)
}

/// Whether a restored symlink points at something that does not exist here.
fn is_dangling(dest: &Path, record: &SymlinkRecord) -> bool {
    let link = dest.join(&record.path);
    if !link.is_symlink() {
        // Not restored at all; not this check's concern.
        return false;
    }
    !link.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create::{CreateOptions, create};
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, body).expect("write");
    }

    /// A pack round-trips: every packed file comes back with its contents.
    #[test]
    fn test_round_trip_preserves_content() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        touch(&root.join("src/main.rs"), "fn main() {}");
        touch(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        touch(&root.join("workspace/journal.md"), "# journal\n");
        touch(&root.join("workspace/.journal.db"), "sqlite");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("restored");
        let report = restore(&RestoreOptions::new(&out, &dest)).expect("restore");

        assert_eq!(
            fs::read_to_string(dest.join("src/main.rs")).expect("read"),
            "fn main() {}"
        );
        assert_eq!(
            fs::read_to_string(dest.join(".git/HEAD")).expect("read"),
            "ref: refs/heads/main\n"
        );
        assert_eq!(
            fs::read_to_string(dest.join("workspace/.journal.db")).expect("read"),
            "sqlite",
            "local state must survive the round trip"
        );
        assert!(report.entries_written > 0);
    }

    /// An existing destination is refused unless `force` is set.
    #[test]
    fn test_restore_refuses_existing_destination() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        touch(&root.join("a.txt"), "a");
        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("existing");
        fs::create_dir_all(&dest).expect("mkdir");

        assert!(matches!(
            restore(&RestoreOptions::new(&out, &dest)),
            Err(PackError::DestinationExists(_))
        ));

        let forced = RestoreOptions {
            force: true,
            ..RestoreOptions::new(&out, &dest)
        };
        restore(&forced).expect("force should proceed");
        assert!(dest.join("a.txt").is_file());
    }

    /// Worktree pointers are rewritten to the new root, not left aimed at the
    /// machine the pack came from.
    #[test]
    fn test_restore_rewrites_worktree_pointers() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        touch(&root.join(".git/HEAD"), "ref: refs/heads/main\n");

        let wt = root.join(".worktrees/feature");
        touch(&wt.join("file.txt"), "work");
        let admin = root.join(".git/worktrees/feature");
        fs::create_dir_all(&admin).expect("mkdir");
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).expect("write");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .expect("write");
        fs::write(admin.join("commondir"), "../..\n").expect("write");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("moved");
        let report = restore(&RestoreOptions::new(&out, &dest)).expect("restore");

        assert_eq!(report.rewritten_worktrees, vec!["feature".to_string()]);

        let new_admin_gitdir =
            fs::read_to_string(dest.join(".git/worktrees/feature/gitdir")).expect("read");
        let new_dot_git = fs::read_to_string(dest.join(".worktrees/feature/.git")).expect("read");

        let dest_real = fs::canonicalize(&dest).expect("canonicalize");
        assert!(
            new_admin_gitdir
                .trim()
                .starts_with(&dest_real.to_string_lossy().to_string()),
            "gitdir must point into the new root, got {new_admin_gitdir}"
        );
        assert!(
            new_dot_git
                .trim()
                .contains(&dest_real.to_string_lossy().to_string()),
            "worktree .git must point into the new root, got {new_dot_git}"
        );
        assert!(
            !new_admin_gitdir.contains("/proj/"),
            "stale source path must not survive: {new_admin_gitdir}"
        );
    }

    /// A symlink whose target is gone is restored and reported, not hidden.
    #[cfg(unix)]
    #[test]
    fn test_restore_reports_dangling_symlink() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        let vanishing = dir.path().join("vanishing");
        fs::create_dir_all(&vanishing).expect("mkdir");
        touch(&vanishing.join("target.md"), "t");
        std::os::unix::fs::symlink(vanishing.join("target.md"), root.join("link.md"))
            .expect("symlink");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        // The link target disappears before the restore.
        fs::remove_dir_all(&vanishing).expect("rm");

        let dest = dir.path().join("restored");
        let report = restore(&RestoreOptions::new(&out, &dest)).expect("restore");

        assert!(dest.join("link.md").is_symlink(), "link itself is restored");
        assert_eq!(report.dangling_symlinks.len(), 1);
        assert_eq!(report.dangling_symlinks[0].path, "link.md");
        assert!(report.needs_attention());
    }

    /// A symlink whose target still exists is not reported as dangling.
    #[cfg(unix)]
    #[test]
    fn test_restore_does_not_report_live_symlink() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        touch(&root.join("real.txt"), "r");
        std::os::unix::fs::symlink("real.txt", root.join("rel-link")).expect("symlink");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("restored");
        let report = restore(&RestoreOptions::new(&out, &dest)).expect("restore");

        assert!(report.dangling_symlinks.is_empty());
    }

    // ------------------------------------------------------------------
    // dry run
    // ------------------------------------------------------------------

    /// A dry run creates nothing at all, not even the destination directory.
    #[test]
    fn test_dry_run_writes_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        touch(&root.join("a.txt"), "a");
        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("nowhere");
        let opts = RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&out, &dest)
        };
        let report = restore(&opts).expect("dry run");

        assert!(report.dry_run);
        assert!(!dest.exists(), "dry run must not create the destination");
        assert!(
            report.entries_written > 0,
            "it still counts what would land"
        );
        assert!(!report.destination_exists);
        assert!(report.would_overwrite.is_empty());
        assert!(report.would_remain.is_empty());
    }

    /// An existing destination is previewable without `--force`, and the split
    /// between "replaced" and "remains" is what makes the overwrite semantics
    /// legible before committing to them.
    #[test]
    fn test_dry_run_splits_existing_destination() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        touch(&root.join("shared.txt"), "from pack");
        touch(&root.join("only-in-pack.txt"), "new");
        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("existing");
        touch(&dest.join("shared.txt"), "old content");
        touch(&dest.join("only-in-dest.txt"), "leftover");

        // No force, and yet the preview succeeds — refusing here would defeat
        // the purpose of asking what would happen.
        let opts = RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&out, &dest)
        };
        let report = restore(&opts).expect("dry run over existing dest");

        assert!(report.destination_exists);
        assert_eq!(report.would_overwrite, vec!["shared.txt".to_string()]);
        assert_eq!(report.would_remain, vec!["only-in-dest.txt".to_string()]);
        assert!(report.needs_attention());

        // And the destination is untouched by the preview itself.
        assert_eq!(
            fs::read_to_string(dest.join("shared.txt")).expect("read"),
            "old content"
        );
    }

    /// The prediction matches what the real restore then does.
    #[test]
    fn test_dry_run_agrees_with_real_restore() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        touch(&root.join("a.txt"), "a");
        touch(&root.join("sub/b.txt"), "b");
        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("dest");
        let predicted = restore(&RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&out, &dest)
        })
        .expect("dry run");

        let actual = restore(&RestoreOptions::new(&out, &dest)).expect("real restore");

        assert_eq!(
            predicted.entries_written, actual.entries_written,
            "a dry run that miscounts is worse than none"
        );
        assert_eq!(predicted.rewritten_worktrees, actual.rewritten_worktrees);
        assert_eq!(
            predicted.dangling_symlinks.len(),
            actual.dangling_symlinks.len()
        );
    }

    /// A dangling symlink is predicted before the link exists.
    #[cfg(unix)]
    #[test]
    fn test_dry_run_predicts_dangling_symlink() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        let vanishing = dir.path().join("vanishing");
        fs::create_dir_all(&vanishing).expect("mkdir");
        touch(&vanishing.join("t.md"), "t");
        std::os::unix::fs::symlink(vanishing.join("t.md"), root.join("link.md")).expect("symlink");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");
        fs::remove_dir_all(&vanishing).expect("rm");

        let dest = dir.path().join("dest");
        let predicted = restore(&RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&out, &dest)
        })
        .expect("dry run");

        assert_eq!(predicted.dangling_symlinks.len(), 1);
        assert_eq!(predicted.dangling_symlinks[0].path, "link.md");
        assert!(!dest.exists(), "still nothing written");

        // The real restore then agrees.
        let actual = restore(&RestoreOptions::new(&out, &dest)).expect("restore");
        assert_eq!(actual.dangling_symlinks.len(), 1);
    }

    /// A relative link that stays inside the project is not predicted dangling.
    #[cfg(unix)]
    #[test]
    fn test_dry_run_does_not_predict_live_relative_link() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        touch(&root.join("real.txt"), "r");
        std::os::unix::fs::symlink("real.txt", root.join("rel-link")).expect("symlink");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("dest");
        let predicted = restore(&RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&out, &dest)
        })
        .expect("dry run");

        assert!(
            predicted.dangling_symlinks.is_empty(),
            "a link resolving inside the restored tree is fine"
        );
    }

    /// Worktree pointer rewrites are announced ahead of time.
    #[test]
    fn test_dry_run_announces_worktree_rewrite() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        touch(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        let wt = root.join(".worktrees/feature");
        touch(&wt.join("f.txt"), "w");
        let admin = root.join(".git/worktrees/feature");
        fs::create_dir_all(&admin).expect("mkdir");
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).expect("write");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .expect("write");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("dest");
        let predicted = restore(&RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&out, &dest)
        })
        .expect("dry run");

        assert_eq!(predicted.rewritten_worktrees, vec!["feature".to_string()]);
        assert!(!dest.exists());
    }

    /// Skipped secrets and caches are carried into the report so the operator
    /// learns what still needs doing.
    #[test]
    fn test_restore_report_carries_skips() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        touch(&root.join("a.txt"), "a");
        touch(&root.join(".env"), "S=1");
        touch(&root.join("target/x"), "bin");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let dest = dir.path().join("restored");
        let report = restore(&RestoreOptions::new(&out, &dest)).expect("restore");

        assert!(report.secrets_not_carried.iter().any(|s| s.path == ".env"));
        assert!(report.regenerable_caches.iter().any(|s| s.path == "target"));
        assert!(!dest.join(".env").exists());
        assert!(report.needs_attention());
    }
}
