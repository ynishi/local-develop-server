//! First-class configuration for lds.
//!
//! Reads and writes `~/.config/lds/config.toml` (or an explicit path).
//! The primary design constraints are:
//!
//! 1. **patch-safe write** — `Config::save` uses `toml_edit` to update only
//!    the `recipes.dirs` array while preserving comments and unrelated sections.
//! 2. **tilde expansion** — any path stored on disk must be an absolute path;
//!    tilde literals are never written to `config.toml`.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use toml_edit::{Array, DocumentMut, Item, Value};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during config load or save operations.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// An I/O error (e.g. permission denied, parent directory not found).
    #[error("config I/O error: {0}")]
    Io(#[from] io::Error),

    /// TOML deserialization error (returned by `Config::load`).
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),

    /// `toml_edit` document-level error (returned by `Config::save`).
    #[error("config edit error: {0}")]
    Edit(#[from] toml_edit::TomlError),

    /// TOML serialization error.
    #[error("config serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

// ---------------------------------------------------------------------------
// Config structs
// ---------------------------------------------------------------------------

/// Top-level configuration for lds.
///
/// Deserializes from `~/.config/lds/config.toml`.  Missing sections fall back
/// to `Default` via `#[serde(default)]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Recipe directory settings.
    pub recipes: Recipes,
    /// Path overrides.
    pub paths: Paths,
}

/// Recipe-related configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Recipes {
    /// Additional global recipe directories (highest priority source).
    ///
    /// Entries are absolute paths.  Tilde is expanded on load and must be
    /// absent from `config.toml` on disk.
    pub dirs: Vec<PathBuf>,
}

/// Path overrides for well-known lds locations.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Paths {
    /// Override for the global justfile path (default: `~/.config/lds/justfile`).
    pub global_justfile: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// tilde_expand
// ---------------------------------------------------------------------------

/// Expand a leading `~/` or lone `~` to the user's home directory.
///
/// # Arguments
///
/// * `input` — A path string that may start with `~/`.
///
/// # Returns
///
/// An absolute `PathBuf`.  If `input` does not start with `~/` or `~`, it is
/// returned as-is wrapped in `PathBuf`.
///
/// # Errors
///
/// Returns `ConfigError::Io(NotFound)` when the home directory cannot be
/// determined (e.g. `$HOME` is unset on Unix).
pub fn tilde_expand(input: &str) -> Result<PathBuf, ConfigError> {
    if input == "~" {
        let home = dirs::home_dir().ok_or_else(|| {
            ConfigError::Io(io::Error::new(io::ErrorKind::NotFound, "HOME not set"))
        })?;
        Ok(home)
    } else if let Some(rest) = input.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| {
            ConfigError::Io(io::Error::new(io::ErrorKind::NotFound, "HOME not set"))
        })?;
        Ok(home.join(rest))
    } else {
        Ok(PathBuf::from(input))
    }
}

