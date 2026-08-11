//! Restore a pack into a directory, then repair what a plain extract leaves
//! broken and report what the pack could not carry.
//!
//! Two things survive extraction only with help:
//!
//! - **worktree pointers.** A registered worktree is wired with two absolute
//!   paths — `.git/worktrees/<name>/gitdir` names the worktree's `.git` file,
//!   and that file names the admin directory back. Both still point at the
//!   machine the pack came from, so they are rewritten here. When the worktree
//!   lives *beside* the project rather than inside it, those two paths are in
//!   two different packs; the pair is wired up once both have been restored,
//!   in whichever order that happens.
//! - **symlinks leaving the project.** They are restored verbatim; the ones
//!   whose targets do not exist on this machine are reported rather than
//!   silently left broken.
//!
//! Restoring over an existing directory requires `force`, and that flag means
//! *overwrite*, not *wipe*: files already present that the pack does not carry
//! survive the restore. Nothing is deleted on the operator's behalf.
//!
//! One thing is deliberately *not* done: a hard link entry is reported and left
//! uncreated, with the command that would create it. See [`HardLinkRecord`].
//!
//! Everything the archive says about where things go is checked before any of
//! it is joined onto the destination. Entry names and manifest paths are the
//! same kind of claim, and both go through `Contained`, which cannot hold a
//! path that leaves the destination. What may be written is then decided by one
//! exhaustive match per question — [`EntryPlan`] for an entry's type, `Wiring`
//! for a worktree's layout — so a shape nobody has thought about yet is refused
//! rather than falling through to a write.

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::contained::Contained;
use crate::create::PAYLOAD_PREFIX;
use crate::error::PackError;
use crate::manifest::{
    CacheRecord, Manifest, SkipRecord, SymlinkRecord, WorktreeOrigin, WorktreeRecord,
};
use crate::scan::canonicalize_or;

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
    ///
    /// Includes worktrees that live *beside* the root rather than inside it:
    /// their contents travel in their own pack, but the wiring is repaired here
    /// as soon as both halves are on this machine.
    pub rewritten_worktrees: Vec<String>,
    /// Worktrees registered at pack time whose checkout is not on this machine.
    ///
    /// Their contents are not in this pack — restore the pack that holds them
    /// and the wiring completes itself, in either order.
    pub missing_worktrees: Vec<String>,
    /// Worktree pairings left unwired because the counterpart's place is
    /// occupied by something that is not this worktree's other half.
    ///
    /// Nothing was written: wiring writes both pointer files, and one of them
    /// belongs to whatever is sitting there — an unrelated repository, or a
    /// worktree of a different one. Overwriting it would break that project to
    /// repair this one, so the collision is reported and the decision is the
    /// operator's.
    pub conflicting_worktrees: Vec<WorktreeConflict>,
    /// Set when this pack is a worktree checkout and its repository was not
    /// found beside the restored root, leaving the checkout with no repository.
    ///
    /// Holds the repository's path as of pack time, as a hint for where its
    /// pack belongs.
    pub missing_worktree_parent: Option<String>,
    /// Restored symlinks whose targets do not exist on this machine.
    ///
    /// Covers every link the pack reported. Links the pack's author declared
    /// under `no_link_report` are restored but never checked here, which is
    /// what [`Self::link_reports_suppressed`] exists to say.
    pub dangling_symlinks: Vec<SymlinkRecord>,
    /// `no_link_report` rules that were in force when this pack was written.
    ///
    /// Informational, and deliberately not part of [`Self::needs_attention`]:
    /// the author declared those links expected, so there is nothing to act on.
    /// It is reported anyway because otherwise an empty `dangling_symlinks`
    /// reads as "every link here is fine" when it means "every link I was
    /// allowed to look at is fine".
    pub link_reports_suppressed: Vec<String>,
    /// Cache directories the pack deliberately dropped; regenerate as needed.
    ///
    /// Each carries the file count and size that went with it, so a directory
    /// that was named a cache by mistake is visible as one whose figures do not
    /// look like a build tree's.
    pub regenerable_caches: Vec<CacheRecord>,
    /// Secrets the pack deliberately did not carry; move them out of band.
    pub secrets_not_carried: Vec<SkipRecord>,
    /// Hard links the archive carried, which this restore did not create.
    ///
    /// Each carries the command that would create it. Deciding whether to run
    /// it is the operator's, which is the whole reason the entry is listed
    /// here rather than acted on — see [`HardLinkRecord`].
    pub hard_links_not_created: Vec<HardLinkRecord>,
}

/// A hard link the archive carried and the restore did not create.
///
/// A hard link is a second name for a file that already exists, and the archive
/// says which one. That target is not carried in the pack and not checked
/// against it — it names something on the machine the restore is landing on. A
/// crafted archive can therefore name any file the operator can read, and
/// creating the link would publish that file's contents into the restored tree
/// under a name of the archive's choosing.
///
/// Nothing this crate writes produces one: the scan stores every file as its
/// own regular entry, so two names for one inode come back as two files. A hard
/// link entry is thus always from another producer, and there is no reading of
/// it that makes creating it automatically the right thing to do.
///
/// It is also not passed over in silence. Skipping quietly would leave the
/// restored tree missing a path the archive listed, with nothing to say why.
/// The record names the link, the target as the archive wrote it, and a command
/// that creates it — to be run once the operator has looked at what the target
/// actually is.
#[derive(Debug, Clone, Serialize)]
pub struct HardLinkRecord {
    /// Path of the link, relative to the restored root.
    pub path: String,
    /// The file the archive says the link should point at, exactly as written.
    ///
    /// Two shapes occur. A target inside the payload is a link to another file
    /// in this same pack, which is what a tar writer produces for a project
    /// that contains hard links; it reads as an archive-relative path like
    /// `payload/b.txt`. Anything else names a file on the machine doing the
    /// restore, and is absolute and pointing anywhere if the archive says so.
    pub target: String,
    /// A POSIX `ln` invocation that would create the link, both paths quoted.
    ///
    /// A target inside the payload is resolved to where that entry actually
    /// landed, so the command works from any directory. One outside it is used
    /// as written, because that is the only thing it can mean.
    pub command: String,
}

impl HardLinkRecord {
    /// Describe one hard link entry against the destination it would land in.
    fn new(dest: &Path, rel: &Path, target: &str) -> Self {
        let at = dest.join(rel);
        Self {
            path: rel.display().to_string(),
            target: target.to_string(),
            command: format!(
                "ln {} {}",
                shell_quote(&resolve_link_target(dest, target)),
                shell_quote(&at.display().to_string())
            ),
        }
    }
}

/// Where a hard link's target actually is, once the restore has run.
///
/// A target naming another payload entry is archive-relative, so quoting it
/// into a command would aim it at whatever the operator's working directory
/// happens to hold. It is resolved against the destination instead. A target
/// that is not in the payload, or one that would leave the destination, is
/// returned as written: it refers to something outside this pack, and rewriting
/// it would be inventing a claim the archive did not make.
fn resolve_link_target(dest: &Path, target: &str) -> String {
    Path::new(target)
        .strip_prefix(PAYLOAD_PREFIX)
        .ok()
        .and_then(|rel| Contained::entry(rel).ok())
        .map_or_else(
            || target.to_string(),
            |rel| rel.join_onto(dest).display().to_string(),
        )
}

/// Wrap a path for a POSIX shell, so a name with a space or a quote in it
/// survives being pasted into one.
fn shell_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// A worktree pairing that was found occupied by something other than this
/// worktree's counterpart.
///
/// Wiring is a write to both halves, and one of the two files belongs to
/// whatever sits at the counterpart's place. When that occupant cannot be
/// confirmed as this worktree's other half — its pointer names a different
/// repository, or it is an independent repository outright — nothing is
/// written and the collision is reported instead.
#[derive(Debug, Clone, Serialize)]
pub struct WorktreeConflict {
    /// Worktree name under `.git/worktrees/`.
    pub name: String,
    /// The occupied path the wiring stopped at.
    pub path: String,
    /// What was found there.
    pub found: String,
}

