# Global recipes

How lds finds and serves justfile recipes. The README covers the common case
(`~/.config/lds/justfile` plus `config.toml`); this is the full contract and the
two older routes that still work.

## Contract

Five points, none of them optional behaviour.

1. **Discovery paths**: lds reads `~/.config/lds/justfile` (default global),
   every directory listed in `config.toml` `recipes.dirs`, and every directory
   listed in `LDS_RECIPE_GLOBAL_DIRS`. Recipes pulled in by just's native
   `import '<path>'` from any of those justfiles are served too — imported
   recipes need no separate registration.

2. **Group filter** (mutually exclusive routing):

   | Tag | Routing |
   |---|---|
   | `[group('lds-plugin')]` | Registered as a dedicated MCP tool at startup (`mcp__lds__<name>`) **and** listed by `recipe_list` / runnable via `recipe_run`. For global utilities. |
   | `[group('allow-agent')]` | Listed by `recipe_list` and runnable via `recipe_run` only. Not a tool of its own. For project/task recipes. |
   | no group | **Excluded.** Not served at all. The legacy `# [allow-agent]` doc comment is still honored. |

3. **Dedup**: when the same recipe arrives through two paths (env injection plus
   a root `import`, say), `just --dump` dedupes by name; lds serves one entry
   rather than erroring.

4. **Restart required**: the global justfile set is resolved at process startup
   from `config.toml` and `LDS_RECIPE_GLOBAL_DIRS`. `recipe_list` / `recipe_run`
   re-parse justfiles live, but changes to `config.toml`, to env vars, or newly
   added global directories need an MCP client restart. SIGHUP reload is not
   implemented.

5. **Three coexisting routes**, all supported simultaneously: declare in
   `config.toml` `recipes.dirs` (recommended), inject via
   `LDS_RECIPE_GLOBAL_DIRS`, or `import '<abs>/justfile'` from
   `~/.config/lds/justfile`.

## `LDS_RECIPE_GLOBAL_DIRS`

Colon-separated directory list, PATH-style. Useful for CI and ephemeral
environments where writing a config file is inconvenient.

```json
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

With both this and `config.toml` set, env entries win on name collision — env is
loaded after config in the resolution chain, following the convention cargo, git
and gh use. Later entries override earlier ones; the project justfile always
wins.

## `import '<abs>/justfile'`

```just
import '/opt/shared/shared-recipes.just'
```

Added by hand to `~/.config/lds/justfile`. Does not appear in `lds recipe-dir
list`.

## Authoring

See [plugin-recipe-authoring.md](plugin-recipe-authoring.md) for the IF
contract, parameter mapping, shebang recipes, the macOS-awk and CWD pitfalls,
the [Plugin vs AllowAgent decision flowchart](plugin-recipe-authoring.md#11-decision-flowchart),
and the [naming-collision guide](plugin-recipe-authoring.md#12-plugin-naming-collision-guide).
