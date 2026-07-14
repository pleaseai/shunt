# Contributing to shunt

Thanks for your interest. `shunt` is an early-stage, private project (a Claude Code LLM
gateway). The design is captured in [`docs/implementation-plan.md`](docs/implementation-plan.md)
and the per-milestone specs alongside it — read those before proposing changes.

## Development

`shunt` is a Rust (stable, edition 2021) Cargo project.

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all --check
```

All four must pass before a PR is ready; CI enforces them.

### Working in a worktree

Use an Orca worktree rather than editing the main checkout in place. `orca.yaml` seeds local
files (`.worktreeinclude`) and warms the Cargo cache on worktree creation.

### Faster local builds (optional)

CI compiles through [sccache](https://github.com/mozilla/sccache) (GitHub Actions cache
backend). You can opt in locally — it's per-developer and nothing in the repo forces it, so a
missing `sccache` binary never breaks the build:

```bash
brew install sccache  # or: cargo install sccache
```

Then turn it on either globally via your shell profile:

```bash
export RUSTC_WRAPPER=sccache
```

…or scoped to just this repo with a gitignored `.cargo/config.toml` (keeps other Rust projects
unaffected):

```toml
[build]
rustc-wrapper = "sccache"
```

sccache stores artifacts on disk by default (`~/Library/Caches/Mozilla.sccache` on macOS, 10 GB
cap; tune with `SCCACHE_DIR` / `SCCACHE_CACHE_SIZE`). For the biggest win across `cargo clean` and
branch switches, also set `CARGO_INCREMENTAL=0` — sccache can't cache incremental builds, though
that trades away incremental rebuild speed. Check hits with `sccache --show-stats`.

## Pull requests

- Keep changes scoped to a single milestone/concern; split unrelated work.
- Keep every source file under 500 lines; split modules as they grow.
- English only for code, comments, and docs.
- Follow the frozen specs in `docs/`; if a change deviates, update the spec in the same PR and
  explain why.
- Don't weaken or delete tests to make code pass — fix the code, or justify the test change.

## Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`,
`chore:`, `refactor:`, `test:`, …). Keep commits atomic and reviewable.

## GitHub Actions

Third-party actions **must** be pinned to a full 40-character commit SHA (with a `# vX.Y.Z`
comment), never a tag or branch. See [`.github/workflows/ci.yml`](.github/workflows/ci.yml) for
the pattern.

## Security

Do not open public issues for vulnerabilities — see [`SECURITY.md`](SECURITY.md).

## Author

Maintained by 이민수 (Minsu Lee, [@amondnet](https://github.com/amondnet)) ·
minsu.lee@passionfactory.ai
