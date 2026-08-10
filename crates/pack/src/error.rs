//! Error type for pack creation, inspection, and restore.

use std::path::PathBuf;

use thiserror::Error;

/// Errors produced by the pack module.
#[derive(Debug, Error)]
pub enum PackError {
    /// An I/O error while reading the project or writing the archive.
    #[error("pack I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The tree walk failed (permission denied, vanished directory, cycle).
    #[error("pack walk error: {0}")]
    Walk(#[from] walkdir::Error),

    /// The given path is not a directory.
    #[error("not a directory: {0}")]
    NotADirectory(PathBuf),

    /// A configured glob could not be compiled.
    ///
    /// Carries the offending pattern so a typo in `[pack]` points at itself
    /// rather than silently matching nothing.
    #[error("invalid pack pattern '{pattern}': {message}")]
    BadPattern {
        /// The pattern as written in configuration.
        pattern: String,
        /// Why it failed to compile.
        message: String,
    },

    /// The manifest could not be serialized.
    #[error("manifest serialize error: {0}")]
    ManifestSerialize(#[from] toml::ser::Error),

    /// The manifest could not be parsed.
    #[error("manifest parse error: {0}")]
    ManifestParse(#[from] toml::de::Error),

    /// The archive did not begin with a `pack.toml` entry.
    #[error("archive has no {name} entry — not an lds pack?", name = crate::manifest::MANIFEST_NAME)]
    MissingManifest,

    /// The archive was written by a newer pack format than this build reads.
    #[error("pack format version {found} is newer than supported version {supported}")]
    UnsupportedFormat {
        /// Version recorded in the archive.
        found: u32,
        /// Highest version this build understands.
        supported: u32,
    },

    /// The restore destination already exists and would be overwritten.
    #[error("destination already exists: {0}")]
    DestinationExists(PathBuf),
}
