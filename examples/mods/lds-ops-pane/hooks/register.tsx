import { atom, read, update } from 'claude-code'
import type { McpToolResult, Register, StateDollar } from 'claude-code'

import type { PublicityRow, Snapshot, Worktree } from '../types'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

type Level = Snapshot['level']

// The slice of `$` the data side uses. `$` of a hook is structurally assignable.
type Dollar = StateDollar & {
  mcp: {
    call: (
      server: string,
      tool: string,
      args?: Record<string, unknown>,
    ) => Promise<McpToolResult>
  }
  clock: { now: () => Promise<number> }
}

export type Tone = 'danger' | 'safe' | 'warn' | 'dim' | 'plain'
export type Cell = { text: string; tone: Tone }
export type Row = { label: string; cells: Cell[] }

type ToneProps = { color?: string; bold?: true; dimColor?: true }

// ---------------------------------------------------------------------------
// State (drawing state lives in `$.state`, never in a module variable)
// ---------------------------------------------------------------------------

const PANE_ID = 'lds-ops'
const snapshot = atom({ plugin: 'lds-ops-pane', key: 'snapshot' } as const, null)
const isBandHidden = atom({ plugin: 'lds-ops-pane', key: 'isBandHidden' } as const, false)

// ---------------------------------------------------------------------------
// Parsing (pure; none of these throws)
// ---------------------------------------------------------------------------

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

function textOf(r: McpToolResult): string | undefined {
  for (const block of r.content) {
    if (block.type === 'text' && typeof block.text === 'string') return block.text
  }
  return undefined
}

/** structuredContent first, then the first text block parsed as JSON. */
function jsonCandidates(r: McpToolResult): unknown[] {
  const out: unknown[] = []
  if (r.structuredContent !== undefined && r.structuredContent !== null) {
    out.push(r.structuredContent)
  }
  const text = textOf(r)
  if (text !== undefined) {
    try {
      out.push(JSON.parse(text))
    } catch {
      // not JSON; the structured candidate (if any) stands alone
    }
  }
  return out
}

function firstParsed<T>(r: McpToolResult, parse: (v: unknown) => T | undefined): T | undefined {
  for (const c of jsonCandidates(r)) {
    const v = parse(c)
    if (v !== undefined) return v
  }
  return undefined
}

/** Reads `session_id:` and `root:` out of lds `session_info` (plain text). */
export function parseSessionInfo(text: string): { sessionId: string | null; root: string | null } {
  const id = /^session_id:[ \t]*(\S+)/m.exec(text)
  const root = /^root:[ \t]*(\S.*?)[ \t\r]*$/m.exec(text)
  return {
    sessionId: id !== null && id[1] !== undefined ? id[1] : null,
    root: root !== null && root[1] !== undefined ? root[1] : null,
  }
}

function parseWorktrees(v: unknown): Worktree[] | undefined {
  const list = isRecord(v) ? v.worktrees : undefined
  if (!Array.isArray(list)) return undefined
  const out: Worktree[] = []
  for (const item of list as unknown[]) {
    if (!isRecord(item) || typeof item.path !== 'string') continue
    out.push({
      path: item.path,
      branch: typeof item.branch === 'string' ? item.branch : null,
      head: typeof item.head === 'string' ? item.head : null,
      owned: item.owned === true,
    })
  }
  return out
}

function parsePublicityResults(v: unknown): unknown[] | undefined {
  return isRecord(v) && Array.isArray(v.results) ? (v.results as unknown[]) : undefined
}

const PRIVATE_ISH = new Set(['PRIVATE', 'INTERNAL', 'LOCAL', 'NOT_GIT', 'FORKED'])

/** Folds lds `publicity` results[] into one level. Never throws. */
export function foldPublicity(results: unknown): Level {
  if (!Array.isArray(results) || results.length === 0) return 'unknown'
  let anyPublic = false
  let anyUnknown = false
  for (const item of results as unknown[]) {
    if (!isRecord(item)) {
      anyUnknown = true
      continue
    }
    const value = typeof item.publicity === 'string' ? item.publicity : 'UNKNOWN'
    const underlying =
      typeof item.underlying_visibility === 'string' ? item.underlying_visibility : undefined
    if (value === 'PUBLIC' || (value === 'FORKED' && underlying === 'PUBLIC')) {
      anyPublic = true
    } else if (!PRIVATE_ISH.has(value)) {
      anyUnknown = true
    }
  }
  if (anyPublic) return 'public'
  if (anyUnknown) return 'unknown'
  return 'private'
}

function toPublicityRows(results: unknown[]): PublicityRow[] {
  const rows: PublicityRow[] = []
  for (const item of results) {
    if (!isRecord(item)) continue
    const row: PublicityRow = {
      platform: typeof item.platform === 'string' ? item.platform : '?',
      publicity: typeof item.publicity === 'string' ? item.publicity : 'UNKNOWN',
    }
    if (typeof item.underlying_visibility === 'string') row.underlying = item.underlying_visibility
    rows.push(row)
  }
  return rows
}