impl RestoreReport {
    /// Whether anything needs operator attention after the restore.
    ///
    /// On a dry run this reads as "would need attention", and additionally
    /// counts a destination that already holds files: proceeding there needs
    /// `--force`, and whatever the pack does not carry stays behind.
    pub fn needs_attention(&self) -> bool {
        !self.dangling_symlinks.is_empty()
            || !self.missing_worktrees.is_empty()
            || !self.conflicting_worktrees.is_empty()
            || self.missing_worktree_parent.is_some()
            || !self.secrets_not_carried.is_empty()
            || !self.would_overwrite.is_empty()
            || !self.would_remain.is_empty()
            || !self.hard_links_not_created.is_empty()
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
/// - [`PackError::EscapingArchivePath`], [`PackError::EscapingManifestPath`],
///   [`PackError::WriteThroughSymlink`] or [`PackError::UnusableArchiveEntry`]
///   if the archive is crafted. The manifest is checked before the first byte
///   is written, so an archive that lies about where its worktrees go is
///   refused with the destination still empty.
/// - [`PackError::Io`] on read or write failure.
pub fn restore(opts: &RestoreOptions) -> Result<RestoreReport, PackError> {
    let manifest = crate::inspect::verify(&opts.archive)?;
    let checked = check_archive_paths(&manifest)?;
    let destination_exists = opts.dest.exists();

    if opts.dry_run {
        return predict(opts, &checked, &manifest, destination_exists);
    }

    if destination_exists && !opts.force {
        return Err(PackError::DestinationExists(opts.dest.clone()));
    }
    std::fs::create_dir_all(&opts.dest)?;
    let dest = resolve_dest(&opts.dest);

    let (entries_written, hard_links_not_created) = unpack_payload(&opts.archive, &dest)?;
    let plan = plan_worktree_pointers(&dest, &checked, &manifest, true);
    let rewritten_worktrees = apply_worktree_plan(&plan)?;

    let dangling_symlinks = manifest
        .symlinks
        .iter()
        .filter(|s| is_dangling(&dest, s))
        .cloned()
        .collect();

    let link_reports_suppressed = manifest.no_link_report_applied.clone();

    Ok(RestoreReport {
        dest,
        dry_run: false,
        entries_written,
        destination_exists,
        would_overwrite: Vec::new(),
        would_remain: Vec::new(),
        rewritten_worktrees,
        missing_worktrees: plan.missing,
        conflicting_worktrees: plan.conflicted,
        missing_worktree_parent: plan.missing_parent,
        dangling_symlinks,
        link_reports_suppressed,
        regenerable_caches: manifest.skipped_cache.clone(),
        secrets_not_carried: manifest.skipped_secret.clone(),
        hard_links_not_created,
        manifest,
    })
}

/// Work out what a restore would do, touching nothing.
///
/// Everything here is derived from the archive plus the current state of the
/// destination, so the prediction is about *this* machine — a symlink is
/// reported as dangling because its target is absent here, not because it was
/// absent where the pack was made.
///
/// The archive's paths were checked by the caller, before this and before a
/// real restore alike, so a dry run refuses exactly the archives a restore
/// would refuse rather than describing an operation that would abort.
fn predict(
    opts: &RestoreOptions,
    checked: &Checked<'_>,
    manifest: &Manifest,
    destination_exists: bool,
) -> Result<RestoreReport, PackError> {
    let dest = resolve_dest(&opts.dest);
    let payload = crate::inspect::scan_payload(&opts.archive)?;
    let payload_set: BTreeSet<&str> = payload.paths.iter().map(|s| s.as_str()).collect();
    let hard_links_not_created = payload
        .hard_links
        .iter()
        .map(|(rel, target)| HardLinkRecord::new(&dest, Path::new(rel), target))
        .collect();

    let (would_overwrite, would_remain) = if destination_exists {
        compare_destination(&dest, &payload_set)
    } else {
        (Vec::new(), Vec::new())
    };

    let plan = plan_worktree_pointers(&dest, checked, manifest, false);
    let rewritten_worktrees = plan.pairs.iter().map(|p| p.name.clone()).collect();

    let dangling_symlinks = manifest
        .symlinks
        .iter()
        .filter(|s| would_dangle(&dest, s, &payload_set))
        .cloned()
        .collect();

    let link_reports_suppressed = manifest.no_link_report_applied.clone();

    Ok(RestoreReport {
        dest,
        dry_run: true,
        entries_written: payload.paths.len() as u64,
        destination_exists,
        would_overwrite,
        would_remain,
        rewritten_worktrees,
        missing_worktrees: plan.missing,
        conflicting_worktrees: plan.conflicted,
        missing_worktree_parent: plan.missing_parent,
        dangling_symlinks,
        link_reports_suppressed,
        regenerable_caches: manifest.skipped_cache.clone(),
        secrets_not_carried: manifest.skipped_secret.clone(),
        hard_links_not_created,
        manifest: manifest.clone(),
    })
}

/// Settle on the destination path, whether or not it exists yet.
///
/// A restore creates the directory and then canonicalizes it, which resolves
/// every symlink on the way and makes the path absolute. A dry run must not
/// create anything, so it used to canonicalize a directory that was not there,
/// fail, and keep the path as the caller typed it. The two then answered
/// different questions: a relative destination, or one under a symlinked
/// parent, put the prediction's idea of "beside the root" somewhere the restore
/// would never look, and the worktree wiring it forecast was not the wiring that
/// would happen.
///
/// So resolve as far as the filesystem allows and no further: canonicalize the
/// nearest ancestor that exists, then re-attach the part that does not. Those
/// missing components can only become real directories, never symlinks, so this
/// is the same path a restore arrives at after creating them — reached without
/// creating anything.
fn resolve_dest(dest: &Path) -> PathBuf {
    let absolute = std::path::absolute(dest).unwrap_or_else(|_| dest.to_path_buf());

    let mut missing = Vec::new();
    let mut cursor = absolute.as_path();
    loop {
        if let Ok(existing) = std::fs::canonicalize(cursor) {
            let mut resolved = existing;
            resolved.extend(missing.iter().rev());
            return resolved;
        }
        // Nothing exists all the way up, or the path ran out of names to
        // strip: the lexically absolute form is the best answer available.
        let (Some(parent), Some(name)) = (cursor.parent(), cursor.file_name()) else {
            return absolute;
        };
        missing.push(name.to_os_string());
        cursor = parent;
    }
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

/// What a restore does with an archive entry, decided by its type alone.
///
/// This crate's writer emits three shapes — directories, regular files,
/// symlinks — so every other type in `tar::EntryType` came from another
/// producer. Naming the decision as an enum puts it in one exhaustive match:
/// the type is `#[non_exhaustive]`, and tar's own default for a type it does
/// not recognize is to write it out as a regular file, which turns "I do not
/// know what this is" into a write. Here an unrecognized type has to be given
/// a case before it can reach the filesystem.
pub(crate) enum EntryPlan {
    /// Written out as it stands.
    Extract,
    /// Reported and not created — see [`HardLinkRecord`].
    HardLink,
    /// Refused, carrying the words the error and the report use for it.
    Refuse(&'static str),
}

/// Classify one archive entry.
pub(crate) fn entry_plan(kind: tar::EntryType) -> EntryPlan {
    use tar::EntryType as T;

    match kind {
        // `Continuous` is a high-performance variant of a regular file, and
        // every reader treats it as one.
        T::Regular | T::Continuous | T::Directory | T::Symlink => EntryPlan::Extract,
        T::Link => EntryPlan::HardLink,
        T::Char => EntryPlan::Refuse("character device"),
        T::Block => EntryPlan::Refuse("block device"),
        T::Fifo => EntryPlan::Refuse("named pipe"),
        T::GNUSparse => EntryPlan::Refuse("sparse file"),
        // tar folds these into the entry they describe and never yields one on
        // its own, so reaching here means a malformed header.
        T::GNULongName | T::GNULongLink | T::XHeader | T::XGlobalHeader => {
            EntryPlan::Refuse("stray extension header")
        }
        _ => EntryPlan::Refuse("entry of an unrecognized type"),
    }
}

/// Extract every `payload/` entry into `dest`, stripping the prefix.
///
/// Returns the number of entries written and the hard links that were not.
fn unpack_payload(archive: &Path, dest: &Path) -> Result<(u64, Vec<HardLinkRecord>), PackError> {
    let file = File::open(archive)?;
    let decoder = zstd::stream::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);

    // Directories confirmed to be real (not symlinks), so each ancestor is
    // checked once rather than once per entry underneath it.
    let mut real_dirs: HashSet<PathBuf> = HashSet::new();

    let mut written = 0u64;
    let mut hard_links = Vec::new();
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
        // Refuse anything that would escape the destination, and stop: a pack
        // is normally produced by this same crate, but an archive is an
        // untrusted input the moment it arrives from elsewhere, and one that
        // carries such an entry is not trustworthy in what remains either.
        let rel = Contained::entry(rel)?;

        match entry_plan(entry.header().entry_type()) {
            EntryPlan::Extract => {}
            EntryPlan::HardLink => {
                // The target names a file on *this* machine, not one the pack
                // carries, so there is nothing to check it against and no
                // reading of it that makes linking automatically right.
                let target = entry.link_name()?.map(|t| t.display().to_string());
                let Some(target) = target.filter(|t| !t.is_empty()) else {
                    return Err(PackError::UnusableArchiveEntry {
                        path: rel.as_path().display().to_string(),
                        kind: "hard link naming no target".to_string(),
                    });
                };
                hard_links.push(HardLinkRecord::new(dest, rel.as_path(), &target));
                continue;
            }
            EntryPlan::Refuse(kind) => {
                return Err(PackError::UnusableArchiveEntry {
                    path: rel.as_path().display().to_string(),
                    kind: kind.to_string(),
                });
            }
        }

        // The same escape, routed indirectly: an earlier entry plants a
        // symlink and a later one names a path through it, which would land
        // the write wherever the link points. Legitimate packs cannot produce
        // this shape — the scan never descends into a symlinked directory —
        // so it too refuses the archive.
        ensure_real_ancestors(dest, rel.as_path(), &mut real_dirs)?;

        let out = rel.join_onto(dest);
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

    Ok((written, hard_links))
}

/// Refuse an entry whose ancestors include a symlink.
///
/// Writing `a/b/x` when `a/b` is a link puts `x` wherever the link points —
/// outside the destination, if the archive planted the link there first. Every
/// existing ancestor must therefore be a real directory before the entry is
/// written; a missing one is fine, since `create_dir_all` will create it as a
/// real directory.
///
/// Only ancestors confirmed to exist as non-links are cached: a path that did
/// not exist at check time may be a symlink planted by a later entry, so it is
/// re-examined when next seen.
///
/// # Errors
///
/// [`PackError::WriteThroughSymlink`] naming the entry and the link.
fn ensure_real_ancestors(
    dest: &Path,
    rel: &Path,
    real_dirs: &mut HashSet<PathBuf>,
) -> Result<(), PackError> {
    let Some(parent) = rel.parent() else {
        return Ok(());
    };
    let mut cur = dest.to_path_buf();
    for component in parent.components() {
        cur.push(component);
        if real_dirs.contains(&cur) {
            continue;
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(PackError::WriteThroughSymlink {
                    path: rel.display().to_string(),
                    via: cur,
                });
            }
            Ok(_) => {
                real_dirs.insert(cur.clone());
            }
            // Not there yet; created as a real directory just below.
            Err(_) => {}
        }
    }
    Ok(())
}

/// One worktree's wiring, named on both sides.
///
/// The two paths are always written as a pair: writing one without the other
/// leaves the worktree half-attached, which git reports as a broken worktree
/// rather than as no worktree at all.
#[derive(Debug, Clone)]
struct PointerPair {
    /// Worktree name under `.git/worktrees/`.
    name: String,
    /// The repository's admin directory for this worktree.
    admin: PathBuf,
    /// The worktree checkout's own `.git` file.
    dot_git: PathBuf,
}

/// What the restore intends to do about worktree wiring.
#[derive(Debug, Default)]
struct WorktreePlan {
    /// Pairs that can be wired up, in report order.
    pairs: Vec<PointerPair>,
    /// Registered worktrees whose checkout could not be found here.
    missing: Vec<String>,
    /// Pairings stopped because the counterpart's place is occupied by
    /// something that is not this worktree's other half.
    conflicted: Vec<WorktreeConflict>,
    /// Set when this pack is a worktree whose repository is not here.
    missing_parent: Option<String>,
}

/// The archive's claims about where things go, checked before any of them is
/// joined onto the destination.
///
/// Built once, at the top of both a restore and a dry run, so the two cannot
/// disagree about which archives are acceptable — and built *before* the first
/// byte is written, so an archive that lies is refused with the destination
/// still untouched.
struct Checked<'a> {
    /// Registered worktrees, names and locations checked.
    worktrees: Vec<CheckedWorktree<'a>>,
    /// The repository this pack belongs to, when the pack is a worktree.
    origin: Option<CheckedOrigin<'a>>,
}

/// A registered worktree whose archive-supplied paths have been checked.
struct CheckedWorktree<'a> {
    /// Directory name under `.git/worktrees/`.
    name: Contained,
    /// Location under the root, when the checkout travels in this pack.
    ///
    /// `None` means the checkout is elsewhere on the machine; nothing is joined
    /// onto the destination for it, and it is looked for beside the new root
    /// instead.
    path: Option<Contained>,
    /// The record as written, for the fields that are not destination paths.
    record: &'a WorktreeRecord,
}

