import type { McpToolResult, On } from 'claude-code'
import { expect, mock, test } from 'claude-code/testing'
import { foldPublicity, formatAge, parseSessionInfo, pathTail } from './register'

// --- fixtures ----------------------------------------------------------------

const SESSION_INFO = [
  'session_id: 18dd487a290c134d-6010e',
  'root: /home/u/proj/lds',
  'global_recipe_dirs: (default)',
  'justfiles:',
  '  - Global: /home/u/.config/lds/justfile',
  'external tools:',
  '  - git: available (/usr/bin/git)',
].join('\n')

const ldsText = (text: string): McpToolResult => ({
  content: [{ type: 'text', text }],
  isError: false,
})

const ldsJson = (value: unknown): McpToolResult => ldsText(JSON.stringify(value))

const ldsError: McpToolResult = {
  content: [{ type: 'text', text: 'lds: no session' }],
  isError: true,
}

const platform = (name: string, publicity: string) => ({
  platform: name,
  publicity,
  reason: 'test',
  detail: {},
})

const DEFAULTS: Record<string, McpToolResult> = {
  session_info: ldsText(SESSION_INFO),
  git_worktree_list: ldsJson({
    worktrees: [
      {
        path: '/home/u/proj/lds',
        head: '5795a59b0000000000000000000000000000000',
        branch: 'main',
        owned: false,
      },
      {
        path: '/home/u/proj/lds/.worktrees/x',
        head: '1a2b3c4d0000000000000000000000000000000',
        branch: 'topic/x',
        owned: true,
      },
    ],
  }),
  publicity: ldsJson({ results: [platform('github', 'PUBLIC'), platform('crates', 'PUBLIC')] }),
}

const START = 1_000_000

const RUN = {
  command: 'lds',
  args: '',
  origin: { kind: 'composer' as const },
  presentation: { isFullscreen: true, columns: 120 },
}

const SURFACES = ['terminal', 'desktop'] as const

const PANE_PROPS = {
  title: 'lds',
  isFocused: false,
  bodyColumns: 100,
  placement: 'dock' as const,
  scroll: { offset: 0, bodyRows: 30 },
  view: {},
}

const BAND_PROPS = {
  hasSurvey: false,
  isWorking: false,
  maxRows: 6,
  bodyColumns: 100,
  scroll: { offset: 0, bodyRows: 5 },
  view: {},
}

const VIEWPORT = { columns: 120, rows: 40, isFullscreen: true }

type World = {
  calls: { server: string; tool: string; args: Record<string, unknown> }[]
  opened: string[]
  registered: string[]
  clock: ReturnType<typeof mock.clock>
}

/**
 * Beneath the plugin: lds (`mcp.call`, by tool), the clock, the pane opener,
 * the command registry, and the engine's own answers for the events the plugin
 * passes on (`session.start`, `turn.complete`, `ui.render`). `answers` replaces
 * a tool's default; `'deny'` makes that call refuse.
 */
function mockWorld(on: On, answers: Record<string, McpToolResult | 'deny'> = {}): World {
  const world: World = {
    calls: [],
    opened: [],
    registered: [],
    clock: mock.clock(on, { now: START }),
  }
  on('mcp.call', (_$, e) => {
    world.calls.push({ server: e.server, tool: e.tool, args: e.args })
    const answer = answers[e.tool] ?? DEFAULTS[e.tool]
    if (answer === undefined || answer === 'deny') return { deny: `lds: ${e.tool} unavailable` }
    return { value: answer }
  })
  on('ui.open', (_$, e) => {
    world.opened.push(e.id)
    return { value: { isPlaced: true as const } }
  })
  on('command.register', (_$, e) => {
    world.registered.push(e.name)
    return { value: { command: e.name } }
  })
  on('session.start', (_$, e) => ({ cwd: e.cwd }))
  on('turn.complete', () => ({ text: '' }))
  on('ui.render', () => ({ type: 'Text', children: ['engine band'] }))
  return world
}

// --- pure helpers -------------------------------------------------------------

test('parseSessionInfo: takes session_id and root, null when a line is missing', () => {
  expect(parseSessionInfo(SESSION_INFO)).toEqual({
    sessionId: '18dd487a290c134d-6010e',
    root: '/home/u/proj/lds',
  })
  expect(parseSessionInfo('session_id: abc')).toEqual({ sessionId: 'abc', root: null })
  expect(parseSessionInfo('root: /x y')).toEqual({ sessionId: null, root: '/x y' })
  expect(parseSessionInfo('')).toEqual({ sessionId: null, root: null })
})

test('foldPublicity: PUBLIC -> public, PRIVATE only -> private, empty -> unknown', () => {
  expect(foldPublicity([platform('github', 'PUBLIC'), platform('crates', 'PRIVATE')])).toBe('public')
  expect(foldPublicity([platform('github', 'PRIVATE')])).toBe('private')
  expect(foldPublicity([platform('github', 'AMBIGUOUS')])).toBe('unknown')
  expect(foldPublicity([{ platform: 'github', publicity: 'FORKED', underlying_visibility: 'PUBLIC' }])).toBe(
    'public',
  )
  expect(foldPublicity([])).toBe('unknown')
  expect(foldPublicity(undefined)).toBe('unknown')
})

