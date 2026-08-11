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

    /// The manifest is larger than this build will read into memory.
    ///
    /// Reading is what happens before the archive has been judged, so the
    /// archive cannot be the one deciding how much memory that takes.
    /// Compression makes the difference enormous: a small file expands into as
    /// much filler as its author cares to write.
    #[error("manifest is larger than the {limit} byte limit; refusing the archive")]
    ManifestTooLarge {
        /// The largest manifest this build reads, in bytes.
        limit: u64,
    },

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

    /// An archive entry names a path that would land outside the restore
    /// destination (`..` or an absolute component).
    ///
    /// A pack produced by this crate never contains such an entry, so one that
    /// does is not a pack with a blemish — it is not trustworthy at all, and
    /// the restore stops rather than extracting the rest of it.
    #[error("archive entry '{0}' escapes the restore destination; refusing the archive")]
    EscapingArchivePath(String),

    /// A manifest field names a path that would land outside the restore
    /// destination.
    ///
    /// The manifest travels in the archive, so its paths are the archive's
    /// claims about the project just as much as the entry names are — worktree
    /// locations and symlink paths are all joined onto the destination. Checked
    /// with the same rule and refused the same way; the field is named because
    /// the value alone does not say which claim carried it.
    #[error(
        "manifest field '{field}' names '{value}', which escapes the restore destination; refusing the archive"
    )]
    EscapingManifestPath {
        /// The manifest key that carried the path.
        field: String,
        /// The path as the manifest wrote it.
        value: String,
    },

    /// An archive entry is of a type a restore will not materialize.
    ///
    /// This crate's writer produces directories, regular files and symlinks and
    /// nothing else, so any other type came from another producer. Device
    /// nodes and fifos are not project content, and a hard link names a file
    /// the pack does not carry — see [`crate::restore::HardLinkRecord`] for why
    /// that one is reported rather than followed.
    #[error(
        "archive entry '{path}' is a {kind}, which a pack never contains; refusing the archive"
    )]
    UnusableArchiveEntry {
        /// The entry as the archive names it.
        path: String,
        /// What it is, in the words the report uses.
        kind: String,
    },

    /// An archive entry would be written through a symlink, which would land
    /// it outside the restore destination.
    ///
    /// The scan never descends into a symlinked directory, so a legitimate
    /// pack has no file entries underneath a link. An archive that routes a
    /// write through one is crafted, and the restore stops.
    #[error("archive entry '{path}' would write through symlink '{via}'; refusing the archive")]
    WriteThroughSymlink {
        /// The entry that would have been written.
        path: String,
        /// The symlink it would have been written through.
        via: PathBuf,
    },
}
