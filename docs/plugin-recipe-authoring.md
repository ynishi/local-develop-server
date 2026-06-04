# Writing lds Plugin Recipes

lds exposes `[group('lds-plugin')]` recipes as first-class MCP tools at
startup. This document is the canonical reference for authoring them,
including the IF contract, parameter mapping, and the pitfalls that
have bitten real implementations.

## 1. Plugin vs Task Recipe

Two recipe groups, two consumption modes:

| | `[group('lds-plugin')]` | `[group('allow-agent')]` |
|---|---|---|
| MCP surface | each recipe becomes its own named tool (`mcp__lds__<name>`) | tools are discovered via `recipe_list` and invoked via `recipe_run` |
| Discovery | automatic, at server startup | on-demand listing |
| Intended for | reusable wrappers (rg / codedash / git remote, etc.) | project-specific workflows (build / deploy / release) |
| Source review | agents should NOT need to read recipe source | agents OPEN the recipe to understand what runs |

If you find yourself writing the same wrapper recipe across multiple
projects, it's a Plugin. If the recipe encodes one project's
deployment ladder, it's a Task.

### 1.1 Decision flowchart

When you write a new recipe, walk this tree before choosing a group:

```
Is the recipe useful across multiple unrelated projects?
├── yes → Is it a thin wrapper around a single CLI / tool?
│        ├── yes → [group('lds-plugin')]   (e.g. complexity, search-excluding)
│        └── no  → still likely Plugin, but verify the next question:
│                  Does invoking it require a session root?
│                  ├── no  → [group('lds-plugin')]
│                  └── yes → consider whether per-project state belongs in
│                            the recipe (Task) or in a session arg (Plugin).
└── no  → Does it encode a project-specific workflow (build / deploy /
         release / scaffold)?
         ├── yes → [group('allow-agent')]   (e.g. profile-install, q-fix,
         │        coding-orch, deploy-lds-import)
         └── no  → omit the group. Recipe will not be served.
```

Two anchors:

- **Plugin** = "the wrapper an agent reaches for instinctively, like
  `grep`." Same name, same shape, every project.
- **Task** = "the runbook step that depends on this repo's layout."
  Agents call it through `recipe_run` and may open the recipe source.

### 1.2 Plugin naming collision guide

`[group('lds-plugin')]` recipes occupy the global MCP tool namespace
(`mcp__lds__<recipe-name>`). Every Claude Code session sees them by
that exact name. Two rules follow:

1. **Names must be globally unique across every justfile lds loads**
   (directories in `config.toml` `recipes.dirs`, every dir in
   `LDS_RECIPE_GLOBAL_DIRS`, `~/.config/lds/justfile`, and every
   justfile they `import`). If two plugins share a name, just's own
   dedup picks one — but which one wins is an implementation detail you
   should not depend on. Treat collisions as a config bug.

2. **Pick names that read as utilities, not nouns from your project.**
   `complexity`, `search-excluding`, `text-stats`, `remote-url` work
   because they describe an operation. `mywebapp-deploy` would compile
   but pollutes the namespace and signals you should have used
   `[group('allow-agent')]` instead.

Existing global plugins (reference set, all live in
`examples/global-justfile.skeleton`):

| Name | What it wraps | Why it's a Plugin |
|---|---|---|
| `complexity` | codedash + jq + sort | every Rust/TS repo, same shape |
| `search-excluding` | rg with stable exclude list | repo-agnostic search |
| `text-stats` | wc -l/-w/-c on a list of files | repo-agnostic |
| `remote-url` | `git remote get-url origin` | one git invocation, universal |
| `greet` | echo test | smoke test, intentionally tiny |

Existing AllowAgent task recipes (typical patterns):

| Name | Why it's a Task (not a Plugin) |
|---|---|
| `profile-install` | mutates the consumer project's local config layout |
| `q-fix-*` | encodes a multi-step fix workflow |
| `coding-orch-*` | drives a project-aware pipeline |
| `deploy-lds-import` | edits the user's `~/.config/lds/justfile` |

### 1.3 Legacy `# [allow-agent]` doc-comment marker

`is_allow_agent` still honors a `# [allow-agent]` token in the doc
comment for backward compatibility with pre-group justfiles. **Do not
use it in new recipes.** It exists to keep old recipes working until
they're migrated; once a recipe is touched, switch to
`[group('allow-agent')]`.

