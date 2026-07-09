# Releasing shunt

Everything below is prepared but **not yet executed** — the repo is still private
(`chatbot-pf/shunt`) and nothing has been published. This is the go-live sequence.

## What is already in place

- **Dual license**: `LICENSE-MIT` + `LICENSE-APACHE` (`MIT OR Apache-2.0`), copyright
  Passion Factory, matching the other pleaseai OSS tools.
- **Cargo metadata**: the package publishes as **`shunt-gateway`** (the bare `shunt` name on
  crates.io is taken by an unrelated project); the library and binary are still named `shunt`,
  so `cargo install shunt-gateway` installs a `shunt` binary and no source changes were needed.
  `cargo publish --dry-run` passes; the tarball is trimmed via `include` (54 KiB compressed).
- **Release CI**: `.github/workflows/release.yml` triggers on `v*` tags — builds
  `shunt-darwin-arm64` / `shunt-darwin-x64` (macos-14) and `shunt-linux-x64` /
  `shunt-linux-arm64` (musl, static) on native runners, creates a GitHub release with the
  binaries + `SHA256SUMS`, then publishes `shunt-gateway` to crates.io. All third-party
  actions are pinned to full commit SHAs.
- **Homebrew formula draft**: `packaging/homebrew/shunt.rb`, following the
  `pleaseai/homebrew-tap` binary-release pattern used by `csp.rb` / `ask.rb`.

## Go-live sequence

1. **Make the repo public.** Preferably transfer `chatbot-pf/shunt` → `pleaseai/shunt`
   (matches the tap convention — every formula in `pleaseai/homebrew-tap` points at the
   `pleaseai` org — and the org's open-source standards). The transfer needs org admin
   permissions. If the repo ends up anywhere other than `pleaseai/shunt`, update
   `repository` in `Cargo.toml` and the URLs in `packaging/homebrew/shunt.rb`.
   Also update `README.md`'s "private, early" status line.
2. **Add the crates.io token** as the `CARGO_REGISTRY_TOKEN` repo secret
   (crates.io → Account Settings → API Tokens, scope `publish-new` + `publish-update`).
   The first publish must be done by an account that will own the crate; add other owners
   with `cargo owner --add`.
3. **Tag and push:**
   ```bash
   git tag v0.1.0
   git push origin v0.1.0
   ```
   The release workflow builds the four binaries, creates the GitHub release with
   `SHA256SUMS`, and publishes `shunt-gateway` to crates.io.
4. **Publish the formula.** Copy `packaging/homebrew/shunt.rb` into `pleaseai/homebrew-tap`
   as `shunt.rb`, fill in the four `sha256` values from the release's `SHA256SUMS` asset,
   and open a PR against the tap. Then:
   ```bash
   brew install pleaseai/tap/shunt
   ```

## Subsequent releases

Bump `version` in `Cargo.toml`, commit, tag `v<version>`, push the tag, then update
`version` + the four `sha256` values in the tap's `shunt.rb`.

## Notes

- The linux binaries are **musl static** builds to avoid glibc version constraints. If a
  dependency ever grows a C dependency that breaks musl, switch the linux targets in
  `release.yml` to `-gnu` and accept the newer-glibc floor.
- `cargo install shunt-gateway` is the crates.io install path; homebrew is the
  binary path. Both produce a `shunt` binary.
- The `publish-crate` job runs after the GitHub release; if it fails (e.g. missing token),
  the release itself is unaffected — fix and `cargo publish` manually from the tag.
