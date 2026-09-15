import type { On } from 'claude-code'

import { endpointOf } from './endpoint'
import {
  COMMAND_NAME,
  NOT_ENABLED_TEXT,
  NO_POOL_TEXT,
  REFUSED_TEXT,
} from './names'
import { WINDOW_KEYS, parseReport } from './report'
import { reportText } from './views'

/**
 * An error's message, less the `shunt: ` the engine stamps on its own — the
 * transcript line already carries the plugin's name, so keeping it would read
 * `shunt: ... \u2014 shunt: ...`.
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
 * Registers the mod's one hook: `/shunt:usage`, answered from the gateway's
 * `GET /usage` rather than sent to the model.
 *
 * The command itself is `commands/usage.md`, which lists as `shunt:usage`
 * because the plugin namespaces it; the markdown body is the fallback for a
 * session without function hooks, and this hook answers before it is ever
 * read. `$.command.register` cannot serve this command: it takes a bare global
 * name and refuses `usage` as the built-in's.
 *
 * @param on the engine's registrar
 */
export function register(on: On) {
  on('command.run', { command: COMMAND_NAME }, async ($, e, next) => {
    const resolved = endpointOf({
      shuntBaseUrl: await $.env.get('SHUNT_BASE_URL'),
      anthropicBaseUrl: await $.env.get('ANTHROPIC_BASE_URL'),
      shuntToken: await $.env.get('SHUNT_TOKEN'),
      anthropicAuthToken: await $.env.get('ANTHROPIC_AUTH_TOKEN'),
      anthropicApiKey: await $.env.get('ANTHROPIC_API_KEY'),
    })

    if ('problem' in resolved) {
      return { text: resolved.problem }
    }

    const { endpoint } = resolved

    let response

    try {
      response = await $.http.fetch(endpoint.url, { headers: endpoint.headers })
    } catch (error) {
      return {
        text:
          `could not reach the gateway at ${endpoint.base} ` +
          `${'—'} ${messageOf(error)}`,
      }
    }

    if (response.status === 404) {
      return { text: NOT_ENABLED_TEXT }
    }

    if (response.status === 401 || response.status === 403) {
      return { text: REFUSED_TEXT }
    }

    if (!response.ok) {
      const detail = firstLineOf(response.text)

      return {
        text:
          `the gateway answered GET /usage with ${response.status}` +
          `${detail === '' ? '' : ` ${'—'} ${detail}`}`,
      }
    }

    const parsed = parseReport(response.text)

    if ('problem' in parsed) {
      return { text: parsed.problem }
    }

    const { report } = parsed

    const isSilent =
      report.providers.length === 0 &&
      WINDOW_KEYS.every(key => report.pool.windows[key].remaining === null)

    if (isSilent) {
      return { text: NO_POOL_TEXT }
    }

    return {
      text: reportText(report, endpoint.base, await $.clock.now()),
    }
  })
}
