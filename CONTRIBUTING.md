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

CI compiles through [mbx (mr boxington)](https://mr-boxington.jdx.dev), a shared Cargo build
cache, via `jdx/mr-boxington-action` (GitHub Actions cache backend). Locally, the committed
`mise.toml` turns it on for [mise](https://mise.jdx.dev) users through the Rust tool's
`mr_boxington` option, so a build in one worktree warms every other worktree:

```bash
mise install                  # installs mbx next to the existing `stable` toolchain
mise exec -- cargo build      # or plain `cargo` in a shell with mise activation or shims
```

There is no prebuilt mbx for Intel macOS, so `mise.toml` aliases the tool to `cargo:mbx`,
which builds it from crates.io (Rust 1.91 or newer). Without mise, run
`cargo install mbx --locked` and use `mbx build` / `mbx test` directly.

mbx wraps rustc itself, so it **defers to any `RUSTC_WRAPPER` already set** (for example
`RUSTC_WRAPPER=sccache` in a shell profile, or `build.rustc-wrapper` in `.cargo/config.toml`)
and then caches nothing. Remove that wrapper before using mbx. Check that a build uses the
cache with the `mbx[cache]` summary line it prints, `mbx doctor`, and `mbx explain --last`.
To opt out in this repo, set `mr_boxington = false` on the `rust` entry in a gitignored
`mise.local.toml`.

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
