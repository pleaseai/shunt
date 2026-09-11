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
});
