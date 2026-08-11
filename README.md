# lds — local-develop-server

Unified MCP server for AI-driven coding agents. Bundles git read/write,
`just`-based recipe execution, file-sandbox operations, journal and outline
forwarding, and whole-project archives into one process behind a shared
`Session` — so an agent opens a repository once with `session_start` and then
runs every tool against the same project root.

## Install

```sh
cargo install --path crates/lds
```

```json
{
  "mcpServers": {
    "lds": { "command": "lds", "args": [] }
  }
}
```

Launched inside a directory containing `.git` or a `justfile`, the first tool
call starts a session on its own; `session_start` is only needed to switch to a
different root.

## Tools

### Session

| Tool | Description |
|---|---|
| `session_start` | Initialize session with project root |

### Git (read)

| Tool | Description |
|---|---|
| `git_status` | Working tree status |
| `git_log` | Commit log (configurable max_count) |
| `git_diff` | Diff working tree vs HEAD |

### Git (write)

`worktree_add` registers the worktree it creates in the session's
`owned_worktrees` set, and `commit` / `merge` / `worktree_remove` /
`branch_delete` refuse paths and branches that are not session-owned — one agent
cannot destroy another's work.

| Tool | Description |
|---|---|
| `git_commit` | Stage and commit changes in a session-owned working directory |
| `git_worktree_add` | Create a worktree on a new branch (session-owned). Placed under `SessionConfig::worktrees_dir` → env `LDS_WORKTREES_DIR` → `<root>/.worktrees` (default; the parent repo must gitignore this path) |
| `git_worktree_remove` | Remove a session-owned worktree |
| `git_worktree_list` | List worktrees with session-ownership annotation |
| `git_merge` | Merge a branch into another in a session-owned working directory |
| `git_branch_delete` | Delete a session-owned branch |

### Gh (read)

GitHub CLI wrapper. Requires `gh auth login`; every invocation checks
`gh auth status` and returns a typed error if unauthenticated.

| Tool | Description |
|---|---|
| `gh_auth_status` | Check gh CLI authentication status |
| `gh_pr_list` / `gh_pr_view` / `gh_pr_diff` / `gh_pr_checks` | PRs as JSON; `limit` defaults to 30 |
| `gh_issue_list` / `gh_issue_view` | Issues as JSON |
| `gh_repo_view` | Repository metadata (name/owner/defaultBranchRef) |
| `gh_run_list` / `gh_run_view` / `gh_run_jobs` | Actions workflow runs as JSON |
| `gh_run_log_failed` | Failed-step logs, parsed into `{failed_steps: [{job_name, step_name, log_tail}]}` |
| `gh_release_list` / `gh_release_view` | Releases as JSON |
| `gh_workflow_list` / `gh_workflow_view` | Workflows as JSON |

**Write operations are not exposed**: `gh pr create` / `gh issue create` /
`gh release create` / `gh pr merge` are deliberately absent, as are
`gh run cancel` (a write op) and `gh run watch` (long-polling, incompatible with
MCP request-response — poll `gh_run_view` instead). Invoke those from a shell.

### Recipe

| Tool | Description |
|---|---|
| `recipe_list` | List allow-agent recipes (with ResolveInfo source tracking) |
| `recipe_run` | Run recipe with args + content env vars, timeout + truncation |

### Pack

| Tool | Description |
|---|---|
| `pack_create` | Pack a whole project (`.git` + untracked local state) into one archive. `root` defaults to the session root; `dry_run` classifies without writing |
| `pack_restore` | Restore a pack, rewriting worktree gitdir wiring for the new location — including worktrees beside the project, wired up once both packs are restored side by side. `dry_run` predicts without writing |
| `pack_inspect` | Read a pack's manifest without unpacking; `files` also lists every packed path |

### Outline

The upstream [outline-mcp][outline-mcp] surface, forwarded in-process under the
`outline_` prefix (`outline_shelf`, `outline_select_book`, `outline_toc`,
`outline_node_*`, `outline_snapshot_*`, `outline_book_history`, `outline_dump`,
`outline_import`, `outline_checklist`, `outline_init`, `outline_gen_routing`).
Shelf root defaults to `$HOME/.config/outline-mcp/books`, overridable via
`LDS_OUTLINE_SHELF_DIR`. Tool semantics are the upstream ones; bundled guides
are surfaced verbatim under `outline://guides/*`.

[outline-mcp]: https://github.com/ynishi/outline-mcp

## `lds pack` — moving a whole project

A pack carries a project *as it exists on this machine*: the `.git` directory
itself, plus the untracked local state that normally never leaves — a
`workspace/` tree, journal databases, `.mcp.json`, sandbox snapshots. `.git` is
copied rather than bundled, because a bundle is built from an enumerated set of
refs and everything outside it (stashes, the reflog, local-only branches,
unreferenced objects) would not make the trip.

```sh
lds pack create                        # -> <project>-<timestamp>.pack, here
lds pack create --root ~/proj -o p.pack
lds pack create --dry-run              # classify only; write nothing
lds pack inspect p.pack                # manifest only; no decompression
lds pack restore p.pack --into ~/dest
lds pack restore p.pack --into ~/proj --dry-run # predict the restore
lds pack restore p.pack --into ~/proj --force   # restore over a working copy
```

`create --dry-run` reports what would travel; `restore --dry-run` reports what
would happen at the destination — which files get replaced, which survive, which
symlinks land dangling, which worktree pointers get rewritten.

`restore` refuses an existing destination unless `--force`, and `--force`
**overwrites without wiping**: pack entries replace their counterparts, and files
already there that the pack does not carry are left alone. A restored backup can
therefore end up dirtier than the machine the pack came from.

