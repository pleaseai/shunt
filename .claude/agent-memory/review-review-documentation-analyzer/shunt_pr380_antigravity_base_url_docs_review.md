---
name: shunt-pr380-antigravity-base-url-docs-review
description: Issue #380 AntigravityAuthStore::new base_url threading — doc review method and zero-finding result, useful for future Antigravity/Google-Code-Assist base_url or config-validation-adjacent doc reviews.
metadata:
  type: project
---

Reviewed commit `6cfc54a` (branch `amondnet/fix-antigravity-honor-a-configured-base_url-duri`,
issue #380): `AntigravityAuthStore::new` gained a `base_url: impl Into<String>` param so
credential-path discovery (`loadCodeAssist`) and first-time onboarding (`onboardUser`) address the
provider's configured host instead of a hardcoded production one, with a carve-out that onboarding
still uses the `daily-` control-plane host only when the resolved `api_endpoint` equals the
production default. Result: **zero findings** — every doc claim (2 `configuration.md` table-row
edits, 1 `cli.mdx` paragraph, the `auth.rs` module header + `API_ENDPOINT`/`DAILY_API_ENDPOINT`
doc comments) verified literally true against the code.

**Key verification step that mattered**: the `AntigravityAuthStore::new` doc comment and the
`daily_api_endpoint` derivation's inline comment both assert "config validation only admits a
non-production base_url when its host is loopback" — this is a load-bearing precondition for the
whole onboarding carve-out design, and it is *not* obviously true from the diff alone. Traced it
into `src/config.rs`'s `AuthMode::AntigravityOauth` validation block (~line 3343): non-loopback
hosts must pass `host_is_google_codeassist`, which is `host == "cloudcode-pa.googleapis.com"`
**exactly** (line 1898) — so a non-loopback, non-production `base_url` is genuinely impossible to
configure. Confirmed the claim true. Method: when a diff's code comment makes a claim about what
config validation *permits* (not just what the changed function does), grep the validator function
by name and read its actual branches — don't take the comment's characterization on faith even
though comments aren't in the flagging scope themselves, since a wrong precondition there would
mean the *doc* claims built on top of it (the `daily-` carve-out sentences in both
`configuration.md` rows) are actually unverified.

Also checked `main.rs`'s `login` fn (`antigravity` arm) against the `cli.mdx` claim "Discovery
addresses `[providers.antigravity] base_url` when the config file supplies one ... and falls back
to the default Code Assist backend when no config is readable" — matches exactly:
`Config::load(config_path).ok().and_then(|c| c.provider("antigravity")...).unwrap_or_else(||
"https://cloudcode-pa.googleapis.com".to_string())`.

Locale files (ja/ko/zh-cn `reference/configuration.md`) and `site/src/content/docs/guides/providers.mdx`
were deliberately not updated with the new base_url-discovery clause (author's call: net-new
addition, not a correction). Checked each for a *surviving false* claim rather than just an
absence — found none; their `antigravity` rows either omit `base_url` behavior entirely or state
the still-true default (`cloudcode-pa.googleapis.com`). Per the fan-out pattern in
[[shunt_retry_docs_fan_out]] this is the right check to run whenever a PR intentionally skips
locale translation.
