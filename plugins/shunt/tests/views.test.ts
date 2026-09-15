import { describe, expect, test } from 'vitest'

import { parseReport } from '../hooks/report'
import { barOf, percentOf, reportText, resetTextOf } from '../hooks/views'
import { USAGE_BODY } from './fixtures'

// 2027-01-14 04:13 UTC, a Thursday; the suite pins TZ=UTC (vitest.config.ts).
const NOW_MS = 1_799_900_000_000

const report = () => {
  const parsed = parseReport(JSON.stringify(USAGE_BODY))

  if ('problem' in parsed) {
    throw new Error(parsed.problem)
  }

  return parsed.report
}

describe('views', () => {
  test('the bar fills with the headroom that is left', () => {
    expect(barOf(1)).toBe('▓'.repeat(10))
    expect(barOf(0)).toBe('░'.repeat(10))
    expect(barOf(0.6234)).toBe(`${'▓'.repeat(6)}${'░'.repeat(4)}`)
  })

  test('an unreported window draws no bar', () => {
    expect(barOf(null).trim()).toBe('')
  })

  test('headroom reads as whole percent, and as a dash when unreported', () => {
    expect(percentOf(0.6234).trim()).toBe('62%')
    expect(percentOf(null).trim()).toBe('—')
  })

  test('a sliver of headroom does not round down to 0%', () => {
    expect(percentOf(0.004).trim()).toBe('1%')
    expect(percentOf(0).trim()).toBe('0%')
  })

  test('a hair short of full does not round up to 100%', () => {
    expect(percentOf(0.998).trim()).toBe('99%')
    expect(percentOf(1).trim()).toBe('100%')
  })

  test('a reset within the day shows the clock alone', () => {
    expect(resetTextOf(NOW_MS / 1000 + 3 * 3600, NOW_MS)).toBe('resets 07:13')
  })

  test('a reset beyond the day names the weekday too', () => {
    expect(resetTextOf(NOW_MS / 1000 + 4 * 86_400, NOW_MS)).toBe(
      'resets Mon 04:13',
    )
  })

  test('a reset already past reads as due, not as a past time', () => {
    expect(resetTextOf(NOW_MS / 1000 - 60, NOW_MS)).toBe('resets due')
  })

  test('an unreported reset says nothing at all', () => {
    expect(resetTextOf(null, NOW_MS)).toBe('')
  })

  test('the report draws the pool, each provider, and what the numbers mean', () => {
    const text = reportText(report(), 'http://127.0.0.1:3001', NOW_MS)

    expect(text).toContain('pool — degraded')
    expect(text).toContain('http://127.0.0.1:3001')
    expect(text).toContain('5h')
    expect(text).toContain('fable')
    expect(text).toContain('claude')
    expect(text).toContain('exhausted')
    // The legend is load-bearing: a bare 62% invites the opposite reading.
    expect(text).toContain('headroom left')
  })

  test('with no pooled provider it draws the pool alone', () => {
    const parsed = parseReport(
      JSON.stringify({ pool: USAGE_BODY.pool, providers: {} }),
    )

    if ('problem' in parsed) {
      throw new Error(parsed.problem)
    }

    const text = reportText(parsed.report, 'http://gateway', NOW_MS)

    expect(text).toContain('pool — degraded')
    expect(text).not.toContain('claude')
  })
})
