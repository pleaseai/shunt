import type { ShuntRateLimit, ShuntSnapshot } from '../types'

import type { Reading } from './reading'
import type { WindowKey } from './report'
import { WINDOW_KEYS } from './report'
import { percentOf } from './views'

/** How loudly a figure is drawn: plain, amber, red. */
export type Level = 'ok' | 'warn' | 'hot'

/** One window as the band draws it. */
export type BandCell = {
  key: string
  /** `5H`, `WK`, `Fable`, `Spend`. */
  label: string
  /** How much of the window is used, whole percent: `46%`. */
  percent: string
  /** Time to the window's reset (`1h 41m`), `due`, or `''`. */
  reset: string
  level: Level
}

/** A status worth flagging after the cells: `anthropic degraded`. */
export type BandAlert = { text: string; level: Level }

/**
 * The band's whole content. `isShunt` puts the `shunt ·` tag in front: the
 * figures are the gateway pool's, not the session's own account's.
 */
export type Band = { isShunt: boolean; cells: BandCell[]; alerts: BandAlert[] }

/** Usage from which a window turns amber, then red. */
const WARN_FROM = 0.7
const HOT_FROM = 0.9

const SHUNT_LABELS: Readonly<Record<WindowKey, string>> = {
  '5h': '5H',
  '7d': 'WK',
  fable: 'Fable',
}

const LIMIT_LABELS: Readonly<Record<string, string>> = {
  five_hour: '5H',
  seven_day: 'WK',
  spend_limit: 'Spend',
}

const STATUS_LEVELS: Readonly<Record<string, Level>> = {
  ok: 'ok',
  degraded: 'warn',
  exhausted: 'hot',
  capped: 'hot',
}

/**
 * The level of one window's usage, a fraction: red from nine tenths used,
 * amber from seven.
 */
export const levelOf = (used: number): Level =>
  used >= HOT_FROM ? 'hot' : used >= WARN_FROM ? 'warn' : 'ok'

/**
 * A status's level. One this build does not know is amber rather than plain:
 * the gateway said something other than `ok`.
 */
export const statusLevelOf = (status: string): Level =>
  STATUS_LEVELS[status] ?? 'warn'

/**
 * The time from `nowMs` to `resetsAt` (epoch seconds) as the band shows it:
 * `4d 0h`, `2h 55m`, `12m`; `due` once it has passed, `''` with no reset.
 *
 * A countdown rather than the clock time `/shunt:usage` prints: the band sits
 * there for hours, and "in 2h 55m" stays readable without a calendar.
 */
export const countdownOf = (resetsAt: number | null, nowMs: number): string => {
  if (resetsAt === null) {
    return ''
  }

  const ms = resetsAt * 1000 - nowMs

  if (ms <= 0) {
    return 'due'
  }

  const minutes = Math.max(1, Math.round(ms / 60_000))
  const days = Math.floor(minutes / 1440)
  const hours = Math.floor((minutes % 1440) / 60)
  const rest = minutes % 60

  return days > 0 ? `${days}d ${hours}h` : hours > 0 ? `${hours}h ${rest}m` : `${rest}m`
}

/**
 * What the band keeps of a reading: the pool and each provider's status, the
 * brief reason, or `null` where the session is not on a shunt gateway that
 * serves `GET /usage` — the band then shows the session's own rate limits.
 */
export function snapshotOf(reading: Reading): ShuntSnapshot | null {
  if ('report' in reading) {
    const { pool, providers } = reading.report

    return {
      pool,
      providers: providers.map(([name, provider]) => ({
        name,
        status: provider.status,
      })),
    }
  }

  return reading.brief === null ? null : { problem: reading.brief }
}

const cellOf = (
  key: string,
  label: string,
  used: number,
  resetsAt: number | null,
  nowMs: number,
): BandCell => ({
  key,
  label,
  percent: percentOf(used).trim(),
  reset: countdownOf(resetsAt, nowMs),
  level: levelOf(used),
})

/**
 * The gateway pool's band. Usage is `1 - remaining`, the mean share of the
 * pool's capacity spent, so it reads the same way as the session's own limits
 * do without the tag. A window no account reports is left out; each provider
 * whose status is not `ok` is flagged, or the pool itself where none is.
 */
function shuntBandOf(snapshot: ShuntSnapshot, nowMs: number): Band {
  if ('problem' in snapshot) {
    return {
      isShunt: true,
      cells: [],
      alerts: [{ text: snapshot.problem, level: 'warn' }],
    }
  }

  const { pool, providers } = snapshot

  const cells = WINDOW_KEYS.flatMap(key => {
    const { remaining, resetsAt } = pool.windows[key]

    return remaining === null
      ? []
      : [cellOf(key, SHUNT_LABELS[key], 1 - remaining, resetsAt, nowMs)]
  })

  const flagged = providers
    .filter(provider => provider.status !== 'ok')
    .map(provider => ({
      text: `${provider.name} ${provider.status}`,
      level: statusLevelOf(provider.status),
    }))

  const alerts =
    flagged.length > 0 || pool.status === 'ok'
      ? flagged
      : [{ text: `pool ${pool.status}`, level: statusLevelOf(pool.status) }]

  return { isShunt: true, cells, alerts }
}

const epochOf = (iso: string | undefined): number | null => {
  const ms = iso === undefined ? NaN : Date.parse(iso)

  return Number.isFinite(ms) ? ms / 1000 : null
}

/** The session's own rate limits, as Claude Code reports them. */
function limitsBandOf(limits: readonly ShuntRateLimit[], nowMs: number): Band {
  const cells = limits.map(limit =>
    cellOf(
      limit.kind,
      LIMIT_LABELS[limit.kind] ?? limit.kind.replace(/_/g, ' '),
      limit.percentUsed / 100,
      epochOf(limit.resetsAt),
      nowMs,
    ),
  )

  return { isShunt: false, cells, alerts: [] }
}

/**
 * What the band draws: the gateway pool when the session is on shunt, else
 * the session's own rate limits, else nothing (`null`) — an API-key session
 * off any gateway has no limits to show.
 */
export function bandOf(
  snapshot: ShuntSnapshot | null,
  limits: readonly ShuntRateLimit[],
  nowMs: number,
): Band | null {
  if (snapshot !== null) {
    return shuntBandOf(snapshot, nowMs)
  }

  return limits.length === 0 ? null : limitsBandOf(limits, nowMs)
}
