//! Archive-supplied paths, proven to stay inside the restore destination.
//!
//! An archive names paths twice over: as tar entry names, and as strings in the
//! manifest — what a worktree is called, where it sat, where a symlink was.
//! Restore joins both kinds onto the destination and writes there, so both are
//! the same kind of input; only the first was ever checked. That left the
//! manifest as a way to aim a write at any directory on the machine while the
//! payload beside it was being guarded.
//!
//! [`Contained`] closes that by construction rather than by remembering. It is
//! the only thing restore joins onto a destination, and it cannot be built out
//! of a path that leaves one — so a case that skips the check is not a case
//! that writes to the wrong place, it is a case that does not compile.

use std::path::{Component, Path, PathBuf};

use crate::error::PackError;

/// A relative path from an archive that stays under whatever it is joined onto.
///
/// Every component is a plain name: no `..`, no root, no drive prefix, and not
/// a leading `.` — one in the middle is normalized away by `Path::components`
/// before it is ever seen, and resolves to a path that stays inside anyway.
///
/// That is the whole of what this type promises. It says nothing about whether
/// the path exists, and nothing about symlinks met on the way: a link planted
/// by an earlier entry redirects a perfectly contained path, and refusing that
/// is a separate check made against the filesystem at write time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Contained(PathBuf);

impl Contained {
    /// Check a tar entry's path, already stripped of the payload prefix.
    ///
    /// # Errors
    ///
    /// [`PackError::EscapingArchivePath`] naming the entry.
    pub(crate) fn entry(rel: &Path) -> Result<Self, PackError> {
        if plain_components(rel) {
            Ok(Self(rel.to_path_buf()))
        } else {
            Err(PackError::EscapingArchivePath(rel.display().to_string()))
        }
    }

    /// Check a manifest string that names a location under the project root.
    ///
    /// `field` is the manifest's own name for it, so a refusal points at the
    /// key that carried the path rather than leaving the reader to find it.
    ///
    /// # Errors
    ///
    /// [`PackError::EscapingManifestPath`] naming the field and the value.
    pub(crate) fn path(field: &str, raw: &str) -> Result<Self, PackError> {
        let candidate = Path::new(raw);
        if plain_components(candidate) {
            Ok(Self(candidate.to_path_buf()))
        } else {
            Err(PackError::EscapingManifestPath {
                field: field.to_string(),
                value: raw.to_string(),
            })
        }
    }

    /// Check a manifest string that names one directory, not a path.
    ///
    /// A worktree's name is a single directory under `.git/worktrees/`, so a
    /// value with any separator in it is wrong whether or not it escapes.
    ///
    /// # Errors
    ///
    /// [`PackError::EscapingManifestPath`] naming the field and the value.
    pub(crate) fn name(field: &str, raw: &str) -> Result<Self, PackError> {
        let checked = Self::path(field, raw)?;
        if checked.0.components().count() == 1 {
            Ok(checked)
        } else {
            Err(PackError::EscapingManifestPath {
                field: field.to_string(),
                value: raw.to_string(),
            })
        }
    }

    /// Join this onto a base directory, landing inside it.
    pub(crate) fn join_onto(&self, base: &Path) -> PathBuf {
        base.join(&self.0)
    }

    /// The path as checked, relative and unmodified.
    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Whether every component is an ordinary name.
///
/// An empty path fails: it names the destination itself, and joining it would
/// hand back a directory where a file was expected.
fn plain_components(path: &Path) -> bool {
    let mut any = false;
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return false;
        }
        any = true;
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ordinary relative paths pass and come back unchanged.
    #[test]
    fn test_accepts_relative_paths() {
        let checked = Contained::path("worktrees[].path", ".worktrees/feature").expect("contained");
        assert_eq!(checked.as_path(), Path::new(".worktrees/feature"));
        assert_eq!(
            checked.join_onto(Path::new("/dest")),
            PathBuf::from("/dest/.worktrees/feature")
        );
    }

    /// Every way out of a destination is refused, including the empty path,
    /// which names the destination itself.
    #[test]
    fn test_refuses_every_escape() {
        for raw in ["..", "../sibling", "a/../../b", "/etc", "", ".", "./a"] {
            assert!(
                Contained::path("worktrees[].path", raw).is_err(),
                "must refuse {raw:?}"
            );
        }
    }

    /// A `.` in the middle is not an escape: `Path::components` normalizes it
    /// away, so the value is the contained path it resolves to.
    #[test]
    fn test_accepts_interior_current_dir() {
        let checked = Contained::path("worktrees[].path", "a/./b").expect("contained");
        assert_eq!(
            checked.join_onto(Path::new("/dest")),
            PathBuf::from("/dest/a/./b"),
            "which resolves to /dest/a/b — still inside"
        );
    }

    /// An absolute path is refused rather than silently replacing the base:
    /// `Path::join` drops its left side when handed one.
    #[test]
    fn test_refuses_absolute_which_would_replace_the_base() {
        let err =
            Contained::path("worktrees[].path", "/opt/other-project").expect_err("must refuse");
        assert!(
            matches!(err, PackError::EscapingManifestPath { ref field, .. } if field == "worktrees[].path"),
            "got {err:?}"
        );
    }

    /// A name is one component; a path in a name field is refused even when it
    /// would have stayed inside.
    #[test]
    fn test_name_takes_exactly_one_component() {
        assert!(Contained::name("worktrees[].name", "feature").is_ok());
        assert!(Contained::name("worktrees[].name", "nested/feature").is_err());
        assert!(Contained::name("worktrees[].name", "../feature").is_err());
    }

    /// An entry path is checked the same way but refused as an archive entry,
    /// which is what the operator sees it as.
    #[test]
    fn test_entry_reports_itself_as_an_entry() {
        let err = Contained::entry(Path::new("../evil")).expect_err("must refuse");
        assert!(
            matches!(err, PackError::EscapingArchivePath(_)),
            "got {err:?}"
        );
    }
}
