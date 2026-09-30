/**
 * `GET /usage`'s sanitized aggregate, as the mod reads it.
 *
 * The gateway's own shape (src/usage.rs): `{ pool, providers }`, each a status
 * string and the three tracked windows. Every number here is pool-wide — the
 * endpoint deliberately carries no account identity, count or priority.
 */

/** The three windows the gateway tracks, in the order they are shown. */
export const WINDOW_KEYS = ['5h', '7d', 'fable'] as const

export type WindowKey = (typeof WINDOW_KEYS)[number]

export type WindowStatus = {
  /**
   * The fraction of the pool's combined capacity still *unused*, `0..=1`, or
   * `null` where no non-disabled account reports the window. Headroom, not
   * consumption: a full bar is a fresh pool.
   */
  remaining: number | null
  /** Earliest reported reset, unix epoch seconds, or `null`. */
  resetsAt: number | null
}

export type PoolStatus = {
  /** `ok`, `degraded` or `exhausted`, derived from availability alone. */
  status: string
  windows: Record<WindowKey, WindowStatus>
}

export type UsageReport = {
  pool: PoolStatus
  /** One entry per pooled provider, by configured name, sorted. */
  providers: readonly (readonly [string, PoolStatus])[]
}

export type Parsed = { report: UsageReport } | { problem: string }

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)

/**
 * A finite number in `0..=1`, or `null` for anything else (including the
 * `null` the gateway sends for an unreported window).
 */
const fractionOf = (value: unknown): number | null =>
  typeof value === 'number' && Number.isFinite(value)
    ? Math.min(1, Math.max(0, value))
    : null

const epochOf = (value: unknown): number | null =>
  typeof value === 'number' && Number.isFinite(value) && value > 0 ? value : null

const windowOf = (value: unknown): WindowStatus => {
  const row = isRecord(value) ? value : {}

  return { remaining: fractionOf(row.remaining), resetsAt: epochOf(row.resets_at) }
}

const poolOf = (value: unknown): PoolStatus | null => {
  if (!isRecord(value)) {
    return null
  }

  const windows = isRecord(value.windows) ? value.windows : {}

  return {
    status: typeof value.status === 'string' ? value.status : 'unknown',
    windows: {
      '5h': windowOf(windows['5h']),
      '7d': windowOf(windows['7d']),
      fable: windowOf(windows.fable),
    },
  }
}

/**
 * Reads a `GET /usage` body, keeping every field the mod draws and refusing a
 * body that is not one — a proxy's HTML error page parses as neither.
 *
 * @param text the response body
 * @returns the report, or the reason it could not be read
 */
export function parseReport(text: string): Parsed {
  let value: unknown

  try {
    value = JSON.parse(text)
  } catch {
    return {
      problem:
        'the gateway answered GET /usage with something that is not ' +
        'JSON. Check that ANTHROPIC_BASE_URL points at the gateway itself and ' +
        'not at a proxy in front of it.',
    }
  }

  if (!isRecord(value)) {
    return { problem: 'the gateway answered GET /usage with an unexpected body.' }
  }

  const pool = poolOf(value.pool)

  if (pool === null) {
    return {
      problem:
        'the gateway answered GET /usage without a pool aggregate. ' +
        'This build may be older than the endpoint the mod reads.',
    }
  }

  const rows = isRecord(value.providers) ? value.providers : {}

  const providers = Object.keys(rows)
    .sort()
    .flatMap(name => {
      const status = poolOf(rows[name])

      return status === null ? [] : [[name, status] as const]
    })

  return { report: { pool, providers } }
}
