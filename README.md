# lds — local-develop-server

Unified MCP server for AI-driven coding agents. Bundles git read/write,
`just`-based recipe execution, and file-sandbox operations into one process
backed by a shared `Session` state — so an agent can open a repository once
with `session_start` and then run git, recipe, and sandbox tools against the
same project root without re-establishing context per tool call.

## Architecture

```
Claude Code / Agent
       │
       │ stdio (MCP JSON-RPC)
       ▼
┌─────────────────────────────────┐
│         LdsServer               │
│    Arc<RwLock<Inner>>           │
│    #[tool_router] + stdio       │
└──────────┬──────────────────────┘
           │
     ┌─────┼──────────┐
     ▼     ▼          ▼
  GitModule  RecipeModule  SandboxModule
  git2-rs    just CLI      fs + snapshot
     │         │             │
     └─────────┴─────────────┘
                 │
                 ▼
        Session (core)
        root / session_id / timeout / max_output / global_recipe_dirs
```

### Session — Smart Inject Env

`session_start(root)` injects the project root into every module in one call.
Each module reads `root` / `timeout` / `max_output` from the shared `Session`.
The git module additionally tracks write scope (`owned_worktrees`) internally,
separately from `Session`.

**Auto session-start**: when the server is launched inside a ProjectRoot
(a directory containing `.git` or `justfile`), the first tool call
automatically starts a session using the startup CWD — `session_start` is
optional in that case. It remains available for switching to a different
project root explicitly. Auto-started calls include an `auto_session_start`
field in their response.

**No-session error**: when a tool is called without an active session, every
handler returns JSON-RPC error code `-32603` with the message `"no session"`.

**Session root gone error**: when the session root is removed after
`session_start` (e.g. a worktree was deleted while the session was still
active), recipe-family tools (`recipe_run` / `recipe_list` /
`recipe_list_plugins`) return
`"session root path no longer exists, please call session_start again: <path>"`.
Re-invoking `session_start` with a valid root recovers the state.

### Resolve Chain (recipe)

Dotenv-style hierarchical resolution. Justfiles are scanned in the priority
order below (low → high) and merged per recipe; later sources win on name
collision (Project has highest priority).

| Priority | Source | Notes |
|---|---|---|
| lowest | `~/.config/lds/justfile` (default global) | always scanned |
| ↑ | `config.toml` `recipes.dirs` | additional directories declared in `~/.config/lds/config.toml` |
| ↑ | `LDS_RECIPE_GLOBAL_DIRS` env var | colon-separated dirs; legacy / CI |
| highest | Project (`{root}/justfile`) | project justfile at the session root |

Each recipe carries `ResolveInfo { level, source_path }` so its source layer
is traceable. Adding a new layer (e.g. Worktree) only requires extending the
`ResolveLevel` enum with a new variant.

### Output Safety

- **timeout**: `tokio::time::timeout` is applied to recipe / sandbox execution (default 60s)
- **truncation**: when stdout / stderr exceeds `max_output` (default 100KB) it
  is truncated to a head + tail pair, respecting UTF-8 character boundaries

## Crate Structure

```
crates/
├── core/    lds-core     Session, SessionConfig, LdsState, truncate_output
├── git/     lds-git      GitModule (git2-rs, write scope tracking)
├── gh/      lds-gh       GhModule (gh CLI subprocess wrapper, read-only API, auth fail-fast)
├── recipe/  lds-recipe   RecipeModule (just CLI, resolve chain, content args)
├── sandbox/ lds-sandbox  SandboxModule (file-scoped read/append, snapshot/rollback)
└── lds/     lds          MCP binary (rmcp v1.7, stdio transport)
```

## Tools

### Session

| Tool | Description |
|---|---|
| `session_start` | Initialize session with project root. Optional when the server was launched inside a ProjectRoot (directory containing `.git` or `justfile`) — the first tool call auto-starts the session using the startup CWD. Call `session_start` explicitly to use a different root. |

### Git (read)

| Tool | Description |
|---|---|
| `git_status` | Working tree status |
| `git_log` | Commit log (configurable max_count) |
| `git_diff` | Diff working tree vs HEAD |

### Gh (read)

GitHub CLI (`gh`) wrapper. Requires `gh auth login` before use; every tool
invocation checks `gh auth status` and returns a typed error if unauthenticated.

