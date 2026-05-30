# lds — local-develop-server

Unified MCP server for the orch coding pipeline. Consolidates task-mcp (recipe),
git-reader / git-workflow (git), and boxed-analysis (sandbox, future) into a single
process with shared session state.

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
  GitModule  RecipeModule  (SandboxModule)
  git2-rs   just CLI        future: MVP2
     │         │
     ▼         ▼
  Session (core)
  root / session_id / timeout / max_output / global_recipe_dirs
```

### Session — Smart Inject Env

`session_start(root)` で全 module に project root を 1 回で注入する。
各 module は Session から root / timeout / max_output を読むだけ。
git module の write scope tracking (owned_worktrees) は Session とは別に module 内部で管理。

**Auto session-start**: サーバ起動時の CWD が ProjectRoot (`.git` または `justfile` を含むディレクトリ) である場合、`session_start` を呼ばなくても最初の tool call で自動的にセッションが開始される。`session_start` は引き続き利用可能で、別の project root に切り替える際に明示的に呼ぶ。自動起動した tool call のレスポンスには `auto_session_start` フィールドが含まれる。

**No-session error**: セッションが存在しない状態でツールを呼んだ場合、全ハンドラで JSON-RPC エラーコード `-32603` を返す (メッセージ: `"no session"`)。

### Resolve Chain (recipe)

dotenv-like な階層解決。`~/.config/lds/justfile` (default global) → `LDS_RECIPE_GLOBAL_DIRS` 追加 global → Project (`{root}/justfile`) の順で justfile を探索し、recipe 単位で merge する。name 衝突は後勝ち (Project が最高優先)。
各 recipe は `ResolveInfo { level, source_path }` を持ち、どこから来たかを追跡する。
`ResolveLevel` enum に variant を足すだけで Worktree 層等を追加可能。

### Output Safety

- **timeout**: recipe / sandbox の実行に `tokio::time::timeout` を適用 (default 60s)
- **truncation**: stdout / stderr が `max_output` (default 100KB) を超えたら
  head + tail に切り詰め。UTF-8 char 境界を尊重

## Crate Structure

```
crates/
├── core/    lds-core     Session, SessionConfig, LdsState, truncate_output
├── git/     lds-git      GitModule (git2-rs, write scope tracking)
├── recipe/  lds-recipe   RecipeModule (just CLI, resolve chain, content args)
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

### Git (write) — S1 in progress

| Tool | Description | Status |
|---|---|---|
| `git_commit` | Commit staged changes | planned |
| `git_worktree_add` | Create worktree (session-owned) | planned |
| `git_worktree_remove` | Remove session-owned worktree | planned |
| `git_worktree_list` | List worktrees | planned |
| `git_merge` | Merge branch into current | planned |
| `git_branch_delete` | Delete branch | planned |

### Recipe

| Tool | Description |
|---|---|
| `recipe_list` | List allow-agent recipes (with ResolveInfo source tracking) |
| `recipe_run` | Run recipe with args + content env vars, timeout + truncation |

## Consolidation Roadmap

```
S1: git write ops (commit/worktree/merge/branch_delete)
    → replace git-reader + git-workflow
    → verify with: committer, workspace-setup, topic-setup, worktree-merge

S2: recipe validation (content key validation)
    → replace task-mcp
    → verify with: impl-lead, build-resolver, quality-coding

S3: sandbox module (Docker container, subprocess delegation)
    → replace boxed-analysis
    → verify with: quality-gate, context-broad-scout, context-librarian
    → Docker daemon dependency requires careful liveness design
```

## Quantitative Justification

```
                        Current (12 MCP)   lds (top 4)    Δ
─────────────────────────────────────────────────────────────
Processes                  12               8            -33%
session_start / 3-ST run   56              43            -23%
Agent-MCP references       25              20            -20%
Install targets (top 5)     5               1            -80%
```

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

#### Additional Global Recipe Directories (`LDS_RECIPE_GLOBAL_DIRS`)

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

Precedence (low → high): `~/.config/lds` → `LDS_RECIPE_GLOBAL_DIRS` dirs in
declaration order → project `justfile`. Same-name recipes in later entries
override earlier ones; the project justfile always wins.

### Global Recipe Contract

Consumer-facing IF for serving recipes via lds. The five points below are
the contract; they are not optional behaviors.

1. **Discovery paths**: lds reads `~/.config/lds/justfile` (default global)
   and every directory listed in `LDS_RECIPE_GLOBAL_DIRS`. Recipes brought
   in by just's native `import '<path>'` from any of those justfiles are
   also served — there is no separate registration step for imported
   recipes.

2. **Group filter** (mutually exclusive routing):

   | Tag | Routing |
   |---|---|
   | `[group('lds-plugin')]` | Registered as a dedicated MCP tool at startup (`mcp__lds__<name>`) **and** listed by `recipe_list` / runnable via `recipe_run`. Intended for global utilities. |
   | `[group('allow-agent')]` | Listed by `recipe_list` and runnable via `recipe_run` only. Not exposed as an individual MCP tool. Intended for project/task recipes invoked through `recipe_run`. |
   | no group | **Excluded.** Not served at all (legacy `# [allow-agent]` doc comment is still honored for backward compatibility). |

3. **Dedup**: When the same recipe arrives through two paths (e.g. env
   injection + root `import`), `just --dump` dedupes by recipe name; lds
   does not error and serves a single entry.

4. **Restart required**: lds reads `LDS_RECIPE_GLOBAL_DIRS` and resolves
   the global justfile set at **process startup**. `recipe_list` /
   `recipe_run` re-parse justfiles live, but env changes and newly added
   global directories require a Claude Code restart to take effect.

5. **Two coexisting routes for adding global recipes**: (a) inject via
   `LDS_RECIPE_GLOBAL_DIRS` env, or (b) add `import '<abs>/justfile'` to
   `~/.config/lds/justfile`. Both are supported simultaneously. (a) is
   reproducible via `.mcp.json`; (b) requires editing the user's
   `~/.config/lds/justfile` (no first-class CLI today — see future work).

## License

MIT
