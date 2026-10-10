import type { McpToolResult, Register } from 'claude-code'

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type Classification =
  | { kind: 'remote'; what: string }
  | { kind: 'local-git-write'; what: string }
  | { kind: 'none' }

export type PublicityLevel = {
  level: 'public' | 'private' | 'unknown'
  summary: string
}

type Verdict = {
  decision: 'allow' | 'ask' | 'deny'
  reason?: string
}

// The slice of `$` this module uses. `$` of the hook is structurally assignable.
type Dollar = {
  mcp: {
    call: (
      server: string,
      tool: string,
      args?: Record<string, unknown>,
    ) => Promise<McpToolResult>
  }
  session: { cwd: () => Promise<string> }
  clock: { now: () => Promise<number> }
  ui: { toast: (text: string) => void }
}

// ---------------------------------------------------------------------------
// classify: Bash command -> Classification (pure, synchronous)
// ---------------------------------------------------------------------------

const NONE: Classification = { kind: 'none' }

const WRAPPERS = new Set(['sudo', 'env', 'command', 'exec', 'nohup', 'time'])
const SKIP_WORDS = new Set(['!', '{', '(', 'then', 'do', 'else'])
const SUDO_VALUE_FLAGS = new Set(['-u', '-g', '-h', '-p', '-C', '-r', '-t', '-U'])
const ENV_ASSIGN = /^[A-Za-z_][A-Za-z0-9_]*=/
const GIT_VALUE_OPTS = new Set([
  '-C',
  '-c',
  '--git-dir',
  '--work-tree',
  '--namespace',
  '--super-prefix',
  '--config-env',
])
const WRITE_METHODS = new Set(['POST', 'PUT', 'PATCH', 'DELETE'])

const GH_REMOTE_PAIRS = new Set([
  'pr create',
  'pr merge',
  'pr close',
  'pr edit',
  'pr comment',
  'pr review',
  'issue create',
  'issue close',
  'issue edit',
  'issue comment',
  'release create',
  'release upload',
  'release delete',
  'release edit',
  'repo create',
  'repo delete',
  'repo edit',
  'workflow run',
  'run cancel',
  'run rerun',
])

/** Splits on && || ; | newline outside quotes. */
function splitSegments(command: string): string[] {
  const segments: string[] = []
  let cur = ''
  let quote: string | null = null
  const push = (): void => {
    if (cur.trim() !== '') segments.push(cur)
    cur = ''
  }
  for (let i = 0; i < command.length; i++) {
    const ch = command.charAt(i)
    if (quote !== null) {
      cur += ch
      if (ch === '\\' && quote === '"' && i + 1 < command.length) {
        i++
        cur += command.charAt(i)
      } else if (ch === quote) {
        quote = null
      }
      continue
    }
    if (ch === '"' || ch === "'") {
      quote = ch
      cur += ch
      continue
    }
    if (ch === '\\') {
      cur += ch
      if (i + 1 < command.length) {
        i++
        cur += command.charAt(i)
      }
      continue
    }
    if (ch === '\n' || ch === ';') {
      push()
      continue
    }
    if (ch === '|') {
      push()
      if (command.charAt(i + 1) === '|') i++
      continue
    }
    if (ch === '&' && command.charAt(i + 1) === '&') {
      push()
      i++
      continue
    }
    cur += ch
  }
  push()
  return segments
}

/** Splits a segment on whitespace outside quotes; quote characters are dropped. */
function tokenize(segment: string): string[] {
  const tokens: string[] = []
  let cur = ''
  let has = false
  let quote: string | null = null
  for (let i = 0; i < segment.length; i++) {
    const ch = segment.charAt(i)
    if (quote !== null) {
      if (ch === quote) {
        quote = null
      } else if (ch === '\\' && quote === '"' && i + 1 < segment.length) {
        i++
        cur += segment.charAt(i)
      } else {
        cur += ch
      }
      continue
    }
    if (ch === '"' || ch === "'") {
      quote = ch
      has = true
      continue
    }
    if (ch === '\\' && i + 1 < segment.length) {
      i++
      cur += segment.charAt(i)
      has = true
      continue
    }
    if (/\s/.test(ch)) {
      if (has) tokens.push(cur)
      cur = ''
      has = false
      continue
    }
    cur += ch
    has = true
  }
  if (has) tokens.push(cur)
  return tokens
}