| Tool | Description |
|---|---|
| `gh_auth_status` | Check gh CLI authentication status |
| `gh_pr_list` | List PRs as JSON (number/title/state/author). `limit` optional (default 30). |
| `gh_pr_view` | View a single PR as JSON. Requires `number`. |
| `gh_pr_diff` | Show diff of a PR. Requires `number`. |
| `gh_issue_list` | List issues as JSON (number/title/state). `limit` optional (default 30). |
| `gh_issue_view` | View a single issue as JSON. Requires `number`. |
| `gh_repo_view` | Repository metadata as JSON (name/owner/defaultBranchRef). |
| `gh_run_list` | List Actions workflow runs as JSON. `limit` optional (default 30). |

**Write operations not exposed**: `gh pr create` / `gh issue create` /
`gh release create` / `gh pr merge` are deliberately not exposed as MCP tools.
Users invoke these via shell directly.

### Git (write)

Session-scoped write operations: `worktree_add` registers the created worktree in
the session's `owned_worktrees` set, and subsequent write tools (`commit`,
`merge`, `worktree_remove`, `branch_delete`) refuse to operate on paths /
branches that are not session-owned. This prevents one agent from destroying
another's work.

| Tool | Description |
|---|---|
| `git_commit` | Stage and commit changes in a session-owned working directory |
| `git_worktree_add` | Create a worktree under `.worktrees/` with a new branch (session-owned) |
| `git_worktree_remove` | Remove a session-owned worktree |
| `git_worktree_list` | List worktrees with session-ownership annotation |
| `git_merge` | Merge a branch into another in a session-owned working directory |
| `git_branch_delete` | Delete a session-owned branch |

### Recipe

| Tool | Description |
|---|---|
| `recipe_list` | List allow-agent recipes (with ResolveInfo source tracking) |
| `recipe_run` | Run recipe with args + content env vars, timeout + truncation |

## Roadmap

| Stage | Scope | Status |
|---|---|---|
| S1 | Git write ops (`commit` / `merge` / `worktree_{add,remove,list}` / `branch_delete`) with session-scoped write safety | ✅ done |
| S2 | Recipe schema validation (typed content-key contract for `recipe_run`) | planned |
| S3 | Sandbox extensions — optional container / subprocess isolation backends for the sandbox module | planned |

## Why one process?

Each MCP server an agent has to talk to is one more process to install, one
more `session_start` call to make, and one more reference to thread through
prompts. Folding git, recipe, and sandbox into a single binary backed by a
shared `Session` collapses the install surface to one target, the per-task
session call to one invocation, and lets every module read the same project
root / timeout / output limits without duplicate configuration.

## Usage

```sh
cargo install --path crates/lds

# .mcp.json
{
  "mcpServers": {
    "lds": { "command": "lds", "args": [] }
  }
}
```

### Plugin Recipes

Justfile recipes tagged with `[group('lds-plugin')]` are auto-registered
as MCP tools at startup. Drop a `justfile` at `~/.config/lds/justfile`
(global) or in your project root (project-scoped) and each plugin
recipe becomes `mcp__lds__<name>`.

Quick bootstrap:

```sh
cp examples/global-justfile.skeleton ~/.config/lds/justfile
# restart Claude Code so the MCP server re-reads the global plugin set
```