/// The repository a packed worktree belongs to, its name checked.
struct CheckedOrigin<'a> {
    /// This worktree's name under the repository's `.git/worktrees/`.
    name: Contained,
    /// The record as written.
    origin: &'a WorktreeOrigin,
}

/// Check every path the archive supplies for a place under the destination.
///
/// The payload's entry names were always checked; the manifest's were not, and
/// they are the same kind of claim — `dest.join(record.path)` writes wherever
/// `record.path` says, and `Path::join` hands the whole result to an absolute
/// one. Checking them here rather than at each use is what makes the check
/// impossible to skip: the wiring code below is handed `Contained` values and
/// has no access to the raw strings.
///
/// # Errors
///
/// [`PackError::EscapingManifestPath`] naming the field that carried the path.
fn check_archive_paths(manifest: &Manifest) -> Result<Checked<'_>, PackError> {
    let mut worktrees = Vec::with_capacity(manifest.worktrees.len());
    for record in &manifest.worktrees {
        worktrees.push(CheckedWorktree {
            name: Contained::name("worktrees[].name", &record.name)?,
            path: record
                .path
                .as_deref()
                .map(|raw| Contained::path("worktrees[].path", raw))
                .transpose()?,
            record,
        });
    }

    let origin = match &manifest.worktree_of {
        Some(origin) => Some(CheckedOrigin {
            name: Contained::name("worktree_of.name", &origin.name)?,
            origin,
        }),
        None => None,
    };

    // Symlink paths are joined onto the destination too — the report asks of
    // each one whether it landed dangling, which is a question about a path
    // inside the restored tree and nowhere else.
    for link in &manifest.symlinks {
        Contained::path("symlinks[].path", &link.path)?;
    }

    Ok(Checked { worktrees, origin })
}

