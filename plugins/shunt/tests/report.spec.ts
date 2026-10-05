import { describe, expect, test } from 'vitest'

import { parseReport } from '../hooks/report'
import { USAGE_BODY } from './fixtures'

const report = (text: string) => {
  const parsed = parseReport(text)

  if ('problem' in parsed) {
    throw new Error(`expected a report, got: ${parsed.problem}`)
  }

  return parsed.report
}

describe('report', () => {
  test('reads the pool aggregate the gateway sends', () => {
    const { pool } = report(JSON.stringify(USAGE_BODY))

    expect(pool.status).toBe('degraded')
    expect(pool.windows['5h']).toEqual({
      remaining: 0.6234,
      resetsAt: 1_800_000_000,
    })
  })

  test('an unreported window stays null rather than becoming zero', () => {
    const { providers } = report(JSON.stringify(USAGE_BODY))
    const codex = providers.find(([name]) => name === 'codex')?.[1]

    expect(codex?.windows.fable).toEqual({ remaining: null, resetsAt: null })
    // The neighbouring 0.0 is a real exhausted reading, not a missing one.
    expect(codex?.windows['5h'].remaining).toBe(0)
  })

  test('providers come back sorted, whatever order they arrived in', () => {
    const { providers } = report(JSON.stringify(USAGE_BODY))

    expect(providers.map(([name]) => name)).toEqual(['claude', 'codex'])
  })

  test('a fraction outside 0..1 is clamped', () => {
    const { pool } = report(
      JSON.stringify({
        pool: { status: 'ok', windows: { '5h': { remaining: 1.4 } } },
        providers: {},
      }),
    )

    expect(pool.windows['5h'].remaining).toBe(1)
  })

  test("a proxy's HTML page is refused, not read as a pool", () => {
    const parsed = parseReport('<html><body>502 Bad Gateway</body></html>')

    expect(parsed).toHaveProperty('problem')
    expect('problem' in parsed && parsed.problem).toContain('not ')
  })

  test('a body with no pool aggregate is refused', () => {
    const parsed = parseReport(JSON.stringify({ providers: {} }))

    expect(parsed).toHaveProperty('problem')
  })

  test('a provider row that is not an object is dropped, not drawn', () => {
    const { providers } = report(
      JSON.stringify({
        pool: { status: 'ok', windows: {} },
        providers: { good: { status: 'ok', windows: {} }, bad: 'nonsense' },
      }),
    )

    expect(providers.map(([name]) => name)).toEqual(['good'])
  })
})