// ---------------------------------------------------------------------------
// refresh: 4 lds calls in parallel, every failure folds into errors[]
// ---------------------------------------------------------------------------

type Interpreted<T> = { value: T | undefined; error: string | null }

function interpret<T>(
  name: string,
  settled: PromiseSettledResult<McpToolResult>,
  parse: (r: McpToolResult) => T | undefined,
): Interpreted<T> {
  if (settled.status === 'rejected') {
    return { value: undefined, error: `${name}: lds not connected` }
  }
  try {
    const r = settled.value
    if (r.isError === true) return { value: undefined, error: `${name}: lds returned an error` }
    const value = parse(r)
    return value === undefined
      ? { value: undefined, error: `${name}: unreadable result` }
      : { value, error: null }
  } catch {
    return { value: undefined, error: `${name}: unreadable result` }
  }
}

/** Never rejects: a hook must not throw because lds is away. */
export async function refresh($: Dollar, server: string): Promise<void> {
  try {
    const call = async (tool: string, args: Record<string, unknown>): Promise<McpToolResult> =>
      $.mcp.call(server, tool, args)
    const [info, trees, pub] = await Promise.allSettled([
      call('session_info', {}),
      call('git_worktree_list', {}),
      call('publicity', {}),
    ])

    const errors: string[] = []
    function take<T>(x: Interpreted<T>): T | undefined {
      if (x.error !== null) errors.push(x.error)
      return x.value
    }
    const session = take(
      interpret('session_info', info, r => {
        const text = textOf(r)
        if (text === undefined) return undefined
        const parsed = parseSessionInfo(text)
        return parsed.sessionId === null && parsed.root === null ? undefined : parsed
      }),
    )
    const worktrees = take(
      interpret('git_worktree_list', trees, r => firstParsed(r, parseWorktrees)),
    )
    const results = take(interpret('publicity', pub, r => firstParsed(r, parsePublicityResults)))

    let at = 0
    try {
      at = await $.clock.now()
    } catch {
      at = 0
    }
    const snap: Snapshot = {
      at,
      sessionId: session === undefined ? null : session.sessionId,
      root: session === undefined ? null : session.root,
      level: foldPublicity(results),
      publicity: results === undefined ? [] : toPublicityRows(results),
      worktrees: worktrees === undefined ? [] : worktrees,
      errors,
    }
    await update($, snapshot, () => snap)
  } catch {
    // The snapshot (or the state write) failed outright; the previous one stays.
  }
}

// ---------------------------------------------------------------------------
// View model (pure)
// ---------------------------------------------------------------------------

const LABEL_W = 10
const MAX_WORKTREES = 20

/** Keeps the tail of a path: `…/local-develop-server`. */
export function pathTail(path: string, max: number): string {
  if (max < 2 || path.length <= max) return path
  return `…${path.slice(path.length - (max - 1))}`
}

function headOf(text: string, max: number): string {
  if (max < 2 || text.length <= max) return text
  return `${text.slice(0, max - 1)}…`
}

export function formatAge(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  return `${Math.floor(m / 60)}h`
}

function levelTone(level: Level): Tone {
  if (level === 'public') return 'danger'
  if (level === 'private') return 'safe'
  return 'warn'
}

function rowLabel(row: PublicityRow): string {
  return row.publicity === 'FORKED' && row.underlying !== undefined
    ? `${row.platform}=FORKED(${row.underlying})`
    : `${row.platform}=${row.publicity}`
}

function rowTone(row: PublicityRow): Tone {
  return levelTone(
    foldPublicity([{ publicity: row.publicity, underlying_visibility: row.underlying }]),
  )
}

function toneProps(tone: Tone): ToneProps {
  switch (tone) {
    case 'danger':
      return { color: 'red', bold: true }
    case 'safe':
      return { color: 'green' }
    case 'warn':
      return { color: 'yellow' }
    case 'dim':
      return { dimColor: true }
    default:
      return {}
  }
}

function baseName(path: string | null): string {
  if (path === null) return '(unknown root)'
  const parts = path.split(/[\\/]/).filter(p => p !== '')
  const last = parts[parts.length - 1]
  return last === undefined ? path : last
}