/// What the wiring decision came to for one worktree.
///
/// The three layouts used to be three inline branches, and the one that
/// believed both halves were already inside the destination wrote without
/// checking anything — an assumption nothing enforced, since the location came
/// from the manifest. Naming the outcomes makes every branch answer the same
/// question and makes both callers match exhaustively, so a layout added later
/// cannot reach [`apply_worktree_plan`] without a case being written for it.
enum Wiring {
    /// Both pointer files may be written.
    Wire(PointerPair),
    /// The counterpart's place holds something that is not this worktree's
    /// other half. Nothing is written.
    Occupied(WorktreeConflict),
    /// The counterpart is not on this machine.
    Absent,
    /// Nothing to repair here.
    Nothing,
}

/// Decide what worktree wiring this restore can repair, writing nothing.
///
/// Three layouts occur, and all three are decided from the manifest plus what
/// is on disk *outside* the destination — never from the payload, which is why
/// a dry run can answer the same question as the real restore:
///
/// 1. **worktree inside the root** (`.worktrees/<name>`). Both halves travel in
///    this one pack, so the pair is always wireable.
/// 2. **worktree beside the root** — what `git worktree add ../<name>` makes.
///    The halves live in two packs, so this one can only be wired once the
///    other has been restored. The counterpart is looked for at the same offset
///    from the new root that it had from the old one, which is where restoring
///    a set of sibling projects puts it.
/// 3. **the root is itself a worktree**, and its repository is the counterpart.
///    Same lookup, mirrored.
///
/// Cases 2 and 3 make the operation order-independent: whichever pack is
/// restored second finds the first and completes the wiring, and re-restoring
/// with `force` repairs a pair that was incomplete at the time.
fn plan_worktree_pointers(
    dest: &Path,
    checked: &Checked<'_>,
    manifest: &Manifest,
    unpacked: bool,
) -> WorktreePlan {
    let mut plan = WorktreePlan::default();

    for worktree in &checked.worktrees {
        match decide_worktree(dest, worktree, manifest, unpacked) {
            Wiring::Wire(pair) => plan.pairs.push(pair),
            Wiring::Occupied(conflict) => plan.conflicted.push(conflict),
            Wiring::Absent => plan.missing.push(worktree.record.name.clone()),
            Wiring::Nothing => {}
        }
    }

    if let Some(origin) = &checked.origin {
        match decide_origin(dest, origin, manifest, unpacked) {
            Wiring::Wire(pair) => plan.pairs.push(pair),
            Wiring::Occupied(conflict) => plan.conflicted.push(conflict),
            // A repository that is not beside the restored root leaves the
            // checkout with no repository at all, which is worth more than a
            // line in a list of names.
            Wiring::Absent => plan.missing_parent = Some(origin.origin.parent_root.clone()),
            Wiring::Nothing => {}
        }
    }

    plan
}

/// Decide one registered worktree's wiring — layouts 1 and 2.
fn decide_worktree(
    dest: &Path,
    worktree: &CheckedWorktree<'_>,
    manifest: &Manifest,
    unpacked: bool,
) -> Wiring {
    let admin = worktree
        .name
        .join_onto(&dest.join(".git").join("worktrees"));

    if let Some(rel) = &worktree.path {
        // Case 1: both halves are in this pack, so both pointer files are ones
        // this restore just wrote. That is only true of a location that stays
        // inside the destination, which is what `rel` being a `Contained`
        // establishes — an absolute or `..`-bearing location was refused
        // before the payload was touched, rather than aiming these writes at
        // whatever occupies it.
        let worktree_root = rel.join_onto(dest);
        if unpacked && (!admin.is_dir() || !worktree_root.is_dir()) {
            // Neither made it into the payload after all; nothing to repair.
            return Wiring::Nothing;
        }
        return Wiring::Wire(PointerPair {
            name: worktree.record.name.clone(),
            admin,
            dot_git: worktree_root.join(".git"),
        });
    }

    // Case 2: the checkout is not in this pack. It may still be on this
    // machine — restored from its own pack, or never moved at all.
    let Some(candidate) =
        relocate_beside(&manifest.source_root, dest, &worktree.record.source_path)
    else {
        return Wiring::Absent;
    };
    let dot_git = candidate.join(".git");
    if !dot_git.exists() || (unpacked && !admin.is_dir()) {
        return Wiring::Absent;
    }
    // Occupancy alone is not identity: whatever sits at the candidate path
    // has to be confirmed as *this* worktree's checkout before either
    // pointer is written, or the wiring would break someone else's project
    // to repair this one.
    //
    // A `.git` *directory* there is an independent repository. A `.git`
    // file is a worktree checkout of *some* repository — ours only if its
    // pointer names this worktree's admin directory, at the old location
    // (not yet wired) or the new one (already wired; re-wiring is
    // idempotent).
    if dot_git.is_dir() {
        return Wiring::Occupied(WorktreeConflict {
            name: worktree.record.name.clone(),
            path: candidate.display().to_string(),
            found: "an independent repository — its `.git` is a directory".to_string(),
        });
    }
    let old_admin = worktree.name.join_onto(
        &Path::new(&manifest.source_root)
            .join(".git")
            .join("worktrees"),
    );
    match pointer_target(&dot_git) {
        Some(claimed) if same_place(&claimed, &old_admin) || same_place(&claimed, &admin) => {
            Wiring::Wire(PointerPair {
                name: worktree.record.name.clone(),
                admin,
                dot_git,
            })
        }
        Some(claimed) => Wiring::Occupied(WorktreeConflict {
            name: worktree.record.name.clone(),
            path: candidate.display().to_string(),
            found: format!(
                "a worktree of a different repository — its `.git` names {}",
                claimed.display()
            ),
        }),
        None => Wiring::Occupied(WorktreeConflict {
            name: worktree.record.name.clone(),
            path: candidate.display().to_string(),
            found: "an unreadable or unrecognized `.git` file".to_string(),
        }),
    }
}

/// Decide the wiring when this pack *is* a worktree — layout 3.
///
/// The same identity rule as case 2, mirrored: the admin directory found beside
/// the restored root is ours only if its `gitdir` names this checkout, at the
/// old location or the new one.
fn decide_origin(
    dest: &Path,
    origin: &CheckedOrigin<'_>,
    manifest: &Manifest,
    unpacked: bool,
) -> Wiring {
    let dot_git = dest.join(".git");
    let old_dot_git = Path::new(&manifest.source_root).join(".git");
    let admin = relocate_beside(&manifest.source_root, dest, &origin.origin.parent_root)
        .map(|root| origin.name.join_onto(&root.join(".git").join("worktrees")))
        .filter(|admin| admin.is_dir());

    let Some(admin) = admin else {
        return Wiring::Absent;
    };

    match pointer_target(&admin.join("gitdir")) {
        Some(claimed) if same_place(&claimed, &old_dot_git) || same_place(&claimed, &dot_git) => {
            if !unpacked || dot_git.is_file() {
                Wiring::Wire(PointerPair {
                    name: origin.origin.name.clone(),
                    admin,
                    dot_git,
                })
            } else {
                Wiring::Absent
            }
        }
        claimed => Wiring::Occupied(WorktreeConflict {
            name: origin.origin.name.clone(),
            path: admin.display().to_string(),
            found: match claimed {
                Some(other) => format!(
                    "a same-named worktree of a different checkout — its `gitdir` names {}",
                    other.display()
                ),
                None => "an admin directory with no readable `gitdir`".to_string(),
            },
        }),
    }
}

