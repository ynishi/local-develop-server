# Architecture

Internal layout of the lds server. Nothing here is needed to *use* lds — see
the README for that. This is for working on it.

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

## Crate structure

```
crates/
├── core/    lds-core     Session, SessionConfig, LdsState, truncate_output
├── git/     lds-git      GitModule (git2-rs, write scope tracking)
├── gh/      lds-gh       GhModule (gh CLI subprocess wrapper, read-only API, auth fail-fast)
├── recipe/  lds-recipe   RecipeModule (just CLI, resolve chain, content args)
├── sandbox/ lds-sandbox  SandboxModule (file-scoped read/append, snapshot/rollback)
├── journal/ lds-journal  JournalModule (journal-mcp-rmcp SDK consumer, `journal_*` tools forwarded)
├── outline/ lds-outline  OutlineModule (outline-mcp-rmcp SDK consumer, prefixed `outline_*` tools)
├── pack/    lds-pack     Whole-project archives (`pack_*` MCP tools + `lds pack` CLI)
├── router/  lds-router   Upstream MCP routing and re-export
├── publicity/ lds-publicity  Repository visibility probing
└── lds/     lds          MCP binary (rmcp v1.7, stdio transport)
```

## Session

`session_start(root)` injects the project root into every module in one call.
Each module reads `root` / `timeout` / `max_output` from the shared `Session`.
The git module additionally tracks write scope (`owned_worktrees`) internally,
separately from `Session`.

**Auto session-start**: when the server is launched inside a ProjectRoot (a
directory containing `.git` or `justfile`), the first tool call starts a session
using the startup CWD. `session_start` remains available for switching to a
different root. Auto-started calls carry an `auto_session_start` field in their
response.

**No-session error**: a tool called without an active session returns JSON-RPC
error code `-32603` with the message `"no session"`.

**Session root gone**: when the root is removed after `session_start` (a
worktree deleted while the session was live), the recipe family (`recipe_run` /
`recipe_list` / `recipe_list_plugins`) returns `"session root path no longer
exists, please call session_start again: <path>"`. Re-invoking `session_start`
with a valid root recovers.

## Recipe resolve chain

Justfiles are scanned in the order below (low → high) and merged per recipe;
later sources win on name collision.

| Priority | Source | Notes |
|---|---|---|
| lowest | `~/.config/lds/justfile` (default global) | always scanned |
| ↑ | `config.toml` `recipes.dirs` | additional directories declared in `~/.config/lds/config.toml` |
| ↑ | `LDS_RECIPE_GLOBAL_DIRS` env var | colon-separated dirs; legacy / CI |
| highest | Project (`{root}/justfile`) | project justfile at the session root |

Each recipe carries `ResolveInfo { level, source_path }` so its source layer is
traceable. A new layer (a Worktree level, say) is a new `ResolveLevel` variant.

## Output safety

- **timeout**: `tokio::time::timeout` wraps recipe / sandbox execution (default 60s).
- **truncation**: stdout / stderr beyond `max_output` (default 100KB) is cut to a
  head + tail pair, respecting UTF-8 character boundaries.

Pack operations run on a blocking thread (`spawn_blocking`), so packing a large
tree does not occupy a tokio worker.

## Why one process

Each MCP server an agent talks to is one more process to install, one more
`session_start` to call, and one more reference to thread through prompts.
Folding git, recipe, and sandbox into a single binary behind a shared `Session`
collapses the install surface to one target and lets every module read the same
root / timeout / output limits without duplicate configuration.