test('pathTail keeps the tail of a long path; formatAge counts s / m / h', () => {
  expect(pathTail('/home/u/projects/local-develop-server', 22)).toBe('…/local-develop-server')
  expect(pathTail('/short', 22)).toBe('/short')
  expect(formatAge(12_000)).toBe('12s')
  expect(formatAge(-5)).toBe('0s')
  expect(formatAge(5 * 60_000)).toBe('5m')
  expect(formatAge(3 * 3_600_000)).toBe('3h')
})

// --- /lds command -------------------------------------------------------------

test('/lds: refreshes from the 3 lds tools, opens the pane, answers { text }', async ($, on) => {
  const world = mockWorld(on)
  const r = await $.command.run(RUN)
  expect(r.text).toBe('lds pane opened.')
  expect(world.opened).toEqual(['lds-ops'])
  expect(world.calls.map(c => c.tool).sort()).toEqual([
    'git_worktree_list',
    'publicity',
    'session_info',
  ])
  expect(world.calls.every(c => c.server === 'lds')).toBe(true)
  const publicity = world.calls.find(c => c.tool === 'publicity')
  expect(publicity?.args).toEqual({})
})

test('session.start: registers /lds and fills the snapshot, but opens no pane', async ($, on) => {
  const world = mockWorld(on)
  await $.session.start({ cwd: '/repo', surface: 'terminal', isInteractive: true })
  expect(world.registered).toEqual(['lds'])
  expect(world.opened).toEqual([])
  expect(world.calls).toHaveLength(3)
})

// --- Pane ---------------------------------------------------------------------

test('Pane: shows publicity and worktrees on terminal and desktop', async ($, on) => {
  mockWorld(on)
  await $.command.run(RUN)
  for (const surface of SURFACES) {
    const pane = await $.ui.mount({
      plugin: 'lds-ops-pane',
      surface,
      component: 'Pane',
      requestId: 'lds-ops',
      props: PANE_PROPS,
      viewport: VIEWPORT,
    })
    expect(await pane.find({ type: 'Text', text: /PUBLIC/ })).toBeDefined()
    expect(await pane.find({ type: 'Text', text: /github=PUBLIC/ })).toBeDefined()
    expect(await pane.find({ type: 'Text', text: /topic\/x/ })).toBeDefined()
    expect(await pane.find({ type: 'Text', text: /5795a59/ })).toBeDefined()
    expect(await pane.find({ type: 'Text', text: /18dd487a/ })).toBeDefined()
    expect(await pane.find({ type: 'Button', key: 'refresh' })).toBeDefined()
    expect(await pane.find({ type: 'Text', text: /errors/ })).toBeUndefined()
    await pane.unmount()
  }
})

for (const surface of SURFACES) {
  test(`Pane (${surface}): without a snapshot it says so, and Refresh fills it`, async ($, on) => {
    const world = mockWorld(on)
    const pane = await $.ui.mount({
      plugin: 'lds-ops-pane',
      surface,
      component: 'Pane',
      requestId: 'lds-ops',
      props: PANE_PROPS,
      viewport: VIEWPORT,
    })
    expect(await pane.find({ type: 'Text', text: /No lds snapshot yet/ })).toBeDefined()
    expect(world.calls).toHaveLength(0)

    await pane.press({ key: 'refresh' })
    expect(world.calls).toHaveLength(3)
    expect(await pane.find({ type: 'Text', text: /No lds snapshot yet/ })).toBeUndefined()
    expect(await pane.find({ type: 'Text', text: /github=PUBLIC/ })).toBeDefined()
  })
}

test('refresh: an isError publicity folds into errors and the band reads unknown', async ($, on) => {
  mockWorld(on, { publicity: ldsError })
  await $.command.run(RUN)
  for (const surface of SURFACES) {
    const pane = await $.ui.mount({
      plugin: 'lds-ops-pane',
      surface,
      component: 'Pane',
      requestId: 'lds-ops',
      props: PANE_PROPS,
      viewport: VIEWPORT,
    })
    expect(await pane.find({ type: 'Text', text: /publicity: lds returned an error/ })).toBeDefined()
    expect(await pane.find({ type: 'Text', text: /github=PUBLIC/ })).toBeUndefined()
    await pane.unmount()

    const band = await $.ui.mount({
      plugin: 'lds-ops-pane',
      surface,
      component: 'AbovePrompt',
      requestId: 'band',
      props: BAND_PROPS,
      viewport: VIEWPORT,
    })
    expect(await band.find({ type: 'Text', text: /publicity=unknown/ })).toBeDefined()
    expect(await band.find({ type: 'Text', text: /PUBLIC/ })).toBeUndefined()
    await band.unmount()
  }
})

