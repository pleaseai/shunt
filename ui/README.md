# shunt admin UI

React + Vite + TypeScript source for the admin dashboard bundle that
`--features ui` embeds in the `shunt` binary.

Requires Node.js 22.12 or newer.

```sh
npm ci        # npm install when changing dependencies
npm run dev
npm run build
npm run typecheck
npm test      # vitest, once
npm run test:watch
```

`npm run build` writes the bundle to `ui/dist`. `vite.config.ts` sets
`base: '/admin/'`, so the emitted asset URLs are `/admin/assets/...` — the paths
`src/admin/ui.rs` serves them on.

## Layout

| Path | What lives there |
| :-- | :-- |
| `src/App.tsx` | Fetches `GET /admin/api/session` — the CSRF token and the refresh buffer — then renders the dashboard |
| `src/Dashboard.tsx` | Page layout: usage first, pool management behind a disclosure |
| `src/accounts.ts` | Folding managed pool accounts and local observations into one row set, and the single effective state each row renders from |
| `src/useDashboard.ts` | The four reloadable reads, each sequenced so an older response cannot repaint over a newer one, plus the one-shot `[server.status]` read |
| `src/useProvisioningFlow.ts` | One add-account form's start → authorize → complete flow, and the two guards that keep a superseded request from writing its result back |
| `src/components/` | The tables and the two add-account forms |
| `src/__tests__/` | The behavioral suite |

## Tests

`npm test` renders components and asserts on what an operator sees. That is the
point of the suite rather than an incidental choice: the server-rendered
dashboard it replaces could only be tested by matching substrings of its emitted
JavaScript, which cannot distinguish a guard that runs from one that is merely
present.

When adding a test for a guard, check it is falsifiable — delete the guard and
confirm the test fails. Several properties here (the provisioning-flow epochs in
particular) are easy to write green against code that does nothing.

## Relationship to the Rust build

`cargo build` needs no Node toolchain and produces no dashboard bundle. Building
with `--features ui` embeds `ui/dist` into the binary, so **`ui/dist` must exist
before `cargo build --features ui`** (and before `cargo clippy`/`cargo test`
with `--all-features`) — run `npm ci && npm run build` here first. Release CI
does exactly that.

`ui/dist` and `ui/node_modules` are generated and not committed.

`GET /admin` serves this bundle, as does any path under the mount that no
other route claims — `/admin/login`, `/admin/oidc/callback`, and
`/admin/api/*` answer their own way, and `/admin/` itself `404`s because an
axum wildcard must match at least one character. The server-rendered dashboard
it replaced is gone; `src/admin/html.rs` now renders only the login page. That
page stays server-rendered because this bundle exists only in a
`--features ui` build: without the feature `/admin` answers `404` with a body
naming it, and an admin surface that lost its *sign-in* page the same way
would be unusable rather than merely dashboard-less. It is not an authentication
boundary — the shell is served unauthenticated as well.

The toolchain is deliberately separate from `site/`'s Astro/Nimbus one: the two
serve different purposes and upgrade on different schedules
(`docs/admin-ui-delivery.md`, Resolution 2).
