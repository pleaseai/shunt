# Releasing shunt

shunt releases are distributed through the Homebrew tap and prebuilt GitHub release binaries. The historical `shunt-gateway` crates.io package receives no new versions.

## Release automation

- **Release metadata:** release-please (`.github/workflows/release-please.yml`) maintains the release PR, version in `Cargo.toml`/`Cargo.lock`, and `CHANGELOG.md`. Merging the release PR creates the `v<version>` tag and GitHub release.
- **Binary release:** `.github/workflows/release.yml` builds native `shunt` binaries for macOS (arm64/x64) and Linux (arm64/x64, musl), creates `SHA256SUMS`, and attaches all five assets to the GitHub release.
- **Homebrew:** after the assets are uploaded, the same workflow updates `shunt.rb` in `pleaseai/homebrew-tap`. A manual `workflow_dispatch` can backfill the formula for an existing release tag.
- **Cargo metadata:** `Cargo.toml` sets `publish = false`. Per issue #292, permessage-deflate currently requires `[patch.crates-io]` fork pins. Cargo omits patches from packaged manifests, and a dependent cannot inherit them, so this configuration cannot be published correctly.

All third-party GitHub Actions must remain pinned to full commit SHAs.

## Cut a release

1. Land conventional commits on `main`. release-please keeps a release PR current (`feat:` → minor, `fix:` → patch, `feat!:`/`BREAKING CHANGE:` → major). Use a `Release-As: x.y.z` commit footer when you need a specific version.
2. Review and merge the release PR. release-please updates the manifest and changelog, creates the tag, and creates the GitHub release.
3. Confirm that the tag-triggered Release workflow succeeds:
   - all four target builds complete;
   - the GitHub release contains the four binaries plus `SHA256SUMS`;
   - `pleaseai/homebrew-tap` receives the matching formula update.
4. Smoke-test the formula:

   ```bash
   brew update
   brew upgrade pleaseai/tap/shunt
   shunt --version
   ```

## Backfill the Homebrew formula

Run the Release workflow manually with an existing tag such as `v0.31.0`. The workflow skips binary builds and reads `SHA256SUMS` from that release before updating the tap formula.

## Source installation

Users with Rust can install from the repository, where Cargo applies the patch pins:

```bash
cargo install --git https://github.com/pleaseai/shunt
```

Published crates.io versions stop at the last version released before `publish = false`; do not direct users to `cargo install shunt-gateway` for current releases.

## Revisit crates.io publication

When upstream [`snapview/tungstenite-rs#426`](https://github.com/snapview/tungstenite-rs/issues/426) ships permessage-deflate in a crates.io release, remove the `openai-oss-forks` pins and direct `tungstenite` dependency together. Then re-evaluate removing `publish = false` and restoring a crate-publishing release job.

## Notes

- The Linux binaries are musl static builds to avoid glibc version constraints. If a dependency gains a C dependency that breaks musl, switch the Linux targets in `release.yml` to `-gnu` and document the resulting glibc floor.
- Keep the formula template in `packaging/homebrew/shunt.rb` synchronized with the inline template in `.github/workflows/release.yml`.
