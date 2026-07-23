# C1 — Config foundation (2026-07-21) · DONE

Branch: `feat/gemini-provider` (off `main`).

## Shipped
- `src/routing.rs`: `AdapterKind::Gemini` + `From<ProviderKind>` mapping.
- `src/config.rs`: `ProviderKind::Gemini`, `AuthMode::GoogleOauth`, `host_is_google_codeassist()`
  (googleapis.com origin guard), three validation errors (`GoogleOauthWrongKind` /
  `GoogleOauthNonGoogleHost` / `GoogleOauthNotHttps`) + the validation block mirroring the
  `chatgpt_oauth`/`cursor_oauth` leak guards (gemini-kind, https, googleapis.com host, loopback
  allowed), a `ProviderConfig::gemini()` constructor, and the built-in `gemini` provider seeded at
  `https://cloudcode-pa.googleapis.com`.
- 4 unit tests: default seeding + adapter mapping, wrong-kind, non-Google-host, non-https.

## Placeholders left for later tickets (compile-green, return clean errors, never panic)
- `src/auth/mod.rs` — `AuthMode::GoogleOauth` arm returns an auth_error → **A1** replaces with GoogleAuthStore.
- `src/proxy.rs` — `AdapterKind::Gemini` arm returns 501 → **D1** replaces with GeminiAdapter dispatch.

## Verification
- `cargo test --lib`: 799 passed, 0 failed (incl. the 4 new gemini/google_oauth tests).
- `cargo fmt --all --check`: clean. `cargo clippy --all-targets --all-features`: no warnings.

## Reviewer pass
Self-reviewed (no separate reviewer agent — not authorized to spawn). Mirrors existing
oauth-provider conventions exactly; table-driven, no hardcoded provider logic. A cross-model
review is still available via `/plan review` if wanted before D1.
