import { describe, expect, test } from 'vitest'

import type { Endpoint } from '../hooks/endpoint'
import { NOT_ENABLED_TEXT, NO_POOL_TEXT, REFUSED_TEXT } from '../hooks/names'
import { readingOf, unreachableOf } from '../hooks/reading'

import { USAGE_BODY } from './fixtures'

const ENDPOINT: Endpoint = {
  base: 'http://127.0.0.1:3001',
  url: 'http://127.0.0.1:3001/usage',
  headers: { authorization: 'Bearer token' },
}

const answered = (status: number, text: string) =>
  readingOf(ENDPOINT, { status, ok: status >= 200 && status < 300, text })

describe('reading', () => {
  test('a pool body is a report from the endpoint base', () => {
    const reading = answered(200, JSON.stringify(USAGE_BODY))

    expect(reading).toMatchObject({ base: ENDPOINT.base })
    expect('report' in reading && reading.report.pool.status).toBe('degraded')
  })

  test('a disabled endpoint keeps the band quiet', () => {
    expect(answered(404, '')).toEqual({ problem: NOT_ENABLED_TEXT, brief: null })
  })

  test('a refused credential is shown in the band', () => {
    expect(answered(401, '')).toEqual({
      problem: REFUSED_TEXT,
      brief: 'credential refused',
    })
  })

  test('another error status carries the body first line', () => {
    expect(answered(502, 'upstream down\n<html>')).toEqual({
      problem: 'the gateway answered GET /usage with 502 — upstream down',
      brief: 'GET /usage answered 502',
    })
  })

  test('a body that is not a pool report is not shunt', () => {
    expect(answered(200, '<html>')).toMatchObject({ brief: null })
  })

  test('a gateway that pools no provider keeps the band quiet', () => {
    const body = {
      pool: {
        status: 'ok',
        windows: {
          '5h': { remaining: null, resets_at: null },
          '7d': { remaining: null, resets_at: null },
          fable: { remaining: null, resets_at: null },
        },
      },
      providers: {},
    }

    expect(answered(200, JSON.stringify(body))).toEqual({
      problem: NO_POOL_TEXT,
      brief: null,
    })
  })

  test('an unreachable gateway names the base and drops the engine stamp', () => {
    expect(unreachableOf(ENDPOINT, new Error('shunt: connection refused'))).toEqual({
      problem:
        'could not reach the gateway at http://127.0.0.1:3001 — connection refused',
      brief: 'gateway unreachable',
    })
  })
})