### What is not packed

| class | treatment |
|---|---|
| secrets (`.env`, `*.pem`, `id_rsa`, …) | **not packed**, reported — moving credentials stays yours to do |
| caches (`target/`, `node_modules/`, …) | **not packed**, recorded with the file count, size, and any credentials inside them |
| OS debris (`.DS_Store`, `Thumbs.db`) | **not packed**, recorded |
| symlinks | packed as links, never followed; recorded so a restore elsewhere names what dangles |

State outside the project root (`~/.config/...`) is not collected.

Nothing is dropped without a record: a path in your tree and not in the payload
can always be pointed at one of those lists. That matters most for caches, where
one line stands in for a whole subtree — a `cache_dirs` entry aimed at
hand-written source reads as `dist (412 files, 2.1 MB)` next to
`target (38104 files, 4.2 GB)`.

### Configuring the rules

```toml
[pack]
secret_globs   = ["my-app-keys.json", "*.vault"]  # also treat these as secrets
cache_dirs     = ["frontend/dist"]                # also treat these as caches
keep           = ["docs/samples/*.pem"]           # pack these despite a built-in rule
no_link_report = [".zsh/**"]                      # links here are expected; do not report them
```

Globs are scoped the way `.gitignore` scopes them: one with no `/` matches the
**file name at any depth**, one containing `/` is anchored to that **path
relative to the project root**. `keep = ["*.pem"]` carries every private key you
own; `keep = ["docs/samples/*.pem"]` carries the sample.

`secret_globs` and `cache_dirs` add to the built-ins rather than replacing them.
`keep` is the only subtractive list, and the only way a file the secret rules
named gets into the archive — whatever it rescues is named in the manifest's
`kept_over_secret`. A malformed glob is a hard error naming itself.

Every symlink is reported by default, because a link breaks when the project
lands elsewhere. `no_link_report` names the exception — a `.zsh/` tree shared
across your machines, a vendored link tree — and its links are packed without
being listed. Rules that actually suppressed something come back in
`no_link_report_applied`, so an empty `symlinks` list is never ambiguous.

```sh
$ lds pack create --dry-run
dry run: /home/u/proj -> proj-20260811-004500.pack (nothing written)
  would pack 2 files, 0 symlinks, 38 B
  (with [pack] overrides from config.toml)

not packed — secrets (move these yourself):
  .env              (secret pattern: .env)
  my-app-keys.json  (secret pattern: my-app-keys.json)

not packed — caches (regenerable):
  target        (38104 files, 4.2 GB)  (cache directory: target)
  node_modules  (21877 files, 412 MB)  (cache directory: node_modules)
      credential inside, dropped with it: node_modules/.npmrc
```

### Worktrees

Both halves of a worktree's wiring — `.git/worktrees/<name>/gitdir` and the
worktree's own `.git` file — are absolute paths, so `restore` rewrites them; a
plain `tar` extract would leave both aimed at the source machine.

A worktree made with `git worktree add ../name` sits *beside* the project, so the
two halves belong to two projects and travel in two packs. Restore them beside
each other and whichever lands second wires the pair up:

```sh
lds pack create --root ~/projects/proj         --out proj.pack
lds pack create --root ~/projects/proj-feature --out proj-feature.pack

lds pack restore proj.pack         --into /backup/proj
#   worktrees registered outside the project root, not found here: proj-feature
lds pack restore proj-feature.pack --into /backup/proj-feature
#   rewrote worktree pointers: proj-feature
```

Order does not matter, and `--force` re-restoring one side repairs a pair left
half-attached. A counterpart that is genuinely not on this machine is reported
(`missing_worktrees`, or `missing_worktree_parent` when the pack is itself a
worktree) rather than guessed at.

Full field reference: the `lds://docs/pack` MCP resource.

## Plugin recipes

Justfile recipes tagged `[group('lds-plugin')]` are registered as MCP tools at
startup. Drop a `justfile` at `~/.config/lds/justfile` (global) or in your
project root, and each plugin recipe becomes `mcp__lds__<name>`.

```sh
cp examples/global-justfile.skeleton ~/.config/lds/justfile
# restart the MCP client so the server re-reads the global plugin set
```

The skeleton ships `complexity` / `search-excluding` / `remote-url` /
`text-stats` / `greet`.

Persistent global recipe directories go in `~/.config/lds/config.toml`:

```toml
[recipes]
dirs = ["/opt/shared-recipes", "~/team-recipes"]

[paths]
global_justfile = "~/.config/lds/justfile"
```

```sh
lds recipe-dir add ~/team-recipes
lds recipe-dir list
lds recipe-dir remove ~/team-recipes
```

`recipe-dir add` expands `~` before writing, and preserves existing comments and
sections. `config.toml` is read once at startup, so changes need a restart.

Resolution priority (low → high): `~/.config/lds/justfile` → `config.toml`
`recipes.dirs` → `LDS_RECIPE_GLOBAL_DIRS` → project `justfile`.

## Further reading

| Document | Contents |
|---|---|
| [docs/recipes.md](docs/recipes.md) | Full recipe contract, `LDS_RECIPE_GLOBAL_DIRS`, `import` |
| [docs/plugin-recipe-authoring.md](docs/plugin-recipe-authoring.md) | Writing plugin recipes: IF contract, parameter mapping, pitfalls |
| [docs/architecture.md](docs/architecture.md) | Internal layout, session model, resolve chain |
| `lds://docs/pack` | Pack field reference (MCP resource) |
| `lds://docs/multi-session`, `lds://docs/routing` | Session and routing guides (MCP resources) |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
