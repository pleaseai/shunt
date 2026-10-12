import { describe, expect, test } from 'vitest'

import {
  bandOf,
  countdownOf,
  levelOf,
  snapshotOf,
  statusLevelOf,
} from '../hooks/band'
import { parseReport } from '../hooks/report'

import { USAGE_BODY } from './fixtures'

const NOW_MS = 1_799_989_500_000

const report = () => {
  const parsed = parseReport(JSON.stringify(USAGE_BODY))

  if ('problem' in parsed) {
    throw new Error(parsed.problem)
  }

  return parsed.report
}

describe('countdown', () => {
  test.each([
    [null, ''],
    [NOW_MS / 1000 - 1, 'due'],
    [NOW_MS / 1000 + 12 * 60, '12m'],
    [NOW_MS / 1000 + 2 * 3600 + 55 * 60, '2h 55m'],
    [NOW_MS / 1000 + 4 * 86400 + 20 * 60, '4d 0h'],
  ])('%s reads %j', (resetsAt, text) => {
    expect(countdownOf(resetsAt, NOW_MS)).toBe(text)
  })
})

describe('levels', () => {
  test.each([
    [0.69, 'ok'],
    [0.7, 'warn'],
    [0.89, 'warn'],
    [0.9, 'hot'],
  ] as const)('usage %s is %s', (used, level) => {
    expect(levelOf(used)).toBe(level)
  })

  test.each([
    ['ok', 'ok'],
    ['degraded', 'warn'],
    ['exhausted', 'hot'],
    ['capped', 'hot'],
    ['something-new', 'warn'],
  ] as const)('status %s is %s', (status, level) => {
    expect(statusLevelOf(status)).toBe(level)
  })
})

describe('snapshot', () => {
  test('keeps the pool aggregate and each provider status', () => {
    expect(snapshotOf({ report: report(), base: 'b' })).toEqual({
      pool: report().pool,
      providers: [
        { name: 'claude', status: 'ok' },
        { name: 'codex', status: 'exhausted' },
      ],
    })
  })

  test('off shunt there is no snapshot', () => {
    expect(snapshotOf({ problem: 'set-up', brief: null })).toBeNull()
  })

  test('a fault on shunt keeps its brief', () => {
    expect(snapshotOf({ problem: 'long', brief: 'short' })).toEqual({
      problem: 'short',
    })
  })
})

describe('shunt band', () => {
  test('draws each reported window used, with its countdown, and flags providers', () => {
    const snapshot = snapshotOf({ report: report(), base: 'b' })

    expect(bandOf(snapshot, [], NOW_MS)).toEqual({
      isShunt: true,
      cells: [
        { key: '5h', label: '5H', percent: '38%', reset: '2h 55m', level: 'ok' },
        { key: '7d', label: 'WK', percent: '19%', reset: '3d 14h', level: 'ok' },
        { key: 'fable', label: 'Fable', percent: '81%', reset: '3d 14h', level: 'warn' },
      ],
      alerts: [{ text: 'codex exhausted', level: 'hot' }],
    })
  })

  test('leaves out a window no account reports', () => {
    const pool = {
      ...report().pool,
      windows: {
        ...report().pool.windows,
        fable: { remaining: null, resetsAt: null },
      },
    }

    const band = bandOf({ pool, providers: [] }, [], NOW_MS)

    expect(band?.cells.map(cell => cell.label)).toEqual(['5H', 'WK'])
  })

  test('flags the pool when no provider explains its status', () => {
    const band = bandOf({ pool: report().pool, providers: [] }, [], NOW_MS)

    expect(band?.alerts).toEqual([{ text: 'pool degraded', level: 'warn' }])
  })

  test('a fault draws its brief as the one alert', () => {
    expect(bandOf({ problem: 'gateway unreachable' }, [], NOW_MS)).toEqual({
      isShunt: true,
      cells: [],
      alerts: [{ text: 'gateway unreachable', level: 'warn' }],
    })
  })

  test('wins over the session rate limits', () => {
    const band = bandOf(
      { pool: report().pool, providers: [] },
      [{ kind: 'five_hour', percentUsed: 5 }],
      NOW_MS,
    )

    expect(band?.isShunt).toBe(true)
  })
})

describe('session band', () => {
  test('draws the session rate limits untagged', () => {
    const resetsAt = new Date(NOW_MS + (60 + 41) * 60_000).toISOString()

    expect(
      bandOf(
        null,
        [
          { kind: 'five_hour', percentUsed: 5, resetsAt },
          { kind: 'seven_day', percentUsed: 92.5 },
          { kind: 'spend_limit', percentUsed: 110 },
        ],
        NOW_MS,
      ),
    ).toEqual({
      isShunt: false,
      cells: [
        { key: 'five_hour', label: '5H', percent: '5%', reset: '1h 41m', level: 'ok' },
        { key: 'seven_day', label: 'WK', percent: '93%', reset: '', level: 'hot' },
        { key: 'spend_limit', label: 'Spend', percent: '110%', reset: '', level: 'hot' },
      ],
      alerts: [],
    })
  })

  test('no limits and no gateway draw nothing', () => {
    expect(bandOf(null, [], NOW_MS)).toBeNull()
  })
})
