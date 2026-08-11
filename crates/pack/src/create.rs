//! Archive creation: classify the project, then write `pack.toml` followed by
//! the payload into a zstd-compressed tar stream.
//!
//! The `.git` directory is copied wholesale rather than converted to a
//! `git bundle`. A bundle is built from an explicit set of refs, so everything
//! outside that set — stashes, the reflog, dangling objects a rebase left
//! behind — silently does not make the trip. Copying the directory removes the
//! question entirely: whatever git had, the pack has.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::error::PackError;
use crate::manifest::{MANIFEST_NAME, Manifest, PACK_FORMAT_VERSION, Stats};
use crate::rules::PackRules;
use crate::scan::{self, Entry, EntryKind, Scan};

/// Directory prefix under which payload entries are stored inside the archive.
///
/// Keeping the payload under its own prefix means a project that happens to
/// contain a top-level `pack.toml` cannot collide with the manifest.
pub const PAYLOAD_PREFIX: &str = "payload";

/// Default zstd compression level.
///
/// Level 3 is zstd's own default and sits at the knee of the curve: most of the
/// ratio, a small fraction of the time of the high levels. A pack is dominated
/// by already-compressed git objects, so spending longer buys little.
pub const DEFAULT_COMPRESSION_LEVEL: i32 = 3;

/// Inputs for [`create`].
#[derive(Debug, Clone)]
pub struct CreateOptions {
    /// Project root to pack.
    pub root: PathBuf,
    /// Destination archive path.
    pub out: PathBuf,
    /// Version string recorded in the manifest.
    pub lds_version: String,
    /// zstd compression level.
    pub compression_level: i32,
    /// Which names count as secrets and caches.
    pub rules: PackRules,
    /// Classify and build the manifest, but write no archive.
    ///
    /// The point of a dry run is to answer "what would travel, and what would
    /// be left behind?" — particularly after editing `[pack]` in config —
    /// without producing a file that then has to be cleaned up.
    pub dry_run: bool,
}

impl CreateOptions {
    /// Build options with the default compression level and rules.
    pub fn new(root: impl Into<PathBuf>, out: impl Into<PathBuf>, lds_version: &str) -> Self {
        Self {
            root: root.into(),
            out: out.into(),
            lds_version: lds_version.to_string(),
            compression_level: DEFAULT_COMPRESSION_LEVEL,
            rules: PackRules::default(),
            dry_run: false,
        }
    }
}

/// What [`create`] produced.
#[derive(Debug, Clone)]
pub struct CreateReport {
    /// Manifest embedded in the archive (or that would have been, on a dry run).
    pub manifest: Manifest,
    /// Path of the written archive, or where one would have been written.
    pub out_path: PathBuf,
    /// Size of the archive on disk, after compression. `0` on a dry run.
    pub compressed_bytes: u64,
    /// Whether this was a dry run, in which case nothing was written.
    pub dry_run: bool,
}

/// Pack a project root into a single archive.
///
/// # Arguments
///
/// * `opts` — Source root, destination path, and compression level.
///
/// # Returns
///
/// A [`CreateReport`] carrying the manifest — including everything that was
/// deliberately skipped — so the caller can report it without re-reading the
/// archive.
///
/// # Errors
///
/// - [`PackError::NotADirectory`] if the root is not a directory.
/// - [`PackError::Io`] on any read or write failure.
/// - [`PackError::ManifestSerialize`] if the manifest cannot be serialized.
pub fn create(opts: &CreateOptions) -> Result<CreateReport, PackError> {
    let root = canonical_or_self(&opts.root);
    let scanned = scan::scan_with(&root, &opts.rules)?;

    let manifest = build_manifest(&root, &scanned, &opts.lds_version);
    let manifest_toml = manifest.to_toml()?;

    if opts.dry_run {
        // Everything the caller needs to decide is in the manifest; stop
        // before the first byte is written.
        return Ok(CreateReport {
            manifest,
            out_path: opts.out.clone(),
            compressed_bytes: 0,
            dry_run: true,
        });
    }

    if let Some(parent) = opts.out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(&opts.out)?;
    let encoder = zstd::stream::Encoder::new(file, opts.compression_level)?;
    let mut builder = tar::Builder::new(encoder);
    // Symlinks are stored as links. Following them would pull whole unrelated
    // trees into the archive along with whatever they happen to contain.
    builder.follow_symlinks(false);

    write_manifest_entry(&mut builder, &manifest_toml)?;
    for entry in &scanned.entries {
        write_entry(&mut builder, entry)?;
    }

    let encoder = builder.into_inner()?;
    let mut file = encoder.finish()?;
    file.flush()?;

    let compressed_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);

    Ok(CreateReport {
        manifest,
        out_path: opts.out.clone(),
        compressed_bytes,
        dry_run: false,
    })
}