test('refresh: lds refusing every call still yields a snapshot with errors', async ($, on) => {
  mockWorld(on, {
    session_info: 'deny',
    git_worktree_list: 'deny',
    publicity: 'deny',
  })
  const r = await $.command.run(RUN)
  expect(r.text).toBe('lds pane opened.')
  const pane = await $.ui.mount({
    plugin: 'lds-ops-pane',
    surface: 'terminal',
    component: 'Pane',
    requestId: 'lds-ops',
    props: PANE_PROPS,
    viewport: VIEWPORT,
  })
  expect(await pane.find({ type: 'Text', text: /publicity: / })).toBeDefined()
  expect(await pane.find({ type: 'Text', text: /session_info: / })).toBeDefined()
  expect(await pane.find({ type: 'Text', text: /No lds snapshot yet/ })).toBeUndefined()
})

// --- band ---------------------------------------------------------------------

test('band: yields to the engine while there is no snapshot', async ($, on) => {
  mockWorld(on)
  for (const surface of SURFACES) {
    const band = await $.ui.mount({
      plugin: 'lds-ops-pane',
      surface,
      component: 'AbovePrompt',
      requestId: 'band',
      props: BAND_PROPS,
      viewport: VIEWPORT,
    })
    expect(await band.find({ type: 'Text', text: /engine band/ })).toBeDefined()
    expect(await band.find({ type: 'Button', key: 'hide' })).toBeUndefined()
    await band.unmount()
  }
})

test('band: Hide yields to the engine, and the Pane button turns into Show band', async ($, on) => {
  mockWorld(on)
  await $.command.run(RUN)
  for (const surface of SURFACES) {
    const pane = await $.ui.mount({
      plugin: 'lds-ops-pane',
      surface,
      component: 'Pane',
      requestId: 'lds-ops',
      props: PANE_PROPS,
      viewport: VIEWPORT,
    })
    const band = await $.ui.mount({
      plugin: 'lds-ops-pane',
      surface,
      component: 'AbovePrompt',
      requestId: 'band',
      props: BAND_PROPS,
      viewport: VIEWPORT,
    })
    expect(await band.find({ type: 'Text', text: /github=PUBLIC crates=PUBLIC/ })).toBeDefined()
    expect((await pane.find({ type: 'Button', key: 'toggle-band' }))?.text).toBe('Hide band')

    await band.press({ key: 'hide' })
    expect(await band.find({ type: 'Button', key: 'hide' })).toBeUndefined()
    expect(await band.find({ type: 'Text', text: /engine band/ })).toBeDefined()
    expect((await pane.find({ type: 'Button', key: 'toggle-band' }))?.text).toBe('Show band')

    await pane.press({ key: 'toggle-band' })
    expect(await band.find({ type: 'Button', key: 'hide' })).toBeDefined()
    expect((await pane.find({ type: 'Button', key: 'toggle-band' }))?.text).toBe('Hide band')

    await band.unmount()
    await pane.unmount()
  }
})

test('band: yields while a survey holds it', async ($, on) => {
  mockWorld(on)
  await $.command.run(RUN)
  const band = await $.ui.mount({
    plugin: 'lds-ops-pane',
    surface: 'terminal',
    component: 'AbovePrompt',
    requestId: 'band',
    props: { ...BAND_PROPS, hasSurvey: true },
    viewport: VIEWPORT,
  })
  expect(await band.find({ type: 'Text', text: /engine band/ })).toBeDefined()
  expect(await band.find({ type: 'Button', key: 'hide' })).toBeUndefined()
})

test('band: show_band=false always yields to the engine', { options: { show_band: false } }, async ($, on) => {
  mockWorld(on)
  await $.command.run(RUN)
  for (const surface of SURFACES) {
    const band = await $.ui.mount({
      plugin: 'lds-ops-pane',
      surface,
      component: 'AbovePrompt',
      requestId: 'band',
      props: BAND_PROPS,
      viewport: VIEWPORT,
    })
    expect(await band.find({ type: 'Text', text: /engine band/ })).toBeDefined()
    expect(await band.find({ type: 'Button', key: 'hide' })).toBeUndefined()
    await band.unmount()
  }
})

// --- turn.complete -------------------------------------------------------------

test('turn.complete: refreshes the snapshot, so its age restarts', async ($, on) => {
  const world = mockWorld(on)
  await $.command.run(RUN)
  const pane = await $.ui.mount({
    plugin: 'lds-ops-pane',
    surface: 'terminal',
    component: 'Pane',
    requestId: 'lds-ops',
    props: PANE_PROPS,
    viewport: VIEWPORT,
  })
  expect(await pane.find({ type: 'Text', text: /updated 0s ago/ })).toBeDefined()

  await world.clock.advance(30_000)
  await pane.redraw()
  expect(await pane.find({ type: 'Text', text: /updated 30s ago/ })).toBeDefined()

  const callsBefore = world.calls.length
  await $.turn.complete({
    answer: 'done',
    durationMs: 1000,
    isAborted: false,
    turnId: 't1',
    reason: 'answer',
  })
  expect(world.calls.length).toBe(callsBefore + 3)
  expect(await pane.find({ type: 'Text', text: /updated 0s ago/ })).toBeDefined()
})
