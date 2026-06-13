# Changelog

All notable changes to this project will be documented in this file.

## [Unreleased]

## [0.3.0] - 2026-06-13

### Added

- **`crates/gh` — 8 additional read-only tools** — adds `gh_run_view`,
  `gh_run_log_failed`, `gh_run_jobs`, `gh_release_view`, `gh_release_list`,
  `gh_workflow_list`, `gh_workflow_view`, `gh_pr_checks`. All inherit the v0.2.0
  invariants: per-call `gh auth status` check (no caching),
  `Command::new("gh").args(&[...])` with shell=false, and structural read-only
  (write ops like `gh run cancel` / `gh release create` / `gh pr merge` remain
  absent). `gh_run_log_failed` parses `gh run view --log-failed` text output into
  `{ failed_steps: [{ job_name, step_name, log_tail }] }`. MCP tool surface grows
  from 8 to 16 gh tools.

### Changed

### Deprecated

### Removed

### Fixed

### Security

## [0.2.0] - 2026-06-13

### Added

- **`crates/gh` — new `lds-gh` crate with `GhModule` (GitHub CLI subprocess wrapper)** — adds a read-only MCP plugin for `gh` CLI operations. Exposes 8 tools: `gh_auth_status`, `gh_pr_list`, `gh_pr_view`, `gh_pr_diff`, `gh_issue_list`, `gh_issue_view`, `gh_repo_view`, `gh_run_list`. Every tool invocation runs `gh auth status` before the subprocess call and returns a typed authentication error when unauthenticated. All subprocess calls use `Command::new("gh").args(args)` (shell=false, arg-by-arg) — no string concatenation or `sh -c`. Write operations (`gh pr create` / `gh issue create` / `gh release create` / `gh pr merge`) are structurally absent from both `GhModule` and the MCP tool router; they are never exposed as callable tools.
- **README: `### Gh (read)` section added** — documents the 8 read-only `gh_*` tools with parameters and the write-operations exclusion rationale.
- **README: `crates/gh` row added to Crate Structure** — `gh/ lds-gh GhModule (gh CLI subprocess wrapper, read-only API, auth fail-fast)`.
- **Publish metadata for all 5 crates** — added `description` / `license` / `authors` / `repository` / `homepage` / `keywords` / `categories` (workspace-inherited where shared, per-crate `description`). License changed from `MIT` to `MIT OR Apache-2.0` (dual, matching common Rust crate convention). Repository / homepage URLs point to `https://github.com/ynishi/local-develop-server`. `LICENSE-MIT` and `LICENSE-APACHE` files added at the repo root.

### Documentation

- **`docs/plugin-recipe-authoring.md`: §2.1-§2.3 added — Justfile placement, multi-file patterns, anti-patterns** — clarifies that lds's `find_justfile()` matches the same canonical filename set as just itself (`justfile` / `Justfile` / `.justfile`); arbitrary `.just` filenames are not picked up by either tool's default search and must be reached via the parent justfile's `import` / `mod` directive. Adds a 4-row decision table for splitting recipes across files (project root / sub-dir with canonical filename / arbitrary filename via `import` / module via `mod`) and the corresponding anti-patterns (extending `find_justfile()`, pointing wire args at a single `.just` file, expecting lds to serve `{root}/scripts/build.just` directly). Resolves issue 9d986c99 — investigation confirmed that the existing resolve chain already covers all legitimate placements; no code changes required.
- **README: removed internal framing for a public-facing first read** — the lead paragraph, architecture diagram, "Consolidation Roadmap" section, and "Quantitative Justification" table previously referred to sibling MCP servers and in-house pipeline subagents by name, plus an internal-only metric table. All such references have been replaced with a neutral "Why one process?" paragraph and a simple Roadmap table describing the capability scope of each stage.
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