/** Skips env assignments and wrappers; returns the executable name and its args. */
function findCommand(tokens: string[]): { name: string; args: string[] } | undefined {
  let i = 0
  while (i < tokens.length) {
    const raw = tokens[i]
    if (raw === undefined) return undefined
    const t = raw.replace(/^\(+/, '')
    if (t === '' || SKIP_WORDS.has(t) || ENV_ASSIGN.test(t)) {
      i++
      continue
    }
    if (WRAPPERS.has(t)) {
      i++
      for (;;) {
        const opt = tokens[i]
        if (opt === undefined || !opt.startsWith('-')) break
        i++
        if (t === 'sudo' && SUDO_VALUE_FLAGS.has(opt)) i++
      }
      continue
    }
    const slash = t.lastIndexOf('/')
    return { name: slash >= 0 ? t.slice(slash + 1) : t, args: tokens.slice(i + 1) }
  }
  return undefined
}

/** `-X POST` / `-XPOST` / `-sX POST` / `--request POST` / `--method=POST`. */
function extractMethod(args: string[]): string | undefined {
  for (let i = 0; i < args.length; i++) {
    const t = args[i]
    if (t === undefined) continue
    if (t === '-X' || t === '--request' || t === '--method' || /^-[A-Za-z]*X$/.test(t)) {
      return args[i + 1]?.toUpperCase()
    }
    if (t.startsWith('--request=')) return t.slice('--request='.length).toUpperCase()
    if (t.startsWith('--method=')) return t.slice('--method='.length).toUpperCase()
    const m = /^-[A-Za-z]*?X([A-Za-z]+)$/.exec(t)
    if (m !== null && m[1] !== undefined) return m[1].toUpperCase()
  }
  return undefined
}

function positionals(args: string[]): string[] {
  return args.filter(a => !a.startsWith('-'))
}

function classifyGit(args: string[]): Classification {
  let i = 0
  for (;;) {
    const a = args[i]
    if (a === undefined || !a.startsWith('-')) break
    i += GIT_VALUE_OPTS.has(a) ? 2 : 1
  }
  const sub = args[i]
  if (sub === undefined) return NONE
  const rest = args.slice(i + 1)
  switch (sub) {
    case 'push':
      return { kind: 'remote', what: 'git push' }
    case 'commit':
    case 'merge':
    case 'rebase':
    case 'cherry-pick':
    case 'reset':
      return { kind: 'local-git-write', what: `git ${sub}` }
    case 'worktree': {
      const action = positionals(rest)[0]
      if (action === 'add' || action === 'remove' || action === 'prune') {
        return { kind: 'local-git-write', what: `git worktree ${action}` }
      }
      return NONE
    }
    case 'branch':
      return rest.some(a => a === '--delete' || /^-[A-Za-z]*[dD][A-Za-z]*$/.test(a))
        ? { kind: 'local-git-write', what: 'git branch -d' }
        : NONE
    case 'stash': {
      const action = positionals(rest)[0]
      if (action === 'drop' || action === 'pop' || action === 'apply' || action === 'clear') {
        return { kind: 'local-git-write', what: `git stash ${action}` }
      }
      return NONE
    }
    case 'checkout':
      return rest.some(a => a === '-b' || a === '-B')
        ? { kind: 'local-git-write', what: 'git checkout -b' }
        : NONE
    case 'switch':
      return rest.some(a => a === '-c' || a === '-C' || a === '--create')
        ? { kind: 'local-git-write', what: 'git switch -c' }
        : NONE
    default:
      return NONE
  }
}

function classifyGh(args: string[]): Classification {
  const pos: string[] = []
  for (let i = 0; i < args.length; i++) {
    const a = args[i]
    if (a === undefined) continue
    if (a === '-R' || a === '--repo') {
      i++
      continue
    }
    if (a.startsWith('-')) continue
    pos.push(a)
  }
  const group = pos[0]
  const action = pos[1]
  if (group === undefined) return NONE
  if (group === 'api') {
    const method = extractMethod(args)
    if (method !== undefined) {
      return method === 'GET' ? NONE : { kind: 'remote', what: 'gh api' }
    }
    const writes = args.some(
      a =>
        a === '-f' ||
        a === '-F' ||
        a === '--field' ||
        a === '--raw-field' ||
        a === '--input' ||
        a.startsWith('--field=') ||
        a.startsWith('--raw-field=') ||
        a.startsWith('--input='),
    )
    return writes ? { kind: 'remote', what: 'gh api' } : NONE
  }
  if (action !== undefined && GH_REMOTE_PAIRS.has(`${group} ${action}`)) {
    return { kind: 'remote', what: `gh ${group} ${action}` }
  }
  return NONE
}

function classifyHttp(name: string, args: string[]): Classification {
  const what = `${name} (write request)`
  const remote: Classification = { kind: 'remote', what }
  const method = extractMethod(args)
  if (method !== undefined && WRITE_METHODS.has(method)) return remote
  if (name === 'curl') {
    const hasData = args.some(
      a =>
        a === '-d' ||
        a === '-F' ||
        a === '-T' ||
        a.startsWith('--data') ||
        a.startsWith('--form') ||
        a.startsWith('--upload-file') ||
        a.startsWith('--json'),
    )
    return hasData ? remote : NONE
  }
  if (name === 'wget') {
    const hasBody = args.some(
      a =>
        a.startsWith('--post-data') ||
        a.startsWith('--post-file') ||
        a.startsWith('--body-data') ||
        a.startsWith('--body-file'),
    )
    return hasBody ? remote : NONE
  }
  // httpie / xh: `-d` means download there, so only a leading method word counts.
  const first = positionals(args)[0]
  return first !== undefined && WRITE_METHODS.has(first.toUpperCase()) ? remote : NONE
}

function classifyCloud(name: string, args: string[]): Classification {
  const pos = positionals(args)
  if (pos.includes('deploy')) return { kind: 'remote', what: `${name} deploy` }
  if (name === 'aws') {
    const s3 = pos.indexOf('s3')
    const sub = s3 >= 0 ? pos[s3 + 1] : undefined
    if (sub === 'cp' || sub === 'sync') {
      const operands = pos.slice(s3 + 2)
      if (operands.some((p, idx) => idx >= 1 && p.startsWith('s3://'))) {
        return { kind: 'remote', what: `aws s3 ${sub}` }
      }
    }
  }
  return NONE
}

function classifySegment(name: string, args: string[]): Classification {
  switch (name) {
    case 'git':
      return classifyGit(args)
    case 'gh':
      return classifyGh(args)
    case 'cargo':
      return args.find(a => !a.startsWith('-') && !a.startsWith('+')) === 'publish'
        ? { kind: 'remote', what: 'cargo publish' }
        : NONE
    case 'npm':
    case 'pnpm':
    case 'yarn':
      return positionals(args)[0] === 'publish'
        ? { kind: 'remote', what: `${name} publish` }
        : NONE
    case 'docker':
    case 'podman':
      return positionals(args)[0] === 'push' ? { kind: 'remote', what: `${name} push` } : NONE
    case 'gcloud':
    case 'aws':
    case 'az':
      return classifyCloud(name, args)
    case 'curl':
    case 'wget':
    case 'http':
    case 'xh':
      return classifyHttp(name, args)
    default:
      return NONE
  }
}

/** Pure, synchronous. A `remote` segment wins over a `local-git-write` one. */
export function classify(command: string): Classification {
  let local: Classification = NONE
  for (const segment of splitSegments(command)) {
    const found = findCommand(tokenize(segment))
    if (found === undefined) continue
    const c = classifySegment(found.name, found.args)
    if (c.kind === 'remote') return c
    if (c.kind === 'local-git-write' && local.kind === 'none') local = c
  }
  return local
}

// ---------------------------------------------------------------------------
// publicity folding / lookup
// ---------------------------------------------------------------------------

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v)
}

