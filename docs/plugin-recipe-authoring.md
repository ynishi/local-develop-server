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

## 2. Where Plugin recipes live

lds scans two justfile locations (resolve chain):

1. **Global**: `~/.config/lds/justfile` (or `$XDG_CONFIG_HOME/lds/justfile`)
2. **Project**: `{session.root}/justfile`

Global plugins are loaded eagerly at server startup, before the first
`session_start` call. This means the MCP client (Claude Code) sees them
in the very first `tools/list` response after connecting.

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
