/**
 * The mod's hooks against the engine itself, under `claude plugin test`: on a
 * shunt gateway the band shows the pool tagged `shunt ·`; anywhere else it
 * shows the session's own rate limits; and it stays out of the way when there
 * is nothing to show or the option turns it off.
 *
 * vitest runs only `*.spec.ts`, and `tsconfig.json` excludes this folder,
 * since `claude-code/testing` exists only inside the engine.
 */
import { describe, expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

const SURFACES = ['terminal', 'desktop'] as const

const GATEWAY = {
  ANTHROPIC_BASE_URL: 'http://127.0.0.1:3001',
  ANTHROPIC_AUTH_TOKEN: 'token',
}

/** 2h 55m before the pool's 5-hour window resets. */
const NOW_MS = 1_799_989_500_000

/** 1h 41m after `NOW_MS`, as the session's own limits spell a reset. */
const SESSION_RESET = new Date(NOW_MS + 101 * 60_000).toISOString()

const bodyOf = (fiveHour: number, providers: Record<string, string> = {}) => {
  const windows = {
    '5h': { remaining: fiveHour, resets_at: 1_800_000_000 },
    '7d': { remaining: 0.54, resets_at: 1_800_300_000 },
    fable: { remaining: null, resets_at: null },
  }

  return JSON.stringify({
    pool: { status: 'ok', windows },
    providers: Object.fromEntries(
      Object.entries(providers).map(([name, status]) => [name, { status, windows }]),
    ),
  })
}

type Limit = { kind: string; percentUsed: number; resetsAt?: string }

const PROPS = {
  hasSurvey: false,
  isWorking: false,
  maxRows: 6,
  bodyColumns: 120,
  scroll: { offset: 0, bodyRows: 5 },
  view: {},
}

/**
 * Stands in for the gateway, the session's own usage, the session start and
 * the engine's own band beneath the plugin, and counts the requests that
 * reached the gateway.
 */
const world = (
  on: On,
  env: Record<string, string>,
  answer: (headers: Record<string, string>) => [number, string],
  rateLimits: Limit[] | 'unavailable' = [],
  helper: {
    command?: string
    tokens: string[]
    failure?: { exitCode: number; stderr: string }
  } = { tokens: [] },
) => {
  const clock = mock.clock(on, { now: NOW_MS })
  const calls: string[] = []
  const runs: (readonly string[])[] = []

  on('settings.read', () => ({
    value: helper.command === undefined ? {} : { apiKeyHelper: helper.command },
  }))
  on('process.run', ($, e) => {
    runs.push(e.argv)

    if (helper.failure !== undefined) {
      return { value: { exitCode: helper.failure.exitCode, stdout: '', stderr: helper.failure.stderr } }
    }

    return { value: { exitCode: 0, stdout: `${helper.tokens[runs.length - 1] ?? ''}\n`, stderr: '' } }
  })

  mock.env(on, env)
  on('session.start', ($, e) => ({ cwd: e.cwd }))
  on('session.usage', () =>
    rateLimits === 'unavailable'
      ? { deny: 'unavailable' }
      : { value: { startedAt: 0, context: { window: 200_000 }, rateLimits } },
  )
  on('session.measure', ($, e) => ({ changed: e.changed }))
  on('turn.complete', () => ({ text: '' }))
  on('http.fetch', ($, e) => {
    calls.push(e.url)
    const [status, text] = answer(e.init?.headers ?? {})

    return { value: { status, ok: status >= 200 && status < 300, headers: {}, text } }
  })
  // The engine's own band, beneath the plugin: empty.
  on('ui.render', { component: 'AbovePrompt' }, ($, e) => {
    const { Box } = $.ui.resolve(e)

    return <Box key="engine" />
  })

  return { clock, calls, runs }
}

const START = { cwd: '/', surface: 'terminal', isInteractive: true } as const

describe('usage band on shunt', () => {
  for (const surface of SURFACES) {
    test(`draws the pool used, tagged, with countdowns and flags on ${surface}`, async ($, on) => {
      const { clock, calls } = world(
        on,
        GATEWAY,
        () => [200, bodyOf(0.95, { anthropic: 'degraded', codex: 'ok' })],
        [{ kind: 'five_hour', percentUsed: 12 }],
      )

      await $.session.start(START)
      await clock.settle()

      const ui = await $.ui.mount({ plugin: 'shunt', surface, component: 'AbovePrompt', props: PROPS })

      expect(calls).toEqual(['http://127.0.0.1:3001/usage'])
      expect((await ui.find({ key: 'usage' }))?.text).toBe(
        'shunt · 5H 5% ↻2h 55m · WK 46% ↻3d 14h ⚠ anthropic degraded',
      )
      expect((await ui.find({ type: 'Text', text: '⚠ anthropic degraded' }))?.props.color).toBe('yellow')
    })
  }

  test('reads the pool again every minute and after each turn', async ($, on) => {
    let fiveHour = 0.62
    const { clock, calls } = world(on, GATEWAY, () => [200, bodyOf(fiveHour)])

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    fiveHour = 0.05
    await clock.advance(60_000)

    expect((await ui.find({ key: '5h' }))?.text).toBe('5H 95% ↻2h 54m')
    expect((await ui.find({ type: 'Text', text: '95%' }))?.props.color).toBe('red')

    await $.turn.complete({
      answer: 'done',
      durationMs: 1000,
      isAborted: false,
      turnId: 'turn-1',
      reason: 'answer',
    })
    await clock.settle()

    expect(calls).toHaveLength(3)
  })

  test('polls the pool even when the session usage cannot be read', async ($, on) => {
    const { clock, calls } = world(on, GATEWAY, () => [200, bodyOf(0.5)], 'unavailable')

    await $.session.start(START)
    await clock.advance(60_000)

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect(calls).toHaveLength(2)
    expect((await ui.find({ key: '5h' }))?.text).toBe('5H 50% ↻2h 54m')
  })

  test('flags a refused token instead of falling back', async ($, on) => {
    const { clock } = world(on, GATEWAY, () => [401, ''], [{ kind: 'five_hour', percentUsed: 12 }])

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect((await ui.find({ key: 'usage' }))?.text).toBe('shunt · ⚠ credential refused')
  })
})

describe('usage band in a shunt gateway claude session', () => {
  /** What `shunt gateway claude` leaves: a base URL, no credential variable. */
  const LAUNCHED = { ANTHROPIC_BASE_URL: 'http://127.0.0.1:3001' }
  const HELPER = "'/opt/shunt/bin/shunt' gateway token"

  /** A gateway that takes only the login tokens listed. */
  const accepting =
    (...tokens: string[]) =>
    (headers: Record<string, string>): [number, string] =>
      tokens.some(token => headers.authorization === `Bearer ${token}`) ? [200, bodyOf(0.6)] : [401, '']

  test('reads the pool with the login token the helper prints', async ($, on) => {
    const { clock, runs } = world(on, LAUNCHED, accepting('login-1'), [{ kind: 'five_hour', percentUsed: 12 }], {
      command: HELPER,
      tokens: ['login-1'],
    })

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect(runs).toEqual([['/opt/shunt/bin/shunt', 'gateway', 'token']])
    expect((await ui.find({ key: 'usage' }))?.text).toBe('shunt · 5H 40% ↻2h 55m · WK 46% ↻3d 14h')
  })

  test('reuses the token for five minutes, then asks the helper again', async ($, on) => {
    const { clock, calls, runs } = world(on, LAUNCHED, accepting('login-1', 'login-2'), [], {
      command: HELPER,
      tokens: ['login-1', 'login-2'],
    })

    await $.session.start(START)
    await clock.advance(4 * 60_000)

    expect(calls).toHaveLength(5)
    expect(runs).toHaveLength(1)

    await clock.advance(60_000)

    expect(runs).toHaveLength(2)
  })

  test('asks the helper again when the gateway refuses a reused token', async ($, on) => {
    let accepted = ['login-1']
    const { clock, runs } = world(
      on,
      LAUNCHED,
      headers => accepting(...accepted)(headers),
      [],
      { command: HELPER, tokens: ['login-1', 'login-2'] },
    )

    await $.session.start(START)
    await clock.settle()

    accepted = ['login-2']
    await clock.advance(60_000)

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect(runs).toHaveLength(2)
    expect((await ui.find({ key: 'usage' }))?.text).toBe('shunt · 5H 40% ↻2h 54m · WK 46% ↻3d 14h')
  })

  test('flags a shunt helper that fails instead of falling back', async ($, on) => {
    const { clock, runs } = world(on, LAUNCHED, accepting('login-1'), [{ kind: 'five_hour', percentUsed: 12 }], {
      command: HELPER,
      tokens: [],
      failure: { exitCode: 1, stderr: 'not logged in' },
    })

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect(runs).toEqual([['/opt/shunt/bin/shunt', 'gateway', 'token']])
    expect((await ui.find({ key: 'usage' }))?.text).toBe('shunt · ⚠ gateway login unavailable')
  })

  test('leaves any other apiKeyHelper alone', async ($, on) => {
    const { clock, calls, runs } = world(on, LAUNCHED, accepting('login-1'), [{ kind: 'five_hour', percentUsed: 12 }], {
      command: 'op read op://vault/anthropic/key',
      tokens: ['secret'],
    })

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect(runs).toEqual([])
    expect(calls).toEqual([])
    expect((await ui.find({ key: 'usage' }))?.text).toBe('5H 12%')
  })
})

describe('usage band off shunt', () => {
  test('draws the session rate limits untagged', async ($, on) => {
    const { clock, calls } = world(on, {}, () => [200, bodyOf(0.5)], [
      { kind: 'five_hour', percentUsed: 5, resetsAt: SESSION_RESET },
      { kind: 'seven_day', percentUsed: 46 },
    ])

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect(calls).toEqual([])
    expect((await ui.find({ key: 'usage' }))?.text).toBe('5H 5% ↻1h 41m · WK 46%')
  })

  test('falls back when the gateway does not serve GET /usage', async ($, on) => {
    const { clock } = world(on, GATEWAY, () => [404, ''], [{ kind: 'five_hour', percentUsed: 7 }])

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect((await ui.find({ key: 'usage' }))?.text).toBe('5H 7%')
  })

  test('follows the limits each measurement pushes', async ($, on) => {
    const { clock } = world(on, {}, () => [200, ''], [{ kind: 'five_hour', percentUsed: 5 }])

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    await $.session.measure({
      context: { window: 200_000 },
      rateLimits: [{ kind: 'five_hour', percentUsed: 91 }],
      changed: ['rateLimits'],
    })

    expect((await ui.find({ key: 'usage' }))?.text).toBe('5H 91%')
    expect((await ui.find({ type: 'Text', text: '91%' }))?.props.color).toBe('red')
  })

  test('stays empty with no gateway and no limits', async ($, on) => {
    const { clock } = world(on, {}, () => [200, bodyOf(0.5)])

    await $.session.start(START)
    await clock.settle()

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect(await ui.find({ key: 'usage' })).toBeUndefined()
  })
})

describe('usageBand off', () => {
  test('polls nothing and draws nothing', { options: { usageBand: false } }, async ($, on) => {
    const { clock, calls } = world(on, GATEWAY, () => [200, bodyOf(0.5)], [
      { kind: 'five_hour', percentUsed: 5 },
    ])

    await $.session.start(START)
    await clock.advance(120_000)

    const ui = await $.ui.mount({ plugin: 'shunt', surface: 'terminal', component: 'AbovePrompt', props: PROPS })

    expect(calls).toEqual([])
    expect(await ui.find({ key: 'usage' })).toBeUndefined()
  })

  test('/shunt:usage still answers', { options: { usageBand: false } }, async ($, on) => {
    world(on, GATEWAY, () => [200, bodyOf(0.6234)])

    const { text } = await $.command.run({ command: 'shunt:usage' })

    expect(text).toContain('pool — ok   http://127.0.0.1:3001')
    expect(text).toContain('62% left')
  })
})