/** What the pane lists, line by line. */
export function paneRows(snap: Snapshot, columns: number): Row[] {
  const room = Math.max(20, columns - LABEL_W - 2)
  const rows: Row[] = []

  const sid =
    snap.sessionId === null
      ? '(no session)'
      : snap.sessionId.length > 8
        ? `${snap.sessionId.slice(0, 8)}…`
        : snap.sessionId
  rows.push({
    label: 'lds',
    cells: [
      { text: sid, tone: 'plain' },
      {
        text: snap.root === null ? '(unknown root)' : pathTail(snap.root, Math.max(10, room - sid.length - 2)),
        tone: 'plain',
      },
    ],
  })

  rows.push({
    label: 'publicity',
    cells:
      snap.publicity.length === 0
        ? [{ text: 'unknown', tone: 'warn' }]
        : snap.publicity.map(p => ({ text: rowLabel(p), tone: rowTone(p) })),
  })

  if (snap.worktrees.length === 0) {
    rows.push({ label: 'worktrees', cells: [{ text: '(none)', tone: 'dim' }] })
  }
  snap.worktrees.slice(0, MAX_WORKTREES).forEach((w, i) => {
    rows.push({
      label: i === 0 ? 'worktrees' : '',
      cells: [
        { text: w.branch === null ? '(detached)' : w.branch, tone: 'plain' },
        { text: w.head === null ? '-' : w.head.slice(0, 7), tone: 'dim' },
        { text: w.owned ? 'owned' : '(not owned)', tone: w.owned ? 'safe' : 'dim' },
      ],
    })
  })
  if (snap.worktrees.length > MAX_WORKTREES) {
    rows.push({
      label: '',
      cells: [{ text: `… ${snap.worktrees.length - MAX_WORKTREES} more`, tone: 'dim' }],
    })
  }

  snap.errors.forEach((err, i) => {
    rows.push({
      label: i === 0 ? 'errors' : '',
      cells: [{ text: headOf(err, room), tone: 'dim' }],
    })
  })
  return rows
}

function bandCell(snap: Snapshot): Cell {
  return {
    text:
      snap.publicity.length === 0 ? 'publicity=unknown' : snap.publicity.map(rowLabel).join(' '),
    tone: levelTone(snap.level),
  }
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

export const register: Register = (on, options) => {
  const server = String(options.lds_server ?? '') || 'lds'
  const isBandEnabled = options.show_band !== false && options.show_band !== 'false'

  on('session.start', async ($, e, next) => {
    try {
      await $.command.register({
        name: 'lds',
        description: 'Show the lds session pane (root, publicity, worktrees)',
      })
    } catch {
      // the typeahead entry is a convenience; the snapshot below still helps the band
    }
    await refresh($, server)
    return next(e)
  })

  on('command.run', { command: 'lds' }, async $ => {
    await refresh($, server)
    try {
      await $.ui.open({ id: PANE_ID, title: 'lds' })
    } catch {
      return { text: 'lds: the pane could not be opened.' }
    }
    return { text: 'lds pane opened.' }
  })

  on('turn.complete', async ($, e, next) => {
    await refresh($, server)
    return next(e)
  })

  on('ui.render', { component: 'Pane', requestId: PANE_ID }, async ($, e) => {
    const { Box, Button, Text } = $.ui.resolve(e)
    const snap = await read($, snapshot)
    const isHidden = await read($, isBandHidden)

    if (snap === null) {
      return (
        <Box flexDirection="column">
          <Text dimColor>No lds snapshot yet. Run /lds.</Text>
          <Box>
            <Button key="refresh" label="Refresh" hotkey="r" onPress={() => refresh($, server)} />
          </Box>
        </Box>
      )
    }

    const now = await $.clock.now()
    const rows = paneRows(snap, e.props.bodyColumns)

    return (
      <Box flexDirection="column">
        {rows.map(row => (
          <Box flexDirection="row" gap={2}>
            <Text dimColor>{row.label.padEnd(LABEL_W)}</Text>
            {row.cells.map(cell => (
              <Text {...toneProps(cell.tone)}>{cell.text}</Text>
            ))}
          </Box>
        ))}
        <Box flexDirection="row" gap={2}>
          <Text dimColor>{`updated ${formatAge(now - snap.at)} ago`}</Text>
          <Button key="refresh" label="Refresh" hotkey="r" onPress={() => refresh($, server)} />
          <Button
            key="toggle-band"
            label={isHidden ? 'Show band' : 'Hide band'}
            hotkey="b"
            onPress={() => update($, isBandHidden, hidden => !hidden)}
          />
        </Box>
      </Box>
    )
  })

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    if (!isBandEnabled || e.props.hasSurvey) return next(e)
    const snap = await read($, snapshot)
    const isHidden = await read($, isBandHidden)
    if (snap === null || isHidden) return next(e)

    const { Box, Button, Text } = $.ui.resolve(e)
    const cell = bandCell(snap)

    return (
      <Box flexDirection="row" gap={2}>
        <Text bold>lds</Text>
        <Text>{baseName(snap.root)}</Text>
        <Text {...toneProps(cell.tone)}>{cell.text}</Text>
        <Button
          key="hide"
          label="Hide"
          hotkey="h"
          onPress={() => update($, isBandHidden, () => true)}
        />
      </Box>
    )
  })
}
