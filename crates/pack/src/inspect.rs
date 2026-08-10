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
            entry.read_to_string(&mut text)?;
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
/// [`PackError::Io`] if the archive cannot be read.
pub fn list_payload_paths(archive: &Path) -> Result<Vec<String>, PackError> {
    let file = File::open(archive)?;
    let decoder = zstd::stream::Decoder::new(file)?;
    let mut tar = tar::Archive::new(decoder);

    let prefix = format!("{PAYLOAD_PREFIX}/");
    let mut out = Vec::new();
    for entry in tar.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(&prefix)
            && !rest.is_empty()
        {
            out.push(rest.trim_end_matches('/').to_string());
        }
    }
    Ok(out)
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