function unknownLevel(summary: string): PublicityLevel {
  return { level: 'unknown', summary }
}

/** Folds lds `publicity` results[] into one level. Never throws. */
export function foldPublicity(results: unknown): PublicityLevel {
  if (!Array.isArray(results) || results.length === 0) {
    return unknownLevel('lds publicity の results が空です')
  }
  const parts: string[] = []
  let anyPublic = false
  let anyUnknown = false
  for (const item of results as unknown[]) {
    if (!isRecord(item)) {
      anyUnknown = true
      parts.push('?=UNKNOWN')
      continue
    }
    const platform = typeof item.platform === 'string' ? item.platform : '?'
    const value = typeof item.publicity === 'string' ? item.publicity : 'UNKNOWN'
    const underlying =
      typeof item.underlying_visibility === 'string' ? item.underlying_visibility : undefined
    parts.push(
      value === 'FORKED' && underlying !== undefined
        ? `${platform}=FORKED(${underlying})`
        : `${platform}=${value}`,
    )
    if (value === 'PUBLIC' || (value === 'FORKED' && underlying === 'PUBLIC')) {
      anyPublic = true
    } else if (
      value === 'PRIVATE' ||
      value === 'INTERNAL' ||
      value === 'LOCAL' ||
      value === 'NOT_GIT' ||
      value === 'FORKED'
    ) {
      // private-ish
    } else {
      anyUnknown = true
    }
  }
  const summary = parts.join(', ')
  if (anyPublic) return { level: 'public', summary }
  if (anyUnknown) return { level: 'unknown', summary }
  return { level: 'private', summary }
}

