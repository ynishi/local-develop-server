# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

### Added

- **Publish metadata for all 5 crates** — added `description` / `license` / `authors` / `repository` / `homepage` / `keywords` / `categories` (workspace-inherited where shared, per-crate `description`). License changed from `MIT` to `MIT OR Apache-2.0` (dual, matching algocline / seed-scope). Repository / homepage URLs point to `https://github.com/ynishi/local-develop-server`. `LICENSE-MIT` and `LICENSE-APACHE` files added at the repo root.

### Documentation

- **README: rewritten in English, License section updated to dual MIT OR Apache-2.0** — the Session / Resolve Chain / Output Safety sections previously carried Japanese prose; rewritten in English to match the repo-wide doc language policy. The License section now points to both `LICENSE-APACHE` and `LICENSE-MIT` (was MIT-only).
- **README: Git (write) section synced with implementation** — the table previously labelled `Git (write) — S1 in progress` with all six tools marked `planned` was out of date. The six write tools (`git_commit`, `git_merge`, `git_branch_delete`, `git_worktree_add`, `git_worktree_remove`, `git_worktree_list`) have been implemented and exposed via the MCP tool router since earlier in 0.1.0 development (verified by `cargo test --test e2e_mcp` 8/8 pass on 2026-06-01). README now describes the session-scoped write safety contract and drops the stale `Status` column. The Crate Structure block gained the missing `sandbox/ lds-sandbox` row, and Consolidation Roadmap marks S1 as ✅ done.

### Changed

- **lds-recipe: `RecipeError::Just(String)` split into two typed variants** — `JustDumpFailed { justfile: PathBuf, stderr: String }` (subprocess non-zero exit) and `JustDumpParse { justfile: PathBuf, source: serde_json::Error }` (JSON parse failure). The old `Just(String)` catch-all is removed. `dump_justfile` and `merged_recipes` now return `Result<_, RecipeError>` instead of `anyhow::Result`; callers that need `anyhow::Error` add `.map_err(anyhow::Error::from)` at the boundary.
- **lds-core: `uuid_v4()` renamed to `session_id_new()`** — the private helper that generates `{nanos_hex}-{pid_hex}` identifiers was renamed to reflect that it produces a lightweight session-ownership token, not an RFC 4122 UUID. The generated format is unchanged; `Session::id()` consumers are unaffected.
- **lds (test): `git_status_round_trip` assertion refined to two-phase structural contract** — replaced the trivial `contains("\n") || contains("CURRENT")` OR-chain with a clean-phase check (empty string or `Status(…)` entry) followed by a dirty-phase check that writes an untracked file and asserts `Status(` and `dirty.txt` both appear in the output.
- Derive `Debug + Default` for `SessionConfig` and switch call sites to `..Default::default()` spread — eliminates K-87 (struct literal breakage on field addition).
- Extracted `Session::ensure_alive()` to core crate and introduced typed `SessionError`; `RecipeError::SessionRootGone(PathBuf)` was replaced by `RecipeError::Session(#[from] SessionError)` (transparent Display). K-239 recovery error message string preserved verbatim through the wrapper chain.
- **lds-core: migrated from `anyhow` to `thiserror`** — introduced `CoreError` with two typed variants (`RootNotFound(PathBuf)` / `NoSession`); `anyhow` dependency removed from `crates/core/Cargo.toml`. `Session::new`, `LdsState::start_session`, and `LdsState::session` now return `Result<_, CoreError>`. Consumer call sites are unaffected: `Display` output of both variants matches the previous `bail!`/`anyhow!` message strings verbatim.

### Fixed

- **Session root existence check on recipe calls (K-239)**: `recipe_run`, `recipe_list`, and `recipe_list_plugins` now verify that the session root directory still exists before executing. When the root has been removed (e.g. a worktree was deleted while the session was still active), callers receive a clear error — `"session root path no longer exists, please call session_start again: <path>"` — instead of an opaque `just --dump` failure. Call `session_start` with a valid root to recover.
- **session_start now succeeds after the previous session's root has been removed** — `try_plugin_call` is bypassed for `session_start`, mirroring the auto-start gate exemption (K-239 regression fix).
