import { atom, read, update } from 'claude-code'
import type { EngineInterface, On, PluginOptions } from 'claude-code'

import type { ShuntRateLimit, ShuntSnapshot } from '../types'

import { bandOf, snapshotOf } from './band'
import type { Level } from './band'
import type { Endpoint, Environment } from './endpoint'
import { endpointOf, helperMayRideTo } from './endpoint'
import { shuntHelperArgvOf } from './helper'
import { COMMAND_NAME, HOOK_FAILED_TEXT, NO_TOKEN_TEXT } from './names'
import type { Reading } from './reading'
import {
  CREDENTIAL_REFUSED,
  helperFailedOf,
  readingOf,
  unreachableOf,
} from './reading'
import { reportText } from './views'

/** How often the band reads `GET /usage` again while the session is open. */
const POLL_MS = 60_000

/** How long a token from shunt's `apiKeyHelper` is reused before asking again. */
const HELPER_TTL_MS = 5 * 60_000

const COLORS: Readonly<Record<Level, string | undefined>> = {
  ok: undefined,
  warn: 'yellow',
  hot: 'red',
}

/** The gateway as last read; `null` off shunt, where `limits` is drawn. */
const snapshot = atom(
  { plugin: 'shunt', key: 'snapshot' } as const,
  null as ShuntSnapshot | null,
)

/** The session's own rate limits, as Claude Code last measured them. */
const limits = atom(
  { plugin: 'shunt', key: 'limits' } as const,
  [] as ShuntRateLimit[],
)

/** The engine clock at the last refresh: what the countdowns run from. */
const now = atom({ plugin: 'shunt', key: 'now' } as const, 0)

/** A plain copy: the engine's values are frozen, the state's are owned. */
const copyOf = (rateLimits: readonly ShuntRateLimit[]): ShuntRateLimit[] =>
  rateLimits.map(limit => ({ ...limit }))

/**
 * The last token shunt's `apiKeyHelper` printed, and when. A module variable
 * rather than `$.state`: it is a credential, and nothing draws from it. A
 * reload drops it, and the next read asks the helper again.
 */
let helperToken: { value: string; at: number } | undefined

/**
 * The gateway login token of a `shunt gateway claude` session: what shunt's
 * own `apiKeyHelper` prints, run the way Claude Code runs it. `undefined`
 * when the helper is not shunt's (see `hooks/helper.ts`); a `failure` when it
 * is shunt's but could not give a token, which is a fault to flag rather than
 * a set-up to fall back from.
 *
 * Reused for five minutes so a poll every minute does not run the helper
 * every minute; `isCached` says the token came from that reuse.
 */
async function helperTokenOf(
  $: EngineInterface,
): Promise<
  { value: string; isCached: boolean } | { failure: Reading } | undefined
> {
  const at = await $.clock.now()

  if (helperToken !== undefined && at - helperToken.at < HELPER_TTL_MS) {
    return { value: helperToken.value, isCached: true }
  }

  helperToken = undefined

  const home = (await $.env.get('HOME')) || (await $.env.get('USERPROFILE'))
  const argv = shuntHelperArgvOf((await $.settings.read()).apiKeyHelper, home)

  if (argv === null) {
    return undefined
  }

  try {
    const run = await $.process.run(argv)
    const value = run.exitCode === 0 ? run.stdout.trim() : ''

    if (value === '') {
      return { failure: helperFailedOf(run) }
    }

    helperToken = { value, at }

    return { value, isCached: false }
  } catch (error) {
    return { failure: helperFailedOf({ error }) }
  }
}

/** One request to `GET /usage`, read into a reading. */
async function fetchUsage($: EngineInterface, endpoint: Endpoint): Promise<Reading> {
  try {
    const response = await $.http.fetch(endpoint.url, {
      headers: endpoint.headers,
    })

    return readingOf(endpoint, response)
  } catch (error) {
    return unreachableOf(endpoint, error)
  }
}

