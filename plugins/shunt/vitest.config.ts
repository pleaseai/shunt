import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    // The reset times render in local time, so the suite pins a zone rather
    // than asserting whatever the runner's happens to be.
    env: { TZ: 'UTC' },
    // `*.spec.ts` rather than `*.test.ts`: `claude plugin test` runs every
    // `*.test.ts(x)` under the plugin inside the engine, where vitest does not
    // load; the engine's own suite is `tests/mod/*.test.tsx`.
    include: ['tests/**/*.spec.ts'],
  },
})
