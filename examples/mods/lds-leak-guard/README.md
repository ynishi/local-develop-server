# lds-leak-guard

A Claude Code mod (function hooks, `tool.check` on `Bash`) that asks
[local-develop-server (lds)](../../../README.md) whether the repository is public before
a Bash command writes to a remote, and routes local git writes to `mcp__lds__git_*`.

## What it does

| Bash command | lds `publicity` | Verdict |
|---|---|---|
| `git push`, `gh pr/issue/release/repo ... create/merge/edit/...`, `gh api` writes, `cargo publish`, `npm/pnpm/yarn publish`, `docker/podman push`, cloud `deploy`, `aws s3 cp/sync` to `s3://`, `curl/wget` write requests | `PUBLIC` (or `FORKED` of a public repo) | `deny` |
| same | `AMBIGUOUS`, `UNKNOWN`, lds error, lds not connected | `ask` |
| same | `PRIVATE`, `INTERNAL`, `LOCAL`, `NOT_GIT` | `ask` |
| `git commit/merge/rebase/cherry-pick/reset`, `worktree add/remove/prune`, `branch -d/-D`, `stash drop/pop/apply/clear`, `checkout -b`, `switch -c` | any | `deny` (option `guard_local_git_writes`) |
| anything else (`ls`, `git status`, `curl` GET ...) | not asked | `next(e)` (engine decides) |

If the hook itself fails, the `.catch` handler denies.

## Requirements

- Claude Code 2.1.287 or later (function hooks / Mods).
- lds registered as an MCP server that `/mcp` lists as `lds` (change it with `lds_server`).

## Try it

```
claude --plugin-dir examples/mods/lds-leak-guard
claude plugin validate examples/mods/lds-leak-guard
claude plugin test examples/mods/lds-leak-guard
```

## Options (`userConfig`)

- `lds_server` (string, default `lds`): the MCP server name used for `$.mcp.call`.
- `guard_local_git_writes` (boolean, default `true`): deny Bash git writes and point to `mcp__lds__git_*`.

## Why

lds guards git writes per session ownership (`owned_worktrees`), but that guard only
sees `mcp__lds__git_*` calls. A Bash `git commit` or `git push` walks past it, and the
publicity classifier never gets a say. This mod puts both in front of Claude's tool call.

## Limits

- Hooks run on every surface, but `$.ui.toast` is shown only where the surface has a UI (terminal, Desktop).
- No WSL support is claimed.
- Command parsing is heuristic (quote-aware splitting on `&&`, `||`, `;`, `|`, newline), not a
  shell parser: `bash -c "..."`, `$(...)`, aliases and scripts that push internally are not seen.
- A publicity answer is cached for 5 minutes per working directory (`unknown` is never cached).
- lds answers for the project root of its own session, which may differ from Claude's cwd.
