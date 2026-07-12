## Summary

<!-- What does this change do, and why? Link the milestone/spec in docs/ it implements. -->

## Milestone / spec

<!-- e.g. M1 — docs/m1-responses-translation.md §6 streaming machine -->

## Checklist

- [ ] `cargo build` passes
- [ ] `cargo test` passes (new behavior is covered; tests run without network/loopback where possible)
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo fmt --all --check` clean
- [ ] Source files stay under 500 lines
- [ ] English only; matches surrounding style
- [ ] Frozen spec in `docs/` updated if this change deviates from it
- [ ] User-facing docs updated for behavior/config/endpoint/CLI/provider/model changes — `README.md` / `site/` as applicable (`wiki/` is generated; don't hand-edit)
- [ ] Any new GitHub Action is pinned to a full commit SHA

## Notes for reviewers

<!-- Anything to look at closely: credential handling, SSE translation edge cases, etc. -->
