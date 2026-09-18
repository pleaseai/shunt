import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    // The reset times render in local time, so the suite pins a zone rather
    // than asserting whatever the runner's happens to be.
    env: { TZ: 'UTC' },
    include: ['tests/**/*.test.ts'],
  },
})
