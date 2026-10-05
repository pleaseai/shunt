import type { Endpoint } from './endpoint'
import { NOT_ENABLED_TEXT, NO_POOL_TEXT, REFUSED_TEXT } from './names'
import type { UsageReport } from './report'
import { WINDOW_KEYS, parseReport } from './report'

/**
 * What one `GET /usage` round trip came to: the report, or the reason there
 * is none.
 *
 * `problem` is the full sentence `/shunt:usage` prints. `brief` is the few
 * words the band above the prompt flags instead, or `null` where the session
 * is not on a shunt gateway that serves `GET /usage`: no gateway is set, it
 * answers 404 or with a body that is not a pool report (another proxy), or it
 * pools no provider. Those are set-ups, not faults, and the band shows the
 * session's own rate limits for them rather than nagging.
 */
export type Reading =
  | { report: UsageReport; base: string }
  | { problem: string; brief: string | null }

/** The band's brief for a 401 or 403: the gateway refused the credential. */
export const CREDENTIAL_REFUSED = 'credential refused'

/** The subset of the engine's `HttpResponse` a reading needs. */
export type Response = { status: number; ok: boolean; text: string }

/**
 * An error's message, less the `shunt: ` the engine stamps on its own — the
 * transcript line already carries the plugin's name, so keeping it would read
 * `shunt: ... — shunt: ...`.
 */
const messageOf = (error: unknown): string =>
  (error instanceof Error ? error.message : String(error)).replace(
    /^shunt: /,
    '',
  )

/**
 * The first line of a body, clipped, for an error the mod has no words of its
 * own for — enough to recognize the gateway's own message without pasting a
 * page of HTML into the transcript.
 */
const firstLineOf = (text: string): string => {
  const line = text.split('\n', 1)[0]?.trim() ?? ''

  return line.length > 200 ? `${line.slice(0, 200)}…` : line
}

/**
 * The reading for a request that never got an answer.
 *
 * @param endpoint where it was sent
 * @param error what `$.http.fetch` rejected with
 */
export function unreachableOf(endpoint: Endpoint, error: unknown): Reading {
  return {
    problem:
      `could not reach the gateway at ${endpoint.base} ` +
      `${'—'} ${messageOf(error)}`,
    brief: 'gateway unreachable',
  }
}

/**
 * Reads the gateway's answer to `GET /usage`.
 *
 * @param endpoint where it was sent
 * @param response the status and body the gateway answered with
 */
export function readingOf(endpoint: Endpoint, response: Response): Reading {
  if (response.status === 404) {
    return { problem: NOT_ENABLED_TEXT, brief: null }
  }

  if (response.status === 401 || response.status === 403) {
    return { problem: REFUSED_TEXT, brief: CREDENTIAL_REFUSED }
  }

  if (!response.ok) {
    const detail = firstLineOf(response.text)

    return {
      problem:
        `the gateway answered GET /usage with ${response.status}` +
        `${detail === '' ? '' : ` ${'—'} ${detail}`}`,
      brief: `GET /usage answered ${response.status}`,
    }
  }

  const parsed = parseReport(response.text)

  if ('problem' in parsed) {
    return { problem: parsed.problem, brief: null }
  }

  const { report } = parsed

  const isSilent =
    report.providers.length === 0 &&
    WINDOW_KEYS.every(key => report.pool.windows[key].remaining === null)

  if (isSilent) {
    return { problem: NO_POOL_TEXT, brief: null }
  }

  return { report, base: endpoint.base }
}
