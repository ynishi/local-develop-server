# lds-ops-pane

A Claude Code mod that draws the [local-develop-server (lds)](../../../README.md) session as a
pane and as a one-line band above the prompt: root, publicity (`PUBLIC` in red), owned
worktrees and the latest journal chapter.

## What it shows

`/lds` opens the pane:

```
lds         18dd487a…  /home/u/proj/lds
publicity   github=PUBLIC  crates=PUBLIC
worktrees   main  5795a59  (not owned)
            topic/x  1a2b3c4  owned
journal     2026-10-10 — Claude Code Mods survey  closed
updated 12s ago  [ Refresh ]  [ Hide band ]
```

The band sits above the prompt (red and bold while the repository is public):

```
lds  local-develop-server  github=PUBLIC crates=PUBLIC  [ Hide ]
```

The data comes from lds `session_info`, `git_worktree_list`, `publicity` and `journal_tail`,
fetched together at session start, on `/lds`, on `[ Refresh ]` and after every turn. A source that
fails is listed under `errors` and never stops the rest.

## Keyboard (SSH, tmux, no mouse)

The buttons are keyboard controls; a mouse is optional and only works where
the terminal forwards clicks.

| Keys | Effect |
|---|---|
| `ctrl+x` then `tab` | Focus the band (or the open pane). `tab` / arrows walk its buttons, `enter` presses, `esc` returns to the prompt |
| `h` (band focused) | Hide the band |
| `r` / `b` (pane focused) | Refresh / toggle the band |
| `ctrl+x ctrl+a` | Collapse the band (Claude Code's own `[-]`) |
| `ctrl+x x` | Close the pane |

## Requirements

- Claude Code 2.1.287 or later (function hooks / Mods).
- lds registered as an MCP server that `/mcp` lists as `lds` (change it with `lds_server`).

## Try it

```
claude --plugin-dir examples/mods/lds-ops-pane
claude plugin validate examples/mods/lds-ops-pane
claude plugin test examples/mods/lds-ops-pane
```

Or, inside a session, from this repository's marketplace:

```
/plugin install lds-ops-pane --marketplace ynishi/local-develop-server
```

Then type `/lds`.

## Options (`userConfig`)

- `lds_server` (string, default `lds`): the MCP server name used for `$.mcp.call`.
- `show_band` (boolean, default `true`): draw the band above the prompt.

## Why

[`lds-leak-guard`](../lds-leak-guard) refuses remote writes on a public repository. This mod puts
the same judgement where you can see it before you ask for a push: a red `PUBLIC` badge.

## Limits

- Drawing happens on the terminal and Desktop only; the hooks also run on other surfaces.
- `publicity` answers for the root of lds's own session, which may differ from Claude's cwd.
- The pane is not opened at session start (it would take over the screen on a main-screen
  terminal); type `/lds`. The band appears once the first snapshot exists.
