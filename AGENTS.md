# shunt Agent Instructions

## Build & Run Commands

- Build: `cargo build`
- Release build: `cargo build --release`
- Run: `cargo run -- run` or `./target/release/shunt run`
- Validate config: `cargo run -- check` or `./target/release/shunt check`
- Token helper: `cargo run -- token`
- Learned prefill router (off by default, absent from release binaries):
  `cargo build --release --features prefill-router` (add `,ui` for the dashboard).
  It embeds Python via pyo3, so set `PYO3_PYTHON` to an interpreter (>= 3.10, shared
  libpython) whose environment has `torch`, `transformers`, `numpy`, and `accelerate`;
  pyo3 otherwise takes the first `python3` on `PATH`.

## Testing

- Full test suite: `cargo test --all-features --workspace`
- Format check: `cargo fmt --all --check`
- Lints: `cargo clippy --all-targets --all-features -- -D warnings`
- CI runs format, clippy, and tests with `RUSTFLAGS=-D warnings`.
- `tests/prefill_router.rs` runs in the default-build CI step for the
  feature-off load error; its feature-on live test skips itself unless
  `SHUNT_PREFILL_ROUTER_CHECKPOINT` names a checkpoint and
  `python3 -c "import torch, transformers"` succeeds, so CI never depends on
  Python packages.
- Benchmarks: `cargo bench`. `benches/stage_router.rs` additionally needs
  `--features bench`, which exposes `shunt::bench_support` — the facade that
  reaches the crate-private stage-router path. Without the feature that target
  builds and runs but registers no benchmarks, so pass it (CodSpeed does).
