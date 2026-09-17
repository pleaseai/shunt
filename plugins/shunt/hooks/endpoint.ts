import { NO_BASE_URL_TEXT, NO_TOKEN_TEXT, USAGE_PATH } from './names'

/**
 * The environment the mod resolves the gateway from, read once per run with
 * one literal `$.env.get` each so `claude plugin validate` can list them.
 */
export type Environment = {
  shuntBaseUrl?: string
  anthropicBaseUrl?: string
  shuntToken?: string
  anthropicAuthToken?: string
  anthropicApiKey?: string
}

/**
 * Where to ask, and with what: the absolute `GET /usage` URL and the one
 * credential header to send.
 */
export type Endpoint = {
  /** The gateway's base, as it was configured, for error text. */
  base: string
  url: string
  headers: Record<string, string>
}

/**
 * Either an endpoint to call or the reason there is none to call.
 */
export type Resolved = { endpoint: Endpoint } | { problem: string }

/**
 * Joins `base` and `USAGE_PATH` without doubling or dropping the separator.
 */
const usageUrlOf = (base: string) => `${base.replace(/\/+$/, '')}${USAGE_PATH}`

/**
 * Resolves the gateway endpoint from the session's environment.
 *
 * The base URL is the gateway this session already sends every message to, so
 * asking it for `/usage` reaches no host the session was not already using —
 * with no base URL set there is nothing to ask, and the mod says so instead of
 * sending the session's credential to Anthropic's own API.
 *
 * The credential rides the header Claude Code itself uses for that variable —
 * `ANTHROPIC_AUTH_TOKEN` as a `Bearer`, `ANTHROPIC_API_KEY` as `x-api-key` —
 * so whichever shape the operator's `[server.auth]` matches on, it matches the
 * same way here. `SHUNT_TOKEN` overrides both and rides as a `Bearer`.
 *
 * A credential that is set but blank is no credential: `$.env.get` reads an
 * exported-but-empty variable as `''`, so every candidate is normalized to
 * absent before the choice is made rather than after. That cuts both ways — a
 * blank override falls back instead of shadowing a working one, and a blank
 * fallback reports the missing token here instead of sending an empty header
 * for the gateway to reject as a wrong one.
 */
export function endpointOf(env: Environment): Resolved {
  const base = env.shuntBaseUrl?.trim() || env.anthropicBaseUrl

  if (base === undefined || base.trim() === '') {
    return { problem: NO_BASE_URL_TEXT }
  }

  const bearer = env.shuntToken?.trim() || env.anthropicAuthToken?.trim()
  const apiKey = env.anthropicApiKey?.trim()

  const headers: Record<string, string> | undefined = bearer
    ? { authorization: `Bearer ${bearer}` }
    : apiKey
      ? { 'x-api-key': apiKey }
      : undefined

  if (headers === undefined) {
    return { problem: NO_TOKEN_TEXT }
  }

  const trimmed = base.trim()

  return {
    endpoint: { base: trimmed, url: usageUrlOf(trimmed), headers },
  }
}
