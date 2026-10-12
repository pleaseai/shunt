/**
 * The `shunt` mod's `$.state` contract: what the band above the prompt draws
 * from — the gateway pool as `GET /usage` last reported it, the session's own
 * Anthropic rate limits, and the clock its countdowns run from.
 */

/** One window of the pool aggregate, as `hooks/report.ts` reads it. */
export type ShuntWindow = {
  /** The unused fraction of the pool's capacity, `0..=1`, or `null`. */
  remaining: number | null
  /** The earliest reported reset, unix epoch seconds, or `null`. */
  resetsAt: number | null
}

/** The pool-wide aggregate: a status and the three tracked windows. */
export type ShuntPool = {
  status: string
  windows: { '5h': ShuntWindow; '7d': ShuntWindow; fable: ShuntWindow }
}

/** One pooled provider's status, by its configured name. */
export type ShuntProviderStatus = { name: string; status: string }

/**
 * The gateway as last read: the pool and each provider's status, or the short
 * reason a gateway that serves `GET /usage` could not be read.
 */
export type ShuntSnapshot =
  | { pool: ShuntPool; providers: ShuntProviderStatus[] }
  | { problem: string }

/**
 * One of the session's own rate-limit windows, as `$.session.usage()` and
 * `session.measure` report them.
 */
export type ShuntRateLimit = {
  /** `five_hour`, `seven_day`, `spend_limit`, or another the engine names. */
  kind: string
  /** 0 to 100, past 100 on an exceeded spend limit. */
  percentUsed: number
  /** ISO 8601. */
  resetsAt?: string
}

declare module 'claude-code' {
  interface PluginState {
    shunt: {
      /**
       * `null` while the session is not on a shunt gateway that serves
       * `GET /usage`: the band then draws `limits` instead.
       */
      snapshot: ShuntSnapshot | null
      /** The session's own rate limits; empty off a subscription. */
      limits: ShuntRateLimit[]
      /** The engine clock at the last refresh, in milliseconds. */
      now: number
    }
  }
}