function textOf(r: McpToolResult): string | undefined {
  for (const block of r.content) {
    if (block.type === 'text' && typeof block.text === 'string') return block.text
  }
  return undefined
}

/** Calls lds `publicity`; every failure folds to `unknown`. Never rejects. */
async function lookupPublicity(
  $: Dollar,
  server: string,
): Promise<PublicityLevel> {
  let r: McpToolResult
  try {
    r = await $.mcp.call(server, 'publicity', {})
  } catch {
    return unknownLevel('lds に接続できません')
  }
  try {
    if (r.isError === true) return unknownLevel('lds publicity が error を返しました')
    const candidates: unknown[] = []
    if (r.structuredContent !== undefined && r.structuredContent !== null) {
      candidates.push(r.structuredContent)
    }
    const text = textOf(r)
    if (text !== undefined) {
      try {
        candidates.push(JSON.parse(text))
      } catch {
        // fall through to the next candidate
      }
    }
    for (const c of candidates) {
      if (isRecord(c) && Array.isArray(c.results)) return foldPublicity(c.results)
    }
    return unknownLevel('lds publicity の結果を解釈できませんでした')
  } catch {
    return unknownLevel('lds publicity の結果を解釈できませんでした')
  }
}

const TTL_MS = 5 * 60 * 1000
const cache = new Map<string, { at: number; value: PublicityLevel }>()

async function publicity($: Dollar, server: string): Promise<PublicityLevel> {
  let cwd = '?'
  try {
    cwd = await $.session.cwd()
  } catch {
    // keep '?'
  }
  let now: number | undefined
  try {
    now = await $.clock.now()
  } catch {
    now = undefined
  }
  const hit = cache.get(cwd)
  if (hit !== undefined && now !== undefined && now - hit.at < TTL_MS) return hit.value
  const value = await lookupPublicity($, server)
  if (value.level !== 'unknown' && now !== undefined) cache.set(cwd, { at: now, value })
  return value
}

// ---------------------------------------------------------------------------
// hook
// ---------------------------------------------------------------------------

function commandOf(input: unknown): string {
  if (typeof input === 'object' && input !== null) {
    const c = (input as { command?: unknown }).command
    if (typeof c === 'string') return c
  }
  return ''
}

function deny($: Dollar, reason: string): Verdict {
  try {
    $.ui.toast(reason.slice(0, 80))
  } catch {
    // no UI on this surface; the verdict still stands
  }
  return { decision: 'deny', reason }
}

async function decideRemote($: Dollar, server: string, what: string): Promise<Verdict> {
  const p = await publicity($, server)
  if (p.level === 'public') {
    return deny(
      $,
      `lds-leak-guard: ${what} を拒否。${p.summary}。この repo は PUBLIC です。remote への書込は User が shell で実行してください`,
    )
  }
  if (p.level === 'unknown') {
    return {
      decision: 'ask',
      reason: `lds-leak-guard: ${what} は remote への書込です。publicity を判定できませんでした (${p.summary})。実行前に確認してください`,
    }
  }
  return {
    decision: 'ask',
    reason: `lds-leak-guard: ${what} は remote への書込です (${p.summary})。実行前に確認してください`,
  }
}

export const register: Register = (on, options) => {
  const server = String(options.lds_server ?? '') || 'lds'
  const guardLocal =
    options.guard_local_git_writes !== false && options.guard_local_git_writes !== 'false'

  on('tool.check', { tool: 'Bash' }, ($, e, next) => {
    const c = classify(commandOf(e.input))
    if (c.kind === 'none') return next(e)
    if (c.kind === 'local-git-write') {
      return guardLocal
        ? deny(
            $,
            `lds-leak-guard: ${c.what} は Bash ではなく mcp__lds__git_* を使ってください (session ownership guard の対象外になります)`,
          )
        : next(e)
    }
    return decideRemote($, server, c.what)
  }).catch(($, e, next) =>
    next.called
      ? next(e)
      : { decision: 'deny', reason: 'lds-leak-guard: guard 自体が失敗したため拒否しました' },
  )
}