/// Read a worktree pointer file and return the path it names.
///
/// Handles both halves of the wiring: a checkout's `.git` file
/// (`gitdir: <path>`) and an admin directory's `gitdir` file (the bare path).
fn pointer_target(file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let trimmed = text.trim();
    let path = trimmed
        .strip_prefix("gitdir:")
        .map(str::trim)
        .unwrap_or(trimmed);
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// Whether two recorded paths name the same place.
///
/// Resolved against the filesystem where possible, so a path git recorded
/// through a symlinked prefix still matches the canonical form the manifest
/// carries. A path that no longer exists falls back to lexical comparison,
/// which is the best that can be done for a pointer aimed at another machine.
fn same_place(claimed: &Path, expected: &Path) -> bool {
    canonicalize_or(claimed) == canonicalize_or(expected)
}

/// Re-aim an absolute path that sat beside the source root at the restored root.
///
/// `~/projects/proj` restored to `/backup/proj` puts its sibling
/// `~/projects/proj-feature` at `/backup/proj-feature`. Restoring in place is
/// the same computation with a zero delta, so no special case is needed for it.
///
/// Yields `None` for a path that was not under the source root's parent: a
/// worktree kept somewhere unrelated moves independently of the project, and
/// this has no way to know where it went.
fn relocate_beside(source_root: &str, dest: &Path, original: &str) -> Option<PathBuf> {
    let source_parent = Path::new(source_root).parent()?;
    let rel = Path::new(original).strip_prefix(source_parent).ok()?;
    Some(dest.parent()?.join(rel))
}

/// Write both halves of every planned pair.
///
/// Returns the names wired up, which is every pair: a pair that could not be
/// wired was already excluded during planning.
fn apply_worktree_plan(plan: &WorktreePlan) -> Result<Vec<String>, PackError> {
    let mut rewritten = Vec::new();
    for pair in &plan.pairs {
        std::fs::write(
            pair.admin.join("gitdir"),
            format!("{}\n", pair.dot_git.display()),
        )?;
        std::fs::write(&pair.dot_git, format!("gitdir: {}\n", pair.admin.display()))?;
        rewritten.push(pair.name.clone());
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

    // ------------------------------------------------------------------
    // worktrees living beside the root
    //
    // `git worktree add ../name` is the layout in the field, and it splits a
    // project across two packs. Neither pack can be restored into a working
    // state alone; the pair has to find each other.
    // ------------------------------------------------------------------

    /// Build `<base>/proj` with a worktree registered at `<base>/proj-feature`,
    /// wired the way git wires it: two absolute paths pointing at each other.
    fn sibling_worktree(base: &Path) -> (PathBuf, PathBuf) {
        let root = base.join("proj");
        let wt = base.join("proj-feature");
        let admin = root.join(".git/worktrees/feature");

        touch(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(&admin).expect("mkdir");
        fs::write(admin.join("commondir"), "../..\n").expect("write");
        touch(&wt.join("work.txt"), "w");

        // Both halves name absolute paths on this machine.
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .expect("write");
        fs::write(wt.join(".git"), format!("gitdir: {}\n", admin.display())).expect("write");

        (root, wt)
    }

    fn pack(root: &Path, out: &Path) {
        create(&CreateOptions::new(root, out, "0.14.0")).expect("create");
    }

    /// Reading a pointer file back with its trailing newline removed.
    fn pointer(path: &Path) -> String {
        fs::read_to_string(path).expect("read").trim().to_string()
    }

    /// The headline case: a project and its sibling worktree are packed
    /// separately, restored side by side somewhere new, and end up wired to
    /// each other there — not to the machine they came from.
    #[test]
    fn test_restore_wires_sibling_worktree_into_new_location() {
        let dir = TempDir::new().expect("tempdir");
        let src = dir.path().join("projects");
        let (root, wt) = sibling_worktree(&src);

        let root_pack = dir.path().join("proj.pack");
        let wt_pack = dir.path().join("proj-feature.pack");
        pack(&root, &root_pack);
        pack(&wt, &wt_pack);

        // Somewhere entirely new, worktree first.
        let moved = dir.path().join("moved");
        let new_wt = moved.join("proj-feature");
        let new_root = moved.join("proj");

        let wt_report = restore(&RestoreOptions::new(&wt_pack, &new_wt)).expect("restore worktree");
        // Its repository is not here yet, and saying so is the point.
        assert!(wt_report.rewritten_worktrees.is_empty());
        let source_root = fs::canonicalize(&root)
            .expect("canonicalize")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            wt_report.missing_worktree_parent.as_deref(),
            Some(source_root.as_str()),
            "the report must name the repository this checkout belongs to"
        );
        assert!(wt_report.needs_attention());

        // The repository lands beside it, and the pair is wired up.
        let root_report =
            restore(&RestoreOptions::new(&root_pack, &new_root)).expect("restore root");
        assert_eq!(root_report.rewritten_worktrees, vec!["feature".to_string()]);
        assert!(root_report.missing_worktrees.is_empty());

        let real_root = fs::canonicalize(&new_root).expect("canonicalize");
        let real_wt = fs::canonicalize(&new_wt).expect("canonicalize");
        assert_eq!(
            pointer(&real_root.join(".git/worktrees/feature/gitdir")),
            real_wt.join(".git").display().to_string(),
            "the repository must name the worktree where it now is"
        );
        assert_eq!(
            pointer(&real_wt.join(".git")),
            format!(
                "gitdir: {}",
                real_root.join(".git/worktrees/feature").display()
            ),
            "and the worktree must name the repository where it now is"
        );
    }

    /// The same pair, restored repository-first. Whichever pack lands second
    /// completes the wiring, so the operator does not have to know an order.
    #[test]
    fn test_restore_wiring_is_order_independent() {
        let dir = TempDir::new().expect("tempdir");
        let src = dir.path().join("projects");
        let (root, wt) = sibling_worktree(&src);

        let root_pack = dir.path().join("proj.pack");
        let wt_pack = dir.path().join("proj-feature.pack");
        pack(&root, &root_pack);
        pack(&wt, &wt_pack);

        let moved = dir.path().join("moved");
        let new_root = moved.join("proj");
        let new_wt = moved.join("proj-feature");

        // Repository first: the checkout is not here, so it is reported.
        let first = restore(&RestoreOptions::new(&root_pack, &new_root)).expect("restore root");
        assert!(first.rewritten_worktrees.is_empty());
        assert_eq!(first.missing_worktrees, vec!["feature".to_string()]);

        // Checkout second: it finds the repository beside it.
        let second = restore(&RestoreOptions::new(&wt_pack, &new_wt)).expect("restore worktree");
        assert_eq!(second.rewritten_worktrees, vec!["feature".to_string()]);
        assert!(second.missing_worktree_parent.is_none());

        let real_root = fs::canonicalize(&new_root).expect("canonicalize");
        let real_wt = fs::canonicalize(&new_wt).expect("canonicalize");
        assert_eq!(
            pointer(&real_root.join(".git/worktrees/feature/gitdir")),
            real_wt.join(".git").display().to_string()
        );
        assert_eq!(
            pointer(&real_wt.join(".git")),
            format!(
                "gitdir: {}",
                real_root.join(".git/worktrees/feature").display()
            )
        );
    }

    /// Re-restoring the repository over itself once the checkout is in place
    /// repairs the wiring — the operator's way out of having restored in an
    /// order that left it half-attached.
    #[test]
    fn test_forced_re_restore_repairs_existing_sibling() {
        let dir = TempDir::new().expect("tempdir");
        let src = dir.path().join("projects");
        let (root, wt) = sibling_worktree(&src);

        let root_pack = dir.path().join("proj.pack");
        let wt_pack = dir.path().join("proj-feature.pack");
        pack(&root, &root_pack);
        pack(&wt, &wt_pack);

        let moved = dir.path().join("moved");
        let new_root = moved.join("proj");
        restore(&RestoreOptions::new(&root_pack, &new_root)).expect("restore root");
        restore(&RestoreOptions::new(&wt_pack, moved.join("proj-feature")))
            .expect("restore worktree");

        // The repository's own pointer is now stale — it was written before the
        // checkout existed. A forced re-restore is what fixes it.
        let again = restore(&RestoreOptions {
            force: true,
            ..RestoreOptions::new(&root_pack, &new_root)
        })
        .expect("re-restore");

        assert_eq!(again.rewritten_worktrees, vec!["feature".to_string()]);
        assert!(
            again.missing_worktrees.is_empty(),
            "a checkout that is right there must not be reported missing"
        );

        let real_root = fs::canonicalize(&new_root).expect("canonicalize");
        let gitdir = pointer(&real_root.join(".git/worktrees/feature/gitdir"));
        assert!(
            !gitdir.contains("/projects/"),
            "the source machine's path must not survive: {gitdir}"
        );
    }

    /// A checkout that is nowhere on this machine stays reported, not silently
    /// counted as repaired.
    #[test]
    fn test_restore_reports_sibling_worktree_that_is_absent() {
        let dir = TempDir::new().expect("tempdir");
        let src = dir.path().join("projects");
        let (root, _wt) = sibling_worktree(&src);

        let root_pack = dir.path().join("proj.pack");
        pack(&root, &root_pack);

        let report = restore(&RestoreOptions::new(
            &root_pack,
            dir.path().join("elsewhere/proj"),
        ))
        .expect("restore");

        assert_eq!(report.missing_worktrees, vec!["feature".to_string()]);
        assert!(report.rewritten_worktrees.is_empty());
        assert!(report.needs_attention());
    }

    /// An unrelated repository sitting at the path the worktree would occupy is
    /// left alone and reported as a conflict — not as missing, which would
    /// advise restoring a pack on top of it. Its `.git` is a directory, and
    /// overwriting it with a pointer file would destroy a repository to repair
    /// a different one.
    #[test]
    fn test_restore_will_not_clobber_a_repository_at_the_sibling_path() {
        let dir = TempDir::new().expect("tempdir");
        let src = dir.path().join("projects");
        let (root, _wt) = sibling_worktree(&src);

        let root_pack = dir.path().join("proj.pack");
        pack(&root, &root_pack);

        // A real repository, not a worktree, where the worktree used to be.
        let moved = dir.path().join("moved");
        let squatter = moved.join("proj-feature");
        touch(&squatter.join(".git/HEAD"), "ref: refs/heads/main\n");

        let report =
            restore(&RestoreOptions::new(&root_pack, moved.join("proj"))).expect("restore");

        assert!(report.rewritten_worktrees.is_empty());
        assert!(report.missing_worktrees.is_empty());
        assert_eq!(report.conflicting_worktrees.len(), 1);
        let conflict = &report.conflicting_worktrees[0];
        assert_eq!(conflict.name, "feature");
        assert!(
            conflict.found.contains("independent repository"),
            "the report must say what is sitting there, got {:?}",
            conflict.found
        );
        assert!(report.needs_attention());
        assert!(
            squatter.join(".git").is_dir(),
            "the unrelated repository must survive untouched"
        );
    }

    /// A same-named worktree belonging to a *different* repository at the
    /// counterpart path is not wired: its `.git` names someone else's admin
    /// directory, and rewriting it would hijack that repository's worktree.
    #[test]
    fn test_restore_will_not_rewire_a_foreign_worktree_at_the_sibling_path() {
        let dir = TempDir::new().expect("tempdir");
        let src = dir.path().join("projects");
        let (root, _wt) = sibling_worktree(&src);

        let root_pack = dir.path().join("proj.pack");
        pack(&root, &root_pack);

        // A worktree checkout of another repository, where ours would be.
        let other_admin = dir.path().join("other/.git/worktrees/feature");
        fs::create_dir_all(&other_admin).expect("mkdir");
        let moved = dir.path().join("moved");
        let squatter = moved.join("proj-feature");
        let original_pointer = format!("gitdir: {}\n", other_admin.display());
        touch(&squatter.join(".git"), &original_pointer);

        let report =
            restore(&RestoreOptions::new(&root_pack, moved.join("proj"))).expect("restore");

        assert!(report.rewritten_worktrees.is_empty());
        assert_eq!(report.conflicting_worktrees.len(), 1);
        assert!(
            report.conflicting_worktrees[0]
                .found
                .contains("different repository"),
            "got {:?}",
            report.conflicting_worktrees[0].found
        );
        assert_eq!(
            fs::read_to_string(squatter.join(".git")).expect("read"),
            original_pointer,
            "the foreign worktree's pointer must survive untouched"
        );
    }

    /// The mirrored collision: this pack is a worktree, and the repository
    /// found beside it has a same-named worktree that belongs to a different
    /// checkout. Its admin `gitdir` is not overwritten.
    #[test]
    fn test_restore_will_not_claim_a_foreign_admin_directory() {
        let dir = TempDir::new().expect("tempdir");
        let src = dir.path().join("projects");
        let (_root, wt) = sibling_worktree(&src);

        let wt_pack = dir.path().join("proj-feature.pack");
        pack(&wt, &wt_pack);

        // At the destination, `proj` is a different repository that happens to
        // have its own worktree named `feature`, checked out somewhere else.
        let moved = dir.path().join("moved");
        let foreign_admin = moved.join("proj/.git/worktrees/feature");
        fs::create_dir_all(&foreign_admin).expect("mkdir");
        let elsewhere = dir.path().join("elsewhere/checkout");
        fs::create_dir_all(&elsewhere).expect("mkdir");
        let original_gitdir = format!("{}\n", elsewhere.join(".git").display());
        fs::write(foreign_admin.join("gitdir"), &original_gitdir).expect("write");

        let report =
            restore(&RestoreOptions::new(&wt_pack, moved.join("proj-feature"))).expect("restore");

        assert!(report.rewritten_worktrees.is_empty());
        assert_eq!(report.conflicting_worktrees.len(), 1);
        assert!(
            report.conflicting_worktrees[0]
                .found
                .contains("different checkout"),
            "got {:?}",
            report.conflicting_worktrees[0].found
        );
        assert!(report.missing_worktree_parent.is_none());
        assert_eq!(
            fs::read_to_string(foreign_admin.join("gitdir")).expect("read"),
            original_gitdir,
            "the foreign admin directory must survive untouched"
        );
    }

    /// A worktree kept somewhere unrelated to the project moves independently,
    /// and this has no way to know where. It is reported, never guessed at.
    #[test]
    fn test_restore_does_not_guess_at_a_distant_worktree() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("projects/proj");
        let far = dir.path().join("somewhere/else/wt");
        let admin = root.join(".git/worktrees/far");

        touch(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        fs::create_dir_all(&admin).expect("mkdir");
        touch(&far.join(".keep"), "");
        fs::write(
            admin.join("gitdir"),
            format!("{}\n", far.join(".git").display()),
        )
        .expect("write");
        fs::write(far.join(".git"), format!("gitdir: {}\n", admin.display())).expect("write");

        let out = dir.path().join("proj.pack");
        pack(&root, &out);

        let report =
            restore(&RestoreOptions::new(&out, dir.path().join("moved/proj"))).expect("restore");

        assert_eq!(report.missing_worktrees, vec!["far".to_string()]);
        assert!(report.rewritten_worktrees.is_empty());
    }

    /// The dry run predicts the sibling wiring, and the real restore agrees.
    #[test]
    fn test_dry_run_predicts_sibling_wiring() {
        let dir = TempDir::new().expect("tempdir");
        let src = dir.path().join("projects");
        let (root, wt) = sibling_worktree(&src);

        let root_pack = dir.path().join("proj.pack");
        let wt_pack = dir.path().join("proj-feature.pack");
        pack(&root, &root_pack);
        pack(&wt, &wt_pack);

        let moved = dir.path().join("moved");
        restore(&RestoreOptions::new(&wt_pack, moved.join("proj-feature")))
            .expect("restore worktree");

        let new_root = moved.join("proj");
        let predicted = restore(&RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&root_pack, &new_root)
        })
        .expect("dry run");

        assert_eq!(predicted.rewritten_worktrees, vec!["feature".to_string()]);
        assert!(!new_root.exists(), "still nothing written");

        let actual = restore(&RestoreOptions::new(&root_pack, &new_root)).expect("restore");
        assert_eq!(predicted.rewritten_worktrees, actual.rewritten_worktrees);
        assert_eq!(predicted.missing_worktrees, actual.missing_worktrees);
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

    // ------------------------------------------------------------------
    // crafted archives
    //
    // A pack is normally produced by this crate, but an archive is an untrusted
    // input the moment it arrives from elsewhere. These build the bytes by hand.
    // ------------------------------------------------------------------

    /// The smallest manifest that parses, for archives built by hand.
    const BARE_MANIFEST: &str = "\
format_version = 2
created_at = \"2026-08-10T00:00:00Z\"
source_root = \"/tmp/proj\"
project_name = \"proj\"
lds_version = \"0.15.0\"

[stats]
file_count = 0
symlink_count = 0
total_bytes = 0
";

    /// Write a hand-built archive: a v2 manifest followed by the given
    /// `(builder)` entries, all under the payload prefix already.
    fn craft_archive(path: &Path, add_entries: impl FnOnce(&mut tar::Builder<Vec<u8>>)) {
        craft_archive_with_manifest(path, BARE_MANIFEST, add_entries);
    }

    /// The same, with the manifest written out by the caller — which is how an
    /// archive lies about where its worktrees go.
    fn craft_archive_with_manifest(
        path: &Path,
        manifest: &str,
        add_entries: impl FnOnce(&mut tar::Builder<Vec<u8>>),
    ) {
        let mut tar = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(manifest.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, "pack.toml", manifest.as_bytes())
            .expect("manifest entry");
        add_entries(&mut tar);
        let uncompressed = tar.into_inner().expect("finish tar");

        let file = File::create(path).expect("create archive");
        let mut encoder = zstd::stream::Encoder::new(file, 3).expect("zstd");
        std::io::Write::write_all(&mut encoder, &uncompressed).expect("write");
        encoder.finish().expect("finish zstd");
    }

    // NB: a `..` entry cannot be produced here — `tar::Builder` refuses to
    // write one — so the `PackError::EscapingArchivePath` guard is exercised
    // only against archives from other producers. The guard itself is a plain
    // component scan over `rel`; the symlink route below is the one a crafted
    // archive can actually reach through this crate's own writer.

    /// A file entry routed through an archive-planted symlink is refused, and
    /// nothing is written outside the destination.
    #[cfg(unix)]
    #[test]
    fn test_restore_refuses_write_through_planted_symlink() {
        let dir = TempDir::new().expect("tempdir");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).expect("mkdir");

        let archive = dir.path().join("evil.pack");
        let outside_for_closure = outside.clone();
        craft_archive(&archive, |tar| {
            // payload/link -> <outside>
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            h.set_cksum();
            tar.append_link(&mut h, "payload/link", &outside_for_closure)
                .expect("symlink entry");
            // payload/link/evil.txt — through the link
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, "payload/link/evil.txt", &b"pwnd"[..])
                .expect("file entry");
        });

        let dest = dir.path().join("dest");
        let err = restore(&RestoreOptions::new(&archive, &dest)).expect_err("must refuse");
        assert!(
            matches!(err, PackError::WriteThroughSymlink { .. }),
            "got {err:?}"
        );
        assert!(
            !outside.join("evil.txt").exists(),
            "the write must not have escaped through the link"
        );
    }

    /// A manifest that puts a worktree outside the destination is refused, and
    /// the file it aimed at is untouched.
    ///
    /// The entry names in the payload were always checked; this is the other
    /// half of the archive making the same claim. `dest.join(path)` with an
    /// absolute `path` discards the destination entirely, so the wiring would
    /// have written `gitdir: …` over another project's `.git`.
    #[test]
    fn test_restore_refuses_worktree_path_pointing_outside() {
        let dir = TempDir::new().expect("tempdir");
        let victim = dir.path().join("victim");
        fs::create_dir_all(&victim).expect("mkdir");
        let victim_git = victim.join(".git");
        fs::write(&victim_git, "gitdir: /somewhere/real\n").expect("write");

        let manifest = format!(
            "{BARE_MANIFEST}
[[worktrees]]
name = \"feature\"
path = \"{}\"
source_path = \"/tmp/proj/.worktrees/feature\"
included = true
",
            victim.display()
        );

        let archive = dir.path().join("evil.pack");
        craft_archive_with_manifest(&archive, &manifest, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, "payload/a.txt", &b"a"[..])
                .expect("file entry");
        });

        let dest = dir.path().join("dest");
        let err = restore(&RestoreOptions::new(&archive, &dest)).expect_err("must refuse");
        assert!(
            matches!(err, PackError::EscapingManifestPath { ref field, .. } if field == "worktrees[].path"),
            "got {err:?}"
        );
        assert_eq!(
            fs::read_to_string(&victim_git).expect("read"),
            "gitdir: /somewhere/real\n",
            "the other project's pointer must be exactly as it was"
        );
        assert!(
            !dest.exists(),
            "the manifest is checked before the payload is touched"
        );
    }

    /// A worktree name is one directory under `.git/worktrees/`; one that walks
    /// out of there is refused.
    #[test]
    fn test_restore_refuses_worktree_name_pointing_outside() {
        let dir = TempDir::new().expect("tempdir");
        let manifest = format!(
            "{BARE_MANIFEST}
[[worktrees]]
name = \"../../../escape\"
path = \".worktrees/feature\"
source_path = \"/tmp/proj/.worktrees/feature\"
included = true
"
        );

        let archive = dir.path().join("evil.pack");
        craft_archive_with_manifest(&archive, &manifest, |_| {});

        let dest = dir.path().join("dest");
        let err = restore(&RestoreOptions::new(&archive, &dest)).expect_err("must refuse");
        assert!(
            matches!(err, PackError::EscapingManifestPath { ref field, .. } if field == "worktrees[].name"),
            "got {err:?}"
        );
    }

    /// A dry run refuses the same archive a restore would, rather than
    /// describing an operation that is going to abort.
    #[test]
    fn test_dry_run_refuses_what_restore_refuses() {
        let dir = TempDir::new().expect("tempdir");
        let manifest = format!(
            "{BARE_MANIFEST}
[[worktrees]]
name = \"feature\"
path = \"../outside\"
source_path = \"/tmp/proj/.worktrees/feature\"
included = true
"
        );

        let archive = dir.path().join("evil.pack");
        craft_archive_with_manifest(&archive, &manifest, |_| {});

        let err = restore(&RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&archive, dir.path().join("dest"))
        })
        .expect_err("must refuse");
        assert!(
            matches!(err, PackError::EscapingManifestPath { .. }),
            "got {err:?}"
        );
    }

    /// A hard link is reported with the command that would create it, and is
    /// not created.
    ///
    /// The target names a file on this machine that the pack does not carry, so
    /// there is nothing to check it against; creating the link would publish
    /// that file into the restored tree under a name the archive chose.
    #[cfg(unix)]
    #[test]
    fn test_restore_reports_hard_link_without_creating_it() {
        let dir = TempDir::new().expect("tempdir");
        let secret = dir.path().join("private.key");
        fs::write(&secret, "PRIVATE").expect("write");

        let archive = dir.path().join("linky.pack");
        let secret_for_closure = secret.clone();
        craft_archive(&archive, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_link(&mut h, "payload/borrowed", &secret_for_closure)
                .expect("hard link entry");
        });

        let dest = dir.path().join("dest");
        let report = restore(&RestoreOptions::new(&archive, &dest)).expect("restore");

        assert!(
            !dest.join("borrowed").exists(),
            "the link must not have been created"
        );
        assert_eq!(report.hard_links_not_created.len(), 1);
        let link = &report.hard_links_not_created[0];
        assert_eq!(link.path, "borrowed");
        assert_eq!(link.target, secret.display().to_string());
        assert!(
            link.command.starts_with("ln '"),
            "the report has to carry a runnable command, got {}",
            link.command
        );
        assert!(link.command.contains(&secret.display().to_string()));
        assert!(
            report.needs_attention(),
            "a path the archive listed and the restore did not create is not silent"
        );
    }

    /// A hard link naming another entry in the same pack — what a tar writer
    /// produces for a project that contains hard links — gets a command that
    /// points at where that entry landed, not at the archive-relative path.
    ///
    /// Left as written it would read `ln 'payload/b.txt' …`, which resolves
    /// against whatever directory the operator happens to be in.
    #[cfg(unix)]
    #[test]
    fn test_hard_link_into_the_payload_resolves_to_the_restored_file() {
        let dir = TempDir::new().expect("tempdir");
        let archive = dir.path().join("linky.pack");
        craft_archive(&archive, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_size(2);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, "payload/b.txt", &b"b\n"[..])
                .expect("file entry");

            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_link(&mut h, "payload/a.txt", "payload/b.txt")
                .expect("hard link entry");
        });

        let dest = dir.path().join("dest");
        let report = restore(&RestoreOptions::new(&archive, &dest)).expect("restore");

        let link = &report.hard_links_not_created[0];
        assert_eq!(
            link.target, "payload/b.txt",
            "the target is reported as the archive wrote it"
        );
        assert_eq!(
            link.command,
            format!(
                "ln {} {}",
                shell_quote(&report.dest.join("b.txt").display().to_string()),
                shell_quote(&report.dest.join("a.txt").display().to_string())
            ),
            "but the command has to name the file that actually landed"
        );
        assert!(dest.join("b.txt").is_file(), "the real entry is restored");
        assert!(!dest.join("a.txt").exists());
    }

    /// A target that is not in the payload is left exactly as the archive wrote
    /// it — rewriting it would invent a claim the archive did not make.
    #[cfg(unix)]
    #[test]
    fn test_hard_link_outside_the_payload_keeps_its_target() {
        let dir = TempDir::new().expect("tempdir");
        let archive = dir.path().join("linky.pack");
        craft_archive(&archive, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_link(&mut h, "payload/borrowed", "/etc/hosts")
                .expect("hard link entry");
        });

        let dest = dir.path().join("dest");
        let report = restore(&RestoreOptions::new(&archive, &dest)).expect("restore");

        let link = &report.hard_links_not_created[0];
        assert_eq!(link.target, "/etc/hosts");
        assert!(
            link.command.contains("'/etc/hosts'"),
            "got {}",
            link.command
        );
    }

    /// The dry run names hard links too — a prediction that omitted them would
    /// promise a file the restore is not going to create.
    #[cfg(unix)]
    #[test]
    fn test_dry_run_reports_hard_link() {
        let dir = TempDir::new().expect("tempdir");
        let target = dir.path().join("elsewhere.txt");
        fs::write(&target, "x").expect("write");

        let archive = dir.path().join("linky.pack");
        let target_for_closure = target.clone();
        craft_archive(&archive, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_link(&mut h, "payload/borrowed", &target_for_closure)
                .expect("hard link entry");
        });

        let dest = dir.path().join("dest");
        let predicted = restore(&RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&archive, &dest)
        })
        .expect("dry run");

        assert_eq!(predicted.hard_links_not_created.len(), 1);
        assert_eq!(predicted.hard_links_not_created[0].path, "borrowed");
        assert_eq!(
            predicted.entries_written, 0,
            "a hard link is not an entry that will be written"
        );
        assert!(!dest.exists());
    }

    /// An entry type this crate never writes and a restore should not
    /// materialize refuses the archive rather than falling through to tar's
    /// "unrecognized means regular file" default.
    #[test]
    fn test_restore_refuses_device_entry() {
        let dir = TempDir::new().expect("tempdir");
        let archive = dir.path().join("odd.pack");
        craft_archive(&archive, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Fifo);
            h.set_size(0);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append_data(&mut h, "payload/pipe", &b""[..])
                .expect("fifo entry");
        });

        let dest = dir.path().join("dest");
        let err = restore(&RestoreOptions::new(&archive, &dest)).expect_err("must refuse");
        assert!(
            matches!(err, PackError::UnusableArchiveEntry { ref kind, .. } if kind == "named pipe"),
            "got {err:?}"
        );
    }

    /// A dry run and the restore it predicts agree on where the destination is,
    /// even when the destination does not exist yet and its parent is a link.
    ///
    /// The prediction used to canonicalize a directory that was not there, fail,
    /// and keep the path as written — so it looked for sibling worktrees beside
    /// a path the restore would never use.
    #[cfg(unix)]
    #[test]
    fn test_dry_run_and_restore_agree_on_the_destination() {
        let dir = TempDir::new().expect("tempdir");
        let real = dir.path().join("real");
        fs::create_dir_all(&real).expect("mkdir");
        let via_link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &via_link).expect("symlink");

        let root = dir.path().join("proj");
        touch(&root.join("a.txt"), "a");
        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        // Through the link, into a directory that does not exist yet.
        let dest = via_link.join("restored");
        let predicted = restore(&RestoreOptions {
            dry_run: true,
            ..RestoreOptions::new(&out, &dest)
        })
        .expect("dry run");

        // The shape the fix addresses, asserted while it still holds:
        // canonicalizing a directory that is not there fails, and the old
        // fallback was the path exactly as the caller typed it.
        assert!(fs::canonicalize(&dest).is_err());
        assert_ne!(
            predicted.dest, dest,
            "the prediction has to resolve past the path as typed"
        );

        let actual = restore(&RestoreOptions::new(&out, &dest)).expect("restore");

        assert_eq!(
            predicted.dest, actual.dest,
            "a prediction about another directory is not a prediction"
        );
        assert_eq!(
            actual.dest,
            fs::canonicalize(&real)
                .expect("canonicalize")
                .join("restored"),
            "both must resolve through the link"
        );
    }

    /// A manifest larger than the read limit refuses the archive instead of
    /// allocating whatever the archive asked for.
    #[test]
    fn test_inspect_refuses_an_oversized_manifest() {
        let dir = TempDir::new().expect("tempdir");
        let archive = dir.path().join("bomb.pack");

        // Compresses to almost nothing and expands past the limit — the shape
        // of the problem, in miniature.
        let bloat = "# ".repeat(40 * 1024 * 1024);
        let manifest = format!("{BARE_MANIFEST}{bloat}");
        craft_archive_with_manifest(&archive, &manifest, |_| {});

        let err =
            restore(&RestoreOptions::new(&archive, dir.path().join("dest"))).expect_err("refuse");
        assert!(
            matches!(err, PackError::ManifestTooLarge { .. }),
            "got {err:?}"
        );
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
