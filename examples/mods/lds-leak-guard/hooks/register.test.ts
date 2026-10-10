import type { McpToolResult, On } from 'claude-code'
import { expect, mock, test } from 'claude-code/testing'
import { classify, foldPublicity } from './register'

// --- helpers ---------------------------------------------------------------

const ldsResult = (results: unknown[]): McpToolResult => ({
  content: [{ type: 'text', text: JSON.stringify({ results }) }],
  isError: false,
})

const ldsError: McpToolResult = {
  content: [{ type: 'text', text: 'lds: no session' }],
  isError: true,
}

const github = (publicity: string, extra: Record<string, unknown> = {}) => ({
  platform: 'github',
  publicity,
  reason: 'test',
  detail: {},
  ...extra,
})

/**
 * Beneath the plugin: answers lds `publicity`, the session's cwd, the clock,
 * the toast, and stands in for the engine's own `tool.check` verdict (allow).
 * Each test gets its own cwd so the module-level cache never crosses tests.
 */
function mockWorld(
  on: On,
  cwd: string,
  answer: McpToolResult,
): { calls: number; lastArgs: Record<string, unknown> | undefined } {
  const counter: { calls: number; lastArgs: Record<string, unknown> | undefined } = {
    calls: 0,
    lastArgs: undefined,
  }
  mock.clock(on)
  on('mcp.call', (_$, e) => {
    counter.calls++
    counter.lastArgs = e.args
    return { value: answer }
  })
  on('session.cwd', () => ({ value: cwd }))
  on('ui.toast', () => ({ value: undefined }))
  on('tool.check', () => ({ decision: 'allow', reason: 'engine' }))
  return counter
}

// --- classify (pure) --------------------------------------------------------

test('classify: a heredoc body is not read as commands', () => {
  const script = ["python3 - <<'EOF'", "print('git push origin main')", 'EOF'].join('\n')
  expect(classify(script)).toEqual({ kind: 'none' })
  const tabbed = ['cat <<-TAG > out.txt', '\tgit push', '\tTAG'].join('\n')
  expect(classify(tabbed)).toEqual({ kind: 'none' })
})

test('classify: a command after a heredoc is still seen', () => {
  const chain = ['cat <<EOF > notes.txt', 'hello', 'EOF', 'git push origin main'].join('\n')
  expect(classify(chain).kind).toBe('remote')
})

test('classify: git push -> remote', () => {
  expect(classify('git push origin main').kind).toBe('remote')
})

test('classify: cd && git -C <path> push --force -> remote', () => {
  expect(classify('cd /x && git -C /y push --force').kind).toBe('remote')
})

test('classify: gh pr create -> remote', () => {
  const c = classify('gh pr create --title t --body-file f')
  expect(c.kind).toBe('remote')
  expect(c).toEqual({ kind: 'remote', what: 'gh pr create' })
})

test('classify: cargo publish --dry-run -> remote', () => {
  expect(classify('cargo publish --dry-run').kind).toBe('remote')
})

test('classify: read-only git chain -> none', () => {
  expect(classify('git status && git log -3').kind).toBe('none')
})

test('classify: git commit -> local-git-write', () => {
  expect(classify('git commit -m x').kind).toBe('local-git-write')
})

test('classify: git worktree list is none, worktree add is local-git-write', () => {
  expect(classify('git worktree list').kind).toBe('none')
  expect(classify('git worktree add ../w').kind).toBe('local-git-write')
})

test('classify: curl GET is none, curl -X POST -d is remote', () => {
  expect(classify('curl -s https://x/api').kind).toBe('none')
  expect(classify('curl -X POST -d a=b https://x').kind).toBe('remote')
})

// --- $.tool.check end to end ------------------------------------------------

test('tool.check: git push on a PUBLIC repo is denied', async ($, on) => {
  const world = mockWorld(on, '/repo/public', ldsResult([github('PUBLIC')]))
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'git push origin main' } })
  expect(r.decision).toBe('deny')
  expect(r.reason).toContain('PUBLIC')
  // lds is asked about the directory the command runs in, not its own root
  expect(world.lastArgs).toEqual({ root: '/repo/public' })
})

test('tool.check: git push on a PRIVATE repo asks', async ($, on) => {
  mockWorld(on, '/repo/private', ldsResult([github('PRIVATE')]))
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'git push origin main' } })
  expect(r.decision).toBe('ask')
})

test('tool.check: git push on a FORKED repo whose upstream is PUBLIC is denied', async ($, on) => {
  mockWorld(
    on,
    '/repo/forked',
    ldsResult([github('FORKED', { underlying_visibility: 'PUBLIC' })]),
  )
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'git push origin main' } })
  expect(r.decision).toBe('deny')
})

test('tool.check: lds isError folds to unknown and asks', async ($, on) => {
  mockWorld(on, '/repo/error', ldsError)
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'git push origin main' } })
  expect(r.decision).toBe('ask')
  expect(r.reason).toContain('判定できませんでした')
})

test('tool.check: git commit is denied and points to mcp__lds__git_*', async ($, on) => {
  mockWorld(on, '/repo/local', ldsResult([github('PRIVATE')]))
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'git commit -m x' } })
  expect(r.decision).toBe('deny')
  expect(r.reason).toContain('mcp__lds__git_')
})

test('tool.check: ls -la never calls lds and passes through next(e)', async ($, on) => {
  const counter = mockWorld(on, '/repo/ls', ldsResult([github('PUBLIC')]))
  const r = await $.tool.check({ tool: 'Bash', input: { command: 'ls -la' } })
  expect(counter.calls).toBe(0)
  expect(r.decision).toBe('allow')
  expect(r.reason).toBe('engine')
})

// --- foldPublicity ----------------------------------------------------------

test('foldPublicity: empty results fold to unknown', () => {
  expect(foldPublicity([]).level).toBe('unknown')
  expect(foldPublicity(undefined).level).toBe('unknown')
})