- CodSpeed (`.github/workflows/codspeed.yml`) is a **required** check, and it
  compares the PR against the stored baseline from `main` — not against the
  merge base. A red CodSpeed on a diff that changes no Rust is therefore
  expected to be environmental, not a regression you introduced. Before
  treating one as real: confirm the build inputs actually differ. The base to
  diff against is the commit CodSpeed names as `BASE` in its report footer
  ("Comparing … with `main` (b000df6)") — the stored `main` tip, **not**
  `git merge-base`, which on a typical PR is an older commit and would answer a
  different question:

  ```bash
  BASE_SHA=b000df6e   # replace with the BASE commit from the CodSpeed report
  git diff "$BASE_SHA" HEAD --name-only | grep -E '\.rs$|Cargo|rust-toolchain|\.github/'
  ```

  No output means no build input changed, so the result is environmental. Then
  compare the `Record the measurement environment` step between the PR run
  and the baseline run on `main`. The Rust toolchain is pinned there, and
  `runs-on` names one OS version rather than a moving `latest` — but GitHub
  still revises that image, and the CPU model is **not** pinnable on hosted
  runners at all. CodSpeed names both the runner image and differing CPU models
  among its causes of a false regression, recommending an immutable environment
  and a consistent CPU type
  (<https://codspeed.io/docs/instruments/cpu/regression-causes>); the logged
  image and CPU lines are what tell you which one moved. Bumping the toolchain
  pin, or GitHub revising the image, re-seeds the baseline on the next `main`
  run.

## Project Structure

- `src/main.rs`: CLI entry point.
- `src/server.rs`: Axum router and endpoint registration.
- `src/proxy.rs`: request buffering, routing, adapter dispatch.
- `src/config.rs`: typed config, defaults, TOML/env loading, validation.
- `src/routing.rs`: exact route, prefix route, default-provider resolution.
- `src/adapters/`: provider protocol adapters.
- `src/model/`: Anthropic Messages and OpenAI Responses translation.
- `src/auth/`: credential lookup and refresh helpers.
- `ui/`: React + Vite source for the admin dashboard bundle; `--features ui` embeds `ui/dist` (see `ui/README.md`).
- `tests/`: protocol and translation integration tests.
- `README.md`: top-level project overview (features, quickstart, supported providers/models).
- `docs/`: engineering specs and milestone records (`m1`–`m16`, config, running, `RELEASING`), and captured research (`docs/research/`).
- `site/`: published Nimbus documentation site deployed to Cloudflare Pages; sources under `site/src/content/docs/` (`getting-started`, `guides`, `providers`, `reference`) with custom locale fallback routing.
- `wiki/`: generated Astro Starlight wiki (wiki-please — do not hand-edit; regenerate).

## Code Style

- Write code in English, and documentation in English except for the maintained
  translations — the root `README.<locale>.md` files and the `site/` locale
  trees (see [Documentation](#documentation)).
- Keep Rust files focused and preferably under 500 lines.
- Preserve streaming semantics; do not buffer upstream SSE responses unless the client requested non-streaming output.
- Keep gateway-owned errors in the Anthropic error shape, except on the inbound Codex endpoint (`[server.codex_endpoint]`), where gateway-owned errors use the OpenAI Responses error shape so its OpenAI-protocol clients parse them through their own error path (issue #127).
- Prefer table-driven config additions over hardcoded provider logic.

## Documentation

Code and docs must not drift. When a change alters observable behavior, config
keys, endpoints, CLI, provider/model support, or defaults, update the docs it
affects **in the same PR** as the code. Update by surface:

- `README.md` — when adding/removing a capability or changing setup, supported providers/models, or the quickstart.
- `docs/` — update the relevant milestone/spec when implementation behavior deviates from it (see CONTRIBUTING.md); add a new `docs/` note for a substantial new subsystem.
- `site/src/content/docs/` — update the affected page when user-facing behavior, config keys, endpoints, or CLI changes (`getting-started/` for installation/quickstart, `guides/` for how-tos, `providers/` for a single provider's setup page, `reference/` for config/endpoints/CLI/troubleshooting).
- `wiki/` — generated by wiki-please; do **not** hand-edit and do not include it in a routine code PR. Regenerate separately when its source material changes.

A change that genuinely needs no doc update is fine — but confirm each surface
above was considered rather than skipped by default. `docs/` and code are
English-only. Two surfaces additionally carry maintained translations, and both
follow the same rule — update the English source and its translations in the
same PR rather than leaving a page current in some languages only:

- the root README: `README.ko.md`, `README.ja.md`, and `README.zh-CN.md`
  alongside `README.md`.
- the site: `ko`/`ja`/`zh-cn` copies under `site/src/content/docs/<locale>/`
  alongside the English page. This covers **every** docs surface —
  `getting-started/`, `guides/`, `providers/`, `reference/` — so a new provider
  page ships with its three locale copies in the same PR.

Anchors do not survive translation. Astro derives heading ids from the
*rendered* heading text, so a locale page linking an English fragment is a dead
link: the ko config reference is `#serverstatus-선택`, not `#serverstatus-optional`.
Before adding a cross-page fragment link to a locale file, confirm the target
section exists in that locale (they are not all at parity) and take the id from
the built `site/dist/<locale>/.../index.html`. When the target section has no
locale counterpart, link the English page rather than inventing an anchor.

## Git Workflow

- Use conventional commits.
- Keep PRs focused and link milestone docs when changing implementation behavior.
- Third-party GitHub Actions must be pinned to full commit SHAs.

## Boundaries

- ✅ Always preserve existing tests and add focused coverage for protocol changes.
- ✅ Always run format, clippy, and tests before reporting code changes as complete.
- ✅ Always update the affected docs (`README.md` / `docs/` / `site/`) in the same PR as a behavior, config, endpoint, CLI, or provider/model change; `wiki/` is generated — regenerate, never hand-edit.
- ⚠️ Ask before changing credential-file writeback behavior.
- ⚠️ Ask before changing public config keys or documented provider semantics.
- 🚫 Never weaken or remove tests to make a change pass.
- 🚫 Never commit secrets, tokens, or generated local config files.
