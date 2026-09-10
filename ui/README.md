# shunt admin UI

React + Vite + TypeScript source for the admin dashboard bundle that
`--features ui` embeds in the `shunt` binary.

Requires Node.js 22.12 or newer.

```sh
npm ci        # npm install when changing dependencies
npm run dev
npm run build
npm run typecheck
```

`npm run build` writes the bundle to `ui/dist`. `vite.config.ts` sets
`base: '/admin/'`, so the emitted asset URLs are `/admin/assets/...` — the paths
`src/admin/ui.rs` serves them on.

## Relationship to the Rust build

`cargo build` needs no Node toolchain and produces no dashboard bundle. Building
with `--features ui` embeds `ui/dist` into the binary, so **`ui/dist` must exist
before `cargo build --features ui`** (and before `cargo clippy`/`cargo test`
with `--all-features`) — run `npm ci && npm run build` here first. Release CI
does exactly that.

`ui/dist` and `ui/node_modules` are generated and not committed.

The toolchain is deliberately separate from `site/`'s Astro/Nimbus one: the two
serve different purposes and upgrade on different schedules
(`docs/admin-ui-delivery.md`, Resolution 2).