## 2. Where Plugin recipes live

lds scans justfile locations in the following resolve chain (priority low → high):

| Priority | Source | How to configure |
|---|---|---|
| lowest | `~/.config/lds/justfile` (default global) | Drop a justfile here to get started |
| ↑ | Directories in `config.toml` `recipes.dirs` | `lds recipe-dir add <path>` (recommended) |
| ↑ | Directories in `LDS_RECIPE_GLOBAL_DIRS` env | Legacy / CI; colon-separated |
| highest | `{session.root}/justfile` (project) | Per-project recipes |

Directories from `LDS_RECIPE_GLOBAL_DIRS` take precedence over `config.toml`
on name collision (env is loaded after config in the resolution chain,
following the cargo/git/gh convention where env overrides file config).
On name collision the higher-priority source wins; the project justfile always wins.

**Adding recipe directories** (recommended):

```sh
lds recipe-dir add ~/team-recipes   # expands tilde; preserves existing config.toml
lds recipe-dir list                 # show all configured dirs
lds recipe-dir remove ~/team-recipes
```

For CI or one-off environments, set `LDS_RECIPE_GLOBAL_DIRS` instead
(see [Additional Global Recipe Directories — Legacy](../README.md#additional-global-recipe-directories--legacy-lds_recipe_global_dirs)).

**Loading order**: global plugins are loaded eagerly at server startup, before
the first `session_start` call. The MCP client (Claude Code) sees them in the
very first `tools/list` response after connecting. Changes to `config.toml` or
env vars require restarting the lds process (SIGHUP reload is not implemented).

Project plugins become visible only after `session_start` is called.
On name collision, project wins.

## 3. Parameter mapping

Each just recipe parameter becomes a `string` field in the MCP tool's
JSON Schema. Defaults shape the `required` array.

```just
[group('lds-plugin')]
greet name="world":
    @echo "hello {{name}}"
```

Compiles to:

```json
{
  "name": "greet",
  "description": "<doc comment becomes description>",
  "input_schema": {
    "type": "object",
    "properties": {
      "name": { "type": "string" }
    }
  }
}
```

`name="world"` carries a default, so the parameter is optional. Drop
the default to make it required:

```just
[group('lds-plugin')]
search-excluding pattern paths="." exclude="comment,string":
    ...
```

Here `pattern` is required (no default); `paths` and `exclude` are
optional.

**Restrictions** (deliberately strict):

- Parameter values are **always String** — no integers, no booleans, no
  nested objects. Agents pass numbers as `"20"`, not `20`.
- Recipe parameters are passed as **positional args** by lds. Order
  matters: lds walks `recipe.parameters` in declaration order and
  supplies the MCP arguments in that sequence.
- `Vec<String>` / array parameters are **not supported**. Use a
  delimiter convention (space-separated, comma-separated) and split
  inside the recipe.

## 4. Content args (`TASK_MCP_CONTENT_*`)

For multi-value / structured payloads, accept a `content: HashMap<String, String>`
via the recipe_run path. Plugin invocations from MCP tool calls don't
use this channel — they use positional args. Content args are for
agents that explicitly call `recipe_run`.

```just
[group('allow-agent')]
publish-note:
    echo "$TASK_MCP_CONTENT_TITLE: $TASK_MCP_CONTENT_BODY"
```

Plugin recipes generally don't need content args. If a Plugin would
benefit from one, prefer adding more positional parameters instead.

## 5. Shebang recipes (when you need real shell)

just recipes default to running each line in a separate sub-shell.
That's fine for one-shot commands but breaks the moment you need:

- multi-line pipelines that share local state
- `set -euo pipefail`
- shell case / if branches
- variables that persist across lines

Use a shebang recipe:

```just
[group('lds-plugin')]
complexity path="." top="20" sort_by="cyclomatic":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{sort_by}}" in
        lines) k=2 ;;
        depth) k=4 ;;
        *) k=3 ;;
    esac
    codedash analyze {{path}} -t {{top}} -o json \
      | jq -r '.entries[] | ...' \
      | sort -t: -k${k} -rn \
      | head -n {{top}}
```

The recipe body becomes a single script with the requested
interpreter. just substitutes `{{...}}` before the script runs, so
recipe params are visible as literal text.

## 6. Footguns

### macOS awk doesn't expand variables in pipe destinations

This will compile and run but fails with `syntax error` at runtime
under BSD awk (default on macOS):

```awk
{ print $0 | "sort -t: -k" k " -rn" }
```

GNU awk handles it; BSD awk treats the right-hand side of `|` as a
fixed string. Avoid it. Either pre-compute `k` in a `bash` `case`
before the awk pipeline, or skip awk entirely (the `complexity`
recipe in §5 does exactly this).

### CWD is set by lds, not by the recipe author

When lds invokes a recipe through `RecipeModule::run`, it passes
`--working-directory {session.root}` to just. So `git remote ...` and
`codedash analyze .` work as if you were inside the project.

If you `just --justfile ~/.config/lds/justfile complexity` directly
from the shell, just runs in `~/.config/lds/`, which is not a git repo
— recipes that depend on the CWD will fail. That's not a recipe bug;
it's an invocation mode mismatch. Test plugin recipes by calling them
through `mcp__lds__<name>`, not through `just` directly.

**Anti-pattern: don't `cd` inside the recipe body.**

`just` provides `invocation_directory()` which expands to the directory
where `just` was *launched* (typically your project root), NOT the
session root that lds set as cwd via `--working-directory`. Writing
this in a recipe body **destroys** the session root that lds carefully
arranged:

```just
[group('lds-plugin')]
cp-check:
    #!/usr/bin/env bash
    cd "{{invocation_directory()}}"   # ❌ BREAKS session_root
    [ -f Cargo.toml ] && cargo check
```

Symptom: with `mcp__lds__session_start(root=<subdir>)` followed by
`mcp__lds__recipe_run(recipe="cp-check")`, the recipe's `pwd` lands on
the just invocation point (project root) instead of `<subdir>`, and
files under `<subdir>` become invisible. The fix is to **delete the
`cd` line** — lds already set the cwd correctly. If you need to verify,
`pwd` at the top of the recipe shows `{session.root}`, not the justfile
dir.

### Quoting interpolated parameters

just performs textual substitution. If a parameter contains a
space or shell metacharacter and you write `{{pattern}}` unquoted,
the shell tokenizes it. Quote interpolations:

```just
[group('lds-plugin')]
search pattern:
    rg "{{pattern}}" .   # correct
    rg {{pattern}} .     # breaks on patterns with spaces
```

### Description comes from the doc comment

The MCP tool's `description` is the recipe's doc comment (the
`#`-prefixed line directly above the recipe). Make it useful — agents
read it to pick the right tool.

```just
# Code complexity metrics via codedash (top N functions sorted by metric)
# sort_by: cyclomatic | lines | depth (default: cyclomatic)
[group('lds-plugin')]
complexity path="." top="20" sort_by="cyclomatic":
    ...
```

Multiple comment lines all become the description.

## 7. Testing

Three layers:

1. **Parse check**: `just --justfile ~/.config/lds/justfile --list` —
   confirms the recipe is well-formed and lds will be able to dump it.
   Should show the recipe under `[lds-plugin]`.

2. **Schema check via MCP**: after restarting Claude Code (so the lds
   MCP server re-reads global plugins on startup), `ToolSearch` for
   `mcp__lds__<name>` and inspect the parameters. Confirm
   `description`, `properties`, and `required` match your intent.

3. **End-to-end smoke**: call the tool through MCP after
   `mcp__lds__session_start(root=<project>)`. The session root is
   critical for any recipe that touches git / codedash / files —
   without a session, lds returns "no session" (plugin tools list
   eagerly but execution still requires `session_start`).

## 8. Common recipes (reference)

See `examples/global-justfile.skeleton` for working implementations of
`complexity`, `search-excluding`, `remote-url`, `text-stats`, and a
minimal `greet` test recipe. Drop the file into `~/.config/lds/justfile`
to bootstrap a global plugin set.

## 9. Related

- `crates/recipe/src/lib.rs` — `is_plugin`, `list_plugins`,
  `list_global_plugins` (loader internals)
- `crates/lds/src/main.rs` — `ServerHandler::list_tools` /
  `call_tool` override, `try_plugin_call`, `plugin_to_tool`
- `workspace/docs/plugin-recipe-design.md` — design rationale and the
  Plugin vs Task split decision (supersedes the earlier
  `migration-new-tools.md` typed-tool proposal)
