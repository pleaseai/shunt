/// <reference types="vitest/config" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// `base` is what makes the emitted asset URLs `/admin/assets/...`: the bundle is
// served from the `/admin` mount, not from the site root. `src/admin/ui.rs`
// embeds `dist/` verbatim and serves `dist/assets/*` at `/admin/assets/*`, so
// changing this base without changing that route breaks every asset link.
export default defineConfig({
  base: '/admin/',
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  test: {
    // The dashboard is a DOM surface end to end -- every property worth pinning
    // is about what an operator sees or what a click sends, so the tests render
    // components rather than inspecting their source.
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./src/test/setup.ts'],
    include: ['src/**/*.test.{ts,tsx}'],
    // The API stub and the `window` spies are per-test fixtures; leaving one
    // installed makes the next test pass against the previous one's world.
    restoreMocks: true,
    unstubGlobals: true,
  },
});
