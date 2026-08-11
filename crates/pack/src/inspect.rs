//! Read a pack's manifest without unpacking it.
//!
//! `pack.toml` is written as the archive's first entry, so answering "what is
//! in this pack, and what did it leave behind?" costs one small read rather
//! than a full decompression.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::create::PAYLOAD_PREFIX;
use crate::error::PackError;
use crate::manifest::{MANIFEST_NAME, Manifest, PACK_FORMAT_VERSION};

/// How much manifest this build will read into memory.
///
/// The manifest is decompressed before it can be parsed, and an archive decides
/// both how much it declares and how much it actually carries. Compressible
/// filler expands enormously — a few megabytes of zeroes become gigabytes — so
/// reading "the whole first entry" hands the archive control of how much memory
/// the process allocates, and one small file can end it.
///
/// The manifest records what was left behind rather than every packed file:
/// skipped secrets and caches, symlinks, worktrees. Sixty-four mebibytes is far
/// past what a real project produces — a hundred thousand records is a few tens
/// of megabytes — and still a bound.
const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;

/// Read the manifest from a pack.
///
/// # Arguments
///
/// * `archive` — Path to a `.pack` file.
///
/// # Returns
///
/// The embedded [`Manifest`].
///
/// # Errors
///
/// - [`PackError::Io`] if the file cannot be read or is not a valid zstd/tar stream.
/// - [`PackError::MissingManifest`] if the archive contains no `pack.toml`.
/// - [`PackError::ManifestTooLarge`] if the manifest exceeds
///   [`MAX_MANIFEST_BYTES`] — the archive is read before it is trusted, so how
///   much of it reaches memory cannot be the archive's decision.
/// - [`PackError::ManifestParse`] if the manifest is malformed.
pub fn inspect(archive: &Path) -> Result<Manifest, PackError> {
    let file = File::open(archive)?;
    let decoder = zstd::stream::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);

    // Only the first entry is examined: the manifest is written first, so
    // anything else there means this is not one of our archives and scanning
    // the rest of the payload would tell us nothing.
    if let Some(first) = tar.entries()?.next() {
        let mut entry = first?;
        let path = entry.path()?.to_path_buf();
        if path.as_os_str() == MANIFEST_NAME {
            let mut text = String::new();
            // One byte past the limit, so an over-long manifest is detected by
            // having read it rather than by believing the declared size.
            let read = entry
                .by_ref()
                .take(MAX_MANIFEST_BYTES + 1)
                .read_to_string(&mut text)? as u64;
            if read > MAX_MANIFEST_BYTES {
                return Err(PackError::ManifestTooLarge {
                    limit: MAX_MANIFEST_BYTES,
                });
            }
            return Ok(Manifest::from_toml(&text)?);
        }
    }

    Err(PackError::MissingManifest)
}

/// Verify that a pack can be read by this build.
///
/// # Errors
///
/// - [`PackError::UnsupportedFormat`] if the archive was written by a newer
///   pack format than this build understands.
/// - Any error from [`inspect`].
pub fn verify(archive: &Path) -> Result<Manifest, PackError> {
    let manifest = inspect(archive)?;
    if manifest.format_version > PACK_FORMAT_VERSION {
        return Err(PackError::UnsupportedFormat {
            found: manifest.format_version,
            supported: PACK_FORMAT_VERSION,
        });
    }
    Ok(manifest)
}

/// List every payload path in a pack, relative to the project root.
///
/// Unlike [`inspect`] this walks the whole archive, so it costs a full
/// decompression pass.
///
/// # Errors
///
/// - [`PackError::UnusableArchiveEntry`] if the archive carries an entry a
///   restore would refuse; listing a pack that cannot be restored would
///   describe an operation that is not going to happen.
/// - [`PackError::Io`] if the archive cannot be read.
pub fn list_payload_paths(archive: &Path) -> Result<Vec<String>, PackError> {
    Ok(scan_payload(archive)?.paths)
}

/// What one decompression pass over the payload found.
pub(crate) struct Payload {
    /// Every payload path, relative to the project root.
    pub(crate) paths: Vec<String>,
    /// Hard links the archive carries, as `(path, target)` pairs. Restore does
    /// not create these, so a prediction has to name them too.
    pub(crate) hard_links: Vec<(String, String)>,
}

/// Walk the payload once, applying the same entry-type policy the restore uses.
///
/// The policy lives in [`crate::restore::entry_plan`] so that a prediction and
/// the restore it predicts cannot drift apart: an entry type the restore would
/// refuse fails here as well, and one it would decline to create is collected
/// rather than counted as a file that is going to appear.
///
/// # Errors
///
/// - [`PackError::UnusableArchiveEntry`] for an entry the restore would refuse.
/// - [`PackError::Io`] if the archive cannot be read.
pub(crate) fn scan_payload(archive: &Path) -> Result<Payload, PackError> {
    use crate::restore::EntryPlan;

    let file = File::open(archive)?;
    let decoder = zstd::stream::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);

    let prefix = format!("{PAYLOAD_PREFIX}/");
    let mut payload = Payload {
        paths: Vec::new(),
        hard_links: Vec::new(),
    };
    for entry in tar.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let s = path.to_string_lossy();
        let Some(rest) = s.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        let rel = rest.trim_end_matches('/').to_string();

        match crate::restore::entry_plan(entry.header().entry_type()) {
            EntryPlan::Extract => payload.paths.push(rel),
            EntryPlan::HardLink => {
                let target = entry
                    .link_name()?
                    .map(|t| t.display().to_string())
                    .unwrap_or_default();
                payload.hard_links.push((rel, target));
            }
            EntryPlan::Refuse(kind) => {
                return Err(PackError::UnusableArchiveEntry {
                    path: rel,
                    kind: kind.to_string(),
                });
            }
        }
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::create::{CreateOptions, create};
    use std::fs;
    use tempfile::TempDir;

    /// Inspecting something that is not a pack fails cleanly rather than panicking.
    #[test]
    fn test_inspect_rejects_non_archive() {
        let dir = TempDir::new().expect("tempdir");
        let bogus = dir.path().join("not.pack");
        fs::write(&bogus, b"definitely not zstd").expect("write");
        assert!(inspect(&bogus).is_err());
    }

    /// A pack whose first entry is not the manifest is rejected.
    #[test]
    fn test_inspect_requires_manifest_first() {
        let dir = TempDir::new().expect("tempdir");
        let out = dir.path().join("hand.pack");

        let file = fs::File::create(&out).expect("create");
        let encoder = zstd::stream::Encoder::new(file, 1).expect("encoder");
        let mut builder = tar::Builder::new(encoder);
        let body = b"x";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "payload/a.txt", &body[..])
            .expect("append");
        builder
            .into_inner()
            .expect("into_inner")
            .finish()
            .expect("finish");

        assert!(matches!(inspect(&out), Err(PackError::MissingManifest)));
    }

    /// `verify` accepts a pack this build wrote.
    #[test]
    fn test_verify_accepts_current_format() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path().join("proj");
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(root.join("src/main.rs"), "fn main() {}").expect("write");

        let out = dir.path().join("proj.pack");
        create(&CreateOptions::new(&root, &out, "0.13.3")).expect("create");

        let manifest = verify(&out).expect("verify");
        assert_eq!(manifest.format_version, PACK_FORMAT_VERSION);
    }
}
