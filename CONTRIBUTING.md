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
