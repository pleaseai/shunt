/**
 * A `GET /usage` body in the gateway's own shape (src/usage.rs): the pool
 * aggregate, then the same aggregate per pooled provider.
 */
export const USAGE_BODY = {
  pool: {
    status: 'degraded',
    windows: {
      '5h': { remaining: 0.6234, resets_at: 1_800_000_000 },
      '7d': { remaining: 0.8112, resets_at: 1_800_300_000 },
      fable: { remaining: 0.19, resets_at: 1_800_300_000 },
    },
  },
  providers: {
    codex: {
      status: 'exhausted',
      windows: {
        '5h': { remaining: 0.0, resets_at: 1_799_990_000 },
        '7d': { remaining: 0.4, resets_at: 1_800_300_000 },
        fable: { remaining: null, resets_at: null },
      },
    },
    claude: {
      status: 'ok',
      windows: {
        '5h': { remaining: 0.71, resets_at: 1_800_000_000 },
        '7d': { remaining: 0.84, resets_at: 1_800_300_000 },
        fable: { remaining: 0.19, resets_at: 1_800_300_000 },
      },
    },
  },
}