/**
 * One `GET /usage` round trip against the gateway this session routes
 * through, with the credential it already sends there: a variable from the
 * environment, or else the gateway login token shunt's `apiKeyHelper` prints.
 *
 * Each variable is read with its own literal `$.env.get`, so
 * `claude plugin validate` can list them.
 */
async function readUsage($: EngineInterface): Promise<Reading> {
  const env: Environment = {
    shuntBaseUrl: await $.env.get('SHUNT_BASE_URL'),
    anthropicBaseUrl: await $.env.get('ANTHROPIC_BASE_URL'),
    shuntToken: await $.env.get('SHUNT_TOKEN'),
    anthropicAuthToken: await $.env.get('ANTHROPIC_AUTH_TOKEN'),
    anthropicApiKey: await $.env.get('ANTHROPIC_API_KEY'),
  }

  const resolved = endpointOf(env)

  if (!('problem' in resolved)) {
    return fetchUsage($, resolved.endpoint)
  }

  if (resolved.problem !== NO_TOKEN_TEXT) {
    return { problem: resolved.problem, brief: null }
  }

  // No credential in the environment: a `shunt gateway claude` session keeps
  // its gateway login behind the helper instead. That login belongs to the
  // session's own gateway, so when `SHUNT_BASE_URL` names another one the
  // helper is not even run.
  if (!helperMayRideTo(env)) {
    return { problem: resolved.problem, brief: null }
  }

  const readWithHelper = async (): Promise<Reading & { isCached?: boolean }> => {
    const token = await helperTokenOf($)

    if (token === undefined) {
      return { problem: resolved.problem, brief: null }
    }

    if ('failure' in token) {
      return token.failure
    }

    const withHelper = endpointOf({ ...env, helperToken: token.value })

    return 'problem' in withHelper
      ? { problem: withHelper.problem, brief: null }
      : { ...(await fetchUsage($, withHelper.endpoint)), isCached: token.isCached }
  }

  const reading = await readWithHelper()

  // A reused token the gateway refused has likely rotated: drop it and ask
  // the helper once more. A fresh one it refused is refused for good.
  if (reading.isCached && 'problem' in reading && reading.brief === CREDENTIAL_REFUSED) {
    helperToken = undefined

    return readWithHelper()
  }

  return reading
}

/** The refresh under way, which a refresh asked for meanwhile joins. */
let inFlight: Promise<void> | undefined

/** Set when a refresh joined the one under way: it reads once more. */
let again = false

/**
 * Reads the pool again and moves the band's clock on. Off a gateway the read
 * resolves at once, with no request, and only the clock moves. A refresh that
 * fails outright leaves the last snapshot standing rather than blanking the
 * band.
 */
async function refresh($: EngineInterface) {
  // The minute's poll and a turn's end can land together: join the read
  // under way rather than run the helper and the request twice. A read that
  // began before the turn's usage was recorded would leave the band stale, so
  // the joiner asks for one trailing read, however many join.
  if (inFlight !== undefined) {
    again = true

    return inFlight
  }

  inFlight = (async () => {
    try {
      do {
        again = false

        try {
          const reading = await readUsage($)
          const at = await $.clock.now()

          await update($, snapshot, () => snapshotOf(reading))
          await update($, now, () => at)
        } catch {
          // A timer's callback has no caller to report to: the next tick retries.
        }
      } while (again)
    } finally {
      inFlight = undefined
    }
  })()

  return inFlight
}

/**
 * The `/shunt:usage` command: answered from the gateway's `GET /usage` rather
 * than sent to the model.
 *
 * The command itself is `commands/usage.md`, which lists as `shunt:usage`
 * because the plugin namespaces it; the markdown body is the fallback for a
 * session without function hooks, and this hook answers before it is ever
 * read. `$.command.register` cannot serve this command: it takes a bare global
 * name and refuses `usage` as the built-in's.
 */
