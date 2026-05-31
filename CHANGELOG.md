# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Changed

- Derive `Debug + Default` for `SessionConfig` and switch call sites to `..Default::default()` spread — eliminates K-87 (struct literal breakage on field addition).

### Fixed

- **Session root existence check on recipe calls (K-239)**: `recipe_run`, `recipe_list`, and `recipe_list_plugins` now verify that the session root directory still exists before executing. When the root has been removed (e.g. a worktree was deleted while the session was still active), callers receive a clear error — `"session root path no longer exists, please call session_start again: <path>"` — instead of an opaque `just --dump` failure. Call `session_start` with a valid root to recover.
- **session_start now succeeds after the previous session's root has been removed** — `try_plugin_call` is bypassed for `session_start`, mirroring the auto-start gate exemption (K-239 regression fix).