The skeleton ships with `complexity` / `search-excluding` /
`remote-url` / `text-stats` / `greet`. See
[docs/plugin-recipe-authoring.md](docs/plugin-recipe-authoring.md) for
the full IF contract, parameter mapping, shebang recipes, and the
macOS-awk / CWD pitfalls. The same doc carries the
[Plugin vs AllowAgent decision flowchart](docs/plugin-recipe-authoring.md#11-decision-flowchart)
and the
[naming-collision guide](docs/plugin-recipe-authoring.md#12-plugin-naming-collision-guide)
for picking the right group.

#### config.toml (Recommended)

The preferred way to configure persistent global recipe directories is
`~/.config/lds/config.toml`:

```toml
[recipes]
dirs = ["/opt/shared-recipes", "~/team-recipes"]

[paths]
global_justfile = "~/.config/lds/justfile"
```

Use the `lds recipe-dir` CLI to manage `recipes.dirs` without hand-editing:

```sh
lds recipe-dir add ~/team-recipes
lds recipe-dir list
lds recipe-dir remove ~/team-recipes
```

> **Tilde expansion**: `lds recipe-dir add ~/team-recipes` expands the path
> to an absolute path before writing it to `config.toml`. Existing comments
> and other sections in `config.toml` are preserved (patch-safe write).

**Resolution priority** (low → high):
`~/.config/lds/justfile` (default) → `config.toml` `recipes.dirs` →
`LDS_RECIPE_GLOBAL_DIRS` env → project `justfile`

**Restart required**: `config.toml` is read once at process startup.
Changes to `config.toml` require restarting the lds process to take effect.
SIGHUP-based reload is not implemented (tracked as a separate issue).

#### Additional Global Recipe Directories — Legacy (`LDS_RECIPE_GLOBAL_DIRS`)

> **Legacy**: prefer `config.toml` + `lds recipe-dir add` (above) for new
> setups. `LDS_RECIPE_GLOBAL_DIRS` continues to work and is useful for CI /
> ephemeral environments where a config file is inconvenient.

Set `LDS_RECIPE_GLOBAL_DIRS` to a colon-separated list of directories
(PATH-style) to load additional global justfiles beyond `~/.config/lds/`:

```sh
# .mcp.json
{
  "mcpServers": {
    "lds": {
      "command": "lds",
      "args": [],
      "env": {
        "LDS_RECIPE_GLOBAL_DIRS": "/opt/shared-recipes:/home/user/team-recipes"
      }
    }
  }
}
```

When both `config.toml` and `LDS_RECIPE_GLOBAL_DIRS` are set, directories
from `LDS_RECIPE_GLOBAL_DIRS` take precedence over `config.toml` on name
collision — env is loaded after config in the resolution chain, following
the standard CLI convention (cargo, git, gh: env overrides file config).
Same-name recipes in later entries override earlier ones; the project
justfile always wins.

#### Alternative: `import '<abs>/justfile'`

Add an `import` statement to `~/.config/lds/justfile` to pull in another
justfile directly:

```just
import '/opt/shared/shared-recipes.just'
```

This approach requires editing `~/.config/lds/justfile` by hand and does
not appear in `lds recipe-dir list`. It is provided for compatibility with
existing setups.

### Global Recipe Contract

Consumer-facing IF for serving recipes via lds. The five points below are
the contract; they are not optional behaviors.

1. **Discovery paths**: lds reads `~/.config/lds/justfile` (default global),
   every directory listed in `config.toml` `recipes.dirs`, and every
   directory listed in `LDS_RECIPE_GLOBAL_DIRS`. Recipes brought in by
   just's native `import '<path>'` from any of those justfiles are also
   served — there is no separate registration step for imported recipes.

2. **Group filter** (mutually exclusive routing):

   | Tag | Routing |
   |---|---|
   | `[group('lds-plugin')]` | Registered as a dedicated MCP tool at startup (`mcp__lds__<name>`) **and** listed by `recipe_list` / runnable via `recipe_run`. Intended for global utilities. |
   | `[group('allow-agent')]` | Listed by `recipe_list` and runnable via `recipe_run` only. Not exposed as an individual MCP tool. Intended for project/task recipes invoked through `recipe_run`. |
   | no group | **Excluded.** Not served at all (legacy `# [allow-agent]` doc comment is still honored for backward compatibility). |

3. **Dedup**: When the same recipe arrives through two paths (e.g. env
   injection + root `import`), `just --dump` dedupes by recipe name; lds
   does not error and serves a single entry.

4. **Restart required**: lds resolves the global justfile set at **process
   startup** using `config.toml` and `LDS_RECIPE_GLOBAL_DIRS`. `recipe_list`
   / `recipe_run` re-parse justfiles live, but changes to `config.toml`,
   env vars, or newly added global directories require a Claude Code restart
   to take effect. SIGHUP reload is not implemented.

5. **Three coexisting routes for adding global recipes**: (a) declare in
   `config.toml` `recipes.dirs` via `lds recipe-dir add` (recommended), (b)
   inject via `LDS_RECIPE_GLOBAL_DIRS` env (legacy / CI), or (c) add
   `import '<abs>/justfile'` to `~/.config/lds/justfile` (manual). All three
   are supported simultaneously.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