/// Assemble the manifest from a completed scan.
fn build_manifest(root: &Path, scanned: &Scan, lds_version: &str) -> Manifest {
    let project_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());

    Manifest {
        format_version: PACK_FORMAT_VERSION,
        created_at: chrono::Utc::now().to_rfc3339(),
        source_root: root.to_string_lossy().into_owned(),
        project_name,
        lds_version: lds_version.to_string(),
        stats: Stats {
            file_count: scanned.file_count(),
            symlink_count: scanned.symlink_count(),
            total_bytes: scanned.total_bytes(),
        },
        claude: scanned.claude.clone(),
        skipped_cache: scanned.skipped_cache.clone(),
        skipped_secret: scanned.skipped_secret.clone(),
        symlinks: scanned.symlinks.clone(),
        worktrees: scanned.worktrees.clone(),
        worktree_of: scanned.worktree_of.clone(),
    }
}

/// Write `pack.toml` as the archive's first entry so it can be read without
/// decompressing the payload.
fn write_manifest_entry<W: Write>(
    builder: &mut tar::Builder<W>,
    manifest_toml: &str,
) -> Result<(), PackError> {
    let bytes = manifest_toml.as_bytes();
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(now_epoch_secs());
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    builder.append_data(&mut header, MANIFEST_NAME, bytes)?;
    Ok(())
}

/// Write one scanned entry into the archive under the payload prefix.
fn write_entry<W: Write>(builder: &mut tar::Builder<W>, entry: &Entry) -> Result<(), PackError> {
    let archive_path = format!("{PAYLOAD_PREFIX}/{}", entry.rel);

    match entry.kind {
        EntryKind::Dir => {
            builder.append_dir(&archive_path, &entry.abs)?;
        }
        EntryKind::File => {
            // A file can vanish between the scan and the write (a build running
            // in parallel, an editor swapping a temp file). Skip it rather than
            // abandoning the whole pack.
            match File::open(&entry.abs) {
                Ok(mut f) => builder.append_file(&archive_path, &mut f)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    tracing::warn!("file vanished during pack, skipping: {}", entry.rel);
                }
                Err(e) => return Err(PackError::Io(e)),
            }
        }
        EntryKind::Symlink => {
            let target = std::fs::read_link(&entry.abs)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o777);
            header.set_mtime(now_epoch_secs());
            header.set_entry_type(tar::EntryType::Symlink);
            builder.append_link(&mut header, &archive_path, &target)?;
        }
    }

    Ok(())
}