// ---------------------------------------------------------------------------
// Config impl
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration from an explicit file path.
    ///
    /// # Arguments
    ///
    /// * `path` — Path to a TOML configuration file.
    ///
    /// # Returns
    ///
    /// A fully populated `Config`.  Missing optional sections are filled with
    /// `Default`.
    ///
    /// # Errors
    ///
    /// - `ConfigError::Io` if the file cannot be read.
    /// - `ConfigError::Parse` if the TOML is malformed.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load configuration from the default path (`~/.config/lds/config.toml`).
    ///
    /// If the file does not exist this returns `Config::default()` silently.
    /// Any other I/O error or parse error is also silently swallowed and the
    /// default is returned — suitable for startup where a missing config is
    /// expected to be common.
    ///
    /// # Returns
    ///
    /// A `Config`, falling back to `Default` on any error.
    pub fn load_or_default() -> Self {
        let Some(home) = dirs::home_dir() else {
            return Self::default();
        };
        let path = home.join(".config/lds/config.toml");
        match Self::load(&path) {
            Ok(cfg) => cfg,
            Err(ConfigError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!("failed to load config from {}: {}", path.display(), e);
                Self::default()
            }
        }
    }

    /// Save the `recipes.dirs` list to `path` using a **patch-safe** write.
    ///
    /// The file is parsed by `toml_edit` so that comments and sections not
    /// managed by this function (e.g. `[paths]`) are preserved verbatim.
    /// Only the `recipes.dirs` array is replaced.
    ///
    /// All paths in `dirs` must already be absolute (tilde-expanded before
    /// calling this function).  Passing a tilde literal is a logic error and
    /// will be written literally — callers are responsible for expanding first.
    ///
    /// If the parent directory does not exist it is created with
    /// `fs::create_dir_all`.
    ///
    /// # Arguments
    ///
    /// * `path` — Destination file (typically `~/.config/lds/config.toml`).
    /// * `dirs` — Absolute paths to persist in `recipes.dirs`.
    ///
    /// # Errors
    ///
    /// - `ConfigError::Io` for I/O failures (create dir, read, write).
    /// - `ConfigError::Edit` if the existing file is not valid TOML.
    pub fn save(path: &Path, dirs: &[PathBuf]) -> Result<(), ConfigError> {
        // Ensure parent directory exists.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Read existing content (empty string when file is absent).
        let existing = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(ConfigError::Io(e)),
        };

        // Parse with toml_edit to preserve comments and unrelated sections.
        let mut doc: DocumentMut = existing.parse::<DocumentMut>()?;

        // Build a fresh TOML array from `dirs`.
        let mut arr = Array::new();
        for dir in dirs {
            // Safety: PathBuf::to_string_lossy is infallible (may be lossy on
            // non-UTF-8 systems, but that is acceptable given TOML's UTF-8 requirement).
            arr.push(dir.to_string_lossy().as_ref());
        }

        // Write `recipes.dirs` — create intermediate tables as needed.
        if !doc.contains_table("recipes") {
            doc["recipes"] = toml_edit::table();
        }
        doc["recipes"]["dirs"] = Item::Value(Value::Array(arr));

        std::fs::write(path, doc.to_string())?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ------------------------------------------------------------------
    // T1: happy-path / property tests
    // ------------------------------------------------------------------

    /// T1-a: round-trip — serialize a Config and read it back identically.
    #[test]
    fn test_round_trip_load_save() {
        let dir = TempDir::new().unwrap(); // justification: TempDir::new is infallible in practice; any failure surfaces as a test setup panic which is acceptable in test code
        let path = dir.path().join("config.toml");

        let dirs_in = vec![
            PathBuf::from("/opt/shared-recipes"),
            PathBuf::from("/home/user/team-recipes"),
        ];

        Config::save(&path, &dirs_in).expect("save should succeed");
        let cfg = Config::load(&path).expect("load should succeed");

        assert_eq!(cfg.recipes.dirs, dirs_in);
    }

    /// T1-b: load_or_default returns Default when no file exists.
    #[test]
    fn test_load_or_default_missing_file() {
        // Temporarily override HOME to a directory with no config.toml.
        let dir = TempDir::new().unwrap(); // justification: same as above
        // We cannot easily unset HOME in a portable way, so we test Config::load
        // directly with a non-existent path to exercise the NotFound branch.
        let path = dir.path().join("nonexistent/config.toml");
        match Config::load(&path) {
            Err(ConfigError::Io(e)) => {
                assert_eq!(e.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected Io(NotFound), got {:?}", other),
        }
    }

    /// T1-c: tilde_expand returns an absolute path for a ~/... input.
    #[test]
    fn test_tilde_expand_tilde_slash() {
        // Only run when HOME is available.
        if dirs::home_dir().is_none() {
            return;
        }
        let result = tilde_expand("~/foo/bar").expect("tilde_expand should succeed");
        let home = dirs::home_dir().unwrap(); // justification: we just checked it is Some above
        assert_eq!(result, home.join("foo/bar"));
    }

    /// T1-d: tilde_expand with bare `~`.
    #[test]
    fn test_tilde_expand_bare_tilde() {
        if dirs::home_dir().is_none() {
            return;
        }
        let result = tilde_expand("~").expect("bare tilde should expand");
        let home = dirs::home_dir().unwrap(); // justification: checked is Some above
        assert_eq!(result, home);
    }

    // ------------------------------------------------------------------
    // T2: boundary / edge-case tests
    // ------------------------------------------------------------------

    /// T2-a: empty dirs list produces empty `recipes.dirs` array.
    #[test]
    fn test_save_empty_dirs() {
        let dir = TempDir::new().unwrap(); // justification: test setup
        let path = dir.path().join("config.toml");

        Config::save(&path, &[]).expect("save should succeed");
        let cfg = Config::load(&path).expect("load should succeed");
        assert!(cfg.recipes.dirs.is_empty());
    }

    /// T2-b: load_or_default on truly missing file via `Config::load` NotFound.
    #[test]
    fn test_load_or_default_does_not_panic_on_missing() {
        // Exercise the public load_or_default by calling it; if HOME is not
        // set or the file is absent it returns Default without panic.
        let _cfg = Config::load_or_default();
        // No assertion needed — absence of panic is the contract.
    }

    /// T2-c: tilde_expand with no tilde passes through unchanged.
    #[test]
    fn test_tilde_expand_no_tilde() {
        let result = tilde_expand("/absolute/path").expect("should succeed");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    /// T2-d: tilde_expand with a relative path (no tilde) passes through.
    #[test]
    fn test_tilde_expand_relative() {
        let result = tilde_expand("relative/path").expect("should succeed");
        assert_eq!(result, PathBuf::from("relative/path"));
    }

    /// T2-e: Config::load on an empty file returns all-default values.
    #[test]
    fn test_load_empty_file() {
        let dir = TempDir::new().unwrap(); // justification: test setup
        let path = dir.path().join("config.toml");
        fs::write(&path, "").unwrap(); // justification: writing empty file in test, infallible on tempdir

        let cfg = Config::load(&path).expect("empty file should parse as default");
        assert!(cfg.recipes.dirs.is_empty());
        assert!(cfg.paths.global_justfile.is_none());
    }

    /// T2-f: Config::load on a file with only [paths] section (no [recipes]).
    #[test]
    fn test_load_partial_file_no_recipes() {
        let dir = TempDir::new().unwrap(); // justification: test setup
        let path = dir.path().join("config.toml");
        fs::write(&path, "[paths]\nglobal_justfile = \"/etc/lds/justfile\"\n").unwrap(); // justification: writing known-good TOML in test

        let cfg = Config::load(&path).expect("partial file should parse");
        assert!(
            cfg.recipes.dirs.is_empty(),
            "missing [recipes] should default to empty"
        );
        assert_eq!(
            cfg.paths.global_justfile,
            Some(PathBuf::from("/etc/lds/justfile"))
        );
    }

    // ------------------------------------------------------------------
    // T3: error-path tests
    // ------------------------------------------------------------------

    /// T3-a: Config::load on a non-existent path returns ConfigError::Io(NotFound).
    #[test]
    fn test_load_nonexistent_returns_io_not_found() {
        let result = Config::load(Path::new("/nonexistent/path/config.toml"));
        match result {
            Err(ConfigError::Io(e)) => {
                assert_eq!(e.kind(), io::ErrorKind::NotFound);
            }
            other => panic!("expected Io(NotFound), got {:?}", other),
        }
    }

    /// T3-b: Config::load on malformed TOML returns ConfigError::Parse.
    #[test]
    fn test_load_malformed_toml_returns_parse_error() {
        let dir = TempDir::new().unwrap(); // justification: test setup
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is not = valid toml [\n").unwrap(); // justification: intentional bad TOML for error path test

        let result = Config::load(&path);
        assert!(
            matches!(result, Err(ConfigError::Parse(_))),
            "malformed TOML should yield Parse error, got {:?}",
            result
        );
    }

    // ------------------------------------------------------------------
    // Crux 2 preservation test: patch-safe write
    // ------------------------------------------------------------------

    /// Crux 2: `Config::save` must preserve comments and unrelated sections.
    ///
    /// This test writes a config.toml with a comment and `[paths]` section,
    /// then calls `Config::save` to update `recipes.dirs`, and asserts that
    /// the comment and `[paths]` section survive unmodified.
    #[test]
    fn test_save_preserves_comments_and_other_sections() {
        let dir = TempDir::new().unwrap(); // justification: test setup
        let path = dir.path().join("config.toml");

        // Seed file with a comment and [paths] section.
        let initial = r#"# This is a user comment that must survive.
[recipes]
dirs = []

[paths]
global_justfile = "/etc/lds/justfile"
"#;
        fs::write(&path, initial).unwrap(); // justification: seeding known-good TOML in test

        let new_dirs = vec![PathBuf::from("/opt/recipes")];
        Config::save(&path, &new_dirs).expect("save should succeed");

        let saved = fs::read_to_string(&path).unwrap(); // justification: reading back tempfile in test

        // Comment must be preserved.
        assert!(
            saved.contains("# This is a user comment that must survive."),
            "comment was not preserved:\n{}",
            saved
        );

        // [paths] section must be preserved.
        assert!(
            saved.contains("[paths]"),
            "[paths] section was not preserved:\n{}",
            saved
        );
        assert!(
            saved.contains("global_justfile"),
            "global_justfile key was not preserved:\n{}",
            saved
        );

        // recipes.dirs must be updated.
        let cfg = Config::load(&path).expect("load after save should succeed");
        assert_eq!(cfg.recipes.dirs, new_dirs);

        // Crux 2: tilde literal must not appear on disk.
        assert!(
            !saved.contains('~'),
            "tilde literal found on disk — crux 2 violation:\n{}",
            saved
        );
    }

    /// Crux 2 (tilde): paths saved to disk must be absolute (no tilde literal).
    #[test]
    fn test_save_does_not_write_tilde_literal() {
        if dirs::home_dir().is_none() {
            return;
        }
        let dir = TempDir::new().unwrap(); // justification: test setup
        let path = dir.path().join("config.toml");

        // Expand tilde before saving — as callers are required to do.
        let raw = "~/my-recipes";
        let expanded = tilde_expand(raw).expect("tilde_expand should succeed");
        assert!(
            !expanded.to_string_lossy().contains('~'),
            "expanded path must not contain tilde"
        );

        Config::save(&path, &[expanded]).expect("save should succeed");

        let saved = fs::read_to_string(&path).unwrap(); // justification: reading back tempfile in test
        assert!(
            !saved.contains('~'),
            "tilde literal found on disk after save — crux 2 violation:\n{}",
            saved
        );
    }
}
