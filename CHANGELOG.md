# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Changed

- Derive `Debug + Default` for `SessionConfig` and switch call sites to `..Default::default()` spread — eliminates K-87 (struct literal breakage on field addition).
- Extracted `Session::ensure_alive()` to core crate and introduced typed `SessionError`; `RecipeError::SessionRootGone(PathBuf)` was replaced by `RecipeError::Session(#[from] SessionError)` (transparent Display). K-239 recovery error message string preserved verbatim through the wrapper chain.
- **lds-core: migrated from `anyhow` to `thiserror`** — introduced `CoreError` with two typed variants (`RootNotFound(PathBuf)` / `NoSession`); `anyhow` dependency removed from `crates/core/Cargo.toml`. `Session::new`, `LdsState::start_session`, and `LdsState::session` now return `Result<_, CoreError>`. Consumer call sites are unaffected: `Display` output of both variants matches the previous `bail!`/`anyhow!` message strings verbatim.

### Fixed

- **Session root existence check on recipe calls (K-239)**: `recipe_run`, `recipe_list`, and `recipe_list_plugins` now verify that the session root directory still exists before executing. When the root has been removed (e.g. a worktree was deleted while the session was still active), callers receive a clear error — `"session root path no longer exists, please call session_start again: <path>"` — instead of an opaque `just --dump` failure. Call `session_start` with a valid root to recover.
- **session_start now succeeds after the previous session's root has been removed** — `try_plugin_call` is bypassed for `session_start`, mirroring the auto-start gate exemption (K-239 regression fix).