function registerCommand(on: On) {
  /**
   * A hook that throws or overruns its budget is treated as absent, and core
   * runs in its place — for this command that is `commands/usage.md`, which
   * asks the model to fetch the endpoint with a tool call. The answer would
   * still arrive, but it would cost a model turn in a mod whose whole point is
   * that it costs none, so the failure is reported rather than handed on.
   */
  on('command.run', { command: COMMAND_NAME }, async $ => {
    const reading = await readUsage($)

    if ('problem' in reading) {
      return { text: reading.problem }
    }

    return {
      text: reportText(reading.report, reading.base, await $.clock.now()),
    }
  }).catch(() => ({ text: HOOK_FAILED_TEXT }))
}

/**
 * The usage band: how much of each window is used and the time to each
 * reset, one row above the prompt.
 *
 * On a shunt gateway that serves `GET /usage` it shows the pool, tagged
 * `shunt ·`; anywhere else, the session's own rate limits as Claude Code
 * measures them. It reads the pool when the session starts, every minute
 * after, and after each turn, which is when the pool has just been spent
 * from. Whatever another plugin draws in the band stays on its left.
 */
function registerBand(on: On) {
  registerPolling(on)
  registerDrawing(on)
}

/**
 * Keeps the band's figures current: the pool at session start, every minute
 * and after each turn, and the session's own rate limits as they are measured.
 */
function registerPolling(on: On) {
  on('session.start', async ($, e, next) => {
    const result = await next(e)

    $.clock.after(0, () => refresh($))
    $.clock.every(POLL_MS, () => refresh($))

    // The session's own limits are the fallback, so failing to read them
    // must not cost the pool its polling: `session.measure` brings them later.
    try {
      const usage = await $.session.usage()

      await update($, limits, () => copyOf(usage.rateLimits))
    } catch {
      // Left empty until the first measurement.
    }

    return result
  })

  on('session.measure', async ($, e, next) => {
    if (e.changed.includes('rateLimits')) {
      await update($, limits, () => copyOf(e.rateLimits))
    }

    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    const result = await next(e)

    $.clock.after(0, () => refresh($))

    return result
  })
}

/**
 * Draws the band to the right of whatever is drawn beneath it, or leaves the
 * row alone when there is nothing to show or a survey holds it.
 */
function registerDrawing(on: On) {
  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    const inner = await next(e)

    if (e.props.hasSurvey) {
      return inner
    }

    const [current, rateLimits, at] = await Promise.all([
      read($, snapshot),
      read($, limits),
      read($, now),
    ])

    const band = bandOf(current, rateLimits, at || (await $.clock.now()))

    if (band === null) {
      return inner
    }

    const { Box, Text } = $.ui.resolve(e)

    return (
      <Box flexDirection="row" gap={2} flexGrow={1}>
        <Box flexGrow={1}>{inner}</Box>
        <Box key="usage" flexDirection="row">
          {band.isShunt ? <Text dimColor>{'shunt · '}</Text> : null}
          {band.cells.map((cell, index) => (
            <Box key={cell.key} flexDirection="row">
              <Text dimColor>{`${index === 0 ? '' : ' · '}${cell.label} `}</Text>
              <Text color={COLORS[cell.level]}>{cell.percent}</Text>
              {cell.reset === '' ? null : (
                <Text dimColor>{` ↻${cell.reset}`}</Text>
              )}
            </Box>
          ))}
          {band.alerts.map((alert, index) => (
            <Box key={`alert:${index}:${alert.text}`} flexDirection="row">
              <Text color={COLORS[alert.level]}>
                {`${index === 0 && band.cells.length === 0 ? '' : ' '}⚠ ${alert.text}`}
              </Text>
            </Box>
          ))}
        </Box>
      </Box>
    )
  })
}

/**
 * Registers `/shunt:usage`, and the usage band unless the `usageBand` option
 * turns it off — off, the mod polls nothing and draws nothing.
 *
 * @param on the engine's registrar
 * @param options the manifest's `userConfig` values, defaults filled in
 */
export function register(on: On, options: PluginOptions) {
  registerCommand(on)

  if (options.usageBand !== false) {
    registerBand(on)
  }
}