/// Seconds since the Unix epoch, saturating at 0 before it.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Canonicalize a path, falling back to the input when it cannot be resolved.
fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inspect;
    use std::fs;
    use tempfile::TempDir;

    fn touch(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, body).expect("write");
    }

    fn sample_project(root: &Path) {
        touch(&root.join("src/main.rs"), "fn main() {}");
        touch(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        touch(&root.join("workspace/journal.md"), "# journal\n");
        touch(&root.join(".env"), "SECRET=1\n");
        touch(&root.join("target/debug/app"), "binary");
    }

    /// A created pack exists, is non-empty, and its manifest is readable
    /// without unpacking the payload.
    #[test]
    fn test_create_writes_readable_archive() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        sample_project(&root);

        let out = dir.path().join("out/proj.pack");
        let report = create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        assert!(out.is_file(), "archive should exist");
        assert!(report.compressed_bytes > 0);
        assert_eq!(report.manifest.project_name, "proj");
        assert_eq!(report.manifest.format_version, PACK_FORMAT_VERSION);

        let read_back = inspect::inspect(&out).expect("inspect");
        assert_eq!(read_back.project_name, "proj");
        assert_eq!(read_back.stats.file_count, report.manifest.stats.file_count);
    }

    /// Secrets are reported in the manifest and absent from the payload.
    #[test]
    fn test_create_reports_but_omits_secrets() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        sample_project(&root);

        let out = dir.path().join("proj.pack");
        let report = create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        assert!(
            report
                .manifest
                .skipped_secret
                .iter()
                .any(|s| s.path == ".env"),
            "secret must be reported"
        );

        let names = crate::inspect::list_payload_paths(&out).expect("list");
        assert!(
            !names.iter().any(|n| n == ".env"),
            "secret must not be in the payload"
        );
        assert!(names.iter().any(|n| n == "src/main.rs"));
        assert!(names.iter().any(|n| n == ".git/HEAD"));
        assert!(!names.iter().any(|n| n.starts_with("target/")));
    }

    /// The destination's parent directory is created when absent.
    #[test]
    fn test_create_makes_parent_directory() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        touch(&root.join("a.txt"), "a");

        let out = dir.path().join("deeply/nested/proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");
        assert!(out.is_file());
    }

    /// A dry run classifies everything but writes no archive.
    #[test]
    fn test_dry_run_writes_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        sample_project(&root);

        let out = dir.path().join("would-be.pack");
        let mut opts = CreateOptions::new(&root, &out, "0.13.3");
        opts.dry_run = true;

        let report = create(&opts).expect("dry run should succeed");

        assert!(report.dry_run);
        assert_eq!(report.compressed_bytes, 0);
        assert!(!out.exists(), "dry run must not write an archive");
        // The classification is still complete, which is the point of asking.
        assert!(report.manifest.stats.file_count > 0);
        assert!(
            report
                .manifest
                .skipped_secret
                .iter()
                .any(|s| s.path == ".env"),
            "a dry run still reports what would be left behind"
        );
    }

    /// A project-specific secret name declared in config is honored end to end.
    #[test]
    fn test_custom_secret_glob_reaches_the_archive() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        touch(&root.join("keep-me.txt"), "content");
        touch(&root.join("my-app-keys.json"), "SECRET");
        touch(&root.join("prod.vault"), "SECRET");

        let out = dir.path().join("proj.pack");
        let mut opts = CreateOptions::new(&root, &out, "0.13.3");
        opts.rules = crate::rules::PackRules::new(&crate::rules::RuleOverrides {
            secret_globs: vec!["my-app-keys.json".to_string(), "*.vault".to_string()],
            ..Default::default()
        })
        .expect("rules compile");

        let report = create(&opts).expect("create");

        let skipped: Vec<&str> = report
            .manifest
            .skipped_secret
            .iter()
            .map(|s| s.path.as_str())
            .collect();
        assert!(skipped.contains(&"my-app-keys.json"));
        assert!(skipped.contains(&"prod.vault"));

        let payload = crate::inspect::list_payload_paths(&out).expect("list");
        assert!(payload.iter().any(|p| p == "keep-me.txt"));
        assert!(!payload.iter().any(|p| p == "my-app-keys.json"));
        assert!(!payload.iter().any(|p| p == "prod.vault"));
    }

    /// `keep` overrides a built-in exclusion end to end.
    #[test]
    fn test_keep_carries_a_builtin_secret() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        touch(&root.join(".npmrc"), "registry=...");

        let out = dir.path().join("proj.pack");
        let mut opts = CreateOptions::new(&root, &out, "0.13.3");
        opts.rules = crate::rules::PackRules::new(&crate::rules::RuleOverrides {
            keep: vec![".npmrc".to_string()],
            ..Default::default()
        })
        .expect("rules compile");

        let report = create(&opts).expect("create");
        assert!(
            report.manifest.skipped_secret.is_empty(),
            "keep must remove it from the skip list"
        );
        let payload = crate::inspect::list_payload_paths(&out).expect("list");
        assert!(payload.iter().any(|p| p == ".npmrc"));
    }

    /// An extra cache directory declared in config is pruned and recorded.
    #[test]
    fn test_custom_cache_dir_is_pruned() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        touch(&root.join("src/main.rs"), "fn main() {}");
        touch(&root.join("dist/bundle.js"), "built");

        let out = dir.path().join("proj.pack");
        let mut opts = CreateOptions::new(&root, &out, "0.13.3");
        opts.rules = crate::rules::PackRules::new(&crate::rules::RuleOverrides {
            cache_dirs: vec!["dist".to_string()],
            ..Default::default()
        })
        .expect("rules compile");

        let report = create(&opts).expect("create");
        assert!(
            report
                .manifest
                .skipped_cache
                .iter()
                .any(|c| c.path == "dist"),
            "custom cache must be recorded"
        );
        let payload = crate::inspect::list_payload_paths(&out).expect("list");
        assert!(!payload.iter().any(|p| p.starts_with("dist")));
        assert!(payload.iter().any(|p| p == "src/main.rs"));
    }

    /// Without that config, `dist/` is packed — it is source in many projects.
    #[test]
    fn test_dist_is_packed_by_default() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).expect("mkdir");
        touch(&root.join("dist/hand-written.js"), "source");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let payload = crate::inspect::list_payload_paths(&out).expect("list");
        assert!(payload.iter().any(|p| p == "dist/hand-written.js"));
    }

    /// Packing a path that is not a directory is an error.
    #[test]
    fn test_create_rejects_non_directory() {
        let dir = TempDir::new().expect("tempdir");
        let file = dir.path().join("f.txt");
        touch(&file, "x");
        let out = dir.path().join("o.pack");
        assert!(matches!(
            create(&CreateOptions::new(&file, &out, "0.13.3")),
            Err(PackError::NotADirectory(_))
        ));
    }
}
